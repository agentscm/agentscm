//! The read-only status page: a built single-page app served at `/ui/`. It signs in with an
//! agent token sent as `Authorization: Bearer`; there are no cookies and no server-side session.
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;

use super::HubState;

// `'unsafe-inline'` for styles only: Ant Design injects style tags at runtime.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; font-src 'self' data:; connect-src 'self'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

#[cfg(feature = "ui")]
const INDEX: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/index.html"));
#[cfg(feature = "ui")]
const SCRIPT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/app.js"));
#[cfg(feature = "ui")]
const STYLE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/app.css"));

#[cfg(not(feature = "ui"))]
const INDEX: &[u8] = b"<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
    <title>AgentSCM</title></head><body><p>This hub was built without the status page. \
    The container image includes it.</p></body></html>";

pub fn routes() -> Router<HubState> {
    let router = Router::new()
        .route(
            "/",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ui/")]) }),
        )
        .route(
            "/ui",
            get(|| async { secured("text/html; charset=utf-8", Redirect::permanent("/ui/")) }),
        )
        .route(
            "/ui/",
            get(|| async { secured("text/html; charset=utf-8", INDEX) }),
        );
    #[cfg(feature = "ui")]
    let router = router
        .route(
            "/ui/app.js",
            get(|| async { secured("text/javascript; charset=utf-8", SCRIPT) }),
        )
        .route(
            "/ui/app.css",
            get(|| async { secured("text/css; charset=utf-8", STYLE) }),
        );
    router
}

fn secured(content_type: &'static str, body: impl IntoResponse) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}
