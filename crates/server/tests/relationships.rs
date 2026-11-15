mod support;

use reqwest::Method;
use serde_json::{Value, json};
use support::Server;

async fn create(server: &Server, body: Value) -> Value {
    let (status, reply) = server.send(Method::POST, "/api/tasks", body).await;
    assert_eq!(status, 201, "{reply}");
    reply["task"].clone()
}

async fn latest(server: &Server, task: &Value) -> Value {
    server.get(&format!("/api/tasks/{}", id(task))).await.1["task"].clone()
}

fn id(task: &Value) -> &str {
    task["id"].as_str().unwrap()
}

fn ids(issues: &Value) -> Vec<&str> {
    issues.as_array().unwrap().iter().map(id).collect()
}

/// Adds (`POST`) or removes (`DELETE`) the `kind` relation from `task` to `related`.
async fn relate(
    server: &Server,
    method: Method,
    task: &Value,
    kind: &str,
    related: &Value,
    version: &Value,
) -> (u16, Value) {
    let path = format!("/api/tasks/{}/relations/{kind}/{}", id(task), id(related));
    let body = json!({ "version": version, "threadId": "thread-relations" });
    server.send(method, &path, body).await
}

#[tokio::test]
async fn issues_support_parent_sub_issue_blocking_and_related_issue_relationships() {
    let server = Server::start().await;
    let issue = async |title: &str, status: &str| {
        create(
            &server,
            json!({ "projectId": "local", "title": title, "status": status }),
        )
        .await
    };
    let add = async |task: &Value, kind: &str, related: &Value| {
        relate(&server, Method::POST, task, kind, related, &task["version"]).await
    };
    let parent = issue("Parent issue", "todo").await;
    let child = issue("Child issue", "done").await;
    let grandchild = issue("Grandchild issue", "canceled").await;
    let blocker = issue("Blocking issue", "in_progress").await;
    let related = issue("Related issue", "todo").await;

    let (status, added) = add(&child, "parent", &parent).await;
    assert_eq!(status, 200);
    assert_eq!(
        added["task"]["version"],
        child["version"].as_i64().unwrap() + 1
    );
    assert_eq!(added["task"]["threadId"], "thread-relations");
    assert_eq!(added["task"]["relations"]["parent"]["id"], id(&parent));
    assert_eq!(added["relatedTask"]["id"], id(&parent));
    let sub_issues = &latest(&server, &parent).await["relations"]["subIssues"];
    assert_eq!(ids(sub_issues), [id(&child)]);
    assert_eq!(sub_issues[0]["status"], "done");

    let child_now = latest(&server, &child).await;
    assert_eq!(add(&grandchild, "parent", &child_now).await.0, 200);
    let parent_now = latest(&server, &parent).await;
    let grandchild_now = latest(&server, &grandchild).await;
    let (status, cycle) = add(&parent_now, "parent", &grandchild_now).await;
    assert_eq!(status, 409);
    assert_eq!(cycle["error"]["code"], "RELATION_CYCLE");
    let (status, itself) = add(&parent_now, "related", &parent_now).await;
    assert_eq!(status, 400);
    assert_eq!(itself["error"]["code"], "SELF_RELATION");

    let (status, blocks) = add(&parent_now, "blocks", &blocker).await;
    assert_eq!(status, 200);
    assert_eq!(ids(&blocks["task"]["relations"]["blocks"]), [id(&blocker)]);
    let blocker_now = latest(&server, &blocker).await;
    assert_eq!(ids(&blocker_now["relations"]["blockedBy"]), [id(&parent)]);
    let parent_now = latest(&server, &parent).await;
    let (status, duplicate) = add(&blocker_now, "blocked_by", &parent_now).await;
    assert_eq!(status, 409);
    assert_eq!(duplicate["error"]["code"], "RELATION_EXISTS");

    let (status, added) = add(&parent_now, "related", &related).await;
    assert_eq!(status, 200);
    let with_related = &added["task"];
    assert_eq!(ids(&with_related["relations"]["related"]), [id(&related)]);
    let related_now = latest(&server, &related).await;
    assert_eq!(ids(&related_now["relations"]["related"]), [id(&parent)]);
    let stale_version = json!(with_related["version"].as_i64().unwrap() - 1);
    let (status, stale) = relate(
        &server,
        Method::DELETE,
        with_related,
        "related",
        &related,
        &stale_version,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(stale["error"]["code"], "VERSION_CONFLICT");
    let version = &with_related["version"];
    let (status, removed) = relate(
        &server,
        Method::DELETE,
        with_related,
        "related",
        &related,
        version,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(removed["task"]["relations"]["related"], json!([]));
    assert_eq!(
        latest(&server, &related).await["relations"]["related"],
        json!([])
    );

    let replacement = issue("Replacement parent", "todo").await;
    let child_now = latest(&server, &child).await;
    let (status, replaced) = add(&child_now, "parent", &replacement).await;
    assert_eq!(status, 200);
    assert_eq!(
        replaced["task"]["relations"]["parent"]["id"],
        id(&replacement)
    );
    assert_eq!(
        latest(&server, &parent).await["relations"]["subIssues"],
        json!([])
    );
    let replacement_now = latest(&server, &replacement).await;
    assert_eq!(
        ids(&replacement_now["relations"]["subIssues"]),
        [id(&child)]
    );
    let child_now = &replaced["task"];
    let version = &child_now["version"];
    let (status, removed) = relate(
        &server,
        Method::DELETE,
        child_now,
        "parent",
        &replacement,
        version,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(removed["task"]["relations"]["parent"], Value::Null);
    assert_eq!(
        latest(&server, &replacement).await["relations"]["subIssues"],
        json!([])
    );

    let (status, _) = server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": "other", "name": "Other" }),
        )
        .await;
    assert_eq!(status, 201);
    let other = create(
        &server,
        json!({ "projectId": "other", "title": "Other project issue", "status": "todo" }),
    )
    .await;
    let parent_now = latest(&server, &parent).await;
    let (status, cross) = add(&parent_now, "related", &other).await;
    assert_eq!(status, 400);
    assert_eq!(cross["error"]["code"], "CROSS_PROJECT_RELATION");
}

#[tokio::test]
async fn issue_tree_returns_deterministic_direct_and_nested_parent_paths_without_changing_relation_apis()
 {
    let server = Server::start().await;
    let issue = async |title: &str, project: &str| {
        create(&server, json!({ "projectId": project, "title": title })).await
    };
    let add_parent = async |child: &Value, parent: &Value| {
        let path = format!("/api/tasks/{}/relations/parent/{}", id(child), id(parent));
        let body = json!({ "version": child["version"] });
        server.send(Method::POST, &path, body).await.0
    };
    let tree = async |task: &Value, query: &str| {
        server
            .get(&format!("/api/tasks/{}/tree?{query}", id(task)))
            .await
    };
    let root = issue("Tree root", "local").await;
    let first = issue("Tree first", "local").await;
    let second = issue("Tree second", "local").await;
    let grandchild = issue("Tree grandchild", "local").await;
    let great_grandchild = issue("Tree great-grandchild", "local").await;
    for (child, parent) in [
        (&first, &root),
        (&second, &root),
        (&grandchild, &first),
        (&great_grandchild, &grandchild),
    ] {
        assert_eq!(add_parent(child, parent).await, 200);
    }

    let (status, direct) = tree(&root, "direction=descendants&depth=1").await;
    assert_eq!(status, 200);
    assert_eq!(direct["tree"]["nodeCount"], 3);
    let nodes = &direct["tree"]["nodes"];
    let children: Vec<&str> = ids(nodes).into_iter().skip(1).collect();
    let mut sorted = children.clone();
    sorted.sort();
    let mut expected = vec![id(&first), id(&second)];
    expected.sort();
    assert_eq!(sorted, expected);
    for node in nodes.as_array().unwrap() {
        let parent = if node["depth"] == 0 {
            Value::Null
        } else {
            json!(id(&root))
        };
        assert_eq!(node["parentId"], parent);
    }
    let (_, repeat) = tree(&root, "direction=descendants&depth=1").await;
    assert_eq!(repeat["tree"]["nodes"], *nodes);

    let (status, descendants) = tree(&root, "direction=descendants&depth=3").await;
    assert_eq!(status, 200);
    let nodes = descendants["tree"]["nodes"].as_array().unwrap();
    let mut order = vec![id(&root)];
    order.extend(&children);
    order.extend([id(&grandchild), id(&great_grandchild)]);
    assert_eq!(ids(&descendants["tree"]["nodes"]), order);
    let tail: Vec<_> = nodes[nodes.len() - 2..]
        .iter()
        .map(|node| (node["parentId"].clone(), node["depth"].clone()))
        .collect();
    assert_eq!(
        tail,
        [
            (json!(id(&first)), json!(2)),
            (json!(id(&grandchild)), json!(3))
        ]
    );
    let last = nodes.last().unwrap();
    assert_eq!(
        last["path"],
        json!([
            id(&root),
            id(&first),
            id(&grandchild),
            id(&great_grandchild)
        ])
    );
    assert_eq!(
        last["summary"],
        json!({
            "identifier": great_grandchild["identifier"],
            "title": "Tree great-grandchild",
            "status": "backlog",
            "priority": "none",
            "archivedAt": null,
        })
    );

    let (status, ancestors) = tree(&great_grandchild, "direction=ancestors&depth=3").await;
    assert_eq!(status, 200);
    let chain: Vec<_> = ancestors["tree"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["id"].clone(),
                node["parentId"].clone(),
                node["depth"].clone(),
            )
        })
        .collect();
    assert_eq!(
        chain,
        [
            (json!(id(&great_grandchild)), Value::Null, json!(0)),
            (
                json!(id(&grandchild)),
                json!(id(&great_grandchild)),
                json!(1)
            ),
            (json!(id(&first)), json!(id(&grandchild)), json!(2)),
            (json!(id(&root)), json!(id(&first)), json!(3)),
        ]
    );

    let grandchild_now = latest(&server, &grandchild).await;
    let second_now = latest(&server, &second).await;
    assert_eq!(add_parent(&grandchild_now, &second_now).await, 200);
    let (_, reparented) = tree(&root, "direction=descendants&depth=3").await;
    assert_eq!(ids(&reparented["tree"]["nodes"]), order);
    let moved = reparented["tree"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == id(&grandchild))
        .unwrap();
    assert_eq!(
        moved["path"],
        json!([id(&root), id(&second), id(&grandchild)])
    );

    let (status, invalid) = tree(&root, "direction=descendants&depth=26").await;
    assert_eq!(status, 400);
    assert_eq!(invalid["error"]["code"], "INVALID_TREE_QUERY");

    // The database itself refuses cycles and cross-project relations.
    let insert = |kind: &str, source: &Value, target: &Value| {
        let error = server
            .database()
            .execute(
                "INSERT INTO task_relations (relation_type, source_task_id, target_task_id, origin, created_at)
                 VALUES (?1, ?2, ?3, 'manual', ?4)",
                [kind, id(source), id(target), &chrono::Utc::now().to_rfc3339()],
            )
            .expect_err("the trigger rejects the relation");
        error.to_string()
    };
    assert!(insert("parent", &great_grandchild, &root).contains("RELATION_CYCLE"));
    let (status, _) = server
        .send(
            Method::POST,
            "/api/projects",
            json!({ "id": "tree-other", "name": "Tree other" }),
        )
        .await;
    assert_eq!(status, 201);
    let external = issue("Tree external", "tree-other").await;
    assert!(insert("blocks", &root, &external).contains("CROSS_PROJECT_RELATION"));
}
