//! The status page route: served without credentials, with a strict CSP; the built files are
//! embedded only with the `ui` feature.
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store;
use agentscm::hub::{HubState, router};
use reqwest::redirect::Policy;
use tempfile::TempDir;

/// Serves a hub on a loopback port; the directory must outlive it.
async fn serve(dir: &TempDir, demo: bool) -> String {
    let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
    let store = store::open(&url).unwrap();
    let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
    let state = HubState::new(store, Arc::new(objects));
    let app = router(if demo { state.with_demo() } else { state });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_says_demo_only_for_a_demo_hub() {
    let dir = TempDir::new().unwrap();
    for demo in [false, true] {
        let base = serve(&dir, demo).await;
        let body = reqwest::get(format!("{base}/v1/hub/health"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let health: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(health.get("demo"), demo.then_some(&serde_json::json!(true)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_is_served_with_a_content_security_policy() {
    let dir = TempDir::new().unwrap();
    let base = serve(&dir, false).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    let get = |path: &str| client.get(format!("{base}{path}")).send();

    let page = get("/ui/").await.unwrap();
    assert_eq!(page.status(), 200);
    let header = |name: &str| page.headers()[name].to_str().unwrap().to_owned();
    assert_eq!(header("content-type"), "text/html; charset=utf-8");
    let csp = header("content-security-policy");
    assert_eq!(
        csp,
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; font-src 'self' data:; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'"
    );
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(header("referrer-policy"), "no-referrer");
    assert_eq!(header("cache-control"), "no-cache");

    for (path, status, target) in [("/", 302, "/ui/"), ("/ui", 308, "/ui/")] {
        let moved = get(path).await.unwrap();
        assert_eq!(moved.status(), status, "{path}");
        assert_eq!(moved.headers()["location"], target, "{path}");
    }
    // Without the feature the page is a notice and there are no assets.
    let script = get("/ui/app.js").await.unwrap().status();
    assert_eq!(script, if cfg!(feature = "ui") { 200 } else { 404 });
    assert_eq!(get("/ui/other").await.unwrap().status(), 404);
}
