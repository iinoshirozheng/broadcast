//! `injectTarget` / `injectAll`: mount the Broadcast userscript in each Codex window and keep it
//! registered for reloads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::cdp::{Browser, Event, Session};
use crate::host::{Bridge, Context};
use crate::process::is_codex_target;

const USER_SCRIPT: &str = include_str!("../assets/broadcast.user.js");
const SOURCE_HASH: &str = "__BROADCAST_SOURCE_HASH__";
const SCRIPT_IDENTIFIER: &str = "__BROADCAST_SCRIPT_IDENTIFIER__";

/// `currentInjectionSource`.
pub struct Source {
    pub hash: String,
    pub text: String,
}

impl Source {
    pub fn new(context: &Context) -> Self {
        let runtime = format!(
            "window.__BROADCAST_MANAGED_ORIGIN__ = {};\nwindow.__BROADCAST_HOST_CAPABILITY__ = {};\nwindow.__BROADCAST_URL__ = {};\n{USER_SCRIPT}",
            json!(context.origin),
            json!(context.capability),
            json!(context.page_url),
        );
        let hash: String = Sha256::digest(runtime.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let text = format!(
            "window[{}] = {};\n{runtime}",
            json!(SOURCE_HASH),
            json!(hash)
        );
        Self { hash, text }
    }
}

/// A Codex window the userscript is mounted in, kept attached for its host bridge.
pub struct Injected {
    pub bridge: Arc<Bridge>,
    events: JoinHandle<()>,
}

impl Injected {
    pub fn is_closed(&self) -> bool {
        self.bridge.session.is_closed()
    }

    pub fn close(&self) {
        self.bridge
            .context()
            .routes
            .unregister(&self.bridge.session);
        self.events.abort();
        self.bridge.session.close();
    }
}

async fn evaluate(session: &Session, expression: &str) -> Result<Value> {
    session
        .send(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .await
}

/// `readInjectionStatus`.
async fn status(session: &Session) -> Result<Value> {
    let expression = format!(
        r#"({{
      version: window.__broadcastInjection__?.version || null,
      sourceHash: window.__broadcastInjection__?.sourceHash || null,
      scriptIdentifier: window[{}] || null,
      entryMounted: Boolean(document.getElementById("broadcast-entry")),
      pageMounted: Boolean(document.getElementById("broadcast-page")),
      pageVisible: document.getElementById("broadcast-page")?.hidden === false,
      frameReady: window.__broadcastInjection__?.ready === true,
      frameUrl: document.getElementById("broadcast-frame")?.src || null
    }})"#,
        json!(SCRIPT_IDENTIFIER)
    );
    Ok(evaluate(session, &expression).await?["result"]["value"].clone())
}

/// `waitForInjectionStatus`.
async fn wait_for_status(
    session: &Session,
    open: bool,
    hash: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut current = status(session).await?;
    let pending = |status: &Value| {
        status["sourceHash"] != hash
            || status["entryMounted"] != true
            || (open
                && (status["pageVisible"] != true
                    || !truthy(&status["frameUrl"])
                    || status["frameReady"] != true))
    };
    while tokio::time::Instant::now() < deadline && pending(&current) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        current = status(session).await?;
    }
    Ok(current)
}

fn truthy(value: &Value) -> bool {
    value.as_str().is_some_and(|value| !value.is_empty())
}

fn frame_tree_contains(tree: &Value, url: &str) -> bool {
    tree["frame"]["url"] == url
        || tree["childFrames"]
            .as_array()
            .is_some_and(|children| children.iter().any(|child| frame_tree_contains(child, url)))
}

/// `waitForFrame`.
async fn wait_for_frame(session: &Session, url: &str, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let (targets, tree) = tokio::try_join!(
            session.send("Target.getTargets", json!({})),
            session.send("Page.getFrameTree", json!({}))
        )?;
        let iframe = targets["targetInfos"].as_array().is_some_and(|targets| {
            targets
                .iter()
                .any(|target| target["type"] == "iframe" && target["url"] == url)
        });
        if iframe || frame_tree_contains(&tree["frameTree"], url) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}

/// `evaluateInjectionSource`.
async fn run_source(session: &Session, source: &Source) -> Result<()> {
    let evaluation = session
        .send(
            "Runtime.evaluate",
            json!({ "expression": source.text, "awaitPromise": true, "returnByValue": true }),
        )
        .await?;
    if let Some(description) = crate::cdp::exception(&evaluation) {
        bail!(if description.is_empty() {
            "Broadcast injection failed".to_owned()
        } else {
            description
        });
    }
    Ok(())
}

/// `registerInjectionSource`.
async fn register(session: &Session, source: &Source) -> Result<String> {
    let registration = session
        .send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": format!("{}\n//# sourceURL=broadcast.user.js", source.text) }),
        )
        .await?;
    Ok(registration["identifier"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// `publishInjectionScriptIdentifier`.
async fn publish(session: &Session, identifier: &str) -> Result<()> {
    evaluate(
        session,
        &format!(
            "window[{}] = {}",
            json!(SCRIPT_IDENTIFIER),
            json!(identifier)
        ),
    )
    .await?;
    Ok(())
}

const OPEN: &str = "window.__broadcastInjection__?.open()";

/// The window's events: binding calls always, reloads once the source is registered.
fn watch(
    bridge: Arc<Bridge>,
    mut events: mpsc::UnboundedReceiver<Event>,
    identifier: Arc<Mutex<Option<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event.method.as_str() {
                "Runtime.bindingCalled" => bridge.binding(&event),
                "Page.loadEventFired" => {
                    let Some(identifier) = identifier
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    else {
                        continue;
                    };
                    let bridge = Arc::clone(&bridge);
                    tokio::spawn(async move {
                        let reloaded = async {
                            bridge.install().await?;
                            publish(&bridge.session, &identifier).await?;
                            bridge.heartbeat().await
                        };
                        if let Err(error) = reloaded.await {
                            eprintln!("Broadcast reload handling failed: {error:#}");
                        }
                    });
                }
                _ => {}
            }
        }
        bridge.context().routes.unregister(&bridge.session);
    })
}

/// `injectTarget` for a kept-alive window.
async fn inject_target(
    browser: &Browser,
    target: &Value,
    context: &Arc<Context>,
    source: &Source,
    open: bool,
    attach_existing: bool,
) -> Result<(Value, Injected)> {
    let (session, events) = browser
        .attach(target["targetId"].as_str().unwrap_or_default())
        .await?;
    let bridge = Bridge::new(session.clone(), Arc::clone(context));
    let identifier = Arc::new(Mutex::new(None));
    let injected = Injected {
        bridge: Arc::clone(&bridge),
        events: watch(Arc::clone(&bridge), events, Arc::clone(&identifier)),
    };
    match mount(&bridge, source, open, attach_existing, &identifier).await {
        Ok(result) => Ok((result, injected)),
        Err(error) => {
            injected.close();
            Err(error)
        }
    }
}

async fn mount(
    bridge: &Bridge,
    source: &Source,
    open: bool,
    attach_existing: bool,
    identifier: &Mutex<Option<String>>,
) -> Result<Value> {
    let session = &bridge.session;
    session.send("Page.enable", json!({})).await?;
    session
        .send("Page.setBypassCSP", json!({ "enabled": true }))
        .await?;
    session.send("Runtime.enable", json!({})).await?;
    bridge.install().await?;
    let remain_open = if attach_existing {
        let current = status(session).await?;
        if let Some(previous) = current["scriptIdentifier"].as_str() {
            let _ = session
                .send(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({ "identifier": previous }),
                )
                .await;
        }
        let registered = register(session, source).await?;
        run_source(session, source).await?;
        publish(session, &registered).await?;
        let remain_open = current["pageVisible"] == true;
        if current["sourceHash"] != source.hash.as_str() && remain_open {
            evaluate(session, OPEN).await?;
        }
        *identifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registered);
        bridge.heartbeat().await?;
        if open && !remain_open {
            evaluate(session, OPEN).await?;
        }
        open || remain_open
    } else {
        let registered = register(session, source).await?;
        *identifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registered.clone());
        run_source(session, source).await?;
        publish(session, &registered).await?;
        bridge.heartbeat().await?;
        if open {
            wait_for_status(session, false, &source.hash, Duration::from_secs(60)).await?;
            evaluate(session, "(() => {\n          const broadcast = window.__broadcastInjection__;\n          broadcast?.close();\n          broadcast?.open();\n        })()").await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        open
    };
    let status =
        wait_for_status(session, remain_open, &source.hash, Duration::from_secs(15)).await?;
    let frame_loaded = match status["frameUrl"].as_str().filter(|url| !url.is_empty()) {
        Some(url) => wait_for_frame(session, url, Duration::from_secs(15)).await?,
        None => false,
    };
    if remain_open && (status["frameReady"] != true || !frame_loaded) {
        bail!("Broadcast frame did not report ready in the Codex renderer");
    }
    let mut result = status.as_object().cloned().unwrap_or_default();
    result.insert("cspBypassed".into(), true.into());
    result.insert("frameLoaded".into(), frame_loaded.into());
    Ok(Value::Object(result))
}

/// `injectAll` (kept alive): drop windows that went away, mount the new ones; only the first
/// window of an empty set opens the page.
pub async fn inject_all(
    browser: &Browser,
    context: &Arc<Context>,
    source: &Source,
    open: bool,
    injected: &mut HashMap<String, Injected>,
    attach_existing: bool,
) -> Result<Vec<Value>> {
    let targets: Vec<Value> = browser
        .targets()
        .await?
        .into_iter()
        .filter(is_codex_target)
        .collect();
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    injected.retain(|id, window| {
        let keep = !window.is_closed()
            && targets
                .iter()
                .any(|target| target["targetId"] == id.as_str());
        if !keep {
            window.close();
        }
        keep
    });
    let mut results = Vec::new();
    for target in &targets {
        let id = target["targetId"].as_str().unwrap_or_default();
        if injected.contains_key(id) {
            continue;
        }
        let first = injected.is_empty() && results.is_empty();
        let (result, window) = inject_target(
            browser,
            target,
            context,
            source,
            open && first,
            attach_existing,
        )
        .await?;
        injected.insert(id.to_owned(), window);
        let mut entry = Map::from_iter([
            ("targetId".to_owned(), json!(id)),
            ("title".to_owned(), target["title"].clone()),
            ("url".to_owned(), target["url"].clone()),
        ]);
        entry.extend(result.as_object().cloned().unwrap_or_default());
        results.push(Value::Object(entry));
    }
    Ok(results)
}
