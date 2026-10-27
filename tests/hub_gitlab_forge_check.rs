//! The Hub-published forge commit status for a candidate GitLab merge request, end to end: a real
//! native store, an in-process hub, and a fake GitLab whose project is backed by a real bare Git
//! repository. Synthetic data only.
#![cfg(unix)]
#[path = "support/fake_gitlab.rs"]
mod fake_gitlab;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentscm::authority::Authority;
use agentscm::authority::workflows::WorkflowRequirement;
use agentscm::backend::LocalBackend;
use agentscm::candidate::CandidateSubject;
use agentscm::checks::SourcePathExpectation;
use agentscm::context::ReviewContext;
use agentscm::hub::connection::{ForgeCheck, NewConnection};
use agentscm::hub::publish::{NewPublication, Update, derive_candidate_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::sync::{self, Sync, SyncConfig};
use agentscm::hub::workspace::{Identity, Workspace, WorkspaceConfig};
use agentscm::hub::workspace_execution::{AgentTaskProfile, ExecutionConfig, ExecutionService};
use agentscm::hub::{HubState, router};
use agentscm::model::FileChange;
use agentscm::skill_workflow::{
    self, SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use agentscm::store::{Engine, digest};
use fake_gitlab::{Failure, FakeCommit, FakeGitlab, Mr, Project, Token, TreeEntry, WEBHOOK_TOKEN};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "group/sub/widgets";
const REPO_ID: i64 = 100;
const TOKEN: &str = "synthetic-gitlab-token";
const SKILL: &str = "skills/check/SKILL.md";
const SKILL_BYTES: &[u8] = b"# Check synthetic source\n";
const SOURCE_BYTES: &[u8] = b"fn example() {}\n";
const CONTEXT: &str = "agentscm/workflow/main";
const BRANCH: &str = "agents/feat";
const PENDING: &str = "awaiting native qualification";
const GIT: &str = "/usr/bin/git";

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new(GIT)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Synthetic")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "Git {args:?}: {:?}", output.stderr);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Env {
    fake: FakeGitlab,
    state: HubState,
    url: String,
    store: Arc<dyn Store>,
    _root: TempDir,
    data: PathBuf,
    context: ReviewContext,
    subject: CandidateSubject,
    bare: PathBuf,
    /// The target tip, the published candidate commit on top of it, and that commit's tree.
    seed: String,
    commit: String,
    tree: String,
    host: String,
    connection: String,
    token: String,
    token_id: String,
    publication: String,
    workspace: Arc<Workspace>,
    profile: AgentTaskProfile,
    dir: PathBuf,
}

impl Env {
    async fn start(publish: bool) -> Self {
        let root = TempDir::new().unwrap();
        let data = root.path().join("native");
        std::fs::create_dir(&data).unwrap();
        let engine = Engine::open(&data).unwrap();
        let change = |path: &str, bytes: &[u8]| FileChange {
            path: path.into(),
            content: Some(bytes.to_vec()),
        };
        let revision = engine
            .checkpoint(
                None,
                &[
                    change(SKILL, SKILL_BYTES),
                    change("src/example.rs", SOURCE_BYTES),
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

        // The forge's repository: `main`, and a candidate commit whose tree is exactly the
        // native revision's two files.
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "--initial-branch=main"]);
        std::fs::write(work.join("old"), b"old bytes").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "base"]);
        let seed = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["rm", "-q", "old"]);
        for (path, bytes) in [(SKILL, SKILL_BYTES), ("src/example.rs", SOURCE_BYTES)] {
            std::fs::create_dir_all(work.join(path).parent().unwrap()).unwrap();
            std::fs::write(work.join(path), bytes).unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "candidate"]);
        let commit = git(&work, &["rev-parse", "HEAD"]);
        let tree = git(&work, &["rev-parse", "HEAD^{tree}"]);
        let bare = root.path().join("forge.git");
        std::fs::create_dir(&bare).unwrap();
        git(&bare, &["init", "--bare", "--initial-branch=main"]);
        git(
            &work,
            &[
                "push",
                bare.to_str().unwrap(),
                &format!("{seed}:refs/heads/main"),
                &format!("{commit}:refs/heads/{BRANCH}"),
            ],
        );

        let fake = FakeGitlab::start().await;
        fake.with(|s| {
            let mut project = Project::new(REPO_ID, REPO, "private");
            project.git = Some(bare.clone());
            let mut mr = Mr::new(1, BRANCH, &["src/example.rs"]);
            mr.sha = commit.clone();
            project.mrs.insert(1, mr);
            s.projects.insert(REPO_ID, project);
            s.tokens.insert(
                TOKEN.into(),
                Token {
                    username: "robot".into(),
                    user_id: 77,
                    access: [(REPO_ID, 30)].into(),
                },
            );
        });
        let host = fake
            .base
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let endpoint = format!("https://{host}/{REPO}.git");
        let mut subject = CandidateSubject {
            id: String::new(),
            attachment: "1".repeat(64),
            endpoint,
            output: BRANCH.into(),
            change: context.change.clone(),
            change_version: 1,
            context_digest: context.digest().unwrap(),
            policy_epoch,
            revision: revision.clone(),
            target: "main".into(),
            baseline_commit: seed.clone(),
            parent_commit: seed.clone(),
            source_commit: "d".repeat(40),
            source_fingerprint: "4".repeat(64),
            comparison_fingerprint: "5".repeat(64),
        };
        subject.id = subject.canonical_id().unwrap();

        let dir = root.path().join("hub");
        std::fs::create_dir(&dir).unwrap();
        let store = store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display())).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let secrets = dir.join("secrets");
        sync::prepare_secrets_dir(&secrets, true).unwrap();
        let base = fake.base.clone();
        let config = SyncConfig::from_lookup(&url, secrets, 14, |name| match name {
            "AGENTSCM_GITLAB_URL" => Some(base.clone()),
            "AGENTSCM_GITLAB_TOKEN" => Some(TOKEN.into()),
            "AGENTSCM_GITLAB_WEBHOOK_SECRET" => Some(WEBHOOK_TOKEN.into()),
            _ => None,
        })
        .unwrap();
        let workspace = Arc::new(
            Workspace::open(
                WorkspaceConfig {
                    workspace_id: "synthetic-workspace".into(),
                    name: "Synthetic workspace".into(),
                    database: dir.join("workspace.sqlite"),
                    initial_owner: Identity {
                        provider: "gitlab".into(),
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
            native_repository: repository,
            data_dir: data.clone(),
            target: "main".into(),
            requirement_digest,
            publish_forge_status: publish,
        };
        let service = ExecutionService::open(
            &dir.join("execution.sqlite"),
            ExecutionConfig {
                attached_publication_profiles: vec![],
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
        let app = router(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });

        // Outbound writes stay off: statuses have their own opt-in.
        let connection = store.pat_connection(&host).unwrap().unwrap().id;
        store
            .upsert_repository(&connection, &host, REPO_ID, REPO, None, None)
            .unwrap();
        let owner = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "gitlab".into(),
                    host: host.clone(),
                    user_id: 5,
                },
                "alice",
            )
            .unwrap();
        let (token_id, secret) =
            mint_agent_token(&*store, &owner.id, "agent-a", REPO, 3600).unwrap();
        let mut env = Self {
            fake,
            state,
            url,
            store,
            _root: root,
            data,
            context,
            subject,
            bare,
            seed,
            commit,
            tree,
            host,
            connection,
            token: secret.expose().to_owned(),
            token_id,
            publication: String::new(),
            workspace,
            profile,
            dir,
        };
        env.publication = env.publish(1, &env.commit.clone(), BRANCH, &env.endpoint());
        env
    }

    fn endpoint(&self) -> String {
        format!("https://{}/{REPO}.git", self.host)
    }

    /// A candidate publication for merge request `number`, as the publish steps leave it.
    fn publish(&self, number: i64, commit: &str, branch: &str, endpoint: &str) -> String {
        let mut subject = self.subject.clone();
        subject.output = branch.into();
        subject.endpoint = endpoint.into();
        subject.id = String::new();
        subject.id = subject.canonical_id().unwrap();
        let id = derive_candidate_id(REPO, &subject.id, commit, branch);
        let manifest = serde_json::from_value(json!({
            "revision": subject.revision, "commit": commit, "tree": "c".repeat(40),
            "parent": self.seed, "branch": branch, "base": "main", "title": "t", "body": "",
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
                number,
                node_id: format!("gid://gitlab/MergeRequest/{number}"),
                html_url: format!("https://example.invalid/{REPO}/-/merge_requests/{number}"),
            },
        ] {
            self.store
                .update_publication(&id, &update, unix_now())
                .unwrap();
        }
        id
    }

    /// Replaces the profile with `edit` applied, as an operator reconfiguring the Hub.
    fn reconfigure(&mut self, edit: impl FnOnce(&mut AgentTaskProfile)) {
        edit(&mut self.profile);
        let service = ExecutionService::open(
            &self.dir.join("execution-2.sqlite"),
            ExecutionConfig {
                attached_publication_profiles: vec![],
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

    async fn poll(&self) {
        sync::refresh(&self.state, &self.connection).await.unwrap();
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

    fn advance_policy(&self) {
        Authority::open(&self.data)
            .unwrap()
            .advance_policy(self.subject.policy_epoch)
            .unwrap();
    }

    /// `(sha, GitLab state, description)` of every status posted under the workflow context.
    fn statuses(&self) -> Vec<(String, String, String)> {
        self.fake.with(|s| {
            s.projects[&REPO_ID]
                .statuses
                .iter()
                .filter(|(_, v)| v["name"] == CONTEXT)
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

    fn requests_to(&self, path: &str) -> usize {
        self.fake.with(|s| {
            s.requests
                .iter()
                .filter(|r| r.path_and_query.ends_with(path))
                .count()
        })
    }

    fn edit_mr(&self, edit: impl FnOnce(&mut Mr)) {
        self.fake.with(|s| {
            edit(
                s.projects
                    .get_mut(&REPO_ID)
                    .unwrap()
                    .mrs
                    .get_mut(&1)
                    .unwrap(),
            )
        });
    }

    fn row(&self) -> Option<ForgeCheck> {
        let repo = self
            .store
            .repository_named(&self.host, REPO)
            .unwrap()
            .unwrap();
        self.store.forge_check(&repo.id, 1, CONTEXT).unwrap()
    }

    fn phase(&self) -> String {
        self.row().unwrap().phase
    }

    /// A new commit with the candidate tree on `parents`, which the source branch is moved to
    /// (forced); the merge request reports it as its head only when `follow`.
    fn push(&self, parents: &[&str], follow: bool) -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        let message = format!("push {}", N.fetch_add(1, Ordering::Relaxed));
        let mut args = vec!["commit-tree", &self.tree, "-m", &message];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        let sha = git(&self.bare, &args);
        git(
            &self.bare,
            &["update-ref", &format!("refs/heads/{BRANCH}"), &sha],
        );
        if follow {
            self.edit_mr(|mr| mr.sha = sha.clone());
        }
        sha
    }

    /// A new target tip on top of the seed.
    fn advance_main(&self) -> String {
        let tip = git(
            &self.bare,
            &["commit-tree", &self.tree, "-p", &self.seed, "-m", "next"],
        );
        git(&self.bare, &["update-ref", "refs/heads/main", &tip]);
        tip
    }

    /// A status already on GitLab for the published commit, as a lost reply would leave it.
    fn seed_status(&self, state: &str, description: &str) {
        self.fake.with(|s| {
            let project = s.projects.get_mut(&REPO_ID).unwrap();
            let id = project.statuses.len() + 1;
            project.statuses.push((
                self.commit.clone(),
                json!({"state": state, "name": CONTEXT, "description": description,
                    "ref": BRANCH, "id": id}),
            ));
        });
    }

    /// The publication as the Hub API shows it.
    async fn show(&self) -> Value {
        let path = format!(
            "{}/v1/hub/repos/group%2Fsub%2Fwidgets/publications/{}",
            self.url, self.publication
        );
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(path)
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        serde_json::from_str(&response.text().await.unwrap()).unwrap()
    }

    async fn hook(&self, event: &str, uuid: &str, body: &Value, token: &str) -> u16 {
        fake_gitlab::deliver(&self.url, &self.connection, event, uuid, body, token).await
    }

    fn mr_hook(&self, project: i64) -> Value {
        json!({"object_kind": "merge_request", "project": {"id": project},
            "object_attributes": {"iid": 1}})
    }
}

/// A qualified candidate whose merge request is at the published head.
async fn ready() -> Env {
    let env = Env::start(true).await;
    env.qualify();
    env
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

const SETTLE: Duration = Duration::from_millis(400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_qualified_candidate_with_a_matching_head_gets_exactly_one_success() {
    let env = ready().await;
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        (posted[0].0.as_str(), posted[0].1.as_str()),
        (env.commit.as_str(), "success")
    );
    // The status is keyed by the merge request's source branch in the connected project.
    let reference = env
        .fake
        .with(|s| s.projects[&REPO_ID].statuses[0].1["ref"].clone());
    assert_eq!(reference, BRANCH);
    assert_eq!(env.phase(), "posted");
    // Re-reading an unchanged world posts nothing.
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_publication_reports_a_gitlab_forge_check_read_only() {
    let env = Env::start(true).await;
    let before = env.show().await["forge_check"].clone();
    assert_eq!(
        (before["enabled"].clone(), before["provider"].clone()),
        (json!(true), json!("gitlab"))
    );
    assert_eq!(
        (before["state"].clone(), before["context"].clone()),
        (json!(null), json!(CONTEXT))
    );
    env.qualify();
    env.poll().await;
    let shown = env.show().await["forge_check"].clone();
    assert_eq!(shown["provider"], "gitlab");
    assert_eq!(shown["state"], "success");
    assert_eq!(shown["head_sha"], env.commit);
    assert_eq!(shown["posting"], "posted");
    assert_eq!(shown["protection"], "unverified");
    assert_eq!(env.status_posts(), 1, "showing posts nothing");

    let off = Env::start(false).await;
    let view = off.show().await["forge_check"].clone();
    assert_eq!(
        (view["enabled"].clone(), view["provider"].clone()),
        (json!(false), json!("gitlab"))
    );
    off.qualify();
    off.poll().await;
    assert_eq!(off.status_posts(), 0);
    assert!(off.row().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_receipt_is_pending_and_a_changed_policy_is_a_failure() {
    let env = Env::start(true).await;
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        (posted[0].1.as_str(), posted[0].2.as_str()),
        ("pending", PENDING)
    );

    // A success posted earlier is replaced on the same head once the policy moves.
    let env = ready().await;
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    env.advance_policy();
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (env.commit.as_str(), "failed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_push_supersedes_the_old_success_and_is_not_itself_a_success() {
    let env = ready().await;
    env.poll().await;
    let pushed = env.push(&[&env.seed], true);
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (env.commit.as_str(), "failed")
    );
    assert!(posted[1].2.starts_with("superseded"));
    assert_eq!(
        (posted[2].0.as_str(), posted[2].1.as_str()),
        (pushed.as_str(), "failed")
    );
    env.poll().await;
    assert_eq!(env.status_posts(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_force_push_never_keeps_the_old_success_or_earns_a_new_one() {
    let env = ready().await;
    env.poll().await;
    // History rewritten: the candidate tree again, but without the candidate's parent.
    let pushed = env.push(&[], true);
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert!(posted[1].2.starts_with("superseded"));
    assert_eq!(
        (posted[2].0.as_str(), posted[2].1.as_str()),
        (pushed.as_str(), "failed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_request_head_lagging_its_branch_gets_no_success() {
    let env = ready().await;
    // The source branch moved on; the merge request still reports the old head.
    env.push(&[&env.seed], false);
    env.poll().await;
    assert_eq!(env.status_posts(), 0);
    assert!(env.row().is_none());

    // Once the merge request catches up the new head is judged: never a success.
    let env = ready().await;
    env.poll().await;
    let pushed = env.push(&[&env.seed], false);
    env.poll().await;
    assert_eq!(env.status_posts(), 1, "a lagging read must claim nothing");
    env.edit_mr(|mr| mr.sha = pushed.clone());
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert_eq!(
        (posted[2].0.as_str(), posted[2].1.as_str()),
        (pushed.as_str(), "failed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advanced_target_is_a_failure_including_in_the_confirming_read() {
    let env = ready().await;
    env.advance_main();
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].1, "failed");
    assert!(posted[0].2.contains("advanced"));

    // The target moves after the success was posted: the confirming read catches it.
    let env = ready().await;
    let tip = git(
        &env.bare,
        &["commit-tree", &env.tree, "-p", &env.seed, "-m", "next"],
    );
    env.fake.with(|s| s.advance_main_after_status = Some(tip));
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[0].1.as_str(), posted[1].1.as_str()),
        ("success", "failed")
    );
    assert!(posted[1].2.contains("advanced"), "{posted:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_merge_request_is_ignored_entirely() {
    let env = ready().await;
    env.edit_mr(|mr| mr.source_project_id = Some(999));
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), 0);
    assert!(env.row().is_none());
    assert!(env.statuses().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_merge_request_retires_its_success_once_and_a_locked_one_is_left_alone() {
    let env = ready().await;
    env.poll().await;
    env.edit_mr(|mr| mr.state = "locked".into());
    env.poll().await;
    assert_eq!(
        env.status_posts(),
        1,
        "a locked merge request has no determined state"
    );
    env.edit_mr(|mr| mr.state = "closed".into());
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (env.commit.as_str(), "failed")
    );
    assert!(posted[1].2.starts_with("not applicable"));
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_forward_merge_is_recorded_from_the_published_head() {
    let env = ready().await;
    env.poll().await;
    env.edit_mr(|mr| {
        mr.state = "merged".into();
        mr.merge_commit_sha = None;
    });
    env.poll().await;
    let merged = env
        .store
        .publication(REPO, &env.publication)
        .unwrap()
        .unwrap();
    assert_eq!(merged.merge_commit.as_deref(), Some(env.commit.as_str()));
    assert_eq!(
        env.status_posts(),
        1,
        "a merged head keeps whatever it shows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_status_reply_is_adopted_from_the_list_without_a_second_post() {
    let env = ready().await;
    env.fake.with(|s| s.lose_next_status_reply = true);
    env.poll().await;
    assert_eq!(env.statuses().len(), 1);
    assert_eq!(env.phase(), "attempting");
    env.poll().await;
    assert_eq!(env.phase(), "posted");
    env.poll().await;
    assert_eq!((env.status_posts(), env.statuses().len()), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_status_post_is_not_recorded_as_posted_and_is_retried() {
    let env = ready().await;
    env.fake.with(|s| s.limit_next_status = true);
    env.poll().await;
    assert!(env.statuses().is_empty());
    assert_eq!(
        env.phase(),
        "attempting",
        "a limited post is not recorded as posted"
    );
    env.poll().await;
    assert_eq!((env.statuses().len(), env.status_posts()), (1, 2));
    assert_eq!(env.phase(), "posted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cannot_transition_reply_is_resolved_by_reading_the_list() {
    // The desired pending status is already there: adopted, not posted again.
    let env = Env::start(true).await;
    env.seed_status("pending", PENDING);
    env.poll().await;
    assert_eq!(env.phase(), "posted");
    assert_eq!((env.statuses().len(), env.status_posts()), (1, 1));

    // A finished success with another binding is never reused by GitLab: the desired success is
    // a new status, accepted and recorded.
    let env = ready().await;
    env.seed_status(
        "success",
        "Qualified in the pinned native store; binding 000000000000",
    );
    env.poll().await;
    assert_eq!(env.phase(), "posted");
    assert_eq!((env.statuses().len(), env.status_posts()), (2, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_status_can_return_to_success_on_the_same_head() {
    let env = ready().await;
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    // The target moves on, so the same head is no longer a success.
    let tip = env.advance_main();
    env.poll().await;
    assert_eq!(env.statuses().last().unwrap().1, "failed");
    // The target returns to where the head was qualified: a new success replaces the failure.
    git(&env.bare, &["update-ref", "refs/heads/main", &env.seed]);
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 3, "{posted:?}");
    assert_eq!(posted[2].1, "success");
    assert_eq!(env.phase(), "posted");
    assert_ne!(tip, env.seed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_status_is_recorded_and_not_posted_again() {
    let env = ready().await;
    env.fake.with(|s| s.refuse_next_status = Some(403));
    env.poll().await;
    let row = env.row().unwrap();
    assert_eq!(
        (row.phase.as_str(), row.state.as_str()),
        ("refused", "success")
    );
    assert!(env.statuses().is_empty());
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert_eq!(env.show().await["forge_check"]["posting"], "refused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_400_on_a_retiring_failure_is_final_and_on_a_replacing_failure_is_retried() {
    // Retiring a closed merge request: the 400 is a definite refusal and settles.
    let env = ready().await;
    env.poll().await;
    env.edit_mr(|mr| mr.state = "closed".into());
    env.fake.with(|s| s.refuse_next_status = Some(400));
    env.poll().await;
    assert_eq!(env.phase(), "refused");
    env.poll().await;
    assert_eq!(env.status_posts(), 2);

    // Replacing a shown success with a failure: the success is still there, so the row stays
    // `attempting` and the next pass posts again.
    let env = ready().await;
    env.poll().await;
    env.advance_policy();
    env.fake.with(|s| s.refuse_next_status = Some(400));
    env.poll().await;
    assert_eq!(env.phase(), "attempting");
    env.poll().await;
    assert_eq!(env.statuses().last().unwrap().1, "failed");
    assert_eq!(env.status_posts(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_refused_over_a_running_status_stays_uncertain() {
    let env = Env::start(true).await;
    env.seed_status("running", "another system");
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert_eq!(env.statuses().len(), 1, "the pending status was refused");
    assert_eq!(env.phase(), "attempting");
    assert_eq!(env.show().await["forge_check"]["posting"], "uncertain");
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_read_stops_the_pass_without_recording_anything() {
    let env = ready().await;
    env.fake.with(|s| {
        s.fail.push_back(Failure {
            path_contains: "/repository/commits/".into(),
            status: 429,
            headers: vec![("retry-after".into(), "600".into())],
        });
    });
    env.poll().await;
    assert!(
        env.fake.with(|s| s.fail.is_empty()),
        "the limit was not hit"
    );
    assert_eq!(env.status_posts(), 0);
    assert!(env.row().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listings_past_their_caps_never_yield_a_success() {
    let env = ready().await;
    let entry = |n: usize| TreeEntry {
        id: format!("{n:040x}"),
        name: format!("f{n}"),
        kind: "blob".into(),
        mode: "100644".into(),
    };
    env.fake.with(|s| {
        s.projects.get_mut(&REPO_ID).unwrap().commits.insert(
            env.commit.clone(),
            FakeCommit {
                parent_ids: vec![env.seed.clone()],
                tree: (1..=501).map(entry).collect(),
            },
        );
    });
    env.poll().await;
    assert_eq!(env.status_posts(), 0);
    assert!(env.row().is_none());

    // More statuses than the list reads: the answer would be a guess, so nothing is posted.
    let env = ready().await;
    env.fake.with(|s| s.lose_next_status_reply = true);
    env.poll().await;
    env.fake.with(|s| {
        let project = s.projects.get_mut(&REPO_ID).unwrap();
        for id in 1000..1250 {
            project.statuses.push((
                env.commit.clone(),
                json!({"state": "pending", "name": CONTEXT, "description": "noise",
                    "ref": BRANCH, "id": id}),
            ));
        }
    });
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert_eq!(env.phase(), "attempting");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_the_profile_retires_its_success_once() {
    let mut env = ready().await;
    env.poll().await;
    assert_eq!(env.statuses()[0].1, "success");
    env.reconfigure(|p| p.publish_forge_status = false);
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (env.commit.as_str(), "failed")
    );
    assert!(posted[1].2.contains("forge check disabled"));
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_merge_request_still_has_its_success_retired_on_the_publication_branch() {
    let mut env = ready().await;
    env.poll().await;
    env.reconfigure(|p| p.publish_forge_status = false);
    env.fake
        .with(|s| s.projects.get_mut(&REPO_ID).unwrap().mrs.remove(&1));
    env.poll().await;
    let posted = env.statuses();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        (posted[1].0.as_str(), posted[1].1.as_str()),
        (env.commit.as_str(), "failed")
    );
    assert!(posted[1].2.contains("forge check disabled"));
    env.poll().await;
    assert_eq!(env.status_posts(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publication_to_another_host_is_never_reconciled_against_a_same_numbered_merge_request() {
    let env = ready().await;
    // A publication to another forge shares merge request number 2 with a qualifying, merged one.
    let other = "d".repeat(40);
    env.publish(
        2,
        &other,
        "agents/other",
        "https://github.example.invalid/group/sub/widgets.git",
    );
    env.fake.with(|s| {
        let project = s.projects.get_mut(&REPO_ID).unwrap();
        let mut mr = Mr::new(2, "agents/other", &[]);
        mr.state = "merged".into();
        mr.sha = other.clone();
        mr.merge_commit_sha = Some("e".repeat(40));
        project.mrs.insert(2, mr);
    });
    env.poll().await;
    env.poll().await;
    assert_eq!(env.status_posts(), 1);
    assert!(env.statuses().iter().all(|(sha, _, _)| *sha == env.commit));
    let foreign = env.store.pull_publications(REPO, 1, 1).unwrap();
    assert_eq!(
        foreign.len(),
        1,
        "a merge was recorded on the other host's publication"
    );
    // The bounded catch-up read is independent of the foreign publication above.
    assert_eq!(env.requests_to("/merge_requests/2"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_are_verified_and_trigger_a_reconcile_of_their_own_project() {
    let env = ready().await;
    let hook = env.mr_hook(REPO_ID);
    // A forged token and another project's event change nothing.
    assert_eq!(
        env.hook("Merge Request Hook", "u-1", &hook, "forged").await,
        401
    );
    assert_eq!(
        env.hook(
            "Merge Request Hook",
            "u-2",
            &env.mr_hook(999),
            WEBHOOK_TOKEN
        )
        .await,
        202
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(env.status_posts(), 0);

    assert_eq!(
        env.hook("Merge Request Hook", "u-3", &hook, WEBHOOK_TOKEN)
            .await,
        202
    );
    eventually("the success", || env.statuses().len() == 1).await;
    assert_eq!(env.statuses()[0].1, "success");

    // A push moves the target: every candidate is read again.
    env.advance_main();
    let push = json!({"object_kind": "push", "project_id": REPO_ID, "ref": "refs/heads/main"});
    assert_eq!(
        env.hook("Push Hook", "u-4", &push, WEBHOOK_TOKEN).await,
        202
    );
    eventually("the failure", || env.statuses().len() == 2).await;
    assert!(env.statuses()[1].2.contains("advanced"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_named_repository_on_another_host_stops_the_reconcile() {
    let env = ready().await;
    let hook = env.mr_hook(REPO_ID);
    assert_eq!(
        env.hook("Merge Request Hook", "u-1", &hook, WEBHOOK_TOKEN)
            .await,
        202
    );
    eventually("the success", || env.statuses().len() == 1).await;
    // Take the repository lock once more so the initial reconcile's confirming pass has finished.
    env.poll().await;
    // Publications are keyed by name alone: once another host shares the name, none is trusted.
    let other = NewConnection {
        id: format!("con_{}", "f".repeat(32)),
        provider: "github".into(),
        host: "github.com".into(),
        kind: "pat".into(),
        app_id: None,
        app_slug: None,
        acts_as: Some("robot".into()),
    };
    env.store.insert_connection(&other, "test").unwrap();
    env.store
        .upsert_repository(&other.id, "github.com", 555, REPO, None, None)
        .unwrap();
    env.advance_main();
    let push = json!({"object_kind": "push", "project_id": REPO_ID, "ref": "refs/heads/main"});
    assert_eq!(
        env.hook("Push Hook", "u-2", &push, WEBHOOK_TOKEN).await,
        202
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(env.status_posts(), 1);
}
