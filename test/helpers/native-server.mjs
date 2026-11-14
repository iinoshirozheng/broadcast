import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, symlink, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
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
  const upstream = options.cloudConfigStore ? await writeCloudConfig(directory, options) : null;
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
    if (upstream) await new Promise((resolve) => upstream.close(resolve));
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


// The native server reads cloud settings from its data directory. A `remoteFetch` stub is served
// from a loopback upstream that hands it the request as addressed to the configured remote origin.
async function writeCloudConfig(directory, options) {
  const config = await options.cloudConfigStore.read();
  let upstream = null;
  if (options.remoteFetch && config.remoteUrl) {
    const remoteOrigin = new URL(config.remoteUrl).origin;
    upstream = createServer(async (request, response) => {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = chunks.length ? Buffer.concat(chunks) : undefined;
      const headers = Object.entries(request.headers).flatMap(([name, value]) => [value].flat().map((item) => [name, item]));
      const reply = await options.remoteFetch(new Request(`${remoteOrigin}${request.url}`, { method: request.method, headers, body }));
      response.writeHead(reply.status, Object.fromEntries(reply.headers));
      response.end(Buffer.from(await reply.arrayBuffer()));
    });
    await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
    config.remoteUrl = `http://127.0.0.1:${upstream.address().port}`;
  }
  const { remoteUrl = null, actorName = null, sharedKey = null, projectMappings = {} } = config;
  const file = { version: 1, remoteUrl, actorName, sharedKey, projectMappings };
  await writeFile(path.join(directory, "cloud-companion.json"), JSON.stringify(file));
  return upstream;
}
