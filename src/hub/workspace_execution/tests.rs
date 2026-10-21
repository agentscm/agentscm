//! Synthetic dispatch-boundary tests. They do not qualify Docker or Claude.
use super::*;
use crate::hub::store::{self, Store, mint_bound_agent_token};
use crate::hub::workspace::{Identity, MemberChange, WorkspaceConfig};
use crate::model::FileChange;
use crate::skill_workflow::{
    SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource,
};
use crate::store::{Engine, digest};
use crate::team::auth::Role;
use serde_json::json;
use tempfile::TempDir;

const REPO: &str = "sample/project";
const SKILL: &str = "skills/check/SKILL.md";

struct Fixture {
    root: TempDir,
    workspace: Arc<Workspace>,
    store: Arc<dyn Store>,
    config: ExecutionConfig,
    owner: WorkspaceActor,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let native = root.path().join("native");
        fs::create_dir(&native).unwrap();
        let engine = Engine::open(&native).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[
                    FileChange {
                        path: SKILL.into(),
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
            name: "synthetic-boundary".into(),
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
                    expectations: vec![crate::checks::SourcePathExpectation {
                        path: "src/example.rs".into(),
                        sha256: digest(b"fn example() {}\n"),
                    }],
                },
            }],
        };
        let plan = skill_workflow::compile(&native, &definition).unwrap();
        let required = WorkflowRequirement::from_plan("main", &plan).unwrap();
        let requirement_digest = authority.install_workflow_requirement(&required).unwrap();
        let context:ReviewContext = serde_json::from_value(json!({
            "version":1,"change":{"repository":repository,"id":"work"},"change_version":1,
            "revision":revision,"base":null,"target":"main","policy_epoch":1,
            "intent":"Verify synthetic source","acceptance_criteria":["Source matches"],"scope":["src/example.rs"],
            "exclusions":[],"omissions":[],"dependencies":[],
            "recipe":{"state":"digest","sha256":"a".repeat(64)},
            "toolchain":{"state":"digest","sha256":"b".repeat(64)},
            "environment":{"state":"digest","sha256":"c".repeat(64)},
            "required_evidence":[],"required_capabilities":["agentscm.review-context/v1"]
        })).unwrap();
        authority.start_change(&context).unwrap();
        drop(authority);
        let store = store::open(&format!(
            "sqlite://{}",
            root.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let identity = Identity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: 101,
        };
        let workspace = Arc::new(
            Workspace::open(
                WorkspaceConfig {
                    workspace_id: "synthetic-workspace".into(),
                    name: "Synthetic workspace".into(),
                    database: root.path().join("workspace.sqlite"),
                    initial_owner: identity.clone(),
                },
                store.clone(),
            )
            .unwrap(),
        );
        let owner = browser_actor(&root, &workspace, &*store, &identity);
        let config = ExecutionConfig {
            version: 1,
            source_profiles: vec![],
            agent_task_profiles: vec![],
            profiles: vec![ExecutionProfile {
                repository_id: REPO.into(),
                profile_id: "checks".into(),
                native_repository: context.change.repository.clone(),
                data_dir: native,
                target: "main".into(),
                requirement_digest,
                docker: root.path().join("not-invoked-docker"),
                docker_socket: root.path().join("not-invoked.socket"),
                claude: root.path().join("not-invoked-claude"),
            }],
        };
        Self {
            root,
            workspace,
            store,
            config,
            owner,
        }
    }
    fn service(&self) -> ExecutionService {
        ExecutionService::open(
            &self.root.path().join("queue.sqlite"),
            self.config.clone(),
            self.workspace.clone(),
        )
        .unwrap()
    }
    fn request(&self, service: &ExecutionService, id: &str) -> StartRequest {
        StartRequest {
            request_id: id.into(),
            expected_generation: self.owner.generation(),
            profile_id: "checks".into(),
            subject: service
                .subject(&self.owner, REPO, "checks", "work")
                .unwrap(),
        }
    }
    fn member(&self) -> WorkspaceActor {
        let identity = Identity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: 202,
        };
        self.grant(
            &identity,
            vec![Role::Reader, Role::Contributor],
            vec![REPO.into()],
            true,
            "add-member",
        );
        browser_actor(&self.root, &self.workspace, &*self.store, &identity)
    }
    fn grant(
        &self,
        identity: &Identity,
        roles: Vec<Role>,
        repositories: Vec<String>,
        enabled: bool,
        id: &str,
    ) {
        self.workspace
            .put_member(
                &self.owner,
                MemberChange {
                    request_id: id.into(),
                    expected_generation: self.workspace.settings().unwrap().generation,
                    identity: identity.clone(),
                    roles,
                    repository_ids: repositories,
                    enabled,
                },
            )
            .unwrap();
    }
    fn native_success(&self, run: &WorkspaceRun) {
        let profile = &self.config.profiles[0];
        let context = profile.validate_subject(&run.subject).unwrap();
        let (_, requirement) = profile.current_requirement().unwrap();
        let plan = skill_workflow::compile(
            &profile.data_dir,
            &requirement.instantiate(&context).unwrap(),
        )
        .unwrap();
        skill_workflow::start(&profile.data_dir, &run.native_run_id, &plan).unwrap();
        assert_eq!(
            skill_workflow::advance(&profile.data_dir, &run.native_run_id, 1)
                .unwrap()
                .state,
            RunState::Succeeded
        );
    }
}

fn browser_actor(
    root: &TempDir,
    workspace: &Workspace,
    store: &dyn Store,
    identity: &Identity,
) -> WorkspaceActor {
    let user = store
        .upsert_user(&identity.forge(), "synthetic-user")
        .unwrap();
    workspace.issue_session(&user).unwrap();
    let db = Connection::open(root.path().join("workspace.sqlite")).unwrap();
    let session: String = db
        .query_row(
            "SELECT digest FROM workspace_sessions WHERE user_id=?1 ORDER BY rowid DESC LIMIT 1",
            [&user.id],
            |row| row.get(0),
        )
        .unwrap();
    let generation: i64 = db.query_row("SELECT m.generation FROM workspace_members m JOIN workspace_sessions s ON s.identity_key=m.identity_key WHERE s.digest=?1",[&session],|row|row.get(0)).unwrap();
    serde_json::from_value(json!({"workspace_id":"synthetic-workspace","identity":identity,"user_id":user.id,"generation":generation,"credential":{"Browser":{"digest":session}}})).unwrap()
}

#[test]
fn one_profile_per_repository_and_target_may_publish_forge_status() {
    let fixture = Fixture::new();
    let open = |profiles: Vec<AgentTaskProfile>| {
        let mut config = fixture.config.clone();
        config.agent_task_profiles = profiles;
        ExecutionService::open(
            &fixture.root.path().join("publish-queue.sqlite"),
            config,
            fixture.workspace.clone(),
        )
    };
    let publishing = |repository: &str, id: &str| {
        let mut task = AgentTaskProfile::from(&fixture.config.profiles[0]);
        (task.repository_id, task.profile_id) = (repository.into(), id.into());
        task.publish_forge_status = true;
        task
    };
    open(vec![publishing(REPO, "one")]).unwrap();
    let error = open(vec![
        publishing(REPO, "one"),
        publishing(&REPO.to_uppercase(), "two"),
    ])
    .err()
    .expect("two publishers for one repository and target");
    assert!(error.to_string().contains("forge status"), "{error}");
}

#[test]
fn requests_bind_exact_registered_subject_and_are_durably_idempotent() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let request = fixture.request(&service, "same-request");
    let run = service
        .start(&fixture.owner, REPO, request.clone())
        .unwrap();
    for key in [
        "data_dir",
        "executable",
        "docker_socket",
        "provider_url",
        "skill",
        "workflow",
    ] {
        let mut body = serde_json::to_value(&request).unwrap();
        body.as_object_mut()
            .unwrap()
            .insert(key.into(), json!("request-controlled"));
        assert!(
            serde_json::from_value::<StartRequest>(body).is_err(),
            "unknown field {key} was accepted"
        );
    }

    assert_eq!(
        service
            .start(&fixture.owner, REPO, request.clone())
            .unwrap(),
        run
    );
    let mut conflict = request.clone();
    conflict.subject.revision = "f".repeat(64);
    assert!(service.start(&fixture.owner, REPO, conflict).is_err());
    let mut wrong_profile = request.clone();
    wrong_profile.request_id = "other-profile".into();
    wrong_profile.profile_id = "unregistered".into();
    assert!(service.start(&fixture.owner, REPO, wrong_profile).is_err());
    let mut wrong_requirement = request.clone();
    wrong_requirement.request_id = "other-requirement".into();
    wrong_requirement.subject.requirement_digest = "f".repeat(64);
    assert!(
        service
            .start(&fixture.owner, REPO, wrong_requirement)
            .is_err()
    );
    assert!(
        service
            .start(&fixture.owner, "sample/other", request)
            .is_err()
    );
    assert!(
        service
            .get(&fixture.owner, "sample/other", &run.run_id)
            .is_err()
    );
    let page = service
        .subjects(&fixture.owner, REPO, "checks", 20, None)
        .unwrap();
    assert_eq!(page.subjects.len(), 1);
    assert_eq!(page.subjects[0].change_id, "work");
}

#[test]
fn restart_marks_queued_and_running_unknown_and_never_replays() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let first = service
        .start(&fixture.owner, REPO, fixture.request(&service, "queued"))
        .unwrap();
    let second = service
        .start(&fixture.owner, REPO, fixture.request(&service, "running"))
        .unwrap();
    {
        let mut queue = service.lock().unwrap();
        let mut run = queue.get(REPO, &second.run_id).unwrap();
        run.state = ExecutionState::Running;
        queue.update(&mut run).unwrap();
    }
    assert!(
        ExecutionService::open(
            &fixture.root.path().join("queue.sqlite"),
            fixture.config.clone(),
            fixture.workspace.clone()
        )
        .is_err(),
        "second server must not recover an active queue"
    );
    drop(service);
    let service = fixture.service();
    for run in [&first, &second] {
        let retained = service.get(&fixture.owner, REPO, &run.run_id).unwrap();
        assert_eq!(retained.state, ExecutionState::Unknown);
        assert!(retained.receipt.is_none());
    }
    assert!(
        !service
            .process_next_using(|_, _| panic!("interrupted run replayed"))
            .unwrap()
    );
    let events: i64 = service
        .lock()
        .unwrap()
        .db
        .query_row(
            "SELECT count(*) FROM workspace_run_events WHERE event='unknown'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 2);
}

#[test]
fn cancellation_is_exact_idempotent_and_no_late_result_can_pass() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let run = service
        .start(&fixture.owner, REPO, fixture.request(&service, "run"))
        .unwrap();
    service
        .process_next_using(|_, running| {
            fixture.native_success(running);
            let cancel = CancelRequest {
                request_id: "cancel".into(),
                expected_generation: running.generation,
            };
            let cancelled = service
                .cancel(&fixture.owner, REPO, &run.run_id, cancel.clone())
                .unwrap();
            assert_eq!(
                service
                    .cancel(&fixture.owner, REPO, &run.run_id, cancel)
                    .unwrap(),
                cancelled
            );
            Ok((
                ExecutionState::Succeeded,
                "late synthetic boundary result".into(),
            ))
        })
        .unwrap();
    let final_run = service.get(&fixture.owner, REPO, &run.run_id).unwrap();
    assert_eq!(final_run.state, ExecutionState::Cancelled);
    assert!(final_run.receipt.is_none());
    let authority =
        Connection::open(fixture.config.profiles[0].data_dir.join("state.sqlite")).unwrap();
    assert_eq!(
        authority
            .query_row("SELECT count(*) FROM workflow_receipts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn role_revocation_between_dispatches_blocks_next_call_and_passing_receipt() {
    let fixture = Fixture::new();
    let member = fixture.member();
    let service = fixture.service();
    let mut request = fixture.request(&service, "run");
    request.expected_generation = member.generation();
    let run = service.start(&member, REPO, request).unwrap();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    service
        .process_next_using(|actor, running| {
            service.check_current(actor, running).unwrap();
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            fixture.native_success(running);
            fixture.grant(
                &Identity::from(&member.identity()),
                vec![Role::Reader],
                vec![REPO.into()],
                true,
                "revoke-contributor",
            );
            assert!(service.check_current(actor, running).is_err());
            // A completed external result cannot bypass final current authorization.
            Ok((ExecutionState::Succeeded, "late synthetic result".into()))
        })
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    let retained = service.get(&fixture.owner, REPO, &run.run_id).unwrap();
    assert_eq!(retained.state, ExecutionState::Stale);
    assert!(retained.receipt.is_none());
}

#[test]
fn real_native_source_plan_finalizes_only_after_current_fence() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let run = service
        .start(&fixture.owner, REPO, fixture.request(&service, "run"))
        .unwrap();
    service
        .process_next_using(|actor, running| {
            service.check_current(actor, running).unwrap();
            fixture.native_success(running);
            Ok((
                ExecutionState::Succeeded,
                "native source observations complete".into(),
            ))
        })
        .unwrap();
    let retained = service.get(&fixture.owner, REPO, &run.run_id).unwrap();
    assert_eq!(retained.state, ExecutionState::Succeeded);
    assert!(retained.receipt.is_some());
    assert!(
        service
            .workflow(&fixture.owner, REPO, &run.run_id)
            .unwrap()
            .projection
            .is_some()
    );
}

#[test]
fn agent_tokens_require_scope_write_permission_and_current_revocation_check() {
    let fixture = Fixture::new();
    let service = fixture.service();
    let identity = fixture.owner.identity();
    let user = fixture
        .store
        .upsert_user(&identity, "synthetic-user")
        .unwrap();
    for (repository, read_only) in [(REPO, true), ("sample/other", false), (REPO, false)] {
        let (token_id, secret, _) = mint_bound_agent_token(
            &*fixture.store,
            &user.id,
            "synthetic-agent",
            repository,
            1,
            600,
            read_only,
        )
        .unwrap();
        let digest = store::agent_token_digest(secret.expose()).unwrap();
        let actor:WorkspaceActor=serde_json::from_value(json!({"workspace_id":"synthetic-workspace","identity":Identity::from(&identity),"user_id":user.id,"generation":1,"credential":{"Agent":{"digest":digest.as_str(),"repository":repository,"read_only":read_only}}})).unwrap();
        if read_only || repository != REPO {
            assert!(
                service
                    .start(&actor, REPO, fixture.request(&service, "run"))
                    .is_err()
            );
        } else {
            let run = service
                .start(&actor, REPO, fixture.request(&service, "write-run"))
                .unwrap();
            service
                .process_next_using(|current, running| {
                    service.check_current(current, running).unwrap();
                    fixture.native_success(running);
                    fixture
                        .store
                        .revoke_owned_agent_token(&user.id, &token_id)
                        .unwrap();
                    assert!(service.check_current(current, running).is_err());
                    Ok((
                        ExecutionState::Succeeded,
                        "late result after token revocation".into(),
                    ))
                })
                .unwrap();
            let retained = service.get(&fixture.owner, REPO, &run.run_id).unwrap();
            assert_eq!(retained.state, ExecutionState::Stale);
            assert!(retained.receipt.is_none());
        }
    }
}
