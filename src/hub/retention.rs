//! What the Hub keeps for one repository, and for how long. A read-only token may read it: the
//! numbers are the Hub's own limits, and the one measured figure is the repository's retained
//! stand-up bytes. Nothing here names a person, a Work item or a record's content.
use axum::Router;
use axum::extract::{Json, Path, State};
use axum::routing::get;
use serde_json::{Value, json};

use super::capacity::RETAIN_SECONDS as CAPACITY_REFUSAL_RETAIN_SECONDS;
use super::catchup::{
    CATCHUP_DELIVERY_CAPACITY_BYTES, MAX_CATCHUP_MERGE_BYTES, MAX_CATCHUP_MERGE_ROWS,
};
use super::http::{ApiError, Reader, blocking};
use super::intent::scope;
use super::pickup::{MAX_PICKUPS_PER_OWNER, PICKUP_RETAIN_SECONDS, PICKUP_TTL_SECONDS};
use super::work::{WORK_PAYLOAD_CAPACITY_BYTES, WorkRepositoryScope};
use super::{HubState, WORK_AUTO_COMPACTION_AGE_SECONDS, WORK_COMPACTION_INTERVAL};

pub fn routes() -> Router<HubState> {
    Router::new().route("/v1/hub/repos/{repo}/retention", get(show))
}

/// The repository's retention facts: configured limits, plus the stand-up bytes it holds now.
async fn show(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> Result<Json<Value>, ApiError> {
    scope(&principal, &repo)?;
    let used_scope = WorkRepositoryScope {
        provider: principal.owner.identity.provider.clone(),
        host: principal.owner.identity.host.clone(),
        repository: principal.repository.clone(),
        repository_id: principal.repository_id,
    };
    let store = state.store.clone();
    let used = blocking(&state, move || store.work_payload_used(&used_scope)).await?;
    Ok(Json(json!({
        "repository": principal.repository,
        "shared_conversations": {
            "retention_days": state.shared_conversation_retention_days,
            "readable_by": ["author", "hub_admin"],
            "deletable_early_by": ["author", "hub_admin"],
        },
        "work": {
            "payload_capacity_bytes": WORK_PAYLOAD_CAPACITY_BYTES,
            "payload_used_bytes": used,
            "compaction": {
                "after_seconds": WORK_AUTO_COMPACTION_AGE_SECONDS,
                "interval_seconds": WORK_COMPACTION_INTERVAL.as_secs(),
            },
        },
        "merge_observations": {
            "max_rows": MAX_CATCHUP_MERGE_ROWS,
            "max_bytes": MAX_CATCHUP_MERGE_BYTES,
        },
        "catchup_deliveries": {"capacity_bytes": CATCHUP_DELIVERY_CAPACITY_BYTES},
        "handoff_pickups": {
            "waits_seconds": PICKUP_TTL_SECONDS,
            "retain_seconds": PICKUP_RETAIN_SECONDS,
            "max_per_person": MAX_PICKUPS_PER_OWNER,
        },
        "capacity_refusals": {"retain_seconds": CAPACITY_REFUSAL_RETAIN_SECONDS},
    })))
}

#[cfg(test)]
mod tests {
    //! `GET /v1/hub/repos/{repo}/retention`: a repository's retention facts for a read-only token.
    use std::sync::Arc;

    use crate::backend::LocalBackend;
    use crate::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token};
    use crate::hub::work::WORK_PAYLOAD_CAPACITY_BYTES;
    use crate::hub::{HubState, WORK_AUTO_COMPACTION_AGE_SECONDS, router};
    use reqwest::Method;
    use serde_json::{Value, json};
    use tempfile::TempDir;

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

        async fn token(
            &self,
            id: i64,
            repository: &str,
            repository_id: i64,
            read_only: bool,
        ) -> String {
            let store = self.store.clone();
            let repository = repository.to_owned();
            tokio::task::spawn_blocking(move || {
                let owner = store
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
                    &*store,
                    &owner.id,
                    "test-agent",
                    &repository,
                    repository_id,
                    3600,
                    read_only,
                )
                .unwrap();
                secret.expose().to_owned()
            })
            .await
            .unwrap()
        }

        async fn call(
            &self,
            method: Method,
            path: &str,
            secret: Option<&str>,
            body: Option<Value>,
        ) -> (u16, Value) {
            let mut request = self.client.request(method, format!("{}{path}", self.base));
            if let Some(secret) = secret {
                request = request.bearer_auth(secret);
            }
            if let Some(body) = body {
                request = request
                    .header("content-type", "application/json")
                    .body(body.to_string());
            }
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }
    }

    const ROUTE: &str = "/v1/hub/repos/acme%2Fwidgets/retention";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_only_token_reads_its_own_repository_retention_only() {
        let hub = Hub::start().await;
        let agent = hub.token(1, "acme/widgets", 101, false).await;
        let reader = hub.token(2, "acme/widgets", 101, true).await;
        let elsewhere = hub.token(3, "acme/private", 202, true).await;

        // Signed out and the wrong repository are refused; the wrong repository is not revealed.
        assert_eq!(hub.call(Method::GET, ROUTE, None, None).await.0, 401);
        assert_eq!(
            hub.call(Method::GET, ROUTE, Some("nope"), None).await.0,
            401
        );
        let (status, body) = hub.call(Method::GET, ROUTE, Some(&elsewhere), None).await;
        assert_eq!((status, body["error"].as_str()), (404, Some("not_found")));
        let (status, _) = hub
            .call(
                Method::GET,
                "/v1/hub/repos/acme%2Fprivate/retention",
                Some(&reader),
                None,
            )
            .await;
        assert_eq!(status, 404);
        assert_eq!(
            hub.call(Method::POST, ROUTE, Some(&reader), Some(json!({})))
                .await
                .0,
            405
        );

        // A repository with no stand-up history holds no bytes.
        let (status, quiet) = hub.call(Method::GET, ROUTE, Some(&reader), None).await;
        assert_eq!(status, 200, "{quiet}");
        assert_eq!(quiet["repository"], "acme/widgets");
        assert_eq!(quiet["shared_conversations"]["retention_days"], 30);
        assert_eq!(
            quiet["shared_conversations"]["readable_by"],
            json!(["author", "hub_admin"])
        );
        assert_eq!(
            quiet["work"]["payload_capacity_bytes"],
            WORK_PAYLOAD_CAPACITY_BYTES
        );
        assert_eq!(quiet["work"]["payload_used_bytes"], 0);
        assert_eq!(
            quiet["work"]["compaction"]["after_seconds"],
            WORK_AUTO_COMPACTION_AGE_SECONDS
        );
        assert_eq!(quiet["work"]["compaction"]["interval_seconds"], 3600);
        assert_eq!(quiet["merge_observations"]["max_rows"], 100_000);
        assert_eq!(quiet["handoff_pickups"]["max_per_person"], 200);
        assert!(quiet["catchup_deliveries"]["capacity_bytes"].is_i64());

        // The agent's own write is counted, and the agent token reads the same figure.
        let (status, started) = hub
            .call(
                Method::POST,
                "/v1/hub/standup/start",
                Some(&agent),
                Some(
                    json!({"request_key": "retention-goal", "title": "Tidy request validation",
                    "session": "session-a", "scope": ["src/request.rs"],
                    "branch": "feat/validation", "worktree": "checkout",
                    "next_action": "Add boundary tests"}),
                ),
            )
            .await;
        assert_eq!(status, 200, "{started}");
        let (_, busy) = hub.call(Method::GET, ROUTE, Some(&reader), None).await;
        let used = busy["work"]["payload_used_bytes"].as_i64().unwrap();
        assert!(
            used > 0 && used < WORK_PAYLOAD_CAPACITY_BYTES,
            "used {used}"
        );
        let (_, same) = hub.call(Method::GET, ROUTE, Some(&agent), None).await;
        assert_eq!(same["work"]["payload_used_bytes"], used);

        // Another repository's figure stays its own.
        let (_, other) = hub
            .call(
                Method::GET,
                "/v1/hub/repos/acme%2Fprivate/retention",
                Some(&elsewhere),
                None,
            )
            .await;
        assert_eq!(other["work"]["payload_used_bytes"], 0);
    }
}
