#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const scriptPath = fileURLToPath(import.meta.url);
const projectRoot = path.resolve(path.dirname(scriptPath), "..");
const appPath = process.argv[2] ? path.resolve(process.argv[2]) : null;
const identity = process.env.APPLE_SIGNING_IDENTITY?.trim();
const releasePolicy = JSON.parse(
  readFileSync(path.join(projectRoot, "apps", "desktop", "release.json"), "utf8"),
);

if (!appPath) throw new Error("Usage: sign-macos-app.mjs <App.app>");
if (!identity) throw new Error("APPLE_SIGNING_IDENTITY is required");

const macosDirectory = path.join(appPath, "Contents", "MacOS");
// The `broadcast-cli` sidecar is a nested executable: it is signed before the App seals it.
const executables = ["broadcast-cli", "broadcast-launcher"].map((name) =>
  path.join(macosDirectory, name),
);
const entitlementsPath = path.join(projectRoot, "apps", "desktop", "Entitlements.plist");

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    ...options,
  });
  if (result.status !== 0) {
    throw new Error(result.stderr.trim() || result.stdout.trim() || `${command} failed`);
  }
  return { stdout: result.stdout.trim(), stderr: result.stderr.trim() };
}

function entitlements(targetPath) {
  const { stdout } = run("/usr/bin/codesign", ["-d", "--entitlements", ":-", targetPath]);
  const { stdout: json } = run("/usr/bin/plutil", ["-convert", "json", "-o", "-", "-"], {
    input: stdout,
  });
  return JSON.parse(json);
}

function signingDetails(targetPath) {
  return run("/usr/bin/codesign", ["-dv", "--verbose=4", targetPath]).stderr;
}

run("/usr/bin/xattr", ["-crs", appPath]);
for (const targetPath of [...executables, appPath]) {
  run("/usr/bin/codesign", [
    "--force",
    "--timestamp",
    "--options",
    "runtime",
    "--entitlements",
    entitlementsPath,
    "--sign",
    identity,
    targetPath,
  ]);
}

run("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", appPath]);
for (const targetPath of [...executables, appPath]) {
  if (Object.keys(entitlements(targetPath)).length !== 0) {
    throw new Error(`${targetPath} must use the empty App entitlement policy`);
  }
  if (!signingDetails(targetPath).includes(`TeamIdentifier=${releasePolicy.appleTeamId}`)) {
    throw new Error(`Signed target does not use Apple Team ${releasePolicy.appleTeamId}`);
  }
}
run(executables[0], ["--help"]);

console.log(`Signed and verified ${appPath}`);
