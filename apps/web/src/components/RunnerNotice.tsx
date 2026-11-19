import { useEffect, useState } from "react";
import { listRunners } from "../api";
import { type MentionableAgent, MENTIONABLE_AGENTS, isAgentId } from "../actors";
import { useBroadcastI18n } from "../i18n";
import type { Comment, RunnerPresence, Task } from "../types";

// A runner works on issues in these statuses when a new comment mentions its agent.
const SUMMONABLE_STATUSES = ["backlog", "todo", "in_review", "blocked"];
const RUNNER_POLL_MS = 15_000;

type Wait = { agent: MentionableAgent; agentName: string } & (
  | { agentId: string }
  | { ownerId: string }
);

// The agent an issue waits for: its assignee while it is todo, or the runner of whoever
// mentioned the agent since it last replied.
function agentWait(task: Task, comments: Comment[], agent: MentionableAgent): Wait | null {
  if (task.status === "todo" && isAgentId(task.assignee.id, agent.actor)) {
    return { agent, agentId: task.assignee.id, agentName: task.assignee.name };
  }
  if (!SUMMONABLE_STATUSES.includes(task.status)) return null;
  const pattern = new RegExp(`(^|\\s)@${agent.handle}\\b`, "i");
  let mention: Comment | undefined;
  for (const comment of comments) {
    if (isAgentId(comment.authorId, agent.actor)) mention = undefined;
    else if (pattern.test(comment.body)) mention = comment;
  }
  return mention ? { agent, ownerId: mention.authorId, agentName: agent.actor.name } : null;
}

function servedBy(wait: Wait, runner: RunnerPresence) {
  if ("agentId" in wait) return runner.agentId === wait.agentId;
  return (
    isAgentId(runner.agentId, wait.agent.actor) &&
    (runner.ownerId === null || runner.ownerId === wait.ownerId)
  );
}

export function RunnerNotice({ task, comments }: { task: Task; comments: Comment[] }) {
  const { text } = useBroadcastI18n();
  const waits = MENTIONABLE_AGENTS.map((agent) => agentWait(task, comments, agent)).filter(
    (wait) => wait !== null,
  );
  const waiting = waits.length > 0;
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

  if (!runners) return null;
  const wait = waits.find((wait) => !runners.some((runner) => servedBy(wait, runner)));
  if (!wait) return null;
  return (
    <div className="runner-notice" role="status">
      <strong>
        {text(`${wait.agentName} 的 runner 未在线`, `${wait.agentName}'s runner is offline`)}
      </strong>
      <p>
        {text(
          "runner 上线后才会处理这个议题。在项目目录运行 ",
          "This issue is picked up once a runner is online. Run ",
        )}
        <code>broadcast runner {wait.agent.handle}</code>
        {text("。", " in your project folder.")}
      </p>
    </div>
  );
}
