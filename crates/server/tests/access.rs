mod support;

use reqwest::Method;
use serde_json::Value;
use support::Server;

const TRUSTED_ORIGIN: &str = "https://board.example.test";

/// GETs `path` with the `host` header, and `headers`; a body that is not JSON is a string.
async fn get_as(server: &Server, host: &str, headers: &[(&str, &str)], path: &str) -> (u16, Value) {
    let mut request = server.request(Method::GET, path).header("host", host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.expect("request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("response body");
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn health_and_the_default_local_project_are_available() {
    let server = Server::builder().start().await;

    let (status, health) = server.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["status"], "ok");

    let (status, body) = server.get("/api/projects").await;
    assert_eq!(status, 200);
    let projects = body["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["id"], "local");
    assert_eq!(projects[0]["name"], "全局");
    assert_eq!(projects[0]["workspacePath"], Value::Null);
    assert_eq!(projects[0]["issueCount"], 0);
}

#[tokio::test]
async fn accepts_private_lan_requests_and_rejects_public_host_and_origin_headers() {
    let server = Server::builder()
        .env("BROADCAST_HOST", "0.0.0.0")
        .start()
        .await;
    let local = "127.0.0.1";

    assert_eq!(
        get_as(&server, "192.168.1.24:47823", &[], "/health")
            .await
            .0,
        200
    );
    let lan_origin = [("origin", "http://192.168.1.24:47823")];
    assert_eq!(get_as(&server, local, &lan_origin, "/health").await.0, 200);
    assert_eq!(
        get_as(&server, "broadcast.local:47823", &[], "/health")
            .await
            .0,
        200
    );

    let (status, body) = get_as(&server, "broadcast.example.com", &[], "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_HOST");
    let evil = [("origin", "https://evil.example")];
    let (status, body) = get_as(&server, local, &evil, "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_ORIGIN");
}

#[tokio::test]
async fn trusted_https_origins_allow_their_exact_public_host_without_trusting_forwarded_headers() {
    let builder = Server::builder();
    std::fs::write(
        builder.dir().join("index.html"),
        "<!doctype html><title>Broadcast</title>",
    )
    .unwrap();
    let static_dir = builder.dir().to_owned();
    let server = builder
        .env("BROADCAST_STATIC_DIR", static_dir)
        .env("BROADCAST_TRUSTED_ORIGINS", TRUSTED_ORIGIN)
        .start()
        .await;
    let local = "127.0.0.1";
    let public = "board.example.test";

    for (origin, expected) in [
        (Some(TRUSTED_ORIGIN), 200),
        (Some("https://other.example.test"), 403),
        (None, 200),
    ] {
        let headers: Vec<_> = origin
            .map(|origin| ("origin", origin))
            .into_iter()
            .collect();
        assert_eq!(
            get_as(&server, local, &headers, "/health").await.0,
            expected
        );
        let mut events = server
            .request(Method::GET, "/api/events")
            .header("host", local);
        if let Some(origin) = origin {
            events = events.header("origin", origin);
        }
        assert_eq!(events.send().await.unwrap().status(), expected);
    }

    let (status, page) = get_as(&server, public, &[], "/").await;
    assert_eq!(status, 200);
    assert!(page.as_str().unwrap().contains("<title>Broadcast</title>"));
    let (status, api) = get_as(&server, public, &[], "/api/projects").await;
    assert_eq!(status, 200);
    assert_eq!(api["projects"].as_array().unwrap().len(), 1);
    let trusted = [("origin", TRUSTED_ORIGIN)];
    assert_eq!(get_as(&server, public, &trusted, "/health").await.0, 200);

    let foreign = [("origin", "https://other.example.test")];
    let (status, body) = get_as(&server, public, &foreign, "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_ORIGIN");
    let (status, body) = get_as(&server, "board.example.test:8443", &[], "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_HOST");
    let forwarded = [
        ("origin", TRUSTED_ORIGIN),
        ("x-forwarded-host", public),
        ("x-forwarded-proto", "https"),
    ];
    let (status, body) = get_as(&server, "untrusted.example.test", &forwarded, "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_HOST");
    let wrong_port = [("origin", "https://board.example.test:8443")];
    let (status, body) = get_as(&server, local, &wrong_port, "/health").await;
    assert_eq!(status, 403);
    assert_eq!(body["error"]["code"], "INVALID_ORIGIN");
}
