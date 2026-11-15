//! Release naming: the version and tag checks of a release, the asset names on the GitHub
//! Release, and the macOS updater's `latest.json`.

use std::cmp::Ordering;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use regex::Regex;
use serde_json::{Value, json};

use crate::{read_json, root};

const DOWNLOADS: &str = "https://github.com/iinoshirozheng/broadcast/releases/download";

/// The updater platforms the universal macOS archive serves.
pub const MAC_PLATFORMS: [&str; 6] = [
    "darwin-aarch64",
    "darwin-x86_64",
    "darwin-universal",
    "darwin-aarch64-app",
    "darwin-x86_64-app",
    "darwin-universal-app",
];

pub struct Metadata {
    pub package_version: String,
    pub release_tag: String,
    pub release_version: String,
    pub beta_number: Option<String>,
    pub product_name: String,
    pub assets: Assets,
}

pub struct Assets {
    pub dmg: String,
    pub linux_deb: String,
    pub linux_appimage: String,
    pub windows_installer: String,
    pub macos_updater: String,
}

impl Metadata {
    /// Reads the version from package.json, Cargo.toml and tauri.conf.json, which must agree,
    /// and names the release of `tag`: `v<version>` or `v<version>-beta.<n>`.
    pub fn load(tag: &str) -> Result<Self> {
        let root = root();
        let tauri = read_json(&root.join("apps/desktop/tauri.conf.json"))?;
        let package = read_json(&root.join("package.json"))?;
        let cargo: toml::Table = fs::read_to_string(root.join("Cargo.toml"))?.parse()?;
        let version = string(&package["version"], "package.json version")?;
        let cargo_version = cargo["workspace"]["package"]["version"]
            .as_str()
            .context("Cargo.toml has no workspace.package.version")?;
        if cargo_version != version
            || string(&tauri["version"], "tauri.conf.json version")? != version
        {
            bail!("package.json, Cargo.toml and tauri.conf.json must carry the same version");
        }
        if !Regex::new(r"^\d+\.\d+\.\d+$")?.is_match(version) {
            bail!("the App version {version} must be numeric major.minor.patch for Apple");
        }

        let stable_tag = format!("v{version}");
        let beta_number = match tag.strip_prefix(&format!("{stable_tag}-beta.")) {
            Some(number) if Regex::new(r"^[1-9]\d*$")?.is_match(number) => Some(number.to_string()),
            None if tag == stable_tag => None,
            _ => bail!("release tag {tag} does not match version {version}"),
        };
        let stable_name = string(&tauri["productName"], "tauri.conf.json productName")?;
        let product_name = match beta_number {
            Some(_) => format!("{stable_name} Beta"),
            None => stable_name.to_string(),
        };
        let prefix = format!("Codex.Broadcast_{version}");
        Ok(Self {
            package_version: version.to_string(),
            release_tag: tag.to_string(),
            release_version: tag[1..].to_string(),
            beta_number,
            product_name,
            assets: Assets {
                dmg: format!("{prefix}_macOS-universal.dmg"),
                linux_deb: format!("{prefix}_Ubuntu-24.04-x64.deb"),
                linux_appimage: format!("{prefix}_Ubuntu-24.04-x64.AppImage"),
                windows_installer: format!("{prefix}_NSIS-x64-unsigned.exe"),
                macos_updater: format!("{prefix}_universal.app.tar.gz"),
            },
        })
    }

    pub fn app_name(&self) -> String {
        format!("{}.app", self.product_name)
    }

    pub fn download_url(&self, asset: &str) -> String {
        format!("{DOWNLOADS}/{}/{asset}", self.release_tag)
    }

    pub fn to_json(&self) -> Value {
        let assets = &self.assets;
        let names = json!({
            "dmg": assets.dmg,
            "linuxDeb": assets.linux_deb,
            "linuxDebSignature": format!("{}.sig", assets.linux_deb),
            "linuxAppImage": assets.linux_appimage,
            "linuxAppImageSignature": format!("{}.sig", assets.linux_appimage),
            "windowsInstaller": assets.windows_installer,
            "macosUpdater": assets.macos_updater,
            "macosUpdaterSignature": format!("{}.sig", assets.macos_updater),
            "latest": "latest.json",
        });
        let asset_names: Vec<Value> = names.as_object().unwrap().values().cloned().collect();
        json!({
            "packageVersion": self.package_version,
            "releaseTag": self.release_tag,
            "releaseVersion": self.release_version,
            "channel": if self.beta_number.is_some() { "beta" } else { "stable" },
            "prerelease": self.beta_number.is_some(),
            "betaNumber": self.beta_number.as_deref().unwrap_or(""),
            "productName": self.product_name,
            "appName": self.app_name(),
            // What `tauri build` names the universal DMG and the updater archive.
            "tauriDmgName": format!("{}_{}_universal.dmg", self.product_name, self.package_version),
            "tauriUpdaterName": format!("{}.app.tar.gz", self.product_name),
            "assets": names,
            "assetNames": asset_names,
        })
    }
}

fn string<'a>(value: &'a Value, what: &str) -> Result<&'a str> {
    value.as_str().with_context(|| format!("{what} is missing"))
}

/// Refuses a tag that is not newer than every release in `releases` (the tab-separated
/// `tag, draft, prerelease, immutable, target` rows of the GitHub releases), and a stable tag
/// whose version has no published immutable beta on `commit`.
pub fn check_order(tag: &str, releases: &Path, commit: &str) -> Result<()> {
    let metadata = Metadata::load(tag)?;
    let version = &metadata.package_version;
    let stable = Regex::new(r"^(?:app-)?v(\d+(?:\.\d+)*)$")?;
    let beta = Regex::new(r"^v(\d+(?:\.\d+)*)-beta\.([1-9]\d*)$")?;
    let rows = fs::read_to_string(releases)?;
    let mut confirmed_beta = false;
    for row in rows.lines().filter(|row| !row.is_empty()) {
        let fields: Vec<&str> = row.split('\t').collect();
        let [other, draft, prerelease, immutable, target] = fields[..] else {
            bail!("malformed release row: {row}");
        };
        if let Some(captures) = stable.captures(other)
            && compare_versions(&captures[1], version) != Ordering::Less
        {
            bail!("the stable release {other} has the same or a higher version");
        }
        let Some(captures) = beta.captures(other) else {
            continue;
        };
        if let Some(number) = &metadata.beta_number {
            let base = compare_versions(&captures[1], version);
            if base == Ordering::Greater
                || (base == Ordering::Equal
                    && compare_numbers(&captures[2], number) != Ordering::Less)
            {
                bail!("the beta release {other} has the same or a higher version");
            }
        }
        confirmed_beta |= &captures[1] == version
            && draft == "false"
            && prerelease == "true"
            && immutable == "true"
            && target == commit;
    }
    if metadata.beta_number.is_none() && !confirmed_beta {
        bail!("a stable release needs a published immutable beta of {version} on the same commit");
    }
    Ok(())
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left: Vec<&str> = left.split('.').collect();
    let right: Vec<&str> = right.split('.').collect();
    (0..left.len().max(right.len()))
        .map(|index| {
            compare_numbers(
                left.get(index).unwrap_or(&"0"),
                right.get(index).unwrap_or(&"0"),
            )
        })
        .find(|ordering| ordering.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Compares decimal digit strings of any length.
fn compare_numbers(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

/// Writes `<dir>/latest.json`, pointing every macOS platform at the universal updater archive
/// staged in `<dir>` with its `.sig`.
pub fn write_macos_latest(tag: &str, dir: &Path) -> Result<()> {
    let metadata = Metadata::load(tag)?;
    let updater = &metadata.assets.macos_updater;
    let signature = fs::read_to_string(dir.join(format!("{updater}.sig")))?;
    let platform = json!({ "signature": signature, "url": metadata.download_url(updater) });
    let platforms: serde_json::Map<String, Value> = MAC_PLATFORMS
        .iter()
        .map(|name| (name.to_string(), platform.clone()))
        .collect();
    let latest = json!({
        "version": metadata.release_version,
        "notes": format!("Broadcast {}", metadata.release_version),
        "pub_date": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "platforms": platforms,
    });
    fs::write(
        dir.join("latest.json"),
        format!("{}\n", serde_json::to_string_pretty(&latest)?),
    )?;
    Ok(())
}
