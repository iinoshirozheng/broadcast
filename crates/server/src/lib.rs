//! HTTP API and web host for the Broadcast board, ported from `server/app.mjs`.

mod actor;
mod client_storage;
mod cloud;
mod codex;
mod config;
mod events;
mod files;
mod host;
mod http;
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

pub use config::Config;
use events::EventHub;

pub(crate) struct App {
    store: Store,
    events: EventHub,
    host: host::Host,
    cloud: cloud::config::ConfigStore,
    client_storage: client_storage::ClientStorage,
    codex_sessions: codex::progress::Sessions,
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
}

impl Server {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.data_dir.join("broadcast.sqlite"))?;
        let data = &config.data_dir;
        let app = App {
            store,
            events: EventHub::new(),
            host: host::Host::default(),
            cloud: cloud::config::ConfigStore::new(data.join("cloud-companion.json")),
            client_storage: client_storage::ClientStorage::new(data.join("client-storage.json")),
            codex_sessions: codex::progress::Sessions::new(&config.codex_home),
            config,
        };
        Ok(Self { app: Arc::new(app) })
    }

    /// Serves until `shutdown` resolves, then ends open event streams so graceful shutdown can finish.
    /// The trust gate wraps the router because it rewrites launcher-prefixed paths before routing.
    pub async fn serve(self, listener: TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let app = self.app;
        let service = middleware::from_fn_with_state(Arc::clone(&app), trust::guard).layer(routes::router(Arc::clone(&app)));
        axum::serve(listener, ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(service))
            .with_graceful_shutdown(async move {
                shutdown.await;
                app.events.close();
            })
            .await
    }
}
