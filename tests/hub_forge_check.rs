//! The Hub-published forge commit status for a candidate pull request, end to end: a real
//! native store, an in-process hub and a fake GitHub. Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_github.rs"]
mod fake_github;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use agentscm::authority::Authority;
use agentscm::authority::workflows::WorkflowRequirement;
use agentscm::backend::LocalBackend;
use agentscm::candidate::CandidateSubject;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::ReviewContext;
use agentscm::hub::HubState;
use agentscm::hub::connection::NewForgeCheck;
use agentscm::hub::publish::{NewPublication, Update, derive_candidate_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::workspace::{Identity, Workspace, WorkspaceConfig};
use agentscm::hub::workspace_execution::{AgentTaskProfile, ExecutionConfig, ExecutionService};
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use agentscm::store::{Engine, digest};
use fake_github::{Branch, Failure, FakeGithub, Pull, Repo};
use serde_json::json;
use tempfile::TempDir;

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
                _ => None,
            })
            .unwrap();
        let host = fake.base.trim_start_matches("http://").to_owned();
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
        let service = ExecutionService::open(
            &dir.join("execution.sqlite"),
            ExecutionConfig {
                version: 1,
                source_profiles: vec![],
                agent_task_profiles: vec![profile.clone()],
                profiles: vec![],
            },
            workspace.clone(),
        )
        .unwrap();
        let state = HubState::new(
            store.clone(),
            Arc::new(LocalBackend::open(dir.join("objects")).unwrap()),
        )
        .with_sync(Sync::new(config).unwrap())
        .with_workspace_execution(Arc::new(service));
        sync::start(&state).await.unwrap();

        // Outbound writes stay off: statuses have their own opt-in.
        let (setup, setup_subject, setup_host) = (store.clone(), subject.clone(), host.clone());
        let setup_endpoint = endpoint;
        let (token, token_id, publication) = tokio::task::spawn_blocking(move || {
            let store = setup;
            let connection = store.pat_connection(&setup_host).unwrap().unwrap().id;
            store
                .upsert_repository(&connection, &setup_host, REPO_ID, REPO, None, None)
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
        }
    }

    /// Replaces the profile with `edit` applied, as an operator reconfiguring the Hub.
    fn reconfigure(&mut self, edit: impl FnOnce(&mut AgentTaskProfile)) {
        edit(&mut self.profile);
        let service = ExecutionService::open(
            &self.dir.join("execution-2.sqlite"),
            ExecutionConfig {
                version: 1,
                source_profiles: vec![],
                agent_task_profiles: vec![self.profile.clone()],
                profiles: vec![],
            },
            self.workspace.clone(),
        )
        .unwrap();
        self.state = self
            .state
            .clone()
            .with_workspace_execution(Arc::new(service));
    }

    /// A second publication, for pull request `number` on its own branch and head commit.
    fn add_pull(&self, number: u64, commit: &str, open: bool) {
        let branch = format!("agents/pull-{number}");
        let endpoint = format!(
            "{}/{REPO}.git",
            self.fake.base.replace("http://", "https://")
        );
        let mut subject = self.subject.clone();
        subject.output = branch.clone();
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

    /// `(sha, state, description)` of every status posted under the workflow context.
    fn statuses(&self) -> Vec<(String, String, String)> {
        self.fake.with(|s| {
            s.repos[&REPO_ID]
                .statuses
                .iter()
                .filter(|(_, v)| v["context"] == CONTEXT)
                .map(|(sha, v)| {
                    let text = |key: &str| v[key].as_str().unwrap().to_owned();
                    (sha.clone(), text("state"), text("description"))
                })
                .collect()
        })
    }

    fn status_posts(&self) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.method == "POST" && r.path_and_query.contains("/statuses/"))
                .count()
        })
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

    fn row(&self) -> Option<agentscm::hub::connection::ForgeCheck> {
        let repo = self
            .store
            .repository_named(self.fake.base.trim_start_matches("http://"), REPO)
            .unwrap()
            .unwrap();
        self.store.forge_check(&repo.id, 1, CONTEXT).unwrap()
    }
}

#[tokio::test]
async fn a_qualified_candidate_with_a_matching_head_gets_exactly_one_success() {
    let env = Env::start(true).await;
    assert_eq!(env.tree.len(), 40);
    env.qualify();
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        (posted[0].0.as_str(), posted[0].1.as_str()),
        (COMMIT, "success")
    );
    assert_eq!(env.row().unwrap().phase, "posted");
    // Re-reading an unchanged world posts nothing.
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
}

impl Env {
    /// The publication as the Hub API shows it.
    async fn show(&self) -> serde_json::Value {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = agentscm::hub::router(self.state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let path = format!(
            "{url}/v1/hub/repos/acme%2Fwidgets/publications/{}",
            self.publication
        );
        let response = reqwest::Client::new()
            .get(path)
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        serde_json::from_str(&response.text().await.unwrap()).unwrap()
    }
}

#[tokio::test]
async fn the_publication_reports_its_forge_check_read_only() {
    let env = Env::start(true).await;
    let before = env.show().await["forge_check"].clone();
    assert_eq!(
        (before["enabled"].clone(), before["state"].clone()),
        (json!(true), json!(null))
    );
    assert_eq!(before["context"], CONTEXT);
    env.qualify();
    env.poll().await;
    let shown = env.show().await["forge_check"].clone();
    assert_eq!(shown["mode"], "git_default");
    assert_eq!(shown["provider"], "github");
    assert_eq!(shown["state"], "success");
    assert_eq!(shown["head_sha"], COMMIT);
    assert_eq!(shown["posting"], "posted");
    assert_eq!(shown["protection"], "unverified");
    assert!(shown["note"].as_str().unwrap().contains("not enforced"));
    assert_eq!(env.status_posts(), 1, "showing posts nothing");

    let off = Env::start(false).await;
    let view = off.show().await["forge_check"].clone();
    assert_eq!(view["enabled"], false);
    assert!(view.get("context").is_none() && view["protection"] == "unverified");
}

#[tokio::test]
async fn the_profile_flag_off_makes_no_status_calls() {
    let env = Env::start(false).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.status_posts(), 0);
    assert!(env.row().is_none());
}

#[tokio::test]
async fn no_receipt_is_pending_and_never_success() {
    let env = Env::start(true).await;
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].1, "pending");
}

#[tokio::test]
async fn an_advanced_policy_is_a_failure_not_a_success() {
    let env = Env::start(true).await;
    env.qualify();
    Authority::open(&env.data)
        .unwrap()
        .advance_policy(env.subject.policy_epoch)
        .unwrap();
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].1, "failure");

    // A success that was posted earlier is replaced on the same head once the policy moves.
    let env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    Authority::open(&env.data)
        .unwrap()
        .advance_policy(env.subject.policy_epoch)
        .unwrap();
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (COMMIT, "failure")
    );
}

#[tokio::test]
async fn a_new_head_supersedes_the_old_success_and_is_not_itself_a_success() {
    let env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    let pushed = "2".repeat(40);
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().commits.insert(
            pushed.clone(),
            (env.tree.clone(), vec![PARENT.into(), BASELINE.into()]),
        );
    });
    env.edit_pull(|pull| pull.head_sha = "2".repeat(40));
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (COMMIT, "failure")
    );
    assert!(posted[1].2.starts_with("superseded"));
    assert_eq!(
        (posted[2].0.as_str(), posted[2].1.as_str()),
        (pushed.as_str(), "failure")
    );
}

#[tokio::test]
async fn an_advanced_target_a_fork_or_a_closed_pull_never_succeed() {
    let env = Env::start(true).await;
    env.qualify();
    env.fake.with(|s| {
        let branch = &mut s.repos.get_mut(&REPO_ID).unwrap().branches[0];
        branch.sha = "9".repeat(40);
    });
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].1, "failure");
    assert!(posted[0].2.contains("advanced"));

    let env = Env::start(true).await;
    env.qualify();
    env.edit_pull(|pull| pull.head_repo = Some(Some(999)));
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].1, "failure");
    assert!(posted[0].2.contains("connected repository"));

    let env = Env::start(true).await;
    env.qualify();
    env.edit_pull(|pull| pull.state = "closed".into());
    env.poll().await;
    assert_eq!(env.status_posts(), 0);
}

#[tokio::test]
async fn a_lost_status_reply_is_reposted_once_and_then_left_alone() {
    let env = Env::start(true).await;
    env.qualify();
    env.fake.with(|s| s.lose_next_status_reply = true);
    env.poll().await;
    assert_eq!(env.statuses().len(), 1);
    assert_eq!(env.row().unwrap().phase, "attempting");
    env.poll().await;
    assert_eq!(env.statuses().len(), 2);
    assert_eq!(env.row().unwrap().phase, "posted");
    env.poll().await;
    assert_eq!(env.statuses().len(), 2);
    assert!(env.statuses().iter().all(|s| s.1 == "success"));
}

#[tokio::test]
async fn a_rate_limited_pull_read_posts_nothing() {
    let env = Env::start(true).await;
    // The first poll caches the pull list, so the next one reads only the pull for the check.
    env.poll().await;
    assert_eq!(env.statuses().len(), 1);
    env.qualify();
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/pulls/1".into(),
            status: 429,
            headers: vec![("retry-after".into(), "600".into())],
        });
    });
    env.poll().await;
    assert!(
        env.fake.with(|s| s.fail.is_empty()),
        "the limit was not hit"
    );
    assert_eq!(env.status_posts(), 1, "{:?}", env.statuses());
    assert_eq!(env.statuses()[0].1, "pending");
}

#[tokio::test]
async fn a_lost_superseding_reply_is_reposted_before_the_new_head_is_judged() {
    let env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    let pushed = "2".repeat(40);
    env.fake.with(|s| {
        s.repos.get_mut(&REPO_ID).unwrap().commits.insert(
            pushed.clone(),
            (env.tree.clone(), vec![PARENT.into(), BASELINE.into()]),
        );
        s.lose_next_status_reply = true;
    });
    env.edit_pull(|pull| pull.head_sha = pushed.clone());
    env.poll().await;
    // The failure on the old head may or may not exist; the new head waits for it.
    assert_eq!(env.status_posts(), 2);
    assert_eq!(env.row().unwrap().phase, "attempting");
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(env.status_posts(), 4, "{posted:?}");
    assert_eq!(
        (posted[2].0.as_str(), posted[2].1.as_str()),
        (COMMIT, "failure")
    );
    assert_eq!(
        (posted[3].0.as_str(), posted[3].1.as_str()),
        (pushed.as_str(), "failure")
    );
    env.poll().await;
    assert_eq!(env.status_posts(), 4);
}

#[tokio::test]
async fn closing_a_pull_request_retires_its_success_once() {
    let env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    env.edit_pull(|pull| pull.state = "closed".into());
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (COMMIT, "failure")
    );
    assert!(posted[1].2.starts_with("not applicable"));
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

#[tokio::test]
async fn a_refused_status_is_recorded_and_not_posted_again() {
    let env = Env::start(true).await;
    env.qualify();
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/statuses/".into(),
            status: 422,
            headers: vec![],
        });
    });
    env.poll().await;
    assert!(
        env.fake.with(|s| s.fail.is_empty()),
        "the refusal was not hit"
    );
    let row = env.row().unwrap();
    assert_eq!(
        (row.phase.as_str(), row.state.as_str()),
        ("refused", "success")
    );
    assert!(env.statuses().is_empty());
    let posts = env.status_posts();
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), posts);
    assert_eq!(env.show().await["forge_check"]["posting"], "refused");
}

#[tokio::test]
async fn a_late_pull_request_is_reached_when_older_ones_fill_the_window() {
    let env = Env::start(true).await;
    env.qualify();
    for number in 2..=55 {
        env.add_pull(number, COMMIT, false);
    }
    let late = "3".repeat(40);
    env.add_pull(60, &late, true);
    env.poll().await;
    env.poll().await;
    let posted = env.statuses();
    assert!(
        posted.iter().any(|p| p.0 == late && p.1 == "success"),
        "{posted:?}"
    );
}

#[tokio::test]
async fn a_refused_downgrade_on_the_same_head_is_retried() {
    let env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    Authority::open(&env.data)
        .unwrap()
        .advance_policy(env.subject.policy_epoch)
        .unwrap();
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/statuses/".into(),
            status: 403,
            headers: vec![],
        });
    });
    env.poll().await;
    assert!(
        env.fake.with(|s| s.fail.is_empty()),
        "the refusal was not hit"
    );
    // GitHub still shows the success, so the journal must not call the row settled.
    assert_eq!(env.statuses()[0].1, "success");
    assert_eq!(env.row().unwrap().phase, "attempting");
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (COMMIT, "failure")
    );
    assert_eq!(env.row().unwrap().phase, "posted");
    env.poll().await;
    assert_eq!(env.status_posts(), 3);
}

#[tokio::test]
async fn a_refusal_on_a_row_that_never_showed_success_is_not_retried() {
    let env = Env::start(true).await;
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "pending");
    env.qualify();
    Authority::open(&env.data)
        .unwrap()
        .advance_policy(env.subject.policy_epoch)
        .unwrap();
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/statuses/".into(),
            status: 422,
            headers: vec![],
        });
    });
    env.poll().await;
    assert_eq!(env.row().unwrap().phase, "refused");
    let posts = env.status_posts();
    env.poll().await;
    assert_eq!(env.status_posts(), posts);
}

#[tokio::test]
async fn disabling_the_profile_retires_its_success_once() {
    let mut env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    env.reconfigure(|p| p.publish_forge_status = false);
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (COMMIT, "failure")
    );
    assert!(posted[1].2.contains("forge check disabled"));
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

impl Env {
    /// Journals a posted success on `context` at `COMMIT`, as an earlier pass would have.
    fn seed_success(&self, number: i64, context: &str) {
        let repo = self
            .store
            .repository_named(self.fake.base.trim_start_matches("http://"), REPO)
            .unwrap()
            .unwrap();
        let new = NewForgeCheck {
            head_sha: COMMIT.into(),
            state: "success".into(),
            binding_digest: Some("d".repeat(64)),
            description: "seeded".into(),
        };
        self.store
            .reserve_forge_check(&repo.id, number, context, None, &new, unix_now())
            .unwrap();
        self.store
            .complete_forge_check(&repo.id, number, context, 1, "posted", unix_now())
            .unwrap();
    }

    fn posts_to(&self, context: &str) -> usize {
        self.fake.with(|s| {
            s.repos[&REPO_ID]
                .statuses
                .iter()
                .filter(|(_, v)| v["context"] == context)
                .count()
        })
    }
}

#[tokio::test]
async fn enabled_context_successes_do_not_crowd_out_a_disabled_context_retire() {
    let env = Env::start(true).await;
    let old = "agentscm/workflow/old";
    // More enabled-context rows than one pass reads, all numbered ahead of the disabled one.
    for number in 2..=61 {
        env.seed_success(number, CONTEXT);
    }
    env.seed_success(70, old);
    env.poll().await;
    assert_eq!(env.posts_to(old), 1);
    assert_eq!(env.posts_to(CONTEXT), 1, "enabled rows are left alone");
    env.poll().await;
    assert_eq!(env.posts_to(old), 1);
}

#[tokio::test]
async fn a_merged_pull_request_keeps_whatever_its_head_shows() {
    let mut env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    // The forge says merged before the Hub recorded the merge: leave the head alone.
    env.edit_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
    });
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    // Once recorded as merged, disabling the profile does not post onto the merged head.
    env.store
        .update_publication(
            &env.publication,
            &Update::Merged {
                merge_commit: "e".repeat(40),
            },
            unix_now(),
        )
        .unwrap();
    env.reconfigure(|p| p.publish_forge_status = false);
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert_eq!(env.row().unwrap().state, "success");
}

#[tokio::test]
async fn a_base_that_advances_after_the_success_post_is_caught_in_the_same_pass() {
    let env = Env::start(true).await;
    env.qualify();
    env.fake
        .with(|s| s.advance_base_after_status = Some("9".repeat(40)));
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(posted[0].1, "success");
    assert_eq!(posted[1].1, "failure");
    assert!(posted[1].2.contains("advanced"), "{posted:?}");
}

#[tokio::test]
async fn a_pull_request_the_forge_reports_merged_is_not_retired_when_disabled() {
    let mut env = Env::start(true).await;
    env.qualify();
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    // Nothing recorded the merge in the Hub; only the forge knows.
    env.edit_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
    });
    env.reconfigure(|p| p.publish_forge_status = false);
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert_eq!(env.row().unwrap().state, "success");
}
