#!/usr/bin/env node

// Stages the desktop bundle: the web build and the manage-broadcast skill as resources, the
// `broadcast` CLI as the `broadcast-cli` sidecar, and the `bin/broadcast` wrapper that runs it.

import { spawnSync } from "node:child_process";
import { chmod, copyFile, cp, link, mkdir, rm, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const windowsTarget = "x86_64-pc-windows-msvc";
const linuxTarget = "x86_64-unknown-linux-gnu";
const macTargets = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
const universalTarget = "universal-apple-darwin";
const supportedTargets = new Set([...macTargets, universalTarget, windowsTarget, linuxTarget]);
const projectRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const tauriRoot = path.join(projectRoot, "apps", "desktop");
const binariesDirectory = path.join(tauriRoot, "binaries");
const resourcesDirectory = path.join(tauriRoot, "resources");
const target = parseTarget(process.argv.slice(2));

if (target === windowsTarget && process.platform !== "win32") {
  throw new Error("Broadcast for Windows must be prepared on Windows");
}
if (target === linuxTarget && process.platform !== "linux") {
  throw new Error("Broadcast for Linux must be prepared on Linux");
}
if (target !== windowsTarget && target !== linuxTarget && process.platform !== "darwin") {
  throw new Error("Broadcast for macOS must be prepared on macOS");
}

function parseTarget(argv) {
  let selected = process.platform === "win32"
    ? windowsTarget
    : process.platform === "linux"
      ? linuxTarget
      : universalTarget;
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--target") selected = argv[++index];
    else throw new Error(`Unknown option: ${argument}`);
  }
  if (!supportedTargets.has(selected)) {
    throw new Error(`Unsupported Tauri target: ${selected}`);
  }
  return selected;
}

function run(command, args) {
  const result = spawnSync(command, args, { cwd: projectRoot, stdio: "inherit" });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} exited with ${result.status ?? result.signal}`);
  }
}

function buildCli(triple) {
  run("cargo", ["build", "--release", "--locked", "-p", "broadcast-cli", "--target", triple]);
  const suffix = triple === windowsTarget ? ".exe" : "";
  return path.join(projectRoot, "target", triple, "release", `broadcast${suffix}`);
}

async function prepareCli() {
  await rm(binariesDirectory, { recursive: true, force: true });
  await mkdir(binariesDirectory, { recursive: true });
  if (target === universalTarget) {
    const universal = path.join(binariesDirectory, `broadcast-cli-${universalTarget}`);
    run("/usr/bin/lipo", ["-create", ...macTargets.map(buildCli), "-output", universal]);
    for (const triple of macTargets) {
      await link(universal, path.join(binariesDirectory, `broadcast-cli-${triple}`));
    }
    return;
  }
  const suffix = target === windowsTarget ? ".exe" : "";
  await copyFile(buildCli(target), path.join(binariesDirectory, `broadcast-cli-${target}${suffix}`));
}

async function writeWrapper(name, content, executable) {
  const wrapperPath = path.join(resourcesDirectory, "bin", name);
  await mkdir(path.dirname(wrapperPath), { recursive: true });
  await writeFile(wrapperPath, content);
  if (executable) await chmod(wrapperPath, 0o755);
}

async function prepareResources() {
  await rm(resourcesDirectory, { recursive: true, force: true });
  await mkdir(resourcesDirectory, { recursive: true });
  await Promise.all([
    cp(path.join(projectRoot, "apps", "web", "dist"), path.join(resourcesDirectory, "web"), { recursive: true }),
    cp(
      path.join(projectRoot, "skills", "manage-broadcast"),
      path.join(resourcesDirectory, "skills", "manage-broadcast"),
      { recursive: true },
    ),
  ]);

  if (target === windowsTarget) {
    await writeWrapper("broadcast.cmd", [
      "@echo off",
      "setlocal",
      "set \"BROADCAST_RUNTIME_FILE=%APPDATA%\\Broadcast\\launcher-runtime.json\"",
      "\"%~dp0..\\broadcast-cli.exe\" %*",
      "exit /b %ERRORLEVEL%",
      "",
    ].join("\r\n"), false);
    return;
  }

  if (target === linuxTarget) {
    await writeWrapper("broadcast", `#!/bin/sh
set -u

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
RESOURCE_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
if [ -z "\${WSL_DISTRO_NAME-}" ] && [ -z "\${WSL_INTEROP-}" ] && [ -z "\${BROADCAST_RUNTIME_FILE-}" ]; then
  export BROADCAST_RUNTIME_FILE="\${XDG_DATA_HOME:-$HOME/.local/share}/Broadcast/launcher-runtime.json"
fi
exec "$RESOURCE_DIR/../../bin/broadcast-cli" "$@"
`, true);
    return;
  }

  await writeWrapper("broadcast", `#!/bin/zsh
set -u

SCRIPT_PATH="$0"
if [ -L "$SCRIPT_PATH" ]; then
  SCRIPT_PATH="$(readlink "$SCRIPT_PATH")"
fi
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"
CONTENTS_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
export BROADCAST_RUNTIME_FILE="$HOME/Library/Application Support/Broadcast/launcher-runtime.json"
exec "$CONTENTS_DIR/MacOS/broadcast-cli" "$@"
`, true);
}

await prepareResources();
await prepareCli();
console.log(`Prepared Tauri resources for ${target}`);
