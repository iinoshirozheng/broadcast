import { useEffect, useState } from "react";
import { listRunners } from "../api";
import { isClaudeAgentId } from "../actors";
import { useBroadcastI18n } from "../i18n";
import type { Comment, RunnerPresence, Task } from "../types";

// The runner works on issues in these statuses when a new comment mentions @claude.
const SUMMONABLE_STATUSES = ["backlog", "todo", "in_review", "blocked"];
const MENTION = /(^|\s)@claude\b/i;
const RUNNER_POLL_MS = 15_000;

type Wait = { agentId: string; agentName: string } | { ownerId: string; agentName: string };

// The Claude Agent an issue waits for: its assignee while it is todo, or the runner of
// whoever mentioned @claude since Claude last replied.
function claudeWait(task: Task, comments: Comment[]): Wait | null {
  if (task.status === "todo" && isClaudeAgentId(task.assignee.id)) {
    return { agentId: task.assignee.id, agentName: task.assignee.name };
  }
  if (!SUMMONABLE_STATUSES.includes(task.status)) return null;
  let mention: Comment | undefined;
  for (const comment of comments) {
    if (isClaudeAgentId(comment.authorId)) mention = undefined;
    else if (MENTION.test(comment.body)) mention = comment;
  }
  return mention ? { ownerId: mention.authorId, agentName: "Claude Agent" } : null;
}

function servedBy(wait: Wait, runner: RunnerPresence) {
  if ("agentId" in wait) return runner.agentId === wait.agentId;
  return (
    isClaudeAgentId(runner.agentId) && (runner.ownerId === null || runner.ownerId === wait.ownerId)
  );
}

export function ClaudeRunnerNotice({ task, comments }: { task: Task; comments: Comment[] }) {
  const { text } = useBroadcastI18n();
  const wait = claudeWait(task, comments);
  const waiting = wait !== null;
  const [runners, setRunners] = useState<RunnerPresence[] | null>(null);

  useEffect(() => {
    if (!waiting) return;
    const controller = new AbortController();
    const load = () =>
      listRunners(controller.signal).then(setRunners, () => {
        // A board without runner presence shows no notice.
      });
    void load();
    const timer = window.setInterval(load, RUNNER_POLL_MS);
    return () => {
      controller.abort();
      window.clearInterval(timer);
    };
  }, [waiting]);

  if (!wait || !runners || runners.some((runner) => servedBy(wait, runner))) return null;
  return (
    <div className="runner-notice" role="status">
      <strong>
        {text(`${wait.agentName} 的 runner 未在线`, `${wait.agentName}'s runner is offline`)}
      </strong>
      <p>
        {text(
          "runner 上线后才会处理这个议题。在 Broadcast 桌面版托盘菜单勾选「Claude runner」，或在项目目录运行 ",
          "This issue is picked up once a runner is online. Turn on “Claude runner” in the Broadcast desktop tray menu, or run ",
        )}
        <code>broadcast runner claude</code>
        {text("。", " in your project folder.")}
      </p>
    </div>
  );
}
