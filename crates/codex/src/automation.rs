//! `shared/broadcast-automation.mjs`: the frame's automation request, the Codex cron it stands
//! for, and reconciling it against Codex's own automations through the renderer bridge.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::cdp::Session;
use crate::host::{self, Context};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    EnsureActive,
    Pause,
    List,
    ApplyPolicy,
}

impl Operation {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ensure-active" => Self::EnsureActive,
            "pause" => Self::Pause,
            "list" => Self::List,
            "apply-policy" => Self::ApplyPolicy,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnsureActive => "ensure-active",
            Self::Pause => "pause",
            Self::List => "list",
            Self::ApplyPolicy => "apply-policy",
        }
    }
}

const FIELDS: [&str; 18] = [
    "id",
    "action",
    "requestId",
    "operation",
    "broadcastProjectId",
    "codexProjectId",
    "codexProjectKind",
    "codexHostId",
    "projectName",
    "workspacePath",
    "remoteProjects",
    "skillPath",
    "automationId",
    "enabledByUser",
    "quotaAware",
    "intervalMinutes",
    "model",
    "reasoningEffort",
];
const REMOTE_PROJECT_FIELDS: [&str; 4] = ["codexProjectId", "codexProjectKind", "codexHostId", "workspacePath"];
const INTERVAL_MINUTES: [u64; 5] = [5, 10, 15, 30, 60];

/// A validated `automation` host request.
#[derive(Clone, Debug)]
pub struct Request {
    pub operation: Operation,
    pub broadcast_project_id: String,
    pub codex_project_id: String,
    pub remote: bool,
    pub codex_host_id: String,
    pub project_name: String,
    pub workspace_path: String,
    /// The exact remote project mappings, as the frame sent them.
    pub remote_projects: Vec<Value>,
    pub skill_path: String,
    pub automation_id: Option<String>,
    pub enabled_by_user: bool,
    pub quota_aware: bool,
    pub interval_minutes: u64,
    pub model: String,
    pub reasoning_effort: String,
}

impl Request {
    pub fn kind(&self) -> &'static str {
        if self.remote { "remote" } else { "local" }
    }

    pub fn with_operation(&self, operation: Operation) -> Self {
        Self { operation, ..self.clone() }
    }

    pub fn rrule(&self) -> String {
        format!("RRULE:FREQ=MINUTELY;INTERVAL={}", self.interval_minutes)
    }
}

/// `parseBroadcastAutomationHostRequest`.
pub fn parse(value: &Value) -> Option<Request> {
    let object = value.as_object()?;
    if object.keys().any(|field| !FIELDS.contains(&field.as_str())) || value["action"] != "automation" {
        return None;
    }
    identifier(&value["id"], 80)?;
    identifier(&value["requestId"], 100)?;
    let operation = value["operation"].as_str().and_then(Operation::parse)?;
    let broadcast_project_id = project_id(&value["broadcastProjectId"])?;
    let codex_project_id = text(&value["codexProjectId"], 256)?;
    let project_name = text(&value["projectName"], 200)?;
    let remote = match &value["codexProjectKind"] {
        Value::Null => false,
        kind if kind == "local" => false,
        kind if kind == "remote" => true,
        _ => return None,
    };
    let codex_host_id = match &value["codexHostId"] {
        Value::Null => "local".to_owned(),
        host => text(host, 256)?,
    };
    if remote == (codex_host_id == "local") {
        return None;
    }
    let workspace_path = absolute_path(&value["workspacePath"])?;
    let skill_path = absolute_path(&value["skillPath"])?;
    let remote_projects = match value.get("remoteProjects") {
        None => Vec::new(),
        Some(Value::Array(projects)) => projects.clone(),
        Some(_) => return None,
    };
    let valid_project = |project: &Value| {
        project.as_object().is_some_and(|fields| fields.keys().all(|field| REMOTE_PROJECT_FIELDS.contains(&field.as_str())))
            && text(&project["codexProjectId"], 256).is_some()
            && project["codexProjectKind"] == "remote"
            && project["codexHostId"] == codex_host_id.as_str()
            && absolute_path(&project["workspacePath"]).is_some()
    };
    if !remote_projects.iter().all(valid_project) || (!remote && !remote_projects.is_empty()) {
        return None;
    }
    let interval_minutes = value["intervalMinutes"].as_f64().filter(|minutes| INTERVAL_MINUTES.iter().any(|allowed| *allowed as f64 == *minutes))? as u64;
    let model = text(&value["model"], 256)?;
    let reasoning_effort = text(&value["reasoningEffort"], 100)?;
    let automation_id = match value.get("automationId") {
        None => None,
        Some(automation_id) => Some(text(automation_id, 256)?),
    };
    let enabled_by_user = value["enabledByUser"].as_bool()?;
    let quota_aware = value["quotaAware"].as_bool()?;
    Some(Request {
        operation,
        broadcast_project_id,
        codex_project_id,
        remote,
        codex_host_id,
        project_name,
        workspace_path,
        remote_projects,
        skill_path,
        automation_id,
        enabled_by_user,
        quota_aware,
        interval_minutes,
        model,
        reasoning_effort,
    })
}

/// `buildBroadcastAutomationName`.
pub fn name(request: &Request) -> String {
    format!("Broadcast 自动认领 · {}", request.broadcast_project_id)
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `shellQuote`.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `buildBroadcastAutomationPrompt`. `command` is the full `broadcast` command prefix.
pub fn prompt(request: &Request, command: &str) -> String {
    let candidate_instructions = [
        "从返回的 todo 中只选择依赖已完成的议题：relations.blockedBy 为空，或其中每个依赖的 status 都严格等于 done。无依赖的 todo 仍可并行处理。若有 todo 但全部被未完成依赖阻塞，本轮直接结束，不暂停自动化，也不创建或打开新的任务会话。".to_owned(),
        "每次仅处理一个符合依赖条件的 todo：选定后先用 issue get 读取最新议题内容，并用 comment list 读取全部评论。根据描述和最新评论判断是否允许开始；若其中写明等待、暂不执行或当前不应开始，立即跳过并报告，不改状态。评论也包含已完成后被打回的返工要求。".to_owned(),
        "完成 issue get 和 comment list 后、移动状态前，必须再次运行 issue get，并复核 relations.blockedBy 仍为空或其中每个依赖的 status 都严格等于 done。若依赖条件不再满足，立即跳过并结束本轮，不改状态，也不暂停自动化。".to_owned(),
    ];
    let project_id = quoted(&request.codex_project_id);
    let host_id = quoted(&request.codex_host_id);
    let workspace_path = quoted(&request.workspace_path);
    let mut execution_instructions = Vec::new();
    if request.remote {
        let remote_projects = serde_json::to_string(&request.remote_projects).unwrap_or_default();
        execution_instructions.push(format!("本自动化仅在本机作为任务面板控制器运行；实际开发必须派发到 Codex SSH 远程项目。导入项目的基础 identity 是 projectId={project_id}、hostId={host_id}、workspacePath={workspace_path}；同一保存主机当前可用的精确远程项目映射是 {remote_projects}。不要在当前本地自动化会话修改项目文件。"));
        execution_instructions.extend(candidate_instructions);
        execution_instructions.push("先检查 issue get 的 projectId、version、status、archivedAt、threadId 和 threadBinding。完整 threadBinding 包含 threadId、codexProjectId、codexProjectKind、codexHostId、workspacePath，且它是该议题后续 send、wait 和状态写回的唯一目标；当前自动化的项目和主机只能作为未绑定议题的首次目标，不能替换已有绑定。若存在 threadId 但没有完整 threadBinding，这是只能由 UI 打开的 legacy local 绑定：使用 comment add 说明自动化无法确认项目和主机，再使用首次读取的 version 作为 --if-version、用 --binding-thread-id 保留原 threadId 将议题移动到 blocked；若冲突立即停止。不得 send、create 或覆盖该绑定。".to_owned());
        execution_instructions.push(format!("未绑定议题必须先从上述精确远程项目映射解析 actualTarget。若 developmentContext.type 是 worktree，只保留 codexProjectKind=\"remote\"、codexHostId={host_id} 且 workspacePath 与 developmentContext.path 完全相同的项；必须恰好命中一项，并使用该项自己的 codexProjectId、codexHostId 和 workspacePath。零项或多项时使用 comment add 明确记录“目标 SSH worktree 未映射”，随后结束本轮，不认领、不 create、不写基础项目 binding。若没有 worktree，actualTarget 才是上述基础 identity，并且它必须存在于精确映射中。不得回退到基础 root、local、项目名、其他主机或同路径的其他主机。"));
        execution_instructions.extend(
            [
                "确认允许开始后，只有未绑定且仍为未归档 todo 的议题才可在读取代码、下载附件、分析或实施前，由当前本地控制器使用刚读取的 version 移到 in_progress。已有完整 threadBinding 时，issue move 必须同时传 --binding-thread-id、--binding-codex-project-id、--binding-codex-project-kind、--binding-codex-host-id、--binding-workspace-path 的保存值，但在旧会话 send/stale 判断完成前不得把这个 todo 移到 in_progress；stale 清除步骤按后文显式使用 --clear-binding-thread。未绑定时必须传 --clear-binding-thread，避免把本地控制器 CODEX_THREAD_ID 写成任务绑定。写入成功后记录响应 task 的 version 为 ownedVersion、projectId 为 ownedProjectId，并记录本轮 binding；以后本轮每次 issue move 都必须显式传 --if-version ownedVersion，成功后再用响应 version 更新 ownedVersion。不得省略 --if-version 后让 broadcast 自动读取最新 version。写入成功前不得继续。所有认领、评论和状态写入只由当前本地控制器完成，不得要求远程会话运行 broadcast。",
                "若因 version 陈旧发生版本冲突，重新运行 issue get 和 comment list；仅当仍为可认领 todo、绑定身份未变化、未归档且描述和最新评论未变化时，用最新 version 重试一次。若已被认领、绑定、状态或要求已变、已归档、服务或永久 API 错误，或重试仍失败，立即跳过该议题、退出并报告；不得抢占或循环重试。",
                "认领成功后，已有完整 threadBinding 时，只能使用其保存的 threadId 和 codexHostId 调用 Codex send_message_to_thread。send 成功后必须重新 issue get 一次，确认 projectId 未变、未归档、status 仍为 todo 且完整 threadBinding 与保存值完全相同；然后由当前本地控制器使用这次复核返回的最新 version、完整旧 binding 和 --if-version 执行 issue move --status in_progress，传入 --binding-thread-id、--binding-codex-project-id、--binding-codex-project-kind、--binding-codex-host-id、--binding-workspace-path，并记录响应 task.version 为 ownedVersion。认领成功后继续执行后文现有 Codex wait_threads、结果评论和 in_review 写回路径，不得结束本轮；若认领发生 409，立即停止，不得重读新 version 覆盖。只有旧会话工具明确返回终态 NOT_FOUND 或 CLOSED 等会话不存在或已关闭结果时，才确认 stale。timeout、network failure、Codex host 暂时不可达或 Broadcast service unavailable 都不是 stale：保留 binding 并结束本轮，不得猜测、clear、create 或抢占。若任务已是 in_progress、活跃、已归档、状态或 binding 已变化，立即停止，不得在当前自动化目标创建替代会话。只有未绑定议题才使用 Codex create_thread 创建远程任务，target 必须是 {type:\"project\",projectId:actualTarget.codexProjectId,environment:{type:\"local\"}}，首次 identity 必须使用 actualTarget 的 projectId、kind=\"remote\"、hostId 和 workspacePath。发送给远程会话的指令必须包含议题编号、标题、完整描述、全部评论和开发上下文，并说明远程会话不运行 broadcast，只需完成实现、验证并返回改动、结果和剩余风险。",
                "确认旧会话 stale 后，必须先用 comment add 保存一条历史记录，并同时传 --thread-id、--binding-thread-id、--binding-codex-project-id、--binding-codex-project-kind、--binding-codex-host-id 和 --binding-workspace-path 的完整旧 binding；评论写入成功后，再使用同一次 issue get 的 version 执行 issue move --status todo --clear-binding-thread --if-version。评论或清除失败立即停止，不得认领。然后只重新 issue get 一次；仅当 projectId 未变、未归档、status 仍为 todo、threadId 为空且 threadBinding 为空时，才进入未绑定议题的现有认领和 create_thread 路径。",
                "仅当 send_message_to_thread 成功，或 create_thread 成功返回远程 threadId，才视为远程 worker 已确认。未绑定议题在 create_thread 失败时，使用 comment add 记录失败工具和错误；随后用 ownedVersion、显式 --if-version 和 --clear-binding-thread 将当前议题移回 todo 并结束。若发生 409，说明其他控制端已修改任务，立即停止且不得重读最新 version 后覆盖。此补偿只处理本轮当前已认领议题；不得扫描或接管其他 in_progress。",
                "新建远程任务成功后，使用 ownedVersion 和显式 --if-version 再次移动到 in_progress；必须用完整 binding 参数保存 create_thread 返回的 threadId，以及 actualTarget 的 projectId、kind=\"remote\"、hostId 和 workspacePath。成功后用响应 version 更新 ownedVersion 和本轮 binding。若请求响应丢失或结果不确定，只允许重新 issue get 一次；仅当 projectId 等于 ownedProjectId、未归档、状态仍为本轮 in_progress，且 threadBinding 为空或与本轮五字段 binding 完全相同时才可继续。读到相同 binding 视为前次保存成功；读到空 binding 时才可用本次核对后的 version 重试一次；读到不同 binding 或任一其他核对项变化时立即退出，不得写回。若确定绑定写入失败，使用 comment add 记录失败和远程 threadId，再用 ownedVersion、显式 --if-version 和同一完整 binding 将议题移动到 blocked；409 时停止且不得重复派发。",
                "使用 Codex wait_threads 等待远程会话时，目标必须使用任务保存的 threadBinding.threadId 和 threadBinding.codexHostId。wait_threads 失败、远程会话明确需要用户输入或无法继续时，使用 comment add 记录原因，再用 ownedVersion、显式 --if-version 和完整保存 binding 将议题移动到 blocked；409 时立即停止。远程会话完成后，使用 comment add 写入改动、验证结果、执行结果和剩余风险，再用 ownedVersion、显式 --if-version 和完整保存 binding 将议题移动到 in_review。worker 确认后的每一次 issue move 都必须显式传完整远程 binding；不要把未完成工作标记为 in_review。",
            ]
            .map(str::to_owned),
        );
    } else {
        execution_instructions.extend(candidate_instructions);
        execution_instructions.push(format!("确认允许开始后，只有 threadId 和 threadBinding 都为空且仍为未归档 todo 的议题才可在读取代码、下载附件、分析或实施前认领。认领必须使用刚读取的 version 移到 in_progress，并显式传 --binding-thread-id \"$CODEX_THREAD_ID\"、--binding-codex-project-id {project_id}、--binding-codex-project-kind \"local\"、--binding-codex-host-id {host_id}、--binding-workspace-path {workspace_path}，把当前自动化会话一次写成完整 binding；记录响应 task.version 为 ownedVersion。写入成功前不得继续。已有完整 binding 或 legacy local binding 的议题必须先按旧会话规则处理，不得先认领；不得认领已被其他会话绑定或其他 Agent 领取的议题。认领后的每一次 issue move 都必须显式传 ownedVersion 和这五个完整 binding 字段，成功后更新 ownedVersion。"));
        execution_instructions.push("若因 version 陈旧发生版本冲突，重新运行 issue get 和 comment list；仅当仍为可认领 todo、未绑定其他会话、未归档且描述和最新评论未变化时，用最新 version 重试一次。若已被认领、状态或要求已变、已归档、服务或永久 API 错误，或重试仍失败，立即跳过该议题、退出并报告；不得抢占或循环重试。".to_owned());
        execution_instructions.push(format!("若首次 issue get 返回完整 threadBinding，议题已绑定原会话：不要在当前自动化会话认领；只能使用保存的 threadId 和 codexHostId 调用 Codex send_message_to_thread。send 成功时保留 binding 并结束本轮；只有工具明确返回终态 NOT_FOUND 或 CLOSED 等会话不存在或已关闭结果时才确认 stale。timeout、network failure、Codex host 暂时不可达或 Broadcast service unavailable 都保留 binding 并结束本轮，不得猜测 stale。确认 stale 后，先用 comment add 同时传 --thread-id 和完整旧 binding 保存历史，再用同一次 issue get 的 version 执行 issue move --status todo --clear-binding-thread --if-version；然后只重新 issue get 一次，仍为未归档 todo 且 threadId、threadBinding 都为空时，才在当前自动化会话处理。若任务已是 in_progress、活跃、已归档、状态或 binding 已变化，或发生 409，立即停止，不得抢占。若返回 threadId 但没有完整 threadBinding，这是 legacy local 绑定：先调用 Codex list_threads（limit=50），合并 pinnedThreads 与 threads，并按完整 threadId 精确查找。只有恰好一项 kind=\"codex\"、projectId={project_id}、hostId={host_id}、cwd={workspace_path} 全部一致时，才把该项视为可核验旧会话；使用最新 issue version 执行 issue move --status todo --if-version，并显式传旧 threadId 及上述 projectId、kind=\"local\"、hostId、workspacePath 五字段，将 legacy local 原位升级为完整 binding。升级成功后只向该旧 threadId 和 hostId 调用 send_message_to_thread，随后结束本轮，由旧会话按议题最新要求继续。若 list_threads 未找到、出现多项或任一字段不一致，不得迁移或发送；使用 comment add 记录实际不一致项，再用首次读取的 version 和 --if-version、--binding-thread-id 保留原 threadId 将议题移到 blocked。若升级发生 409，立即停止，不得用新 version 覆盖。若没有 threadId，则按未绑定议题处理。"));
        execution_instructions.push("若议题已绑定 branch 或 worktree，必须在该议题绑定的开发上下文执行，避免并行 Agent 修改同一工作目录。".to_owned());
        execution_instructions.push("执行完成并验证后，先用 comment add 记录关键改动、验证结果、执行结果和剩余风险，再使用 ownedVersion、显式 --if-version 和认领时保存的完整 binding 将议题移动到 in_review；成功后更新 ownedVersion。不要省略 binding，避免把完整绑定降级为 legacy local；不要直接标记为 done。".to_owned());
    }
    let project = &request.broadcast_project_id;
    let mut lines = vec![
        format!(
            "[$manage-broadcast]({}) e-broadcast 每 {} 分钟检查任务面板中的「{}」项目（项目 ID：{project}，项目目录：{}）。",
            request.skill_path, request.interval_minutes, request.project_name, request.workspace_path,
        ),
        format!("本轮所有 broadcast 操作都使用完整命令前缀 {command}，不要使用 PATH 中的 broadcast。"),
        format!("开始时先运行 {command} issue list --project {project} --status todo --json。若没有 todo，直接结束；Broadcast 主机侧会暂停当前自动化，不要创建或打开新的任务会话。"),
    ];
    lines.extend(execution_instructions);
    lines.push(format!("本次处理或交接后，再次运行 {command} issue list --project {project} --status todo --json。若没有 todo，直接结束；Broadcast 主机侧会暂停当前自动化，避免后续创建空会话。"));
    lines.join("\n")
}

/// `buildBroadcastAutomationSpec`.
pub fn spec(request: &Request, command: &str) -> Map<String, Value> {
    let spec = json!({
        "kind": "cron",
        "name": name(request),
        "prompt": prompt(request, command),
        "projectId": if request.remote { Value::Null } else { request.codex_project_id.clone().into() },
        "executionEnvironment": "local",
        "localEnvironmentConfigPath": null,
        "model": request.model,
        "reasoningEffort": request.reasoning_effort,
        "rrule": request.rrule(),
    });
    spec.as_object().cloned().unwrap_or_default()
}

/// What `broadcastAutomationPolicyOperation` decides from.
#[derive(Default)]
pub struct PolicyState<'a> {
    pub explicit: bool,
    pub has_todo: Option<bool>,
    pub previous_quota: Option<&'a str>,
    pub quota: Option<&'a str>,
    pub current_status: Option<&'a str>,
    pub idle_paused: bool,
}

/// `broadcastAutomationPolicyOperation`.
pub fn policy_operation(request: &Request, state: &PolicyState<'_>) -> Operation {
    if !request.enabled_by_user || state.has_todo == Some(false) {
        return Operation::Pause;
    }
    if !state.explicit
        && state.current_status == Some("PAUSED")
        && !state.idle_paused
        && (!request.quota_aware || state.previous_quota == Some("available"))
    {
        return Operation::List;
    }
    if request.quota_aware && state.quota != Some("available") {
        return Operation::Pause;
    }
    Operation::EnsureActive
}

/// Where automation requests go: the window that asked, or the latest live window.
#[derive(Clone)]
pub enum Rpc {
    Window(Session),
    Latest(Arc<Context>),
}

impl Rpc {
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let session = match self {
            Self::Window(session) => session.clone(),
            Self::Latest(context) => context.routes.latest()?,
        };
        request(&session, method, &params).await
    }
}

/// `reconcileBroadcastAutomation`.
pub async fn reconcile(request: &Request, command: &str, rpc: &Rpc) -> Result<Map<String, Value>> {
    let listed = rpc.call("list-automations", json!({})).await?;
    let name = name(request);
    let matching: Vec<&Value> = listed["items"].as_array().into_iter().flatten().filter(|item| item["name"] == name.as_str()).collect();
    if request.operation == Operation::List {
        let items: Vec<Value> = matching.into_iter().filter_map(sanitize).collect();
        return Ok(Map::from_iter([("items".to_owned(), Value::Array(items))]));
    }
    let existing = request
        .automation_id
        .as_ref()
        .and_then(|automation_id| matching.iter().find(|item| item["id"] == automation_id.as_str()))
        .or(matching.first())
        .copied();
    let spec = spec(request, command);
    let status = match request.operation {
        Operation::Pause => "PAUSED",
        Operation::EnsureActive => "ACTIVE",
        operation => bail!("Unsupported automation operation: {}", operation.as_str()),
    };
    let Some(existing) = existing else {
        if request.operation == Operation::Pause {
            return Ok(Map::from_iter([("error".to_owned(), "not-found".into())]));
        }
        return Ok(object(rpc.call("automation-create", Value::Object(spec)).await?));
    };
    if matches_spec(existing, &spec, status) {
        return Ok(Map::from_iter([("item".to_owned(), existing.clone())]));
    }
    let mut update = spec;
    if let Some(id) = existing.get("id") {
        update.insert("id".to_owned(), id.clone());
    }
    update.insert("status".to_owned(), status.into());
    Ok(object(rpc.call("automation-update", Value::Object(update)).await?))
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// `sanitizeAutomation`.
fn sanitize(item: &Value) -> Option<Value> {
    let id = text(&item["id"], 256)?;
    let status = item["status"].as_str().filter(|status| *status == "ACTIVE" || *status == "PAUSED")?;
    let model = text(&item["model"], 256)?;
    let reasoning_effort = text(&item["reasoningEffort"], 100)?;
    let rrule = item["rrule"].as_str().filter(|rrule| {
        rrule.strip_prefix("RRULE:FREQ=MINUTELY;INTERVAL=").is_some_and(|minutes| ["5", "10", "15", "30", "60"].contains(&minutes))
    })?;
    let mut sanitized = json!({ "id": id, "status": status, "model": model, "reasoningEffort": reasoning_effort, "rrule": rrule });
    if let Some(next_run_at @ (Value::Null | Value::Number(_))) = item.get("nextRunAt") {
        sanitized["nextRunAt"] = next_run_at.clone();
    }
    Some(sanitized)
}

/// `automationMatchesSpec`.
fn matches_spec(item: &Value, spec: &Map<String, Value>, status: &str) -> bool {
    item["status"] == status
        && spec.iter().all(|(field, value)| {
            if field == "projectId" {
                let project = [item.get("projectId"), item.get("target").and_then(|target| target.get("projectId"))]
                    .into_iter()
                    .flatten()
                    .find(|project| !project.is_null())
                    .unwrap_or(&Value::Null);
                project == value
            } else {
                item.get(field) == Some(value)
            }
        })
}

/// `requestCodexAutomationViaCdp`: a `vscode://codex/<method>` fetch through the renderer bridge.
async fn request(session: &Session, method: &str, params: &Value) -> Result<Value> {
    let request_id = json!(host::request_id("broadcast-automation"));
    let expression = format!(
        r#"(() => new Promise((resolve) => {{
      const method = {method};
      const params = {params};
      const requestId = {request_id};
      const bridge = window.electronBridge;
      if (!bridge || typeof bridge.sendMessageFromView !== "function") {{
        resolve({{ ok: false, error: "当前 Codex 版本没有提供原生自动任务能力" }});
        return;
      }}
      let settled = false;
      const finish = (result) => {{
        if (settled) return;
        settled = true;
        window.clearTimeout(timeout);
        window.removeEventListener("message", onMessage);
        resolve(result);
      }};
      const onMessage = (event) => {{
        const message = event.data;
        if (
          !message
          || typeof message !== "object"
          || message.type !== "fetch-response"
          || message.requestId !== requestId
        ) return;
        finish({{
          ok: true,
          responseType: message.responseType,
          status: message.status,
          bodyJsonString: message.bodyJsonString,
        }});
      }};
      const timeout = window.setTimeout(
        () => finish({{ ok: false, error: "Codex 自动任务接口没有响应" }}),
        10_000,
      );
      window.addEventListener("message", onMessage);
      Promise.resolve(bridge.sendMessageFromView({{
        type: "fetch",
        requestId,
        method: "POST",
        url: `vscode://codex/{path}`,
        body: JSON.stringify(params),
      }})).catch((error) => {{
        finish({{
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        }});
      }});
    }}))()"#,
        method = json!(method),
        path = method,
    );
    let evaluation = session.send("Runtime.evaluate", json!({ "expression": expression, "awaitPromise": true, "returnByValue": true })).await?;
    if let Some(description) = crate::cdp::exception(&evaluation) {
        bail!(if description.is_empty() { "Codex automation request failed".to_owned() } else { description });
    }
    let response = &evaluation["result"]["value"];
    if response["ok"] != true {
        let error = response["error"].as_str().filter(|error| !error.is_empty()).unwrap_or("Codex automation request failed");
        return Err(anyhow!(error.to_owned()));
    }
    let status = response.get("status");
    if !status.and_then(Value::as_f64).is_some_and(|status| status.fract() == 0.0 && (200.0..300.0).contains(&status)) {
        bail!("Codex automation request returned HTTP {}", display(status));
    }
    match response["bodyJsonString"].as_str() {
        None | Some("") => Ok(json!({})),
        Some(body) => serde_json::from_str(body).map_err(|_| anyhow!("Codex automation request returned invalid JSON")),
    }
}

/// A JavaScript template literal's rendering of a value (`undefined` when missing).
pub fn display(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items.iter().map(|item| if item.is_null() { String::new() } else { display(Some(item)) }).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
        Some(value) => value.to_string(),
    }
}

/// JavaScript truthiness (`undefined` when missing).
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0 && !number.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// JavaScript's `Number(value)` (`NaN` when missing).
pub fn number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Object(_) | Value::Array(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => {
            let text = text.trim_matches(js_space);
            match text {
                "" => 0.0,
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                text if text.bytes().all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte)) => text.parse().unwrap_or(f64::NAN),
                _ => f64::NAN,
            }
        }
    }
}

/// A number as JavaScript's `JSON.stringify` writes it: integral values without a fraction.
pub fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 { json!(value as i64) } else { json!(value) }
}

/// JavaScript's `WhiteSpace` and `LineTerminator`, which `String.prototype.trim` removes.
pub fn js_space(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn identifier(value: &Value, max: usize) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= max && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
        .map(str::to_owned)
}

fn project_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
        .map(str::to_owned)
}

/// `validText`: trimmed, non-empty, at most `max` UTF-16 units and free of control characters.
pub fn text(value: &Value, max: usize) -> Option<String> {
    value
        .as_str()
        .filter(|value| value.trim_matches(js_space).len() == value.len() && host::bounded_text(value, max))
        .map(str::to_owned)
}

/// `validAbsolutePath`: absolute on POSIX or Windows.
fn absolute_path(value: &Value) -> Option<String> {
    text(value, 2_048).filter(|path| {
        let bytes = path.as_bytes();
        matches!(bytes.first(), Some(b'/' | b'\\'))
            || (bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\'))
    })
}
