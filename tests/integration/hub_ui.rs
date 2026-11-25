//! The Hub serves no dashboard; only the sign-in landing page remains.
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
async fn headless_hub_has_no_dashboard_but_keeps_an_auth_landing() {
    let dir = TempDir::new().unwrap();
    let base = serve(&dir, false).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap();
    for path in [
        "/",
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
        .get(format!("{base}{}", agentscm::hub::landing::landing()))
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
#[cfg(unix)]
fn open_does_not_create_a_service() {
    let dir = TempDir::new().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentscm"))
        .arg("open")
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
