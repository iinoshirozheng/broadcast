mod support;

use axum::Router;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::Message;
use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Server, Upstream, lan_address, serve, write_cloud_config};

#[tokio::test]
async fn cloud_mode_blocks_project_moves_for_issue_linked_local_ai_chats() {
    let upstream = Upstream::json(json!({ "task": { "id": "REMOTE-1", "projectId": "target" } })).await;
    let server = Server::builder()
        .cloud(&upstream.url)
        .start()
        .await;
    let now = chrono::Utc::now().to_rfc3339();
    server
        .database()
        .execute(
            "INSERT INTO ai_chat_threads (
                id, title, status, origin_project_id, origin_project_name, origin_workspace_path,
                origin_issue_id, origin_issue_identifier, model, reasoning_effort, sandbox,
                created_at, updated_at
            ) VALUES ('cloud-linked-thread', 'Cloud issue conversation', 'idle', 'source',
                'Source', '/work/source', 'REMOTE-1', 'REMOTE-1', 'gpt-real', 'medium',
                'workspace-write', ?1, ?1)",
            [now],
        )
        .unwrap();

    let (status, body) = server
        .send(
            Method::PATCH,
            "/api/tasks/REMOTE-1",
            json!({ "version": 1, "projectId": "target" }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "AI_CHAT_PROJECT_MOVE_BLOCKED");
    assert!(upstream.calls().is_empty());
    let origin: (String, String) = server
        .database()
        .query_row(
            "SELECT origin_project_id, origin_issue_id FROM ai_chat_threads WHERE id = 'cloud-linked-thread'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(origin, ("source".into(), "REMOTE-1".into()));
}

#[tokio::test]
async fn local_companion_relays_authenticated_cloud_revision_websockets() {
    let authorization = Arc::new(Mutex::new(None));
    let received = Arc::clone(&authorization);
    let upstream = serve(Router::new().fallback(
        move |headers: HeaderMap, upgrade: WebSocketUpgrade| {
            *received.lock().unwrap() = headers
                .get("authorization")
                .map(|value| value.to_str().unwrap().to_owned());
            async move {
                upgrade.on_upgrade(|mut socket| async move {
                    let revision = json!({ "type": "revision", "revision": 42 }).to_string();
                    let _ = socket.send(Message::Text(revision.into())).await;
                    // Holds the socket open until the relay goes away.
                    while socket.recv().await.is_some() {}
                })
            }
        },
    ))
    .await;
    let server = Server::builder().cloud(&upstream).start().await;

    let (mut client, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/api/events", server.port))
            .await
            .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .expect("the relayed revision")
        .unwrap()
        .unwrap();
    let payload: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(payload, json!({ "type": "revision", "revision": 42 }));
    let expected = format!("Basic {}", STANDARD.encode("Alice:two-person-shared-key"));
    assert_eq!(authorization.lock().unwrap().as_deref(), Some(expected.as_str()));
}

#[tokio::test]
async fn cloud_mode_proxies_business_apis_without_touching_local_rows_and_advertises_push() {
    let upstream =
        Upstream::json(json!({ "tasks": [{ "id": "REMOTE-1", "projectId": "portfolio" }] })).await;
    let builder = Server::builder();
    let skill_path = builder.dir().join("SKILL.md");
    let server = builder
        .env("BROADCAST_SKILL_PATH", &skill_path)
        .cloud(&upstream.url)
        .start()
        .await;

    let (_, metadata) = server.get("/api/meta").await;
    assert_eq!(
        metadata,
        json!({
            "capabilities": { "localAiChat": true },
            "mode": "cloud",
            "realtime": { "transport": "websocket", "endpoint": "/api/events" },
            "localCapabilities": { "available": true },
            "manageBroadcastSkillPath": skill_path,
        })
    );
    let (_, session) = server.get("/api/local/cloud-session").await;
    assert!(session.get("sharedKey").is_none());

    let (status, _) = server.get("/api/tasks").await;
    assert_eq!(status, 200);
    assert_eq!(upstream.calls(), ["/api/tasks"]);
    let tasks: i64 = server
        .database()
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(tasks, 0);
}

#[tokio::test]
async fn cloud_mode_exposes_machine_capabilities_only_to_loopback_while_local_mode_keeps_lan_access()
{
    let Some(lan) = lan_address() else {
        eprintln!("skipped: no non-loopback IPv4 interface is available");
        return;
    };
    let upstream = Upstream::json(json!({ "projects": [] })).await;
    let server = Server::builder()
        .env("BROADCAST_HOST", "0.0.0.0")
        .cloud(&upstream.url)
        .start()
        .await;
    let lan_url = |path: &str| format!("http://{lan}:{}{path}", server.port);
    let local_only = |status: u16, body: &Value, path: &str| {
        assert_eq!(status, 403, "{path}");
        assert_eq!(body["error"]["code"], "LOCAL_ONLY", "{path}");
    };

    for path in [
        "/api/meta",
        "/api/device-workspaces",
        "/api/projects/portfolio/development-contexts",
        "/api/projects",
    ] {
        let response = server.client.get(lan_url(path)).send().await.unwrap();
        let (status, body) = support::json(response).await;
        local_only(status, &body, path);
    }
    let request = server.client.post(lan_url("/api/tasks"));
    let body = json!({ "projectId": "portfolio", "title": "Must not proxy" });
    let response = support::with_json(request, &body).send().await.unwrap();
    let (status, body) = support::json(response).await;
    local_only(status, &body, "POST /api/tasks");
    assert!(upstream.calls().is_empty());

    write_cloud_config(
        server.dir(),
        json!({
            "version": 1,
            "remoteUrl": null,
            "actorName": null,
            "sharedKey": null,
            "projectMappings": {},
        }),
    );
    for path in ["/api/device-workspaces", "/api/projects"] {
        let response = server.client.get(lan_url(path)).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }
}
