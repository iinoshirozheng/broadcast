import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import { composerReferencePersistence } from "../apps/web/src/composerReference.mjs";
import { startNativeServer, writeNodeExecutable } from "./helpers/native-server.mjs";

test("composer rebind HTTP API returns send-ready documents and boundary errors", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "broadcast-composer-rebind-api-"));
  // Codex reports skill paths in the host's absolute form (a drive letter on Windows).
  const skillPath = path.resolve("/server-only/real-skill/SKILL.md");
  const executable = await writeNodeExecutable(
    path.join(directory, "fake-codex"),
    `const args = process.argv.slice(2);
if (args[0] !== "app-server") process.exit(2);
process.stdin.setEncoding("utf8");
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\\n")) >= 0) {
    const line = buffer.slice(0, index); buffer = buffer.slice(index + 1);
    if (!line.trim()) continue;
    const message = JSON.parse(line);
    if (message.method === "initialize" && message.id !== undefined) {
      process.stdout.write(JSON.stringify({id:message.id,result:{platformFamily:"unix"}}) + "\\n");
    }
    if (message.method === "skills/list" && message.id !== undefined) {
      process.stdout.write(JSON.stringify({id:message.id,result:{data:[{skills:[{
        name:"real-skill",enabled:true,path:${JSON.stringify(skillPath)},
        description:"Real skill",interface:{displayName:"Real Skill"}
      }]}]}}) + "\\n");
    }
    if (message.method === "shutdown" && message.id !== undefined) {
      process.stdout.write(JSON.stringify({id:message.id,result:null}) + "\\n");
    }
  }
});
`,
  );
  const dataDirectory = path.join(directory, "data");
  const options = {
    codexExecutable: executable,
    codexStatePath: path.join(directory, "missing-codex-state.json"),
  };
  const { app, port } = await startNativeServer(dataDirectory, options, { host: "127.0.0.1" });
  try {
    const baseUrl = `http://127.0.0.1:${port}`;
    const persistence = composerReferencePersistence("skill", "real-skill", "Real Skill");
    const slashCandidatesResponse = await fetch(
      `${baseUrl}/api/local/ai/composer/candidates?projectId=local&trigger=%2F&query=&surface=issue-description`,
    );
    assert.equal(slashCandidatesResponse.status, 200);
    const slashCandidates = await slashCandidatesResponse.json();
    assert.ok(slashCandidates.candidates.length > 4);
    const newCandidate = slashCandidates.candidates.find(
      (candidate) => candidate.command === "/new",
    );
    assert.deepEqual(newCandidate.selection, { type: "insertText", text: "/new " });
    assert.ok(slashCandidates.candidates.every((candidate) => !("dispatch" in candidate)));

    const aiChatSlashResponse = await fetch(
      `${baseUrl}/api/local/ai/composer/candidates?projectId=local&trigger=%2F&query=&surface=ai-chat`,
    );
    assert.equal(aiChatSlashResponse.status, 200);
    const aiChatSlash = await aiChatSlashResponse.json();
    const verifiedHandlers = new Set([
      "new-conversation",
      "open-model-menu",
      "open-reasoning-menu",
      "compact-conversation",
    ]);
    assert.ok(aiChatSlash.candidates.length > 0);
    assert.ok(
      aiChatSlash.candidates.every((candidate) =>
        verifiedHandlers.has(candidate.dispatch?.handlerId),
      ),
    );

    const skillCandidatesResponse = await fetch(
      `${baseUrl}/api/local/ai/composer/candidates?projectId=local&trigger=%40&query=real&surface=comment`,
    );
    assert.equal(skillCandidatesResponse.status, 200);
    const skillCandidates = await skillCandidatesResponse.json();
    assert.equal(skillCandidates.candidates[0].persistence.referenceKey, persistence.referenceKey);
    assert.equal(JSON.stringify(skillCandidates).includes("server-only"), false);

    const response = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        document: {
          version: 1,
          nodes: [
            { type: "text", text: "Use " },
            {
              type: "persistedReference",
              referenceKind: "skill",
              referenceKey: persistence.referenceKey,
              label: "Saved Label",
            },
          ],
        },
      }),
    });
    assert.equal(response.status, 200);
    const rebound = await response.json();
    assert.equal(rebound.ready, true);
    assert.deepEqual(
      rebound.document.nodes.map((node) => node.type),
      ["text", "skill"],
    );
    assert.equal(rebound.bindings[0].label, "Real Skill");
    assert.equal(JSON.stringify(rebound).includes("server-only"), false);
    assert.equal(
      app.database.database.prepare("SELECT COUNT(*) AS count FROM ai_chat_events").get().count,
      0,
    );

    const unsupportedResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        document: {
          version: 1,
          nodes: [
            {
              type: "unsupportedReference",
              referenceUri: "broadcast://composer-reference/v2/plugin/cGx1Z2lu",
              label: "Future format",
            },
            {
              type: "unsupportedReference",
              referenceUri: "broadcast://composer-reference/v1/plugin/cGx1Z2lu",
              label: "Future kind",
            },
          ],
        },
      }),
    });
    assert.equal(unsupportedResponse.status, 200);
    const unsupported = await unsupportedResponse.json();
    assert.equal(unsupported.ready, false);
    assert.deepEqual(unsupported.bindings, [
      {
        nodeIndex: 0,
        status: "unavailable",
        referenceKind: "unsupported",
        reasonCode: "REFERENCE_FORMAT_UNSUPPORTED",
      },
      {
        nodeIndex: 1,
        status: "unavailable",
        referenceKind: "unsupported",
        reasonCode: "REFERENCE_KIND_UNSUPPORTED",
      },
    ]);

    const invalidResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        document: {
          version: 1,
          nodes: [
            {
              type: "persistedReference",
              referenceKind: "skill",
              referenceKey: "cmVhbC1za2lsbA==",
              label: "Real Skill",
            },
          ],
        },
      }),
    });
    assert.equal(invalidResponse.status, 400);
    assert.equal((await invalidResponse.json()).error.code, "INVALID_COMPOSER_REBIND_REQUEST");

    const missingProjectResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "missing",
        document: { version: 1, nodes: [{ type: "text", text: "No mutation" }] },
      }),
    });
    assert.equal(missingProjectResponse.status, 400);
    assert.equal((await missingProjectResponse.json()).error.code, "INVALID_COMPOSER_QUERY");

    const authoritativeThread = app.database.createAiChatThread({
      id: "authoritative-composer-thread",
      title: "Authoritative workspace",
      origin: {
        projectId: "local",
        projectName: "Local",
        workspacePath: directory,
      },
      model: "gpt-real",
      reasoningEffort: "medium",
      sandbox: "workspace-write",
    });
    const authoritativeThreadResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        threadId: authoritativeThread.id,
        document: {
          version: 1,
          nodes: [
            {
              type: "persistedReference",
              referenceKind: "skill",
              referenceKey: persistence.referenceKey,
              label: "Real Skill",
            },
          ],
        },
      }),
    });
    assert.equal(authoritativeThreadResponse.status, 200);
    assert.equal((await authoritativeThreadResponse.json()).ready, true);

    const crossProjectThread = app.database.createAiChatThread({
      id: "cross-project-composer-thread",
      title: "Cross project",
      origin: { projectId: "other", projectName: "Other", workspacePath: directory },
      model: "gpt-real",
      reasoningEffort: "medium",
      sandbox: "workspace-write",
    });
    const crossProjectThreadResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        threadId: crossProjectThread.id,
        document: { version: 1, nodes: [{ type: "text", text: "No mutation" }] },
      }),
    });
    assert.equal(crossProjectThreadResponse.status, 400);
    assert.equal((await crossProjectThreadResponse.json()).error.code, "INVALID_COMPOSER_QUERY");

    const unavailableThread = app.database.createAiChatThread({
      id: "unavailable-workspace-composer-thread",
      title: "Unavailable workspace",
      origin: {
        projectId: "local",
        projectName: "Local",
        workspacePath: path.join(directory, "missing-workspace"),
      },
      model: "gpt-real",
      reasoningEffort: "medium",
      sandbox: "workspace-write",
    });
    const unavailableThreadResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "local",
        threadId: unavailableThread.id,
        document: { version: 1, nodes: [{ type: "text", text: "No mutation" }] },
      }),
    });
    assert.equal(unavailableThreadResponse.status, 409);
    assert.equal(
      (await unavailableThreadResponse.json()).error.code,
      "PROJECT_WORKSPACE_UNAVAILABLE",
    );

    const createProjectResponse = await fetch(`${baseUrl}/api/projects`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id: "unmapped", name: "Unmapped" }),
    });
    assert.equal(createProjectResponse.status, 201);
    const unavailableWorkspaceResponse = await fetch(`${baseUrl}/api/local/ai/composer/rebind`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        contractVersion: "composer.v1",
        projectId: "unmapped",
        document: { version: 1, nodes: [{ type: "text", text: "No mutation" }] },
      }),
    });
    assert.equal(unavailableWorkspaceResponse.status, 409);
    assert.equal(
      (await unavailableWorkspaceResponse.json()).error.code,
      "PROJECT_WORKSPACE_UNAVAILABLE",
    );
  } finally {
    await app.close();
    await rm(directory, { recursive: true, force: true });
  }
});
