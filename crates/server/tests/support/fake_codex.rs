//! A stand-in for the Codex CLI that the AI chat suite points `CODEX_EXECUTABLE` at. It lists one
//! model and one skill, and answers every turn with "ok" in Codex thread `session-1`.

use std::io::{BufRead, Read, Write};

const MODELS: &str = r#"{"models":[{"slug":"gpt-real","display_name":"GPT Real","description":"","default_reasoning_level":"low","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"service_tiers":[]}]}"#;
const INITIALIZED: &str = r#"{"id":1,"result":{}}"#;
const SKILLS: &str = r#"{"id":2,"result":{"data":[{"skills":[{"name":"real-skill","enabled":true,"scope":"repo","interface":null}]}]}}"#;
const TURN: [&str; 3] = [
    r#"{"type":"thread.started","thread_id":"session-1"}"#,
    r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#,
    r#"{"type":"turn.completed"}"#,
];

fn main() -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match std::env::args().nth(1).as_deref() {
        Some("debug") => write!(stdout, "{MODELS}")?,
        Some("app-server") => {
            for line in std::io::stdin().lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let message: serde_json::Value =
                    serde_json::from_str(&line).expect("a JSON-RPC message");
                match message["id"].as_u64() {
                    Some(1) => writeln!(stdout, "{INITIALIZED}")?,
                    Some(2) => writeln!(stdout, "{SKILLS}")?,
                    _ => {}
                }
                stdout.flush()?;
            }
        }
        _ => {
            std::io::stdin().read_to_end(&mut Vec::new())?;
            for event in TURN {
                writeln!(stdout, "{event}")?;
            }
        }
    }
    Ok(())
}
