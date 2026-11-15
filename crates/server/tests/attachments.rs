mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

async fn create_task(server: &Server, body: Value) -> Value {
    let (status, reply) = server.send(Method::POST, "/api/tasks", body).await;
    assert_eq!(status, 201, "{reply}");
    reply["task"].clone()
}

#[tokio::test]
async fn issue_attachments_can_be_uploaded_listed_opened_downloaded_and_deleted() {
    let server = Server::start().await;
    let task = create_task(&server, json!({ "title": "Attach files" })).await;
    let attachments = format!("/api/tasks/{}/attachments", task["id"].as_str().unwrap());
    let (status, empty) = server.get(&attachments).await;
    assert_eq!(status, 200);
    assert_eq!(empty["attachments"], json!([]));

    let contents = "attachment contents\n";
    let (status, uploaded) = server
        .upload(&attachments, "text/plain; charset=utf-8", "设计说明.txt", contents)
        .await;
    assert_eq!(status, 201);
    let attachment = &uploaded["attachment"];
    assert_eq!(attachment["taskId"], task["id"]);
    assert_eq!(attachment["commentId"], Value::Null);
    assert_eq!(attachment["filename"], "设计说明.txt");
    assert_eq!(attachment["contentType"], "text/plain");
    assert_eq!(attachment["size"], contents.len());
    chrono::DateTime::parse_from_rfc3339(attachment["createdAt"].as_str().unwrap()).unwrap();
    assert_eq!(server.get(&attachments).await.1["attachments"], json!([attachment]));

    let id = attachment["id"].as_str().unwrap();
    let content = format!("/api/attachments/{id}/content");
    let response = server.request(Method::GET, &content).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(headers["content-type"], "text/plain");
    let disposition = headers["content-disposition"].to_str().unwrap();
    assert!(disposition.starts_with("inline; filename*=UTF-8''"), "{disposition}");
    assert_eq!(response.text().await.unwrap(), contents);
    let head = server.request(Method::HEAD, &content).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], contents.len().to_string().as_str());
    assert_eq!(head.text().await.unwrap(), "");

    let (_, html) = server
        .upload(
            &attachments,
            "text/html",
            "page.html",
            "<script>document.body.textContent = 'unsafe'</script>",
        )
        .await;
    let html = html["attachment"]["id"].as_str().unwrap();
    let response = server
        .request(Method::GET, &format!("/api/attachments/{html}/content"))
        .send()
        .await
        .unwrap();
    let headers = response.headers();
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment;"));
    assert_eq!(headers["content-security-policy"], "sandbox; default-src 'none'");
    let delete = |id: &str| server.request(Method::DELETE, &format!("/api/attachments/{id}")).send();
    assert_eq!(delete(html).await.unwrap().status(), 204);

    assert_eq!(delete(id).await.unwrap().status(), 204);
    assert_eq!(server.get(&attachments).await.1["attachments"], json!([]));
    let (status, missing) = server.get(&content).await;
    assert_eq!(status, 404);
    assert_eq!(missing["error"]["code"], "ATTACHMENT_NOT_FOUND");
}

#[tokio::test]
async fn permanent_task_deletion_requires_archiving_and_removes_attachment_files() {
    let server = Server::start().await;
    server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": "temp-delete-project", "name": "Delete project", "workspacePath": null }),
        )
        .await;
    let task = create_task(
        &server,
        json!({ "projectId": "temp-delete-project", "title": "Delete permanently" }),
    )
    .await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let (status, uploaded) = server
        .upload(&format!("{path}/attachments"), "text/plain", "evidence.txt", "attachment")
        .await;
    assert_eq!(status, 201);
    let (status, comment) = server
        .send(
            Method::POST,
            &format!("{path}/comments"),
            json!({ "body": "Comment with attachment" }),
        )
        .await;
    assert_eq!(status, 201);
    let comment = &comment["comment"]["id"];
    let (status, comment_upload) = server
        .upload(
            &format!("/api/comments/{}/attachments", comment.as_str().unwrap()),
            "text/plain",
            "comment-evidence.txt",
            "comment attachment",
        )
        .await;
    assert_eq!(status, 201);
    let attachments = [
        uploaded["attachment"]["id"].clone(),
        comment_upload["attachment"]["id"].clone(),
    ];
    let files = attachments.each_ref().map(|id| server.attachment_file(id));
    assert!(files.iter().all(|file| file.exists()));

    let (status, active) = server
        .send(Method::DELETE, &path, json!({ "version": task["version"] }))
        .await;
    assert_eq!(status, 409);
    assert_eq!(active["error"]["code"], "TASK_NOT_ARCHIVED");
    let (_, archived) = server
        .send(
            Method::POST,
            &format!("{path}/archive"),
            json!({ "version": task["version"] }),
        )
        .await;
    let (status, _) = server
        .send(
            Method::DELETE,
            &path,
            json!({ "version": archived["task"]["version"] }),
        )
        .await;
    assert_eq!(status, 204);
    assert!(files.iter().all(|file| !file.exists()));
    assert_eq!(server.get(&path).await.0, 404);

    let database = server.database();
    let exists = |table: &str, id: &Value| {
        let query = format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE id = ?1)");
        database
            .query_row(&query, [id.as_str().unwrap()], |row| row.get::<_, bool>(0))
            .unwrap()
    };
    assert!(!exists("tasks", &task["id"]));
    assert!(!exists("comments", comment));
    assert!(attachments.iter().all(|id| !exists("attachments", id)));
    let project = server.request(Method::DELETE, "/api/projects/temp-delete-project");
    assert_eq!(project.send().await.unwrap().status(), 204);
}

#[tokio::test]
async fn attachment_uploads_reject_unsafe_filenames() {
    let server = Server::start().await;
    let task = create_task(&server, json!({ "title": "Validate attachments" })).await;
    let (status, rejected) = server
        .upload(
            &format!("/api/tasks/{}/attachments", task["id"].as_str().unwrap()),
            "text/plain",
            "../outside.txt",
            "unsafe",
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(rejected["error"]["code"], "INVALID_FILENAME");
}
