//! HTTP API and web host for the Broadcast board.

mod actor;
mod ai;
mod client_storage;
mod cloud;
mod codex;
mod config;
mod events;
mod files;
mod host;
mod http;
mod jira;
mod routes;
mod runners;
mod static_files;
mod trust;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{ServiceExt, extract::Request, middleware};
use broadcast_domain::Result;
use broadcast_store::Store;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower::Layer;

pub use ai::exec::{TURN_OWNER_ARG, turn_owner};
pub use config::{Config, Launcher};
use events::EventHub;

pub(crate) struct App {
    store: Store,
    events: EventHub,
    host: host::Host,
    cloud: cloud::config::ConfigStore,
    cloud_proxy: cloud::proxy::Proxy,
    client_storage: client_storage::ClientStorage,
    jira: jira::Jira,
    codex_sessions: codex::progress::Sessions,
    ai: ai::AiChat,
    summary: ai::summary::ProjectSummaries,
    runners: runners::Runners,
    config: Config,
}

impl App {
    /// Runs a store call on the blocking pool.
    async fn store<T, F>(self: &Arc<Self>, call: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
    {
        let app = Arc::clone(self);
        tokio::task::spawn_blocking(move || call(&app.store)).await?
    }
}

pub struct Server {
    app: Arc<App>,
    notifications: (
        ai::app_server::Notifications,
        mpsc::UnboundedReceiver<ai::app_server::Notification>,
    ),
    host_notifications: mpsc::UnboundedReceiver<broadcast_codex::HostNotification>,
    host_bridge: Option<HostBridge>,
}

/// The ends of the Codex host bridge the desktop launcher serves: the server's requests to remote
/// Codex hosts and the App Server notifications those hosts forward.
pub struct HostBridge {
    pub requests: mpsc::UnboundedReceiver<broadcast_codex::HostRequest>,
    pub notifications: mpsc::UnboundedSender<broadcast_codex::HostNotification>,
}

impl Server {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.data_dir.join("broadcast.sqlite"))?;
        let data = &config.data_dir;
        let notifications = mpsc::unbounded_channel();
        let (host_requests, requests) = mpsc::unbounded_channel();
        let (host_notifier, host_notifications) = mpsc::unbounded_channel();
        let ai = ai::AiChat::new(
            config.codex_executable.clone(),
            config.codex_home.clone(),
            notifications.0.clone(),
            host_requests,
        );
        let summary = ai::summary::ProjectSummaries::new(
            config.codex_executable.clone(),
            std::env::current_dir()?,
        );
        let app = App {
            store,
            events: EventHub::new(),
            host: host::Host::default(),
            cloud: cloud::config::ConfigStore::new(data.join("cloud-companion.json")),
            cloud_proxy: cloud::proxy::Proxy::new()?,
            client_storage: client_storage::ClientStorage::new(data.join("client-storage.json")),
            jira: jira::Jira::new(data.join("jira-connection.json"))?,
            codex_sessions: codex::progress::Sessions::new(&config.codex_home),
            ai,
            summary,
            runners: runners::Runners::default(),
            config,
        };
        let host_bridge = Some(HostBridge {
            requests,
            notifications: host_notifier,
        });
        Ok(Self {
            app: Arc::new(app),
            notifications,
            host_notifications,
            host_bridge,
        })
    }

    /// The host bridge for the launcher that runs beside this server; without one, requests to
    /// remote Codex hosts fail as unavailable.
    pub fn host_bridge(&mut self) -> Option<HostBridge> {
        self.host_bridge.take()
    }

    /// Serves until `shutdown` resolves, then ends open event streams so graceful shutdown can finish.
    /// The trust gate wraps the router because it rewrites launcher-prefixed paths before routing;
    /// inside it, cloud mode diverts business APIs to the cloud board.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let Self {
            app,
            notifications: (notifier, notifications),
            mut host_notifications,
            host_bridge,
        } = self;
        drop(host_bridge);
        tokio::spawn(ai::AiChat::consume(Arc::clone(&app), notifications));
        tokio::spawn(async move {
            while let Some(notification) = host_notifications.recv().await {
                let origin = ai::app_server::Origin::Host(notification.host_id);
                let _ = notifier.send(ai::app_server::Notification {
                    origin,
                    method: notification.method,
                    params: notification.params,
                });
            }
        });
        ai::summary::ProjectSummaries::start(&app);
        let routes = middleware::from_fn_with_state(Arc::clone(&app), cloud::proxy::route)
            .layer(routes::router(Arc::clone(&app)));
        let service = middleware::from_fn_with_state(Arc::clone(&app), trust::guard).layer(routes);
        let stopping = Arc::clone(&app);
        let served = axum::serve(
            listener,
            ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(service),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            stopping.events.close();
            stopping.ai.end_streams();
        })
        .await;
        app.ai.close(&app).await;
        app.summary.close().await;
        served
    }
}
