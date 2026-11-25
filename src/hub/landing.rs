//! The minimal sign-in landing page a browser returns to after Hub authentication.
//!
//! A refused web sign-in arrives as `?status=refused&reason=<code>`. Anyone can forge that link,
//! so the page never reflects it: a known code selects one of the fixed sentences below, and any
//! other value gets the generic one.
use std::collections::HashMap;

use axum::Router;
use axum::extract::Query;
use axum::http::header;
use axum::routing::get;

/// The browser destination after sign-in.
pub const fn landing() -> &'static str {
    "/auth/complete/"
}

/// The sentence for a refusal code, or the generic one when the code is unknown.
fn refusal_sentence(code: &str) -> &'static str {
    match code {
        "login_denied" => {
            "You declined the sign-in at your forge, so nothing changed. Start signing in again to continue."
        }
        "not_allowed" => {
            "This Hub didn't let you in: its admins haven't allowed you, or you have no role on a repository connected to it. Ask a Hub admin or a repository owner."
        }
        "user_disabled" => {
            "Your account is turned off on this Hub. Ask a Hub admin to turn it back on."
        }
        "permission_unavailable" => {
            "Your forge didn't answer the Hub's access check, so nothing changed. Try again in a minute."
        }
        _ => "Sign-in didn't finish. Start signing in again.",
    }
}

fn page(params: &HashMap<String, String>) -> String {
    let (heading, body) = if params.get("status").map(String::as_str) == Some("refused") {
        let code = params.get("reason").map_or("", String::as_str);
        ("Sign-in refused", refusal_sentence(code))
    } else {
        (
            "Return to AgentSCM",
            "Return to your terminal or application for the sign-in result. You can close this window.",
        )
    };
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>AgentSCM sign-in</title><h1>{heading}</h1><p>{body}</p></html>"
    )
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route(
        "/auth/complete/",
        get(|Query(params): Query<HashMap<String, String>>| async move {
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'none'; base-uri 'none'; frame-ancestors 'none'",
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::REFERRER_POLICY, "no-referrer"),
                ],
                page(&params),
            )
        }),
    )
}
