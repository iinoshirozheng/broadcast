//! The kept-alive launcher (`codex-injector --launch --watch`): start or reuse Codex, keep the
//! board injected in every window, carry the server's App Server requests to a window and stop
//! what it started.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::cdp::Browser;
use crate::host::{self, Context, Routes};
use crate::inject::{self, Injected, Source};
use crate::process::{self, App, Record, SpawnFailure};
use crate::{HostNotification, HostRequest, profile};

/// How the launcher reaches Codex's DevTools.
#[derive(Clone, Copy)]
pub enum Cdp {
    /// `--remote-debugging-port` through LaunchServices, reusing a running debuggable Codex.
    Port(u16),
    /// `--remote-debugging-pipe` on a child the launcher spawns (Linux).
    Pipe,
}

pub struct Options {
    pub app_path: PathBuf,
    pub cdp: Cdp,
    /// Broadcast's own Codex profile (`BROADCAST_CODEX_PROFILE`).
    pub profile: PathBuf,
    /// The profile whose in-app browser sign-ins are imported once (`BROADCAST_CODEX_SOURCE_PROFILE`).
    pub source_profile: Option<PathBuf>,
    pub data_dir: PathBuf,
    /// The Broadcast server's loopback port.
    pub port: u16,
    /// The instance token, also the startup token the frame reports back.
    pub token: String,
    pub secret: String,
    /// Open the board in the first window once it is injected.
    pub open: bool,
}

/// What the desktop shows for the launcher (`emitLauncherEvent`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    WaitingForCodex,
    Injected,
    OpenSignalQueued,
    OpenedInExistingCodex,
}

struct Signals {
    open_requested: AtomicU64,
    stop: watch::Sender<bool>,
    events: mpsc::UnboundedSender<Event>,
}

impl Signals {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// Controls a running launcher; clones control the same one.
#[derive(Clone)]
pub struct Launcher(Arc<Signals>);

impl Launcher {
    /// Starts the launcher. The handle resolves once it stopped and cleaned up, with the error
    /// that ended it early, if any.
    pub fn start(
        options: Options,
        requests: mpsc::UnboundedReceiver<HostRequest>,
        notifications: mpsc::UnboundedSender<HostNotification>,
        events: mpsc::UnboundedSender<Event>,
    ) -> (Self, JoinHandle<Result<()>>) {
        let (stop, _) = watch::channel(false);
        let signals = Arc::new(Signals { open_requested: AtomicU64::new(u64::from(options.open)), stop, events });
        let origin = format!("http://127.0.0.1:{}", options.port);
        let base_url = format!("{origin}/{}", host::encode_component(&options.token));
        let context = Arc::new(Context {
            http: reqwest::Client::new(),
            data_dir: options.data_dir.clone(),
            page_url: format!("{base_url}/?host=codex"),
            origin,
            base_url,
            secret: options.secret.clone(),
            capability: uuid::Uuid::new_v4().to_string(),
            startup_token: options.token.clone(),
            routes: Routes::default(),
            notifications,
        });
        let source = Source::new(&context);
        let runner = Runner {
            app: App { path: options.app_path.clone(), profile: options.profile.clone() },
            options,
            context,
            source,
            signals: Arc::clone(&signals),
            browser: None,
            child: None,
            managed: None,
            app_pid: None,
            build: None,
            exited: None,
            native: false,
            idle: false,
            attach_existing: false,
            opened: 0,
            injected: HashMap::new(),
        };
        let task = tokio::spawn(runner.serve(requests));
        (Self(signals), task)
    }

    /// `queueBroadcastOpen`: open the board on the next watch tick.
    pub fn open(&self) {
        self.0.open_requested.fetch_add(1, Ordering::SeqCst);
        println!("{}", json!({ "openBroadcastSignalQueued": true }));
        self.0.emit(Event::OpenSignalQueued);
    }

    /// `requestStop`: the launcher cleans up and its handle resolves.
    pub fn stop(&self) {
        self.0.stop.send_replace(true);
    }
}

async fn stopped(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// `handleCodexAppServerRequest`: each request waits for a window that can carry it.
async fn serve_requests(context: Arc<Context>, mut requests: mpsc::UnboundedReceiver<HostRequest>, stop: watch::Receiver<bool>) {
    loop {
        let request = tokio::select! {
            request = requests.recv() => request,
            () = stopped(stop.clone()) => None,
        };
        let Some(request) = request else { return };
        let (context, stop) = (Arc::clone(&context), stop.clone());
        tokio::spawn(async move {
            let session = tokio::select! {
                session = context.routes.session_for(&request.host_id) => Some(session),
                () = stopped(stop) => None,
            };
            let result = match session {
                Some(session) => host::app_server_request(&session, &request.host_id, &request.method, request.params, host::APP_SERVER_TIMEOUT).await,
                None => Err(anyhow::anyhow!("Codex renderer stopped before the remote request was sent")),
            };
            let _ = request.reply.send(result);
        });
    }
}

struct Runner {
    options: Options,
    app: App,
    context: Arc<Context>,
    source: Source,
    signals: Arc<Signals>,
    browser: Option<Browser>,
    /// The pipe-mode child (`codexProcess`).
    child: Option<tokio::process::Child>,
    /// The instance launched with Broadcast's profile (`managedCodex`).
    managed: Option<Record>,
    app_pid: Option<u32>,
    /// The app build the managed instance started from, to tell an update restart from a quit.
    build: Option<String>,
    exited: Option<(u32, String)>,
    /// Codex runs without a debugging port: the board opens through its deep link instead.
    native: bool,
    /// Codex is gone; the launcher waits for an open request to start it again.
    idle: bool,
    attach_existing: bool,
    opened: u64,
    injected: HashMap<String, Injected>,
}

impl Runner {
    async fn serve(mut self, requests: mpsc::UnboundedReceiver<HostRequest>) -> Result<()> {
        let stop = self.signals.stop.subscribe();
        tokio::spawn(serve_requests(Arc::clone(&self.context), requests, stop.clone()));
        let result = tokio::select! {
            result = self.run() => result,
            () = stopped(stop) => Ok(()),
        };
        // Nothing waits on a route once the launcher is gone.
        self.signals.stop.send_replace(true);
        let cleanup = self.cleanup().await;
        if let Err(error) = &cleanup {
            eprintln!("Cleanup failed: {error:#}");
        }
        result.and(cleanup)
    }

    fn http(&self) -> &reqwest::Client {
        &self.context.http
    }

    fn requested(&self) -> u64 {
        self.signals.open_requested.load(Ordering::SeqCst)
    }

    fn open_pending(&self) -> bool {
        self.opened < self.requested()
    }

    fn mark_opened(&mut self, generation: u64) {
        self.opened = self.opened.max(generation);
    }

    fn waiting(&self, message: &str) {
        eprintln!("{message}");
        self.signals.emit(Event::WaitingForCodex);
    }

    fn close_windows(&mut self) {
        for (_, window) in self.injected.drain() {
            window.close();
        }
    }

    fn close_browser(&mut self) {
        if let Some(browser) = self.browser.take() {
            browser.close();
        }
    }

    /// `requestBroadcastOpen`.
    async fn request_open(&mut self) -> bool {
        let generation = self.requested();
        if generation <= self.opened {
            return true;
        }
        let window = self.injected.values().next().map(|window| window.bridge.session.clone());
        if !self.native && window.is_none() {
            return false;
        }
        let opened = async {
            if self.native {
                let mut link = url::Url::parse("codex://threads/new")?;
                link.query_pairs_mut().append_pair("browserUrl", &self.context.page_url);
                host::open(link.as_str()).await?;
                return anyhow::Ok(true);
            }
            let Some(session) = window else { return Ok(false) };
            let evaluation = session
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": "(() => {\n          const broadcast = window.__broadcastInjection__;\n          if (typeof broadcast?.open !== \"function\") return false;\n          broadcast.open();\n          return true;\n        })()",
                        "returnByValue": true,
                    }),
                )
                .await?;
            if evaluation["result"]["value"] != true {
                bail!("Broadcast injection is not ready");
            }
            session.send("Page.bringToFront", json!({})).await?;
            if let Some(pid) = self.app_pid {
                process::activate(pid).await?;
            }
            Ok(false)
        };
        match opened.await {
            Ok(native) => {
                self.mark_opened(generation);
                if native {
                    println!("{}", json!({ "openedBroadcastInExistingCodex": true }));
                    self.signals.emit(Event::OpenedInExistingCodex);
                }
                true
            }
            Err(error) => {
                eprintln!("Waiting to open Broadcast: {error:#}");
                false
            }
        }
    }

    /// `startManagedCodex`: reuse a debuggable Codex, otherwise launch one with Broadcast's
    /// profile. `false` when Codex runs but cannot be debugged.
    async fn start_managed(&mut self) -> Result<bool> {
        if let Cdp::Port(_) = self.options.cdp {
            let running = self.app.processes().await?;
            let mut debugging = false;
            for record in &running {
                let Some(port) = process::debugging_port(record) else { continue };
                debugging = true;
                if !process::reachable(self.http(), port).await || !process::has_codex_targets(self.http(), port).await {
                    continue;
                }
                self.browser = Some(Browser::connect(self.http(), port).await?);
                self.app_pid = Some(record.pid);
                self.attach_existing = true;
                println!("{}", json!({ "reusedCodexPid": record.pid, "cdpPort": port }));
                return Ok(true);
            }
            if !running.is_empty() {
                self.native = !debugging;
                return Ok(false);
            }
        }
        profile::import(self.options.source_profile.as_deref(), &self.options.profile).await?;
        match self.options.cdp {
            Cdp::Pipe => self.launch_with_pipe().await?,
            Cdp::Port(port) => {
                let managed = self.app.launch(self.http(), port).await?;
                self.app_pid = Some(managed.pid);
                self.managed = Some(managed);
                self.build = self.app.bundle_build().await;
                process::wait_until_reachable(self.http(), port, Duration::from_secs(30)).await?;
                self.browser = Some(Browser::connect(self.http(), port).await?);
            }
        }
        Ok(true)
    }

    /// `launchCodexWithPipe`.
    async fn launch_with_pipe(&mut self) -> Result<()> {
        let (child, pipe) = self.app.spawn_with_pipe()?;
        let pid = child.id();
        self.child = Some(child);
        match Browser::pipe(pipe).await {
            Ok(browser) => {
                self.browser = Some(browser);
                Ok(())
            }
            Err(error) => {
                self.child = None;
                if let Some(pid) = pid {
                    let _ = process::signal(pid, nix::sys::signal::Signal::SIGTERM);
                }
                Err(error)
            }
        }
    }

    /// `recoverManagedCodexAfterUpdate`: when the managed instance quit because the app updated
    /// itself and the updater relaunched an ordinary instance, swap that one for a managed one.
    async fn recover_after_update(&mut self) -> Result<bool> {
        let Some((pid, build)) = self.exited.clone() else { return Ok(false) };
        let Some((_, replacement)) = self.app.update_replacement(pid, Some(&build)).await? else { return Ok(false) };
        self.exited = None;
        let restarted = async {
            process::stop(&replacement).await?;
            self.managed = None;
            self.build = None;
            self.app_pid = None;
            self.native = false;
            if !self.start_managed().await? {
                bail!("Managed Codex did not restart after the app update");
            }
            Ok(())
        };
        match restarted.await {
            Ok(()) => {
                self.idle = false;
                let port = match self.options.cdp {
                    Cdp::Port(port) => Some(port),
                    Cdp::Pipe => None,
                };
                println!(
                    "{}",
                    json!({
                        "restartedCodexAfterUpdate": true,
                        "previousPid": pid,
                        "replacementPid": replacement.pid,
                        "managedPid": self.app_pid,
                        "previousBuild": build,
                        "build": self.build,
                        "cdpPort": port,
                    })
                );
            }
            Err(error) => {
                self.close_browser();
                self.managed = None;
                self.build = None;
                self.app_pid = None;
                self.native = false;
                self.idle = true;
                self.waiting(&format!("Waiting for Codex after update recovery failed: {error:#}"));
            }
        }
        Ok(true)
    }

    async fn inject(&mut self, open: bool) -> Result<Vec<Value>> {
        let Some(browser) = &self.browser else { bail!("Codex DevTools are not connected") };
        inject::inject_all(browser, &self.context, &self.source, open, &mut self.injected, self.attach_existing).await
    }

    fn report_injected(&self, results: &[Value]) {
        if !results.is_empty() {
            println!("{}", json!({ "injected": results }));
            self.signals.emit(Event::Injected);
        }
    }

    async fn run(&mut self) -> Result<()> {
        let reachable = match self.options.cdp {
            Cdp::Port(port) => process::reachable(self.http(), port).await,
            Cdp::Pipe => false,
        };
        if reachable {
            let Cdp::Port(port) = self.options.cdp else { unreachable!() };
            let running = self.app.processes().await?.into_iter().find(|record| process::debugging_port(record) == Some(port));
            let Some(running) = running else { bail!("Codex CDP port {port} belongs to another process") };
            if !process::has_codex_targets(self.http(), port).await {
                bail!("Codex CDP port {port} belongs to another process");
            }
            self.managed = self.app.managed().await?.filter(|record| record.pid == running.pid);
            self.app_pid = Some(running.pid);
            if self.managed.is_some() {
                self.build = self.app.bundle_build().await;
            } else {
                self.attach_existing = true;
                println!("{}", json!({ "reusedCodexPid": running.pid, "cdpPort": port }));
            }
            self.browser = Some(Browser::connect(self.http(), port).await?);
        } else {
            let generation = self.requested();
            match self.start_managed().await {
                Ok(started) => self.idle = !started && !self.native,
                Err(error) if error.is::<SpawnFailure>() => {
                    self.mark_opened(generation);
                    self.idle = true;
                    self.waiting(&format!("Waiting for Codex launch: {error:#}"));
                }
                Err(error) => return Err(error),
            }
        }

        let first = self.requested();
        let open_first = first > self.opened;
        if !self.idle && !self.native {
            match self.inject(open_first).await {
                Ok(results) if !results.is_empty() => {
                    if open_first {
                        self.mark_opened(first);
                        if let Some(pid) = self.app_pid {
                            process::activate(pid).await?;
                        }
                    }
                    self.report_injected(&results);
                }
                Ok(_) => {}
                Err(error) => self.waiting(&format!("Waiting for Codex renderer: {error:#}")),
            }
        }
        if self.open_pending() {
            self.request_open().await;
        }

        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            for window in self.injected.values() {
                let _ = window.bridge.heartbeat().await;
            }
            if self.native {
                if self.app.processes().await?.is_empty() {
                    self.native = false;
                    self.idle = true;
                    self.waiting("Waiting for Codex after exit; open Broadcast again to restart it.");
                    continue;
                }
                if self.open_pending() {
                    self.request_open().await;
                }
                continue;
            }
            if self.idle {
                if self.recover_after_update().await? {
                    if self.idle {
                        continue;
                    }
                } else {
                    if !self.open_pending() {
                        continue;
                    }
                    let generation = self.requested();
                    match self.start_managed().await {
                        Ok(false) => {
                            if self.native {
                                self.request_open().await;
                            }
                            continue;
                        }
                        Ok(true) => {
                            self.exited = None;
                            self.idle = false;
                        }
                        Err(error) => {
                            if error.is::<SpawnFailure>() {
                                self.mark_opened(generation);
                            }
                            eprintln!("Waiting to restart Codex: {error:#}");
                            continue;
                        }
                    }
                }
            }
            match self.inject(false).await {
                Ok(results) => {
                    self.report_injected(&results);
                    if self.open_pending() {
                        self.request_open().await;
                    }
                }
                Err(error) => self.recover(error).await?,
            }
        }
    }

    /// The watch loop's failure path: tell a Codex that quit from a renderer that is not ready.
    async fn recover(&mut self, error: anyhow::Error) -> Result<()> {
        let browser_closed = self.browser.as_ref().is_none_or(Browser::is_closed);
        if let Cdp::Pipe = self.options.cdp
            && browser_closed
        {
            let exit = match self.child.as_mut() {
                Some(child) => match child.try_wait()? {
                    Some(status) => Some(status),
                    None => tokio::time::timeout(Duration::from_millis(250), child.wait()).await.ok().transpose()?,
                },
                None => None,
            };
            match exit {
                Some(status) => return self.pipe_exited(status).await,
                None => return Err(error),
            }
        }
        let exited = match self.options.cdp {
            Cdp::Pipe => false,
            Cdp::Port(_) => match self.app_pid {
                Some(pid) => !self.app.processes().await?.iter().any(|record| record.pid == pid),
                None => false,
            },
        };
        if !exited {
            if let (Cdp::Port(port), true) = (self.options.cdp, browser_closed) {
                // The single DevTools connection dropped while Codex still runs: reconnect.
                self.close_windows();
                self.browser = Browser::connect(self.http(), port).await.ok();
            }
            self.waiting(&format!("Waiting for Codex renderer: {error:#}"));
            return Ok(());
        }
        let pid = self.app_pid;
        self.exited = match (&self.managed, &self.build) {
            (Some(managed), Some(build)) if Some(managed.pid) == pid => Some((managed.pid, build.clone())),
            _ => None,
        };
        self.close_windows();
        self.close_browser();
        if self.recover_after_update().await? {
            return Ok(());
        }
        self.managed = None;
        self.build = None;
        self.app_pid = None;
        self.idle = true;
        self.waiting("Waiting for Codex after exit; open Broadcast again to restart it.");
        Ok(())
    }

    /// The pipe child exited: wait after a normal quit, restart after a crash.
    async fn pipe_exited(&mut self, status: std::process::ExitStatus) -> Result<()> {
        self.close_windows();
        self.close_browser();
        self.child = None;
        if status.success() {
            self.idle = true;
            self.waiting("Waiting for Codex after normal exit; open Broadcast again to restart it.");
            return Ok(());
        }
        eprintln!("Codex exited unexpectedly; restarting it for the broadcast launcher.");
        let generation = self.requested();
        match self.start_managed().await {
            Ok(_) => {
                if self.options.open {
                    self.signals.open_requested.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(error) => {
                if error.is::<SpawnFailure>() {
                    self.mark_opened(generation);
                    self.idle = true;
                }
                eprintln!("Waiting to restart Codex: {error:#}");
            }
        }
        Ok(())
    }

    /// `cleanup`: detach from every window, then stop the Codex the launcher started.
    async fn cleanup(&mut self) -> Result<()> {
        self.close_windows();
        self.close_browser();
        let child = self.child.take();
        let mut managed = self.managed.take();
        if managed.is_none()
            && let Cdp::Port(port) = self.options.cdp
        {
            managed = self.app.managed().await?.filter(|record| process::uses_port(record, port));
        }
        if let Some(managed) = managed {
            process::stop(&managed).await?;
        }
        if let Some(mut child) = child
            && child.try_wait()?.is_none()
        {
            if let Some(pid) = child.id() {
                process::signal(pid, nix::sys::signal::Signal::SIGTERM)?;
            }
            if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            }
        }
        Ok(())
    }
}
