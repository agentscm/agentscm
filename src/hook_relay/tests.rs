use super::*;
use serde_json::json;

fn scratch() -> tempfile::TempDir {
    #[cfg(unix)]
    {
        tempfile::tempdir_in("/tmp").unwrap()
    }
    #[cfg(not(unix))]
    {
        tempfile::tempdir().unwrap()
    }
}

fn address() -> (tempfile::TempDir, Address) {
    let scratch = scratch();
    let address = address_in(&scratch, "http://127.0.0.1:1");
    (scratch, address)
}

fn address_in(scratch: &tempfile::TempDir, hub: &str) -> Address {
    let address = Address::new(scratch.path(), hub).unwrap();
    #[cfg(unix)]
    let address = Address {
        directory: scratch.path().join("relay"),
        ..address
    };
    address
}

fn request() -> HookRequest {
    HookRequest {
        agent: "claude".into(),
        repo: Some("acme/widgets".into()),
        token: "synthetic-token".into(),
        root: None,
        client: None,
        report_completion: false,
        body: b"{}".to_vec(),
    }
}

#[test]
fn keys_bind_hub_config_and_binary_but_not_tokens() {
    let config = tempfile::tempdir().unwrap();
    let first = Address::new(config.path(), "https://hub.example.com").unwrap();
    let normalized = Address::new(config.path(), "https://hub.example.com/").unwrap();
    assert_eq!(first.key, normalized.key);
    assert_ne!(
        first.key,
        Address::new(config.path(), "https://other.example.com")
            .unwrap()
            .key
    );
    assert_ne!(first.key, address().1.key);
    assert_ne!(
        first.key,
        Address::new(&config.path().join("other"), "https://hub.example.com")
            .unwrap()
            .key
    );
    assert_ne!(
        first.key,
        Address::new(config.path(), "https://hub.example.com:8443")
            .unwrap()
            .key
    );
    for bad in [
        "http://remote.example.com",
        "https://secret@hub.example.com",
        "https://hub.example.com?secret",
    ] {
        assert!(Address::new(config.path(), bad).is_err());
    }
}

#[test]
fn forward_is_only_a_hook_post_and_preserves_headers_body_and_token_rotation() {
    let client = crate::hub::forge_http::client().unwrap();
    let mut hook = request();
    hook.repo = Some("acme/widgets?x=/other".into());
    hook.root = Some("synthetic-root".into());
    hook.client = Some("synthetic-binding".into());
    hook.report_completion = true;
    let built = hook
        .request(&client, "https://hub.example.com")
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(built.method(), reqwest::Method::POST);
    assert_eq!(built.url().path(), "/v1/hub/hook/claude");
    assert_eq!(
        built.url().query_pairs().collect::<Vec<_>>(),
        vec![("repo".into(), "acme/widgets?x=/other".into())]
    );
    assert_eq!(built.headers()["x-agentscm-root"], "synthetic-root");
    assert_eq!(
        built.headers()[crate::hub::onboarding::CLIENT_HEADER],
        "synthetic-binding"
    );
    assert_eq!(
        built.headers()[crate::hub::insights::COMPLETION_HEADER],
        "true"
    );
    assert_eq!(built.body().unwrap().as_bytes().unwrap(), b"{}");
    hook.token = "synthetic-rotated-token".into();
    assert_eq!(
        hook.request(&client, "https://hub.example.com")
            .unwrap()
            .build()
            .unwrap()
            .headers()["authorization"],
        "Bearer synthetic-rotated-token"
    );
    for bad in [
        "",
        "../mcp",
        "claude?x=1",
        "claude/other",
        "https://elsewhere.example.com",
    ] {
        hook.agent = bad.into();
        assert!(hook.request(&client, "https://hub.example.com").is_err());
    }
}

#[tokio::test]
async fn framing_rejects_oversize_truncated_and_invalid_json() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    write_frame(&mut writer, b"{}", 2).await.unwrap();
    assert_eq!(read_frame(&mut reader, 2).await.unwrap(), b"{}");
    assert!(write_frame(&mut writer, b"too long", 2).await.is_err());
    writer.write_u32(3).await.unwrap();
    assert!(read_frame(&mut reader, 2).await.is_err());
    writer.write_u32(2).await.unwrap();
    writer.write_all(b"{").await.unwrap();
    drop(writer);
    assert!(read_frame(&mut reader, 2).await.is_err());
    assert!(serde_json::from_value::<Head>(json!({"request": {}, "remaining_ms": 1})).is_err());
    let mut value = serde_json::to_value(request()).unwrap();
    value["url"] = json!("https://other.example.com");
    assert!(serde_json::from_value::<HookRequest>(value).is_err());
}

#[tokio::test]
async fn no_relay_or_expired_deadline_allows_direct_fallback() {
    let (_scratch, address) = address();
    let before = Instant::now();
    assert!(
        try_forward(&address, &request(), before + Duration::from_secs(3))
            .await
            .is_none()
    );
    assert!(try_forward(&address, &request(), before).await.is_none());
    assert!(before.elapsed() < Duration::from_millis(100));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn socket_path(address: &Address) -> std::path::PathBuf {
        address
            .directory
            .join(format!("{}.sock", &address.key[..32]))
    }

    #[tokio::test]
    async fn election_cleans_stale_socket_and_rejects_links_and_permissions() {
        let (_scratch, address) = address();
        let owner = transport::Listener::bind(&address).unwrap().unwrap();
        assert!(transport::Listener::bind(&address).unwrap().is_none());
        let path = socket_path(&address);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        drop(owner);
        assert!(!path.exists());
        for mode in [0o600, 0o755] {
            let stale = tokio::net::UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            drop(stale);
            let owner = transport::Listener::bind(&address).unwrap().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
            drop(owner);
        }
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(transport::Listener::bind(&address).is_err());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("absent", &path).unwrap();
        assert!(transport::Listener::bind(&address).is_err());
        std::fs::remove_file(&path).unwrap();
        let owner = transport::Listener::bind(&address).unwrap().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(transport::connect(&address).await.is_err());
        drop(owner);
        let lock = path.with_extension("lock");
        let linked = path.with_extension("linked");
        std::fs::hard_link(&lock, &linked).unwrap();
        assert!(transport::Listener::bind(&address).is_err());
        std::fs::remove_file(&linked).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(transport::Listener::bind(&address).is_err());
        std::fs::remove_file(&lock).unwrap();
        std::fs::set_permissions(&address.directory, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(transport::Listener::bind(&address).is_err());
        assert!(transport::connect(&address).await.is_err());
        std::fs::set_permissions(&address.directory, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::remove_dir(&address.directory).unwrap();
        std::os::unix::fs::symlink("absent", &address.directory).unwrap();
        assert!(transport::Listener::bind(&address).is_err());
    }

    #[tokio::test]
    async fn owner_cleanup_preserves_a_replacement_socket() {
        let (_scratch, address) = address();
        let owner = transport::Listener::bind(&address).unwrap().unwrap();
        let path = socket_path(&address);
        let moved = path.with_extension("moved");
        std::fs::rename(&path, &moved).unwrap();
        let replacement = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(owner);
        assert!(path.exists());
        assert!(moved.exists());
        assert!(transport::connect(&address).await.is_ok());
        drop(replacement);
    }

    #[tokio::test]
    async fn wedged_or_wrong_version_probe_falls_back_without_sending_credentials() {
        for hello in [None, Some([b'x'; 64])] {
            let (_scratch, address) = address();
            let mut listener = transport::Listener::bind(&address).unwrap().unwrap();
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                if let Some(hello) = hello {
                    stream.write_all(&hello).await.unwrap();
                }
                let mut received = [0; 1];
                assert_eq!(stream.read(&mut received).await.unwrap(), 0);
            });
            let before = Instant::now();
            assert!(
                try_forward(&address, &request(), before + Duration::from_secs(3))
                    .await
                    .is_none()
            );
            assert!(before.elapsed() < Duration::from_millis(100));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn accepted_then_dead_or_wedged_never_permits_resending() {
        for wedged in [false, true] {
            let (_scratch, address) = address();
            let mut listener = transport::Listener::bind(&address).unwrap().unwrap();
            let hello = address.key.clone();
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                stream.write_all(hello.as_bytes()).await.unwrap();
                let _: Head =
                    serde_json::from_slice(&read_frame(&mut stream, HEAD_BYTES).await.unwrap())
                        .unwrap();
                assert_eq!(read_frame(&mut stream, STDIN_BYTES).await.unwrap(), b"{}");
                if wedged {
                    let mut byte = [0; 1];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                }
            });
            let result = try_forward(
                &address,
                &request(),
                Instant::now() + Duration::from_millis(100),
            )
            .await;
            assert!(matches!(result, Some(Err(_))));
            server.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_posts_reuse_connection_but_never_retry_after_request_was_read() {
        let hub = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = scratch();
        let address = address_in(&config, &format!("http://{}", hub.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (mut stream, _) = hub.accept().await.unwrap();
            for token in ["synthetic-token", "synthetic-rotated-token"] {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                while !bytes.ends_with(b"\r\n\r\n{}") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0 && bytes.len() < HEAD_BYTES);
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(
                    headers
                        .starts_with("POST /v1/hub/hook/claude?repo=acme%2Fwidgets HTTP/1.1\r\n")
                );
                assert!(headers.contains(&format!("authorization: Bearer {token}\r\n")));
                if token == "synthetic-token" {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                        .await
                        .unwrap();
                }
            }
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), hub.accept())
                    .await
                    .is_err()
            );
        });
        let relay = Relay::spawn(address.clone(), crate::hub::forge_http::client().unwrap());
        let mut hook = request();
        let first = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(reply) =
                    try_forward(&address, &hook, Instant::now() + Duration::from_secs(1)).await
                {
                    break reply;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first, (200, json!({})));
        hook.token = "synthetic-rotated-token".into();
        assert!(matches!(
            try_forward(&address, &hook, Instant::now() + Duration::from_secs(1)).await,
            Some(Err(_))
        ));
        server.await.unwrap();
        drop(relay);
    }

    #[tokio::test]
    async fn expired_forward_budget_does_not_open_a_hub_connection() {
        let hub = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let scratch = scratch();
        let address = address_in(&scratch, &format!("http://{}", hub.local_addr().unwrap()));
        let mut listener = transport::Listener::bind(&address).unwrap().unwrap();
        let server_address = address.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let _ = serve(
                stream,
                server_address,
                crate::hub::forge_http::client().unwrap(),
            )
            .await;
        });
        let mut stream = transport::connect(&address).await.unwrap();
        let mut hello = [0; 64];
        stream.read_exact(&mut hello).await.unwrap();
        let head = serde_json::to_vec(&Head {
            request: request(),
            remaining_ms: 0,
        })
        .unwrap();
        write_frame(&mut stream, &head, HEAD_BYTES).await.unwrap();
        write_frame(&mut stream, b"{}", STDIN_BYTES).await.unwrap();
        assert_eq!(read_frame(&mut stream, REPLY_BYTES).await.unwrap(), b"null");
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), hub.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn another_mcp_host_takes_over_when_the_owner_exits() {
        let (_scratch, address) = address();
        let mut owner = transport::Listener::bind(&address).unwrap().unwrap();
        let relay = Relay::spawn(address.clone(), crate::hub::forge_http::client().unwrap());
        let mut subscription = tokio::time::timeout(Duration::from_secs(1), owner.accept())
            .await
            .unwrap()
            .unwrap();
        subscription
            .write_all(address.key.as_bytes())
            .await
            .unwrap();
        assert!(
            read_frame(&mut subscription, HEAD_BYTES)
                .await
                .unwrap()
                .is_empty()
        );
        subscription.write_u8(0).await.unwrap();
        drop(owner);
        drop(subscription);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if transport::connect(&address).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        drop(relay);
        tokio::time::timeout(Duration::from_secs(1), async {
            while socket_path(&address).exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            try_forward(
                &address,
                &request(),
                Instant::now() + Duration::from_secs(3)
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn dropping_a_standby_host_closes_its_ownership_subscription() {
        let (_scratch, address) = address();
        let mut owner = transport::Listener::bind(&address).unwrap().unwrap();
        let relay = Relay::spawn(address.clone(), crate::hub::forge_http::client().unwrap());
        let mut subscription = tokio::time::timeout(Duration::from_secs(1), owner.accept())
            .await
            .unwrap()
            .unwrap();
        subscription
            .write_all(address.key.as_bytes())
            .await
            .unwrap();
        assert!(
            read_frame(&mut subscription, HEAD_BYTES)
                .await
                .unwrap()
                .is_empty()
        );
        subscription.write_u8(0).await.unwrap();
        tokio::task::yield_now().await;
        drop(relay);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), subscription.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(transport::Listener::bind(&address).unwrap().is_none());
    }

    #[tokio::test]
    async fn protocol_errors_never_echo_a_secret_from_a_reply() {
        let (_scratch, address) = address();
        let mut listener = transport::Listener::bind(&address).unwrap().unwrap();
        let hello = address.key.clone();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            stream.write_all(hello.as_bytes()).await.unwrap();
            read_frame(&mut stream, HEAD_BYTES).await.unwrap();
            read_frame(&mut stream, STDIN_BYTES).await.unwrap();
            write_frame(&mut stream, br#""synthetic-secret-reply""#, REPLY_BYTES)
                .await
                .unwrap();
        });
        let result = try_forward(
            &address,
            &request(),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.to_string(), "hook relay unavailable");
        assert!(!format!("{error:?}").contains("synthetic-secret-reply"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_or_oversized_heads_never_reach_the_hub() {
        let hub = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let scratch = scratch();
        let address = address_in(&scratch, &format!("http://{}", hub.local_addr().unwrap()));
        let mut listener = transport::Listener::bind(&address).unwrap().unwrap();
        for head in [
            br#"{"request":"synthetic-secret-head","remaining_ms":1}"#.as_slice(),
            &[b'x'; HEAD_BYTES + 1],
        ] {
            let mut stream = transport::connect(&address).await.unwrap();
            let accepted = listener.accept().await.unwrap();
            let server = tokio::spawn(serve(
                accepted,
                address.clone(),
                crate::hub::forge_http::client().unwrap(),
            ));
            let mut hello = [0; 64];
            stream.read_exact(&mut hello).await.unwrap();
            stream.write_u32(head.len() as u32).await.unwrap();
            if head.len() <= HEAD_BYTES {
                stream.write_all(head).await.unwrap();
            }
            assert!(server.await.unwrap().is_none());
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), hub.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_idle_pool_connection_is_replaced_without_duplicate_posts() {
        let hub = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let scratch = scratch();
        let address = address_in(&scratch, &format!("http://{}", hub.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = hub.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                while !bytes.ends_with(b"\r\n\r\n{}") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0 && bytes.len() < HEAD_BYTES);
                    bytes.extend_from_slice(&chunk[..read]);
                }
                assert!(
                    bytes.starts_with(b"POST /v1/hub/hook/claude?repo=acme%2Fwidgets HTTP/1.1\r\n")
                );
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), hub.accept())
                    .await
                    .is_err()
            );
        });
        let relay = Relay::spawn(address.clone(), crate::hub::forge_http::client().unwrap());
        for _ in 0..2 {
            let reply = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(reply) = try_forward(
                        &address,
                        &request(),
                        Instant::now() + Duration::from_secs(1),
                    )
                    .await
                    {
                        break reply;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(reply, (200, json!({})));
        }
        server.await.unwrap();
        drop(relay);
    }

    #[tokio::test]
    async fn dropping_the_owner_cancels_pending_requests_and_observers() {
        let (_scratch, address) = address();
        let relay = Relay::spawn(address.clone(), crate::hub::forge_http::client().unwrap());
        let mut observer = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(stream) = transport::connect(&address).await {
                    break stream;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut hello = [0; 64];
        observer.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, address.key.as_bytes());
        write_frame(&mut observer, &[], HEAD_BYTES).await.unwrap();
        assert_eq!(observer.read_u8().await.unwrap(), 0);
        let mut pending = transport::connect(&address).await.unwrap();
        pending.read_exact(&mut hello).await.unwrap();
        drop(relay);
        for mut stream in [observer, pending] {
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        }
        assert!(!socket_path(&address).exists());
        assert!(transport::Listener::bind(&address).unwrap().is_some());
    }
}
