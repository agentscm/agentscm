//! Headless native routes and the optional server dashboard remain distinct.
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
#[cfg(feature = "ui")]
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
         img-src 'self' data:; font-src 'self' data:; connect-src 'self'; manifest-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
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
    // The install manifest and its icons ship with the page.
    for (path, kind) in [
        ("/ui/manifest.webmanifest", "application/manifest+json"),
        ("/ui/icon-192.png", "image/png"),
        ("/ui/icon-512.png", "image/png"),
    ] {
        let asset = get(path).await.unwrap();
        assert_eq!(
            asset.status(),
            if cfg!(feature = "ui") { 200 } else { 404 },
            "{path}"
        );
        if cfg!(feature = "ui") {
            assert_eq!(asset.headers()["content-type"], kind, "{path}");
            assert!(
                asset.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("manifest-src 'self'")
            );
        }
    }
    assert_eq!(get("/ui/other").await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(feature = "ui"))]
async fn headless_hub_has_no_dashboard_but_keeps_an_auth_landing() {
    let dir = TempDir::new().unwrap();
    let base = serve(&dir, false).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    // `hub login` sends the browser to `/#/signed-in?…`; a headless Hub hands it to its landing page.
    let response = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers()["location"], agentscm::hub::ui::landing());
    for path in [
        "/ui",
        "/ui/",
        "/ui/app.js",
        "/ui/app.css",
        "/ui/manifest.webmanifest",
        "/ui/icon-192.png",
        "/ui/icon-512.png",
    ] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
    let response = client
        .get(format!("{base}{}", agentscm::hub::ui::landing()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Return to your terminal or application")
    );
}

#[test]
#[cfg(all(unix, not(feature = "ui")))]
fn headless_open_and_front_door_do_not_create_a_service() {
    let dir = TempDir::new().unwrap();
    for args in [vec!["open"], vec!["serve", "--front-door"]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
            .args(args)
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .arg("--data-dir")
            .arg(dir.path().join("store"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("dashboard"));
        assert!(!dir.path().join("config/agentscm/front-door").exists());
        assert!(!dir.path().join("store").exists());
    }
}
