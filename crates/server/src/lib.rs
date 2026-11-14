//! HTTP API and web host for the Broadcast board, ported from `server/app.mjs`.

mod actor;
mod config;
mod events;
mod http;
mod routes;
mod static_files;
mod trust;

use std::sync::Arc;

use axum::Router;
use broadcast_domain::Result;
use broadcast_store::Store;

pub use config::Config;
use events::EventHub;

pub(crate) struct App {
    store: Store,
    events: EventHub,
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
        Ok(Self { app: Arc::new(App { store, events: EventHub::new(), config }) })
    }

    pub fn router(&self) -> Router {
        routes::router(Arc::clone(&self.app))
    }

    /// Ends every open event stream so graceful shutdown can finish.
    pub fn close(&self) {
        self.app.events.close();
    }
}
