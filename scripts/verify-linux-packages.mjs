#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { mkdir, mkdtemp, open, rm, stat } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const debPath = process.argv[2] ? path.resolve(process.argv[2]) : null;
const appImagePath = process.argv[3] ? path.resolve(process.argv[3]) : null;
if (!debPath || !appImagePath || process.argv.length !== 4) {
  throw new Error("Usage: verify-linux-packages.mjs <package.deb> <package.AppImage>");
}
if (process.platform !== "linux") {
  throw new Error("Linux packages must be verified on Linux");
}
const productName = process.env.BROADCAST_RELEASE_VERSION?.includes("-beta.")
  ? "Broadcast Beta"
  : "Broadcast";

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
  if (result.status !== 0) {
    throw new Error(
      result.error?.message ||
        result.stderr?.trim() ||
        result.stdout?.trim() ||
        `${command} failed`,
    );
  }
  return result.stdout.trim();
}

async function assertExecutable(filePath) {
  const details = await stat(filePath);
  if (!details.isFile() || (details.mode & 0o111) === 0) {
    throw new Error(`Expected executable file: ${filePath}`);
  }
}

async function assertElfX64(filePath) {
  const header = Buffer.alloc(20);
  const file = await open(filePath, "r");
  const { bytesRead } = await file.read(header, 0, header.length, 0);
  await file.close();
  if (
    bytesRead !== header.length ||
    !header.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46])) ||
    header[4] !== 2 ||
    header[5] !== 1 ||
    header.readUInt16LE(18) !== 0x3e
  ) {
    throw new Error(`Expected Linux x86_64 ELF: ${filePath}`);
  }
}

async function verifyPackageRoot(root, label) {
  const launcherPath = path.join(root, "usr", "bin", "broadcast-launcher");
  const cliPath = path.join(root, "usr", "bin", "broadcast-cli");
  const resourceRoot = path.join(root, "usr", "lib", productName);
  const broadcastPath = path.join(resourceRoot, "bin", "broadcast");
  const requiredResources = ["web/index.html", "skills/manage-broadcast/SKILL.md"];

  await assertExecutable(launcherPath);
  await assertElfX64(launcherPath);
  await assertExecutable(cliPath);
  await assertElfX64(cliPath);
  await assertExecutable(broadcastPath);
  for (const resource of requiredResources) {
    const details = await stat(path.join(resourceRoot, resource));
    if (!details.isFile()) throw new Error(`${label} is missing ${resource}`);
  }

  // The wrapper resolves the sidecar relative to itself, so it runs inside the extracted tree.
  if (!run(broadcastPath, ["--help"]).startsWith("Usage: broadcast ")) {
    throw new Error(`${label} broadcast does not run the packaged broadcast-cli`);
  }
}

const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), "broadcast-linux-packages."));
try {
  if (run("dpkg-deb", ["--field", debPath, "Architecture"]) !== "amd64") {
    throw new Error("Debian package architecture is not amd64");
  }
  const debRoot = path.join(temporaryRoot, "deb");
  run("dpkg-deb", ["--extract", debPath, debRoot]);
  await verifyPackageRoot(debRoot, "Debian package");

  await assertExecutable(appImagePath);
  await assertElfX64(appImagePath);
  const appImageRoot = path.join(temporaryRoot, "appimage");
  await mkdir(appImageRoot);
  run(appImagePath, ["--appimage-extract"], {
    cwd: appImageRoot,
  });
  await verifyPackageRoot(path.join(appImageRoot, "squashfs-root"), "AppImage");
} finally {
  await rm(temporaryRoot, { recursive: true, force: true });
}

console.log("Verified Linux x64 deb and AppImage package contents");
