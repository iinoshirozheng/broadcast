mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

/// Sends `body` and returns the task in the successful reply.
async fn task(server: &Server, method: Method, path: &str, body: Value) -> Value {
    let (status, reply) = server.send(method, path, body).await;
    assert!(status < 300, "{path}: {status} {reply}");
    reply["task"].clone()
}

async fn create(server: &Server, body: Value) -> Value {
    task(server, Method::POST, "/api/tasks", body).await
}

/// The ids of the tasks `path` lists.
async fn listed(server: &Server, path: &str) -> Vec<Value> {
    let (status, body) = server.get(path).await;
    assert_eq!(status, 200, "{path}");
    let tasks = body["tasks"].as_array().unwrap();
    tasks.iter().map(|task| task["id"].clone()).collect()
}

async fn issue_count(server: &Server, project: &str) -> Value {
    let (_, body) = server.get("/api/projects").await;
    let projects = body["projects"].as_array().unwrap();
    let project = projects
        .iter()
        .find(|listed| listed["id"] == project)
        .unwrap();
    project["issueCount"].clone()
}

#[tokio::test]
async fn project_and_task_crud_flow() {
    let server = Server::start().await;
    let (status, project) = server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": "website", "name": "Website", "workspacePath": "/work/website" }),
        )
        .await;
    assert_eq!(status, 201);
    assert_eq!(project["project"]["id"], "website");
    assert_eq!(project["project"]["workspacePath"], "/work/website");

    let worktree = json!({
        "type": "worktree",
        "path": "/work/website/.worktrees/broadcast",
        "branch": "worktree/broadcast",
    });
    let (status, body) = server
        .send(
            Method::POST,
            "/api/tasks",
            json!({
                "projectId": "website",
                "title": "Build task board",
                "description": "Create the first local board",
                "status": "todo",
                "priority": "high",
                "labels": ["frontend", "mvp"],
                "agentSession": { "platform": "codex", "sessionId": "thread-123" },
                "developmentContext": worktree,
                "dueDate": "2026-07-24",
                "recurrence": { "interval": 2, "unit": "week" },
            }),
        )
        .await;
    assert_eq!(status, 201);
    let created = &body["task"];
    assert_eq!(created["identifier"], "WEB-1");
    assert_eq!(created["version"], 1);
    assert_eq!(created["sortOrder"], 1000);
    assert_eq!(created["archivedAt"], Value::Null);
    assert_eq!(created["labels"], json!(["frontend", "mvp"]));
    assert_eq!(created["agentSession"]["sessionId"], "thread-123");
    assert_eq!(created["creatorType"], "user");
    assert_eq!(created["creatorId"], "local-user");
    assert_eq!(created["creatorName"], "本地用户");
    assert_eq!(created["creatorAvatarUrl"], Value::Null);
    assert_eq!(created["developmentContext"], worktree);
    assert_eq!(created["dueDate"], "2026-07-24");
    assert_eq!(
        created["recurrence"],
        json!({ "interval": 2, "unit": "week" })
    );
    assert_eq!(issue_count(&server, "website").await, 1);

    let id = created["id"].as_str().unwrap();
    let identifier = created["identifier"].as_str().unwrap();
    assert_eq!(
        server.get(&format!("/api/tasks/{id}")).await,
        (200, json!({ "task": created }))
    );
    let (status, by_identifier) = server.get(&format!("/api/tasks/{identifier}")).await;
    assert_eq!(status, 200);
    assert_eq!(by_identifier["task"]["id"], id);
    assert_eq!(
        listed(&server, "/api/tasks?projectId=website&status=todo").await,
        [id]
    );

    let branch = json!({ "type": "branch", "branch": "feature/polish" });
    let updated = task(
        &server,
        Method::PATCH,
        &format!("/api/tasks/{identifier}"),
        json!({
            "version": created["version"],
            "title": "Build polished task board",
            "priority": "urgent",
            "agentSession": { "platform": "codex", "sessionId": "thread-456" },
            "developmentContext": branch,
        }),
    )
    .await;
    assert_eq!(updated["title"], "Build polished task board");
    assert_eq!(updated["priority"], "urgent");
    assert_eq!(updated["agentSession"]["sessionId"], "thread-456");
    assert_eq!(updated["developmentContext"], branch);
    assert_eq!(updated["version"], 2);

    let archived = task(
        &server,
        Method::POST,
        &format!("/api/tasks/{id}/archive"),
        json!({ "version": updated["version"], "agentSession": { "platform": "codex", "sessionId": "thread-archive" } }),
    )
    .await;
    assert_eq!(archived["version"], 3);
    assert_eq!(archived["agentSession"]["sessionId"], "thread-archive");
    chrono::DateTime::parse_from_rfc3339(archived["archivedAt"].as_str().unwrap()).unwrap();
    assert!(
        listed(&server, "/api/tasks?projectId=website")
            .await
            .is_empty()
    );
    assert_eq!(
        listed(&server, "/api/tasks?projectId=website&archived=true").await,
        [id]
    );
    assert_eq!(issue_count(&server, "website").await, 0);

    let restored = task(
        &server,
        Method::POST,
        &format!("/api/tasks/{id}/restore"),
        json!({ "version": archived["version"], "agentSession": { "platform": "codex", "sessionId": "thread-restore" } }),
    )
    .await;
    assert_eq!(restored["archivedAt"], Value::Null);
    assert_eq!(restored["version"], 4);
    assert_eq!(restored["agentSession"]["sessionId"], "thread-restore");
    assert_eq!(listed(&server, "/api/tasks?projectId=website").await, [id]);
    assert_eq!(issue_count(&server, "website").await, 1);
}

#[tokio::test]
async fn moving_a_task_updates_its_status_and_sort_order() {
    let server = Server::start().await;
    let created = create(&server, json!({ "title": "Move me" })).await;
    let moved = task(
        &server,
        Method::POST,
        &format!("/api/tasks/{}/move", created["id"].as_str().unwrap()),
        json!({
            "version": created["version"],
            "status": "in_progress",
            "sortOrder": 2500.5,
            "agentSession": { "platform": "codex", "sessionId": "thread-move" },
        }),
    )
    .await;
    assert_eq!(moved["status"], "in_progress");
    assert_eq!(moved["sortOrder"], 2500.5);
    assert_eq!(moved["agentSession"]["sessionId"], "thread-move");
    assert_eq!(moved["version"], 2);
}

#[tokio::test]
async fn all_task_statuses_are_accepted_filtered_and_listed_in_workflow_order() {
    let server = Server::start().await;
    for status in [
        "canceled",
        "done",
        "blocked",
        "in_review",
        "in_progress",
        "todo",
        "backlog",
    ] {
        let created = create(&server, json!({ "title": status, "status": status })).await;
        assert_eq!(created["status"], status);
    }
    let statuses = async |path: &str| -> Vec<Value> {
        let (status, body) = server.get(path).await;
        assert_eq!(status, 200, "{path}");
        let tasks = body["tasks"].as_array().unwrap();
        tasks.iter().map(|task| task["status"].clone()).collect()
    };
    assert_eq!(
        statuses("/api/tasks").await,
        [
            "backlog",
            "todo",
            "in_progress",
            "in_review",
            "blocked",
            "done",
            "canceled"
        ]
    );
    for status in ["in_review", "blocked", "canceled"] {
        assert_eq!(
            statuses(&format!("/api/tasks?status={status}")).await,
            [status]
        );
    }
}

#[tokio::test]
async fn task_and_comment_mutations_keep_content_specific_conversation_attribution() {
    let server = Server::start().await;
    let created = create(
        &server,
        json!({ "title": "Keep attribution", "agentSession": { "platform": "codex", "sessionId": "thread-original" } }),
    )
    .await;
    let path = format!("/api/tasks/{}", created["id"].as_str().unwrap());
    let updated = task(
        &server,
        Method::PATCH,
        &path,
        json!({ "version": created["version"], "title": "Still attributed" }),
    )
    .await;
    assert_eq!(updated["agentSession"]["sessionId"], "thread-original");
    let repeated = task(
        &server,
        Method::PATCH,
        &path,
        json!({
            "version": updated["version"],
            "title": "Still attributed again",
            "agentSession": { "platform": "codex", "sessionId": "thread-original" },
        }),
    )
    .await;
    assert_eq!(repeated["agentSession"]["sessionId"], "thread-original");

    let (_, reply) = server
        .send(
            Method::POST,
            &format!("{path}/comments"),
            json!({ "body": "Attributed comment", "agentSession": { "platform": "codex", "sessionId": "thread-comment" } }),
        )
        .await;
    let comment = &reply["comment"];
    let (_, edited) = server
        .send(
            Method::PATCH,
            &format!("/api/comments/{}", comment["id"].as_str().unwrap()),
            json!({ "version": comment["version"], "body": "Edited from the UI" }),
        )
        .await;
    assert_eq!(
        edited["comment"]["agentSession"]["sessionId"],
        "thread-comment"
    );
    assert_eq!(
        server.get(&path).await.1["task"]["agentSession"]["sessionId"],
        "thread-original"
    );
}

#[tokio::test]
async fn stale_updates_receive_a_version_conflict() {
    let server = Server::start().await;
    let created = create(&server, json!({ "title": "Concurrent edit" })).await;
    let path = format!("/api/tasks/{}", created["id"].as_str().unwrap());
    task(
        &server,
        Method::PATCH,
        &path,
        json!({ "version": created["version"], "title": "First editor" }),
    )
    .await;

    let (status, stale) = server
        .send(
            Method::POST,
            &format!("{path}/move"),
            json!({ "version": created["version"], "status": "done", "sortOrder": 1 }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(stale["error"]["code"], "VERSION_CONFLICT");
    assert_eq!(
        stale["error"]["details"],
        json!({ "expectedVersion": 1, "actualVersion": 2 })
    );
}

#[tokio::test]
async fn request_boundaries_reject_unknown_fields_and_invalid_values() {
    let server = Server::start().await;
    let rejected = async |body: Value| {
        let (status, reply) = server.send(Method::POST, "/api/tasks", body).await;
        assert_eq!(status, 400, "{reply}");
        reply["error"].clone()
    };

    let unknown = rejected(json!({ "title": "Invalid", "unexpected": true })).await;
    assert_eq!(unknown["code"], "UNKNOWN_FIELD");
    let invalid = rejected(json!({ "title": "Invalid", "status": "started" })).await;
    assert_eq!(invalid["code"], "INVALID_FIELD");
    let message = invalid["message"].as_str().unwrap();
    for status in ["in_review", "blocked", "canceled"] {
        assert!(message.contains(status), "{message}");
    }
    let worktree = json!({ "type": "worktree", "path": "/tmp/bad\0path", "branch": null });
    let invalid = rejected(json!({ "title": "Invalid", "developmentContext": worktree })).await;
    assert_eq!(invalid["code"], "INVALID_FIELD");
}
