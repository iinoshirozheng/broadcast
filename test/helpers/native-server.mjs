import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, symlink } from "node:fs/promises";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

// Runs the same request against the native server build instead of the in-process app.
// In-process options map to the environment the native server reads.
export async function startNativeServer(binary, directory, options, listenOptions) {
  const env = {
    ...(options.processEnv ?? process.env),
    BROADCAST_DATA_DIR: directory,
    BROADCAST_HOST: listenOptions.host ?? "127.0.0.1",
    BROADCAST_PORT: "0",
  };
  const variables = {
    instanceToken: "BROADCAST_INSTANCE_TOKEN",
    instanceSecret: "BROADCAST_INSTANCE_SECRET",
    version: "BROADCAST_VERSION",
    skillPath: "BROADCAST_SKILL_PATH",
    staticDirectory: "BROADCAST_STATIC_DIR",
  };
  for (const [option, variable] of Object.entries(variables)) {
    if (options[option] !== undefined) env[variable] = options[option];
  }
  if (options.codexStatePath || options.codexProcessesPath) {
    const codexHome = path.join(directory, "codex-home");
    await mkdir(path.join(codexHome, "process_manager"), { recursive: true });
    if (options.codexStatePath) {
      await symlink(options.codexStatePath, path.join(codexHome, ".codex-global-state.json"));
    }
    if (options.codexProcessesPath) {
      await symlink(options.codexProcessesPath, path.join(codexHome, "process_manager", "chat_processes.json"));
    }
    env.CODEX_HOME = codexHome;
  }
  const child = spawn(binary, [], { env, stdio: ["ignore", "pipe", "inherit"] });
  const port = await new Promise((resolve, reject) => {
    let output = "";
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`${binary} exited with ${code}`)));
    child.stdout.on("data", (chunk) => {
      output += chunk;
      const match = output.match(/listening on http:\/\/127\.0\.0\.1:(\d+)/);
      if (match) resolve(Number(match[1]));
    });
  });
  let database;
  const close = async () => {
    database?.close();
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, "exit");
    child.kill("SIGTERM");
    await exited;
  };
  const app = {
    close,
    options: { attachmentsDirectory: path.join(directory, "attachments") },
    database: {
      get database() {
        database ??= new DatabaseSync(path.join(directory, "broadcast.sqlite"));
        return database;
      },
    },
  };
  return { app, port };
}

