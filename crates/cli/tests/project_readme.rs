mod support;

use serde_json::json;
use support::Board;

#[tokio::test]
async fn project_readme_get_and_set() {
    let board = Board::start().await;
    board
        .post(
            "/api/projects",
            json!({ "id": "cli-proj", "name": "CLI Project" }),
        )
        .await;

    let (code, printed) = board
        .broadcast(&["project", "readme", "get", "cli-proj"])
        .await;
    assert_eq!(code, 0);
    assert_eq!(printed["readme"]["content"], "");
    assert_eq!(printed["readme"]["version"], 0);

    let (code, printed) = board
        .broadcast(&[
            "project",
            "readme",
            "set",
            "cli-proj",
            "--content=# Architecture\n\n- microservices",
        ])
        .await;
    assert_eq!(code, 0);
    assert_eq!(
        printed["readme"]["content"],
        "# Architecture\n\n- microservices"
    );
    assert_eq!(printed["readme"]["version"], 1);

    let file = board.dir().join("README.md");
    std::fs::write(&file, "# From File\n\nCreated by broadcast test.").unwrap();
    let file_option = format!("--file={}", file.display());
    let (code, printed) = board
        .broadcast(&[
            "project",
            "readme",
            "set",
            "cli-proj",
            &file_option,
            "--if-version=1",
        ])
        .await;
    assert_eq!(code, 0);
    assert_from_file(&printed);

    let (code, printed) = board.broadcast(&["project", "readme", "cli-proj"]).await;
    assert_eq!(code, 0);
    assert_from_file(&printed);

    let (code, printed) = board
        .broadcast(&[
            "project",
            "readme",
            "set",
            "cli-proj",
            "--content=foo",
            &file_option,
        ])
        .await;
    assert_eq!(code, 2);
    assert_eq!(printed["error"]["code"], "USAGE_ERROR");
}

fn assert_from_file(printed: &serde_json::Value) {
    assert_eq!(
        printed["readme"]["content"],
        "# From File\n\nCreated by broadcast test."
    );
    assert_eq!(printed["readme"]["version"], 2);
}
