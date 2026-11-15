//! Agent runners seen on this board: each heartbeat keeps its agent online for a short window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use broadcast_domain::records::Actor;
use serde_json::{Value, json};

/// Runners beat every 15 seconds; three missed beats mark the agent offline.
const PRESENCE_WINDOW: Duration = Duration::from_secs(45);

struct Presence {
    agent_name: String,
    seen: Instant,
    last_seen_at: String,
}

#[derive(Default)]
pub struct Runners(Mutex<HashMap<String, Presence>>);

fn record(agent_id: &str, presence: &Presence) -> Value {
    json!({
        "agentId": agent_id,
        "agentName": presence.agent_name,
        "ownerId": null,
        "lastSeenAt": presence.last_seen_at,
    })
}

impl Runners {
    /// Marks the agent online and returns the runner record it acts as.
    pub fn beat(&self, agent: &Actor) -> Value {
        let presence = Presence {
            agent_name: agent.name.clone(),
            seen: Instant::now(),
            last_seen_at: broadcast_store::now(),
        };
        let runner = record(&agent.id, &presence);
        self.0.lock().unwrap().insert(agent.id.clone(), presence);
        runner
    }

    pub fn online(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, presence)| presence.seen.elapsed() < PRESENCE_WINDOW)
            .map(|(agent_id, presence)| record(agent_id, presence))
            .collect()
    }
}
