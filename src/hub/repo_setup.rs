//! What is switched on for one repository, as a read-only token may see it: the forge
//! connection's two-way coordination, and the reader's own Slack direct messages. Nothing here
//! carries a token, a Slack ID, a file path or an error detail.
use axum::Router;
use axum::extract::{Json, Path, State};
use axum::routing::get;
use serde_json::{Value, json};

use super::HubState;
use super::http::{ApiError, Reader, blocking};
use super::intent::scope;
use super::sync::find_repository;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/repos/{repo}/forge-coordination",
            get(forge_coordination),
        )
        .route("/v1/hub/repos/{repo}/slack", get(slack))
}

/// Whether the Hub comments on and sets a status for this repository's pull requests. The
/// switch belongs to the forge connection, so it covers every repository that connection has.
async fn forge_coordination(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> Result<Json<Value>, ApiError> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let who = principal.clone();
    let connection = blocking(&state, move || {
        let Some(repository) = find_repository(
            &*store,
            &who.owner.identity.host,
            &who.repository,
            who.repository_id,
        )?
        else {
            return Ok(None);
        };
        store.connection(&repository.connection)
    })
    .await?;
    Ok(Json(match connection {
        None => json!({"repository": principal.repository, "connected": false}),
        Some(connection) => json!({
            "repository": principal.repository,
            "connected": true,
            "forge": {"provider": connection.provider, "host": connection.host,
                "connection_status": connection.status},
            "outbound": {
                "enabled": connection.outbound,
                "switch": "connection",
                // What the Hub sets when enabled: one status that is always success.
                "status_context": "agentscm/coordination",
                "status_state": "success",
                "is_gate": false,
            },
        }),
    }))
}

/// What the Hub's Slack direct messages do for the reader in this repository.
async fn slack(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> Result<Json<Value>, ApiError> {
    scope(&principal, &repo)?;
    let Some(slack) = state.slack.clone() else {
        return Ok(Json(
            json!({"repository": principal.repository, "configured": false}),
        ));
    };
    let inner = state.clone();
    Ok(Json(
        blocking(&state, move || Ok(slack.reader_view(&inner, &principal))).await?,
    ))
}

#[cfg(test)]
mod tests {
    //! `GET /v1/hub/repos/{repo}/forge-coordination` and `/slack`: what is switched on for a
    //! repository, read with a read-only token. The configured Slack case lives with the Slack
    //! fixture in `src/hub/slack/tests.rs`.
    use std::sync::Arc;

    use crate::backend::LocalBackend;
    use crate::hub::connection::NewConnection;
    use crate::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
    use crate::hub::{HubState, router};
    use reqwest::Method;
    use serde_json::Value;
    use tempfile::TempDir;

    const CONNECTION: &str = "con_11111111111111111111111111111111";
    const FORGE: &str = "/v1/hub/repos/acme%2Fwidgets/forge-coordination";
    const SLACK: &str = "/v1/hub/repos/acme%2Fwidgets/slack";

    struct Hub {
        base: String,
        store: Arc<dyn Store>,
        client: reqwest::Client,
        _dir: TempDir,
    }

    impl Hub {
        async fn start() -> Self {
            let dir = TempDir::new().unwrap();
            let database = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
            let store = tokio::task::spawn_blocking(move || store::open(&database))
                .await
                .unwrap()
                .unwrap();
            let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = router(HubState::new(store.clone(), Arc::new(objects)));
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                base,
                store,
                client: reqwest::Client::new(),
                _dir: dir,
            }
        }

        fn connect(&self, repository: &str, repository_id: i64) {
            // One connection serves every repository a test connects.
            let connection = match self.store.connection(CONNECTION).unwrap() {
                Some(connection) => connection,
                None => self
                    .store
                    .insert_connection(
                        &NewConnection {
                            id: CONNECTION.into(),
                            provider: "github".into(),
                            host: "github.com".into(),
                            kind: "pat".into(),
                            app_id: None,
                            app_slug: None,
                            acts_as: Some("synthetic".into()),
                        },
                        "test",
                    )
                    .unwrap(),
            };
            self.store
                .upsert_repository(
                    &connection.id,
                    "github.com",
                    repository_id,
                    repository,
                    None,
                    None,
                )
                .unwrap();
        }

        fn token(&self, id: i64, repository: &str, repository_id: i64, read_only: bool) -> String {
            let owner = self
                .store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id: id,
                    },
                    &format!("contributor{id}"),
                )
                .unwrap();
            let (_, secret, _) = mint_bound_agent_token(
                &*self.store,
                &owner.id,
                "test-agent",
                repository,
                repository_id,
                3600,
                read_only,
            )
            .unwrap();
            secret.expose().to_owned()
        }

        async fn get(&self, path: &str, secret: Option<&str>) -> (u16, Value) {
            let mut request = self
                .client
                .request(Method::GET, format!("{}{path}", self.base));
            if let Some(secret) = secret {
                request = request.bearer_auth(secret);
            }
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forge_coordination_shows_the_connection_switch_to_its_own_repository_only() {
        let hub = Hub::start().await;
        hub.connect("acme/widgets", 100);
        let reader = hub.token(1, "acme/widgets", 100, true);
        let agent = hub.token(2, "acme/widgets", 100, false);
        let elsewhere = hub.token(3, "acme/private", 200, true);
        let unconnected = hub.token(4, "acme/widgets-fork", 300, true);

        assert_eq!(hub.get(FORGE, None).await.0, 401);
        assert_eq!(hub.get(FORGE, Some("nope")).await.0, 401);
        let (status, body) = hub.get(FORGE, Some(&elsewhere)).await;
        assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));

        // Off by default, and the reply says what turning it on would do.
        let (status, off) = hub.get(FORGE, Some(&reader)).await;
        assert_eq!(status, 200, "{off}");
        assert_eq!(off["repository"], "acme/widgets");
        assert_eq!(off["connected"], true);
        assert_eq!(off["forge"]["provider"], "github");
        assert_eq!(off["forge"]["host"], "github.com");
        assert_eq!(off["forge"]["connection_status"], "enabled");
        assert_eq!(off["outbound"]["enabled"], false);
        assert_eq!(off["outbound"]["switch"], "connection");
        assert_eq!(off["outbound"]["status_context"], "agentscm/coordination");
        assert_eq!(off["outbound"]["status_state"], "success");
        assert_eq!(off["outbound"]["is_gate"], false);

        hub.store
            .set_connection_outbound(CONNECTION, true, "test")
            .unwrap();
        let (_, on) = hub.get(FORGE, Some(&agent)).await;
        assert_eq!(on["outbound"]["enabled"], true);

        // A token whose repository the Hub has no connection for is told so, not refused.
        let (status, none) = hub
            .get(
                "/v1/hub/repos/acme%2Fwidgets-fork/forge-coordination",
                Some(&unconnected),
            )
            .await;
        assert_eq!(status, 200, "{none}");
        assert_eq!(none["connected"], false);
        assert!(none.get("outbound").is_none(), "{none}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slack_says_when_the_hub_has_none() {
        let hub = Hub::start().await;
        let reader = hub.token(1, "acme/widgets", 100, true);
        let elsewhere = hub.token(2, "acme/private", 200, true);

        assert_eq!(hub.get(SLACK, None).await.0, 401);
        let (status, body) = hub.get(SLACK, Some(&elsewhere)).await;
        assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));
        let (status, none) = hub.get(SLACK, Some(&reader)).await;
        assert_eq!(status, 200, "{none}");
        assert_eq!(none["repository"], "acme/widgets");
        assert_eq!(none["configured"], false);
        assert!(none.get("bound").is_none(), "{none}");
    }
}
