//! Checks of the macOS bundle `tauri build` produced: the App, the DMG and the updater archive
//! carry the same signed App, and the packaged `broadcast` command reaches a server.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::release::{MAC_PLATFORMS, Metadata};
use crate::{output, read_json, root, run, updater};

const CODESIGN: &str = "/usr/bin/codesign";
const HDIUTIL: &str = "/usr/bin/hdiutil";
const XCRUN: &str = "/usr/bin/xcrun";
const SPCTL: &str = "/usr/sbin/spctl";

/// Verifies the App, the DMG and the updater archive. With `release`, also the Developer ID
/// signature, notarization, the App's names and version, and the staged `latest.json`.
pub fn verify(
    app: &Path,
    dmg: &Path,
    archive: &Path,
    pubkey: Option<&Path>,
    release: Option<&str>,
) -> Result<()> {
    let release = release.map(Release::load).transpose()?;
    if let Some(release) = &release {
        let name = app.file_name().context("App path has no name")?;
        ensure!(
            name.to_string_lossy() == release.metadata.app_name(),
            "the App must be named {}",
            release.metadata.app_name()
        );
    }
    let pubkey = match pubkey {
        Some(path) => fs::read_to_string(path)?,
        None => updater::configured_pubkey()?,
    };
    let signature = updater::verify(archive, &pubkey)?;
    if let Some(release) = &release {
        release.verify_latest(archive, &signature)?;
    }

    verify_app(app, release.as_ref())?;
    run(Command::new(HDIUTIL).arg("verify").arg(dmg))?;
    if let Some(release) = &release {
        release.verify_dmg(dmg)?;
    }

    let scratch = tempfile::tempdir()?;
    let extracted = scratch.path().join("updater");
    fs::create_dir(&extracted)?;
    run(Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&extracted))?;
    let app_name = app.file_name().context("App path has no name")?;
    let updater_app = extracted.join(app_name);
    verify_app(&updater_app, release.as_ref())?;

    let mount = scratch.path().join("dmg");
    run(Command::new(HDIUTIL)
        .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
        .arg(&mount)
        .arg(dmg))?;
    let mounted = (|| {
        let dmg_app = mount.join(app_name);
        verify_app(&dmg_app, release.as_ref())?;
        let expected = manifest(app)?;
        ensure!(
            manifest(&updater_app)? == expected,
            "the updater archive's App differs from {}",
            app.display()
        );
        ensure!(
            manifest(&dmg_app)? == expected,
            "the DMG's App differs from {}",
            app.display()
        );
        Ok(())
    })();
    run(Command::new(HDIUTIL).arg("detach").arg(&mount))?;
    mounted?;

    println!(
        "Verified {}, {} and {}",
        app.display(),
        dmg.display(),
        archive.display()
    );
    Ok(())
}

fn verify_app(app: &Path, release: Option<&Release>) -> Result<()> {
    run(Command::new(CODESIGN)
        .args(["--verify", "--deep", "--strict", "--verbose=2"])
        .arg(app))?;
    let cli = app.join("Contents/MacOS/broadcast-cli");
    run(Command::new(CODESIGN)
        .args(["--verify", "--strict"])
        .arg(&cli))?;
    let help = run(Command::new(&cli).arg("--help"))?;
    ensure!(
        help.starts_with("Usage: broadcast "),
        "{} --help printed no usage",
        cli.display()
    );
    match release {
        Some(release) => release.verify_app(app),
        None => Ok(()),
    }
}

/// Every path in the App with its type, mode, size and content hash, in a stable order.
fn manifest(app: &Path) -> Result<Vec<String>> {
    fn walk(root: &Path, relative: &Path, entries: &mut Vec<String>) -> Result<()> {
        let mut names: Vec<_> = fs::read_dir(root.join(relative))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        for name in names {
            let child = relative.join(name);
            let path = root.join(&child);
            let details = fs::symlink_metadata(&path)?;
            let mode = details.permissions().mode() & 0o777;
            let shown = child.display();
            if details.is_dir() {
                entries.push(format!("{shown} directory {mode:o}"));
                walk(root, &child, entries)?;
            } else if details.is_symlink() {
                entries.push(format!("{shown} -> {}", fs::read_link(&path)?.display()));
            } else {
                let hash = Sha256::digest(fs::read(&path)?);
                entries.push(format!("{shown} file {mode:o} {} {hash:x}", details.len()));
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(app, Path::new(""), &mut entries)?;
    Ok(entries)
}

/// The Developer ID checks of a tagged release.
struct Release {
    metadata: Metadata,
    identifier: String,
    team: String,
}

impl Release {
    fn load(tag: &str) -> Result<Self> {
        let tauri = read_json(&root().join("apps/desktop/tauri.conf.json"))?;
        let policy = read_json(&root().join("apps/desktop/release.json"))?;
        Ok(Self {
            metadata: Metadata::load(tag)?,
            identifier: text(&tauri["identifier"])?,
            team: text(&policy["appleTeamId"])?,
        })
    }

    fn verify_app(&self, app: &Path) -> Result<()> {
        run(Command::new(XCRUN).args(["stapler", "validate"]).arg(app))?;
        run(Command::new(SPCTL)
            .args(["-a", "-t", "exec", "-vv"])
            .arg(app))?;
        let info = app.join("Contents/Info.plist");
        let product = &self.metadata.product_name;
        let version = &self.metadata.package_version;
        for (key, expected) in [
            ("CFBundleDisplayName", product),
            ("CFBundleName", product),
            ("CFBundleIdentifier", &self.identifier),
            ("CFBundleShortVersionString", version),
            ("CFBundleVersion", version),
        ] {
            let value = run(Command::new("/usr/libexec/PlistBuddy")
                .arg("-c")
                .arg(format!("Print :{key}"))
                .arg(&info))?;
            ensure!(
                &value == expected,
                "{} {key} is {value}, not {expected}",
                app.display()
            );
        }
        let launcher = app.join("Contents/MacOS/broadcast-launcher");
        let release_version = self.metadata.release_version.as_bytes();
        ensure!(
            fs::read(&launcher)?
                .windows(release_version.len())
                .any(|window| window == release_version),
            "the launcher does not embed release version {}",
            self.metadata.release_version
        );
        for signed in [
            app.to_path_buf(),
            launcher,
            app.join("Contents/MacOS/broadcast-cli"),
        ] {
            self.verify_team(&signed)?;
        }
        Ok(())
    }

    fn verify_dmg(&self, dmg: &Path) -> Result<()> {
        run(Command::new(XCRUN).args(["stapler", "validate"]).arg(dmg))?;
        run(Command::new(CODESIGN)
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(dmg))?;
        run(Command::new(SPCTL)
            .args([
                "-a",
                "-t",
                "open",
                "--context",
                "context:primary-signature",
                "-vv",
            ])
            .arg(dmg))?;
        self.verify_team(dmg)
    }

    fn verify_team(&self, path: &Path) -> Result<()> {
        let details = output(
            Command::new(CODESIGN)
                .args(["-dv", "--verbose=4"])
                .arg(path),
        )?;
        ensure!(
            String::from_utf8_lossy(&details.stderr)
                .contains(&format!("TeamIdentifier={}", self.team)),
            "{} is not signed by Apple Team {}",
            path.display(),
            self.team
        );
        Ok(())
    }

    /// The `latest.json` staged next to the updater archive points every macOS platform at it.
    fn verify_latest(&self, archive: &Path, signature: &str) -> Result<()> {
        let latest = read_json(&archive.with_file_name("latest.json"))?;
        ensure!(
            latest["version"] == self.metadata.release_version.as_str(),
            "latest.json version is not {}",
            self.metadata.release_version
        );
        let platforms = latest["platforms"]
            .as_object()
            .context("latest.json has no platforms")?;
        let mut names: Vec<&str> = platforms.keys().map(String::as_str).collect();
        names.sort();
        let mut expected = MAC_PLATFORMS.to_vec();
        expected.sort();
        ensure!(
            names == expected,
            "latest.json must list exactly {expected:?}"
        );
        let url = self
            .metadata
            .download_url(&self.metadata.assets.macos_updater);
        for platform in platforms.values() {
            ensure!(
                platform["url"] == url.as_str() && platform["signature"] == signature,
                "latest.json does not point every macOS platform at the verified archive"
            );
        }
        Ok(())
    }
}

fn text(value: &Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .context("expected a string")
}

/// Runs the App's `broadcast` command against a throwaway server the way an agent does: through
/// the launcher runtime file under a fresh HOME, with only the system PATH.
pub fn verify_packaged_cli(app: &Path) -> Result<()> {
    // The CLI stores the workspace path it is given as an absolute path.
    let app = &fs::canonicalize(app)?;
    let home = tempfile::tempdir()?;
    let server_bin = std::env::var_os("BROADCAST_SERVER_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/debug/broadcast-server"));
    let token = uuid::Uuid::new_v4().to_string();
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut server = Command::new(&server_bin)
        .env("BROADCAST_DATA_DIR", home.path().join("server"))
        .env("BROADCAST_HOST", "127.0.0.1")
        .env("BROADCAST_PORT", "0")
        .env("BROADCAST_INSTANCE_TOKEN", &token)
        .env("BROADCAST_INSTANCE_SECRET", secret)
        .env("BROADCAST_VERSION", "preflight")
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", server_bin.display()))?;
    let checked = (|| {
        let port = listening_port(server.stdout.take().context("server has no stdout")?)?;
        let data = home.path().join("Library/Application Support/Broadcast");
        fs::create_dir_all(&data)?;
        let runtime = format!("{{\"version\":1,\"url\":\"http://127.0.0.1:{port}/{token}\"}}\n");
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(data.join("launcher-runtime.json"))?
            .write_all(runtime.as_bytes())?;
        drive_cli(app, home.path())
    })();
    let _ = server.kill();
    let _ = server.wait();
    checked?;
    println!(
        "Verified the packaged broadcast command of {}",
        app.display()
    );
    Ok(())
}

fn listening_port(stdout: impl std::io::Read + Send + 'static) -> Result<u16> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let pattern = Regex::new(r"listening on http://127\.0\.0\.1:(\d+)").unwrap();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(captures) = pattern.captures(&line) {
                let _ = sender.send(captures[1].to_string());
                return;
            }
        }
    });
    Ok(receiver
        .recv_timeout(Duration::from_secs(15))
        .context("the Broadcast server did not start listening within 15 s")?
        .parse()?)
}

fn drive_cli(app: &Path, home: &Path) -> Result<()> {
    let resources = app.join("Contents/Resources");
    let wrapper = resources.join("bin/broadcast");
    let thread = "00000000-0000-4000-8000-000000000001";
    let broadcast = |args: &[&str]| -> Result<Value> {
        let stdout = run(Command::new(&wrapper)
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("CODEX_THREAD_ID", thread)
            .args(args))?;
        serde_json::from_str(&stdout)
            .with_context(|| format!("broadcast {args:?} printed {stdout}"))
    };

    let projects = broadcast(&["project", "list", "--json"])?;
    let project = text(&projects["projects"][0]["id"])?;
    ensure!(
        broadcast(&["cloud", "status", "--json"])?["mode"] == "local",
        "the packaged broadcast is not in local mode"
    );
    let workspace = resources.to_string_lossy();
    let mapped = broadcast(&[
        "project",
        "map",
        &project,
        "--workspace-path",
        &workspace,
        "--json",
    ])?;
    ensure!(
        mapped["projectId"] == project.as_str() && mapped["workspacePath"] == workspace.as_ref(),
        "project map returned {mapped}"
    );
    let created = broadcast(&[
        "issue",
        "create",
        "--project",
        &project,
        "--title",
        "Packaged broadcast preflight",
        "--status",
        "todo",
        "--thread-id",
        thread,
        "--json",
    ])?;
    let id = text(&created["task"]["id"])?;
    let fetched = broadcast(&["issue", "get", &id, "--json"])?;
    ensure!(
        fetched["task"]["title"] == "Packaged broadcast preflight",
        "issue get returned {fetched}"
    );
    let version = fetched["task"]["version"].to_string();
    let updated = broadcast(&[
        "issue",
        "update",
        &id,
        "--title",
        "Packaged broadcast verified",
        "--if-version",
        &version,
        "--thread-id",
        thread,
        "--json",
    ])?;
    ensure!(
        updated["task"]["title"] == "Packaged broadcast verified",
        "issue update returned {updated}"
    );
    let comment = broadcast(&[
        "comment",
        "add",
        &id,
        "--body",
        "packaged endpoint verified",
        "--thread-id",
        thread,
        "--json",
    ])?;
    if comment["comment"]["body"] != "packaged endpoint verified" {
        bail!("comment add returned {comment}");
    }
    Ok(())
}
