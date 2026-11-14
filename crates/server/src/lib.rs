//! HTTP API and web host for the Broadcast board, ported from `server/app.mjs`.

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
mod static_files;
mod trust;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{ServiceExt, extract::Request, middleware};
use broadcast_domain::Result;
use broadcast_store::Store;
use tokio::net::TcpListener;
use tower::Layer;

pub use ai::exec::{TURN_OWNER_ARG, turn_owner};
pub use config::Config;
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
    notifications: tokio::sync::mpsc::UnboundedReceiver<ai::app_server::Notification>,
}

impl Server {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.data_dir.join("broadcast.sqlite"))?;
        let data = &config.data_dir;
        let (ai, notifications) = ai::AiChat::new(config.codex_executable.clone(), config.codex_home.clone());
        let summary = ai::summary::ProjectSummaries::new(config.codex_executable.clone(), std::env::current_dir()?);
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
            config,
        };
        Ok(Self { app: Arc::new(app), notifications })
    }

    /// Serves until `shutdown` resolves, then ends open event streams so graceful shutdown can finish.
    /// The trust gate wraps the router because it rewrites launcher-prefixed paths before routing;
    /// inside it, cloud mode diverts business APIs to the cloud board.
    pub async fn serve(self, listener: TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let Self { app, notifications } = self;
        tokio::spawn(ai::AiChat::consume(Arc::clone(&app), notifications));
        ai::summary::ProjectSummaries::start(&app);
        let routes = middleware::from_fn_with_state(Arc::clone(&app), cloud::proxy::route).layer(routes::router(Arc::clone(&app)));
        let service = middleware::from_fn_with_state(Arc::clone(&app), trust::guard).layer(routes);
        let stopping = Arc::clone(&app);
        let served = axum::serve(listener, ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(service))
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
