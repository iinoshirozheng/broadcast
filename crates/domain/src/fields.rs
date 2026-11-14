//! Field validators ported from `shared/api-fields.mjs`.
//!
//! A field read is `Option<&Value>`: `None` is JavaScript `undefined`.
//! A three-state result is `Option<Option<T>>`: `None` is undefined, `Some(None)` is null.

use serde_json::{Map, Value};

use crate::ApiError;
use crate::constants::{TASK_PRIORITIES, TASK_STATUSES};
use crate::js;
use crate::records::Recurrence;

pub type Object = Map<String, Value>;
type Result<T> = std::result::Result<T, ApiError>;

pub const NULL: Value = Value::Null;

/// `value ?? fallback` for an absent or `null` value.
pub fn nullish(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

pub fn plain_object(value: &Value) -> Result<&Object> {
    value
        .as_object()
        .ok_or_else(|| ApiError::new(400, "INVALID_BODY", "Request body must be a JSON object"))
}

pub fn assert_allowed_keys(object: &Object, allowed: &[&str]) -> Result<()> {
    let unknown: Vec<&str> =
        js::object_keys(object).into_iter().filter(|key| !allowed.contains(key)).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let plural = if unknown.len() == 1 { "" } else { "s" };
    Err(ApiError::new(400, "UNKNOWN_FIELD", format!("Unknown field{plural}: {}", unknown.join(", "))))
}

/// A plain object restricted to `allowed` keys.
pub fn object<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Object> {
    let object = plain_object(value)?;
    assert_allowed_keys(object, allowed)?;
    Ok(object)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StringRules {
    pub required: bool,
    pub nullable: bool,
    pub max_length: Option<usize>,
}

/// `stringField`: trimmed, length-checked string.
pub fn string_field(value: Option<&Value>, name: &str, rules: StringRules) -> Result<Option<Option<String>>> {
    let Some(value) = value else {
        if rules.required {
            return Err(ApiError::invalid_field(format!("'{name}' is required")));
        }
        return Ok(None);
    };
    if rules.nullable && value.is_null() {
        return Ok(Some(None));
    }
    let Some(text) = value.as_str() else {
        let suffix = if rules.nullable { " or null" } else { "" };
        return Err(ApiError::invalid_field(format!("'{name}' must be a string{suffix}")));
    };
    let normalized = js::trim(text);
    if rules.required && normalized.is_empty() {
        return Err(ApiError::invalid_field(format!("'{name}' cannot be empty")));
    }
    if let Some(max) = rules.max_length
        && js::len(normalized) > max
    {
        return Err(ApiError::invalid_field(format!("'{name}' cannot exceed {max} characters")));
    }
    Ok(Some(Some(normalized.to_owned())))
}

/// A required, non-nullable string.
pub fn required_string(value: Option<&Value>, name: &str, max_length: usize) -> Result<String> {
    let rules = StringRules { required: true, max_length: Some(max_length), ..StringRules::default() };
    Ok(string_field(value, name, rules)?.flatten().unwrap_or_default())
}

/// An optional, non-nullable string: `None` is undefined.
pub fn optional_string(value: Option<&Value>, name: &str, max_length: usize) -> Result<Option<String>> {
    let rules = StringRules { max_length: Some(max_length), ..StringRules::default() };
    Ok(string_field(value, name, rules)?.flatten())
}

/// A nullable string.
pub fn nullable_string(value: Option<&Value>, name: &str, max_length: usize) -> Result<Option<Option<String>>> {
    string_field(value, name, StringRules { nullable: true, max_length: Some(max_length), ..StringRules::default() })
}

/// `parseDueDate`.
pub fn due_date(value: Option<&Value>, name: &str) -> Result<Option<Option<String>>> {
    let date = nullable_string(value, name, 10)?;
    if let Some(Some(text)) = &date {
        let bytes = text.as_bytes();
        let valid = bytes.len() == 10
            && bytes.iter().enumerate().all(|(index, byte)| match index {
                4 | 7 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            });
        if !valid {
            return Err(ApiError::invalid_field(format!("'{name}' must use YYYY-MM-DD")));
        }
    }
    Ok(date)
}

/// `parseRecurrence` for a defined value.
pub fn recurrence(value: &Value) -> Result<Option<Recurrence>> {
    if value.is_null() {
        return Ok(None);
    }
    let object = object(value, &["interval", "unit"])?;
    let interval = js::safe_integer(object.get("interval")).filter(|interval| (1..=365).contains(interval));
    let Some(interval) = interval else {
        return Err(ApiError::invalid_field("'recurrence.interval' must be an integer from 1 to 365"));
    };
    let unit = object.get("unit").and_then(Value::as_str);
    let Some(unit) = unit.filter(|unit| ["day", "week", "month", "year"].contains(unit)) else {
        return Err(ApiError::invalid_field("'recurrence.unit' must be day, week, month, or year"));
    };
    Ok(Some(Recurrence { interval, unit: unit.to_owned() }))
}

/// `parseSortOrder` for a defined value.
pub fn sort_order(value: &Value) -> Result<f64> {
    value.as_f64().filter(|order| order.is_finite() && order.abs() <= 1_000_000_000_000.0).ok_or_else(|| {
        ApiError::invalid_field(
            "'sortOrder' must be a finite number between -1000000000000 and 1000000000000",
        )
    })
}

/// `parseLabels` for a defined value.
pub fn labels(value: &Value) -> Result<Vec<String>> {
    let Some(entries) = value.as_array().filter(|entries| entries.len() <= 20) else {
        return Err(ApiError::invalid_field("'labels' must be an array with at most 20 entries"));
    };
    let mut labels: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(label) = entry.as_str() else {
            return Err(ApiError::invalid_field("Every label must be a string"));
        };
        let normalized = js::trim(label);
        if normalized.is_empty() || js::len(normalized) > 64 {
            return Err(ApiError::invalid_field("Labels must contain 1 to 64 characters"));
        }
        labels.push(normalized.to_owned());
    }
    for (index, label) in labels.iter().enumerate() {
        if labels[..index].contains(label) {
            return Err(ApiError::invalid_field("Labels must be unique"));
        }
    }
    Ok(labels)
}

/// `parseStatus(value, fallback)`.
pub fn status(value: Option<&Value>, fallback: Option<&'static str>) -> Result<&'static str> {
    let candidate = match nullish(value) {
        Some(value) => value.as_str(),
        None => fallback,
    };
    candidate.and_then(|status| TASK_STATUSES.into_iter().find(|known| *known == status)).ok_or_else(|| {
        ApiError::invalid_field(format!("'status' must be one of: {}", TASK_STATUSES.join(", ")))
    })
}

/// `parsePriority(value, fallback)`.
pub fn priority(value: Option<&Value>, fallback: Option<&'static str>) -> Result<&'static str> {
    let candidate = match nullish(value) {
        Some(value) => value.as_str(),
        None => fallback,
    };
    candidate
        .and_then(|priority| TASK_PRIORITIES.into_iter().find(|known| *known == priority))
        .ok_or_else(|| ApiError::invalid_field("'priority' must be none, urgent, high, medium, or low"))
}

pub fn slugify(value: &str) -> String {
    let lower = value.to_lowercase();
    let mut slug = lower
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    slug.truncate(64);
    slug
}

/// `parseProjectLabel`.
pub fn project_label(body: &Value) -> Result<String> {
    let body = object(body, &["label"])?;
    required_string(body.get("label"), "label", 64)
}

/// `parseThreadId`.
pub fn thread_id(value: Option<&Value>) -> Result<Option<String>> {
    value.map(|value| required_string(Some(value), "threadId", 256)).transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssigneeTarget {
    CurrentUser,
    CodexAgent,
    ClaudeAgent,
}

/// `parseAssigneeTarget`.
pub fn assignee_target(value: Option<&Value>) -> Result<Option<AssigneeTarget>> {
    let Some(value) = value else { return Ok(None) };
    match value.as_str() {
        Some("current-user") => Ok(Some(AssigneeTarget::CurrentUser)),
        Some("codex-agent") => Ok(Some(AssigneeTarget::CodexAgent)),
        Some("claude-agent") => Ok(Some(AssigneeTarget::ClaudeAgent)),
        _ => Err(ApiError::invalid_field(
            "'assigneeTarget' must be current-user, codex-agent or claude-agent",
        )),
    }
}

/// `parseRelationOrigin`.
pub fn relation_origin(value: Option<&Value>) -> Result<Option<&'static str>> {
    let Some(value) = value else { return Ok(None) };
    match value.as_str() {
        Some("manual") => Ok(Some("manual")),
        Some("mention") => Ok(Some("mention")),
        _ => Err(ApiError::invalid_field("'origin' must be manual or mention")),
    }
}

/// `parseVersion`.
pub fn version(value: Option<&Value>, allow_zero: bool) -> Result<i64> {
    let minimum = if allow_zero { 0 } else { 1 };
    js::safe_integer(value).filter(|version| *version >= minimum).ok_or_else(|| {
        let kind = if allow_zero { "non-negative" } else { "positive" };
        ApiError::invalid_field(format!("'version' must be a {kind} integer"))
    })
}

/// `validateProjectId` (required).
pub fn project_id(value: Option<&Value>) -> Result<String> {
    let id = required_string(value, "id", 64)?;
    let bytes = id.as_bytes();
    let slug_byte = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let valid = bytes.first().is_some_and(slug_byte)
        && bytes.last().is_some_and(slug_byte)
        && bytes.iter().all(|byte| slug_byte(byte) || *byte == b'-');
    if !valid {
        return Err(ApiError::invalid_field(
            "'id' must be a lowercase slug containing letters, numbers, or hyphens",
        ));
    }
    Ok(id)
}
