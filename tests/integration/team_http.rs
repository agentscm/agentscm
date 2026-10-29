use crate::team_pg;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use agentscm::backend::{ImmutableBackend, LocalBackend, ObjectKey, SOURCE_PROFILE_V1};
use agentscm::model::FileChange;
use agentscm::store::Engine;
use agentscm::sync::SourceOffer;
use agentscm::team::auth::{Role, generate_secret};
use agentscm::team::http;
use agentscm::team::publication::{CompleteRequest, RepositoryScope, ReserveRequest};
use agentscm::team::service::{BootstrapRequest, TeamConfig, operator_bootstrap};
use axum::Router;
use serde_json::{Value, json};
use team_pg::Cluster;

const ORIGIN: &str = "https://team.example";
const SESSION_COOKIE: &str = "__Host-agentscm-session";

struct TestServer {
    address: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                ready_tx.send(()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        ready_rx.recv().unwrap();
        Self {
            address,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }

    fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> HttpResponse {
        let mut stream = TcpStream::connect(self.address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.address,
            body.len()
        )
        .unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        HttpResponse::parse(&raw)
    }

    fn json(
        &self,
        method: &str,
        target: &str,
        mut headers: Vec<(&str, String)>,
        body: Value,
    ) -> HttpResponse {
        headers.push(("Content-Type", "application/json".into()));
        self.request(
            method,
            target,
            &headers,
            &serde_json::to_vec(&body).unwrap(),
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn parse(raw: &[u8]) -> Self {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("HTTP response has a header terminator");
        let head = std::str::from_utf8(&raw[..split]).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<_> = lines
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        let encoded = &raw[split + 4..];
        let body = if headers.iter().any(|(name, value)| {
            name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked")
        }) {
            decode_chunks(encoded)
        } else {
            encoded.to_vec()
        };
        Self {
            status,
            headers,
            body,
        }
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("missing response header {name}"))
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_owned()
    }
}

fn decode_chunks(mut encoded: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    loop {
        let line = encoded
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk has a size line");
        let size = usize::from_str_radix(
            std::str::from_utf8(&encoded[..line])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .unwrap();
        encoded = &encoded[line + 2..];
        if size == 0 {
            break;
        }
        assert!(encoded.len() >= size + 2);
        decoded.extend_from_slice(&encoded[..size]);
        assert_eq!(&encoded[size..size + 2], b"\r\n");
        encoded = &encoded[size + 2..];
    }
    decoded
}

fn scope(repository: &str) -> RepositoryScope {
    RepositoryScope {
        trust_domain: "team".into(),
        repository: repository.into(),
    }
}

fn bootstrap(config: &agentscm::team::service::DatabaseConfig, repository: &str, token: &str) {
    operator_bootstrap(
        config,
        &BootstrapRequest {
            scope: scope(repository),
            authority_generation: 1,
            operation: format!("bootstrap-{repository}"),
            max_bytes: 8_000_000,
            max_objects: 8_000,
            principal: "admin".into(),
            roles: vec![Role::Reader, Role::Contributor, Role::Administrator],
        },
        token,
    )
    .unwrap();
}

fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {token}"))]
}

fn browser(cookie: &str, csrf: &str, origin: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Cookie", cookie.to_owned()),
        ("X-AgentSCM-CSRF", csrf.to_owned()),
        ("Origin", origin.to_owned()),
    ]
}

fn login_body(repository: &str, credential: &str) -> Value {
    json!({
        "scope": {"trust_domain": "team", "repository": repository},
        "authority_generation": 1,
        "credential": credential,
        "ttl_seconds": 120
    })
}

fn login(server: &TestServer, repository: &str, credential: &str) -> HttpResponse {
    server.json(
        "POST",
        "/v1/team/session",
        vec![("Origin", ORIGIN.into())],
        login_body(repository, credential),
    )
}

fn session(response: &HttpResponse) -> (String, String) {
    let set_cookie = response.header("set-cookie");
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let csrf = response.json()["csrf_token"].as_str().unwrap().to_owned();
    (cookie, csrf)
}

fn assert_error(response: &HttpResponse, status: u16, code: &str) {
    assert_eq!(
        response.status,
        status,
        "body: {}",
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(response.code(), code);
    assert_eq!(response.json(), json!({"error": {"code": code}}));
    assert_eq!(response.header("cache-control"), "no-store");
}

#[test]
#[ignore = "requires explicit AGENTSCM_PG_BIN_DIR; fresh isolated PostgreSQL18 only"]
fn pg18_team_http_authentication_isolation_bounds_and_draft_lifecycle() {
    let cluster = Cluster::new();
    let admin_a = generate_secret().unwrap();
    let admin_b = generate_secret().unwrap();
    bootstrap(&cluster.config, "repo-a", admin_a.expose());
    bootstrap(&cluster.config, "repo-b", admin_b.expose());
    let objects = tempfile::tempdir().unwrap();

    // Router construction performs synchronous schema/backend startup before
    // the test creates the listener's Tokio runtime.
    let app = http::router(TeamConfig {
        database: cluster.config.clone(),
        objects: objects.path().to_path_buf(),
        origin: ORIGIN.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        max_concurrency: 2,
    })
    .unwrap();
    let server = TestServer::start(app);

    let missing_origin = server.json(
        "POST",
        "/v1/team/session",
        vec![],
        login_body("repo-a", admin_a.expose()),
    );
    assert_error(&missing_origin, 403, "denied");
    let wrong_origin = server.json(
        "POST",
        "/v1/team/session",
        vec![("Origin", "https://other.example".into())],
        login_body("repo-a", admin_a.expose()),
    );
    assert_error(&wrong_origin, 403, "denied");

    let login_response = login(&server, "repo-a", admin_a.expose());
    assert_eq!(login_response.status, 200);
    let body_text = std::str::from_utf8(&login_response.body).unwrap();
    let set_cookie = login_response.header("set-cookie");
    assert!(set_cookie.starts_with(&format!("{SESSION_COOKIE}=")));
    for attribute in [
        "Max-Age=120",
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Strict",
    ] {
        assert!(set_cookie.split("; ").any(|field| field == attribute));
    }
    let (admin_cookie, admin_csrf) = session(&login_response);
    let session_token = admin_cookie.split_once('=').unwrap().1;
    assert_eq!(session_token.len(), 64);
    assert!(!body_text.contains(session_token));
    assert_eq!(login_response.json()["generation"], 1);
    assert!(login_response.json().get("session_token").is_none());

    let reservation = json!({
        "generation": 1,
        "publication": "draft",
        "root": "0".repeat(64),
        "byte_limit": 1_000_000,
        "object_limit": 100,
        "lease_seconds": 120
    });
    let publication_path = "/v1/team/repositories/team/repo-a/publications";
    let no_mutation_origin = server.json(
        "POST",
        publication_path,
        vec![
            ("Cookie", admin_cookie.clone()),
            ("X-AgentSCM-CSRF", admin_csrf.clone()),
        ],
        reservation.clone(),
    );
    assert_error(&no_mutation_origin, 403, "denied");
    let missing_csrf = server.json(
        "POST",
        publication_path,
        vec![("Cookie", admin_cookie.clone()), ("Origin", ORIGIN.into())],
        reservation.clone(),
    );
    assert_error(&missing_csrf, 401, "unauthorized");
    let wrong_csrf = server.json(
        "POST",
        publication_path,
        browser(&admin_cookie, &"f".repeat(64), ORIGIN),
        reservation.clone(),
    );
    assert_error(&wrong_csrf, 401, "unauthorized");
    let wrong_mutation_origin = server.json(
        "POST",
        publication_path,
        browser(&admin_cookie, &admin_csrf, "https://other.example"),
        reservation.clone(),
    );
    assert_error(&wrong_mutation_origin, 403, "denied");
    let reserved = server.json(
        "POST",
        publication_path,
        browser(&admin_cookie, &admin_csrf, ORIGIN),
        reservation.clone(),
    );
    assert_eq!(reserved.status, 200);
    assert_eq!(reserved.json()["status"], "reserved");

    let inspect_path = "/v1/team/repositories/team/repo-a/publications/1/draft";
    let inspected = server.request(
        "GET",
        inspect_path,
        &[("Cookie", admin_cookie.clone())],
        &[],
    );
    assert_eq!(inspected.status, 200);
    assert_eq!(inspected.json()["state"], "reserved");

    let reader = generate_secret().unwrap();
    let member = generate_secret().unwrap();
    let contributor = generate_secret().unwrap();
    for (principal, credential, roles) in [
        ("reader", reader.expose(), vec!["reader"]),
        ("member", member.expose(), vec!["reader", "contributor"]),
        ("contributor", contributor.expose(), vec!["contributor"]),
    ] {
        let provisioned = server.json(
            "POST",
            "/v1/team/repositories/team/repo-a/principals",
            bearer(admin_a.expose()),
            json!({
                "authority_generation": 1,
                "operation": format!("provision-{principal}"),
                "principal": principal,
                "credential": credential,
                "roles": roles
            }),
        );
        assert_eq!(provisioned.status, 200);
    }

    let reader_denied = server.json(
        "POST",
        publication_path,
        bearer(reader.expose()),
        json!({
            "generation": 1,
            "publication": "reader-write",
            "root": "1".repeat(64),
            "byte_limit": 10,
            "object_limit": 1,
            "lease_seconds": 60
        }),
    );
    assert_error(&reader_denied, 403, "denied");
    let reader_inspect = server.request(
        "GET",
        inspect_path,
        &[("Authorization", format!("Bearer {}", reader.expose()))],
        &[],
    );
    assert_eq!(reader_inspect.status, 200);

    let wrong_repository = server.request(
        "GET",
        "/v1/team/repositories/team/repo-b/publications/1/draft",
        &[("Authorization", format!("Bearer {}", admin_a.expose()))],
        &[],
    );
    assert_error(&wrong_repository, 401, "unauthorized");

    let duplicate_bearer = server.request(
        "GET",
        inspect_path,
        &[
            ("Authorization", format!("Bearer {}", admin_a.expose())),
            ("Authorization", format!("Bearer {}", admin_a.expose())),
        ],
        &[],
    );
    assert_error(&duplicate_bearer, 401, "unauthorized");
    let duplicate_cookie = server.request(
        "GET",
        inspect_path,
        &[("Cookie", format!("{admin_cookie}; {admin_cookie}"))],
        &[],
    );
    assert_error(&duplicate_cookie, 401, "unauthorized");
    let ambiguous = server.request(
        "GET",
        inspect_path,
        &[
            ("Authorization", format!("Bearer {}", admin_a.expose())),
            ("Cookie", admin_cookie.clone()),
        ],
        &[],
    );
    assert_error(&ambiguous, 401, "unauthorized");

    // Updating the principal version invalidates the old browser session but
    // leaves its credential enabled. A normal browser sends that stale cookie
    // during re-login; valid credential exchange must replace it.
    let refreshed_admin = server.json(
        "PUT",
        "/v1/team/repositories/team/repo-a/principals/admin/roles",
        bearer(admin_a.expose()),
        json!({
            "authority_generation": 1,
            "operation": "refresh-admin",
            "expected_version": 1,
            "roles": ["reader", "contributor", "administrator"]
        }),
    );
    assert_eq!(refreshed_admin.status, 200);
    let stale_session = server.request(
        "GET",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &[("Cookie", admin_cookie.clone())],
        &[],
    );
    assert_error(&stale_session, 401, "unauthorized");
    let relogin = server.json(
        "POST",
        "/v1/team/session",
        vec![("Origin", ORIGIN.into()), ("Cookie", admin_cookie.clone())],
        login_body("repo-a", admin_a.expose()),
    );
    assert_eq!(
        relogin.status,
        200,
        "a stale cookie must not prevent replacement: {}",
        String::from_utf8_lossy(&relogin.body)
    );
    let (new_admin_cookie, new_admin_csrf) = session(&relogin);
    assert_ne!(new_admin_cookie, admin_cookie);

    let member_login = login(&server, "repo-a", member.expose());
    assert_eq!(member_login.status, 200);
    let (member_cookie, _) = session(&member_login);
    let revoked = server.json(
        "POST",
        "/v1/team/repositories/team/repo-a/principals/member/revoke",
        bearer(admin_a.expose()),
        json!({
            "authority_generation": 1,
            "operation": "revoke-member",
            "expected_version": 1
        }),
    );
    assert_eq!(revoked.status, 200);
    for credentials in [
        vec![("Authorization", format!("Bearer {}", member.expose()))],
        vec![("Cookie", member_cookie)],
    ] {
        let response = server.request("GET", inspect_path, &credentials, &[]);
        assert_error(&response, 401, "unauthorized");
    }

    // The remote API cannot upload or qualify. A trusted controller builds an
    // actual R1 closure into the same backend, then completes its retained
    // publication before any encoded object can be read through HTTP.
    let source_fixture = tempfile::tempdir().unwrap();
    let source = Engine::open(source_fixture.path().join("source")).unwrap();
    let base = source
        .checkpoint(
            None,
            &[FileChange {
                path: "README.md".into(),
                content: Some(b"retained base\n".to_vec()),
            }],
        )
        .unwrap();
    let root = source
        .checkpoint(
            Some(&base),
            &[FileChange {
                path: "src/lib.rs".into(),
                content: Some(b"pub fn qualified() -> bool { true }\n".to_vec()),
            }],
        )
        .unwrap();
    let backend = LocalBackend::open(objects.path()).unwrap();
    let mut controller = cluster.config.open(scope("repo-a")).unwrap();
    controller
        .reserve(&ReserveRequest {
            generation: 1,
            publication: "qualified".into(),
            root: root.clone(),
            byte_limit: 1_000_000,
            object_limit: 100,
            lease_seconds: 120,
        })
        .unwrap();
    let offer_path = source_fixture.path().join("qualified-offer.json");
    let manifest_digest = controller
        .upload_and_qualify(1, "qualified", &source, &backend, &offer_path)
        .unwrap();
    controller
        .complete(&CompleteRequest {
            generation: 1,
            publication: "qualified".into(),
            operation: "complete-qualified".into(),
            manifest_digest,
        })
        .unwrap();
    let offer: SourceOffer = serde_json::from_slice(&std::fs::read(&offer_path).unwrap()).unwrap();
    let object = offer
        .objects
        .iter()
        .find(|object| object.encoded_bytes >= 2)
        .unwrap();
    let key = ObjectKey::new("team", "repo-a", SOURCE_PROFILE_V1, &object.id).unwrap();
    let expected = backend.get_verified(&key, "1-qualified").unwrap();
    let object_path = format!(
        "/v1/team/repositories/team/repo-a/objects/{}?generation=1&publication=qualified",
        object.id
    );

    let full_object = server.request(
        "GET",
        &object_path,
        &[("Authorization", format!("Bearer {}", reader.expose()))],
        &[],
    );
    assert_eq!(full_object.status, 200);
    assert_eq!(
        full_object.header("content-type"),
        "application/octet-stream"
    );
    assert_eq!(full_object.header("cache-control"), "no-store");
    assert_eq!(full_object.header("x-content-type-options"), "nosniff");
    assert_eq!(full_object.body, expected);

    // Native views use this actual retained source closure and fresh Reader
    // grants, with opaque server continuations bound to the selected view.
    let native_base = "/v1/team/repositories/team/repo-a/publications/1/qualified/source";
    let reader_headers = [("Authorization", format!("Bearer {}", reader.expose()))];
    let revision = server.request(
        "GET",
        &format!("{native_base}/revision"),
        &reader_headers,
        &[],
    );
    assert_eq!(revision.status, 200);
    assert_eq!(revision.json()["id"], root);
    assert_eq!(revision.json()["source"]["profile"], SOURCE_PROFILE_V1);
    let native_file = server.request(
        "GET",
        &format!("{native_base}/file?path=src%2Flib.rs"),
        &reader_headers,
        &[],
    );
    assert_eq!(native_file.status, 200);
    assert_eq!(
        native_file.body,
        source.read_file(&root, "src/lib.rs").unwrap()
    );
    assert_eq!(
        native_file.header("content-type"),
        "application/octet-stream"
    );
    assert_eq!(native_file.header("cache-control"), "no-store");
    assert_eq!(native_file.header("x-content-type-options"), "nosniff");
    let files_first = server.request(
        "GET",
        &format!("{native_base}/files?operation=http-files&limit=1"),
        &reader_headers,
        &[],
    );
    assert_eq!(files_first.status, 200);
    let files_json = files_first.json();
    let cursor = files_json["next"].as_str().unwrap();
    assert_eq!(cursor.len(), 64);
    let mut paths = Vec::new();
    let mut page = files_json.clone();
    let mut terminated = false;
    // A full leaf page can leave exhausted branch frames to unwind on a
    // later page. Follow the native cursor instead of assuming two pages.
    for _ in 0..8 {
        let entries = page["entries"].as_array().unwrap();
        assert!(entries.len() <= 1);
        for entry in entries {
            let path = entry["path"].as_str().unwrap().to_owned();
            assert!(!paths.contains(&path), "duplicate file path {path}");
            paths.push(path);
        }
        if page["next"].is_null() {
            terminated = true;
            break;
        }
        let next_cursor = page["next"].as_str().unwrap();
        assert_eq!(next_cursor.len(), 64);
        let next_path =
            format!("{native_base}/files?operation=http-files&limit=1&cursor={next_cursor}");
        let next = server.request("GET", &next_path, &reader_headers, &[]);
        assert_eq!(next.status, 200);
        let replay = server.request("GET", &next_path, &reader_headers, &[]);
        assert_eq!(replay.status, 200);
        assert_eq!(replay.body, next.body);
        page = next.json();
    }
    assert!(
        terminated,
        "file traversal did not terminate within eight pages"
    );
    paths.sort();
    assert_eq!(paths, ["README.md", "src/lib.rs"]);
    let wrong_view = server.request(
        "GET",
        &format!("{native_base}/history?operation=http-history&limit=1&cursor={cursor}"),
        &reader_headers,
        &[],
    );
    assert_error(&wrong_view, 401, "unauthorized");
    let changed_limit = server.request(
        "GET",
        &format!("{native_base}/files?operation=http-files&limit=2&cursor={cursor}"),
        &reader_headers,
        &[],
    );
    assert_error(&changed_limit, 401, "unauthorized");
    let history = server.request(
        "GET",
        &format!("{native_base}/history?operation=http-history&limit=1"),
        &reader_headers,
        &[],
    );
    assert_eq!(history.status, 200);
    assert_eq!(history.json()["entries"][0]["id"], root);
    let history_json = history.json();
    let history_cursor = history_json["next"].as_str().unwrap();
    let history_next = server.request(
        "GET",
        &format!("{native_base}/history?operation=http-history&limit=1&cursor={history_cursor}"),
        &reader_headers,
        &[],
    );
    assert_eq!(history_next.status, 200);
    assert_eq!(history_next.json()["entries"][0]["id"], base);
    assert!(history_next.json()["next"].is_null());
    let diff = server.request(
        "GET",
        &format!("{native_base}/diff?operation=http-diff&left={base}&right={root}&limit=1"),
        &reader_headers,
        &[],
    );
    assert_eq!(diff.status, 200);
    assert_eq!(diff.json()["entries"].as_array().unwrap().len(), 1);
    let relation = server.request(
        "GET",
        &format!("{native_base}/relation?operation=http-relation&left={base}&right={root}&limit=2"),
        &reader_headers,
        &[],
    );
    assert_eq!(relation.status, 200);
    assert_eq!(relation.json()["relation"], "left_ancestor");
    let anonymous_native = server.request("GET", &format!("{native_base}/revision"), &[], &[]);
    assert_error(&anonymous_native, 401, "unauthorized");
    let contributor_native = server.request(
        "GET",
        &format!("{native_base}/revision"),
        &[("Authorization", format!("Bearer {}", contributor.expose()))],
        &[],
    );
    assert_error(&contributor_native, 403, "denied");
    let foreign_native = server.request(
        "GET",
        "/v1/team/repositories/team/repo-b/publications/1/qualified/source/revision",
        &reader_headers,
        &[],
    );
    assert_error(&foreign_native, 401, "unauthorized");
    let native_range = server.request(
        "GET",
        &format!("{native_base}/file?path=src%2Flib.rs"),
        &[
            ("Authorization", format!("Bearer {}", reader.expose())),
            ("Range", "bytes=0-1".into()),
        ],
        &[],
    );
    assert_error(&native_range, 400, "invalid_request");

    let cancel_path = "/v1/team/repositories/team/repo-a/source-traversals/cancel";
    let cancel_request = json!({"operation": "http-files"});
    let reader_login = login(&server, "repo-a", reader.expose());
    assert_eq!(reader_login.status, 200);
    let (reader_cookie, reader_csrf) = session(&reader_login);
    let no_cancel_csrf = server.json(
        "POST",
        cancel_path,
        vec![("Cookie", reader_cookie.clone()), ("Origin", ORIGIN.into())],
        cancel_request.clone(),
    );
    assert_error(&no_cancel_csrf, 401, "unauthorized");
    let wrong_cancel_csrf = server.json(
        "POST",
        cancel_path,
        browser(&reader_cookie, &"0".repeat(64), ORIGIN),
        cancel_request.clone(),
    );
    assert_error(&wrong_cancel_csrf, 401, "unauthorized");
    let cancel = server.json(
        "POST",
        cancel_path,
        browser(&reader_cookie, &reader_csrf, ORIGIN),
        cancel_request.clone(),
    );
    assert_eq!(cancel.status, 200);
    assert_eq!(cancel.json()["cancelled"], true);
    let cancel_retry = server.json(
        "POST",
        cancel_path,
        reader_headers.to_vec(),
        cancel_request.clone(),
    );
    assert_eq!(cancel_retry.body, cancel.body);
    let cancelled_page = server.request(
        "GET",
        &format!("{native_base}/files?operation=http-files&limit=1&cursor={cursor}"),
        &reader_headers,
        &[],
    );
    assert_error(&cancelled_page, 409, "indeterminate");
    let invalid_cancel = server.json(
        "POST",
        cancel_path,
        reader_headers.to_vec(),
        json!({"operation": "http-files", "authority": true}),
    );
    assert_error(&invalid_cancel, 400, "invalid_request");

    let profile_reservation = json!({
        "reservation": {"generation": 1, "publication": "http-v2", "root": root,
            "byte_limit": 1000000, "object_limit": 100, "lease_seconds": 120},
        "profile": "agentscm-source-v2"
    });
    let profile_path = "/v1/team/repositories/team/repo-a/source-publications";
    let missing_profile_csrf = server.json(
        "POST",
        profile_path,
        vec![
            ("Cookie", new_admin_cookie.clone()),
            ("Origin", ORIGIN.into()),
        ],
        profile_reservation.clone(),
    );
    assert_error(&missing_profile_csrf, 401, "unauthorized");
    let reserved_profile = server.json(
        "POST",
        profile_path,
        bearer(contributor.expose()),
        profile_reservation.clone(),
    );
    assert_eq!(reserved_profile.status, 200);
    let exact_profile_retry = server.json(
        "POST",
        profile_path,
        bearer(contributor.expose()),
        profile_reservation.clone(),
    );
    assert_eq!(exact_profile_retry.status, 200);
    let mut mismatched_profile = profile_reservation.clone();
    mismatched_profile["profile"] = json!("agentscm-source-v1");
    let conflict_profile = server.json(
        "POST",
        profile_path,
        bearer(contributor.expose()),
        mismatched_profile,
    );
    assert_error(&conflict_profile, 409, "conflict");
    let mut unknown_profile = profile_reservation;
    unknown_profile["profile"] = json!("unrecognized-profile");
    let invalid_profile = server.json(
        "POST",
        profile_path,
        bearer(contributor.expose()),
        unknown_profile,
    );
    assert_error(&invalid_profile, 400, "invalid_request");
    let abandoned_profile = server.request(
        "DELETE",
        "/v1/team/repositories/team/repo-a/publications/1/http-v2",
        &bearer(contributor.expose()),
        &[],
    );
    assert_eq!(abandoned_profile.status, 200);

    let range_end = (expected.len() - 1).min(8);
    let ranged_object = server.request(
        "GET",
        &object_path,
        &[
            ("Authorization", format!("Bearer {}", reader.expose())),
            ("Range", format!("bytes=1-{range_end}")),
        ],
        &[],
    );
    assert_eq!(ranged_object.status, 206);
    assert_eq!(
        ranged_object.header("content-range"),
        format!("bytes 1-{range_end}/{}", expected.len())
    );
    assert_eq!(ranged_object.body, expected[1..=range_end]);

    let no_object_credentials = server.request("GET", &object_path, &[], &[]);
    assert_error(&no_object_credentials, 401, "unauthorized");
    let denied_object = server.request(
        "GET",
        &object_path,
        &[("Authorization", format!("Bearer {}", contributor.expose()))],
        &[],
    );
    assert_error(&denied_object, 403, "denied");
    let other_repository_identity = server.request(
        "GET",
        &object_path,
        &[("Authorization", format!("Bearer {}", admin_b.expose()))],
        &[],
    );
    assert_error(&other_repository_identity, 401, "unauthorized");
    let outside_manifest_path = format!(
        "/v1/team/repositories/team/repo-a/objects/{}?generation=1&publication=qualified",
        "f".repeat(64)
    );
    let outside_manifest = server.request(
        "GET",
        &outside_manifest_path,
        &[("Authorization", format!("Bearer {}", reader.expose()))],
        &[],
    );
    assert_error(&outside_manifest, 404, "not_found");
    let error_text = String::from_utf8(outside_manifest.body).unwrap();
    for secret in [reader.expose(), admin_a.expose(), object.id.as_str()] {
        assert!(!error_text.contains(secret));
    }

    let abandoned = server.request(
        "DELETE",
        inspect_path,
        &browser(&new_admin_cookie, &new_admin_csrf, ORIGIN),
        &[],
    );
    assert_eq!(abandoned.status, 200);
    assert_eq!(abandoned.json()["status"], "abandoned");

    let logged_out = server.request(
        "DELETE",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &browser(&new_admin_cookie, &new_admin_csrf, ORIGIN),
        &[],
    );
    assert_eq!(logged_out.status, 200);
    assert_eq!(logged_out.json()["status"], "logged_out");
    let expired_cookie = logged_out.header("set-cookie");
    assert!(expired_cookie.starts_with(&format!("{SESSION_COOKIE}=;")));
    for attribute in [
        "Max-Age=0",
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Strict",
    ] {
        assert!(expired_cookie.split("; ").any(|field| field == attribute));
    }
    let logged_out_session = server.request(
        "GET",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &[("Cookie", new_admin_cookie)],
        &[],
    );
    assert_error(&logged_out_session, 401, "unauthorized");
    let bearer_after_logout = server.request(
        "GET",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &[("Authorization", format!("Bearer {}", admin_a.expose()))],
        &[],
    );
    assert_eq!(bearer_after_logout.status, 200);
    assert_eq!(
        bearer_after_logout.json()["principal"]["principal"],
        "admin"
    );

    let oversized_body = format!("{{\"padding\":\"{}\"}}", "x".repeat(9_000));
    let body_limited = server.request(
        "POST",
        "/v1/team/session",
        &[
            ("Origin", ORIGIN.into()),
            ("Content-Type", "application/json".into()),
        ],
        oversized_body.as_bytes(),
    );
    assert_error(&body_limited, 413, "limit_exceeded");
    let header_limited = server.request(
        "GET",
        "/v1/team/capabilities",
        &[("X-Fill", "x".repeat(8_200))],
        &[],
    );
    assert_error(&header_limited, 413, "limit_exceeded");

    // Database time and authority generation, rather than client time, make
    // browser sessions unusable. Repeated logins remain bounded at eight.
    let expiring_login = login(&server, "repo-a", reader.expose());
    assert_eq!(expiring_login.status, 200);
    let (expiring_cookie, _) = session(&expiring_login);
    let mut sql = cluster.admin();
    sql.execute(
        "UPDATE team_sessions SET expires_at=clock_timestamp()-interval '1 second' \
         WHERE trust_domain='team' AND repository='repo-a' AND principal='reader'",
        &[],
    )
    .unwrap();
    let native_expired = server.request(
        "GET",
        &format!("{native_base}/revision"),
        &[("Cookie", expiring_cookie.clone())],
        &[],
    );
    assert_error(&native_expired, 401, "unauthorized");
    let expired = server.request(
        "GET",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &[("Cookie", expiring_cookie)],
        &[],
    );
    assert_error(&expired, 401, "unauthorized");

    let replacement = login(&server, "repo-a", reader.expose());
    assert_eq!(replacement.status, 200);
    let (generation_cookie, _) = session(&replacement);
    for _ in 1..8 {
        assert_eq!(login(&server, "repo-a", reader.expose()).status, 200);
    }
    let active_sessions: i64 = sql
        .query_one(
            "SELECT count(*) FROM team_sessions WHERE trust_domain='team' \
             AND repository='repo-a' AND principal='reader' AND NOT revoked \
             AND expires_at > clock_timestamp()",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(active_sessions, 8);
    let over_session_limit = login(&server, "repo-a", reader.expose());
    assert_error(&over_session_limit, 413, "limit_exceeded");

    sql.execute(
        "UPDATE team_repositories SET generation=2 \
         WHERE trust_domain='team' AND repository='repo-a'",
        &[],
    )
    .unwrap();
    let invalid_generation = server.request(
        "GET",
        "/v1/team/session?trust_domain=team&repository=repo-a",
        &[("Cookie", generation_cookie.clone())],
        &[],
    );
    assert_error(&invalid_generation, 401, "unauthorized");

    // Generation is an authority/session fence, not deletion of retained
    // history. Fresh bearer authorization can still read the exact completed
    // generation-1 publication, while another repository's credential cannot.
    let historical_read = server.request(
        "GET",
        &object_path,
        &[("Authorization", format!("Bearer {}", reader.expose()))],
        &[],
    );
    assert_eq!(historical_read.status, 200);
    assert_eq!(historical_read.body, expected);
    let stale_cookie_historical_read =
        server.request("GET", &object_path, &[("Cookie", generation_cookie)], &[]);
    assert_error(&stale_cookie_historical_read, 401, "unauthorized");
    let historical_native = server.request(
        "GET",
        &format!("{native_base}/revision"),
        &reader_headers,
        &[],
    );
    assert_eq!(historical_native.status, 200);
    assert_eq!(
        historical_native.json()["source"]["authority_generation"],
        2
    );
    assert_eq!(
        historical_native.json()["source"]["publication_generation"],
        1
    );
    assert_eq!(historical_native.json()["id"], root);
    let old_era_cursor = server.request(
        "GET",
        &format!("{native_base}/history?operation=http-history&limit=1&cursor={history_cursor}"),
        &reader_headers,
        &[],
    );
    assert_error(&old_era_cursor, 401, "unauthorized");
    let cross_repository_historical_read = server.request(
        "GET",
        &object_path,
        &[("Authorization", format!("Bearer {}", admin_b.expose()))],
        &[],
    );
    assert_error(&cross_repository_historical_read, 401, "unauthorized");
}
