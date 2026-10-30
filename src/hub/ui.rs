//! The Hub UI at `/ui/`: read-only status tokens and opt-in workspace sessions. The local service
//! (`agentscm serve --front-door`) serves the same page for this computer's repositories.
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;

// `'unsafe-inline'` for styles only: React sets inline `style` attributes, which `style-src` governs.
// Fonts and images are inlined into app.css as `data:` URIs, hence `font-src` and `img-src` allow `data:`.
// `manifest-src` lets the browser fetch /ui/manifest.webmanifest, which makes the page installable as an app.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; font-src 'self' data:; connect-src 'self'; manifest-src 'self'; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[cfg(feature = "ui")]
const INDEX: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/index.html"));
#[cfg(feature = "ui")]
const SCRIPT: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/app.js"));
#[cfg(feature = "ui")]
const STYLE: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/app.css"));
#[cfg(feature = "ui")]
const MANIFEST: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/ui/dist/manifest.webmanifest"
));
#[cfg(feature = "ui")]
const ICON_192: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/icon-192.png"));
#[cfg(feature = "ui")]
const ICON_512: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist/icon-512.png"));

#[cfg(not(feature = "ui"))]
const INDEX: &[u8] = b"<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
    <title>AgentSCM</title></head><body><p>This hub was built without the status page. \
    Release binaries and the container image include it. From source, run `npm ci &amp;&amp; npm run build` \
    in ui/, then `cargo build --features ui`.</p></body></html>";

/// The page's routes. They hold no state, so the local service mounts the same page.
pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
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
        )
        .route(
            "/ui/manifest.webmanifest",
            get(|| async { secured("application/manifest+json", MANIFEST) }),
        )
        .route(
            "/ui/icon-192.png",
            get(|| async { secured("image/png", ICON_192) }),
        )
        .route(
            "/ui/icon-512.png",
            get(|| async { secured("image/png", ICON_512) }),
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
