#!/usr/bin/env node
// Runs the packaged `bin/broadcast` wrapper and its `broadcast-cli` sidecar against a Broadcast
// server found through the App's launcher-runtime.json, the way Codex agents reach the desktop.
import { randomBytes, randomUUID } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { mkdir, mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appPath = process.argv[2] ? path.resolve(process.argv[2]) : null;
if (!appPath) throw new Error("Usage: verify-packaged-broadcast.mjs <App.app>");
// The desktop runs the server in-process; `pixi run build-native` builds the same server alone.
const serverBin =
  process.env.BROADCAST_SERVER_BIN ??
  fileURLToPath(new URL("../target/debug/broadcast-server", import.meta.url));

function waitForExit(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return Promise.race([
    new Promise((resolve) => child.once("exit", resolve)),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error("Packaged Broadcast server did not exit")), timeoutMs),
    ),
  ]);
}

function runBroadcast(wrapperPath, homeDirectory, args) {
  const result = spawnSync(wrapperPath, args, {
    encoding: "utf8",
    env: {
      HOME: homeDirectory,
      PATH: "/usr/bin:/bin",
      CODEX_THREAD_ID: "00000000-0000-4000-8000-000000000001",
    },
  });
  if (result.status !== 0) {
    throw new Error(result.stderr.trim() || result.stdout.trim() || "Packaged broadcast failed");
  }
  return JSON.parse(result.stdout);
}

const temporaryHome = await mkdtemp(path.join(os.tmpdir(), "broadcast-broadcast."));
const dataDirectory = path.join(temporaryHome, "Library", "Application Support", "Broadcast");
await mkdir(dataDirectory, { recursive: true });
const runtimeFile = path.join(dataDirectory, "launcher-runtime.json");
const resourcesDirectory = path.join(appPath, "Contents", "Resources");
const wrapperPath = path.join(resourcesDirectory, "bin", "broadcast");

const instanceToken = randomUUID();
const server = spawn(serverBin, [], {
  env: {
    ...process.env,
    BROADCAST_DATA_DIR: path.join(temporaryHome, "server"),
    BROADCAST_HOST: "127.0.0.1",
    BROADCAST_PORT: "0",
    BROADCAST_INSTANCE_TOKEN: instanceToken,
    BROADCAST_INSTANCE_SECRET: randomBytes(32).toString("hex"),
    BROADCAST_VERSION: "preflight",
  },
  stdio: ["ignore", "pipe", "pipe"],
});
let stderr = "";
server.stderr.setEncoding("utf8");
server.stderr.on("data", (chunk) => {
  stderr += chunk;
});

try {
  const port = await new Promise((resolve, reject) => {
    let stdout = "";
    const timeout = setTimeout(
      () => reject(new Error(stderr || "Broadcast server did not start")),
      15_000,
    );
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (chunk) => {
      stdout += chunk;
      const match = stdout.match(/listening on http:\/\/127\.0\.0\.1:(\d+)/);
      if (match) {
        clearTimeout(timeout);
        resolve(match[1]);
      }
    });
    server.once("exit", () => {
      clearTimeout(timeout);
      reject(new Error(stderr || "Broadcast server exited during startup"));
    });
  });
  await writeFile(
    runtimeFile,
    `${JSON.stringify({ version: 1, url: `http://127.0.0.1:${port}/${instanceToken}` })}\n`,
    { mode: 0o600 },
  );
  if ((await stat(runtimeFile)).mode % 0o1000 !== 0o600) {
    throw new Error("Packaged Broadcast runtime descriptor must use mode 0600");
  }
  const projects = runBroadcast(wrapperPath, temporaryHome, ["project", "list", "--json"]);
  const projectId = projects.projects?.[0]?.id;
  if (!projectId) throw new Error("Packaged broadcast did not list the local project");
  const cloudStatus = runBroadcast(wrapperPath, temporaryHome, ["cloud", "status", "--json"]);
  if (cloudStatus.mode !== "local")
    throw new Error("Packaged cloud status used the wrong endpoint");
  const mapping = runBroadcast(wrapperPath, temporaryHome, [
    "project",
    "map",
    projectId,
    "--workspace-path",
    resourcesDirectory,
    "--json",
  ]);
  if (mapping.projectId !== projectId || mapping.workspacePath !== resourcesDirectory) {
    throw new Error("Packaged project map used the wrong endpoint");
  }
  const created = runBroadcast(wrapperPath, temporaryHome, [
    "issue",
    "create",
    "--project",
    projectId,
    "--title",
    "Packaged broadcast preflight",
    "--status",
    "todo",
    "--thread-id",
    "00000000-0000-4000-8000-000000000001",
    "--json",
  ]).task;
  const fetched = runBroadcast(wrapperPath, temporaryHome, [
    "issue",
    "get",
    created.id,
    "--json",
  ]).task;
  if (fetched.title !== "Packaged broadcast preflight")
    throw new Error("Packaged issue get failed");
  const updated = runBroadcast(wrapperPath, temporaryHome, [
    "issue",
    "update",
    created.id,
    "--title",
    "Packaged broadcast verified",
    "--if-version",
    String(fetched.version),
    "--thread-id",
    "00000000-0000-4000-8000-000000000001",
    "--json",
  ]).task;
  if (updated.title !== "Packaged broadcast verified")
    throw new Error("Packaged issue update failed");
  const comment = runBroadcast(wrapperPath, temporaryHome, [
    "comment",
    "add",
    created.id,
    "--body",
    "packaged endpoint verified",
    "--thread-id",
    "00000000-0000-4000-8000-000000000001",
    "--json",
  ]).comment;
  if (comment.body !== "packaged endpoint verified") throw new Error("Packaged comment add failed");
} finally {
  if (server.exitCode === null && server.signalCode === null) {
    server.kill("SIGKILL");
    await waitForExit(server, 2_000).catch(() => {});
  }
  await rm(temporaryHome, { recursive: true, force: true });
}

console.log("Verified the packaged broadcast CLI through the launcher runtime descriptor");
