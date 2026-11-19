import { useBroadcastI18n } from "../i18n";
import type { TaskSpecSummary, TaskStatus } from "../types";

export function SpecFileIcon() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M4 2.5h5l3 3V13a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5V3a.5.5 0 0 1 .5-.5z" />
      <path d="M9 2.5v3h3" />
    </svg>
  );
}

export function specCounts(spec: TaskSpecSummary) {
  const tasks = spec.groups.flatMap((group) => group.tasks);
  return { done: tasks.filter((task) => task.done).length, total: tasks.length };
}

// One segment per tasks.md task; the wider gaps fall between its `##` groups.
export function SpecSegments({ spec, status }: { spec: TaskSpecSummary; status: TaskStatus }) {
  const { text } = useBroadcastI18n();
  const { done, total } = specCounts(spec);
  const active =
    status === "in_progress"
      ? spec.groups.flatMap((group) => group.tasks).find((task) => !task.done)
      : undefined;
  return (
    <div
      className="task-spec-segments"
      role="img"
      aria-label={text(`任务进度 ${done}/${total}`, `Task progress ${done}/${total}`)}
    >
      {spec.groups.map((group, groupIndex) => (
        <div
          className="task-spec-group"
          key={`${groupIndex}-${group.title}`}
          style={{ flexGrow: group.tasks.length }}
        >
          {group.tasks.map((task, taskIndex) => (
            <span
              className={task === active ? "is-active" : task.done ? "is-complete" : ""}
              key={`${taskIndex}-${task.number}`}
            />
          ))}
        </div>
      ))}
    </div>
  );
}

export function TaskSpecProgress({ spec, status }: { spec: TaskSpecSummary; status: TaskStatus }) {
  const { text } = useBroadcastI18n();
  const { done, total } = specCounts(spec);
  if (total === 0) return null;
  const outline = spec.groups
    .map((group) =>
      [
        group.title,
        ...group.tasks.map((task) => `${task.done ? "✓" : "○"} ${task.number} ${task.title}`),
      ]
        .filter(Boolean)
        .join("\n"),
    )
    .join("\n\n");
  const chip =
    done === total
      ? { className: "is-archivable", label: text("任务已完成", "Tasks done") }
      : done === 0 && status !== "in_progress"
        ? { className: "is-ready", label: text("Spec 已就绪", "Spec ready") }
        : null;

  return (
    <div className="task-spec" title={outline}>
      <div className="task-spec-head">
        <SpecFileIcon />
        <span className="task-spec-change">Spec</span>
        <span className="task-spec-count">
          {done}/{total}
        </span>
      </div>
      <SpecSegments spec={spec} status={status} />
      {chip && <span className={`task-spec-chip ${chip.className}`}>{chip.label}</span>}
    </div>
  );
}
