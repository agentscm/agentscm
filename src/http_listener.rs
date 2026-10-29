//! The one HTTP/1 accept loop for every listener this crate serves (Hub, team server,
//! `serve` and the demo), so each gets hyper's header read timeout and one connection cap.
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;

/// Open connections served at once, like the per-process request limit in `server.rs`. Past it the
/// listener stops accepting, so excess clients wait in the kernel backlog holding no memory here.
const MAX_CONNECTIONS: usize = 256;

/// A client must send its whole request head within this; trickled headers do not extend it. Hyper
/// starts the same clock for the next request on a kept-alive connection, so a client that
/// connects and goes quiet, or idles after a response, is closed after this long.
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
    serve_limited(
        listener,
        app,
        header_read_timeout,
        MAX_CONNECTIONS,
        shutdown,
    )
    .await;
}

pub(crate) async fn serve_limited(
    listener: tokio::net::TcpListener,
    app: Router,
    header_read_timeout: Duration,
    max_connections: usize,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(tokio::sync::Semaphore::new(max_connections));
    let (stop, stopped) = tokio::sync::watch::channel(());
    let mut connections = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        // Take a slot before accepting, so the cap holds back the accept itself.
        let permit = tokio::select! {
            permit = slots.clone().acquire_owned() => permit.expect("the semaphore is never closed"),
            () = &mut shutdown => break,
        };
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
            let _permit = permit;
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

    async fn listen_limited(header_read_timeout: Duration, max_connections: usize) -> SocketAddr {
        let app = Router::new().route("/peer", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve_limited(
            listener,
            app,
            header_read_timeout,
            max_connections,
            std::future::pending(),
        ));
        address
    }

    /// How long until the server closes a connection that has been quiet since `stream` opened.
    async fn closed_after(mut stream: tokio::net::TcpStream) -> Duration {
        let started = Instant::now();
        let mut sink = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut sink))
            .await
            .expect("the server closes the quiet connection")
            .ok();
        started.elapsed()
    }

    #[tokio::test]
    async fn a_connection_that_never_sends_is_closed() {
        let limit = Duration::from_millis(300);
        let address = listen_limited(limit, 8).await;
        let elapsed = closed_after(tokio::net::TcpStream::connect(address).await.unwrap()).await;
        assert!(elapsed >= limit, "{elapsed:?}");
    }

    #[tokio::test]
    async fn a_kept_alive_connection_that_goes_quiet_is_closed() {
        let limit = Duration::from_millis(300);
        let address = listen_limited(limit, 8).await;
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /peer HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut reply = [0_u8; 256];
        assert!(stream.read(&mut reply).await.unwrap() > 0);
        let elapsed = closed_after(stream).await;
        assert!(elapsed >= limit / 2, "{elapsed:?}");
    }

    #[tokio::test]
    async fn connections_past_the_cap_wait_until_a_slot_frees() {
        let address = listen_limited(HEADER_READ_TIMEOUT, 2).await;
        let first = tokio::net::TcpStream::connect(address).await.unwrap();
        let _second = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut third = tokio::net::TcpStream::connect(address).await.unwrap();
        third
            .write_all(b"GET /peer HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut reply = Vec::new();
        // Both slots are held by quiet clients, so the third is not served.
        let waited = tokio::time::timeout(Duration::from_millis(500), third.read_buf(&mut reply));
        assert!(waited.await.is_err(), "{reply:?}");
        drop(first);
        tokio::time::timeout(Duration::from_secs(5), third.read_to_end(&mut reply))
            .await
            .expect("a freed slot serves the waiting client")
            .unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 200"), "{reply:?}");
    }
}
