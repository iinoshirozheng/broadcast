mod support;

use reqwest::{Method, Response};
use serde_json::{Value, json};
use support::Server;

/// Reads `events` to the end of the next server-sent event, checks that it is `name`, and returns
/// its data.
async fn next_event(events: &mut Response, name: &str) -> Value {
    let mut message = String::new();
    while !message.contains("\n\n") {
        let chunk = events.chunk().await.unwrap().expect("the stream stays open");
        message.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(message.contains(&format!("event: {name}")), "{message}");
    let data = message
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

async fn create(server: &Server, headers: &[(&str, &str)], title: &str) -> Value {
    let (status, reply) = server
        .send_with(Method::POST, "/api/tasks", headers, json!({ "title": title }))
        .await;
    assert_eq!(status, 201);
    reply["task"].clone()
}

#[tokio::test]
async fn issue_relationship_changes_are_broadcast_in_realtime() {
    let server = Server::start().await;
    let first = create(&server, &[], "Realtime source").await;
    let second = create(&server, &[], "Realtime target").await;
    let mut events = server.request(Method::GET, "/api/events").send().await.unwrap();
    events.chunk().await.unwrap();

    let path = format!(
        "/api/tasks/{}/relations/related/{}",
        first["id"].as_str().unwrap(),
        second["id"].as_str().unwrap()
    );
    let body = json!({ "version": first["version"], "threadId": "thread-realtime-relation" });
    assert_eq!(server.send(Method::POST, &path, body).await.0, 200);

    let event = next_event(&mut events, "task.relation.updated").await;
    assert_eq!(event["type"], "task.relation.updated");
    assert_eq!(event["task"]["id"], first["id"]);
    assert_eq!(event["relatedTask"]["id"], second["id"]);
}

#[tokio::test]
async fn task_changes_from_one_lan_client_are_broadcast_to_another_client() {
    let server = Server::builder().env("BROADCAST_HOST", "0.0.0.0").start().await;
    let lan = [
        ("host", "192.168.1.24:47823"),
        ("origin", "http://192.168.1.24:47823"),
    ];
    let mut events = server.request(Method::GET, "/api/events");
    for (name, value) in lan {
        events = events.header(name, value);
    }
    let mut events = events.send().await.unwrap();
    assert_eq!(events.status(), 200);
    events.chunk().await.unwrap();

    let task = create(&server, &lan, "Broadcast me").await;
    let event = next_event(&mut events, "task.created").await;
    assert_eq!(event["type"], "task.created");
    assert_eq!(event["task"]["id"], task["id"]);

    let mut list = server.request(Method::GET, "/api/tasks?projectId=local");
    for (name, value) in lan {
        list = list.header(name, value);
    }
    let (status, body) = support::json(list.send().await.unwrap()).await;
    assert_eq!(status, 200);
    let tasks = body["tasks"].as_array().unwrap();
    assert!(tasks.iter().any(|listed| listed["id"] == task["id"]));
}
