//! `/api/local/ai/*` and `/api/local/projects/:id/summary`: the device-local AI chat and the
//! daily project summaries.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::sse::Event;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use broadcast_domain::constants::AI_SANDBOX_MODES;
use broadcast_domain::fields::{self, Object};
use broadcast_domain::js::{self, decode_uri_component};
use broadcast_domain::{ApiError, Error};
use serde_json::{Value, json};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::App;
use crate::ai::catalog::{is_missing_cli, unavailable};
use crate::ai::composer::{self, DocumentNode, RebindNode};
use crate::ai::context::RemoteTarget;
use crate::ai::exec::SKILL_MARKER;
use crate::ai::{Attachment, CandidateQuery, NewThread, ThreadPatch, Turn, TurnBody};
use crate::events::sse;
use crate::http::{
    Reply, assert_allowed_query, assert_empty_body, empty, json, method_not_allowed, query, read_json, read_json_limit,
    segment,
};

type Fields<T> = std::result::Result<T, ApiError>;

const TURN_BODY_LIMIT: usize = 25 * 1024 * 1024;
const ATTACHMENT_LIMIT: usize = 10;
const TEXT_LIMIT: usize = 100_000;
const NODE_LIMIT: usize = 200;
const TARGET_FIELDS: [&str; 4] = ["codexProjectId", "codexProjectKind", "codexHostId", "workspacePath"];

/// `assertNoQuery`.
fn no_query(request: &Request, label: &str) -> broadcast_domain::Result<()> {
    assert_allowed_query(&query(request.uri()), &[], label)
}

/// `searchParams.get`: the first value of `key`.
fn first<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
}

/// `decodeRouteSegment`: path segment `n`, decoded, 1..=256 units and free of NUL.
fn route_segment(request: &Request, n: usize, name: &str) -> Fields<String> {
    let decoded = decode_uri_component(segment(request.uri(), n))
        .ok_or_else(|| ApiError::new(400, "INVALID_PATH", format!("{name} contains invalid encoding")))?;
    if decoded.is_empty() || js::len(&decoded) > 256 || decoded.contains('\0') {
        return Err(ApiError::new(400, "INVALID_PATH", format!("{name} is invalid")));
    }
    Ok(decoded)
}

/// `parseAiSetting`: an optional, non-empty string.
fn setting(value: Option<&Value>, name: &str, max_length: usize) -> Fields<Option<String>> {
    let setting = fields::optional_string(value, name, max_length)?;
    if setting.as_deref() == Some("") {
        return Err(ApiError::invalid_field(format!("'{name}' cannot be empty")));
    }
    Ok(setting)
}

/// `parseAiSandbox`.
fn sandbox(value: Option<&Value>) -> Fields<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    match value.as_str().filter(|mode| AI_SANDBOX_MODES.contains(mode)) {
        Some(mode) => Ok(Some(mode.to_owned())),
        None => Err(ApiError::new(
            400,
            "INVALID_SANDBOX",
            "'sandbox' must be read-only, workspace-write, or danger-full-access",
        )),
    }
}

/// `parseAiExecutionTarget`: all four Codex project fields or none; only a remote project
/// changes where the conversation runs.
fn target(object: &Object) -> Fields<Option<RemoteTarget>> {
    let present = TARGET_FIELDS.iter().filter(|field| object.contains_key(**field)).count();
    if present == 0 {
        return Ok(None);
    }
    let invalid = |message: &str| ApiError::new(400, "INVALID_CODEX_TARGET", message.to_owned());
    if present != TARGET_FIELDS.len() {
        return Err(invalid("Codex project identity must contain all four fields"));
    }
    let kind = setting(object.get("codexProjectKind"), "codexProjectKind", 16)?.unwrap_or_default();
    if kind != "local" && kind != "remote" {
        return Err(invalid("'codexProjectKind' must be local or remote"));
    }
    let workspace_path = setting(object.get("workspacePath"), "workspacePath", 4096)?.unwrap_or_default();
    if workspace_path.contains('\0') {
        return Err(invalid("'workspacePath' cannot contain null bytes"));
    }
    let codex_project_id = setting(object.get("codexProjectId"), "codexProjectId", 256)?.unwrap_or_default();
    let codex_host_id = setting(object.get("codexHostId"), "codexHostId", 512)?.unwrap_or_default();
    Ok((kind == "remote").then_some(RemoteTarget { codex_project_id, codex_host_id, workspace_path }))
}

/// `aiExecutionTargetFromQuery`.
fn query_target(query: &[(String, String)]) -> Fields<Option<RemoteTarget>> {
    let object: Object = TARGET_FIELDS
        .iter()
        .filter_map(|field| first(query, field).map(|value| ((*field).to_owned(), Value::String(value.to_owned()))))
        .collect();
    target(&object)
}

fn new_thread(body: &Value) -> Fields<NewThread> {
    let body = fields::object(
        body,
        &["projectId", "issueId", "title", "model", "reasoningEffort", "sandbox", TARGET_FIELDS[0], TARGET_FIELDS[1], TARGET_FIELDS[2], TARGET_FIELDS[3]],
    )?;
    Ok(NewThread {
        project_id: fields::project_id(body.get("projectId"))?,
        issue_id: setting(body.get("issueId"), "issueId", 128)?,
        title: setting(body.get("title"), "title", 160)?,
        model: setting(body.get("model"), "model", 128)?,
        reasoning_effort: setting(body.get("reasoningEffort"), "reasoningEffort", 64)?,
        sandbox: sandbox(body.get("sandbox"))?,
        remote: target(body)?,
    })
}

fn thread_patch(body: &Value) -> Fields<ThreadPatch> {
    let body = fields::object(body, &["title", "model", "reasoningEffort", "sandbox"])?;
    let patch = ThreadPatch {
        title: setting(body.get("title"), "title", 160)?,
        model: setting(body.get("model"), "model", 128)?,
        reasoning_effort: setting(body.get("reasoningEffort"), "reasoningEffort", 64)?,
        sandbox: sandbox(body.get("sandbox"))?,
    };
    if patch.title.is_none() && patch.model.is_none() && patch.reasoning_effort.is_none() && patch.sandbox.is_none() {
        return Err(ApiError::new(400, "INVALID_BODY", "PATCH requires at least one thread setting"));
    }
    Ok(patch)
}

fn confirmed(body: &Object) -> Fields<bool> {
    match body.get("dangerFullAccessConfirmed") {
        None => Ok(false),
        Some(Value::Bool(confirmed)) => Ok(*confirmed),
        Some(_) => Err(ApiError::invalid_field("'dangerFullAccessConfirmed' must be a boolean")),
    }
}

/// `parseAiSkillIds`.
fn skill_ids(value: Option<&Value>) -> Fields<Vec<String>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let ids = value
        .as_array()
        .filter(|ids| ids.len() <= 20)
        .ok_or_else(|| ApiError::invalid_field("'skillIds' must be an array with at most 20 entries"))?;
    ids.iter()
        .enumerate()
        .map(|(index, id)| fields::required_string(Some(id), &format!("skillIds[{index}]"), 256))
        .collect()
}

/// `^[A-Za-z0-9+/]+={0,2}$` with a length that is a multiple of four.
fn base64_shaped(encoded: &str) -> bool {
    let data = encoded.trim_end_matches('=');
    encoded.len().is_multiple_of(4)
        && encoded.len() - data.len() <= 2
        && !data.is_empty()
        && data.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
}

/// `parseAiAttachments`: files sent inline as canonical base64.
fn attachments(value: Option<&Value>) -> Fields<Vec<Attachment>> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let invalid = |message: String| ApiError::new(400, "INVALID_ATTACHMENT", message);
    let entries = value
        .as_array()
        .filter(|entries| entries.len() <= ATTACHMENT_LIMIT)
        .ok_or_else(|| invalid(format!("'attachments' must be an array with at most {ATTACHMENT_LIMIT} files")))?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let entry = fields::object(entry, &["filename", "contentType", "dataBase64"])?;
            let name = |field: &str| format!("attachments[{index}].{field}");
            let filename = fields::required_string(entry.get("filename"), &name("filename"), 240)?;
            if filename.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}' || c == '/' || c == '\\') {
                return Err(invalid(format!("'{}' is invalid", name("filename"))));
            }
            let content_type = fields::required_string(entry.get("contentType"), &name("contentType"), 256)?.to_lowercase();
            let encoded = fields::required_string(entry.get("dataBase64"), &name("dataBase64"), TURN_BODY_LIMIT)?;
            let malformed = || invalid(format!("'{}' must contain valid base64", name("dataBase64")));
            if !base64_shaped(&encoded) {
                return Err(malformed());
            }
            let data = STANDARD.decode(&encoded).map_err(|_| malformed())?;
            if data.is_empty() || STANDARD.encode(&data) != encoded {
                return Err(malformed());
            }
            Ok(Attachment { filename, content_type, data })
        })
        .collect()
}

/// `parseAiTurn`: a plain message, or a composer document when `contractVersion` is given.
fn turn(body: &Value) -> Fields<Turn> {
    let body = fields::plain_object(body)?;
    if body.contains_key("contractVersion") {
        return composer_turn(body);
    }
    fields::assert_allowed_keys(body, &["message", "skillIds", "dangerFullAccessConfirmed", "attachments"])?;
    let confirmed = confirmed(body)?;
    let blank = Value::String(String::new());
    let message =
        fields::optional_string(Some(fields::nullish(body.get("message")).unwrap_or(&blank)), "message", TEXT_LIMIT)?
            .unwrap_or_default();
    let skill_ids = skill_ids(body.get("skillIds"))?;
    if message.matches(SKILL_MARKER).count() != skill_ids.len() {
        return Err(ApiError::invalid_field("'skillIds' must match the Skill markers in 'message'"));
    }
    let attachments = attachments(body.get("attachments"))?;
    if message.is_empty() && attachments.is_empty() {
        return Err(ApiError::new(400, "INVALID_MESSAGE", "A message or at least one attachment is required"));
    }
    Ok(Turn { confirmed, attachments, body: TurnBody::Message { message, skill_ids } })
}

/// `parseComposerTurn`.
fn composer_turn(body: &Object) -> Fields<Turn> {
    fields::assert_allowed_keys(body, &["contractVersion", "revision", "document", "dangerFullAccessConfirmed", "attachments"])?;
    if body.get("contractVersion").and_then(Value::as_str) != Some(composer::CONTRACT_VERSION) {
        return Err(ApiError::new(400, "INVALID_COMPOSER_DOCUMENT", "'contractVersion' must be 'composer.v1'"));
    }
    let confirmed = confirmed(body)?;
    let revision = fields::required_string(body.get("revision"), "revision", 512)?;
    let nodes = composer_document(body.get("document").unwrap_or(&Value::Null))?;
    let attachments = attachments(body.get("attachments"))?;
    Ok(Turn { confirmed, attachments, body: TurnBody::Composer { revision, nodes } })
}

/// `parseComposerDocument`.
fn composer_document(value: &Value) -> Fields<Vec<DocumentNode>> {
    let document = fields::object(value, &["version", "nodes"])?;
    let invalid = |message: String| ApiError::new(400, "INVALID_COMPOSER_DOCUMENT", message);
    if document.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(invalid("'document.version' must be 1".into()));
    }
    let nodes = document
        .get("nodes")
        .and_then(Value::as_array)
        .filter(|nodes| nodes.len() <= NODE_LIMIT)
        .ok_or_else(|| invalid(format!("'document.nodes' must be an array with at most {NODE_LIMIT} entries")))?;
    let mut text_length = 0;
    let mut parsed = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let node = fields::plain_object(node)?;
        let kind = node
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())
            .ok_or_else(|| invalid(format!("'document.nodes[{index}].type' is required")))?;
        parsed.push(match kind {
            "text" => {
                fields::assert_allowed_keys(node, &["type", "text"])?;
                let text = node
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid(format!("'document.nodes[{index}].text' must be a string")))?;
                text_length += js::len(text);
                DocumentNode::Text(text.to_owned())
            }
            "skill" | "agent" => {
                fields::assert_allowed_keys(node, &["type", "candidateRef", "label"])?;
                DocumentNode::Reference {
                    kind: if kind == "skill" { "skill" } else { "agent" },
                    candidate_ref: fields::required_string(
                        node.get("candidateRef"),
                        &format!("document.nodes[{index}].candidateRef"),
                        512,
                    )?,
                    label: fields::required_string(node.get("label"), &format!("document.nodes[{index}].label"), 256)?,
                }
            }
            other => DocumentNode::Other(other.to_owned()),
        });
    }
    if text_length > TEXT_LIMIT {
        return Err(invalid(format!("Composer text cannot exceed {TEXT_LIMIT} characters")));
    }
    Ok(parsed)
}

struct Rebind {
    project_id: String,
    thread_id: Option<String>,
    nodes: Vec<RebindNode>,
}

/// `parseComposerRebindRequest`: persisted reference markers to resolve against the catalog.
fn rebind_request(value: &Value) -> Fields<Rebind> {
    let invalid = |message: String| ApiError::new(400, "INVALID_COMPOSER_REBIND_REQUEST", message);
    let keys = |object: &Object, allowed: &[&str], field: &str| match js::object_keys(object).into_iter().find(|key| !allowed.contains(key)) {
        Some(key) => Err(invalid(format!("'{field}.{key}' is not allowed"))),
        None => Ok(()),
    };
    let body = value.as_object().ok_or_else(|| invalid("Composer rebind body must be an object".into()))?;
    keys(body, &["contractVersion", "projectId", "threadId", "document"], "body")?;
    if body.get("contractVersion").and_then(Value::as_str) != Some(composer::CONTRACT_VERSION) {
        return Err(invalid("'contractVersion' must be 'composer.v1'".into()));
    }
    let project_id = fields::project_id(body.get("projectId")).map_err(|_| invalid("'projectId' is invalid".into()))?;
    let thread_id = fields::thread_id(body.get("threadId")).map_err(|_| invalid("'threadId' is invalid".into()))?;
    let document = body.get("document").and_then(Value::as_object).ok_or_else(|| invalid("'document' must be an object".into()))?;
    keys(document, &["version", "nodes"], "document")?;
    if document.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(invalid("'document.version' must be 1".into()));
    }
    let nodes = document
        .get("nodes")
        .and_then(Value::as_array)
        .filter(|nodes| nodes.len() <= NODE_LIMIT)
        .ok_or_else(|| invalid(format!("'document.nodes' must contain at most {NODE_LIMIT} entries")))?;
    let mut text_length = 0;
    let mut parsed = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let path = format!("document.nodes[{index}]");
        let node = node.as_object().ok_or_else(|| invalid(format!("'{path}' must be an object")))?;
        let text = |key: &str| node.get(key).and_then(Value::as_str);
        let label_valid = text("label").is_some_and(|label| (1..=256).contains(&js::len(label)));
        match text("type") {
            Some("text") => {
                keys(node, &["type", "text"], &path)?;
                let content = text("text").ok_or_else(|| invalid(format!("'{path}.text' must be a string")))?;
                text_length += js::len(content);
                parsed.push(RebindNode::Text(content.to_owned()));
            }
            Some("unsupportedReference") => {
                keys(node, &["type", "referenceUri", "label"], &path)?;
                if !label_valid {
                    return Err(invalid(format!("'{path}.label' is invalid")));
                }
                let uri = text("referenceUri")
                    .filter(|uri| js::len(uri) <= 1_024)
                    .ok_or_else(|| invalid(format!("'{path}.referenceUri' is invalid")))?;
                let parts: Vec<&str> = uri
                    .strip_prefix("broadcast://composer-reference/")
                    .map(|rest| rest.split('/').collect())
                    .unwrap_or_default();
                let [format, kind, key] = parts.as_slice() else {
                    return Err(invalid(format!("'{path}.referenceUri' is not a composer reference marker")));
                };
                if format.is_empty() || kind.is_empty() || key.is_empty() {
                    return Err(invalid(format!("'{path}.referenceUri' is not a composer reference marker")));
                }
                if composer::decode_key(key).is_none() {
                    return Err(invalid(format!("'{path}.referenceUri' has an invalid reference key")));
                }
                let reason_code = if *format != "v1" {
                    "REFERENCE_FORMAT_UNSUPPORTED"
                } else if *kind != "skill" && *kind != "agent" {
                    "REFERENCE_KIND_UNSUPPORTED"
                } else {
                    return Err(invalid(format!("'{path}' must use persistedReference for supported markers")));
                };
                parsed.push(RebindNode::Unsupported { reason_code });
            }
            Some("persistedReference") => {
                keys(node, &["type", "referenceKind", "referenceKey", "label"], &path)?;
                let kind = match text("referenceKind") {
                    Some("skill") => "skill",
                    Some("agent") => "agent",
                    _ => return Err(invalid(format!("'{path}.referenceKind' must be skill or agent"))),
                };
                let key = text("referenceKey")
                    .filter(|key| (1..=512).contains(&js::len(key)))
                    .ok_or_else(|| invalid(format!("'{path}.referenceKey' is invalid")))?;
                if !label_valid {
                    return Err(invalid(format!("'{path}.label' is invalid")));
                }
                let stable_id =
                    composer::decode_key(key).ok_or_else(|| invalid(format!("'{path}.referenceKey' is not canonical base64url")))?;
                if kind == "skill" && stable_id != composer::nfc(&stable_id) {
                    return Err(invalid(format!("'{path}.referenceKey' does not contain an NFC Skill name")));
                }
                parsed.push(RebindNode::Persisted { kind, stable_id });
            }
            _ => {
                return Err(invalid(format!(
                    "'{path}.type' must be text, persistedReference or unsupportedReference"
                )));
            }
        }
    }
    if text_length > TEXT_LIMIT {
        return Err(invalid(format!("Composer text cannot exceed {TEXT_LIMIT} characters")));
    }
    Ok(Rebind { project_id, thread_id, nodes: parsed })
}

pub async fn catalog(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let query = query(request.uri());
    let allowed = ["projectId", TARGET_FIELDS[0], TARGET_FIELDS[1], TARGET_FIELDS[2], TARGET_FIELDS[3]];
    assert_allowed_query(&query, &allowed, "GET /api/local/ai/catalog")?;
    let project_id = fields::project_id(first(&query, "projectId").map(|id| Value::String(id.to_owned())).as_ref())?;
    let remote = query_target(&query)?;
    // A missing Codex CLI makes the optional catalog unavailable rather than broken.
    let catalog = match app.ai.catalog(&app, &project_id, remote.as_ref()).await {
        Err(Error::Internal(error)) if is_missing_cli(&error) => unavailable(),
        other => other?,
    };
    Ok(json(200, &json!(catalog)))
}

pub async fn candidates(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    let query = query(request.uri());
    let allowed = [
        "projectId",
        "threadId",
        "trigger",
        "query",
        "surface",
        TARGET_FIELDS[0],
        TARGET_FIELDS[1],
        TARGET_FIELDS[2],
        TARGET_FIELDS[3],
    ];
    assert_allowed_query(&query, &allowed, "GET /api/local/ai/composer/candidates")?;
    let invalid = |message: &str| ApiError::new(400, "INVALID_COMPOSER_QUERY", message.to_owned());
    let text = |value: &str| Value::String(value.to_owned());
    let project_id = first(&query, "projectId")
        .map(|id| fields::project_id(Some(&text(id))).map_err(|_| invalid("Composer project id is invalid")))
        .transpose()?;
    let trigger = first(&query, "trigger")
        .filter(|trigger| *trigger == "/" || *trigger == "@")
        .ok_or_else(|| invalid("Composer trigger must be '/' or '@'"))?
        .to_owned();
    let search = first(&query, "query").unwrap_or_default().to_owned();
    if js::len(&search) > 256 {
        return Err(invalid("Composer query cannot exceed 256 characters").into());
    }
    let thread_id = fields::thread_id(first(&query, "threadId").map(text).as_ref())
        .map_err(|_| invalid("Composer thread id is invalid"))?;
    let surface = first(&query, "surface").unwrap_or("ai-chat");
    if !["ai-chat", "issue-description", "comment"].contains(&surface) {
        return Err(invalid("Composer surface is invalid").into());
    }
    let remote = query_target(&query)?;
    let input = CandidateQuery { project_id, thread_id, trigger: trigger.clone(), query: search.clone(), remote };
    let response = app.ai.candidates(&app, input).await?;
    Ok(json(200, &composer::for_surface(response, surface, &trigger, &search)))
}

pub async fn rebind(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    no_query(&request, "POST /api/local/ai/composer/rebind")?;
    let input = rebind_request(&read_json(request).await?)?;
    Ok(json(200, &app.ai.rebind(&app, &input.project_id, input.thread_id.as_deref(), input.nodes).await?))
}

pub async fn project_summary(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    no_query(&request, "GET /api/local/projects/:id/summary")?;
    let project_id = fields::project_id(Some(&Value::String(route_segment(&request, 4, "Project id")?)))?;
    Ok(json(200, &app.summary.get(&app, &project_id).await?))
}

pub async fn threads(State(app): State<Arc<App>>, request: Request) -> Reply {
    no_query(&request, "/api/local/ai/threads")?;
    match request.method().as_str() {
        "GET" => Ok(json(200, &json!({ "threads": app.ai.threads(&app).await? }))),
        "POST" => {
            let input = new_thread(&read_json(request).await?)?;
            Ok(json(201, &json!({ "thread": app.ai.create(&app, input).await? })))
        }
        _ => method_not_allowed(&["GET", "POST"]),
    }
}

pub async fn thread(State(app): State<Arc<App>>, request: Request) -> Reply {
    no_query(&request, "/api/local/ai/threads/:id")?;
    let thread_id = route_segment(&request, 5, "Thread id")?;
    match request.method().as_str() {
        "GET" => Ok(json(200, &app.ai.snapshot(&app, &thread_id).await?)),
        "PATCH" => {
            let patch = thread_patch(&read_json(request).await?)?;
            Ok(json(200, &json!({ "thread": app.ai.update(&app, &thread_id, patch).await? })))
        }
        "DELETE" => {
            assert_empty_body(request, "DELETE /api/local/ai/threads/:id").await?;
            app.ai.delete(&app, &thread_id).await?;
            Ok(empty(StatusCode::NO_CONTENT))
        }
        _ => method_not_allowed(&["GET", "PATCH", "DELETE"]),
    }
}

pub async fn thread_summary(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    no_query(&request, "GET /api/local/ai/threads/:id/summary")?;
    let thread_id = route_segment(&request, 5, "Thread id")?;
    Ok(json(200, &app.ai.summary(&app, &thread_id).await?))
}

/// The thread's live events: `ai.event` for conversation events, `ai.run` for run changes.
pub async fn thread_events(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "GET" {
        return method_not_allowed(&["GET"]);
    }
    no_query(&request, "GET /api/local/ai/threads/:id/events")?;
    let thread_id = route_segment(&request, 5, "Thread id")?;
    app.ai.thread(&app, &thread_id).await?;
    let updates = BroadcastStream::new(app.ai.subscribe())
        .filter_map(Result::ok)
        .take_while(Option::is_some)
        .filter_map(move |message| message.filter(|(id, _)| *id == thread_id))
        .map(|(_, event)| {
            let kind = if event.get("type").and_then(Value::as_str) == Some("ai.run") { "ai.run" } else { "ai.event" };
            Event::default().event(kind).data(event.to_string())
        });
    let opening = [Event::default().comment("connected"), Event::default().event("ai.event").data(r#"{"type":"ai.event"}"#)];
    Ok(sse(tokio_stream::iter(opening).chain(updates)))
}

pub async fn thread_turns(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    no_query(&request, "POST /api/local/ai/threads/:id/turns")?;
    let thread_id = route_segment(&request, 5, "Thread id")?;
    let body = read_json_limit(request, TURN_BODY_LIMIT, "AI chat turn body cannot exceed 25 MiB").await?;
    let run = app.ai.start_turn(&app, &thread_id, turn(&body)?).await?;
    Ok(json(202, &json!({ "run": run })))
}

pub async fn thread_compact(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    no_query(&request, "POST /api/local/ai/threads/:id/compact")?;
    let thread_id = route_segment(&request, 5, "Thread id")?;
    assert_empty_body(request, "POST /api/local/ai/threads/:id/compact").await?;
    Ok(json(200, &json!({ "thread": app.ai.compact(&app, &thread_id).await? })))
}

pub async fn interrupt(State(app): State<Arc<App>>, request: Request) -> Reply {
    if request.method() != "POST" {
        return method_not_allowed(&["POST"]);
    }
    no_query(&request, "POST /api/local/ai/runs/:id/interrupt")?;
    let run_id = route_segment(&request, 5, "Run id")?;
    assert_empty_body(request, "POST /api/local/ai/runs/:id/interrupt").await?;
    Ok(json(200, &json!({ "run": app.ai.interrupt(&app, &run_id).await? })))
}
