import { useEffect, useState } from "react";
import { getTaskSpec } from "../api";
import { useBroadcastI18n } from "../i18n";
import type { Task, TaskSpec } from "../types";
import { MarkdownDocument } from "./MarkdownDocument";
import { SpecFileIcon, SpecSegments, specCounts } from "./TaskSpecProgress";

type SpecTab = "tasks" | "proposal" | "specs" | "design";

function TaskMark({ state }: { state: "done" | "active" | "todo" }) {
  if (state === "done")
    return (
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
        <circle cx="8" cy="8" r="7" fill="#00a558" />
        <path
          d="M5 8.2l2 2 4-4.4"
          fill="none"
          stroke="#fff"
          strokeWidth="1.7"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    );
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <circle
        cx="8"
        cy="8"
        r="6.25"
        fill="none"
        stroke={state === "active" ? "#00a558" : "var(--text-tertiary)"}
        strokeWidth="1.5"
      />
      {state === "active" && <circle cx="8" cy="8" r="3" fill="#00a558" />}
    </svg>
  );
}

// The spec an agent wrote onto the issue; the board only displays it.
export function TaskSpecPanel({
  task,
  updatedAgo,
  onDraft,
}: {
  task: Task;
  updatedAgo: string | null;
  onDraft: () => void;
}) {
  const { text } = useBroadcastI18n();
  const summary = task.spec;
  const [tab, setTab] = useState<SpecTab>("tasks");
  const [spec, setSpec] = useState<TaskSpec | null>(null);

  useEffect(() => {
    if (!summary) return;
    const controller = new AbortController();
    getTaskSpec(task.id, controller.signal)
      .then(setSpec)
      .catch(() => undefined);
    return () => controller.abort();
  }, [task.id, summary?.updatedAt]);

  if (!summary)
    return (
      <section className="issue-spec is-empty" aria-labelledby="issue-spec-heading">
        <SpecFileIcon />
        <div>
          <h2 id="issue-spec-heading">Spec</h2>
          <p>
            {text(
              "还没有 Spec。较大的需求可以先让 agent 拆成提案、规格、设计和任务。",
              "No spec yet. For larger work, let an agent split it into a proposal, specs, design and tasks first.",
            )}
          </p>
        </div>
        <button type="button" onClick={onDraft}>
          {text("复制拆分指令", "Copy split instruction")}
        </button>
      </section>
    );

  const { done, total } = specCounts(summary);
  const tasks = summary.groups.flatMap((group) => group.tasks);
  const active = task.status === "in_progress" ? tasks.find((item) => !item.done) : undefined;
  const tabs: { id: SpecTab; label: string; count?: number }[] = [
    { id: "tasks", label: text("任务", "Tasks"), count: total },
    { id: "proposal", label: text("提案", "Proposal") },
    { id: "specs", label: text("规格", "Specs") },
    { id: "design", label: text("设计", "Design") },
  ];
  const document =
    tab === "proposal"
      ? spec?.proposal
      : tab === "design"
        ? spec?.design
        : tab === "specs"
          ? spec?.specs
          : undefined;

  return (
    <section className="issue-spec" aria-labelledby="issue-spec-heading">
      <header>
        <div className="issue-spec-title">
          <SpecFileIcon />
          <h2 id="issue-spec-heading">Spec</h2>
        </div>
        {updatedAgo && (
          <div className="issue-spec-meta">
            <span>{text(`agent 更新于 ${updatedAgo}`, `Updated by agent ${updatedAgo}`)}</span>
          </div>
        )}
        <div className="issue-spec-progress">
          <SpecSegments spec={summary} status={task.status} />
          <span>
            {done}/{total}
          </span>
        </div>
      </header>

      <div className="issue-spec-tabs" role="tablist" aria-label={text("变更文件", "Change files")}>
        {tabs.map((item) => (
          <button
            key={item.id}
            type="button"
            role="tab"
            aria-selected={tab === item.id}
            onClick={() => setTab(item.id)}
          >
            {item.label}
            {item.count !== undefined && <span>{item.count}</span>}
          </button>
        ))}
      </div>

      <div className="issue-spec-panel" role="tabpanel">
        {tab === "tasks" ? (
          <>
            {summary.groups.map((group, groupIndex) => (
              <div className="issue-spec-group" key={`${groupIndex}-${group.title}`}>
                {group.title && (
                  <div className="issue-spec-group-head">
                    <span>{group.title}</span>
                    <span>
                      {group.tasks.filter((item) => item.done).length}/{group.tasks.length}
                    </span>
                  </div>
                )}
                {group.tasks.map((item, index) => (
                  <div
                    className={`issue-spec-task${item === active ? " is-active" : ""}`}
                    key={`${index}-${item.number}`}
                  >
                    <TaskMark state={item.done ? "done" : item === active ? "active" : "todo"} />
                    {item.number && <span className="issue-spec-task-number">{item.number}</span>}
                    <span>{item.title}</span>
                  </div>
                ))}
              </div>
            ))}
            <p className="issue-spec-note">
              {text(
                `${done}/${total} 完成。任务由 agent 用 broadcast issue spec check 勾选，看板上只读显示。`,
                `${done}/${total} done. The agent checks tasks off with broadcast issue spec check; the board only displays it.`,
              )}
            </p>
          </>
        ) : document ? (
          <MarkdownDocument value={document} />
        ) : (
          <p className="issue-spec-note">
            {spec
              ? text("这个 Spec 还没有这一部分。", "This spec has no such section yet.")
              : text("正在加载…", "Loading…")}
          </p>
        )}
      </div>
    </section>
  );
}
