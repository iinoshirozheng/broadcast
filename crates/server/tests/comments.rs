mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

async fn create_task(server: &Server, title: &str) -> String {
    let (_, reply) = server
        .send(Method::POST, "/api/tasks", json!({ "title": title }))
        .await;
    reply["task"]["id"].as_str().unwrap().to_owned()
}

async fn thread_id(server: &Server, task: &str) -> Value {
    server.get(&format!("/api/tasks/{task}")).await.1["task"]["threadId"].clone()
}

#[tokio::test]
async fn issue_comments_can_be_created_edited_listed_and_deleted() {
    let server = Server::start().await;
    let task = create_task(&server, "Discuss me").await;
    let comments = format!("/api/tasks/{task}/comments");
    let (status, empty) = server.get(&comments).await;
    assert_eq!(status, 200);
    assert_eq!(empty["comments"], json!([]));

    let (status, created) = server
        .send(
            Method::POST,
            &comments,
            json!({ "body": "First comment", "threadId": "thread-comment-create" }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &created["comment"];
    assert_eq!(comment["taskId"], task);
    assert_eq!(comment["body"], "First comment");
    assert_eq!(comment["threadId"], "thread-comment-create");
    assert_eq!(comment["attachments"], json!([]));
    assert_eq!(comment["authorType"], "user");
    assert_eq!(comment["authorId"], "local-user");
    assert_eq!(comment["authorName"], "本地用户");
    assert_eq!(comment["version"], 1);
    let (_, listed) = server.get(&comments).await;
    assert_eq!(listed["comments"].as_array().unwrap().len(), 1);
    assert_eq!(listed["comments"][0]["id"], comment["id"]);

    let path = format!("/api/comments/{}", comment["id"].as_str().unwrap());
    let (status, edited) = server
        .send(
            Method::PATCH,
            &path,
            json!({
                "version": comment["version"],
                "body": "Edited comment",
                "threadId": "thread-comment-update",
            }),
        )
        .await;
    assert_eq!(status, 200);
    let updated = &edited["comment"];
    assert_eq!(updated["body"], "Edited comment");
    assert_eq!(updated["threadId"], "thread-comment-update");
    assert_eq!(updated["version"], 2);
    assert_eq!(thread_id(&server, &task).await, Value::Null);

    let (status, stale) = server
        .send(
            Method::PATCH,
            &path,
            json!({ "version": comment["version"], "body": "Stale edit" }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(stale["error"]["code"], "VERSION_CONFLICT");

    let (status, _) = server
        .send(
            Method::DELETE,
            &path,
            json!({ "version": updated["version"], "threadId": "thread-comment-delete" }),
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(server.get(&comments).await.1["comments"], json!([]));
    assert_eq!(thread_id(&server, &task).await, Value::Null);
}

#[tokio::test]
async fn comments_support_attachments_and_deleting_a_comment_removes_its_files() {
    let server = Server::start().await;
    let task = create_task(&server, "Comment files").await;
    let (status, created) = server
        .send(
            Method::POST,
            &format!("/api/tasks/{task}/comments"),
            json!({ "body": "", "threadId": "thread-attachment" }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &created["comment"];
    assert_eq!(comment["body"], "");
    let path = format!("/api/comments/{}", comment["id"].as_str().unwrap());

    let (status, uploaded) = server
        .upload(
            &format!("{path}/attachments"),
            "text/plain",
            "comment.txt",
            "comment attachment\n",
        )
        .await;
    assert_eq!(status, 201);
    let attachment = &uploaded["attachment"];
    assert_eq!(attachment["taskId"], task);
    assert_eq!(attachment["commentId"], comment["id"]);
    let only = json!([attachment]);
    assert_eq!(server.get(&format!("{path}/attachments")).await.1["attachments"], only);
    let (_, comments) = server.get(&format!("/api/tasks/{task}/comments")).await;
    assert_eq!(comments["comments"][0]["attachments"], only);
    let (_, task_attachments) = server.get(&format!("/api/tasks/{task}/attachments")).await;
    assert_eq!(task_attachments["attachments"], json!([]));

    let file = server.attachment_file(&attachment["id"]);
    assert!(file.exists());
    let (status, _) = server
        .send(
            Method::DELETE,
            &path,
            json!({ "version": comment["version"], "threadId": "thread-delete-comment" }),
        )
        .await;
    assert_eq!(status, 204);
    assert!(!file.exists());
    let content = format!("/api/attachments/{}/content", attachment["id"].as_str().unwrap());
    assert_eq!(server.get(&content).await.0, 404);
    assert_eq!(thread_id(&server, &task).await, Value::Null);
}
