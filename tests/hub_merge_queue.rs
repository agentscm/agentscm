//! The Hub workflow status on GitHub merge queue groups, end to end: a real native store, an
//! in-process hub served over HTTP (signed `merge_group` deliveries to a GitHub App connection, the
//! only kind GitHub sends them to) and a fake GitHub.
//! Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_github.rs"]
mod fake_github;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscm::authority::Authority;
use agentscm::authority::workflows::WorkflowRequirement;
use agentscm::backend::LocalBackend;
use agentscm::candidate::CandidateSubject;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::ReviewContext;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::forge_check::merge_group::{
    Projection, QueueReadiness, group_projection, queue_readiness,
};
use agentscm::hub::publish::{NewPublication, Update, derive_candidate_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::workspace::{Identity, Workspace, WorkspaceConfig};
use agentscm::hub::workspace_execution::{AgentTaskProfile, ExecutionConfig, ExecutionService};
use agentscm::hub::{HubError, HubState};
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use agentscm::store::{Engine, digest};
use fake_github::{
    APP_ID, APP_SLUG, Branch, CLIENT_SECRET, Failure, FakeGithub, Installation, Pull, Repo,
    WEBHOOK_SECRET, private_pem,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 100;
const PAT: &str = "ghp_synthetic_pat";
const SKILL: &str = "skills/check/SKILL.md";
const SKILL_BYTES: &[u8] = b"# Check synthetic source\n";
const SOURCE_BYTES: &[u8] = b"fn example() {}\n";
const CONTEXT: &str = "agentscm/workflow/main";
const BASELINE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PARENT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const COMMIT: &str = "1111111111111111111111111111111111111111";
const BRANCH: &str = "agents/work";
const GROUP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_TREE: &str = "dddddddddddddddddddddddddddddddddddddddd";
const INSTALLATION: i64 = 7;
/// How long a wait for a positive condition lasts before the test fails.
const DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);

/// How the connection reaches GitHub. Only an App is sent `merge_group` deliveries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Auth {
    App,
    Pat,
}

#[allow(dead_code)] // fields the shared setup keeps for the scenarios that need them
struct Env {
    fake: FakeGithub,
    state: HubState,
    store: Arc<dyn Store>,
    _root: TempDir,
    data: PathBuf,
    context: ReviewContext,
    subject: CandidateSubject,
    tree: String,
    token: String,
    token_id: String,
    publication: String,
    workspace: Arc<Workspace>,
    profile: AgentTaskProfile,
    dir: PathBuf,
    url: String,
    server: JoinHandle<()>,
    auth: Auth,
    connection: String,
    repo_row: String,
}

fn sync_config(base: &str, secrets: PathBuf, auth: Auth) -> SyncConfig {
    let base = base.to_owned();
    SyncConfig::from_lookup("http://127.0.0.1:1", secrets, 14, move |name| match name {
        "AGENTSCM_GITHUB_URL" | "AGENTSCM_GITHUB_API_URL" => Some(base.clone()),
        "AGENTSCM_GITHUB_PAT" if auth == Auth::Pat => Some(PAT.into()),
        "AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET" if auth == Auth::Pat => Some(WEBHOOK_SECRET.into()),
        _ => None,
    })
    .unwrap()
}

async fn serve(state: &HubState) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = agentscm::hub::router(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, server)
}

/// Waits for a positive condition; fails the test loudly when it does not come.
async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(POLL).await;
    }
}

/// A Hub over the store and the native data dir the profile names, as an operator starts it.
/// A previous Hub on the same execution configuration must be dropped: its lease is awaited.
async fn open_hub(
    store: &Arc<dyn Store>,
    dir: &Path,
    workspace: &Arc<Workspace>,
    profile: &AgentTaskProfile,
    base: &str,
    auth: Auth,
) -> (HubState, String, JoinHandle<()>) {
    let deadline = Instant::now() + DEADLINE;
    let service = loop {
        let config = ExecutionConfig {
            attached_publication_profiles: vec![],
            version: 1,
            source_profiles: vec![],
            agent_task_profiles: vec![profile.clone()],
            profiles: vec![],
        };
        match ExecutionService::open(&dir.join("execution.sqlite"), config, workspace.clone()) {
            Ok(service) => break service,
            Err(HubError::Busy) if Instant::now() < deadline => tokio::time::sleep(POLL).await,
            Err(error) => panic!("execution service: {error}"),
        }
    };
    let state = HubState::new(
        store.clone(),
        Arc::new(LocalBackend::open(dir.join("objects")).unwrap()),
    )
    .with_sync(Sync::new(sync_config(base, dir.join("secrets"), auth)).unwrap())
    .with_workspace_execution(Arc::new(service));
    sync::start(&state).await.unwrap();
    let (url, server) = serve(&state).await;
    (state, url, server)
}

/// An App connection with its credentials file, as the manifest flow leaves them.
fn app_connection(store: &dyn Store, secrets: &Path, host: &str) -> String {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let id = store::new_connection_id().unwrap();
    let file = json!({"app_id": APP_ID, "slug": APP_SLUG, "client_id": "Iv1.synthetic",
        "client_secret": CLIENT_SECRET, "webhook_secret": WEBHOOK_SECRET, "pem": private_pem()});
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(secrets.join(format!("{id}.json")))
        .unwrap()
        .write_all(file.to_string().as_bytes())
        .unwrap();
    let new = NewConnection {
        id: id.clone(),
        provider: "github".into(),
        host: host.to_owned(),
        kind: "app".into(),
        app_id: Some(APP_ID),
        app_slug: Some(APP_SLUG.into()),
        acts_as: None,
    };
    store.insert_connection(&new, "test").unwrap();
    id
}

/// The tree Git builds for the native revision's two files.
fn git_tree(dir: &Path) -> String {
    let git = |args: &[&str]| {
        let out = Command::new(std::env::var("AGENTSCM_TEST_GIT").unwrap_or_else(|_| "git".into()))
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--object-format=sha1", "-q"]);
    for (path, bytes) in [(SKILL, SKILL_BYTES), ("src/example.rs", SOURCE_BYTES)] {
        std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(path), bytes).unwrap();
    }
    git(&["add", "."]);
    git(&["write-tree"])
}

impl Env {
    async fn start(publish: bool) -> Self {
        Self::start_as(publish, Auth::App).await
    }

    async fn start_as(publish: bool, auth: Auth) -> Self {
        let root = TempDir::new().unwrap();
        let data = root.path().join("native");
        std::fs::create_dir(&data).unwrap();
        let engine = Engine::open(&data).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: SKILL.into(),
                        content: Some(SKILL_BYTES.to_vec()),
                    },
                    FileChange {
                        path: "src/example.rs".into(),
                        content: Some(SOURCE_BYTES.to_vec()),
                    },
                ],
            )
            .unwrap();
        let mut authority = Authority::open(&data).unwrap();
        let repository = authority.repository_id().unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            name: "synthetic-forge-check".into(),
            source: WorkflowSource {
                repository: repository.clone(),
                revision: revision.clone(),
                policy_epoch: 0,
            },
            skill: SkillReference {
                entrypoint: SKILL.into(),
                resources: vec![],
            },
            steps: vec![StepDefinition {
                id: "source".into(),
                needs: vec![],
                required: true,
                action: StepAction::SourcePaths {
                    expectations: vec![SourcePathExpectation {
                        path: "src/example.rs".into(),
                        sha256: digest(SOURCE_BYTES),
                    }],
                },
            }],
        };
        let plan = skill_workflow::compile(&data, &definition).unwrap();
        let required = WorkflowRequirement::from_plan("main", &plan).unwrap();
        let requirement_digest = required.digest().unwrap();
        authority.install_workflow_requirement(&required).unwrap();
        let policy_epoch = authority.policy_epoch().unwrap();
        let context: ReviewContext = serde_json::from_value(json!({
            "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
            "revision":revision,"base":null,"target":"main","policy_epoch":policy_epoch,
            "intent":"Verify synthetic source","acceptance_criteria":["Source matches"],
            "scope":["src/example.rs"],"exclusions":[],"omissions":[],"dependencies":[],
            "recipe":{"state":"digest","sha256":"a".repeat(64)},
            "toolchain":{"state":"digest","sha256":"b".repeat(64)},
            "environment":{"state":"digest","sha256":"c".repeat(64)},
            "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
        }))
        .unwrap();
        authority.start_change(&context).unwrap();
        drop((authority, engine));
        let tree_dir = root.path().join("tree");
        std::fs::create_dir(&tree_dir).unwrap();
        let tree = git_tree(&tree_dir);

        let fake = FakeGithub::start().await;
        let endpoint = format!("{}/{REPO}.git", fake.base.replace("http://", "https://"));
        let mut subject = CandidateSubject {
            id: String::new(),
            attachment: "1".repeat(64),
            endpoint: endpoint.clone(),
            output: BRANCH.into(),
            change: context.change.clone(),
            change_version: 1,
            context_digest: context.digest().unwrap(),
            policy_epoch,
            revision: revision.clone(),
            target: "main".into(),
            baseline_commit: BASELINE.into(),
            parent_commit: PARENT.into(),
            source_commit: "d".repeat(40),
            source_fingerprint: "4".repeat(64),
            comparison_fingerprint: "5".repeat(64),
        };
        subject.id = subject.canonical_id().unwrap();
        fake.with(|s| {
            let mut repo = Repo::new(REPO_ID, REPO);
            let mut pull = Pull::new(1, BRANCH, &["src/example.rs"]);
            pull.head_sha = COMMIT.into();
            repo.pulls.insert(1, pull);
            repo.commits.insert(
                COMMIT.into(),
                (tree.clone(), vec![PARENT.into(), BASELINE.into()]),
            );
            repo.branches.push(Branch {
                name: "main".into(),
                sha: BASELINE.into(),
                committed_at: "2026-01-01T00:00:00Z".into(),
            });
            s.repos.insert(REPO_ID, repo);
            match auth {
                Auth::App => s.installations.push(Installation {
                    id: INSTALLATION,
                    account: "acme".into(),
                    repos: vec![REPO_ID],
                }),
                Auth::Pat => {
                    s.pats
                        .insert(PAT.into(), ("robot".into(), 77, [REPO_ID].into()));
                }
            }
        });

        let dir = root.path().join("hub");
        std::fs::create_dir(&dir).unwrap();
        let store = store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display())).unwrap();
        let secrets = dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let host = fake.base.trim_start_matches("http://").to_owned();
        let app = (auth == Auth::App).then(|| app_connection(&*store, &secrets, &host));
        let workspace = Arc::new(
            Workspace::open(
                WorkspaceConfig {
                    workspace_id: "synthetic-workspace".into(),
                    name: "Synthetic workspace".into(),
                    database: dir.join("workspace.sqlite"),
                    initial_owner: Identity {
                        provider: "github".into(),
                        host: host.clone(),
                        user_id: 101,
                    },
                    forge_repositories: vec![],
                },
                store.clone(),
            )
            .unwrap(),
        );
        let profile = AgentTaskProfile {
            repository_id: REPO.into(),
            profile_id: "checks".into(),
            native_repository: repository.clone(),
            data_dir: data.clone(),
            target: "main".into(),
            requirement_digest,
            publish_forge_status: publish,
        };
        let (state, url, server) =
            open_hub(&store, &dir, &workspace, &profile, &fake.base, auth).await;
        let connection = app.unwrap_or_else(|| store.pat_connection(&host).unwrap().unwrap().id);
        let installation = (auth == Auth::App).then_some(INSTALLATION);

        // Outbound writes stay off: statuses have their own opt-in.
        let (setup, setup_subject, setup_host) = (store.clone(), subject.clone(), host.clone());
        let (setup_endpoint, setup_connection) = (endpoint, connection.clone());
        let (token, token_id, publication) = tokio::task::spawn_blocking(move || {
            let store = setup;
            store
                .upsert_repository(
                    &setup_connection,
                    &setup_host,
                    REPO_ID,
                    REPO,
                    None,
                    installation,
                )
                .unwrap();
            let owner = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: setup_host,
                        user_id: 5,
                    },
                    "alice",
                )
                .unwrap();
            let (token, secret) = mint_agent_token(&*store, &owner.id, "agent-a", REPO, 3600).unwrap();
            let id = derive_candidate_id(REPO, &setup_subject.id, COMMIT, BRANCH);
            let manifest = serde_json::from_value(json!({
                "revision": setup_subject.revision, "commit": COMMIT, "tree": "c".repeat(40),
                "parent": PARENT, "branch": BRANCH, "base": "main", "title": "t", "body": "",
                "message": "msg\n",
                "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
                "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
                "changes": [{"path": "old", "mode": "000000", "blob": null, "sha256": null, "size": 0}],
                "candidate": {"subject": setup_subject, "endpoint": setup_endpoint,
                    "previous_publication": null}
            }))
            .unwrap();
            store
                .create_publication(&token, &NewPublication { id: id.clone(), manifest }, unix_now())
                .unwrap();
            store
                .update_publication(&id, &Update::BranchCreated, unix_now())
                .unwrap();
            store
                .update_publication(
                    &id,
                    &Update::PullRequest {
                        number: 1,
                        node_id: "PR_1".into(),
                        html_url: format!("https://example.invalid/{REPO}/pull/1"),
                    },
                    unix_now(),
                )
                .unwrap();
            (secret.expose().to_owned(), token, id)
        })
        .await
        .unwrap();
        let repo_row = store.repository_named(&host, REPO).unwrap().unwrap().id;
        Self {
            fake,
            state,
            store,
            _root: root,
            data,
            context,
            subject,
            tree,
            token,
            token_id,
            publication,
            workspace,
            profile,
            dir,
            url,
            server,
            auth,
            connection,
            repo_row,
        }
    }

    /// Restarts the Hub over the same store and native data dir: the running one is dropped, then
    /// a new one is built from the (possibly reconfigured) profile, as an operator would.
    async fn rebuild(&mut self) {
        self.settled().await;
        self.server.abort();
        let backend = Arc::new(LocalBackend::open(self.dir.join("objects")).unwrap());
        drop(std::mem::replace(
            &mut self.state,
            HubState::new(self.store.clone(), backend),
        ));
        (self.state, self.url, self.server) = open_hub(
            &self.store,
            &self.dir,
            &self.workspace,
            &self.profile,
            &self.fake.base,
            self.auth,
        )
        .await;
    }

    /// A second publication, for pull request `number` on its own branch and head commit.
    fn add_pull(&self, number: u64, commit: &str, open: bool) {
        let endpoint = format!(
            "{}/{REPO}.git",
            self.fake.base.replace("http://", "https://")
        );
        self.add_pull_at(number, commit, open, endpoint);
    }

    /// As `add_pull`, for a candidate published to `endpoint`.
    fn add_pull_at(&self, number: u64, commit: &str, open: bool, endpoint: String) {
        let branch = format!("agents/pull-{number}");
        let mut subject = self.subject.clone();
        subject.output = branch.clone();
        subject.endpoint = endpoint.clone();
        subject.id = String::new();
        subject.id = subject.canonical_id().unwrap();
        self.fake.with(|s| {
            let repo = s.repos.get_mut(&REPO_ID).unwrap();
            let mut pull = Pull::new(number, &branch, &["src/example.rs"]);
            pull.head_sha = commit.into();
            if !open {
                pull.state = "closed".into();
            }
            repo.pulls.insert(number, pull);
            repo.commits.insert(
                commit.into(),
                (self.tree.clone(), vec![PARENT.into(), BASELINE.into()]),
            );
        });
        let id = derive_candidate_id(REPO, &subject.id, commit, &branch);
        let manifest = serde_json::from_value(json!({
            "revision": subject.revision, "commit": commit, "tree": "c".repeat(40),
            "parent": PARENT, "branch": branch, "base": "main", "title": "t", "body": "",
            "message": "msg\n",
            "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
            "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
            "changes": [{"path": "old", "mode": "000000", "blob": null, "sha256": null, "size": 0}],
            "candidate": {"subject": subject, "endpoint": endpoint, "previous_publication": null}
        }))
        .unwrap();
        let new = NewPublication {
            id: id.clone(),
            manifest,
        };
        self.store
            .create_publication(&self.token_id, &new, unix_now())
            .unwrap();
        for update in [
            Update::BranchCreated,
            Update::PullRequest {
                number: number as i64,
                node_id: format!("PR_{number}"),
                html_url: format!("https://example.invalid/{REPO}/pull/{number}"),
            },
        ] {
            self.store
                .update_publication(&id, &update, unix_now())
                .unwrap();
        }
    }

    async fn poll(&self) {
        sync::poll_once(&self.state).await.unwrap();
    }

    fn qualify(&self) {
        let requirement = Authority::open_read_only(&self.data)
            .unwrap()
            .workflow_requirement("main")
            .unwrap()
            .unwrap();
        let plan =
            skill_workflow::compile(&self.data, &requirement.instantiate(&self.context).unwrap())
                .unwrap();
        skill_workflow::start(&self.data, "run-1", &plan).unwrap();
        skill_workflow::advance(&self.data, "run-1", 8).unwrap();
        skill_workflow::runtime::finalize(&self.data, "run-1", &self.context).unwrap();
    }

    fn edit_pull(&self, edit: impl FnOnce(&mut Pull)) {
        self.fake.with(|s| {
            edit(
                s.repos
                    .get_mut(&REPO_ID)
                    .unwrap()
                    .pulls
                    .get_mut(&1)
                    .unwrap(),
            )
        });
    }
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

/// A `merge_group` payload as GitHub sends it, for the queue branch of `pr`.
fn mg(action: &str, pr: u64, pr_head: &str, group: &str, base: &str) -> Value {
    json!({"action": action, "reason": "dequeued",
        "merge_group": {"head_sha": group,
            "head_ref": format!("refs/heads/gh-readonly-queue/main/pr-{pr}-{pr_head}"),
            "base_sha": base, "base_ref": "refs/heads/main"},
        "repository": {"id": REPO_ID, "full_name": REPO}})
}

fn requested(pr: u64, pr_head: &str, group: &str, base: &str) -> Value {
    mg("checks_requested", pr, pr_head, group, base)
}

impl Env {
    /// The group commit on GitHub: its queue branch, tree and parents (the base, the pull head).
    fn enqueue(&self, pr: u64, pr_head: &str, group: &str, base: &str, tree: &str) {
        let name = format!("gh-readonly-queue/main/pr-{pr}-{pr_head}");
        self.fake.with(|s| {
            let repo = s.repos.get_mut(&REPO_ID).unwrap();
            repo.commits.insert(
                group.into(),
                (tree.into(), vec![base.into(), pr_head.into()]),
            );
            repo.branches.retain(|b| b.name != name);
            repo.branches.push(Branch {
                name,
                sha: group.into(),
                committed_at: "2026-01-02T00:00:00Z".into(),
            });
        });
    }

    /// GitHub deleting the queue branches, as it does when a group ends.
    fn dequeue_all(&self) {
        self.fake.with(|s| {
            s.repos
                .get_mut(&REPO_ID)
                .unwrap()
                .branches
                .retain(|b| !b.name.starts_with("gh-readonly-queue/"))
        });
    }

    async fn hook(&self, delivery: &str, body: &Value) -> u16 {
        self.hook_signed(delivery, body, WEBHOOK_SECRET).await
    }

    async fn hook_signed(&self, delivery: &str, body: &Value, secret: &str) -> u16 {
        fake_github::deliver(
            &self.url,
            &self.connection,
            "merge_group",
            delivery,
            body,
            secret,
        )
        .await
    }

    /// Delivers `body`, expects it accepted, and waits for the job the Hub started for it.
    async fn accept(&self, delivery: &str, body: &Value) {
        assert_eq!(self.hook(delivery, body).await, 202);
        self.settled().await;
    }

    /// Waits until no webhook job is queued or running. A delivery is counted before its `202`
    /// is sent, so after an accepted delivery this means that delivery's work is done.
    async fn settled(&self) {
        eventually("the webhook jobs to finish", || {
            self.state.webhook_jobs_pending() == 0
        })
        .await;
    }

    async fn projection(&self, group: &str) -> Option<Projection> {
        group_projection(&self.state, &self.repo_row, group)
            .await
            .unwrap()
    }

    /// The group's projection, asserted to be journalled as posted.
    async fn posted(&self, group: &str) -> Projection {
        let shown = self.projection(group).await.expect("the group is recorded");
        assert_eq!(shown.posting, "posted", "{shown:?}");
        shown
    }

    /// What the last poll observed about the queue of `main`.
    fn readiness(&self) -> QueueReadiness {
        queue_readiness(&self.repo_row, "main").expect("a poll observed the queue")
    }

    /// `(state, description)` of every status under the workflow context on `sha`, oldest first.
    fn on(&self, sha: &str) -> Vec<(String, String)> {
        self.fake.with(|s| {
            s.repos[&REPO_ID]
                .statuses
                .iter()
                .filter(|(at, v)| at == sha && v["context"] == CONTEXT)
                .map(|(_, v)| {
                    (
                        v["state"].as_str().unwrap().to_owned(),
                        v["description"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        })
    }

    /// Status POSTs the Hub sent for `sha`, whether or not GitHub kept them.
    fn posts_on(&self, sha: &str) -> usize {
        let path = format!("/repos/{REPO}/statuses/{sha}");
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.method == "POST" && r.path_and_query == path)
                .count()
        })
    }

    /// Reads of pull request 1 the Hub made.
    fn pull_reads(&self) -> usize {
        let path = format!("/repos/{REPO}/pulls/1");
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.method == "GET" && r.path_and_query.starts_with(&path))
                .count()
        })
    }

    /// Whether the connection is waiting out a rate limit.
    fn backing_off(&self) -> bool {
        let connection = self.store.connection(&self.connection).unwrap().unwrap();
        connection.next_poll_at > unix_now()
    }

    fn advance_policy(&self) {
        Authority::open(&self.data)
            .unwrap()
            .advance_policy(self.subject.policy_epoch)
            .unwrap();
    }

    /// The publication as the Hub API shows it to its agent.
    async fn show(&self) -> Value {
        let response = reqwest::Client::new()
            .get(format!(
                "{}/v1/hub/repos/acme%2Fwidgets/publications/{}",
                self.url, self.publication
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        serde_json::from_str(&response.text().await.unwrap()).unwrap()
    }

    /// A qualified candidate with its group queued on the candidate baseline, delivered once.
    async fn queued(publish: bool) -> Self {
        let env = Env::start(publish).await;
        env.qualify();
        env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
        env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
            .await;
        env
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_on_the_candidate_baseline_with_the_qualified_tree_gets_exactly_one_success() {
    let env = Env::queued(true).await;
    let shown = env.posted(GROUP).await;
    assert_eq!(shown.kind, "merge_group");
    assert_eq!(shown.state.as_deref(), Some("success"));
    assert_eq!(shown.context, CONTEXT);
    assert_eq!((shown.pr_number, shown.pr_head_sha.as_str()), (1, COMMIT));
    assert_eq!(shown.base_sha, BASELINE);
    assert!(shown.stacked_on.is_empty() && shown.binding_digest.is_some());
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.on(GROUP)[0].0, "success");
    // The delivery judged the group alone: nothing was posted on the pull request head.
    assert!(env.on(COMMIT).is_empty());
    // Re-reading an unchanged world posts nothing more on the group.
    env.poll().await;
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.posts_on(GROUP), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_passing_head_with_a_different_group_tree_gets_a_failure_never_a_success() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, OTHER_TREE);
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    let shown = env.posted(GROUP).await;
    assert_eq!(shown.state.as_deref(), Some("failure"));
    assert!(shown.description.unwrap().contains("tree differs"));
    env.poll().await;
    env.poll().await;
    // The head itself qualifies and passes; the group does not inherit that.
    assert_eq!(env.on(COMMIT)[0].0, "success");
    let posted = env.on(GROUP);
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].0, "failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_built_after_the_target_advanced_gets_a_failure_never_a_success() {
    let env = Env::start(true).await;
    env.qualify();
    let advanced = sha('9');
    env.enqueue(1, COMMIT, GROUP, &advanced, &env.tree);
    env.accept("d-1", &requested(1, COMMIT, GROUP, &advanced))
        .await;
    let shown = env.posted(GROUP).await;
    assert_eq!(shown.state.as_deref(), Some("failure"));
    assert!(shown.description.unwrap().contains("target advanced"));
    env.poll().await;
    assert!(env.on(GROUP).iter().all(|s| s.0 == "failure"));
    assert_eq!(env.on(GROUP).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_with_the_right_tree_but_not_built_on_the_baseline_gets_a_failure() {
    for parents in [
        vec![sha('9'), COMMIT.to_string()],
        vec![BASELINE.to_string(), sha('9')],
        vec![],
    ] {
        let env = Env::start(true).await;
        env.qualify();
        env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
        env.fake.with(|s| {
            let repo = s.repos.get_mut(&REPO_ID).unwrap();
            repo.commits.get_mut(GROUP).unwrap().1 = parents.clone();
        });
        env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
            .await;
        let shown = env.posted(GROUP).await;
        assert_eq!(shown.state.as_deref(), Some("failure"), "{parents:?}");
        assert!(shown.description.unwrap().contains("not built on"));
        assert!(env.on(GROUP).iter().all(|s| s.0 == "failure"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_whose_single_parent_is_not_the_baseline_stays_pending_as_unsupported() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.fake.with(|s| {
        let repo = s.repos.get_mut(&REPO_ID).unwrap();
        repo.commits.get_mut(GROUP).unwrap().1 = vec![sha('9')];
    });
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    let shown = env.posted(GROUP).await;
    assert_eq!(shown.state.as_deref(), Some("pending"));
    assert!(shown.description.unwrap().contains("unsupported"));
    env.poll().await;
    assert!(env.on(GROUP).iter().all(|s| s.0 == "pending"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_stacked_on_another_queued_group_is_pending_as_unsupported() {
    let env = Env::start(true).await;
    env.qualify();
    let (head2, group2) = (sha('2'), sha('f'));
    env.add_pull(2, &head2, true);
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.enqueue(2, &head2, &group2, GROUP, &env.tree);
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    env.posted(GROUP).await;
    env.accept("d-2", &requested(2, &head2, &group2, GROUP))
        .await;
    let second = env.posted(&group2).await;
    assert_eq!(second.state.as_deref(), Some("pending"));
    assert!(second.description.unwrap().contains("unsupported"));
    assert_eq!(second.stacked_on, [1]);
    assert_eq!(second.pr_number, 2);
    let first = env.projection(GROUP).await.unwrap();
    assert_eq!((first.pr_number, first.stacked_on.is_empty()), (1, true));
    assert_eq!(first.state.as_deref(), Some("success"));
    env.poll().await;
    assert_eq!(env.on(&group2).len(), 1);
    assert!(env.on(&group2).iter().all(|s| s.0 == "pending"));
    assert_eq!(env.on(GROUP).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_group_ends_the_old_row_and_the_new_one_is_judged_separately() {
    let env = Env::queued(true).await;
    env.posted(GROUP).await;
    // GitHub rebuilds the entry: the same queue branch now points at a different commit.
    let rebuilt = sha('f');
    env.enqueue(1, COMMIT, &rebuilt, BASELINE, OTHER_TREE);
    env.poll().await;
    let old = env.projection(GROUP).await.unwrap();
    assert_eq!(old.lifecycle, "ended");
    assert_eq!(old.ended_reason.as_deref(), Some("merge group was rebuilt"));
    // The branch moved on, so this group is known not to have landed.
    assert_eq!(old.outcome, "not_landed");
    assert_eq!(env.on(GROUP).len(), 1, "no further post on the old group");
    env.accept("d-2", &requested(1, COMMIT, &rebuilt, BASELINE))
        .await;
    let new = env.posted(&rebuilt).await;
    assert_eq!(new.state.as_deref(), Some("failure"));
    // A late request for the old commit does not revive it.
    env.accept("d-3", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    env.poll().await;
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.on(&rebuilt).len(), 1);
    assert_eq!(env.projection(GROUP).await.unwrap().lifecycle, "ended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_destroyed_before_it_was_requested_is_never_posted_or_revived() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.accept("d-1", &mg("destroyed", 1, COMMIT, GROUP, BASELINE))
        .await;
    let ended = env.projection(GROUP).await.unwrap();
    assert_eq!(ended.lifecycle, "ended");
    assert_eq!(ended.ended_reason.as_deref(), Some("dequeued"));
    assert_eq!((ended.posting, ended.outcome), ("pending", "not_landed"));
    env.accept("d-2", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    env.poll().await;
    assert!(env.on(GROUP).is_empty());
    assert_eq!(env.posts_on(GROUP), 0);
    let still = env.projection(GROUP).await.unwrap();
    assert_eq!(still.lifecycle, "ended");
    assert_eq!(still.posting, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_delivery_and_a_resent_event_post_once() {
    let env = Env::queued(true).await;
    env.posted(GROUP).await;
    // The same delivery id again is a recorded duplicate.
    assert_eq!(
        env.hook("d-1", &requested(1, COMMIT, GROUP, BASELINE))
            .await,
        200
    );
    // The identical event under a new delivery id is accepted and finds nothing to change.
    env.accept("d-2", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.posts_on(GROUP), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_signature_or_an_unknown_or_foreign_repository_changes_nothing() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    let body = requested(1, COMMIT, GROUP, BASELINE);
    let rejected = env.hook_signed("d-1", &body, "wrong-secret").await;
    assert!(!(200..300).contains(&rejected), "{rejected}");
    // A rejected delivery does not use up its id.
    let mut unknown = body.clone();
    unknown["repository"]["id"] = json!(999);
    env.accept("d-2", &unknown).await;
    // A repository that belongs to another connection of the same host.
    let host = env.fake.base.trim_start_matches("http://").to_owned();
    let other = NewConnection {
        id: format!("con_{}", "f".repeat(32)),
        provider: "github".into(),
        host: host.clone(),
        kind: "app".into(),
        app_id: Some(9),
        app_slug: Some("other-app".into()),
        acts_as: None,
    };
    env.store.insert_connection(&other, "test").unwrap();
    let foreign = env
        .store
        .upsert_repository(&other.id, &host, 555, REPO, None, None)
        .unwrap();
    let mut foreign_body = body.clone();
    foreign_body["repository"]["id"] = json!(555);
    env.accept("d-3", &foreign_body).await;
    assert!(env.projection(GROUP).await.is_none());
    assert!(
        group_projection(&env.state, &foreign.id, GROUP)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(env.posts_on(GROUP), 0);
    assert!(env.on(GROUP).is_empty());
    // The right signature still works afterwards.
    env.accept("d-1", &body).await;
    assert_eq!(env.posted(GROUP).await.state.as_deref(), Some("success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_status_reply_on_a_group_is_uncertain_then_reposted_once_and_settles() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.fake.with(|s| s.lose_next_status_reply = true);
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.projection(GROUP).await.unwrap().posting, "uncertain");
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 2);
    assert_eq!(env.projection(GROUP).await.unwrap().posting, "posted");
    env.poll().await;
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 2);
    assert!(env.on(GROUP).iter().all(|s| s.0 == "success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_status_post_records_nothing_and_recovers_after_the_backoff() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/statuses/".into(),
            status: 429,
            headers: vec![("retry-after".into(), "600".into())],
        });
    });
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    // The limit was hit and the connection waits it out.
    assert!(env.fake.with(|s| s.fail.is_empty()));
    assert!(env.backing_off());
    assert!(env.on(GROUP).is_empty());
    assert_ne!(env.projection(GROUP).await.unwrap().posting, "posted");
    // The connection is backing off, so a poll is a no-op.
    let before = env.fake.with(|s| s.requests.len());
    env.poll().await;
    assert_eq!(env.fake.with(|s| s.requests.len()), before);
    assert!(env.on(GROUP).is_empty());
    env.store.set_next_poll(&env.connection, 0).unwrap();
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.posted(GROUP).await.state.as_deref(), Some("success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_pull_read_posts_nothing_on_the_group_and_recovers() {
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/pulls/1".into(),
            status: 429,
            headers: vec![("retry-after".into(), "600".into())],
        });
    });
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    assert!(env.fake.with(|s| s.fail.is_empty()));
    assert!(env.backing_off());
    assert!(env.on(GROUP).is_empty());
    let shown = env.projection(GROUP).await.unwrap();
    assert_eq!((shown.posting, shown.state), ("pending", None));
    env.store.set_next_poll(&env.connection, 0).unwrap();
    env.poll().await;
    assert_eq!(env.posted(GROUP).await.state.as_deref(), Some("success"));
    assert_eq!(env.on(GROUP).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_reposts_an_attempting_group_and_leaves_an_ended_one_alone() {
    let mut env = Env::start(true).await;
    env.qualify();
    let (head2, group2) = (sha('2'), sha('f'));
    env.add_pull(2, &head2, true);
    env.enqueue(2, &head2, &group2, BASELINE, &env.tree);
    env.accept("d-1", &requested(2, &head2, &group2, BASELINE))
        .await;
    env.posted(&group2).await;
    env.accept("d-2", &mg("destroyed", 2, &head2, &group2, BASELINE))
        .await;
    assert_eq!(env.projection(&group2).await.unwrap().lifecycle, "ended");

    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.fake.with(|s| s.lose_next_status_reply = true);
    env.accept("d-3", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.projection(GROUP).await.unwrap().posting, "uncertain");

    // The same store and native data dir under a new Hub.
    env.rebuild().await;
    // A recorded delivery is still a duplicate afterwards.
    assert_eq!(
        env.hook("d-3", &requested(1, COMMIT, GROUP, BASELINE))
            .await,
        200
    );
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 2);
    assert_eq!(env.projection(GROUP).await.unwrap().posting, "posted");
    assert_eq!(env.on(&group2).len(), 1, "an ended group is not reposted");
    env.poll().await;
    assert_eq!((env.on(GROUP).len(), env.on(&group2).len()), (2, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advanced_policy_makes_a_group_failure_not_success() {
    let env = Env::start(true).await;
    env.qualify();
    env.advance_policy();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    env.posted(GROUP).await;
    let posted = env.on(GROUP);
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].0, "failure");

    // A success posted earlier is replaced once the policy moves.
    let env = Env::queued(true).await;
    assert_eq!(env.posted(GROUP).await.state.as_deref(), Some("success"));
    env.advance_policy();
    env.poll().await;
    let posted = env.on(GROUP);
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(posted[1].0, "failure");
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_the_profile_flag_off_retires_a_group_success_once() {
    let mut env = Env::queued(true).await;
    env.posted(GROUP).await;
    env.profile.publish_forge_status = false;
    env.rebuild().await;
    env.poll().await;
    let posted = env.on(GROUP);
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(posted[1].0, "failure");
    assert!(posted[1].1.contains("not applicable: forge check disabled"));
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_source_and_ruleset_requirement_are_observed_not_fought() {
    let env = Env::queued(true).await;
    let observed = env.projection(GROUP).await.unwrap();
    assert_eq!(observed.status_source, "hub");
    assert_eq!(observed.ruleset_requirement, "not_required");
    let required = |integration: Option<i64>| {
        let mut check = json!({"context": CONTEXT});
        if let Some(id) = integration {
            check["integration_id"] = json!(id);
        }
        Some(json!([{"type": "required_status_checks",
            "parameters": {"required_status_checks": [check]}}]))
    };
    for (rules, expected) in [
        (required(None), "required_any_source"),
        (required(Some(99)), "required_from_another_source"),
        // GitHub itself then ignores the context from any other account.
        (required(Some(APP_ID)), "required_from_this_app"),
    ] {
        env.fake
            .with(|s| s.repos.get_mut(&REPO_ID).unwrap().rules = rules);
        env.poll().await;
        assert_eq!(
            env.projection(GROUP).await.unwrap().ruleset_requirement,
            expected
        );
    }
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/rules/branches/".into(),
            status: 403,
            headers: vec![],
        });
    });
    env.poll().await;
    assert_eq!(
        env.projection(GROUP).await.unwrap().ruleset_requirement,
        "unknown"
    );

    // Another account posts the same context after the Hub: reported, not overwritten.
    let posts = env.posts_on(GROUP);
    env.fake
        .with(|s| s.push_status(REPO_ID, GROUP, "success", CONTEXT, "intruder"));
    env.poll().await;
    env.poll().await;
    assert_eq!(
        env.projection(GROUP).await.unwrap().status_source,
        "another_account"
    );
    assert_eq!(env.posts_on(GROUP), posts);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merged_pull_requests_group_is_left_alone() {
    // Merged before the group was delivered: nothing is posted.
    let env = Env::start(true).await;
    env.qualify();
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.edit_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
    });
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    // The delivery's job read the merged pull request and left the group alone.
    assert!(env.pull_reads() >= 1);
    assert_eq!(env.posts_on(GROUP), 0);
    env.poll().await;
    assert_eq!(env.posts_on(GROUP), 0);

    // Merged after a success: the group keeps whatever it shows.
    let env = Env::queued(true).await;
    env.posted(GROUP).await;
    env.edit_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
    });
    env.poll().await;
    env.poll().await;
    assert_eq!(env.on(GROUP).len(), 1);
    assert_eq!(env.posts_on(GROUP), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_personal_token_connection_reports_that_github_never_sends_it_merge_groups() {
    let env = Env::start_as(true, Auth::Pat).await;
    env.poll().await;
    let readiness = env.readiness();
    assert_eq!(readiness.delivery, "personal_token_unsupported");
    assert_eq!(readiness.undelivered, Some(vec![]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_readiness_follows_the_installations_permission_and_event_subscription() {
    let env = Env::start(true).await;
    // Nothing is known before a poll.
    assert!(queue_readiness(&env.repo_row, "main").is_none());
    env.poll().await;
    let readiness = env.readiness();
    assert_eq!(readiness.target, "main");
    assert_eq!(readiness.delivery, "subscribed");
    assert_eq!(readiness.undelivered, Some(vec![]));
    assert!(readiness.checked_at > 0);
    let install = |permissions: Value, events: &[&str]| {
        env.fake.with(|s| {
            s.installation_permissions = Some(permissions);
            s.installation_events = Some(events.iter().map(|e| (*e).to_owned()).collect());
        })
    };
    // Without merge queue read access GitHub never sends the event.
    install(
        json!({"statuses": "write", "pull_requests": "read"}),
        &["merge_group"],
    );
    env.poll().await;
    assert_eq!(env.readiness().delivery, "permission_missing");
    // The permission without the subscription is no delivery either.
    install(json!({"merge_queues": "read"}), &["pull_request"]);
    env.poll().await;
    assert_eq!(env.readiness().delivery, "event_not_subscribed");
    install(json!({"merge_queues": "write"}), &["merge_group"]);
    env.poll().await;
    assert_eq!(env.readiness().delivery, "subscribed");
    // An installation GitHub will not describe is unknown, not assumed fine.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/app/installations/7".into(),
            status: 500,
            headers: vec![],
        });
    });
    env.poll().await;
    assert_eq!(env.readiness().delivery, "unknown");
    env.poll().await;
    assert_eq!(env.readiness().delivery, "subscribed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_branches_without_a_delivery_are_reported_until_one_arrives() {
    let env = Env::start(true).await;
    env.qualify();
    // GitHub built the group; the Hub was told nothing.
    env.enqueue(1, COMMIT, GROUP, BASELINE, &env.tree);
    env.poll().await;
    assert_eq!(env.readiness().undelivered, Some(vec![1]));
    env.accept("d-1", &requested(1, COMMIT, GROUP, BASELINE))
        .await;
    env.poll().await;
    assert_eq!(env.readiness().undelivered, Some(vec![]));

    // A listing GitHub refuses is unknown, not empty.
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/git/matching-refs/".into(),
            status: 403,
            headers: vec![],
        });
    });
    env.poll().await;
    assert_eq!(env.readiness().undelivered, None);
    env.poll().await;
    assert_eq!(env.readiness().undelivered, Some(vec![]));

    // More queue branches than one page holds: the rest cannot be ruled out.
    env.fake.with(|s| {
        s.repos
            .get_mut(&REPO_ID)
            .unwrap()
            .branches
            .extend((2..=101).map(|n| Branch {
                name: format!("gh-readonly-queue/main/pr-{n}-{n:040x}"),
                sha: format!("{n:040x}"),
                committed_at: "2026-01-03T00:00:00Z".into(),
            }));
    });
    env.poll().await;
    assert_eq!(env.readiness().undelivered, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_group_landed_only_when_its_pull_request_merged_as_that_commit() {
    for (merged_as, reason, outcome) in [
        (Some(GROUP.to_owned()), "landed", "landed"),
        (
            Some(sha('9')),
            "pull request merged as another commit",
            "unknown",
        ),
        (None, "merge group branch is gone", "unknown"),
    ] {
        let env = Env::queued(true).await;
        env.posted(GROUP).await;
        env.dequeue_all();
        if let Some(commit) = &merged_as {
            env.edit_pull(|pull| {
                pull.state = "closed".into();
                pull.merged = true;
                pull.merge_commit_sha = Some(commit.clone());
            });
        }
        env.poll().await;
        let ended = env.projection(GROUP).await.unwrap();
        assert_eq!(ended.lifecycle, "ended", "{merged_as:?}");
        assert_eq!(ended.ended_reason.as_deref(), Some(reason), "{merged_as:?}");
        assert_eq!(ended.outcome, outcome, "{merged_as:?}");
        assert_eq!(env.on(GROUP).len(), 1, "an ended group is not posted again");
    }

    // GitHub's own signed delivery says merged; the Hub did not see the commit itself.
    let env = Env::start(true).await;
    let mut merged = mg("destroyed", 1, COMMIT, GROUP, BASELINE);
    merged["reason"] = json!("merged");
    env.accept("d-1", &merged).await;
    let reported = env.projection(GROUP).await.unwrap();
    assert_eq!(reported.ended_reason.as_deref(), Some("merged"));
    assert_eq!(reported.outcome, "merged_reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_publication_shows_the_queue_readiness_and_the_groups_of_its_pull_request() {
    let env = Env::queued(true).await;
    // Groups are journalled by the delivery; readiness is only known after a poll.
    let before = env.show().await["forge_check"]["merge_queue"].clone();
    assert_eq!(before["readiness"], json!(null));
    assert_eq!(before["groups"][0]["group_sha"], GROUP);
    env.poll().await;
    let shown = env.show().await;
    let queue = &shown["forge_check"]["merge_queue"];
    assert_eq!(queue["readiness"]["target"], "main");
    assert_eq!(queue["readiness"]["delivery"], "subscribed");
    assert_eq!(queue["readiness"]["undelivered"], json!([]));
    assert_eq!(queue["groups"].as_array().unwrap().len(), 1);
    let group = &queue["groups"][0];
    assert_eq!(group["group_sha"], GROUP);
    assert_eq!(group["pr_number"], 1);
    assert_eq!(group["context"], CONTEXT);
    assert_eq!(group["state"], "success");
    assert_eq!(group["posting"], "posted");
    assert_eq!(group["lifecycle"], "current");
    assert_eq!(group["outcome"], "pending");
    assert_eq!(group["status_source"], "hub");

    // After it lands, the same view shows how the group ended.
    env.dequeue_all();
    env.edit_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
        pull.merge_commit_sha = Some(GROUP.into());
    });
    env.poll().await;
    let landed = env.show().await["forge_check"]["merge_queue"]["groups"][0].clone();
    assert_eq!(landed["lifecycle"], "ended");
    assert_eq!(landed["ended_reason"], "landed");
    assert_eq!(landed["outcome"], "landed");
}
