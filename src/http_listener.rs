//! The one HTTP/1 accept loop for every listener this crate serves (Hub, team server,
//! `serve` and the demo), so each gets hyper's header read timeout.
use std::future::Future;
use std::time::Duration;

use axum::Router;

/// A client must send its whole request head within this; trickled headers do not extend it.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Serves `app` until `shutdown` completes, then drains open connections.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()>,
) {
    serve_with(listener, app, HEADER_READ_TIMEOUT, shutdown).await;
}

/// Like `axum::serve` with graceful shutdown and `ConnectInfo<SocketAddr>`, but with a timer,
/// without which hyper silently disables its header read timeout.
pub(crate) async fn serve_with(
    listener: tokio::net::TcpListener,
    app: Router,
    header_read_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) {
    let (stop, stopped) = tokio::sync::watch::channel(());
    let mut connections = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                // Like axum: back off on errors such as running out of file descriptors.
                Err(_) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let app = app.clone();
        let mut stopped = stopped.clone();
        connections.spawn(async move {
            let service = hyper::service::service_fn(move |request: hyper::Request<_>| {
                let mut request = request.map(axum::body::Body::new);
                request
                    .extensions_mut()
                    .insert(axum::extract::ConnectInfo(peer));
                tower_service::Service::call(&mut app.clone(), request)
            });
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(header_read_timeout)
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .with_upgrades();
            let mut connection = std::pin::pin!(connection);
            tokio::select! {
                _ = connection.as_mut() => {}
                _ = stopped.changed() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                }
            }
        });
        while connections.try_join_next().is_some() {}
    }
    drop(stop);
    while connections.join_next().await.is_some() {}
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::SocketAddr;
    use std::time::Instant;

    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Sends a request line, then one header line every 50 ms for three seconds; returns
    /// how long the server took to close the connection and what it sent.
    pub(crate) async fn trickle_headers(address: SocketAddr, path: &str) -> (Duration, Vec<u8>) {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let started = Instant::now();
        let line = format!("GET {path} HTTP/1.1\r\n");
        tokio::spawn(async move {
            let _ = writer.write_all(line.as_bytes()).await;
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if writer.write_all(b"x-slow: 1\r\n").await.is_err() {
                    break;
                }
            }
        });
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut response))
            .await
            .expect("the server closes the connection")
            .ok();
        (started.elapsed(), response)
    }

    async fn listen(header_read_timeout: Duration) -> SocketAddr {
        let app = Router::new().route(
            "/peer",
            get(
                |axum::Extension(axum::extract::ConnectInfo(peer)): axum::Extension<
                    axum::extract::ConnectInfo<SocketAddr>,
                >| async move { peer.ip().to_string() },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve_with(
            listener,
            app,
            header_read_timeout,
            std::future::pending(),
        ));
        address
    }

    #[tokio::test]
    async fn served_requests_carry_the_peer_address() {
        let address = listen(HEADER_READ_TIMEOUT).await;
        let body = reqwest::get(format!("http://{address}/peer"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "127.0.0.1");
    }

    #[tokio::test]
    async fn trickled_request_headers_are_cut_off_at_the_read_timeout() {
        let limit = Duration::from_millis(300);
        let address = listen(limit).await;
        // Each header line arrives well inside the bound; together they outlast it.
        let (elapsed, response) = trickle_headers(address, "/peer").await;
        assert!(elapsed >= limit, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert!(!response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    }
}
