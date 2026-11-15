import type { ActorIdentity, AssigneeTarget } from "./types";

export const CODEX_AGENT_ACTOR: ActorIdentity = {
  type: "agent",
  id: "codex-agent",
  name: "Codex Agent",
  avatarUrl: null,
};

export const CLAUDE_AGENT_ACTOR: ActorIdentity = {
  type: "agent",
  id: "claude-agent",
  name: "Claude Agent",
  avatarUrl: null,
};

export const AGENT_ACTORS = [CODEX_AGENT_ACTOR, CLAUDE_AGENT_ACTOR];

export interface MentionableAgent {
  actor: ActorIdentity;
  handle: string;
}

// Agents a comment can summon with @handle; the Claude runner acts on "@claude".
export const MENTIONABLE_AGENTS: readonly MentionableAgent[] = [
  { actor: CLAUDE_AGENT_ACTOR, handle: "claude" },
];

// Claude Agent is "claude-agent" on a local board and "<user id>:claude-agent" on a cloud board.
export function isClaudeAgentId(id: string): boolean {
  return id === CLAUDE_AGENT_ACTOR.id || id.endsWith(`:${CLAUDE_AGENT_ACTOR.id}`);
}

export function actorKey(actor: ActorIdentity): string {
  return `${actor.type}:${actor.id}`;
}

export function actorForAssigneeTarget(
  target: AssigneeTarget,
  currentUser: ActorIdentity,
): ActorIdentity {
  if (target === "codex-agent") return CODEX_AGENT_ACTOR;
  if (target === "claude-agent") return CLAUDE_AGENT_ACTOR;
  return currentUser;
}

export function assigneeTargetForActor(
  actor: ActorIdentity,
  currentUser: ActorIdentity,
): AssigneeTarget | undefined {
  if (actor.type === "agent") return isClaudeAgentId(actor.id) ? "claude-agent" : "codex-agent";
  return actor.id === currentUser.id ? "current-user" : undefined;
}
