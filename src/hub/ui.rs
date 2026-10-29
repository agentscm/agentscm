//! The Hub UI at `/ui/`: read-only status tokens and opt-in workspace sessions. The local service
//! (`agentscm serve --front-door`) serves the same page for this computer's repositories.
use std::sync::LazyLock;

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};

// `'unsafe-inline'` for styles only: React sets inline `style` attributes, which `style-src` governs.
// Fonts and images are inlined into app.css as `data:` URIs, hence `font-src` and `img-src` allow `data:`.
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
    Release binaries and the container image include it. From source, run `npm ci &amp;&amp; npm run build` \
    in ui/, then `cargo build --features ui`.</p></body></html>";

static INDEX_TAG: LazyLock<String> = LazyLock::new(|| tag(INDEX));
#[cfg(feature = "ui")]
static SCRIPT_TAG: LazyLock<String> = LazyLock::new(|| tag(SCRIPT));
#[cfg(feature = "ui")]
static STYLE_TAG: LazyLock<String> = LazyLock::new(|| tag(STYLE));

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
            get(|headers: HeaderMap| async move {
                asset(&headers, "text/html; charset=utf-8", INDEX, &INDEX_TAG)
            }),
        );
    #[cfg(feature = "ui")]
    let router = router
        .route(
            "/ui/app.js",
            get(|headers: HeaderMap| async move {
                asset(
                    &headers,
                    "text/javascript; charset=utf-8",
                    SCRIPT,
                    &SCRIPT_TAG,
                )
            }),
        )
        .route(
            "/ui/app.css",
            get(|headers: HeaderMap| async move {
                asset(&headers, "text/css; charset=utf-8", STYLE, &STYLE_TAG)
            }),
        );
    router
}

/// A strong validator: the asset's SHA-256, which changes exactly when the embedded bytes do.
fn tag(body: &[u8]) -> String {
    format!("\"{}\"", super::store::lower_hex_of(&Sha256::digest(body)))
}

/// `no-cache` still lets a browser keep the asset; it revalidates and gets 304 while it is current.
fn asset(
    headers: &HeaderMap,
    content_type: &'static str,
    body: &'static [u8],
    tag: &'static str,
) -> Response {
    let current = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').map(str::trim).any(|candidate| {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == tag
            })
        });
    let (status, body) = if current {
        (StatusCode::NOT_MODIFIED, &[][..])
    } else {
        (StatusCode::OK, body)
    };
    (status, [(header::ETAG, tag)], secured(content_type, body)).into_response()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tower_service::Service;

    async fn get(path: &str, if_none_match: Option<&str>) -> Response {
        let mut request = axum::extract::Request::get(path);
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        let request = request.body(axum::body::Body::empty()).unwrap();
        routes::<()>().call(request).await.unwrap()
    }

    #[tokio::test]
    async fn a_current_etag_gets_304_without_the_body() {
        let first = get("/ui/", None).await;
        assert_eq!(first.status(), StatusCode::OK);
        let tag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
        assert_eq!(tag.len(), 66, "{tag}");
        let again = get("/ui/", Some(&format!("\"other\", W/{tag}"))).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(again.headers()[header::ETAG], tag.as_str());
        let body = axum::body::to_bytes(again.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
        assert_eq!(
            get("/ui/", Some("\"stale\"")).await.status(),
            StatusCode::OK
        );
    }
}
