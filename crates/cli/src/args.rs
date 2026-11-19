//! The argument contract: `RESOURCE ACTION [operands] [--name value | --name=value | --flag]`.

use serde_json::{Map, Value};

use crate::error::{Result, usage};

const BOOLEAN_OPTIONS: [&str; 3] = ["json", "help", "undo"];
const ATTRIBUTION: [&str; 2] = ["agent-platform", "session-id"];
const ISSUE_FIELDS: [&str; 14] = [
    "project",
    "title",
    "description",
    "description-file",
    "status",
    "priority",
    "labels",
    "git-branch",
    "worktree-path",
    "worktree-branch",
    "start-date",
    "due-date",
    "recurrence-interval",
    "recurrence-unit",
];

pub const UNKNOWN_COMMAND: &str = "Expected one of: project list/create/readme, issue list/get/create/update/move/archive/restore/tree/relation/spec, comment list/add/update/delete, attachment list/download/upload, runner claude/codex";

pub struct Parsed {
    pub resource: Option<String>,
    pub action: Option<String>,
    pub operands: Vec<String>,
    /// Flags are `true`; every other option is a string.
    pub options: Map<String, Value>,
}

impl Parsed {
    pub fn command(&self) -> String {
        format!(
            "{} {}",
            self.resource.as_deref().unwrap_or(""),
            self.action.as_deref().unwrap_or("")
        )
        .trim()
        .to_owned()
    }

    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.get(name).and_then(Value::as_str)
    }

    pub fn has(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.options
            .get(name)
            .is_some_and(|value| value == &Value::Bool(true))
    }

    pub fn required(&self, name: &str) -> Result<&str> {
        self.option(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| usage(format!("Missing required option --{name}")))
    }

    pub fn operand(&self, index: usize) -> &str {
        self.operands.get(index).map_or("", String::as_str)
    }

    pub fn expect_operands(&self, expected: usize) -> Result<()> {
        if self.operands.len() == expected {
            return Ok(());
        }
        let command = format!(
            "{} {}",
            self.resource.as_deref().unwrap_or(""),
            self.action.as_deref().unwrap_or("")
        );
        Err(usage(match expected {
            0 => format!("{command} does not accept positional arguments"),
            1 => format!("{command} requires exactly 1 positional argument"),
            _ => format!("{command} requires exactly {expected} positional arguments"),
        }))
    }

    pub fn validate_options(&self, allowed: &[&str]) -> Result<()> {
        for name in broadcast_domain::js::object_keys(&self.options) {
            if !allowed.contains(&name) {
                return Err(usage(format!("Unknown option --{name}")));
            }
        }
        Ok(())
    }
}

pub fn parse(argv: &[String]) -> Result<Parsed> {
    let mut positionals = Vec::new();
    let mut options = Map::new();
    let mut tokens = argv.iter();
    while let Some(token) = tokens.next() {
        if token == "--" {
            positionals.extend(tokens.by_ref().cloned());
            break;
        }
        let Some(option) = token.strip_prefix("--") else {
            positionals.push(token.clone());
            continue;
        };
        let (name, inline) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        if name.is_empty() {
            return Err(usage("Invalid empty option"));
        }
        if options.contains_key(name) {
            return Err(usage(format!("Option --{name} may only be specified once")));
        }
        let value = if BOOLEAN_OPTIONS.contains(&name) {
            if inline.is_some() {
                return Err(usage(format!("Option --{name} does not accept a value")));
            }
            Value::Bool(true)
        } else if let Some(value) = inline {
            Value::from(value)
        } else {
            match tokens.as_slice().first() {
                Some(value) if !value.starts_with("--") => {
                    let value = Value::from(value.as_str());
                    tokens.next();
                    value
                }
                _ => return Err(usage(format!("Option --{name} requires a value"))),
            }
        };
        options.insert(name.to_owned(), value);
    }
    let mut positionals = positionals.into_iter();
    Ok(Parsed {
        resource: positionals.next(),
        action: positionals.next(),
        operands: positionals.collect(),
        options,
    })
}

/// The options each command accepts besides the global ones.
pub fn allowed_options(command: &str) -> Option<Vec<&'static str>> {
    let with = |parts: &[&[&'static str]]| Some(parts.concat());
    match command {
        "project list" | "issue get" => with(&[&["json"]]),
        "project create" => with(&[&["id", "name", "workspace-path", "json"]]),
        "project readme" => with(&[&["content", "file", "if-version", "json"]]),
        "issue list" => with(&[&["project", "status", "archived", "json"]]),
        "issue create" => with(&[&ISSUE_FIELDS, &ATTRIBUTION, &["json"]]),
        "issue update" => with(&[&ISSUE_FIELDS, &ATTRIBUTION, &["if-version", "json"]]),
        "issue move" => with(&[&["status"], &ATTRIBUTION, &["if-version", "json"]]),
        "issue archive" | "issue restore" | "comment delete" => {
            with(&[&ATTRIBUTION, &["if-version", "json"]])
        }
        "issue tree" => with(&[&["direction", "depth", "json"]]),
        "issue spec" => with(&[&[
            "proposal-file",
            "specs-file",
            "design-file",
            "tasks-file",
            "undo",
            "json",
        ]]),
        "issue relation" => with(&[&["type", "issue"], &ATTRIBUTION, &["if-version", "json"]]),
        "comment list" => with(&[&["after", "json"]]),
        "comment add" => with(&[&["body", "body-file"], &ATTRIBUTION, &["json"]]),
        "comment update" => with(&[&["body"], &ATTRIBUTION, &["if-version", "json"]]),
        "attachment list" => with(&[&["task", "comment", "after", "json"]]),
        "attachment download" => with(&[&["output", "json"]]),
        "attachment upload" => {
            with(&[&["file", "task", "comment", "content-type", "kind", "json"]])
        }
        "runner claude" => with(&[&["url", "cwd", "permission-mode", "interval"]]),
        "runner codex" => with(&[&["url", "cwd", "sandbox", "interval"]]),
        _ => None,
    }
}

/// Help text for each supported command level.
pub fn help(scope: &str) -> Option<&'static str> {
    Some(match scope {
        "" => {
            r#"Usage: broadcast RESOURCE ACTION [options]

Commands:
  project list
  project create --name NAME [--id ID] [--workspace-path PATH]
  project readme get PROJECT_ID
  project readme set PROJECT_ID (--content TEXT | --file FILE) [--if-version N]
  issue list|get|create|update|move|archive|restore|tree|relation|spec
  comment list ISSUE_ID [--after CURSOR]
  comment add ISSUE_ID (--body TEXT | --body-file FILE)
  comment update COMMENT_ID --body TEXT --if-version N
  comment delete COMMENT_ID --if-version N
  attachment list (--task ISSUE_ID | --comment COMMENT_ID) [--after CURSOR]
  attachment download ATTACHMENT_ID --output PATH
  attachment upload --file PATH (--task ISSUE_ID | --comment COMMENT_ID)
  runner claude [--url URL] [--cwd PATH] [--permission-mode MODE] [--interval SECONDS]
  runner codex [--url URL] [--cwd PATH] [--sandbox MODE] [--interval SECONDS]

Options:
  --json  Make the JSON output contract explicit
  --help  Show help for a supported command level

The service is BROADCAST_URL, or http://127.0.0.1:47823 when it is not set.

Examples:
  broadcast issue get LOCAL-275 --json
  broadcast comment list LOCAL-275 --json

Agent conversation attribution for issue/comment writes:
  --agent-platform claude|codex|pi|agy|grok --session-id ID
  Without them, CODEX_THREAD_ID is recorded as a codex session when it is set.

Run broadcast issue --help for all issue arguments."#
        }
        "issue" => {
            r#"Usage: broadcast issue ACTION [arguments] [options]

Actions:
  list [--project PROJECT_ID] [--status STATUS] [--archived true|false|all] [--json]
  get ISSUE_ID [--json]
  create --project PROJECT_ID --title TITLE
    [--description TEXT | --description-file FILE]
    [--status STATUS] [--priority PRIORITY] [--labels a,b]
    [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]]
    [--start-date YYYY-MM-DD] [--due-date YYYY-MM-DD]
    [--recurrence-interval N --recurrence-unit day|week|month|year] [--json]
  update ISSUE_ID
    [--project PROJECT_ID] [--title TITLE]
    [--description TEXT | --description-file FILE]
    [--status STATUS] [--priority PRIORITY] [--labels a,b]
    [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]]
    [--start-date YYYY-MM-DD] [--due-date YYYY-MM-DD]
    [--recurrence-interval N --recurrence-unit day|week|month|year]
    [--if-version N] [--json]
  move ISSUE_ID --status STATUS [--if-version N] [--json]
  archive ISSUE_ID [--if-version N] [--json]
  restore ISSUE_ID [--if-version N] [--json]
  tree ISSUE_ID --direction descendants|ancestors --depth N [--json]
  relation add|remove ISSUE_ID --type parent|blocks|blocked_by|related
    --issue RELATED_ISSUE_ID [--if-version N] [--json]
  spec get ISSUE_ID [--json]
    The issue's spec: proposal, specs, design and tasks (markdown, stored in Broadcast).
  spec set ISSUE_ID [--proposal-file FILE] [--specs-file FILE] [--design-file FILE]
    [--tasks-file FILE] [--json]
    Write the given sections; the others are kept. tasks is a checklist of
    `## Group` headings and `- [ ] 1.1 Title` items; the card shows its progress.
  spec check ISSUE_ID TASK_NUMBER [--undo] [--json]
    Check off (or with --undo, uncheck) the task numbered TASK_NUMBER, such as 1.2.

All issue writes accept --agent-platform claude|codex|pi|agy|grok --session-id ID;
both options are required together. Without them, CODEX_THREAD_ID is recorded as a
codex session when it is set. Update also accepts only session metadata plus --if-version.

Statuses: backlog, todo, in_progress, in_review, blocked, done, canceled
Priorities: none, urgent, high, medium, low

Example:
  broadcast issue get LOCAL-275 --json"#
        }
        "comment add" => {
            r#"Usage: broadcast comment add ISSUE_ID (--body TEXT | --body-file FILE)
  [--agent-platform claude|codex|pi|agy|grok --session-id ID] [--json]

Attribution requires both options; Pi accepts a full session path or ID.
Without them, CODEX_THREAD_ID is recorded as a codex session when it is set."#
        }
        "comment update" => {
            r#"Usage: broadcast comment update COMMENT_ID --body TEXT --if-version N
  [--agent-platform claude|codex|pi|agy|grok --session-id ID] [--json]

Attribution requires both options; otherwise CODEX_THREAD_ID is used when it is set."#
        }
        "comment delete" => {
            r#"Usage: broadcast comment delete COMMENT_ID --if-version N
  [--agent-platform claude|codex|pi|agy|grok --session-id ID] [--json]

Deleting a comment removes its saved session metadata with it."#
        }
        "comment list" => {
            r#"Usage: broadcast comment list ISSUE_ID [--after CURSOR] [--json]

Options:
  --after CURSOR  Return comments created or modified after a prior nextCursor
  --json          Make the JSON output contract explicit
  --help          Show this help

The response always includes nextCursor. Omit --after for the full list.

Example:
  broadcast comment list LOCAL-275 --after CURSOR --json"#
        }
        _ => return None,
    })
}
