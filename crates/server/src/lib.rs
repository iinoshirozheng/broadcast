//! HTTP API and web host for the Broadcast board.

mod actor;
mod client_storage;
mod config;
mod events;
mod files;
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
use tower::Layer;

pub use config::Config;
use events::EventHub;

pub(crate) struct App {
    store: Store,
    events: EventHub,
    client_storage: client_storage::ClientStorage,
    jira: jira::Jira,
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
}

impl Server {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.data_dir.join("broadcast.sqlite"))?;
        let data = &config.data_dir;
        let app = App {
            store,
            events: EventHub::new(),
            client_storage: client_storage::ClientStorage::new(data.join("client-storage.json")),
            jira: jira::Jira::new(data.join("jira-connection.json"))?,
            runners: runners::Runners::default(),
            config,
        };
        Ok(Self { app: Arc::new(app) })
    }

    /// Serves until `shutdown` resolves, then ends open event streams so graceful shutdown can finish.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let Self { app } = self;
        let routes = routes::router(Arc::clone(&app));
        let service = middleware::from_fn_with_state(Arc::clone(&app), trust::guard).layer(routes);
        let stopping = Arc::clone(&app);
        axum::serve(
            listener,
            ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(service),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            stopping.events.close();
        })
        .await
    }
}
