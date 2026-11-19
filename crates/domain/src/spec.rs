//! An issue's spec: proposal, specs, design and a tasks checklist, stored in Broadcast.

use serde::Serialize;

/// The full spec behind `/api/tasks/:id/spec`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpec {
    pub task_id: String,
    pub proposal: String,
    pub specs: String,
    pub design: String,
    pub tasks: String,
    pub updated_at: String,
}

/// What a task card shows: the tasks checklist.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpecSummary {
    pub updated_at: String,
    pub groups: Vec<SpecGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecGroup {
    pub title: String,
    pub tasks: Vec<SpecTask>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecTask {
    pub number: String,
    pub title: String,
    pub done: bool,
}

/// A `- [ ] 1.2 Title` / `- [x] 1.2 Title` line: (done, number, title).
fn task_line(line: &str) -> Option<(bool, &str, &str)> {
    let line = line.trim();
    let (done, text) = if let Some(text) = line.strip_prefix("- [ ] ") {
        (false, text)
    } else {
        (
            true,
            line.strip_prefix("- [x] ")
                .or_else(|| line.strip_prefix("- [X] "))?,
        )
    };
    let text = text.trim();
    Some(match text.split_once(' ') {
        Some((number, title)) if number.chars().all(|c| c.is_ascii_digit() || c == '.') => {
            (done, number.trim_end_matches('.'), title.trim())
        }
        _ => (done, "", text),
    })
}

/// Reads the `## Group` headings and `- [ ]` / `- [x]` items of a tasks checklist.
pub fn parse_tasks(markdown: &str) -> Vec<SpecGroup> {
    let mut groups: Vec<SpecGroup> = Vec::new();
    for line in markdown.lines() {
        if let Some(title) = line.trim().strip_prefix("## ") {
            groups.push(SpecGroup {
                title: title.trim().to_owned(),
                tasks: Vec::new(),
            });
            continue;
        }
        let Some((done, number, title)) = task_line(line) else {
            continue;
        };
        if groups.is_empty() {
            groups.push(SpecGroup {
                title: String::new(),
                tasks: Vec::new(),
            });
        }
        if let Some(group) = groups.last_mut() {
            group.tasks.push(SpecTask {
                number: number.to_owned(),
                title: title.to_owned(),
                done,
            });
        }
    }
    groups.retain(|group| !group.tasks.is_empty());
    groups
}

/// Sets the checkbox of the task numbered `number`; `None` when no task has that number.
pub fn check_task(markdown: &str, number: &str, done: bool) -> Option<String> {
    let mut found = false;
    let checked = markdown
        .split_inclusive('\n')
        .map(|line| match task_line(line) {
            Some((_, line_number, _)) if !found && line_number == number => {
                found = true;
                let mark = if done { "- [x] " } else { "- [ ] " };
                let start = line.len() - line.trim_start().len();
                format!("{}{mark}{}", &line[..start], &line[start + mark.len()..])
            }
            _ => line.to_owned(),
        })
        .collect();
    found.then_some(checked)
}
