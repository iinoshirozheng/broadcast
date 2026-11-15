mod support;

use axum::Router;
use axum::extract::WebSocketUpgrade;
use hmac::{Hmac, Mac};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::Sha256;
use support::{Server, Upstream, serve};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

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
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

#[tokio::test]
async fn health_and_the_default_local_project_are_available() {
    let builder = Server::builder();
    let skill_path = builder.dir().join("skills/manage-broadcast/SKILL.md");
    let server = builder.env("BROADCAST_SKILL_PATH", &skill_path).start().await;

    assert_eq!(server.get("/health").await, (200, json!({ "status": "ok" })));
    let (status, metadata) = server.get("/api/meta").await;
    assert_eq!(status, 200);
    assert_eq!(
        metadata,
        json!({ "manageBroadcastSkillPath": skill_path, "capabilities": { "localAiChat": true } })
    );

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
async fn launcher_mode_proves_service_identity_and_hides_every_route_behind_its_instance_token() {
    let token = "7a6f8d37-78ce-46c9-87a8-08e10db88da2";
    let secret = "2e587946-96d6-47b5-930a-1ba70214fa88";
    let challenge = "8cbeea6e83e574def3f9d397cabddffc";
    let server = Server::builder()
        .env("BROADCAST_INSTANCE_TOKEN", token)
        .env("BROADCAST_INSTANCE_SECRET", secret)
        .env("BROADCAST_VERSION", "0.2.0")
        .start()
        .await;

    assert_eq!(server.get("/health").await.0, 401);
    let response = server
        .request(Method::GET, "/health")
        .header("x-broadcast-challenge", challenge)
        .send()
        .await
        .unwrap();
    let (status, health) = support::json(response).await;
    assert_eq!(status, 200);
    assert_eq!(health["product"], "broadcast");
    assert_eq!(health["version"], "0.2.0");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(challenge.as_bytes());
    let proof: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(health["proof"], proof);

    assert_eq!(server.get("/api/projects").await.0, 404);
    let launcher_api = server
        .request(Method::GET, &format!("/{token}/api/projects"))
        .header("origin", "null")
        .send()
        .await
        .unwrap();
    assert_eq!(launcher_api.status(), 200);
    assert_eq!(launcher_api.headers()["access-control-allow-origin"], "null");
}

#[tokio::test]
async fn accepts_private_lan_requests_and_rejects_public_host_and_origin_headers() {
    let server = Server::builder().env("BROADCAST_HOST", "0.0.0.0").start().await;
    let local = "127.0.0.1";

    assert_eq!(get_as(&server, local, &[("origin", "app://-")], "/health").await.0, 200);
    assert_eq!(get_as(&server, "192.168.1.24:47823", &[], "/health").await.0, 200);
    let lan_origin = [("origin", "http://192.168.1.24:47823")];
    assert_eq!(get_as(&server, local, &lan_origin, "/health").await.0, 200);
    assert_eq!(get_as(&server, "broadcast.local:47823", &[], "/health").await.0, 200);

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
        let headers: Vec<_> = origin.map(|origin| ("origin", origin)).into_iter().collect();
        assert_eq!(get_as(&server, local, &headers, "/health").await.0, expected);
        let mut events = server.request(Method::GET, "/api/events").header("host", local);
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

#[tokio::test]
async fn trusted_https_origins_do_not_inherit_device_local_capabilities_from_tunnel_loopback() {
    let upstream = Upstream::json(json!({ "projects": [] })).await;
    let builder = Server::builder();
    let skill_path = builder.dir().join("skills/manage-broadcast/SKILL.md");
    let server = builder
        .env("BROADCAST_SKILL_PATH", &skill_path)
        .env("BROADCAST_TRUSTED_ORIGINS", TRUSTED_ORIGIN)
        .cloud(&upstream.url)
        .start()
        .await;
    let local = "127.0.0.1";
    let public = "board.example.test";
    let trusted = [("origin", TRUSTED_ORIGIN)];

    let projects = get_as(&server, local, &trusted, "/api/projects").await;
    assert_eq!(projects, (200, json!({ "projects": [] })));
    let tunnelled = json!({
        "capabilities": { "localAiChat": false },
        "mode": "cloud",
        "realtime": { "transport": "websocket", "endpoint": "/api/events" },
        "localCapabilities": { "available": false },
    });
    assert_eq!(get_as(&server, local, &trusted, "/api/meta").await, (200, tunnelled.clone()));
    assert_eq!(get_as(&server, public, &[], "/api/meta").await, (200, tunnelled));

    for path in [
        "/api/local/host-runtime",
        "/api/local/jira-connection",
        "/api/local/ai/catalog?projectId=local",
        "/api/device-workspaces",
        "/api/projects/local/development-contexts",
    ] {
        for (host, headers) in [(local, &trusted[..]), (public, &[])] {
            let (status, body) = get_as(&server, host, headers, path).await;
            assert_eq!(status, 409, "{host} {path}");
            assert_eq!(body["error"]["code"], "LOCAL_COMPANION_REQUIRED", "{host} {path}");
        }
    }

    let (status, metadata) = server.get("/api/meta").await;
    assert_eq!(status, 200);
    assert_eq!(
        metadata,
        json!({
            "manageBroadcastSkillPath": skill_path,
            "capabilities": { "localAiChat": true },
            "mode": "cloud",
            "realtime": { "transport": "websocket", "endpoint": "/api/events" },
            "localCapabilities": { "available": true },
        })
    );
    assert_eq!(server.get("/api/local/host-runtime").await.0, 200);
    assert_eq!(server.get("/api/device-workspaces").await.0, 200);
}

#[tokio::test]
async fn trusted_https_origins_apply_to_cloud_websocket_upgrades_without_widening_loopback_routes()
{
    let upstream = serve(Router::new().fallback(|upgrade: WebSocketUpgrade| async {
        upgrade.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
    }))
    .await;
    let server = Server::builder()
        .env("BROADCAST_TRUSTED_ORIGINS", TRUSTED_ORIGIN)
        .cloud(&upstream)
        .start()
        .await;
    let url = format!("ws://127.0.0.1:{}/api/events", server.port);

    for (host, origin, expected) in [
        ("board.example.test", Some(TRUSTED_ORIGIN), 101),
        ("board.example.test", None, 101),
        ("board.example.test", Some("https://other.example.test"), 403),
        ("other.example.test", Some(TRUSTED_ORIGIN), 403),
        ("127.0.0.1", Some(TRUSTED_ORIGIN), 101),
        ("127.0.0.1", None, 101),
    ] {
        let mut request = url.as_str().into_client_request().unwrap();
        request.headers_mut().insert("host", host.parse().unwrap());
        if let Some(origin) = origin {
            request.headers_mut().insert("origin", origin.parse().unwrap());
        }
        let status = match tokio_tungstenite::connect_async(request).await {
            Ok(_) => 101,
            Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
            Err(error) => panic!("{host} {origin:?}: {error}"),
        };
        assert_eq!(status, expected, "{host} {origin:?}");
    }
}
