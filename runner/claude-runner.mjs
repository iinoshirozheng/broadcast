#!/usr/bin/env node
// Claude runner: runs on the user's own machine with their own `claude` sign-in and talks to
// the board only through its HTTP API. It picks up `todo` issues assigned to Claude Agent and
// issues with a new comment that mentions @claude, claims one by moving it to `in_progress`
// (the version check makes the claim exclusive), runs `claude -p` in its working directory,
// posts the result as a comment, and moves the issue to `in_review` (or `blocked` when Claude
// fails).
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs } from "node:util";

const CLAUDE_AGENT_ID = "claude-agent";
const MENTION = /(^|\s)@claude\b/i;
// Issues already in progress, done or canceled are not picked up.
const SUMMONABLE_STATUSES = new Set(["backlog", "todo", "in_review", "blocked"]);

const { values: options } = parseArgs({
  options: {
    url: { type: "string", default: process.env.BROADCAST_URL ?? "http://127.0.0.1:47823" },
    cwd: { type: "string", default: process.cwd() },
    "permission-mode": { type: "string", default: "acceptEdits" },
    interval: { type: "string", default: "5" },
  },
});
const pollMs = Number(options.interval) * 1000;

let child = null;
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    child?.kill(signal);
    process.exit(130);
  });
}

function log(message) {
  console.log(`[claude-runner] ${new Date().toISOString()} ${message}`);
}

class ApiRequestError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(method, pathname, body) {
  const response = await fetch(new URL(pathname, options.url), {
    method,
    headers: {
      accept: "application/json",
      "content-type": "application/json",
      "x-broadcast-client": "claude-runner",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json().catch(() => null);
  if (!response.ok) {
    throw new ApiRequestError(response.status, data?.error?.message ?? `HTTP ${response.status}`);
  }
  return data;
}

function buildPrompt(task, comments, resuming) {
  return [
    resuming
      ? `New comments on Broadcast issue ${task.identifier}:`
      : `You are working on Broadcast issue ${task.identifier}: ${task.title}\n\n${task.description}`,
    ...comments.map((comment) => `${comment.authorName}: ${comment.body}`),
    "Do what is asked in the current directory. When you are done, reply concisely with the answer"
      + " or a summary of what you did and anything the reviewer should check, in the same language as the request.",
  ].join("\n\n");
}

function runClaude(prompt, sessionArgs) {
  return new Promise((resolve, reject) => {
    child = spawn("claude", [
      "-p", prompt,
      "--output-format", "json",
      ...sessionArgs,
      "--permission-mode", options["permission-mode"],
    ], { cwd: options.cwd, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.once("error", reject);
    child.once("close", (code) => {
      child = null;
      try {
        const result = JSON.parse(stdout);
        resolve({ ok: !result.is_error, text: String(result.result ?? "") });
      } catch {
        resolve({ ok: false, text: stderr.trim() || `claude exited with code ${code}` });
      }
    });
  });
}

async function work(task, agentSession, prompt, sessionArgs) {
  try {
    await api("POST", `/api/tasks/${task.id}/move`, {
      status: "in_progress",
      version: task.version,
      agentSession,
    });
  } catch (error) {
    if (error.status === 409) return log(`${task.identifier} was changed or claimed elsewhere; skipping`);
    throw error;
  }
  log(`${task.identifier} claimed; running claude (session ${agentSession.sessionId}) in ${options.cwd}`);

  let outcome;
  try {
    outcome = await runClaude(prompt, sessionArgs);
  } catch (error) {
    outcome = { ok: false, text: `Claude runner could not start claude: ${error.message}` };
  }

  await api("POST", `/api/tasks/${task.id}/comments`, { body: outcome.text, agentSession });
  const { task: current } = await api("GET", `/api/tasks/${task.id}`);
  const status = outcome.ok ? "in_review" : "blocked";
  await api("POST", `/api/tasks/${task.id}/move`, { status, version: current.version, agentSession });
  log(`${task.identifier} -> ${status}`);
}

log(`watching ${options.url} for issues delegated to or mentioning @claude`);
// Returns true when it handled an issue. Claude works on an issue when it is delegated
// (`todo` and assigned to Claude Agent) or when a comment since Claude's last reply mentions
// @claude. It resumes the Claude session already bound to the issue, or starts a new one.
async function workOnNextIssue() {
  const { tasks } = await api("GET", "/api/tasks");
  for (const task of tasks) {
    if (!SUMMONABLE_STATUSES.has(task.status)) continue;
    const { comments } = await api("GET", `/api/tasks/${task.id}/comments`);
    const pending = comments.slice(comments.findLastIndex((comment) => comment.authorId === CLAUDE_AGENT_ID) + 1);
    const delegated = task.status === "todo" && task.assignee.id === CLAUDE_AGENT_ID;
    if (!delegated && !pending.some((comment) => MENTION.test(comment.body))) continue;

    const resuming = task.agentSession?.platform === "claude";
    const agentSession = resuming ? task.agentSession : { platform: "claude", sessionId: randomUUID() };
    await work(task, agentSession, buildPrompt(task, pending, resuming), [
      resuming ? "--resume" : "--session-id",
      agentSession.sessionId,
    ]);
    return true;
  }
  return false;
}

for (;;) {
  try {
    if (await workOnNextIssue()) continue;
  } catch (error) {
    log(`error: ${error.message}`);
  }
  await sleep(pollMs);
}
