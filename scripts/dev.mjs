import { spawn } from "node:child_process";

const children = [
  spawn("cargo", ["run", "-p", "broadcast-server"], {
    stdio: "inherit",
    env: { BROADCAST_HOST: "127.0.0.1", ...process.env },
  }),
  spawn(process.platform === "win32" ? "pnpm.cmd" : "pnpm", ["dev:web"], {
    stdio: "inherit",
  }),
];

let shuttingDown = false;

function stop(exitCode = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const child of children) child.kill("SIGTERM");
  process.exitCode = exitCode;
}

for (const child of children) {
  child.on("exit", (code, signal) => {
    if (!shuttingDown && code !== 0 && signal !== "SIGTERM") stop(code ?? 1);
  });
}

process.on("SIGINT", () => stop());
process.on("SIGTERM", () => stop());
