import type { AgentSession, Task } from "./types";

export interface TaskConversationItem {
  key: string;
  projectId: string;
  agentSession: AgentSession;
  title: string;
  source: "task" | "comment";
  updatedAt: string;
}

export interface TaskProcessingPresentation {
  running: boolean;
  startedAt: string | null;
}

export interface TaskCardPresentation {
  conversations: TaskConversationItem[];
  processing: TaskProcessingPresentation;
  unread: boolean;
}

export function taskConversations(task: Task) {
  const items = new Map<string, TaskConversationItem>();
  for (const ref of task.conversationRefs ?? []) {
    const { platform, sessionId } = ref.agentSession;
    const key = `agent:${platform}:${sessionId}`;
    const current = items.get(key);
    if (current && current.updatedAt > ref.updatedAt) continue;
    items.set(key, {
      key,
      projectId: task.projectId,
      agentSession: ref.agentSession,
      title: ref.title || task.title,
      source: ref.source,
      updatedAt: ref.updatedAt,
    });
  }
  return [...items.values()].sort(
    (left, right) =>
      right.updatedAt.localeCompare(left.updatedAt) || left.key.localeCompare(right.key),
  );
}

export function taskCardPresentation(task: Task, unread: boolean): TaskCardPresentation {
  return {
    conversations: taskConversations(task),
    unread,
    processing: { running: task.status === "in_progress", startedAt: null },
  };
}
