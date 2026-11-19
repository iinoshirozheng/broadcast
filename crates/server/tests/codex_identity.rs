mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

const CODEX_AGENT: &str = "codex-agent";

fn codex_agent() -> Value {
    json!({ "type": "agent", "id": CODEX_AGENT, "name": "Codex Agent", "avatarUrl": null })
}

#[tokio::test]
async fn broadcast_issue_creation_and_comments_use_the_codex_agent_identity() {
    let server = Server::start().await;
    let spoofed = [
        ("x-broadcast-client", "broadcast"),
        ("x-broadcast-user-id", "spoofed-user"),
        ("x-broadcast-user-name", "Spoofed User"),
        ("x-broadcast-user-avatar", "https://example.com/spoofed.png"),
    ];

    let (status, created) = server
        .send_with(
            Method::POST,
            "/api/tasks",
            &spoofed,
            json!({ "title": "Created by Codex", "agentSession": { "platform": "codex", "sessionId": "thread-agent-create" } }),
        )
        .await;
    assert_eq!(status, 201);
    let task = &created["task"];
    assert_eq!(task["creatorType"], "agent");
    assert_eq!(task["creatorId"], CODEX_AGENT);
    assert_eq!(task["creatorName"], "Codex Agent");
    assert_eq!(task["creatorAvatarUrl"], Value::Null);
    assert_eq!(task["assignee"], codex_agent());

    let (status, created) = server
        .send_with(
            Method::POST,
            &format!("/api/tasks/{}/comments", task["id"].as_str().unwrap()),
            &spoofed,
            json!({ "body": "Implemented by Codex", "agentSession": { "platform": "codex", "sessionId": "thread-agent-comment" } }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &created["comment"];
    assert_eq!(comment["authorType"], "agent");
    assert_eq!(comment["authorId"], CODEX_AGENT);
    assert_eq!(comment["authorName"], "Codex Agent");
    assert_eq!(comment["authorAvatarUrl"], Value::Null);
    assert_eq!(comment["agentSession"]["sessionId"], "thread-agent-comment");
}

#[tokio::test]
async fn codex_hosted_user_mutations_persist_the_current_account_identity_and_avatar() {
    let server = Server::start().await;
    let user = [
        ("x-broadcast-user-id", "test-user"),
        ("x-broadcast-user-name", "Test%20User"),
        (
            "x-broadcast-user-avatar",
            "https://example.com/test-user.png",
        ),
    ];
    let test_user = json!({
        "type": "user",
        "id": "test-user",
        "name": "Test User",
        "avatarUrl": "https://example.com/test-user.png",
    });

    let (status, created) = server
        .send_with(
            Method::POST,
            "/api/tasks",
            &user,
            json!({ "title": "Created in Codex UI" }),
        )
        .await;
    assert_eq!(status, 201);
    let task = &created["task"];
    assert_eq!(task["creatorType"], "user");
    assert_eq!(task["creatorId"], "test-user");
    assert_eq!(task["creatorName"], "Test User");
    assert_eq!(
        task["creatorAvatarUrl"],
        "https://example.com/test-user.png"
    );
    assert_eq!(task["assignee"], test_user);
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let mut version = task["version"].clone();
    let mut patch = async |headers: &[(&str, &str)], mut body: Value| {
        body["version"] = version.clone();
        let (status, patched) = server.send_with(Method::PATCH, &path, headers, body).await;
        if status == 200 {
            version = patched["task"]["version"].clone();
        }
        (status, patched)
    };

    let (status, patched) = patch(&user, json!({ "assigneeTarget": CODEX_AGENT })).await;
    assert_eq!(status, 200);
    assert_eq!(patched["task"]["assignee"], codex_agent());
    let (status, patched) = patch(&user, json!({ "assigneeTarget": "current-user" })).await;
    assert_eq!(status, 200);
    assert_eq!(patched["task"]["assignee"], test_user);
    let codex = [("x-broadcast-client", "broadcast")];
    let (status, patched) = patch(&codex, json!({ "title": "Updated through broadcast" })).await;
    assert_eq!(status, 200);
    assert_eq!(patched["task"]["assignee"], test_user);
    let (status, invalid) = patch(&user, json!({ "assigneeTarget": { "type": "agent" } })).await;
    assert_eq!(status, 400);
    assert_eq!(invalid["error"]["code"], "INVALID_FIELD");

    let (status, created) = server
        .send_with(
            Method::POST,
            &format!("{path}/comments"),
            &user,
            json!({ "body": "Commented in Codex UI" }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &created["comment"];
    assert_eq!(comment["authorType"], "user");
    assert_eq!(comment["authorId"], "test-user");
    assert_eq!(comment["authorName"], "Test User");
    assert_eq!(
        comment["authorAvatarUrl"],
        "https://example.com/test-user.png"
    );
}
