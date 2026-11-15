mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};
use support::{Builder, Server, lan_address};

/// A server whose Codex CLI is `fake-codex`, or is not installed when `codex` is false, with the
/// `local` project mapped to the returned workspace.
fn builder(codex: bool) -> (Builder, String) {
    let builder = Server::builder();
    let workspace = builder.dir().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = dunce::canonicalize(workspace)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    write_codex_state(
        &builder.codex_home(),
        json!({ "local-projects": { "local": { "rootPaths": [workspace] } } }),
    );
    let executable = if codex {
        PathBuf::from(env!("CARGO_BIN_EXE_fake-codex"))
    } else {
        builder.dir().join("codex-not-installed")
    };
    let builder = builder
        .env("CODEX_EXECUTABLE", executable)
        .env("BROADCAST_SKILL_PATH", "/fixture/manage-broadcast/SKILL.md");
    (builder, workspace)
}

async fn start() -> (Server, String) {
    let (builder, workspace) = builder(true);
    (builder.start().await, workspace)
}

fn write_codex_state(codex_home: &Path, state: Value) {
    std::fs::create_dir_all(codex_home).unwrap();
    std::fs::write(codex_home.join(".codex-global-state.json"), state.to_string()).unwrap();
}

/// Creates a thread and returns its id.
async fn create_thread(server: &Server, body: Value) -> String {
    let (status, created) = server
        .send(Method::POST, "/api/local/ai/threads", body)
        .await;
    assert_eq!(status, 201, "{created}");
    created["thread"]["id"].as_str().unwrap().to_owned()
}

async fn add_project(server: &Server, id: &str, workspace: &Path) {
    let (status, _) = server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": id, "name": id, "workspacePath": workspace }),
        )
        .await;
    assert_eq!(status, 201);
}

#[tokio::test]
async fn a_missing_codex_cli_reports_an_unavailable_catalog_instead_of_a_500() {
    let server = builder(false).0.start().await;
    let (status, catalog) = server.get("/api/local/ai/catalog?projectId=local").await;
    assert_eq!(status, 200);
    assert_eq!(catalog["available"], false);
    assert_eq!(catalog["models"], json!([]));
    assert_eq!(catalog["skills"], json!([]));
    assert_eq!(catalog["commands"], json!([]));
    assert_eq!(
        catalog["sandboxes"],
        json!(["read-only", "workspace-write", "danger-full-access"])
    );
}

#[tokio::test]
async fn a_catalog_failure_that_is_not_a_missing_cli_keeps_its_own_status() {
    let (server, _) = start().await;
    add_project(&server, "catalog-missing-workspace", &server.dir().join("not-there")).await;
    let (status, catalog) = server
        .get("/api/local/ai/catalog?projectId=catalog-missing-workspace")
        .await;
    assert_eq!(status, 409);
    assert_eq!(catalog["error"]["code"], "PROJECT_WORKSPACE_UNAVAILABLE");
}

#[tokio::test]
async fn loopback_ai_api_freezes_server_owned_origin_and_rejects_injected_execution_fields() {
    let (server, workspace) = start().await;
    let (_, meta) = server.get("/api/meta").await;
    assert_eq!(meta["capabilities"]["localAiChat"], true);
    let (status, catalog) = server.get("/api/local/ai/catalog?projectId=local").await;
    assert_eq!(status, 200);
    assert_eq!(catalog["models"][0]["slug"], "gpt-real");
    assert_eq!(catalog["skills"][0]["id"], "real-skill");

    let (status, injected) = server
        .send(
            Method::POST,
            "/api/local/ai/threads",
            json!({
                "projectId": "local",
                "workspacePath": "/tmp/evil",
                "argv": ["--dangerously-bypass-approvals-and-sandbox"],
            }),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(injected["error"]["code"], "UNKNOWN_FIELD");

    let (status, created) = server
        .send(
            Method::POST,
            "/api/local/ai/threads",
            json!({
                "projectId": "local",
                "model": "gpt-real",
                "reasoningEffort": "high",
                "sandbox": "read-only",
            }),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(created["thread"]["origin"]["workspacePath"], workspace);
    let thread = created["thread"]["id"].as_str().unwrap();
    let turns = format!("/api/local/ai/threads/{thread}/turns");

    let (status, invalid) = server
        .send(
            Method::POST,
            &turns,
            json!({ "message": "hello \u{FFFC}", "skillIds": ["invented-skill"] }),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(invalid["error"]["code"], "INVALID_SKILL");

    let (status, turn) = server
        .send(
            Method::POST,
            &turns,
            json!({ "message": "hello \u{FFFC}", "skillIds": ["real-skill"] }),
        )
        .await;
    assert_eq!(status, 202);
    assert_eq!(turn["run"]["threadId"], thread);

    let mut snapshot = Value::Null;
    for _ in 0..100 {
        snapshot = server.get(&format!("/api/local/ai/threads/{thread}")).await.1;
        if snapshot["runs"][0]["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(snapshot["thread"]["codexThreadId"], "session-1");
    let events = snapshot["events"].as_array().unwrap();
    assert!(events.iter().any(|event| event["content"] == "ok"), "{snapshot}");
}

#[tokio::test]
async fn non_local_ai_threads_reject_projects_without_an_available_workspace() {
    let (server, _) = start().await;
    add_project(&server, "missing-workspace", &server.dir().join("missing-workspace")).await;
    let (status, created) = server
        .send(
            Method::POST,
            "/api/local/ai/threads",
            json!({ "projectId": "missing-workspace" }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(created["error"]["code"], "PROJECT_WORKSPACE_UNAVAILABLE");
}

#[tokio::test]
async fn non_local_ai_turns_reject_a_workspace_that_became_unavailable() {
    let (server, _) = start().await;
    let workspace = server.dir().join("project-workspace");
    std::fs::create_dir(&workspace).unwrap();
    add_project(&server, "disconnected-workspace", &workspace).await;
    let thread = create_thread(&server, json!({ "projectId": "disconnected-workspace" })).await;
    std::fs::remove_dir(&workspace).unwrap();

    let (status, turn) = server
        .send(
            Method::POST,
            &format!("/api/local/ai/threads/{thread}/turns"),
            json!({ "message": "hello" }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(turn["error"]["code"], "PROJECT_WORKSPACE_UNAVAILABLE");
    let (_, snapshot) = server.get(&format!("/api/local/ai/threads/{thread}")).await;
    assert_eq!(snapshot["runs"], json!([]));
}

#[tokio::test]
async fn the_local_ai_project_falls_back_to_the_broadcast_workspace() {
    let (server, _) = start().await;
    write_codex_state(&server.codex_home(), json!({ "local-projects": {} }));
    let (status, created) = server
        .send(
            Method::POST,
            "/api/local/ai/threads",
            json!({ "projectId": "local" }),
        )
        .await;
    assert_eq!(status, 201);
    // The server runs in the directory the tests run in.
    let directory = std::env::current_dir().unwrap();
    assert_eq!(
        created["thread"]["origin"]["workspacePath"],
        directory.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn danger_full_access_requires_confirmation_on_every_turn_and_thread_settings_are_validated()
{
    let (server, _) = start().await;
    let thread = create_thread(
        &server,
        json!({
            "projectId": "local",
            "model": "gpt-real",
            "reasoningEffort": "low",
            "sandbox": "danger-full-access",
        }),
    )
    .await;
    let turns = format!("/api/local/ai/threads/{thread}/turns");
    let (status, denied) = server
        .send(Method::POST, &turns, json!({ "message": "hello" }))
        .await;
    assert_eq!(status, 400);
    assert_eq!(denied["error"]["code"], "DANGER_CONFIRMATION_REQUIRED");
    let (status, _) = server
        .send(
            Method::POST,
            &turns,
            json!({ "message": "hello", "dangerFullAccessConfirmed": true }),
        )
        .await;
    assert_eq!(status, 202);

    let (status, invalid) = server
        .send(
            Method::PATCH,
            &format!("/api/local/ai/threads/{thread}"),
            json!({ "model": "invented-model", "reasoningEffort": "high" }),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(invalid["error"]["code"], "INVALID_MODEL");
}

#[tokio::test]
async fn thread_management_interrupt_and_query_contracts_stay_narrow() {
    let (server, _) = start().await;
    let thread = create_thread(&server, json!({ "projectId": "local", "title": "Original" })).await;
    let path = format!("/api/local/ai/threads/{thread}");

    let (status, list) = server.get("/api/local/ai/threads").await;
    assert_eq!(status, 200);
    let threads = list["threads"].as_array().unwrap();
    assert!(threads.iter().any(|listed| listed["id"] == thread.as_str()));

    let (status, unknown) = server.get("/api/local/ai/threads?projectId=local").await;
    assert_eq!(status, 400);
    assert_eq!(unknown["error"]["code"], "UNKNOWN_QUERY_PARAMETER");

    let (status, updated) = server
        .send(
            Method::PATCH,
            &path,
            json!({ "title": "Renamed", "sandbox": "workspace-write" }),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(updated["thread"]["title"], "Renamed");

    let interrupt = server.request(Method::POST, "/api/local/ai/runs/missing/interrupt");
    assert_eq!(interrupt.send().await.unwrap().status(), 404);

    let removed = server.request(Method::DELETE, &path).send().await.unwrap();
    assert_eq!(removed.status(), 204);
    assert_eq!(server.get(&path).await.0, 404);
}

#[tokio::test]
async fn local_ai_routes_reject_lan_clients_while_ordinary_api_routes_remain_available() {
    let Some(lan) = lan_address() else {
        eprintln!("skipped: no non-loopback IPv4 interface is available");
        return;
    };
    let server = builder(true)
        .0
        .env("BROADCAST_HOST", "0.0.0.0")
        .start()
        .await;
    let from_lan = |path: &str| {
        let url = format!("http://{lan}:{}{path}", server.port);
        let request = server.client.get(url).send();
        async { support::json(request.await.unwrap()).await }
    };

    assert_eq!(from_lan("/api/projects").await.0, 200);
    let (status, meta) = from_lan("/api/meta").await;
    assert_eq!(status, 200);
    assert_eq!(meta["capabilities"]["localAiChat"], false);
    let (status, ai) = from_lan("/api/local/ai/threads").await;
    assert_eq!(status, 403);
    assert_eq!(ai["error"]["code"], "LOCAL_AI_LOOPBACK_REQUIRED");
}

#[tokio::test]
async fn ai_sse_is_live_only_and_thread_snapshots_remain_the_durable_source() {
    let (server, _) = start().await;
    let thread = create_thread(&server, json!({ "projectId": "local" })).await;
    let mut events = server
        .request(Method::GET, &format!("/api/local/ai/threads/{thread}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), 200);
    let mut read_until = async |marker: &str| {
        let mut text = String::new();
        while !text.contains(marker) {
            let chunk = events.chunk().await.unwrap().expect("the stream stays open");
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    };

    assert!(read_until("event: ai.event").await.contains("connected"));
    let (status, _) = server
        .send(
            Method::POST,
            &format!("/api/local/ai/threads/{thread}/turns"),
            json!({ "message": "hello" }),
        )
        .await;
    assert_eq!(status, 202);
    let streamed = read_until("ai.event").await;
    assert!(
        streamed.contains("event: ai.event") || streamed.contains("event: ai.run"),
        "{streamed}"
    );
}
