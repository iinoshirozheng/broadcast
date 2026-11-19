---
name: manage-broadcast
description: Manage Broadcast issues and broadcast setup when the request names Broadcast, e-broadcast, or broadcast, or the conversation already establishes that board as the target. Not for GitHub, Phabricator, other external trackers, or unrelated product docs.
---

# Manage Broadcast

This skill serves the Broadcast web board through its `broadcast` CLI.

Apply the workflow below only to work explicitly targeting Broadcast or already established as belonging to it in the conversation. An issue identifier, repository, or generic request to manage tasks, sync status, or add comments does not establish that scope. For GitHub, Phabricator, or another external tracker, use that system's tools and workflow; do not query, claim, or mirror its issues in Broadcast unless the user asks for that board operation. When the target is unclear, clarify it before running `broadcast`.

Within that scope, use `broadcast` for every project, issue, relation, and comment operation. Consume its JSON output. Use the exact issue identifier returned by the broadcast or supplied in the prompt. Never assume, derive, or rewrite an identifier prefix.

Open only the relevant section of [references/cli.md](references/cli.md) when command syntax is needed.

## Select the CLI and active service

- Use the exact `broadcast` binary and Broadcast URL supplied by the task or environment (`BROADCAST_URL`). Do not replace them with another CLI, the default port, or another board.
- If that exact command reaches a sandbox restriction on the service, retry the same command with the required permission. Do not switch binaries or endpoints.

## Core workflow

1. For an existing issue, first run `issue get` and `comment list`. Also run `attachment list --task`. On the first handoff, omit `--after` and read the full results. Keep the separate `nextCursor` from each list. When the same task resumes, run `issue get` again, then pass each saved cursor to its matching list with `--after` so only new or modified entries are returned. Comment lists include the attachments on returned comments; use `attachment list --comment` with its own cursor when a known comment attachment list can grow. Read the description and latest comments before deciding whether to start. Treat comments as current requirements, including returned work. If they say to wait, not execute, or not start now, stop and report without changing the status.
2. Treat `backlog` as not approved for execution. Unless the user explicitly authorizes that issue, do not claim it, move it to another status, or perform task work; its assignee alone is not authorization. If work may start, claim it before reading code, downloading attachments, analyzing the implementation, or doing any other task work. Move a claimable `todo` to `in_progress` with its current `version`; do not continue until the move succeeds. If it is already `in_progress`, continue only when its `agentSession` is the current conversation. Never move an issue claimed by another conversation.
3. If the move conflicts because the `version` is stale, run `issue get` and `comment list` again. Retry once with the latest `version` only when the issue is still a claimable `todo`, is not bound to another conversation, is not archived, and its description and latest comments are unchanged. If it was claimed, its status or requirements changed, it is archived, the service is unavailable, a permanent API error occurs, or the retry fails, stop and report. Never loop or take over another agent's claim.
4. For a new durable requirement, run `project list`. Select the project the user named, or the one whose `workspacePath` is the current directory or one of its ancestors, by its exact id; stop to ask if the result is ambiguous. The `local` project is the global fallback, not proof that the requirement belongs there. Search existing project issues before creating one in that confirmed project, then pass its explicit id to `issue create`. Update a matching issue instead of creating a duplicate. Use the fallback only when the user explicitly wants the global project. Do not track trivial requests.
5. Execute only the requested work in the issue's branch or worktree when one is bound.
6. Verify the requested operation path. Add a comment with the changes, verification result, outcome, and remaining risks. Read the issue again, then move it to `in_review` with its current `version`.
7. Move an issue to `done` only after the user explicitly accepts it or asks to complete it. Use `blocked` when work cannot continue and `canceled` when it will not continue.

## Issue specs

Use this when the user asks to split an issue into a spec first, or the issue already shows a spec. The spec lives in Broadcast, not in the repository: read it with `broadcast issue spec get ISSUE_ID` before working on the issue.

1. Write the proposal, specs, design (when needed) and tasks as markdown files in a scratch directory, then save them with `broadcast issue spec set ISSUE_ID --proposal-file F --specs-file F --design-file F --tasks-file F`. Write tasks as `## 1. Group` headings with `- [ ] 1.1 Task` items. When the user asked to split first, comment that the spec is ready and stop until the user confirms the tasks.
2. Create a sub-issue with a `parent` relation only for a task group that a different person or agent should claim on its own; order those sub-issues with `blocks`. Keep every other group in the parent issue.
3. While implementing, run `broadcast issue spec check ISSUE_ID TASK_NUMBER` after each finished task. To revise a section, `spec set` only that file; the other sections are kept.

## Other operations

- Run `broadcast project readme get [PROJECT_ID]` to inspect project architecture, constraints, and conventions before planning or executing complex tasks.
- Keep the project README focused on root overview and conventions; store detailed multi-page documentation in the local repository's `docs/` folder.
- Preserve existing issue scope when adding requirements or acceptance details.
- Add only relations that the work requires. Use parent for contained work, blocks or blocked_by for dependencies, and related for close association.
- Attribute every issue and comment write to the current conversation with `--agent-platform claude|codex|pi|agy|grok --session-id ID`; Codex can rely on `CODEX_THREAD_ID` instead. See [agent session attribution](references/cli.md#agent-session-attribution).
- Use the latest returned `version` with `--if-version` for concurrent updates. On conflict, read the issue again and reconcile before retrying.
- Download and inspect an inline `![alt](api/attachments/<id>/content)` image only when it is needed to understand the requirement.
