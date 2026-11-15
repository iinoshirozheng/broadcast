import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { WebSocket, WebSocketServer } from "ws";

import { startNativeServer } from "./helpers/native-server.mjs";

const temporaryDirectories = [];

test.afterEach(async () => {
  while (temporaryDirectories.length > 0) {
    await rm(temporaryDirectories.pop(), { recursive: true, force: true });
  }
});

function jsonResponse(payload, status = 200) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function memoryConfigStore(overrides = {}) {
  const state = {
    version: 1,
    remoteUrl: "https://tasks.example.test",
    actorName: "Alice",
    sharedKey: "two-person-shared-key",
    projectMappings: {},
    ...overrides,
  };
  return {
    state,
    async read() {
      return structuredClone(state);
    },
    async setProjectWorkspace(projectId, workspacePath) {
      state.projectMappings[projectId] = workspacePath;
      return structuredClone(state);
    },
  };
}

function firstLanAddress() {
  return Object.values(os.networkInterfaces())
    .flat()
    .find((entry) => entry?.family === "IPv4" && !entry.internal)?.address ?? null;
}

test("cloud companion blocks project moves for issue-linked local AI chats", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "broadcast-cloud-chat-move-"));
  temporaryDirectories.push(directory);
  let upstreamCalls = 0;
  const { app, port } = await startNativeServer(directory, {
    cloudConfigStore: memoryConfigStore(),
    remoteFetch: async () => {
      upstreamCalls += 1;
      return jsonResponse({ task: { id: "REMOTE-1", projectId: "target" } });
    },
  });

  try {
    const thread = app.database.createAiChatThread({
      id: "cloud-linked-thread",
      title: "Cloud issue conversation",
      origin: {
        projectId: "source",
        projectName: "Source",
        workspacePath: "/work/source",
        issueId: "REMOTE-1",
        issueIdentifier: "REMOTE-1",
      },
      model: "gpt-real",
      reasoningEffort: "medium",
      sandbox: "workspace-write",
    });

    const response = await fetch(
      `http://127.0.0.1:${port}/api/tasks/REMOTE-1`,
      {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ version: 1, projectId: "target" }),
      },
    );
    const payload = await response.json();

    assert.equal(response.status, 409);
    assert.equal(payload.error.code, "AI_CHAT_PROJECT_MOVE_BLOCKED");
    assert.equal(upstreamCalls, 0);
    const row = app.database.database
      .prepare("SELECT origin_project_id, origin_issue_id FROM ai_chat_threads WHERE id = ?")
      .get(thread.id);
    assert.deepEqual({ ...row }, { origin_project_id: "source", origin_issue_id: "REMOTE-1" });
  } finally {
    await app.close();
  }
});

test("local companion relays authenticated cloud revision WebSockets", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "broadcast-cloud-websocket-"));
  temporaryDirectories.push(directory);
  const upstreamServer = createServer();
  const upstreamWebSockets = new WebSocketServer({ noServer: true });
  let receivedAuthorization = null;
  upstreamServer.on("upgrade", (request, socket, head) => {
    receivedAuthorization = request.headers.authorization ?? null;
    upstreamWebSockets.handleUpgrade(request, socket, head, (webSocket) => {
      webSocket.send(JSON.stringify({ type: "revision", revision: 42 }));
    });
  });
  await new Promise((resolve) => upstreamServer.listen(0, "127.0.0.1", resolve));
  const upstreamAddress = upstreamServer.address();
  const { app, port } = await startNativeServer(directory, {
    cloudConfigStore: memoryConfigStore({
      remoteUrl: `http://127.0.0.1:${upstreamAddress.port}`,
    }),
  });
  let client;

  try {
    client = new WebSocket(`ws://127.0.0.1:${port}/api/events`);
    const payload = await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error("Timed out waiting for relay")), 1_000);
      client.once("message", (data) => {
        clearTimeout(timeout);
        resolve(JSON.parse(data.toString()));
      });
      client.once("error", reject);
    });
    assert.deepEqual(payload, { type: "revision", revision: 42 });
    assert.equal(
      receivedAuthorization,
      `Basic ${Buffer.from("Alice:two-person-shared-key").toString("base64")}`,
    );
  } finally {
    client?.terminate();
    await app.close();
    upstreamWebSockets.close();
    await new Promise((resolve) => upstreamServer.close(resolve));
  }
});

test("configured server proxies business APIs without touching local rows and advertises push", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "broadcast-cloud-server-"));
  temporaryDirectories.push(directory);
  const upstreamCalls = [];
  const skillPath = path.join(directory, "SKILL.md");
  const { app, port } = await startNativeServer(directory, {
    skillPath,
    cloudConfigStore: memoryConfigStore(),
    remoteFetch: async (request) => {
      upstreamCalls.push({ url: request.url });
      return jsonResponse({ tasks: [{ id: "REMOTE-1", projectId: "portfolio" }] });
    },
  });
  const baseUrl = `http://127.0.0.1:${port}`;

  try {
    const metadata = await fetch(`${baseUrl}/api/meta`).then((response) => response.json());
    assert.deepEqual(metadata, {
      capabilities: { localAiChat: true },
      mode: "cloud",
      realtime: {
        transport: "websocket",
        endpoint: "/api/events",
      },
      localCapabilities: { available: true },
      manageBroadcastSkillPath: skillPath,
    });
    const session = await fetch(`${baseUrl}/api/local/cloud-session`)
      .then((response) => response.json());
    assert.equal(Object.hasOwn(session, "sharedKey"), false);

    const response = await fetch(`${baseUrl}/api/tasks`);
    assert.equal(response.status, 200);
    assert.equal(upstreamCalls.length, 1);
    assert.equal(upstreamCalls[0].url, "https://tasks.example.test/api/tasks");
    assert.equal(app.database.database.prepare("SELECT COUNT(*) AS count FROM tasks").get().count, 0);
  } finally {
    await app.close();
  }
});

test("cloud mode exposes machine capabilities only to loopback while local mode keeps LAN access", async (t) => {
  const lanAddress = firstLanAddress();
  if (!lanAddress) {
    t.skip("No non-loopback IPv4 interface is available");
    return;
  }
  const directory = await mkdtemp(path.join(os.tmpdir(), "broadcast-cloud-lan-"));
  temporaryDirectories.push(directory);
  let upstreamCalls = 0;
  const { app, port } = await startNativeServer(directory, {
    cloudConfigStore: memoryConfigStore(),
    remoteFetch: async () => {
      upstreamCalls += 1;
      return jsonResponse({ projects: [] });
    },
  }, { host: "0.0.0.0" });
  const lanBaseUrl = `http://${lanAddress}:${port}`;

  try {
    for (const pathname of [
      "/api/meta",
      "/api/device-workspaces",
      "/api/projects/portfolio/development-contexts",
    ]) {
      const response = await fetch(`${lanBaseUrl}${pathname}`);
      assert.equal(response.status, 403, pathname);
      assert.equal((await response.json()).error.code, "LOCAL_ONLY", pathname);
    }
    const projectResponse = await fetch(`${lanBaseUrl}/api/projects`);
    assert.equal(projectResponse.status, 403);
    assert.equal((await projectResponse.json()).error.code, "LOCAL_ONLY");
    const taskResponse = await fetch(`${lanBaseUrl}/api/tasks`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ projectId: "portfolio", title: "Must not proxy" }),
    });
    assert.equal(taskResponse.status, 403);
    assert.equal((await taskResponse.json()).error.code, "LOCAL_ONLY");
    assert.equal(upstreamCalls, 0);

    await writeFile(
      path.join(directory, "cloud-companion.json"),
      JSON.stringify({ version: 1, remoteUrl: null, actorName: null, sharedKey: null, projectMappings: {} }),
    );
    const localResponse = await fetch(`${lanBaseUrl}/api/device-workspaces`);
    assert.equal(localResponse.status, 200);
    const localProjects = await fetch(`${lanBaseUrl}/api/projects`);
    assert.equal(localProjects.status, 200);
  } finally {
    await app.close();
  }
});
