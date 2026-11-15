mod support;

use reqwest::Method;
use serde_json::json;
use support::Server;

#[tokio::test]
async fn get_and_put_a_project_readme() {
    let server = Server::start().await;
    server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": "p-test", "name": "P Test" }),
        )
        .await;

    let (status, body) = server.get("/api/projects/p-test/readme").await;
    assert_eq!(status, 200);
    assert_eq!(body["readme"]["content"], "");
    assert_eq!(body["readme"]["version"], 0);

    let readme = "/api/projects/p-test/readme";
    let (status, body) = server
        .send(
            Method::PUT,
            readme,
            json!({ "content": "# Project Overview\n\nWelcome." }),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["readme"]["content"], "# Project Overview\n\nWelcome.");
    assert_eq!(body["readme"]["version"], 1);

    let (status, body) = server
        .send(
            Method::PUT,
            readme,
            json!({ "content": "# Updated Overview", "version": 1 }),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["readme"]["content"], "# Updated Overview");
    assert_eq!(body["readme"]["version"], 2);

    let (status, body) = server
        .send(
            Method::PUT,
            readme,
            json!({ "content": "# Stale", "version": 1 }),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "VERSION_CONFLICT");

    let (status, body) = server.get("/api/projects/non-existent/readme").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "PROJECT_NOT_FOUND");
}
