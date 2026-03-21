//! L4 TCP stream proxy.
//!
//! Provides simple bidirectional TCP proxying for non-HTTP protocols such as
//! MySQL, Redis, or any other TCP-based service. Each configured listener
//! binds on a local address and forwards all traffic to a single upstream
//! target using `tokio::io::copy_bidirectional`.

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::ProxyError;
use crate::config::StreamConfig;

/// Start TCP stream listeners for all entries in the given `StreamConfig`.
///
/// Returns a `Vec<JoinHandle<()>>` — one per listener. Each handle runs
/// indefinitely, accepting connections and proxying them.
pub async fn start_stream_listeners(
    config: &StreamConfig,
) -> Result<Vec<JoinHandle<()>>, ProxyError> {
    let mut handles = Vec::with_capacity(config.listeners.len());

    for listener_cfg in &config.listeners {
        let listen_addr = listener_cfg.listen;
        let target = listener_cfg.proxy.clone();

        let tcp_listener = TcpListener::bind(listen_addr).await?;
        info!(
            listen = %listen_addr,
            target = %target,
            "stream proxy listening"
        );

        let handle = tokio::spawn(async move {
            run_stream_listener(tcp_listener, target).await;
        });

        handles.push(handle);
    }

    Ok(handles)
}

/// Accept loop for a single stream listener.
///
/// For each incoming connection, spawns a task that connects to the upstream
/// target and copies bytes bidirectionally.
async fn run_stream_listener(listener: TcpListener, target: String) {
    loop {
        let (inbound, client_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("stream accept error: {e}");
                continue;
            }
        };

        let target = target.clone();
        tokio::spawn(async move {
            debug!(
                client = %client_addr,
                target = %target,
                "stream proxy: new connection"
            );

            match tokio::net::TcpStream::connect(&target).await {
                Ok(outbound) => {
                    if let Err(e) = proxy_bidirectional(inbound, outbound).await {
                        debug!(
                            client = %client_addr,
                            target = %target,
                            error = %e,
                            "stream proxy: connection ended"
                        );
                    } else {
                        debug!(
                            client = %client_addr,
                            target = %target,
                            "stream proxy: connection closed"
                        );
                    }
                }
                Err(e) => {
                    error!(
                        client = %client_addr,
                        target = %target,
                        error = %e,
                        "stream proxy: failed to connect to upstream"
                    );
                }
            }
        });
    }
}

/// Copy bytes bidirectionally between two TCP streams until either side
/// closes or an error occurs.
async fn proxy_bidirectional(
    mut inbound: tokio::net::TcpStream,
    mut outbound: tokio::net::TcpStream,
) -> Result<(), std::io::Error> {
    let (client_to_server, server_to_client) =
        tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;

    debug!(
        client_to_server = client_to_server,
        server_to_client = server_to_client,
        "stream proxy: transfer complete"
    );

    Ok(())
}
