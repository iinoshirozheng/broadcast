import { execFile as execFileCallback } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import {
  type AdapterContext,
  CLOUD_PROJECT_COUNTS_SQL,
  CLOUD_PROJECT_READMES_SQL,
  createCloudD1ImportSql,
  type D1Adapter,
  type R2Adapter,
} from "./migrate-to-cloud.ts";

type RunCommand = (
  file: string,
  args: string[],
  options: { cwd: string; encoding: "utf8"; maxBuffer: number },
) => Promise<{ stdout: string }>;
type WranglerAdapterOptions = {
  remote?: boolean;
  persistTo?: string;
  configPath?: string;
  wranglerExecutable?: string;
  database?: string;
  bucket?: string;
  preparedImportSql?: string;
  environment?: NodeJS.ProcessEnv;
  runCommand?: RunCommand;
};

const execFile = promisify(execFileCallback);
const projectRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const defaultWrangler = path.join(projectRoot, "node_modules", "wrangler", "bin", "wrangler.js");

function parseD1Results(stdout: string) {
  const parsed = JSON.parse(stdout);
  const resultSets = Array.isArray(parsed) ? parsed : [parsed];
  return resultSets.flatMap((result) => result.results ?? result.result?.[0]?.results ?? []);
}

function missingR2Object(failure: unknown) {
  const error = failure as { message?: string; stdout?: string; stderr?: string } | null;
  const output = `${error?.message ?? ""}\n${error?.stdout ?? ""}\n${error?.stderr ?? ""}`;
  return /not found|does not exist|NoSuchKey|10007/i.test(output);
}

export function createWranglerCloudAdapters({
  remote,
  persistTo,
  configPath,
  wranglerExecutable = defaultWrangler,
  database = "broadcast-db",
  bucket = "broadcast-attachments",
  preparedImportSql,
  environment = process.env,
  runCommand = execFile,
}: WranglerAdapterOptions = {}) {
  const remoteEnabled = environment.BROADCAST_MIGRATION_REMOTE === "1";
  const useRemote = remote ?? remoteEnabled;
  if (useRemote && !remoteEnabled) {
    throw new Error("Remote migration requires BROADCAST_MIGRATION_REMOTE=1");
  }

  const resolvedPersistTo = persistTo ?? environment.BROADCAST_MIGRATION_PERSIST_TO;
  if (!useRemote && !resolvedPersistTo) {
    throw new Error("Local migration requires BROADCAST_MIGRATION_PERSIST_TO");
  }
  const resolvedConfig = path.resolve(
    configPath ??
      environment.BROADCAST_MIGRATION_CONFIG ??
      path.join(projectRoot, "wrangler.jsonc"),
  );
  const modeArguments = useRemote
    ? ["--remote"]
    : ["--local", "--persist-to", path.resolve(resolvedPersistTo!)];
  const temporaryDirectory = mkdtemp(path.join(os.tmpdir(), "broadcast-wrangler-migration-")).then(
    async (directory) => {
      await chmod(directory, 0o700);
      return directory;
    },
  );
  let commandQueue: Promise<unknown> = Promise.resolve();
  let sequence = 0;

  function run(args: string[]) {
    const result = commandQueue.then(() =>
      runCommand(process.execPath, [wranglerExecutable, ...args], {
        cwd: projectRoot,
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
      }),
    );
    commandQueue = result.catch(() => {});
    return result;
  }

  async function privateFile(extension: string, contents: string | Buffer) {
    const directory = await temporaryDirectory;
    sequence += 1;
    const filename = path.join(directory, `${sequence}${extension}`);
    await writeFile(filename, contents, { flag: "wx", mode: 0o600 });
    await chmod(filename, 0o600);
    return filename;
  }

  const d1 = {
    async importTables(tables) {
      const sqlPath = await privateFile(
        ".sql",
        `${preparedImportSql ?? createCloudD1ImportSql(tables)}\n`,
      );
      await run([
        "d1",
        "execute",
        database,
        ...modeArguments,
        "--file",
        sqlPath,
        "--yes",
        "--config",
        resolvedConfig,
      ]);
    },
    async countByProject() {
      const result = await run([
        "d1",
        "execute",
        database,
        ...modeArguments,
        "--command",
        CLOUD_PROJECT_COUNTS_SQL,
        "--json",
        "--config",
        resolvedConfig,
      ]);
      return Object.fromEntries(
        parseD1Results(result.stdout).map((row) => [
          row.project_id,
          {
            projects: Number(row.projects),
            project_readmes: Number(row.project_readmes),
            tasks: Number(row.tasks),
            comments: Number(row.comments),
            task_relations: Number(row.task_relations),
            attachments: Number(row.attachments),
          },
        ]),
      );
    },
    async listProjectReadmes() {
      const readmes: Record<string, unknown>[] = [];
      for (let offset = 0; ; offset += 1) {
        const result = await run([
          "d1",
          "execute",
          database,
          ...modeArguments,
          "--command",
          `${CLOUD_PROJECT_READMES_SQL} LIMIT 1 OFFSET ${offset}`,
          "--json",
          "--config",
          resolvedConfig,
        ]);
        const rows = parseD1Results(result.stdout);
        if (rows.length === 0) return readmes;
        readmes.push(rows[0]);
      }
    },
  } satisfies D1Adapter;

  const r2 = {
    async put(key, body, options?) {
      const bodyPath = await privateFile(".attachment", Buffer.from(body));
      const contentType = options?.httpMetadata?.contentType;
      await run([
        "r2",
        "object",
        "put",
        `${bucket}/${key}`,
        ...modeArguments,
        "--file",
        bodyPath,
        "--force",
        ...(contentType ? ["--content-type", contentType] : []),
        "--config",
        resolvedConfig,
      ]);
    },
    async head(key) {
      const directory = await temporaryDirectory;
      sequence += 1;
      const outputPath = path.join(directory, `${sequence}.download`);
      try {
        await run([
          "r2",
          "object",
          "get",
          `${bucket}/${key}`,
          ...modeArguments,
          "--file",
          outputPath,
          "--config",
          resolvedConfig,
        ]);
      } catch (error) {
        if (missingR2Object(error)) return null;
        throw error;
      }
      await chmod(outputPath, 0o600);
      const body = await readFile(outputPath);
      return {
        size: body.byteLength,
        customMetadata: {
          sha256: createHash("sha256").update(body).digest("hex"),
        },
      };
    },
    async delete(key) {
      await run([
        "r2",
        "object",
        "delete",
        `${bucket}/${key}`,
        ...modeArguments,
        "--force",
        "--config",
        resolvedConfig,
      ]);
    },
  } satisfies R2Adapter;

  return {
    d1,
    r2,
    async cleanup() {
      await rm(await temporaryDirectory, { recursive: true, force: true });
    },
  };
}

export function createCloudMigrationAdapters({ bundle, command }: AdapterContext) {
  const preparedImportSql =
    command === "import" ? createCloudD1ImportSql(bundle.tables) : undefined;
  return createWranglerCloudAdapters({ preparedImportSql });
}

export default createCloudMigrationAdapters;
