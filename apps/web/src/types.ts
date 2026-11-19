export const TASK_STATUSES = [
  "backlog",
  "todo",
  "in_progress",
  "in_review",
  "blocked",
  "done",
  "canceled",
] as const;
export const TASK_PRIORITIES = ["none", "urgent", "high", "medium", "low"] as const;

export type TaskStatus = (typeof TASK_STATUSES)[number];
export type TaskPriority = (typeof TASK_PRIORITIES)[number];
export type ActorType = "user" | "agent";
export type AssigneeTarget = "current-user" | "codex-agent" | "claude-agent";
export type IssueRelationType = "parent" | "blocks" | "blocked_by" | "related";
export type IssueRelationOrigin = "manual" | "mention";

export interface ActorIdentity {
  type: ActorType;
  id: string;
  name: string;
  avatarUrl: string | null;
}

export type DevelopmentContext =
  | { type: "branch"; branch: string }
  | { type: "worktree"; path: string; branch: string | null };

export type Recurrence = {
  interval: number;
  unit: "day" | "week" | "month" | "year";
};

export interface Project {
  id: string;
  name: string;
  workspacePath: string | null;
  source: "local" | "jira";
  labels: string[];
  issueCount: number;
  createdAt: string;
  updatedAt: string;
}

export interface ProjectReadme {
  projectId: string;
  content: string;
  version: number;
  createdAt: string | null;
  updatedAt: string | null;
}

export interface ProjectReadmeAttachment {
  id: string;
  projectId: string;
  kind: "inline";
  filename: string;
  contentType: string;
  size: number;
  createdAt: string;
}

export interface TaskRelationSummary {
  id: string;
  identifier: string;
  externalKey?: string | null;
  projectId: string;
  title: string;
  status: TaskStatus;
  priority: TaskPriority;
  assignee: ActorIdentity;
  archivedAt: string | null;
}

export interface TaskRelations {
  parent: TaskRelationSummary | null;
  subIssues: TaskRelationSummary[];
  blockedBy: TaskRelationSummary[];
  blocks: TaskRelationSummary[];
  related: TaskRelationSummary[];
}

export type AgentPlatform = "claude" | "codex" | "pi" | "agy" | "grok";

export interface AgentSession {
  platform: AgentPlatform;
  sessionId: string;
}

export interface TaskConversationRef {
  agentSession: AgentSession;
  source: "task" | "comment";
  sourceId: string;
  title: string;
  updatedAt: string;
}

// The spec an agent writes onto the task (`broadcast issue spec set`), stored in Broadcast.
export interface TaskSpecTask {
  number: string;
  title: string;
  done: boolean;
}

export interface TaskSpecGroup {
  title: string;
  tasks: TaskSpecTask[];
}

export interface TaskSpecSummary {
  updatedAt: string;
  groups: TaskSpecGroup[];
}

export interface TaskSpec {
  taskId: string;
  proposal: string;
  specs: string;
  design: string;
  tasks: string;
  updatedAt: string;
}

export interface Task {
  id: string;
  identifier: string;
  projectId: string;
  title: string;
  description: string;
  status: TaskStatus;
  priority: TaskPriority;
  labels: string[];
  sortOrder: number;
  agentSession?: AgentSession | null;
  conversationRefs: TaskConversationRef[];
  participants: ActorIdentity[];
  previewImage: Attachment | null;
  spec?: TaskSpecSummary | null;
  activityKey: string;
  activityUpdatedAt: string;
  creatorType: ActorType;
  creatorId: string;
  creatorName: string;
  creatorAvatarUrl: string | null;
  assignee: ActorIdentity;
  developmentContext: DevelopmentContext | null;
  startDate: string | null;
  dueDate: string | null;
  recurrence: Recurrence | null;
  source: "local" | "jira";
  externalOrigin?: string | null;
  externalKey?: string | null;
  externalUrl: string | null;
  archivedAt: string | null;
  relations: TaskRelations;
  version: number;
  createdAt: string;
  updatedAt: string;
}

export interface JiraConnection {
  configured: boolean;
  baseUrl: string | null;
  username: string | null;
  displayName: string | null;
  projects: string[];
  projectId: string;
  lastSyncedAt: string | null;
  insecureHttp: boolean;
}

// An agent runner that sent a heartbeat recently; ownerId is the user it works for on a cloud board.
export interface RunnerPresence {
  agentId: string;
  agentName: string;
  ownerId: string | null;
  lastSeenAt: string;
}

export interface Comment {
  id: string;
  taskId: string;
  body: string;
  authorType: ActorType;
  authorId: string;
  authorName: string;
  authorAvatarUrl: string | null;
  agentSession?: AgentSession | null;
  attachments: Attachment[];
  version: number;
  createdAt: string;
  updatedAt: string;
}

export interface TaskActivityChange {
  field: string;
  before: unknown;
  after: unknown;
}

export interface TaskChangeActivity {
  id: string;
  taskId: string;
  actorType: ActorType;
  actorId: string;
  actorName: string;
  actorAvatarUrl: string | null;
  changes: TaskActivityChange[];
  createdAt: string;
}

export interface Attachment {
  id: string;
  taskId: string;
  commentId: string | null;
  kind: "inline" | "attachment";
  bodyFallback: boolean;
  filename: string;
  contentType: string;
  size: number;
  createdAt: string;
}

export interface TaskDraft {
  title: string;
  description: string;
  status: TaskStatus;
  priority: TaskPriority;
  labels: string[];
  assigneeTarget?: AssigneeTarget;
  developmentContext: DevelopmentContext | null;
  startDate: string | null;
  dueDate: string | null;
  recurrence: Recurrence | null;
}
