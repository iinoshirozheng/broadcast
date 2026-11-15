import { spawn } from "node:child_process";
import { once } from "node:events";
import { chmod, writeFile } from "node:fs/promises";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";

// `pixi run test` builds this binary first; BROADCAST_SERVER_BIN points at another build.
const SERVER_BIN =
  process.env.BROADCAST_SERVER_BIN ??
  fileURLToPath(new URL("../../target/debug/broadcast-server", import.meta.url));

// Writes a Node script the server can spawn as an executable and returns its path: the script
// itself where a shebang runs it, and a `.cmd` that runs it with this Node on Windows.
export async function writeNodeExecutable(basePath: string, source: string): Promise<string> {
  const script = `${basePath}.mjs`;
  await writeFile(script, `#!/usr/bin/env node\n${source}`);
  if (process.platform !== "win32") {
    await chmod(script, 0o755);
    return script;
  }
  await writeFile(`${basePath}.cmd`, `@"${process.execPath}" "${script}" %*\r\n`);
  return `${basePath}.cmd`;
}

export interface AiChatThread {
  id: string;
  title: string;
  origin: { projectId: string; projectName: string; workspacePath: string };
  model: string;
  reasoningEffort: string;
  sandbox: string;
}

// Starts the server binary on an ephemeral loopback port with its data, and an empty Codex home,
// in `directory`. It runs `codexExecutable` as the Codex CLI.
export async function startNativeServer(directory: string, codexExecutable: string) {
  const child = spawn(SERVER_BIN, [], {
    env: {
      ...process.env,
      BROADCAST_DATA_DIR: directory,
      BROADCAST_HOST: "127.0.0.1",
      BROADCAST_PORT: "0",
      CODEX_HOME: path.join(directory, "codex-home"),
      CODEX_EXECUTABLE: codexExecutable,
    },
    stdio: ["ignore", "pipe", "inherit"],
  });
  const port = await new Promise<number>((resolve, reject) => {
    let output = "";
    child.once("error", reject);
    child.once("exit", (code) => reject(new Error(`${SERVER_BIN} exited with ${code}`)));
    child.stdout.on("data", (chunk) => {
      output += chunk;
      const match = output.match(/listening on http:\/\/127\.0\.0\.1:(\d+)/);
      if (match) resolve(Number(match[1]));
    });
  });
  let database: DatabaseSync | undefined;
  const openDatabase = () => {
    database ??= new DatabaseSync(path.join(directory, "broadcast.sqlite"));
    return database;
  };
  return {
    port,
    database: openDatabase,
    // Seeds a thread row the way the server's database layer writes one.
    createAiChatThread(thread: AiChatThread): AiChatThread {
      const timestamp = new Date().toISOString();
      openDatabase()
        .prepare(`
          INSERT INTO ai_chat_threads (
            id, title, status, origin_project_id, origin_project_name, origin_workspace_path,
            origin_issue_id, origin_issue_identifier, model, reasoning_effort, sandbox, created_at, updated_at
          ) VALUES (?, ?, 'idle', ?, ?, ?, NULL, NULL, ?, ?, ?, ?, ?)
        `)
        .run(
          thread.id,
          thread.title,
          thread.origin.projectId,
          thread.origin.projectName,
          thread.origin.workspacePath,
          thread.model,
          thread.reasoningEffort,
          thread.sandbox,
          timestamp,
          timestamp,
        );
      return thread;
    },
    async close() {
      database?.close();
      if (child.exitCode !== null || child.signalCode !== null) return;
      const exited = once(child, "exit");
      child.kill("SIGTERM");
      await exited;
    },
  };
}
