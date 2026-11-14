import type { ActorIdentity } from "../types";

export function ActorAvatar({
  actor,
  className = "",
}: {
  actor: ActorIdentity;
  className?: string;
}) {
  const kind = actor.id === "claude-agent" ? "claude-agent" : actor.type;
  return (
    <span
      className={`actor-avatar actor-avatar-${kind}${className ? ` ${className}` : ""}`}
      aria-hidden="true"
      title={actor.name}
    >
      {kind === "agent" ? (
        <img
          className="actor-avatar-image actor-avatar-agent-image"
          src="codex-agent-logo.png"
          alt=""
        />
      ) : actor.avatarUrl ? (
        <img
          className="actor-avatar-image"
          src={actor.avatarUrl}
          alt=""
          referrerPolicy="no-referrer"
        />
      ) : actor.name.slice(0, 1)}
    </span>
  );
}
