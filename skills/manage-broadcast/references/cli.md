# broadcast CLI

`broadcast` emits JSON for normal commands. Add `--json` when making the output contract explicit. Built-in help is the only successful stdout exception: it writes plain text, exits with code `0`, and does not request the Broadcast service.

Use built-in help for the current command tree or a specific supported level:

```bash
broadcast --help
broadcast issue --help
broadcast comment list --help
broadcast comment add --help
broadcast comment update --help
```

`broadcast` talks to the Broadcast HTTP API (`/api/tasks`, `/api/comments`, `/api/attachments`, …) at `BROADCAST_URL`, or `http://127.0.0.1:47823` when it is not set.

Except for built-in help, every successful command writes one JSON object with `schemaVersion` to stdout. The current schema version is `2`. Errors write one JSON object to stderr. Exit codes are `0` for success, `2` for invalid input, `3` when the service is unavailable, `4` for API or response errors, and `5` for conflicts.

## Projects

```bash
broadcast project list [--json]
broadcast project create --name NAME [--id ID] [--workspace-path PATH] [--json]
broadcast project readme get PROJECT_ID [--json]
broadcast project readme set PROJECT_ID (--content TEXT | --file PATH) [--if-version N] [--json]
```

Use `--workspace-path` to associate a project with a local repository. Use `project readme get` and `project readme set` to read and update the project's single root README document. Detailed multi-page documentation belongs in the project's local `docs/` folder.

## Agent session attribution

Issue and comment writes record the agent conversation that made them. Pass **both** `--agent-platform claude|codex|pi|agy|grok` and `--session-id ID`. Without them, `broadcast` records `CODEX_THREAD_ID` as a `codex` session when it is set. Read commands take no attribution.

Use the original tool and its full session ID. For Pi, `--session-id` also accepts the full session-file path; prefer an absolute path when copying between working directories.

```bash
broadcast issue create --project local --title "Session traceability" \
  --agent-platform claude --session-id '<claude-session-id>' --json

broadcast comment add ISSUE_ID --body 'Implementation notes' \
  --agent-platform codex --session-id '<codex-session-id>' --json
```

The same two options are accepted by `issue update`, `issue move`, `issue archive`, `issue restore`, `issue relation add|remove`, `comment update`, and `comment delete`. A metadata-only `issue update` is supported. Deleting a comment deletes its metadata.

Task and comment JSON expose `agentSession: { "platform": "claude", "sessionId": "..." }` or `null`. Omitting `agentSession` preserves the saved value; supplying a new one replaces it. Task `conversationRefs` lists the sessions on the task and its comments. The board copies these resume commands:

| Tool | Command |
| --- | --- |
| Claude Code | `claude --resume <session-id>` |
| Codex | `codex resume <session-id>` |
| Pi coding agent | `pi --session <path-or-id>` |
| Google Antigravity CLI (AGY) | `agy --conversation <conversation-id>` |
| xAI Grok CLI | `grok --resume <id>` |

IDs are limited to 256 characters, or 4096 for Pi paths/IDs. Empty values, control characters, and a leading option dash are rejected. Copied commands quote the ID as one POSIX-shell argument.

## Read issues

```bash
broadcast issue list [--project PROJECT_ID] [--status STATUS] [--archived true|false|all] [--json]
broadcast issue get ID [--json]
broadcast issue tree ID --direction descendants|ancestors --depth N [--json]
```

`issue tree` is a bounded structural read. `--depth 1` returns only direct children or the direct parent; larger values include that many levels, up to 25. The response is flat and deterministic: every node carries `id`, traversal `parentId`, `depth`, and `path` (usable as a breadcrumb), plus a small task summary. It never calculates status rollups or changes issues.

## Create issues

```bash
broadcast issue create \
  --project PROJECT_ID \
  --title TITLE \
  [--description TEXT | --description-file FILE] \
  [--status STATUS] \
  [--priority PRIORITY] \
  [--labels a,b] \
  [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]] \
  [--start-date YYYY-MM-DD] \
  [--due-date YYYY-MM-DD] \
  [--recurrence-interval N --recurrence-unit day|week|month|year] \
  [--json]
```

Statuses are `backlog`, `todo`, `in_progress`, `in_review`, `blocked`, `done`, and `canceled`. Priorities are `none`, `urgent`, `high`, `medium`, and `low`.

Issues created through `broadcast` are assigned to Codex Agent by default. Other CLI writes preserve the existing assignee.

## Update issues

Read the issue immediately before a write and pass its `version` with `--if-version`.

```bash
broadcast issue update ID \
  [--project PROJECT_ID] \
  [--title TITLE] \
  [--description TEXT | --description-file FILE] \
  [--status STATUS] \
  [--priority PRIORITY] \
  [--labels a,b] \
  [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]] \
  [--start-date YYYY-MM-DD] \
  [--due-date YYYY-MM-DD] \
  [--recurrence-interval N --recurrence-unit day|week|month|year] \
  [--if-version N] \
  [--json]

broadcast issue move ID --status STATUS [--if-version N] [--json]
broadcast issue archive ID [--if-version N] [--json]
broadcast issue restore ID [--if-version N] [--json]
```

Use `issue move` to set `in_progress` before implementation and `in_review` after implementation and self-verification. Do not move work directly from `in_progress` to `done`; use `done` only after the user explicitly confirms acceptance or explicitly asks to mark the issue complete. Use `blocked` when work cannot continue and `canceled` when it will not continue. On a version conflict, fetch the issue again and reconcile before retrying.

An issue has only one development context: `--git-branch`, or `--worktree-path` with an optional `--worktree-branch`. Issue JSON stores it as `developmentContext`, either `{ "type": "branch", "branch": "..." }` or `{ "type": "worktree", "path": "...", "branch": "..." }`. Recurrence requires a due date.

## Issue relations

Read the anchor issue immediately before adding or removing a relation and use its current version.

```bash
broadcast issue relation add ISSUE_ID \
  --type parent|blocks|blocked_by|related \
  --issue RELATED_ISSUE_ID \
  [--if-version N] \
  [--json]

broadcast issue relation remove ISSUE_ID \
  --type parent|blocks|blocked_by|related \
  --issue RELATED_ISSUE_ID \
  [--if-version N] \
  [--json]
```

For `--type parent`, `ISSUE_ID` is the child and `RELATED_ISSUE_ID` is its parent. Adding another parent replaces the child's current parent atomically.

For `blocks`, the anchor issue blocks the related issue. For `blocked_by`, the related issue blocks the anchor. `related` is symmetric. Self-relations, duplicates, parent cycles, and relations between different projects are rejected.

## Issue spec

```bash
broadcast issue spec get ISSUE_ID [--json]
broadcast issue spec set ISSUE_ID [--proposal-file FILE] [--specs-file FILE] [--design-file FILE] [--tasks-file FILE] [--json]
broadcast issue spec check ISSUE_ID TASK_NUMBER [--undo] [--json]
```

The spec is stored in Broadcast as four markdown sections: `proposal`, `specs`, `design` and `tasks`. `get` returns `{ spec }`, or `{ spec: null }` when the issue has none. `set` writes only the sections it is given and keeps the others; it needs at least one file. `check` checks off the first task whose number is `TASK_NUMBER` (such as `1.2`), and `--undo` unchecks it; an unknown number fails with `SPEC_TASK_NOT_FOUND`.

Spec writes do not change the issue `version` or add activity, so they take no `--if-version` or attribution options. `issue get` returns the parsed checklist as `spec.groups`. The board reads progress only from `tasks`: `## N. Group` headings and `- [ ] N.M Task` / `- [x] N.M Task` items.

## Issue comments

Use the issue id to read or append comments. Comment updates and deletes require the latest comment `version` returned by `comment list`.

```bash
broadcast comment list ISSUE_ID [--after CURSOR] [--json]
broadcast comment add ISSUE_ID (--body TEXT | --body-file FILE) [--json]
broadcast comment update COMMENT_ID --body TEXT --if-version N [--json]
broadcast comment delete COMMENT_ID --if-version N [--json]
```

Without `--after`, `comment list` returns the full list. Its response includes `nextCursor`; keep that value and pass it to the next read of the same issue to return only comments created or modified after that cursor. `--body-file` reads the UTF-8 file and passes its contents directly to the comment write path.

## Attachments

Issue descriptions and comments may contain inline images at exact positions in their Markdown:

```markdown
![alt text](api/attachments/ATTACHMENT_ID/content)
```

Upload a local file to a task or a comment. Provide exactly one of `--task` or `--comment`:

```bash
broadcast attachment list (--task TASK_ID | --comment COMMENT_ID) [--after CURSOR] [--json]
broadcast attachment upload --task TASK_ID --file PATH [--content-type TYPE] [--kind inline|attachment] [--json]
broadcast attachment upload --comment COMMENT_ID --file PATH [--content-type TYPE] [--kind inline|attachment] [--json]
```

Without `--after`, `attachment list` returns the full list for that task or comment. Its response includes `nextCursor`; keep a separate cursor for every attachment list target and pass it to the next read to return only later attachments.

If `--content-type` is omitted, the CLI guesses from the file extension and falls back to `application/octet-stream`. If `--kind` is omitted, images use `inline` and other files use `attachment`. Use `--kind attachment` for an image that must appear in the attachment list. An inline upload returns the attachment id; use that id in the task description or comment Markdown at the required position.

Download an attachment to an explicit local path:

```bash
broadcast attachment download ATTACHMENT_ID --output PATH [--json]
```

The command writes the response body as binary data and returns the absolute output path, content type, and size in its JSON result. Choose the output filename yourself; `broadcast` does not infer or append an extension.

## Agent runners

A runner works delegated issues on the user's own machine, with the agent CLI's own sign-in. It never sends agent credentials to the board.

```bash
broadcast runner claude [--url URL] [--cwd PATH] [--permission-mode MODE] [--interval SECONDS]
broadcast runner codex [--url URL] [--cwd PATH] [--sandbox MODE] [--interval SECONDS]
```

A runner picks up `todo` issues assigned to its agent, and issues in `backlog`, `todo`, `in_review`, or `blocked` with a new comment that mentions `@claude` or `@codex`. It claims the issue (`in_progress`), runs `claude -p` or `codex exec` in `--cwd` (default: the current directory), posts the answer as a comment with the agent session, and moves the issue to `in_review`, or `blocked` when the agent fails. A later mention resumes the same session. `--permission-mode` defaults to `acceptEdits`; `--sandbox` (`read-only`, `workspace-write`, or `danger-full-access`) defaults to `workspace-write`.
