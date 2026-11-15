//! The argument contract: `RESOURCE ACTION [operands] [--name value | --name=value | --flag]`.

use serde_json::{Map, Value};

use crate::error::{Result, usage};

const BOOLEAN_OPTIONS: [&str; 3] = ["json", "clear-binding-thread", "help"];
const GLOBAL_OPTIONS: [&str; 1] = ["runtime-file"];
const ATTRIBUTION: [&str; 3] = ["thread-id", "agent-platform", "session-id"];
const BINDING: [&str; 6] = [
    "binding-thread-id",
    "binding-codex-project-id",
    "binding-codex-project-kind",
    "binding-codex-host-id",
    "binding-workspace-path",
    "clear-binding-thread",
];
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

pub const UNKNOWN_COMMAND: &str = "Expected one of: project list/create/map/readme, cloud login/status/logout, issue list/get/create/update/move/archive/restore/tree/relation, comment list/add/update/delete, attachment list/download/upload, context current, runner claude";

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
            if !allowed.contains(&name) && !GLOBAL_OPTIONS.contains(&name) {
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
        "project list" | "issue get" | "cloud status" | "cloud logout" => with(&[&["json"]]),
        "project create" => with(&[&["id", "name", "workspace-path", "json"]]),
        "project map" => with(&[&["workspace-path", "json"]]),
        "project readme" => with(&[&["content", "file", "if-version", "json"]]),
        "cloud login" => with(&[&["url", "actor-name", "json"]]),
        "issue list" => with(&[&["project", "status", "archived", "json"]]),
        "issue create" => with(&[&ISSUE_FIELDS, &ATTRIBUTION, &["json"]]),
        "issue update" => with(&[&ISSUE_FIELDS, &ATTRIBUTION, &["if-version", "json"]]),
        "issue move" => with(&[&["status"], &ATTRIBUTION, &BINDING, &["if-version", "json"]]),
        "issue archive" | "issue restore" | "comment delete" => {
            with(&[&ATTRIBUTION, &["if-version", "json"]])
        }
        "issue tree" => with(&[&["direction", "depth", "json"]]),
        "issue relation" => with(&[&["type", "issue"], &ATTRIBUTION, &["if-version", "json"]]),
        "comment list" => with(&[&["after", "json"]]),
        "comment add" => with(&[&["body", "body-file"], &ATTRIBUTION, &BINDING, &["json"]]),
        "comment update" => with(&[&["body"], &ATTRIBUTION, &["if-version", "json"]]),
        "attachment list" => with(&[&["task", "comment", "after", "json"]]),
        "attachment download" => with(&[&["output", "json"]]),
        "attachment upload" => {
            with(&[&["file", "task", "comment", "content-type", "kind", "json"]])
        }
        "context current" => with(&[&["cwd", "json"]]),
        "runner claude" => with(&[&["url", "cwd", "permission-mode", "interval"]]),
        _ => None,
    }
}

/// Help text for each supported command level.
pub fn help(scope: &str) -> Option<&'static str> {
    Some(match scope {
        "" => {
            r#"Usage: broadcast RESOURCE ACTION [options]

Commands:
  context current [--cwd PATH] [--json]
  project list
  project create --name NAME [--id ID] [--workspace-path PATH]
  project map PROJECT_ID --workspace-path PATH
  project readme get [PROJECT_ID]
  project readme set [PROJECT_ID] (--content TEXT | --file FILE) [--if-version N]
  cloud login --url URL --actor-name NAME
  cloud status|logout
  issue list|get|create|update|move|archive|restore|tree|relation
  comment list ISSUE_ID [--after CURSOR]
  comment add ISSUE_ID (--body TEXT | --body-file FILE) [--thread-id ID]
  comment update COMMENT_ID --body TEXT --if-version N [--thread-id ID]
  comment delete COMMENT_ID --if-version N [--thread-id ID]
  attachment list (--task ISSUE_ID | --comment COMMENT_ID) [--after CURSOR]
  attachment download ATTACHMENT_ID --output PATH
  attachment upload --file PATH (--task ISSUE_ID | --comment COMMENT_ID)
  runner claude [--url URL] [--cwd PATH] [--permission-mode MODE] [--interval SECONDS]

Global options:
  --runtime-file FILE  Use an explicit launcher runtime descriptor
  --json               Make the JSON output contract explicit
  --help               Show help for a supported command level

Examples:
  broadcast issue get LOCAL-275 --json
  broadcast comment list LOCAL-275 --json

Conversation attribution for issue/comment writes:
  --agent-platform claude|pi|agy|grok --session-id ID
  Or keep Codex --thread-id ID / CODEX_THREAD_ID.
  External options must be supplied together and ignore CODEX_THREAD_ID.
  --binding-* options remain separate native Codex bindings.

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
    [--thread-id ID]
    [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]]
    [--start-date YYYY-MM-DD] [--due-date YYYY-MM-DD]
    [--recurrence-interval N --recurrence-unit day|week|month|year] [--json]
  update ISSUE_ID
    [--project PROJECT_ID] [--title TITLE]
    [--description TEXT | --description-file FILE]
    [--status STATUS] [--priority PRIORITY] [--labels a,b]
    [--thread-id ID]
    [--git-branch BRANCH | --worktree-path PATH [--worktree-branch BRANCH]]
    [--start-date YYYY-MM-DD] [--due-date YYYY-MM-DD]
    [--recurrence-interval N --recurrence-unit day|week|month|year]
    [--if-version N] [--json]
  move ISSUE_ID --status STATUS [--thread-id ID]
    [--binding-thread-id ID
      [--binding-codex-project-id ID --binding-codex-project-kind local|remote
       --binding-codex-host-id ID --binding-workspace-path PATH]
     | --clear-binding-thread]
    [--if-version N] [--json]
  archive ISSUE_ID [--thread-id ID] [--if-version N] [--json]
  restore ISSUE_ID [--thread-id ID] [--if-version N] [--json]
  tree ISSUE_ID --direction descendants|ancestors --depth N [--json]
  relation add|remove ISSUE_ID --type parent|blocks|blocked_by|related
    --issue RELATED_ISSUE_ID [--thread-id ID] [--if-version N] [--json]

All issue writes accept --agent-platform claude|pi|agy|grok --session-id ID
instead of Codex --thread-id / CODEX_THREAD_ID. Both external options are required.
External metadata does not replace --binding-* native Codex identity.
Update also accepts only external session metadata plus --if-version.

Statuses: backlog, todo, in_progress, in_review, blocked, done, canceled
Priorities: none, urgent, high, medium, low

Example:
  broadcast issue get LOCAL-275 --json"#
        }
        "comment add" => {
            r#"Usage: broadcast comment add ISSUE_ID (--body TEXT | --body-file FILE)
  [--thread-id ID | --agent-platform claude|pi|agy|grok --session-id ID]
  [--binding-thread-id ID
    [--binding-codex-project-id ID --binding-codex-project-kind local|remote
     --binding-codex-host-id ID --binding-workspace-path PATH]
   | --clear-binding-thread] [--json]

External attribution requires both options; Pi accepts a full session path or ID.
CODEX_THREAD_ID is used only when no external attribution is supplied.
Binding options remain independent, native Codex identity."#
        }
        "comment update" => {
            r#"Usage: broadcast comment update COMMENT_ID --body TEXT --if-version N
  [--thread-id ID | --agent-platform claude|pi|agy|grok --session-id ID] [--json]

External attribution requires both options; otherwise Codex uses CODEX_THREAD_ID."#
        }
        "comment delete" => {
            r#"Usage: broadcast comment delete COMMENT_ID --if-version N
  [--thread-id ID | --agent-platform claude|pi|agy|grok --session-id ID] [--json]

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
