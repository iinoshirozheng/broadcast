use broadcast_server::{Config, Server};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let listener = TcpListener::bind((config.host, config.port)).await?;
    let port = listener.local_addr()?.port();
    let server = Server::open(config)?;
    let shutdown = shutdown_signal()?;
    println!("Broadcast listening on http://127.0.0.1:{port}");
    axum::serve(listener, server.router())
        .with_graceful_shutdown(async move {
            shutdown.await;
            server.close();
        })
        .await?;
    Ok(())
}

#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()>> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}
