//! Read-only Stop hook for agent-owned workflow tasks. It asks the task facade whether the exact
//! local Change subject is qualified and compares the native workspace to it. It never claims,
//! collects, uploads, finalizes, reconciles or checkpoints; `next` is data for the agent.
//! Feedback carries fixed categories only, never paths or server text.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};

use crate::agent_stop::{self, Prepared, StopAgent};
use crate::agent_task_cli::{QualificationFailure, RemoteArgs, qualification};
use agentscm::authority::Authority;
use agentscm::context::ChangeId;
use agentscm::hub::workspace_execution::RunSubject;
use agentscm::store::{Engine, validate_id};
use agentscm::workspace::{Status, Workspace, WorkspaceState};
use agentscm::{Error, Result};
use clap::Args;
use serde_json::{Value, json};

const MAX_WORKSPACE_CHANGES: usize = 4_096;
const MAX_HOOK_REASON_BYTES: usize = 16 * 1024;
const MAX_FAILURES: usize = 32;
const MAX_ID_BYTES: usize = 128;
const DEFAULT_ACTION: &str =
    "Resolve the reported problem, then try to finish again. The hook does not run tasks.";
const CHECKPOINT_ACTION: &str = "Checkpoint the workspace so the current bytes become the Change subject, then run the required tasks.";

#[derive(Debug, Args)]
pub struct TaskStopArgs {
    /// Local Change id.
    pub change: String,
    #[command(flatten)]
    pub remote: RemoteArgs,
    /// Native workspace holding the candidate bytes.
    #[arg(long)]
    pub workspace: PathBuf,
}

/// Local facts the pass rule needs besides the facade answer.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkspaceFacts {
    pub clean: bool,
    pub base_revision: String,
    pub generation: u64,
}

pub async fn task_stop(data_dir: &Path, args: TaskStopArgs, agent: StopAgent) -> Result<()> {
    let output = match agent_stop::prepare(
        data_dir,
        agent,
        Some(&args.workspace),
        &args.change,
        io::stdin().lock(),
    )? {
        Prepared::Done(output) => output,
        Prepared::Run(guard) => {
            let result = match check(data_dir, &args).await {
                Ok(result) => result,
                Err(failure) => json!({
                    "receipt": null,
                    "failures": [{"detail": failure_detail(failure)}],
                    "next_action": DEFAULT_ACTION,
                }),
            };
            guard.finish(&result, DEFAULT_ACTION, MAX_HOOK_REASON_BYTES)?
        }
    };
    println!("{output}");
    Ok(())
}

fn failure_detail(failure: QualificationFailure) -> &'static str {
    match failure {
        QualificationFailure::Local => "local native store, workspace or token file is unavailable",
        QualificationFailure::Request => "workflow service request failed",
        QualificationFailure::Rejected => "workflow service rejected the agent token",
        QualificationFailure::Malformed => "workflow service response was malformed",
    }
}

fn local(_: Error) -> QualificationFailure {
    QualificationFailure::Local
}

async fn check(
    data_dir: &Path,
    args: &TaskStopArgs,
) -> std::result::Result<Value, QualificationFailure> {
    check_with(data_dir, &args.change, &args.workspace, |subject| {
        qualification(&args.remote, subject)
    })
    .await
}

/// The remote call is injected so tests can move local state while it is "in flight". The local
/// subject is read again afterwards and must equal the one that was asked about.
async fn check_with<F, Fut>(
    data_dir: &Path,
    change_id: &str,
    workspace: &Path,
    qualify: F,
) -> std::result::Result<Value, QualificationFailure>
where
    F: FnOnce(RunSubject) -> Fut,
    Fut: Future<Output = std::result::Result<Value, QualificationFailure>>,
{
    let read_subject = || {
        let authority = Authority::open_read_only(data_dir).map_err(local)?;
        local_subject(&authority, change_id).map_err(local)
    };
    let (change, subject) = read_subject()?;
    let revision = subject.revision.clone();
    let before = read_facts(data_dir, workspace, &change)?;
    let answer = qualify(subject.clone()).await?;
    let (_, current) = read_subject()?;
    let after = read_facts(data_dir, workspace, &change)?;
    if current != subject {
        return Ok(block(
            vec![json!({"detail": "Change subject moved during qualification"})],
            CHECKPOINT_ACTION,
            Some(json!({"operation": "checkpoint"})),
        ));
    }
    Ok(decide(&answer, &revision, &before, &after))
}

fn read_facts(
    data_dir: &Path,
    path: &Path,
    change: &ChangeId,
) -> std::result::Result<WorkspaceFacts, QualificationFailure> {
    // `block_in_place` needs the multi-thread runtime that `main` uses.
    tokio::task::block_in_place(|| workspace_facts(data_dir, path, change)).map_err(local)
}

/// The subject the facade expects, from the local authoritative current Change.
fn local_subject(authority: &Authority, id: &str) -> Result<(ChangeId, RunSubject)> {
    let change = ChangeId {
        repository: authority.repository_id()?,
        id: id.to_owned(),
    };
    let state = authority.change(&change)?;
    let context = authority.review_context(&state.context_digest)?;
    let requirement = authority
        .workflow_requirement(&context.target)?
        .ok_or_else(|| Error::Conflict("target has no installed workflow requirement".into()))?;
    let subject = RunSubject {
        change_id: change.id.clone(),
        change_version: state.version,
        context_digest: state.context_digest,
        revision: context.revision,
        requirement_digest: requirement.digest()?,
    };
    Ok((change, subject))
}

fn workspace_facts(data_dir: &Path, path: &Path, change: &ChangeId) -> Result<WorkspaceFacts> {
    let engine = Engine::open_existing(data_dir)?;
    // Read-only and existing-only: creates no store files, never migrates the workspace schema
    // and never changes its journal mode.
    let workspace = Workspace::open_read_only(path)?;
    let state = workspace.state()?;
    let status = workspace.status(&engine, MAX_WORKSPACE_CHANGES)?;
    Ok(WorkspaceFacts {
        clean: workspace_clean(change, &state, &status),
        base_revision: state.base_revision,
        generation: state.generation,
    })
}

/// Ready, nothing pending, a complete scan with no edits, and the two reads agree.
fn workspace_clean(change: &ChangeId, state: &WorkspaceState, status: &Status) -> bool {
    state.change == *change
        && state.ready
        && state.pending.is_none()
        && status.pending.is_none()
        && status.complete
        && status.changes.is_empty()
        && status.base_revision == state.base_revision
        && status.generation == state.generation
}

/// Pure pass/block decision. Anything not positively shown qualified, current, clean and
/// unchanged across the remote call blocks. `revision` is the local subject revision.
pub fn decide(
    answer: &Value,
    revision: &str,
    before: &WorkspaceFacts,
    after: &WorkspaceFacts,
) -> Value {
    let Some(parsed) = Parsed::from(answer) else {
        return block(
            vec![json!({"detail": "qualification response is malformed"})],
            DEFAULT_ACTION,
            None,
        );
    };
    let changed =
        before.base_revision != after.base_revision || before.generation != after.generation;
    let clean = before.clean && after.clean;
    let revision_matches = parsed.revision == revision;
    let base_matches = after.base_revision == parsed.revision;
    if parsed.qualified
        && parsed.subject_current
        && clean
        && !changed
        && revision_matches
        && base_matches
        && let Some(receipt) = parsed.receipt
    {
        return json!({"receipt": receipt, "workspace_changed_during_run": changed});
    }
    let mut failures = parsed.failures;
    if !clean {
        failures.push(json!({"detail": "workspace has uncheckpointed edits"}));
    }
    if changed {
        failures.push(json!({"detail": "workspace changed during the qualification call"}));
    }
    if !revision_matches {
        failures
            .push(json!({"detail": "qualification revision is not the local subject revision"}));
    }
    if !base_matches {
        failures.push(json!({"detail": "workspace base is not the qualified revision"}));
    }
    if !clean || changed || !revision_matches || !base_matches {
        // The local bytes are not the subject that was asked about, so its follow-up is stale.
        let mut result = block(
            failures,
            CHECKPOINT_ACTION,
            Some(json!({"operation": "checkpoint"})),
        );
        result["workspace_changed_during_run"] = json!(changed);
        return result;
    }
    let action = match parsed.next {
        Next::CollectorClaim(_) => {
            "Run the required workflow task through agentscm agent-task (claim, collect, upload), then try to finish again."
        }
        Next::Reconcile(_) => {
            "A task outcome is uncertain. Reconcile it through agentscm agent-task (status, recovery), then try to finish again."
        }
        Next::CollectorFinalize => {
            "All required tasks succeeded. Finalize the receipt through agentscm agent-task (collector_finalize), then try to finish again."
        }
        Next::Checkpoint => CHECKPOINT_ACTION,
        Next::None => DEFAULT_ACTION,
    };
    block(failures, action, Some(parsed.next.to_value()))
}

fn block(failures: Vec<Value>, next_action: &str, next: Option<Value>) -> Value {
    let mut result = json!({"receipt": null, "failures": failures, "next_action": next_action});
    if let Some(next) = next {
        result["next"] = next;
    }
    result
}

/// The facade's typed follow-up; `task_id` is required exactly where a task is named.
enum Next {
    CollectorClaim(String),
    Reconcile(String),
    CollectorFinalize,
    Checkpoint,
    None,
}

impl Next {
    fn to_value(&self) -> Value {
        match self {
            Self::CollectorClaim(id) => json!({"operation": "collector_claim", "task_id": id}),
            Self::Reconcile(id) => json!({"operation": "reconcile", "task_id": id}),
            Self::CollectorFinalize => json!({"operation": "collector_finalize"}),
            Self::Checkpoint => json!({"operation": "checkpoint"}),
            Self::None => json!({"operation": "none"}),
        }
    }
}

struct Parsed<'a> {
    qualified: bool,
    subject_current: bool,
    receipt: Option<&'a str>,
    revision: &'a str,
    failures: Vec<Value>,
    next: Next,
}

fn is_token(value: &str) -> bool {
    (1..=MAX_ID_BYTES).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

impl<'a> Parsed<'a> {
    /// Strict shape check of the facade answer; any missing field, wrong type or value
    /// outside its bounded format is None.
    fn from(answer: &'a Value) -> Option<Self> {
        let receipt = match answer.get("receipt_digest")? {
            Value::Null => None,
            Value::String(digest) => {
                validate_id(digest).ok()?;
                Some(digest.as_str())
            }
            _ => return None,
        };
        let revision = answer.get("revision")?.as_str()?;
        validate_id(revision).ok()?;
        let entries = answer.get("failures")?.as_array()?;
        if entries.len() > MAX_FAILURES {
            return None;
        }
        let failures = entries
            .iter()
            .map(|failure| {
                let task_id = failure.get("task_id")?.as_str().filter(|id| is_token(id))?;
                let code = failure.get("code")?.as_str().filter(|id| is_token(id))?;
                Some(json!({"task_id": task_id, "code": code}))
            })
            .collect::<Option<Vec<_>>>()?;
        let next = answer.get("next")?;
        let operation = next.get("operation")?.as_str()?;
        let task_id = match next.get("task_id") {
            None => None,
            Some(id) => Some(id.as_str().filter(|id| is_token(id))?.to_owned()),
        };
        let next = match (operation, task_id) {
            ("collector_claim", Some(id)) => Next::CollectorClaim(id),
            ("reconcile", Some(id)) => Next::Reconcile(id),
            ("collector_finalize", None) => Next::CollectorFinalize,
            ("checkpoint", None) => Next::Checkpoint,
            ("none", None) => Next::None,
            _ => return None,
        };
        answer.get("tasks")?.as_array()?;
        Some(Self {
            qualified: answer.get("qualified")?.as_bool()?,
            subject_current: answer.get("subject_current")?.as_bool()?,
            receipt,
            revision,
            failures,
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision() -> String {
        "a".repeat(64)
    }

    fn answer(qualified: bool, current: bool, next: Value) -> Value {
        json!({
            "qualified": qualified,
            "subject_current": current,
            "receipt_digest": if qualified { json!("d".repeat(64)) } else { Value::Null },
            "revision": revision(),
            "tasks": [],
            "failures": [],
            "next": next,
        })
    }

    fn facts(clean: bool, base: &str) -> WorkspaceFacts {
        WorkspaceFacts {
            clean,
            base_revision: base.into(),
            generation: 3,
        }
    }

    fn run(answer: &Value, clean: bool, base: &str) -> Value {
        decide(
            answer,
            &revision(),
            &facts(clean, base),
            &facts(clean, base),
        )
    }

    fn details(result: &Value) -> Vec<&str> {
        result["failures"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|failure| failure["detail"].as_str())
            .collect()
    }

    #[test]
    fn qualified_and_clean_passes_with_the_receipt() {
        let result = run(
            &answer(true, true, json!({"operation": "none"})),
            true,
            &revision(),
        );
        assert_eq!(
            result,
            json!({"receipt": "d".repeat(64), "workspace_changed_during_run": false})
        );
    }

    #[test]
    fn qualified_but_dirty_blocks_with_the_uncheckpointed_detail() {
        let result = run(
            &answer(true, true, json!({"operation": "none"})),
            false,
            &revision(),
        );
        assert!(result["receipt"].is_null());
        assert_eq!(details(&result), ["workspace has uncheckpointed edits"]);
        assert_eq!(result["next"], json!({"operation": "checkpoint"}));
    }

    #[test]
    fn qualified_but_different_base_blocks() {
        let result = run(
            &answer(true, true, json!({"operation": "none"})),
            true,
            &"b".repeat(64),
        );
        assert!(result["receipt"].is_null());
        assert_eq!(
            details(&result),
            ["workspace base is not the qualified revision"]
        );
    }

    #[test]
    fn workspace_changed_across_the_call_blocks() {
        let good = answer(true, true, json!({"operation": "none"}));
        let before = facts(true, &revision());
        let mut moved = facts(true, &revision());
        moved.generation += 1;
        let mut rebased = facts(true, &"b".repeat(64));
        rebased.generation = before.generation;
        for after in [moved, rebased, facts(false, &revision())] {
            let result = decide(&good, &revision(), &before, &after);
            assert!(result["receipt"].is_null(), "{after:?}");
            assert_eq!(result["next"], json!({"operation": "checkpoint"}));
        }
        let mut moved = facts(true, &revision());
        moved.generation += 1;
        let result = decide(&good, &revision(), &before, &moved);
        assert_eq!(result["workspace_changed_during_run"], true);
        assert!(details(&result).contains(&"workspace changed during the qualification call"));
    }

    #[test]
    fn revision_other_than_the_local_subject_blocks() {
        let other = "b".repeat(64);
        let mut mismatch = answer(true, true, json!({"operation": "none"}));
        mismatch["revision"] = json!(other);
        let result = run(&mismatch, true, &other);
        assert!(result["receipt"].is_null());
        assert!(
            details(&result).contains(&"qualification revision is not the local subject revision")
        );
    }

    #[test]
    fn not_current_blocks_with_checkpoint_next() {
        let result = run(
            &answer(false, false, json!({"operation": "checkpoint"})),
            true,
            &revision(),
        );
        assert!(result["receipt"].is_null());
        assert_eq!(result["next"], json!({"operation": "checkpoint"}));
        assert!(
            result["next_action"]
                .as_str()
                .unwrap()
                .starts_with("Checkpoint")
        );
    }

    #[test]
    fn uncertain_blocks_with_reconcile_next_and_typed_failures() {
        let mut uncertain = answer(
            false,
            true,
            json!({"operation": "reconcile", "task_id": "task-1"}),
        );
        uncertain["failures"] = json!([{"task_id": "task-1", "code": "outcome_uncertain"}]);
        let result = run(&uncertain, true, &revision());
        assert!(result["receipt"].is_null());
        assert_eq!(
            result["next"],
            json!({"operation": "reconcile", "task_id": "task-1"})
        );
        assert_eq!(
            result["failures"],
            json!([{"task_id": "task-1", "code": "outcome_uncertain"}])
        );
        assert!(
            result["next_action"]
                .as_str()
                .unwrap()
                .contains("Reconcile")
        );
    }

    #[test]
    fn unqualified_collection_work_names_the_claim() {
        let result = run(
            &answer(
                false,
                true,
                json!({"operation": "collector_claim", "task_id": "t"}),
            ),
            true,
            &revision(),
        );
        assert!(result["next_action"].as_str().unwrap().contains("claim"));
    }

    #[test]
    fn qualified_without_a_receipt_never_passes() {
        let mut odd = answer(true, true, json!({"operation": "none"}));
        odd["receipt_digest"] = Value::Null;
        assert!(run(&odd, true, &revision())["receipt"].is_null());
    }

    #[test]
    fn malformed_answers_block() {
        let good = answer(true, true, json!({"operation": "none"}));
        let mut cases = vec![
            json!(null),
            json!({}),
            json!({"qualified": true}),
            json!("qualified"),
        ];
        for field in [
            "qualified",
            "subject_current",
            "receipt_digest",
            "revision",
            "tasks",
            "failures",
            "next",
        ] {
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push(missing);
        }
        let many = (0..=MAX_FAILURES)
            .map(|index| json!({"task_id": format!("t{index}"), "code": "c"}))
            .collect::<Vec<_>>();
        assert_eq!(many.len(), 33);
        for (field, wrong) in [
            ("qualified", json!("true")),
            ("subject_current", json!(1)),
            ("receipt_digest", json!(7)),
            ("receipt_digest", json!("")),
            ("receipt_digest", json!("not-a-digest")),
            ("receipt_digest", json!("D".repeat(64))),
            ("receipt_digest", json!("d".repeat(63))),
            ("revision", json!(null)),
            ("revision", json!("rev-1")),
            ("revision", json!("A".repeat(64))),
            ("tasks", json!({})),
            ("failures", json!("none")),
            ("failures", json!([{"task_id": "t"}])),
            ("failures", json!([{"task_id": "t", "code": "has space"}])),
            ("failures", json!([{"task_id": "t u", "code": "c"}])),
            (
                "failures",
                json!([{"task_id": "t", "code": "c".repeat(129)}]),
            ),
            ("failures", json!(many)),
            ("next", json!("none")),
            ("next", json!({"operation": 3})),
            ("next", json!({"operation": "launch"})),
            ("next", json!({"operation": "reconcile", "task_id": 3})),
            ("next", json!({"operation": "reconcile"})),
            ("next", json!({"operation": "collector_claim"})),
            ("next", json!({"operation": "reconcile", "task_id": "a b"})),
            ("next", json!({"operation": "none", "task_id": "t"})),
            ("next", json!({"operation": "checkpoint", "task_id": "t"})),
            (
                "next",
                json!({"operation": "collector_finalize", "task_id": "t"}),
            ),
        ] {
            let mut bad = good.clone();
            bad[field] = wrong;
            cases.push(bad);
        }
        for case in cases {
            let result = run(&case, true, &revision());
            assert!(result["receipt"].is_null(), "{case}");
            assert_eq!(details(&result), ["qualification response is malformed"]);
        }
    }

    #[test]
    fn thirty_two_failures_are_accepted() {
        let mut full = answer(false, true, json!({"operation": "none"}));
        full["failures"] = json!(
            (0..MAX_FAILURES)
                .map(|index| json!({"task_id": format!("t{index}"), "code": "c"}))
                .collect::<Vec<_>>()
        );
        let result = run(&full, true, &revision());
        assert_eq!(result["failures"].as_array().unwrap().len(), MAX_FAILURES);
    }

    mod interleaving {
        use super::*;
        use agentscm::authority::workflows::WorkflowRequirement;
        use agentscm::checks::SourcePathExpectation;
        use agentscm::context::{InputIdentity, REVIEW_CONTEXT_VERSION, ReviewContext};
        use agentscm::skill_workflow::{
            SkillReference, StepAction, StepDefinition, WorkflowDefinition, WorkflowSource, compile,
        };
        use std::collections::BTreeSet;
        use std::fs;

        struct Fixture {
            data: tempfile::TempDir,
            workspace: PathBuf,
            context: ReviewContext,
        }

        impl Fixture {
            fn new() -> Self {
                let data = tempfile::tempdir().unwrap();
                let engine = Engine::open(data.path()).unwrap();
                let revision = engine
                    .checkpoint(
                        None,
                        &[
                            agentscm::model::FileChange {
                                path: "SKILL.md".into(),
                                content: Some(b"# Synthetic".to_vec()),
                            },
                            agentscm::model::FileChange {
                                path: "src/file".into(),
                                content: Some(b"abc".to_vec()),
                            },
                        ],
                    )
                    .unwrap();
                let mut authority = Authority::open(data.path()).unwrap();
                let repository = authority.repository_id().unwrap();
                let definition = WorkflowDefinition {
                    version: 1,
                    name: "stop".into(),
                    source: WorkflowSource {
                        repository: repository.clone(),
                        revision: revision.clone(),
                        policy_epoch: 0,
                    },
                    skill: SkillReference {
                        entrypoint: "SKILL.md".into(),
                        resources: vec![],
                    },
                    steps: vec![StepDefinition {
                        id: "tests".into(),
                        needs: vec![],
                        required: true,
                        action: StepAction::SourcePaths {
                            expectations: vec![SourcePathExpectation {
                                path: "src/file".into(),
                                sha256: agentscm::store::digest(b"abc"),
                            }],
                        },
                    }],
                };
                let plan = compile(data.path(), &definition).unwrap();
                authority
                    .install_workflow_requirement(
                        &WorkflowRequirement::from_plan("main", &plan).unwrap(),
                    )
                    .unwrap();
                let digest = |fill: &str| InputIdentity::Digest {
                    sha256: fill.repeat(64),
                };
                let context = ReviewContext {
                    version: REVIEW_CONTEXT_VERSION,
                    change: ChangeId {
                        repository,
                        id: "change-1".into(),
                    },
                    change_version: 1,
                    revision: revision.clone(),
                    base: None,
                    target: "main".into(),
                    policy_epoch: authority.policy_epoch().unwrap(),
                    intent: "exercise the stop hook".into(),
                    acceptance_criteria: vec!["the subject is stable".into()],
                    scope: vec!["src/file".into()],
                    exclusions: vec![],
                    omissions: vec![],
                    dependencies: vec![],
                    recipe: digest("1"),
                    toolchain: digest("2"),
                    environment: digest("3"),
                    required_evidence: vec![],
                    required_capabilities: vec!["agentscm.review-context/v1".into()],
                };
                authority.start_change(&context).unwrap();
                drop(authority);
                let workspace = data.path().join("ws");
                Workspace::create(&engine, &workspace, context.change.clone(), &revision).unwrap();
                Self {
                    data,
                    workspace,
                    context,
                }
            }

            async fn run(
                &self,
                during: impl FnOnce(&RunSubject),
            ) -> std::result::Result<Value, QualificationFailure> {
                check_with(
                    self.data.path(),
                    "change-1",
                    &self.workspace,
                    |subject: RunSubject| {
                        during(&subject);
                        let mut good = answer(true, true, json!({"operation": "none"}));
                        good["revision"] = json!(subject.revision);
                        std::future::ready(Ok(good))
                    },
                )
                .await
            }

            fn inventory(&self) -> BTreeSet<String> {
                fn walk(dir: &Path, out: &mut BTreeSet<String>) {
                    for entry in fs::read_dir(dir).unwrap() {
                        let path = entry.unwrap().path();
                        let name = path.to_string_lossy().into_owned();
                        if name.ends_with("-wal") || name.ends_with("-shm") {
                            continue;
                        }
                        out.insert(name);
                        if path.is_dir() {
                            walk(&path, out);
                        }
                    }
                }
                let mut out = BTreeSet::new();
                walk(self.data.path(), &mut out);
                out
            }
        }

        fn blocked(result: &Value, detail: &str) {
            assert!(result["receipt"].is_null(), "{result}");
            assert_eq!(result["next"], json!({"operation": "checkpoint"}));
            assert!(details(result).contains(&detail), "{result}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn unchanged_subject_passes() {
            let fixture = Fixture::new();
            let result = fixture.run(|_| {}).await.unwrap();
            assert_eq!(result["receipt"], json!("d".repeat(64)));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn workspace_edit_during_the_call_blocks() {
            let fixture = Fixture::new();
            let result = fixture
                .run(|_| fs::write(fixture.workspace.join("src/file"), b"edited").unwrap())
                .await
                .unwrap();
            blocked(&result, "workspace has uncheckpointed edits");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn workspace_checkpoint_during_the_call_blocks() {
            let fixture = Fixture::new();
            let result = fixture
                .run(|_| {
                    fs::write(fixture.workspace.join("src/file"), b"edited").unwrap();
                    let engine = Engine::open(fixture.data.path()).unwrap();
                    let mut workspace = Workspace::open(&fixture.workspace).unwrap();
                    let generation = workspace.state().unwrap().generation;
                    workspace.checkpoint(&engine, "op-1", generation).unwrap();
                })
                .await
                .unwrap();
            blocked(&result, "workspace changed during the qualification call");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn change_context_update_during_the_call_blocks() {
            let fixture = Fixture::new();
            let result = fixture
                .run(|_| {
                    let mut next = fixture.context.clone();
                    next.change_version = 2;
                    next.intent = "a later context".into();
                    Authority::open(fixture.data.path())
                        .unwrap()
                        .update_change_context(1, &next)
                        .unwrap();
                })
                .await
                .unwrap();
            blocked(&result, "Change subject moved during qualification");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn missing_store_component_fails_closed_and_creates_nothing() {
            let fixture = Fixture::new();
            fs::remove_dir_all(fixture.data.path().join("objects")).unwrap();
            let before = fixture.inventory();
            let failure = fixture.run(|_| {}).await.unwrap_err();
            assert_eq!(failure, QualificationFailure::Local);
            assert_eq!(
                failure_detail(failure),
                "local native store, workspace or token file is unavailable"
            );
            assert_eq!(fixture.inventory(), before);
        }
    }
}
