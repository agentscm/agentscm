//! Opt-in external CI and deployment observations for merged publications, end to end: a real
//! hub store, an in-process sync and a fake GitHub. Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_github.rs"]
mod fake_github;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::HubState;
use agentscm::hub::external_runs::{ExternalKind, ExternalOutcome, ExternalRun};
use agentscm::hub::publish::{NewPublication, Update, derive_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use fake_github::{Failure, FakeGithub, Pull, Repo, deployment_json, run_json, status_json};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 100;
const PAT: &str = "ghp_synthetic_pat";
const MERGED: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";
const EARLY: &str = "2023-11-14T22:13:20Z";
const LATE: &str = "2023-11-14T22:15:00Z";

struct Env {
    fake: FakeGithub,
    state: HubState,
    store: Arc<dyn Store>,
    repo: String,
    token: String,
    secret: String,
    _root: TempDir,
}

fn manifest(commit: &str, branch: &str) -> Value {
    json!({
        "revision": "a".repeat(64), "event_seq": 1, "commit": commit,
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": branch, "base": "main",
        "title": "t", "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": "f".repeat(64), "size": 5}]
    })
}

impl Env {
    async fn start(enabled: bool) -> Self {
        let root = TempDir::new().unwrap();
        let fake = FakeGithub::start().await;
        fake.with(|s| {
            s.repos.insert(REPO_ID, Repo::new(REPO_ID, REPO));
            s.pats
                .insert(PAT.into(), ("robot".into(), 77, [REPO_ID].into()));
        });
        let dir = root.path().join("hub");
        std::fs::create_dir(&dir).unwrap();
        let store = store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display())).unwrap();
        let secrets = dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let base = fake.base.clone();
        let config =
            SyncConfig::from_lookup("http://127.0.0.1:1", secrets, 14, |name| match name {
                "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(base.clone()),
                "AGENTSCM_GITHUB_PAT" => Some(PAT.into()),
                "AGENTSCM_GITHUB_EXTERNAL_RUNS" => enabled.then(|| " Acme/Widgets ".into()),
                _ => None,
            })
            .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.join("objects")).unwrap()),
        )
        .with_sync(Sync::new(config).unwrap());
        sync::start(&state).await.unwrap();
        let host = fake.base.trim_start_matches("http://").to_owned();
        let setup = store.clone();
        let (repo, token, secret) = tokio::task::spawn_blocking(move || {
            let connection = setup.pat_connection(&host).unwrap().unwrap().id;
            setup
                .upsert_repository(&connection, &host, REPO_ID, REPO, None, None)
                .unwrap();
            let owner = setup
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: host.clone(),
                        user_id: 5,
                    },
                    "alice",
                )
                .unwrap();
            let (token, secret) =
                mint_agent_token(&*setup, &owner.id, "agent-a", REPO, 3600).unwrap();
            let repo = setup.repository_named(&host, REPO).unwrap().unwrap().id;
            (repo, token, secret.expose().to_owned())
        })
        .await
        .unwrap();
        Self {
            fake,
            state,
            store,
            repo,
            token,
            secret,
            _root: root,
        }
    }

    /// A publication of pull request `number`; merged with `merge` when given.
    async fn publish(&self, number: i64, commit: &str, merge: Option<&str>) -> String {
        let (store, token, commit) = (self.store.clone(), self.token.clone(), commit.to_owned());
        let merge = merge.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let branch = format!("agents/work-{number}");
            let manifest = serde_json::from_value(manifest(&commit, &branch)).unwrap();
            let id = derive_id(REPO, 1, &commit, &branch);
            let now = unix_now();
            store
                .create_publication(
                    &token,
                    &NewPublication {
                        id: id.clone(),
                        manifest,
                    },
                    now,
                )
                .unwrap();
            for update in [
                Update::BranchCreated,
                Update::PullRequest {
                    number,
                    node_id: format!("PR_{number}"),
                    html_url: format!("https://example.invalid/{REPO}/pull/{number}"),
                },
            ] {
                store.update_publication(&id, &update, now).unwrap();
            }
            if let Some(merge_commit) = merge {
                store
                    .update_publication(&id, &Update::Merged { merge_commit }, now)
                    .unwrap();
            }
            id
        })
        .await
        .unwrap()
    }

    async fn pass(&self) {
        let connection = self
            .store
            .connections()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .id;
        sync::refresh(&self.state, &connection).await.unwrap();
    }

    /// The publication as the Hub API shows it.
    async fn show(&self, id: &str) -> Value {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = agentscm::hub::router(self.state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let response = reqwest::Client::new()
            .get(format!(
                "{url}/v1/hub/repos/acme%2Fwidgets/publications/{id}"
            ))
            .bearer_auth(&self.secret)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        serde_json::from_str(&response.text().await.unwrap()).unwrap()
    }

    fn runs(&self, sha: &str) -> Vec<ExternalRun> {
        self.store.external_runs(&self.repo, sha, 1000).unwrap()
    }

    fn marker(&self, sha: &str) -> Option<(i64, bool)> {
        self.store.external_reconciled(&self.repo, sha).unwrap()
    }

    fn observed_events(&self) -> usize {
        self.store
            .events_after(0, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "external_run_observed")
            .count()
    }

    fn requests(&self, containing: &str) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.path_and_query.contains(containing))
                .count()
        })
    }

    fn seed(&self, edit: impl FnOnce(&mut Repo)) {
        self.fake.with(|s| edit(s.repos.get_mut(&REPO_ID).unwrap()));
    }

    fn fail(&self, path: &str, status: u16) {
        self.fake.with(|s| {
            s.fail.push_back(Failure {
                path_contains: path.into(),
                status,
                headers: vec![],
            })
        });
    }
}

fn with(mut value: Value, key: &str, to: Value) -> Value {
    value[key] = to;
    value
}

#[tokio::test]
async fn unlisted_repositories_make_no_requests_and_store_nothing() {
    let env = Env::start(false).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "success")],
        ));
    });
    env.pass().await;
    assert_eq!(env.requests("/actions/runs"), 0);
    assert_eq!(env.requests("/deployments"), 0);
    assert!(env.runs(MERGED).is_empty());
    assert_eq!(env.marker(MERGED), None);
}

#[tokio::test]
async fn only_the_merged_commits_own_runs_and_deployments_are_recorded() {
    let env = Env::start(true).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        let ok = run_json(REPO_ID, 1, MERGED, "main", "completed", Some("success"));
        r.runs.push(ok.clone());
        r.runs.push(run_json(
            REPO_ID,
            2,
            MERGED,
            "feature",
            "completed",
            Some("success"),
        ));
        r.runs.push(with(
            with(ok, "event", json!("pull_request")),
            "id",
            json!(3),
        ));
        r.runs.push(run_json(
            REPO_ID,
            4,
            OTHER,
            "main",
            "completed",
            Some("success"),
        ));
        r.runs.push(run_json(
            999,
            5,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "success")],
        ));
        r.deployments.push((
            deployment_json(11, MERGED, "staging"),
            vec![status_json(2, "failure")],
        ));
        r.deployments.push((
            deployment_json(12, OTHER, "production"),
            vec![status_json(3, "success")],
        ));
    });
    env.pass().await;
    let rows: Vec<_> = env
        .runs(MERGED)
        .iter()
        .map(|r| (r.kind, r.run_id, r.environment.clone(), r.outcome))
        .collect();
    assert_eq!(
        rows,
        [
            (ExternalKind::Ci, 1, None, ExternalOutcome::Succeeded),
            (
                ExternalKind::Deployment,
                10,
                Some("production".into()),
                ExternalOutcome::Succeeded
            ),
            (
                ExternalKind::Deployment,
                11,
                Some("staging".into()),
                ExternalOutcome::Failed
            ),
        ]
    );
    assert!(env.runs(OTHER).is_empty());
    assert!(env.marker(MERGED).is_some_and(|(_, complete)| complete));
}

#[tokio::test]
async fn a_forge_reported_merge_is_recorded_then_its_runs_are_read() {
    let env = Env::start(true).await;
    let id = env.publish(1, OTHER, None).await;
    env.seed(|r| {
        let mut pull = Pull::new(1, "agents/work-1", &["a.txt"]);
        pull.state = "closed".into();
        pull.merged = true;
        pull.merge_commit_sha = Some(MERGED.into());
        r.pulls.insert(1, pull);
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
    });
    env.pass().await;
    let publication = env.store.publication(REPO, &id).unwrap().unwrap();
    assert_eq!(publication.merge_commit.as_deref(), Some(MERGED));
    assert_eq!(env.runs(MERGED).len(), 1);
}

#[tokio::test]
async fn a_repeat_pass_appends_no_events() {
    let env = Env::start(true).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "success")],
        ));
    });
    env.pass().await;
    let events = env.observed_events();
    assert_eq!(events, 2);
    env.pass().await;
    assert_eq!(env.observed_events(), events);
    assert_eq!(env.requests("/actions/runs"), 2, "read again, stored once");
}

#[tokio::test]
async fn run_state_advances_but_never_regresses_and_attempts_stay_separate() {
    let env = Env::start(true).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    let run = |status: &str, conclusion: Option<&str>, at: &str, attempt: i64| {
        let mut run = run_json(REPO_ID, 1, MERGED, "main", status, conclusion);
        run["updated_at"] = json!(at);
        run["run_attempt"] = json!(attempt);
        run
    };
    let set = |env: &Env, runs: Vec<Value>| env.seed(|r| r.runs = runs);
    let outcomes = |env: &Env| -> Vec<(i64, ExternalOutcome)> {
        env.runs(MERGED)
            .iter()
            .map(|r| (r.attempt, r.outcome))
            .collect()
    };
    set(&env, vec![run("in_progress", None, EARLY, 1)]);
    env.pass().await;
    assert_eq!(outcomes(&env), [(1, ExternalOutcome::Running)]);
    set(&env, vec![run("completed", Some("success"), LATE, 1)]);
    env.pass().await;
    assert_eq!(outcomes(&env), [(1, ExternalOutcome::Succeeded)]);
    // A stale in-progress read after completion does not regress it.
    set(&env, vec![run("in_progress", None, EARLY, 1)]);
    env.pass().await;
    assert_eq!(outcomes(&env), [(1, ExternalOutcome::Succeeded)]);
    // A rerun is a new attempt of the same run, not an overwrite.
    set(
        &env,
        vec![
            run("completed", Some("success"), LATE, 1),
            run("in_progress", None, LATE, 2),
        ],
    );
    env.pass().await;
    assert_eq!(
        outcomes(&env),
        [
            (1, ExternalOutcome::Succeeded),
            (2, ExternalOutcome::Running)
        ]
    );
}

#[tokio::test]
async fn auth_and_rate_limit_failures_leave_rows_and_markers_alone() {
    let env = Env::start(true).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "success")],
        ));
    });
    env.fail("/actions/runs", 401);
    env.pass().await;
    assert!(env.runs(MERGED).is_empty());
    assert_eq!(env.marker(MERGED), None);
    // The deployments read fails after the runs were read: nothing is stored.
    env.fail("/deployments?", 429);
    env.pass().await;
    assert!(env.runs(MERGED).is_empty());
    assert_eq!(env.marker(MERGED), None);
    // A missing permission skips the repository without failing the connection.
    env.fail("/actions/runs", 403);
    env.pass().await;
    assert!(env.runs(MERGED).is_empty());
    assert_eq!(env.marker(MERGED), None);
    env.pass().await;
    assert_eq!(env.runs(MERGED).len(), 2);
    assert!(env.marker(MERGED).is_some_and(|(_, complete)| complete));
}

#[tokio::test]
async fn a_list_cut_short_by_the_page_cap_is_marked_incomplete() {
    let env = Env::start(true).await;
    env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        for id in 1..=305 {
            r.runs.push(run_json(
                REPO_ID,
                id,
                MERGED,
                "main",
                "completed",
                Some("success"),
            ));
        }
    });
    env.pass().await;
    assert_eq!(env.runs(MERGED).len(), 300);
    assert!(env.marker(MERGED).is_some_and(|(_, complete)| !complete));
}

#[tokio::test]
async fn external_outcomes_change_no_check_or_publication_state() {
    let env = Env::start(true).await;
    let id = env.publish(1, OTHER, Some(MERGED)).await;
    let before = env.store.publication(REPO, &id).unwrap().unwrap();
    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("failure"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "failure")],
        ));
    });
    env.pass().await;
    assert_eq!(env.runs(MERGED).len(), 2);
    assert_eq!(env.store.publication(REPO, &id).unwrap().unwrap(), before);
    assert_eq!(
        env.store
            .forge_check(&env.repo, 1, "agentscm/workflow/main")
            .unwrap(),
        None
    );
    let writes: Vec<String> = env.fake.with(|s| {
        s.requests
            .iter()
            // The poll's own reads go through GraphQL.
            .filter(|r| r.method != "GET" && r.path_and_query != "/graphql")
            .map(|r| r.path_and_query.clone())
            .collect()
    });
    assert_eq!(writes, Vec::<String>::new());
}

#[tokio::test]
async fn the_publication_shows_nothing_external_unless_opted_in() {
    let env = Env::start(false).await;
    let id = env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
    });
    env.pass().await;
    assert!(env.show(&id).await.get("external").is_none());
}

#[tokio::test]
async fn the_publication_separates_ci_from_each_deployed_environment() {
    let env = Env::start(true).await;
    let id = env.publish(1, OTHER, Some(MERGED)).await;
    let unmerged = env.publish(2, "3".repeat(40).as_str(), None).await;
    // Nothing is inferred before a pass has read the provider.
    let before = env.show(&id).await["external"].clone();
    assert_eq!(before["state"], "unobserved");
    assert_eq!(before["reconciled_at"], Value::Null);
    assert_eq!(before["ci"], json!([]));
    assert_eq!(before["deployments"], json!([]));
    assert!(env.show(&unmerged).await.get("external").is_none());

    env.seed(|r| {
        r.runs.push(run_json(
            REPO_ID,
            1,
            MERGED,
            "main",
            "completed",
            Some("success"),
        ));
        r.deployments.push((
            deployment_json(10, MERGED, "production"),
            vec![status_json(1, "success")],
        ));
        r.deployments.push((
            deployment_json(11, MERGED, "staging"),
            vec![status_json(2, "failure")],
        ));
    });
    env.pass().await;
    let shown = env.show(&id).await;
    let external = &shown["external"];
    assert_eq!(external["provider"], "github");
    assert_eq!(external["merge_commit"], MERGED);
    assert_eq!(external["state"], "observed");
    assert!(external["reconciled_at"].is_i64());
    assert_eq!(external["ci"].as_array().unwrap().len(), 1);
    assert_eq!(external["ci"][0]["run_id"], 1);
    assert_eq!(external["ci"][0]["branch"], "main");
    assert_eq!(external["ci"][0]["outcome"], "succeeded");
    let deployed: Vec<_> = external["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["environment"].clone(), d["outcome"].clone()))
        .collect();
    assert_eq!(
        deployed,
        [
            (json!("production"), json!("succeeded")),
            (json!("staging"), json!("failed"))
        ]
    );
    // The forge check view is a separate field and is unaffected.
    assert!(shown.get("forge_check").is_none());
}

#[tokio::test]
async fn a_cut_short_read_or_list_is_shown_as_incomplete() {
    let env = Env::start(true).await;
    let id = env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        for id in 1..=305 {
            r.runs.push(run_json(
                REPO_ID,
                id,
                MERGED,
                "main",
                "completed",
                Some("success"),
            ));
        }
    });
    env.pass().await;
    let external = env.show(&id).await["external"].clone();
    assert_eq!(external["state"], "incomplete");
    assert_eq!(external["ci"].as_array().unwrap().len(), 200);

    // A complete read whose rows exceed what one reply shows is still incomplete.
    let env = Env::start(true).await;
    let id = env.publish(1, OTHER, Some(MERGED)).await;
    env.seed(|r| {
        for id in 1..=250 {
            r.runs.push(run_json(
                REPO_ID,
                id,
                MERGED,
                "main",
                "completed",
                Some("success"),
            ));
        }
    });
    env.pass().await;
    assert!(env.marker(MERGED).is_some_and(|(_, complete)| complete));
    let external = env.show(&id).await["external"].clone();
    assert_eq!(external["state"], "incomplete");
    assert_eq!(external["ci"].as_array().unwrap().len(), 200);
}

#[test]
fn malformed_repository_lists_are_refused() {
    for bad in [
        "acme",
        "acme/widgets/extra",
        "acme/widgets,",
        "a b/c",
        "../x",
    ] {
        let result = SyncConfig::from_lookup("http://127.0.0.1:1", "s".into(), 14, |name| {
            (name == "AGENTSCM_GITHUB_EXTERNAL_RUNS").then(|| bad.to_owned())
        });
        assert!(result.is_err(), "{bad}");
    }
}
