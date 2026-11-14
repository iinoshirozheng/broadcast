use broadcast_server::{Config, Server, TURN_OWNER_ARG, turn_owner};
use tokio::net::TcpListener;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|arg| arg == TURN_OWNER_ARG) {
        turn_owner(args);
    }
    tokio::runtime::Runtime::new()?.block_on(serve())
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let listener = TcpListener::bind((config.host, config.port)).await?;
    let port = listener.local_addr()?.port();
    let lan = config.host.is_unspecified();
    let server = Server::open(config)?;
    let shutdown = shutdown_signal()?;
    println!("Broadcast listening on http://127.0.0.1:{port}");
    if lan {
        print_lan_addresses(port);
    }
    server.serve(listener, shutdown).await?;
    Ok(())
}

/// Every external IPv4 address, once each.
fn print_lan_addresses(port: u16) {
    let mut seen = Vec::new();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let std::net::IpAddr::V4(address) = interface.ip()
            && !interface.is_loopback()
            && !seen.contains(&address)
        {
            seen.push(address);
            println!("Broadcast available on LAN at http://{address}:{port}");
        }
    }
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
