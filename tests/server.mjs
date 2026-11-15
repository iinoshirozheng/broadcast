// Local E2E harness: boots an isolated Broadcast server on 127.0.0.1.
// - Data lives in a throwaway temp directory (never the developer's `.data`).
// - The Codex executable is pointed at a nonexistent path and PATH is empty, so a test can
//   never reach Codex or the network.
// - Static web UI is served from dist/web (build the web app first).
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const port = process.argv[2] ?? '4310';
const binary = process.env.BROADCAST_SERVER_BIN
  ?? fileURLToPath(new URL('../target/debug/broadcast-server', import.meta.url));
const directory = await mkdtemp(path.join(os.tmpdir(), 'broadcast-e2e-'));
const server = spawn(binary, [], {
  stdio: 'inherit',
  env: {
    PATH: '',
    HOME: directory,
    CODEX_HOME: directory,
    CODEX_EXECUTABLE: path.join(directory, 'disabled-codex'),
    BROADCAST_DATA_DIR: path.join(directory, 'data'),
    BROADCAST_HOST: '127.0.0.1',
    BROADCAST_PORT: port,
  },
});

let closing = false;
async function close(code = 0) {
  if (closing) return;
  closing = true;
  if (server.exitCode === null) {
    server.kill('SIGTERM');
    await new Promise((resolve) => server.once('exit', resolve));
  }
  await rm(directory, { recursive: true, force: true });
  process.exit(code);
}
server.once('exit', (code) => close(code ?? 1));
process.on('SIGINT', () => close());
process.on('SIGTERM', () => close());
