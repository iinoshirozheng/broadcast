mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

const CODEX_AGENT: &str = "codex-agent";

fn codex_agent() -> Value {
    json!({ "type": "agent", "id": CODEX_AGENT, "name": "Codex Agent", "avatarUrl": null })
}

#[tokio::test]
async fn development_context_scan_resolves_the_current_codex_conversation_workspace() {
    let builder = Server::builder();
    let workspace = builder.dir().to_string_lossy().into_owned();
    let processes = builder.codex_home().join("process_manager");
    std::fs::create_dir_all(&processes).unwrap();
    std::fs::write(
        processes.join("chat_processes.json"),
        json!({
            "recent": [{
                "conversationId": "019f7f96-287b-7da0-bc7f-ffe03af85cc8",
                "cwd": workspace,
                "updatedAtMs": 20,
            }],
        })
        .to_string(),
    )
    .unwrap();
    let server = builder.start().await;

    let (status, scan) = server
        .get("/api/projects/local/development-contexts?codexThreadId=019f7f96-287b-7da0-bc7f-ffe03af85cc8")
        .await;
    assert_eq!(status, 200);
    assert_eq!(scan["workspacePath"], workspace);
    assert_eq!(scan["contexts"], json!([]));

    let device_workspace = server.dir().join("another-device-workspace");
    let device_workspace = device_workspace.to_string_lossy();
    let query: String = form_urlencoded::byte_serialize(device_workspace.as_bytes()).collect();
    let (status, scan) = server
        .get(&format!(
            "/api/projects/local/development-contexts?workspacePath={query}"
        ))
        .await;
    assert_eq!(status, 200);
    assert_eq!(scan["workspacePath"], device_workspace.as_ref());
}

#[tokio::test]
async fn device_workspaces_come_from_this_machines_codex_project_roots() {
    let builder = Server::builder();
    std::fs::create_dir_all(builder.codex_home()).unwrap();
    std::fs::write(
        builder.codex_home().join(".codex-global-state.json"),
        json!({
            "local-projects": {
                "local-project-a": { "rootPaths": ["/Users/alice/project-a"] },
                "local-project-b": { "rootPaths": ["/Users/alice/project-b"] },
            },
        })
        .to_string(),
    )
    .unwrap();
    let server = builder.start().await;

    let (status, body) = server.get("/api/device-workspaces").await;
    assert_eq!(status, 200);
    assert_eq!(
        body["workspaces"],
        json!({
            "local-project-a": "/Users/alice/project-a",
            "local-project-b": "/Users/alice/project-b",
        })
    );
}

#[tokio::test]
async fn the_active_local_codex_conversation_supplies_its_exact_task_binding_identity() {
    let server = Server::start().await;
    let binding = json!({
        "threadId": "local-thread",
        "codexProjectId": "local-project",
        "codexProjectKind": "local",
        "codexHostId": "local",
        "workspacePath": "/work/local-project",
    });
    let mut runtime = binding.clone();
    runtime["threadRunning"] = json!(true);
    runtime["threadTodoProgress"] = Value::Null;
    let (status, _) = server
        .send(Method::PUT, "/api/local/host-runtime", runtime)
        .await;
    assert_eq!(status, 200);

    let (_, created) = server
        .send(
            Method::POST,
            "/api/tasks",
            json!({ "title": "Local binding", "threadId": "local-thread" }),
        )
        .await;
    assert_eq!(created["task"]["threadBinding"], binding);
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
            json!({ "title": "Created by Codex", "threadId": "thread-agent-create" }),
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
            json!({ "body": "Implemented by Codex", "threadId": "thread-agent-comment" }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &created["comment"];
    assert_eq!(comment["authorType"], "agent");
    assert_eq!(comment["authorId"], CODEX_AGENT);
    assert_eq!(comment["authorName"], "Codex Agent");
    assert_eq!(comment["authorAvatarUrl"], Value::Null);
    assert_eq!(comment["threadId"], "thread-agent-comment");
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
