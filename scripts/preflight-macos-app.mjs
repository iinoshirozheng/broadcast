#!/usr/bin/env node

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { lstat, mkdtemp, readFile, readdir, readlink, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { verifyUpdaterSignature } from "./verify-updater-signature.mjs";

const appPath = process.argv[2] ? path.resolve(process.argv[2]) : null;
if (!appPath) throw new Error("Usage: preflight-macos-app.mjs <App.app>");
const projectRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const entitlementsPath = path.join(projectRoot, "apps", "desktop", "Entitlements.plist");

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  if (result.status !== 0) {
    throw new Error(result.stderr.trim() || result.stdout.trim() || `${command} failed`);
  }
  return { stdout: result.stdout.trim(), stderr: result.stderr.trim() };
}

// The signed `broadcast-cli` sidecar runs from wherever the App was copied or extracted.
function verifyCli(targetApp) {
  const cliPath = path.join(targetApp, "Contents", "MacOS", "broadcast-cli");
  run("/usr/bin/codesign", ["--verify", "--strict", "--verbose=2", cliPath]);
  if (!run(cliPath, ["--help"]).stdout.startsWith("Usage: broadcast ")) {
    throw new Error(`${cliPath} did not print the broadcast usage`);
  }
}

async function manifest(root, relative = "") {
  const entries = [];
  for (const name of (await readdir(path.join(root, relative))).sort()) {
    const childRelative = path.join(relative, name);
    const childPath = path.join(root, childRelative);
    const details = await lstat(childPath);
    if (details.isDirectory()) {
      entries.push({ path: childRelative, type: "directory", mode: details.mode & 0o777 });
      entries.push(...(await manifest(root, childRelative)));
    } else if (details.isSymbolicLink()) {
      entries.push({ path: childRelative, type: "symlink", target: await readlink(childPath) });
    } else if (details.isFile()) {
      entries.push({
        path: childRelative,
        type: "file",
        mode: details.mode & 0o777,
        size: details.size,
        sha256: createHash("sha256")
          .update(await readFile(childPath))
          .digest("hex"),
      });
    }
  }
  return entries;
}

const macosDirectory = path.join(appPath, "Contents", "MacOS");
for (const targetPath of [
  path.join(macosDirectory, "broadcast-cli"),
  path.join(macosDirectory, "broadcast-launcher"),
  appPath,
]) {
  run("/usr/bin/codesign", [
    "--force",
    "--options",
    "runtime",
    "--entitlements",
    entitlementsPath,
    "--sign",
    "-",
    targetPath,
  ]);
}
run("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", appPath]);
verifyCli(appPath);

const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), "broadcast-preflight."));
let mountedDmg = null;
try {
  const archivePath = path.join(temporaryRoot, "Codex.Broadcast.preflight.app.tar.gz");
  run("/usr/bin/tar", ["-czf", archivePath, path.basename(appPath)], {
    cwd: path.dirname(appPath),
  });
  const keyPath = path.join(temporaryRoot, "updater.key");
  run(path.join(projectRoot, "node_modules", ".bin", "tauri"), [
    "signer",
    "generate",
    "--ci",
    "--password",
    "",
    "--write-keys",
    keyPath,
  ]);
  run(path.join(projectRoot, "node_modules", ".bin", "tauri"), [
    "signer",
    "sign",
    "--private-key-path",
    keyPath,
    "--password",
    "",
    archivePath,
  ]);
  await verifyUpdaterSignature({
    publicKey: await readFile(`${keyPath}.pub`, "utf8"),
    artifactPath: archivePath,
    signature: await readFile(`${archivePath}.sig`, "utf8"),
  });

  const extractedDirectory = path.join(temporaryRoot, "updater");
  run("/bin/mkdir", ["-p", extractedDirectory]);
  run("/usr/bin/tar", ["-xzf", archivePath, "-C", extractedDirectory]);
  const extractedApp = path.join(extractedDirectory, path.basename(appPath));
  run("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", extractedApp]);
  verifyCli(extractedApp);

  const dmgPath = path.join(temporaryRoot, "Codex.Broadcast.preflight.dmg");
  const dmgSource = path.join(temporaryRoot, "dmg-source");
  run("/bin/mkdir", ["-p", dmgSource]);
  run("/usr/bin/ditto", [appPath, path.join(dmgSource, path.basename(appPath))]);
  run("/usr/bin/hdiutil", ["create", "-quiet", "-fs", "HFS+", "-srcfolder", dmgSource, dmgPath]);
  run("/usr/bin/hdiutil", ["verify", dmgPath]);
  const attach = run("/usr/bin/hdiutil", ["attach", "-readonly", "-nobrowse", "-plist", dmgPath]);
  const attachJson = JSON.parse(
    run("/usr/bin/plutil", ["-convert", "json", "-o", "-", "-"], { input: attach.stdout }).stdout,
  );
  mountedDmg = attachJson["system-entities"].map((entry) => entry["mount-point"]).find(Boolean);
  if (!mountedDmg) throw new Error("Preflight DMG did not expose a mount point");
  const dmgApp = path.join(mountedDmg, path.basename(appPath));
  run("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", dmgApp]);
  verifyCli(dmgApp);

  const expectedManifest = JSON.stringify(await manifest(appPath));
  if (JSON.stringify(await manifest(extractedApp)) !== expectedManifest) {
    throw new Error("Preflight updater App differs from the source App");
  }
  if (JSON.stringify(await manifest(dmgApp)) !== expectedManifest) {
    throw new Error("Preflight DMG App differs from the source App");
  }
} finally {
  if (mountedDmg) run("/usr/bin/hdiutil", ["detach", mountedDmg]);
  await rm(temporaryRoot, { recursive: true, force: true });
}

console.log("Verified secretless macOS App, updater, and DMG preflight");
