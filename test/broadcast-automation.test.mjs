import assert from "node:assert/strict";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  buildBroadcastAutomationName,
  buildBroadcastAutomationPrompt,
  buildBroadcastAutomationSpec,
  parseBroadcastAutomationHostRequest,
  reconcileBroadcastAutomation,
  broadcastAutomationPolicyOperation,
} from "../shared/broadcast-automation.mjs";

const baseRequest = {
  id: "host-request-1",
  action: "automation",
  requestId: "iframe-request-1",
  operation: "ensure-active",
  broadcastProjectId: "ppt-skill",
  codexProjectId: "codex-project-123",
  codexProjectKind: "local",
  codexHostId: "local",
  projectName: "PPT Skill",
  workspacePath: "/Users/example/Documents/ppt-skill",
  skillPath: "/Users/example/broadcast/skills/manage-broadcast/SKILL.md",
  enabledByUser: true,
  quotaAware: false,
  intervalMinutes: 5,
  model: "gpt-5.5",
  reasoningEffort: "high",
};

const remoteRequest = {
  ...baseRequest,
  codexProjectId: "remote-project-123",
  codexProjectKind: "remote",
  codexHostId: "remote-ssh-discovered:merlin-agent",
  projectName: "Playground",
  workspacePath: "/mlx_devbox/users/example/playground",
  remoteProjects: [
    {
      codexProjectId: "remote-project-123",
      codexProjectKind: "remote",
      codexHostId: "remote-ssh-discovered:merlin-agent",
      workspacePath: "/mlx_devbox/users/example/playground",
    },
    {
      codexProjectId: "remote-worktree-456",
      codexProjectKind: "remote",
      codexHostId: "remote-ssh-discovered:merlin-agent",
      workspacePath: "/mlx_devbox/users/example/playground-worktree",
    },
  ],
};

test("the automation host request accepts catalog-provided project automation options", () => {
  assert.deepEqual(parseBroadcastAutomationHostRequest(baseRequest), baseRequest);
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, operation: "delete" }),
    null,
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, method: "automation-delete" }),
    null,
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, prompt: "arbitrary" }),
    null,
  );
  assert.deepEqual(
    parseBroadcastAutomationHostRequest({ ...baseRequest, intervalMinutes: 10 }),
    { ...baseRequest, intervalMinutes: 10 },
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, intervalMinutes: 7 }),
    null,
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({
      ...baseRequest,
      model: "gpt-5.6-sol",
      reasoningEffort: "ultra",
    })?.reasoningEffort,
    "ultra",
  );
  assert.deepEqual(
    parseBroadcastAutomationHostRequest({
      ...baseRequest,
      model: "gemini-3.1-pro-preview",
      reasoningEffort: "xhigh",
    }),
    {
      ...baseRequest,
      model: "gemini-3.1-pro-preview",
      reasoningEffort: "xhigh",
    },
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, reasoningEffort: "xhigh" })?.reasoningEffort,
    "xhigh",
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, workspacePath: "relative/path" }),
    null,
  );
  assert.deepEqual(parseBroadcastAutomationHostRequest(remoteRequest), remoteRequest);
  const windowsRemoteRequest = {
    ...remoteRequest,
    workspacePath: String.raw`C:\Users\admin\Documents\broadcast`,
    remoteProjects: [{
      codexProjectId: "remote-project-123",
      codexProjectKind: "remote",
      codexHostId: "remote-ssh-discovered:merlin-agent",
      workspacePath: String.raw`C:\Users\admin\Documents\broadcast`,
    }],
  };
  assert.deepEqual(
    parseBroadcastAutomationHostRequest(windowsRemoteRequest),
    windowsRemoteRequest,
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...remoteRequest, codexHostId: "local" }),
    null,
  );
  assert.equal(
    parseBroadcastAutomationHostRequest({ ...baseRequest, codexHostId: "remote-host" }),
    null,
  );
});

test("the stable name and generated prompt are project-scoped and encode the claim protocol", () => {
  assert.equal(
    buildBroadcastAutomationName(baseRequest),
    "Broadcast 自动认领 · ppt-skill",
  );

  const prompt = buildBroadcastAutomationPrompt(baseRequest);
  assert.match(
    prompt,
    /\[\$manage-broadcast\]\(\/Users\/example\/broadcast\/skills\/manage-broadcast\/SKILL\.md\)/,
  );
  assert.match(prompt, /\[\$manage-broadcast\]\([^)]*\) e-broadcast /);
  assert.match(prompt, /PPT Skill/);
  assert.match(prompt, /每 5 分钟检查/);
  assert.match(prompt, /ppt-skill/);
  assert.match(prompt, /\/Users\/example\/Documents\/ppt-skill/);
  assert.match(prompt, /每次仅处理一个符合依赖条件的 todo/);
  assert.match(prompt, /issue get/);
  assert.match(prompt, /comment list/);
  assert.match(prompt, /最新 version/);
  assert.match(prompt, /in_progress/);
  assert.match(prompt, /版本冲突.*跳过/);
  assert.match(prompt, /关键改动、验证结果、执行结果和剩余风险/);
  assert.match(prompt, /in_review/);
  assert.match(prompt, /已绑定.*branch.*worktree/);
  assert.match(prompt, /Codex list_threads/);
  assert.match(prompt, /list_threads（limit=50）/);
  assert.match(prompt, /pinnedThreads 与 threads/);
  assert.match(prompt, /projectId="codex-project-123"/);
  assert.match(prompt, /hostId="local"/);
  assert.match(prompt, /cwd="\/Users\/example\/Documents\/ppt-skill"/);
  assert.match(prompt, /legacy local 原位升级为完整 binding/);
  assert.match(prompt, /--binding-thread-id "\$CODEX_THREAD_ID"/);
  assert.match(prompt, /认领后的每一次 issue move.*五个完整 binding 字段/);
  assert.match(prompt, /不要省略 binding，避免把完整绑定降级为 legacy local/);
  assert.doesNotMatch(prompt, /automation_update/);
  assert.match(prompt, /Broadcast 主机侧会暂停当前自动化/);
});

test("the remote automation prompt keeps broadcast local and delegates work to the SSH project", () => {
  const prompt = buildBroadcastAutomationPrompt(remoteRequest);
  assert.match(prompt, /仅在本机作为任务面板控制器运行/);
  assert.match(prompt, /remote-ssh-discovered:merlin-agent/);
  assert.match(prompt, /\/mlx_devbox\/users\/example\/playground/);
  assert.match(prompt, /remote-worktree-456/);
  assert.match(prompt, /\/mlx_devbox\/users\/example\/playground-worktree/);
  assert.match(prompt, /Codex create_thread/);
  assert.match(prompt, /projectId:actualTarget\.codexProjectId/);
  assert.match(prompt, /同一保存主机当前可用的精确远程项目映射/);
  assert.match(prompt, /developmentContext\.type 是 worktree[\s\S]*workspacePath 与 developmentContext\.path 完全相同/);
  assert.match(prompt, /零项或多项[\s\S]*目标 SSH worktree 未映射[\s\S]*不认领、不 create、不写基础项目 binding/);
  assert.match(prompt, /不得回退到基础 root、local、项目名、其他主机/);
  assert.match(prompt, /Codex wait_threads/);
  assert.match(prompt, /远程会话不运行 broadcast/);
  assert.match(prompt, /完整 threadBinding 包含 threadId、codexProjectId、codexProjectKind、codexHostId、workspacePath/);
  assert.match(prompt, /当前自动化的项目和主机只能作为未绑定议题的首次目标/);
  assert.match(prompt, /存在 threadId 但没有完整 threadBinding[\s\S]*legacy local[\s\S]*--if-version[\s\S]*不得 send、create 或覆盖该绑定/);
  assert.match(prompt, /所有认领、评论和状态写入只由当前本地控制器完成/);
  assert.match(prompt, /已有完整 threadBinding 时，只能使用其保存的 threadId 和 codexHostId 调用 Codex send_message_to_thread/);
  assert.match(prompt, /send 成功后必须重新 issue get 一次[\s\S]*status 仍为 todo[\s\S]*threadBinding 与保存值完全相同[\s\S]*issue move --status in_progress[\s\S]*记录响应 task\.version 为 ownedVersion/);
  assert.match(prompt, /认领成功后继续执行后文现有 Codex wait_threads、结果评论和 in_review 写回路径，不得结束本轮/);
  assert.doesNotMatch(prompt, /要求原远程会话按本协议判断和认领/);
  assert.match(prompt, /未绑定时必须传 --clear-binding-thread/);
  assert.match(prompt, /记录响应 task 的 version 为 ownedVersion[\s\S]*每次 issue move 都必须显式传 --if-version ownedVersion/);
  assert.match(prompt, /create_thread 失败[\s\S]*ownedVersion[\s\S]*--if-version[\s\S]*--clear-binding-thread[\s\S]*移回 todo/);
  assert.match(prompt, /发生 409[\s\S]*立即停止且不得重读最新 version 后覆盖/);
  assert.match(prompt, /响应丢失或结果不确定[\s\S]*projectId 等于 ownedProjectId[\s\S]*状态仍为本轮 in_progress[\s\S]*threadBinding 为空或与本轮五字段 binding 完全相同/);
  assert.match(prompt, /读到相同 binding 视为前次保存成功[\s\S]*读到不同 binding[\s\S]*立即退出/);
  assert.match(prompt, /确定绑定写入失败[\s\S]*远程 threadId[\s\S]*移动到 blocked/);
  assert.match(prompt, /wait_threads 失败[\s\S]*完整保存 binding[\s\S]*移动到 blocked/);
  assert.match(prompt, /worker 确认后的每一次 issue move 都必须显式传完整远程 binding/);
  assert.match(prompt, /不得扫描或接管其他 in_progress/);
  assert.match(prompt, /移动到 in_review/);
});

test("the generated automation command uses the packaged CLI and an argv runtime file", () => {
  const previous = process.env.BROADCAST_RUNTIME_FILE;
  process.env.BROADCAST_RUNTIME_FILE = "/Users/example/Library/Application Support/Broadcast/launcher-runtime.json";
  try {
    const prompt = buildBroadcastAutomationPrompt(baseRequest);
    const cliPath = fileURLToPath(new URL("../cli/broadcast.mjs", import.meta.url));
    assert.ok(prompt.includes(
      `'${process.execPath}' '${cliPath}' --runtime-file '${process.env.BROADCAST_RUNTIME_FILE}'`,
    ));
    assert.ok(!prompt.includes(path.resolve(path.dirname(baseRequest.skillPath), "../..", "cli/broadcast.mjs")));
    assert.doesNotMatch(prompt, /BROADCAST_RUNTIME_FILE=/);
  } finally {
    if (previous === undefined) {
      delete process.env.BROADCAST_RUNTIME_FILE;
    } else {
      process.env.BROADCAST_RUNTIME_FILE = previous;
    }
  }
});

test("the generated cron spec uses the selected whitelisted local Codex options", () => {
  assert.deepEqual(buildBroadcastAutomationSpec(baseRequest), {
    kind: "cron",
    name: "Broadcast 自动认领 · ppt-skill",
    prompt: buildBroadcastAutomationPrompt(baseRequest),
    projectId: "codex-project-123",
    executionEnvironment: "local",
    localEnvironmentConfigPath: null,
    model: "gpt-5.5",
    reasoningEffort: "high",
    rrule: "RRULE:FREQ=MINUTELY;INTERVAL=5",
  });
  assert.deepEqual(buildBroadcastAutomationSpec({
    ...baseRequest,
    intervalMinutes: 30,
    model: "gpt-5.4",
    reasoningEffort: "medium",
  }), {
    ...buildBroadcastAutomationSpec(baseRequest),
    prompt: buildBroadcastAutomationPrompt({ ...baseRequest, intervalMinutes: 30 }),
    model: "gpt-5.4",
    reasoningEffort: "medium",
    rrule: "RRULE:FREQ=MINUTELY;INTERVAL=30",
  });
  assert.deepEqual(buildBroadcastAutomationSpec(remoteRequest), {
    kind: "cron",
    name: "Broadcast 自动认领 · ppt-skill",
    prompt: buildBroadcastAutomationPrompt(remoteRequest),
    projectId: null,
    executionEnvironment: "local",
    localEnvironmentConfigPath: null,
    model: "gpt-5.5",
    reasoningEffort: "high",
    rrule: "RRULE:FREQ=MINUTELY;INTERVAL=5",
  });
});

test("passive policy checks resume only after quota recovery", () => {
  const passiveAvailable = {
    explicit: false,
    previousQuotaState: "available",
    quotaState: "available",
    currentStatus: "PAUSED",
  };
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: true },
      passiveAvailable,
    ),
    "list",
  );
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: true },
      { ...passiveAvailable, quotaState: "unknown" },
    ),
    "list",
  );
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: true },
      { ...passiveAvailable, previousQuotaState: "blocked" },
    ),
    "ensure-active",
  );
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: true },
      { ...passiveAvailable, explicit: true },
    ),
    "ensure-active",
  );
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: false },
      { ...passiveAvailable, currentStatus: "ACTIVE" },
    ),
    "ensure-active",
  );
  assert.equal(
    broadcastAutomationPolicyOperation(
      { ...baseRequest, quotaAware: false },
      { ...passiveAvailable, currentStatus: "ACTIVE", hasTodo: false },
    ),
    "pause",
  );
});

test("ensure-active updates a matching automation by id with a complete active spec", async () => {
  const existing = {
    id: "automation-1",
    status: "ACTIVE",
    kind: "cron",
    name: "Broadcast 自动认领 · ppt-skill",
    prompt: "old prompt",
    projectId: "old-project",
    executionEnvironment: "local",
    localEnvironmentConfigPath: null,
    model: "gpt-5.5",
    reasoningEffort: "medium",
    rrule: "FREQ=HOURLY",
    createdAt: "2026-07-25T00:00:00.000Z",
    internalRevision: 4,
  };
  const calls = [];
  const response = await reconcileBroadcastAutomation(
    { ...baseRequest, automationId: "automation-1" },
    async (method, params) => {
      calls.push({ method, params });
      if (method === "list-automations") return { items: [existing] };
      return { item: params };
    },
  );

  const spec = buildBroadcastAutomationSpec(baseRequest);
  assert.deepEqual(calls, [
    { method: "list-automations", params: {} },
    {
      method: "automation-update",
      params: {
        ...spec,
        id: "automation-1",
        status: "ACTIVE",
      },
    },
  ]);
  assert.deepEqual(response, {
    item: { ...spec, id: "automation-1", status: "ACTIVE" },
  });
});

test("ensure-active is idempotent when the listed automation already matches", async () => {
  const existing = {
    id: "automation-1",
    status: "ACTIVE",
    ...buildBroadcastAutomationSpec(baseRequest),
    createdAt: "2026-07-25T00:00:00.000Z",
  };
  const calls = [];
  const response = await reconcileBroadcastAutomation(
    { ...baseRequest, automationId: "automation-1" },
    async (method, params) => {
      calls.push({ method, params });
      return { items: [existing] };
    },
  );

  assert.deepEqual(calls, [{ method: "list-automations", params: {} }]);
  assert.deepEqual(response, { item: existing });
});

test("a foreign automation id never grants control outside the project", async () => {
  const foreign = {
    id: "foreign-automation",
    status: "ACTIVE",
    ...buildBroadcastAutomationSpec({
      ...baseRequest,
      broadcastProjectId: "another-project",
    }),
  };
  const ensureCalls = [];
  await reconcileBroadcastAutomation(
    { ...baseRequest, automationId: foreign.id },
    async (method, params) => {
      ensureCalls.push({ method, params });
      if (method === "list-automations") return { items: [foreign] };
      return { item: params };
    },
  );
  assert.deepEqual(ensureCalls, [
    { method: "list-automations", params: {} },
    { method: "automation-create", params: buildBroadcastAutomationSpec(baseRequest) },
  ]);

  const pauseCalls = [];
  const paused = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "pause", automationId: foreign.id },
    async (method, params) => {
      pauseCalls.push({ method, params });
      return { items: [foreign] };
    },
  );
  assert.deepEqual(pauseCalls, [{ method: "list-automations", params: {} }]);
  assert.deepEqual(paused, { error: "not-found" });
});

test("ensure-active falls back to the stable name and otherwise creates", async () => {
  const matching = {
    id: "automation-by-name",
    status: "PAUSED",
    ...buildBroadcastAutomationSpec(baseRequest),
  };
  const updateCalls = [];
  await reconcileBroadcastAutomation(baseRequest, async (method, params) => {
    updateCalls.push({ method, params });
    if (method === "list-automations") return { items: [matching] };
    return { item: params };
  });
  assert.equal(updateCalls[1].method, "automation-update");
  assert.equal(updateCalls[1].params.id, "automation-by-name");

  const createCalls = [];
  const created = await reconcileBroadcastAutomation(baseRequest, async (method, params) => {
    createCalls.push({ method, params });
    if (method === "list-automations") return { items: [] };
    return { item: { id: "created-1", status: "ACTIVE", ...params } };
  });
  assert.deepEqual(createCalls, [
    { method: "list-automations", params: {} },
    { method: "automation-create", params: buildBroadcastAutomationSpec(baseRequest) },
  ]);
  assert.equal(created.item.id, "created-1");
});

test("pause never creates and list returns only sanitized matching project automations", async () => {
  const matching = {
    id: "matching",
    status: "ACTIVE",
    ...buildBroadcastAutomationSpec(baseRequest),
    untrustedListField: "must not be echoed into an update",
  };
  const unrelated = {
    id: "unrelated",
    status: "ACTIVE",
    ...buildBroadcastAutomationSpec({
      ...baseRequest,
      broadcastProjectId: "another-project",
    }),
  };

  const pauseCalls = [];
  const paused = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "pause" },
    async (method, params) => {
      pauseCalls.push({ method, params });
      if (method === "list-automations") return { items: [unrelated, matching] };
      return { item: params };
    },
  );
  assert.deepEqual(pauseCalls, [
    { method: "list-automations", params: {} },
    {
      method: "automation-update",
      params: {
        ...buildBroadcastAutomationSpec(baseRequest),
        id: "matching",
        status: "PAUSED",
      },
    },
  ]);
  assert.deepEqual(paused, {
    item: {
      ...buildBroadcastAutomationSpec(baseRequest),
      id: "matching",
      status: "PAUSED",
    },
  });

  const notFoundCalls = [];
  const notFound = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "pause", broadcastProjectId: "missing" },
    async (method, params) => {
      notFoundCalls.push({ method, params });
      return { items: [matching, unrelated] };
    },
  );
  assert.deepEqual(notFoundCalls, [{ method: "list-automations", params: {} }]);
  assert.deepEqual(notFound, { error: "not-found" });

  const listed = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "list" },
    async () => ({ items: [unrelated, matching] }),
  );
  assert.deepEqual(listed, {
    items: [{
      id: "matching",
      status: "ACTIVE",
      model: "gpt-5.5",
      reasoningEffort: "high",
      rrule: "RRULE:FREQ=MINUTELY;INTERVAL=5",
    }],
  });

  const catalogPair = {
    ...matching,
    id: "catalog-pair",
    model: "gemini-3.1-pro-preview",
    reasoningEffort: "xhigh",
  };
  const catalogListed = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "list" },
    async () => ({ items: [catalogPair] }),
  );
  assert.deepEqual(catalogListed, {
    items: [{
      id: "catalog-pair",
      status: "ACTIVE",
      model: "gemini-3.1-pro-preview",
      reasoningEffort: "xhigh",
      rrule: "RRULE:FREQ=MINUTELY;INTERVAL=5",
    }],
  });
});

test("pause is idempotent for an already paused matching automation", async () => {
  const matching = {
    id: "matching",
    status: "PAUSED",
    ...buildBroadcastAutomationSpec(baseRequest),
  };
  const calls = [];
  const response = await reconcileBroadcastAutomation(
    { ...baseRequest, operation: "pause" },
    async (method, params) => {
      calls.push({ method, params });
      return { items: [matching] };
    },
  );
  assert.deepEqual(calls, [{ method: "list-automations", params: {} }]);
  assert.deepEqual(response, { item: matching });
});
