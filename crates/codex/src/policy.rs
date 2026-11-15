//! Quota policies: each project's saved automation settings, kept in
//! `codex-automation-policies.json`, applied through a per-project queue and re-checked on a
//! timer, so cron pauses while the account is out of quota or no todo may start.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

use crate::automation::{self, Operation, PolicyState, Request, Rpc, js_number, number, truthy};
use crate::cdp::Session;
use crate::host::{self, Context, chrono_millis, lock};
use crate::{quota, remote};

const FILE: &str = "codex-automation-policies.json";

/// `quotaPolicyRecords`, `quotaPolicyTimers` and `quotaPolicyQueues`.
#[derive(Default)]
pub struct Policies {
    /// In insertion order, like the JavaScript `Map` the file was written from.
    records: Mutex<Vec<Record>>,
    timers: Mutex<HashMap<String, JoinHandle<()>>>,
    queues: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    writes: tokio::sync::Mutex<()>,
    loaded: tokio::sync::OnceCell<()>,
}

#[derive(Clone)]
struct Record {
    version: u64,
    request: Request,
    quota: Option<Value>,
    todo_gate: Option<Value>,
    next_run_at: Option<f64>,
    /// `todoCheckInFlight`.
    checking: Arc<AtomicBool>,
}

impl Record {
    fn new(version: u64, request: Request) -> Self {
        Self {
            version,
            request,
            quota: None,
            todo_gate: None,
            next_run_at: None,
            checking: Arc::default(),
        }
    }

    fn key(&self) -> &str {
        &self.request.broadcast_project_id
    }
}

/// A finished "may a todo start?" check, valid for the policy version it ran under.
struct Evaluated {
    version: u64,
    snapshot: String,
    state: &'static str,
}

impl Policies {
    pub fn version(&self, key: &str) -> Option<u64> {
        lock(&self.records)
            .iter()
            .find(|record| record.key() == key)
            .map(|record| record.version)
    }

    fn get(&self, key: &str) -> Option<Record> {
        lock(&self.records)
            .iter()
            .find(|record| record.key() == key)
            .cloned()
    }

    fn set(&self, record: Record) {
        let mut records = lock(&self.records);
        match records.iter_mut().find(|known| known.key() == record.key()) {
            Some(known) => *known = record,
            None => records.push(record),
        }
    }

    fn remove(&self, key: &str) {
        lock(&self.records).retain(|record| record.key() != key);
    }

    /// Writes back a record taken at `version`, unless another policy replaced it meanwhile.
    fn replace(&self, version: u64, record: Record) {
        if let Some(known) = lock(&self.records)
            .iter_mut()
            .find(|known| known.key() == record.key() && known.version == version)
        {
            *known = record;
        }
    }
}

fn stale(quota: Option<Value>) -> Map<String, Value> {
    Map::from_iter([
        ("quota".to_owned(), quota.unwrap_or(Value::Null)),
        ("stale".to_owned(), true.into()),
    ])
}

/// `storedAutomationPolicy`.
fn stored(request: &Request) -> Map<String, Value> {
    let mut policy = Map::new();
    policy.insert(
        "broadcastProjectId".to_owned(),
        request.broadcast_project_id.clone().into(),
    );
    policy.insert(
        "codexProjectId".to_owned(),
        request.codex_project_id.clone().into(),
    );
    policy.insert("codexProjectKind".to_owned(), request.kind().into());
    policy.insert(
        "codexHostId".to_owned(),
        request.codex_host_id.clone().into(),
    );
    policy.insert(
        "projectName".to_owned(),
        request.project_name.clone().into(),
    );
    policy.insert(
        "workspacePath".to_owned(),
        request.workspace_path.clone().into(),
    );
    policy.insert(
        "remoteProjects".to_owned(),
        Value::Array(request.remote_projects.clone()),
    );
    policy.insert("skillPath".to_owned(), request.skill_path.clone().into());
    if let Some(automation_id) = &request.automation_id {
        policy.insert("automationId".to_owned(), automation_id.clone().into());
    }
    policy.insert("enabledByUser".to_owned(), request.enabled_by_user.into());
    policy.insert("quotaAware".to_owned(), request.quota_aware.into());
    policy.insert(
        "intervalMinutes".to_owned(),
        request.interval_minutes.into(),
    );
    policy.insert("model".to_owned(), request.model.clone().into());
    policy.insert(
        "reasoningEffort".to_owned(),
        request.reasoning_effort.clone().into(),
    );
    policy
}

/// `restoredAutomationPolicy`.
fn restored(value: &Value) -> Option<Record> {
    let mut fields = value.as_object()?.clone();
    let next_run_at = fields.remove("nextRunAt");
    let quota = fields.remove("quota");
    let todo_gate = fields.remove("todoGate");
    for (field, text) in [
        ("id", "restored-policy"),
        ("action", "automation"),
        ("requestId", "restored-policy"),
        ("operation", "apply-policy"),
    ] {
        fields.insert(field.to_owned(), text.into());
    }
    let mut record = Record::new(1, automation::parse(&Value::Object(fields))?);
    record.quota = quota.filter(|quota| truthy(Some(quota)));
    record.todo_gate = todo_gate.filter(|gate| {
        gate["snapshot"].is_string()
            && ["checking", "wait", "start"]
                .iter()
                .any(|state| gate["state"] == *state)
    });
    record.next_run_at = next_run_at
        .as_ref()
        .and_then(Value::as_f64)
        .filter(|next| next.is_finite());
    Some(record)
}

/// `ensureQuotaPoliciesLoaded`.
async fn ensure_loaded(context: &Context) -> Result<()> {
    context
        .policies
        .loaded
        .get_or_try_init(|| async {
            let text = match tokio::fs::read_to_string(context.data_dir.join(FILE)).await {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(anyhow::Error::from(error)),
            };
            let stored: Value = serde_json::from_str(&text)?;
            for record in stored
                .as_object()
                .into_iter()
                .flat_map(Map::values)
                .filter_map(restored)
            {
                context.policies.set(record);
            }
            Ok(())
        })
        .await?;
    Ok(())
}

/// `persistQuotaPolicies`: writes run one at a time, each with the records current when it runs.
async fn persist(context: &Context) -> Result<()> {
    let _write = context.policies.writes.lock().await;
    let data: Map<String, Value> = lock(&context.policies.records)
        .iter()
        .map(|record| {
            let mut policy = stored(&record.request);
            if let Some(quota) = &record.quota {
                policy.insert("quota".to_owned(), quota.clone());
            }
            if let Some(todo_gate) = &record.todo_gate {
                policy.insert("todoGate".to_owned(), todo_gate.clone());
            }
            if let Some(next_run_at) = record.next_run_at {
                policy.insert("nextRunAt".to_owned(), js_number(next_run_at));
            }
            (record.key().to_owned(), Value::Object(policy))
        })
        .collect();
    let text = format!("{}\n", serde_json::to_string_pretty(&data)?);
    host::write_private(
        &context.data_dir,
        &context.data_dir.join(FILE),
        text.as_bytes(),
    )
    .await
}

/// `scheduleQuotaPolicyCheck`: the next check comes at the next cron run (a remote policy's own
/// run), after a quota reset, or in a second while a todo check is due.
fn schedule(context: &Arc<Context>, record: &Record, result: &Value) {
    let request = &record.request;
    let key = record.key().to_owned();
    if let Some(previous) = lock(&context.policies.timers).remove(&key) {
        previous.abort();
    }
    if !request.enabled_by_user {
        return;
    }
    let now = chrono_millis() as f64;
    let next_run_at = number(result.get("item").and_then(|item| item.get("nextRunAt")));
    let quota_state = result["quota"]["state"].as_str();
    let check_todos = result["todoGate"]["state"] == "checking"
        && (!request.quota_aware || quota_state == Some("available"));
    let next_run_delay = if check_todos {
        1_000.0
    } else if next_run_at.is_finite() && next_run_at > now {
        (next_run_at - now - if request.remote { 0.0 } else { 15_000.0 }).max(1_000.0)
    } else {
        60_000.0
    };
    let resets_at = result["quota"]["resetsAt"]
        .as_f64()
        .filter(|resets_at| resets_at.is_finite());
    let reset_delay = match resets_at {
        Some(resets_at) if quota_state == Some("blocked") => {
            (resets_at * 1_000.0 - now + 1_000.0).max(1_000.0)
        }
        _ => next_run_delay,
    };
    let delay = Duration::from_secs_f64(next_run_delay.min(reset_delay) / 1_000.0);
    let remote_active = request.remote && result["item"]["status"] == "ACTIVE";
    let local_check = !request.remote && check_todos;
    let version = record.version;
    let timer = tokio::spawn({
        let context = Arc::clone(context);
        let key = key.clone();
        async move {
            tokio::time::sleep(delay).await;
            // The check outlives its timer: rescheduling from inside it must not cancel it.
            tokio::spawn(fire(context, key, version, remote_active, local_check));
        }
    });
    lock(&context.policies.timers).insert(key, timer);
}

async fn fire(
    context: Arc<Context>,
    key: String,
    version: u64,
    remote_active: bool,
    local_check: bool,
) {
    let Some(record) = context
        .policies
        .get(&key)
        .filter(|record| record.version == version)
    else {
        return;
    };
    let outcome = async {
        if remote_active {
            remote::run(&context, &record.request, version).await?;
        }
        let mut evaluated = None;
        if local_check {
            // Model work runs outside the queue, after cron has been paused, so reads and a
            // manual pause never wait for a turn.
            if record.checking.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let checked = evaluate_local(&context, &record).await;
            record.checking.store(false, Ordering::Release);
            evaluated = checked?;
        }
        if context.policies.version(&key) != Some(version) {
            return Ok(());
        }
        enqueue_current(&context, &key, evaluated).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = outcome {
        eprintln!("Broadcast quota policy check failed: {error}");
        if let Some(current) = context
            .policies
            .get(&key)
            .filter(|record| record.version == version)
        {
            schedule(
                &context,
                &current,
                &json!({ "quota": { "state": "unknown" } }),
            );
        }
    }
}

/// `localAutomationTodoGate`: no gate when no todo is a candidate; otherwise the evaluated
/// decision for these todos, the previous one, or a fresh check.
async fn local_gate(
    context: &Context,
    request: &Request,
    tasks: &[Value],
    previous: Option<Value>,
    evaluated: Option<Evaluated>,
) -> Result<Option<Value>> {
    let (candidates, snapshot) = remote::todo_inputs(context, request, tasks).await?;
    if candidates.is_empty() {
        return Ok(None);
    }
    if let Some(evaluated) = evaluated.filter(|evaluated| evaluated.snapshot == snapshot) {
        return Ok(Some(
            json!({ "snapshot": snapshot, "state": evaluated.state }),
        ));
    }
    if previous
        .as_ref()
        .is_some_and(|gate| gate["snapshot"] == snapshot.as_str())
    {
        return Ok(previous);
    }
    Ok(Some(json!({ "snapshot": snapshot, "state": "checking" })))
}

/// `evaluateLocalAutomationTodos`: whether any candidate todo may start, decided by ephemeral turns.
async fn evaluate_local(context: &Context, record: &Record) -> Result<Option<Evaluated>> {
    let request = &record.request;
    let still_current = || context.policies.version(record.key()) == Some(record.version);
    let listed = remote::get(context, &remote::todo_path(request)).await?;
    let (candidates, snapshot) =
        remote::todo_inputs(context, request, &remote::list(&listed, "tasks")).await?;
    let gate = record.todo_gate.as_ref();
    if !still_current()
        || gate
            .is_none_or(|gate| gate["snapshot"] != snapshot.as_str() || gate["state"] != "checking")
    {
        return Ok(None);
    }
    let mut state = "wait";
    for (task, comments) in &candidates {
        if !still_current() {
            return Ok(None);
        }
        if remote::can_start(context, &context.routes.latest()?, request, task, comments).await? {
            state = "start";
            break;
        }
    }
    Ok(still_current().then_some(Evaluated {
        version: record.version,
        snapshot,
        state,
    }))
}

struct Previous {
    explicit: bool,
    quota: Option<String>,
    remote_next_run_at: Option<f64>,
    todo_gate: Option<Value>,
    evaluated: Option<Evaluated>,
}

/// `applyBroadcastAutomationPolicy`: reads todos and quota, then pauses, keeps or activates the
/// project's automation.
async fn apply(
    context: &Context,
    request: &Request,
    rpc: &Rpc,
    still_current: impl Fn() -> bool,
    previous: Previous,
) -> Result<Map<String, Value>> {
    let tasks = if request.enabled_by_user {
        let response = context
            .http
            .get(format!(
                "{}{}",
                context.base_url,
                remote::todo_path(request)
            ))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("Broadcast todo check returned HTTP {}", status.as_u16());
        }
        let payload: Value = serde_json::from_str(&response.text().await?).unwrap_or(Value::Null);
        Some(
            payload["tasks"]
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow!("Broadcast todo check returned invalid JSON"))?,
        )
    } else {
        None
    };
    let has_todo = tasks.as_ref().map(|tasks| !tasks.is_empty());
    let quota = if request.quota_aware && has_todo != Some(false) {
        Some(quota::read(&context.codex, &request.model).await)
    } else {
        None
    };
    if !still_current() {
        return Ok(stale(quota));
    }
    let quota_state = quota.as_ref().and_then(|quota| quota["state"].as_str());
    let previous_quota = previous.quota.as_deref();
    let has_todo_value = has_todo.map_or(Value::Null, Value::Bool);

    if request.remote {
        let current_status = if request.enabled_by_user
            && (!request.quota_aware || previous_quota == Some("available"))
        {
            "ACTIVE"
        } else {
            "PAUSED"
        };
        let current_item = remote::item(
            request,
            current_status,
            previous.remote_next_run_at.map(js_number),
        );
        let state = PolicyState {
            explicit: previous.explicit,
            has_todo,
            previous_quota,
            quota: quota_state,
            current_status: Some(current_status),
            idle_paused: false,
        };
        let operation = automation::policy_operation(request, &state);
        let status = if operation == Operation::Pause {
            "PAUSED"
        } else {
            "ACTIVE"
        };
        let now = chrono_millis() as f64;
        let existing = previous.remote_next_run_at.unwrap_or(f64::NAN);
        let next_run_at = if existing.is_finite() && existing > now {
            existing
        } else {
            now + request.interval_minutes as f64 * 60_000.0
        };
        let item = if operation == Operation::List {
            current_item
        } else {
            remote::item(
                request,
                status,
                Some(if status == "ACTIVE" {
                    js_number(next_run_at)
                } else {
                    Value::Null
                }),
            )
        };
        let mut result = Map::new();
        result.insert("item".to_owned(), item.clone());
        result.insert("items".to_owned(), json!([item]));
        result.insert("operation".to_owned(), operation.as_str().into());
        result.insert("hasTodo".to_owned(), has_todo_value);
        if let Some(quota) = quota {
            result.insert("quota".to_owned(), quota);
        }
        return Ok(result);
    }

    let mut listed = None;
    let mut current_item = None;
    if !previous.explicit && request.enabled_by_user {
        let items = automation::reconcile(
            &request.with_operation(Operation::List),
            &context.cli_command,
            rpc,
        )
        .await?;
        let found = items["items"].as_array().and_then(|items| {
            request
                .automation_id
                .as_ref()
                .and_then(|automation_id| {
                    items
                        .iter()
                        .find(|item| item["id"] == automation_id.as_str())
                })
                .or(items.first())
                .cloned()
        });
        current_item = found;
        listed = Some(items);
    }
    let mut todo_gate = if request.enabled_by_user && has_todo == Some(true) {
        previous.todo_gate
    } else {
        None
    };
    let gate_state = |gate: &Option<Value>| {
        gate.as_ref()
            .and_then(|gate| gate["state"].as_str().map(str::to_owned))
    };
    let state = PolicyState {
        explicit: previous.explicit,
        has_todo,
        previous_quota,
        quota: quota_state,
        current_status: current_item
            .as_ref()
            .and_then(|item| item["status"].as_str()),
        idle_paused: matches!(gate_state(&todo_gate).as_deref(), Some("checking" | "wait")),
    };
    let mut operation = automation::policy_operation(request, &state);
    if operation == Operation::EnsureActive {
        todo_gate = local_gate(
            context,
            request,
            tasks.as_deref().unwrap_or_default(),
            todo_gate,
            previous.evaluated,
        )
        .await?;
        if todo_gate.is_some() && gate_state(&todo_gate).as_deref() != Some("start") {
            operation = Operation::Pause;
        }
    } else if operation == Operation::List {
        // A native or manual pause is not an automatic wait.
        todo_gate = None;
    }
    if !still_current() {
        return Ok(stale(quota));
    }
    let idle_reason = match gate_state(&todo_gate).as_deref() {
        Some("checking") => Some("checking-todos"),
        Some("wait") => Some("waiting-todos"),
        _ => None,
    };
    let mut result = if operation == Operation::List {
        let mut result = Map::new();
        if let Some(item) = current_item {
            result.insert("item".to_owned(), item);
        }
        result.insert(
            "items".to_owned(),
            listed
                .and_then(|mut listed| listed.remove("items"))
                .unwrap_or(Value::Null),
        );
        result
    } else {
        automation::reconcile(
            &request.with_operation(operation),
            &context.cli_command,
            rpc,
        )
        .await?
    };
    if result
        .get("error")
        .is_some_and(|error| error == "not-found")
    {
        result = Map::new();
    }
    result.insert("operation".to_owned(), operation.as_str().into());
    result.insert("hasTodo".to_owned(), has_todo_value);
    if let Some(todo_gate) = todo_gate {
        result.insert("todoGate".to_owned(), todo_gate);
    }
    if let Some(idle_reason) = idle_reason {
        result.insert("idleReason".to_owned(), idle_reason.into());
    }
    if let Some(quota) = quota {
        result.insert("quota".to_owned(), quota);
    }
    Ok(result)
}

/// `enqueueQuotaPolicyMutation`: one apply at a time per project, each against the record
/// current when it runs, then saved and rescheduled.
async fn enqueue(
    context: &Arc<Context>,
    key: &str,
    version: u64,
    rpc: Rpc,
    explicit: bool,
    evaluated: Option<Evaluated>,
) -> Result<Map<String, Value>> {
    let queue = Arc::clone(
        lock(&context.policies.queues)
            .entry(key.to_owned())
            .or_default(),
    );
    let _turn = queue.lock().await;
    let Some(mut current) = context
        .policies
        .get(key)
        .filter(|record| record.version == version)
    else {
        return Ok(Map::from_iter([("stale".to_owned(), true.into())]));
    };
    let previous = Previous {
        explicit,
        quota: current
            .quota
            .as_ref()
            .and_then(|quota| quota["state"].as_str())
            .map(str::to_owned),
        remote_next_run_at: current.next_run_at,
        todo_gate: current.todo_gate.clone(),
        evaluated: evaluated.filter(|evaluated| evaluated.version == version),
    };
    let still_current = || context.policies.version(key) == Some(version);
    let result = apply(context, &current.request, &rpc, still_current, previous).await?;
    if result.get("stale").is_some_and(|stale| truthy(Some(stale))) {
        return Ok(result);
    }
    let operation = result.get("operation").and_then(Value::as_str);
    let item = result.get("item");
    let item_status = item
        .and_then(|item| item.get("status"))
        .and_then(Value::as_str);
    if (result.get("hasTodo") == Some(&Value::Bool(false)) && operation == Some("pause"))
        || (!explicit && operation == Some("list") && item_status == Some("PAUSED"))
    {
        current.version += 1;
        current.request.enabled_by_user = false;
    }
    if let Some(id) = item
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        current.request.automation_id = Some(id.to_owned());
    }
    if current.request.remote {
        let next_run_at = number(item.and_then(|item| item.get("nextRunAt")));
        current.next_run_at =
            (item_status == Some("ACTIVE") && next_run_at.is_finite()).then_some(next_run_at);
    }
    current.todo_gate = result
        .get("todoGate")
        .filter(|gate| truthy(Some(gate)))
        .cloned();
    if !current.request.quota_aware {
        current.quota = None;
    } else if let Some(quota) = result.get("quota").filter(|quota| truthy(Some(quota))) {
        current.quota = Some(quota.clone());
    }
    context.policies.replace(version, current.clone());
    persist(context).await?;
    schedule(context, &current, &Value::Object(result.clone()));
    Ok(result)
}

/// The result with the policy as saved.
fn with_policy(context: &Context, key: &str, mut result: Map<String, Value>) -> Map<String, Value> {
    if let Some(current) = context.policies.get(key) {
        result.insert("policy".to_owned(), Value::Object(stored(&current.request)));
        if let Some(quota) = current.quota {
            result.insert("quota".to_owned(), quota);
        }
    }
    result
}

/// `updateAndApplyQuotaPolicy`: saves the frame's settings as a new policy version and applies
/// them; a failed apply restores the previous policy.
async fn update_and_apply(
    context: &Arc<Context>,
    request: Request,
    rpc: Rpc,
) -> Result<Map<String, Value>> {
    ensure_loaded(context).await?;
    let key = request.broadcast_project_id.clone();
    let previous = context.policies.get(&key);
    let mut record = Record::new(
        previous.as_ref().map_or(0, |previous| previous.version) + 1,
        request,
    );
    if record.request.quota_aware {
        record.quota = previous
            .as_ref()
            .and_then(|previous| previous.quota.clone());
    }
    let version = record.version;
    context.policies.set(record);
    let outcome = async {
        persist(context).await?;
        let result = enqueue(context, &key, version, rpc, true, None).await?;
        Ok(with_policy(context, &key, result))
    }
    .await;
    if outcome.is_err() && context.policies.version(&key) == Some(version) {
        match previous {
            Some(previous) => context.policies.set(previous),
            None => context.policies.remove(&key),
        }
        persist(context).await?;
    }
    outcome
}

/// `reconcileStoredAutomationPolicy`: the saved policy for this project, moved to the frame's
/// project identity when that changed.
async fn reconcile_stored(
    context: &Arc<Context>,
    request: &Request,
    rpc: Rpc,
) -> Result<Option<Map<String, Value>>> {
    ensure_loaded(context).await?;
    let key = &request.broadcast_project_id;
    let Some(record) = context.policies.get(key) else {
        return Ok(None);
    };
    let saved = &record.request;
    if saved.codex_project_id != request.codex_project_id
        || saved.remote != request.remote
        || saved.codex_host_id != request.codex_host_id
        || saved.workspace_path != request.workspace_path
        || serde_json::to_string(&saved.remote_projects).ok()
            != serde_json::to_string(&request.remote_projects).ok()
    {
        let merged = Request {
            automation_id: saved.automation_id.clone(),
            enabled_by_user: saved.enabled_by_user,
            quota_aware: saved.quota_aware,
            interval_minutes: saved.interval_minutes,
            model: saved.model.clone(),
            reasoning_effort: saved.reasoning_effort.clone(),
            ..request.clone()
        };
        return update_and_apply(context, merged, rpc).await.map(Some);
    }
    let result = enqueue(context, key, record.version, rpc, false, None).await?;
    Ok(Some(with_policy(context, key, result)))
}

/// `enqueueCurrentQuotaPolicy`: a re-check through the latest live window.
async fn enqueue_current(
    context: &Arc<Context>,
    key: &str,
    evaluated: Option<Evaluated>,
) -> Result<Map<String, Value>> {
    ensure_loaded(context).await?;
    let Some(version) = context.policies.version(key) else {
        return Ok(Map::from_iter([("stale".to_owned(), true.into())]));
    };
    enqueue(
        context,
        key,
        version,
        Rpc::Latest(Arc::clone(context)),
        false,
        evaluated,
    )
    .await
}

/// `restoreQuotaPolicies`: re-applies every enabled saved policy once a window can carry it.
pub async fn restore(context: &Arc<Context>) -> Result<()> {
    ensure_loaded(context).await?;
    let keys: Vec<String> = lock(&context.policies.records)
        .iter()
        .map(|record| record.key().to_owned())
        .collect();
    for key in keys {
        if context
            .policies
            .get(&key)
            .is_some_and(|record| record.request.enabled_by_user)
        {
            enqueue_current(context, &key, None).await?;
        }
    }
    Ok(())
}

/// The `automation` host action.
pub async fn run_automation(
    context: &Arc<Context>,
    session: &Session,
    request: Request,
) -> Result<Map<String, Value>> {
    let rpc = Rpc::Window(session.clone());
    match request.operation {
        Operation::List => {
            if let Some(stored) = reconcile_stored(context, &request, rpc.clone()).await? {
                return Ok(stored);
            }
            if request.remote {
                return Ok(Map::from_iter([("items".to_owned(), json!([]))]));
            }
            automation::reconcile(&request, &context.cli_command, &rpc).await
        }
        Operation::ApplyPolicy => update_and_apply(context, request, rpc).await,
        _ => automation::reconcile(&request, &context.cli_command, &rpc).await,
    }
}
