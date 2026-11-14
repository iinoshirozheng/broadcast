pub const TASK_STATUSES: [&str; 7] =
    ["backlog", "todo", "in_progress", "in_review", "blocked", "done", "canceled"];
pub const TASK_PRIORITIES: [&str; 5] = ["none", "urgent", "high", "medium", "low"];
pub const AI_SANDBOX_MODES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];

pub const DEFAULT_PROJECT_ID: &str = "local";
pub const JIRA_PROJECT_ID: &str = "jira-my-tasks";
pub const DEFAULT_LABEL_NAMES: [&str; 11] = [
    "缺陷", "特性", "for-claude", "hold", "改进", "phase-1", "phase-2", "phase-3", "phase-4",
    "phase-5", "phase-6",
];

pub fn is_task_status(value: &str) -> bool {
    TASK_STATUSES.contains(&value)
}

pub fn is_task_priority(value: &str) -> bool {
    TASK_PRIORITIES.contains(&value)
}
