//! Shared local-only Hub and synthetic forge fixture; no live accounts.
#![allow(
    dead_code,
    reason = "Shared fixture is consumed by separate integration-test crates."
)]
use std::collections::HashMap;
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::login::{Login, LoginConfig};
use agentscm::hub::store::{self, Store};
use agentscm::hub::workspace::{
    Identity, Workspace, WorkspaceAction, WorkspaceActor, WorkspaceConfig, api_error,
};
use agentscm::hub::{HubState, router};
use agentscm::team::auth::Role;
use axum::extract::{FromRequestParts, Query, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use tempfile::TempDir;

pub(super) struct Env {
    pub(super) dir: TempDir,
    pub(super) store: Arc<dyn Store>,
    pub(super) workspace: Arc<Workspace>,
    pub(super) host: String,
    pub(super) base: String,
    pub(super) client: Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Env {
    pub(super) async fn new() -> Self {
        Self::with_state(|state, _| state).await
    }

    pub(super) async fn with_state(
        build: impl FnOnce(HubState, Arc<Workspace>) -> HubState,
    ) -> Self {
        let forge_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let forge_base = format!("http://{}", forge_listener.local_addr().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let registered_redirect = format!("{base}/v1/hub/login/github/callback");
        let forge=Router::new()
          .route("/login/oauth/authorize",get(move |Query(query):Query<HashMap<String,String>>| async move { synthetic_authorize(query,&registered_redirect) }))
          .route("/login/oauth/access_token",post(|Form(form):Form<HashMap<String,String>>|async move {
              assert_eq!(form["client_id"],"synthetic-client");
              assert_eq!(form["client_secret"],"synthetic-secret");
              assert_eq!(form["code_verifier"].len(),64);
              Json(json!({"access_token":format!("synthetic-user-{}",form["code"]),"expires_in":3600}))
          }))
          .route("/user",get(|headers:HeaderMap|async move {
              let id=headers[header::AUTHORIZATION].to_str().unwrap().strip_prefix("Bearer synthetic-user-").unwrap().parse::<i64>().unwrap();
              Json(json!({"id":id,"login":format!("synthetic-user-{id}")}))
          }));
        let forge_task = tokio::spawn(async move {
            axum::serve(forge_listener, forge).await.unwrap();
        });
        let dir = TempDir::new().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let host = forge_base.trim_start_matches("http://").to_owned();
        let config = WorkspaceConfig {
            workspace_id: "synthetic-workspace".into(),
            name: "Synthetic workspace".into(),
            database: dir.path().join("workspace.sqlite"),
            initial_owner: Identity {
                provider: "github".into(),
                host: host.clone(),
                user_id: 101,
            },
        };
        let workspace = Arc::new(Workspace::open(config, store.clone()).unwrap());
        let vars = HashMap::from([
            ("AGENTSCM_GITHUB_CLIENT_ID", "synthetic-client".to_owned()),
            (
                "AGENTSCM_GITHUB_CLIENT_SECRET",
                "synthetic-secret".to_owned(),
            ),
            ("AGENTSCM_GITHUB_URL", forge_base.clone()),
            ("AGENTSCM_GITHUB_API_URL", forge_base),
        ]);
        let login =
            Login::new(LoginConfig::from_lookup(&base, |key| vars.get(key).cloned()).unwrap())
                .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        )
        .with_login(login)
        .with_workspace(workspace.clone());
        let state = build(state, workspace.clone());
        // Test-only route exercises the real actor extractor and the dispatch
        // authorization service without adding a native execution fixture.
        let check_workspace = workspace.clone();
        let check_state = state.clone();
        let check = post(
            move |Query(query): Query<HashMap<String, String>>, request: Request| {
                let workspace = check_workspace.clone();
                let state = check_state.clone();
                async move {
                    let (mut parts, _) = request.into_parts();
                    let actor = WorkspaceActor::from_request_parts(&mut parts, &state).await?;
                    workspace
                        .authorize(
                            &actor,
                            &query["repository"],
                            WorkspaceAction::ExecuteWorkflow,
                        )
                        .map_err(api_error)?;
                    Ok::<_, agentscm::hub::http::ApiError>(StatusCode::NO_CONTENT)
                }
            },
        );
        let app = router(state).route("/synthetic/execute", check);
        let hub_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            dir,
            store,
            workspace,
            host,
            base,
            client,
            tasks: vec![forge_task, hub_task],
        }
    }
    pub(super) fn identity(&self, id: i64) -> Value {
        json!({"provider":"github","host":self.host,"user_id":id})
    }
    pub(super) async fn signin(&self, id: i64) -> (u16, String) {
        let start = self
            .client
            .get(format!("{}/v1/hub/login/github?workspace=1", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), StatusCode::FOUND);
        let location = Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let pending_cookie = start.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let state = location
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let callback = self
            .client
            .get(format!(
                "{}/v1/hub/login/github/callback?state={state}&code={id}",
                self.base
            ))
            .header(header::COOKIE, pending_cookie)
            .send()
            .await
            .unwrap();
        let status = callback.status().as_u16();
        if status == 302 {
            assert_eq!(callback.headers()[header::LOCATION], "/ui/#workspace");
            let cookie = callback
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .map(|value| value.to_str().unwrap())
                .find(|value| value.starts_with("__Host-agentscm_workspace="))
                .unwrap();
            assert!(
                cookie.contains("HttpOnly")
                    && cookie.contains("Secure")
                    && cookie.contains("SameSite=Lax")
                    && cookie.contains("Max-Age=900")
            );
            (status, cookie.split(';').next().unwrap().to_owned())
        } else {
            (status, String::new())
        }
    }
    pub(super) async fn request(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        csrf: Option<&str>,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.base, path));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    pub(super) async fn session(&self, cookie: &str) -> Value {
        let (status, session) = self
            .request(
                Method::GET,
                "/v1/hub/workspace/session",
                Some(cookie),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{session}");
        session
    }
    pub(super) fn member(
        &self,
        id: i64,
        roles: &[Role],
        enabled: bool,
        generation: i64,
        request: &str,
    ) -> Value {
        json!({"identity":self.identity(id),"roles":roles,"repository_ids":["synthetic/project"],"enabled":enabled,"expected_generation":generation,"request_id":request})
    }
}

fn html_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn synthetic_authorize(query: HashMap<String, String>, registered_redirect: &str) -> Response {
    let get = |key: &str| query.get(key).map(String::as_str).unwrap_or_default();
    let redirect = Url::parse(registered_redirect).unwrap();
    if redirect.scheme() != "http"
        || redirect.host_str() != Some("127.0.0.1")
        || get("redirect_uri") != registered_redirect
        || get("client_id") != "synthetic-client"
        || get("state").len() != 64
        || !get("state").bytes().all(|b| b.is_ascii_hexdigit())
        || get("code_challenge_method") != "S256"
        || get("code_challenge").len() != 43
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut links = String::new();
    for id in [101, 202] {
        let mut callback = redirect.clone();
        callback
            .query_pairs_mut()
            .append_pair("state", get("state"))
            .append_pair("code", &id.to_string());
        links.push_str(&format!(
            "<p><a href=\"{}\">Sign in as synthetic user {id}</a></p>",
            html_text(callback.as_str())
        ));
    }
    ([(header::CACHE_CONTROL,"no-store"),(header::REFERRER_POLICY,"no-referrer"),(header::CONTENT_SECURITY_POLICY,"default-src 'none'; base-uri 'none'; frame-ancestors 'none'")],Html(format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Synthetic forge sign-in</title><h1>Synthetic forge sign-in</h1><p>Local test identities for this test Hub. No external account is used.</p>{links}</html>"))).into_response()
}

#[cfg(unix)]
pub(super) struct NativeSourceProfile {
    pub(super) profile: agentscm::hub::workspace_execution::ExecutionProfile,
    pub(super) subject: agentscm::hub::workspace_execution::RunSubject,
    pub(super) context: agentscm::context::ReviewContext,
    pub(super) plan: agentscm::skill_workflow::CompiledPlan,
}

/// Installs a source-only requirement. No command or provider executable runs.
#[cfg(unix)]
pub(super) fn native_source_profile(root: &std::path::Path) -> NativeSourceProfile {
    use agentscm::authority::{Authority, workflows::WorkflowRequirement};
    use agentscm::checks::SourcePathExpectation;
    use agentscm::context::{REVIEW_CONTEXT_VERSION, ReviewContext};
    use agentscm::hub::workspace_execution::{ExecutionProfile, RunSubject};
    use agentscm::model::FileChange;
    use agentscm::skill_workflow::{
        self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
    };
    use agentscm::store::{Engine, digest};
    let native = root.join("native");
    std::fs::create_dir_all(&native).unwrap();
    let engine = Engine::open(&native).unwrap();
    let revision = engine
        .checkpoint(
            None,
            &[
                FileChange {
                    path: "skills/check/SKILL.md".into(),
                    content: Some(b"# Check synthetic source\n".to_vec()),
                },
                FileChange {
                    path: "src/example.rs".into(),
                    content: Some(b"fn example() {}\n".to_vec()),
                },
            ],
        )
        .unwrap();
    let mut authority = Authority::open(&native).unwrap();
    let repository = authority.repository_id().unwrap();
    let definition = WorkflowDefinition {
        version: 1,
        name: "synthetic-http-check".into(),
        source: WorkflowSource {
            repository: repository.clone(),
            revision: revision.clone(),
            policy_epoch: 0,
        },
        skill: SkillReference {
            entrypoint: "skills/check/SKILL.md".into(),
            resources: vec![],
        },
        steps: vec![StepDefinition {
            id: "source".into(),
            needs: vec![],
            required: true,
            action: StepAction::SourcePaths {
                expectations: vec![SourcePathExpectation {
                    path: "src/example.rs".into(),
                    sha256: digest(b"fn example() {}\n"),
                }],
            },
        }],
    };
    let initial = skill_workflow::compile(&native, &definition).unwrap();
    let requirement = WorkflowRequirement::from_plan("main", &initial).unwrap();
    let requirement_digest = authority
        .install_workflow_requirement(&requirement)
        .unwrap();
    let public = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32])
        .verifying_key()
        .to_bytes();
    let public_hex = public
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let enrollment = skill_workflow::ExecutorEnrollment {
        version: 1,
        registration_id: "registration-http-1".into(),
        executor_id: "executor-http-1".into(),
        operator_id: "operator-http-1".into(),
        repository: repository.clone(),
        step_ids: vec!["source".into()],
        registration_epoch: 1,
        verification_key_id: agentscm::policy::issuer_key_id(&public_hex).unwrap(),
        verification_key_sha256: digest(&public),
        signature_algorithm: skill_workflow::ObservationSignatureAlgorithm::Ed25519,
        issued_at_ms: 1,
    };
    authority
        .enroll_workflow_executor(&enrollment, &public_hex)
        .unwrap();
    let context:ReviewContext=serde_json::from_value(json!({
        "version":REVIEW_CONTEXT_VERSION,"change":{"repository":repository,"id":"work"},"change_version":1,
        "revision":revision,"base":null,"target":"main","policy_epoch":authority.policy_epoch().unwrap(),
        "intent":"Verify synthetic source through the shared workspace", "acceptance_criteria":["The pinned source path matches"],"scope":["src/example.rs"],"exclusions":[],"omissions":[],"dependencies":[],
        "recipe":{"state":"digest","sha256":"a".repeat(64)},"toolchain":{"state":"digest","sha256":"b".repeat(64)},"environment":{"state":"digest","sha256":"c".repeat(64)},"required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
    })).unwrap();
    let change = authority.start_change(&context).unwrap();
    let plan =
        skill_workflow::compile(&native, &requirement.instantiate(&context).unwrap()).unwrap();
    NativeSourceProfile {
        profile: ExecutionProfile {
            repository_id: "synthetic/project".into(),
            profile_id: "checks".into(),
            native_repository: context.change.repository.clone(),
            data_dir: native,
            target: "main".into(),
            requirement_digest: requirement_digest.clone(),
            docker: "/usr/bin/false".into(),
            docker_socket: root.join("not-used.socket"),
            claude: "/usr/bin/false".into(),
        },
        subject: RunSubject {
            change_id: context.change.id.clone(),
            change_version: change.version,
            context_digest: change.context_digest,
            revision: context.revision.clone(),
            requirement_digest,
        },
        context,
        plan,
    }
}
