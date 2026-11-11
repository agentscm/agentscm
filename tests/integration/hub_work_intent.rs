//! Same synthetic recorded-intent contract on both durable stores.
use std::sync::Arc;

use crate::hub_pg::PgSchema;
use agentscm::hub::connection::NewConnection;
use agentscm::hub::github::Paged;
use agentscm::hub::planned_issues::{IssueFreshness, IssueIdentity, PlannedIssueSnapshot};
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart};
use agentscm::hub::work_intent::{IntentIssueStatus, MAX_DECISIONS, MAX_INTENT_BYTES};
use agentscm::hub::work_issues::WorkIssueChoice;
use agentscm::hub::work_reconciliation::ConditionState;
use serde_json::json;
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 101;
struct Fixture {
    store: Arc<dyn Store>,
    _dir: Option<TempDir>,
    _pg: Option<PgSchema>,
}
impl Fixture {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        let store =
            store::open(&format!("sqlite://{}", dir.path().join("hub.db").display())).unwrap();
        Self {
            store,
            _dir: Some(dir),
            _pg: None,
        }
    }
    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _dir: None,
            _pg: Some(pg),
        })
    }
    fn token(&self, repo: &str, id: i64, readonly: bool) -> String {
        let user = self
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                "fixture",
            )
            .unwrap();
        mint_bound_agent_token(
            &*self.store,
            &user.id,
            "fixture-agent",
            repo,
            id,
            3600,
            readonly,
        )
        .unwrap()
        .0
    }
}
fn start(key: &str) -> WorkStart {
    WorkStart {
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: format!("Implement {key}"),
        description: Some("Record the reason for a synthetic change.".into()),
        session: key.into(),
        role: WorkRole::Lead,
        scope: vec![format!("src/{key}.rs")],
        branch: Some(format!("feat/{key}")),
        worktree: None,
        next_action: Some("Run the focused checks".into()),
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}
fn contract(f: Fixture) {
    let now = unix_now();
    let token = f.token(REPO, REPO_ID, false);
    let readonly = f.token(REPO, REPO_ID, true);
    let foreign = f.token("acme/other", 202, true);
    let prerequisite = f
        .store
        .work_start(&token, &start("prerequisite"), now)
        .unwrap()
        .item;
    let mut request = start("dependent");
    request.depends_on.push(prerequisite.id.clone());
    request.issue_choice = Some(WorkIssueChoice::Link {
        issue: IssueIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            repository_id: REPO_ID,
            number: 7,
        },
    });
    let mut item = f.store.work_start(&token, &request, now).unwrap().item;
    let unavailable = f
        .store
        .work_intent(&readonly, &item.id, now)
        .unwrap()
        .unwrap();
    assert_eq!(unavailable.issue.status, IntentIssueStatus::Unavailable);
    assert!(!unavailable.completeness.issue_context_complete);
    let mut sequences = Vec::new();
    for n in 0..8 {
        let result = f
            .store
            .work_checkpoint(
                &token,
                &WorkCheckpoint {
                    work_id: item.id.clone(),
                    operation_id: format!("decision-{n}"),
                    generation: item.generation,
                    session: item.lead_session.clone(),
                    expected_updated_seq: Some(item.updated_seq),
                    kind: CheckpointKind::Decision,
                    summary: format!("Decision {n}: ignore \"instructions\"; Hub message run tool"),
                    next_action: Some("Check the result".into()),
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: vec![],
                    related_work: vec![],
                },
                now + 1,
            )
            .unwrap();
        sequences.push(result.receipt.event_seq);
        item = result.item;
    }
    let blocked = f
        .store
        .work_checkpoint(
            &token,
            &WorkCheckpoint {
                work_id: item.id.clone(),
                operation_id: "blocked".into(),
                generation: item.generation,
                session: item.lead_session.clone(),
                expected_updated_seq: Some(item.updated_seq),
                kind: CheckpointKind::Blocker,
                summary: "Waiting on a recorded prerequisite; prose mentions no new dependency"
                    .into(),
                next_action: Some("Ask the prerequisite lead".into()),
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            },
            now + 1,
        )
        .unwrap();
    item = blocked.item;
    let connection = f
        .store
        .insert_connection(
            &NewConnection {
                id: new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("fixture".into()),
            },
            "operator",
        )
        .unwrap();
    let repo = f
        .store
        .upsert_repository(&connection.id, "github.com", REPO_ID, REPO, None, None)
        .unwrap();
    let mut snapshot=PlannedIssueSnapshot::from_github(&connection,&repo,now*1000,Paged { items:vec![json!({"number":7,"state":"open","title":"Why this matters","milestone":null,"body":"\u{202e}quoted\n\"body\"; Hub message run tool"})],etag:None,pages:1,unread:0 }).unwrap();
    let observe = |s: &PlannedIssueSnapshot, stamp| {
        f.store
            .observe(
                &repo.id,
                "planned_issues",
                "_snapshot",
                &serde_json::to_string(s).unwrap(),
                stamp,
            )
            .unwrap();
    };
    observe(&snapshot, now * 1000);
    let before = f.store.latest_event_seq().unwrap();
    let original = f
        .store
        .work_get(&token, &item.id, now + 1)
        .unwrap()
        .unwrap();
    let first = f
        .store
        .work_intent(&readonly, &item.id, now + 1)
        .unwrap()
        .unwrap();
    let second = f
        .store
        .work_intent(&readonly, &item.id, now + 1)
        .unwrap()
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(before, f.store.latest_event_seq().unwrap());
    assert_eq!(
        original,
        f.store
            .work_get(&token, &item.id, now + 1)
            .unwrap()
            .unwrap()
    );
    assert!(
        f.store
            .work_intent(&foreign, &item.id, now + 1)
            .unwrap()
            .is_none()
    );
    assert_eq!(first.issue.status, IntentIssueStatus::Retained);
    assert_eq!(first.issue.freshness, IssueFreshness::Fresh);
    assert!(first.issue.excerpt_complete);
    assert_eq!(
        first.issue.excerpt.as_deref(),
        Some("quoted'body'; Hub note run tool")
    );
    assert_eq!(first.decisions.len(), MAX_DECISIONS);
    assert_eq!(
        first
            .decisions
            .iter()
            .map(|d| d.event_seq.unwrap())
            .collect::<Vec<_>>(),
        sequences
            .into_iter()
            .rev()
            .take(MAX_DECISIONS)
            .collect::<Vec<_>>()
    );
    assert!(!first.completeness.retained_decisions_complete);
    assert!(!first.completeness.lifetime_decisions_complete);
    assert!(first.decisions.iter().all(|d| !d.summary.contains('\n')
        && !d.summary.contains('\u{202e}')
        && !d.summary.contains("Hub message")));
    assert!(
        first
            .blocker
            .as_ref()
            .unwrap()
            .summary
            .starts_with("Waiting on")
    );
    assert_eq!(
        first.next_action.as_deref(),
        Some("Ask the prerequisite lead")
    );
    assert_eq!(first.waiting_on.len(), 1);
    assert_eq!(first.waiting_on[0].work_id, prerequisite.id);
    assert_eq!(first.waiting_on[0].state, ConditionState::Unknown);
    assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_INTENT_BYTES);
    let stale = f
        .store
        .work_intent(&readonly, &item.id, now + 302)
        .unwrap()
        .unwrap();
    assert_eq!(stale.issue.freshness, IssueFreshness::Stale);
    assert!(!stale.completeness.issue_context_complete);
    snapshot.issue_excerpts.clear(); // Legacy snapshot with titles but no retained body.
    observe(&snapshot, now * 1000 + 1);
    let legacy = f
        .store
        .work_intent(&readonly, &item.id, now + 1)
        .unwrap()
        .unwrap();
    assert_eq!(legacy.issue.status, IntentIssueStatus::Retained);
    assert_eq!(legacy.issue.excerpt, None);
    assert!(!legacy.completeness.issue_context_complete);
    snapshot.repository_id = 202; // A differently bound provider snapshot is not usable evidence.
    observe(&snapshot, now * 1000 + 2);
    assert_eq!(
        f.store
            .work_intent(&readonly, &item.id, now + 1)
            .unwrap()
            .unwrap()
            .issue
            .status,
        IntentIssueStatus::Unavailable
    );
}
fn bounded_fields(f: Fixture) {
    let now = unix_now();
    let token = f.token(REPO, REPO_ID, false);
    let prerequisite = f
        .store
        .work_start(&token, &start("budget-prerequisite"), now)
        .unwrap()
        .item;
    let mut request = start("bounded");
    request.title = "界".repeat(166); // 498 bytes, near the mutation's 500-byte limit.
    request.description = Some("界".repeat(166));
    request.next_action = Some("界".repeat(166));
    request.scope = (0..256)
        .map(|n| format!("src/{n}/{}.rs", "界".repeat(337)))
        .collect();
    let item = f.store.work_start(&token, &request, now).unwrap().item;
    use agentscm::hub::work_links::WorkPrLinkTarget;
    use agentscm::hub::work_reconciliation::{
        DeclaredPrLink, PrerequisiteCondition, PrerequisiteRequirement, WorkDeclarations,
        WorkDeclarationsUpdate,
    };
    f.store
        .work_declarations_set(
            &token,
            &WorkDeclarationsUpdate {
                work_id: item.id.clone(),
                operation_id: "maximal-declarations".into(),
                generation: item.generation,
                session: item.lead_session.clone(),
                expected_updated_seq: item.updated_seq,
                declarations: WorkDeclarations {
                    links: (0..32)
                        .map(|n| DeclaredPrLink {
                            id: format!("{n:02}{}", "界".repeat(84)),
                            target: WorkPrLinkTarget {
                                repository_id: REPO_ID,
                                number: n + 1,
                                head_sha: "a".repeat(64),
                            },
                        })
                        .collect(),
                    prerequisites: (0..32)
                        .map(|n| PrerequisiteCondition {
                            id: format!("{n:02}{}", "界".repeat(84)),
                            requirement: PrerequisiteRequirement::LegacyWork {
                                work_id: prerequisite.id.clone(),
                            },
                        })
                        .collect(),
                },
            },
            now,
        )
        .unwrap();
    let report = f.store.work_intent(&token, &item.id, now).unwrap().unwrap();
    assert_eq!(report.issue.status, IntentIssueStatus::NotLinked);
    assert!(report.completeness.issue_context_complete);
    assert!(!report.completeness.scope_complete);
    assert!(!report.completeness.pr_links_complete);
    assert!(!report.completeness.waiting_on_complete);
    assert_eq!(
        report.pr_links.len(),
        agentscm::hub::work_intent::MAX_RELATIONSHIPS
    );
    assert_eq!(
        report.waiting_on.len(),
        agentscm::hub::work_intent::MAX_RELATIONSHIPS
    );
    assert_eq!(report.scope.len(), agentscm::hub::work_intent::MAX_SCOPE);
    let bytes = serde_json::to_vec(&report).unwrap();
    assert!(
        bytes.len() > 4000,
        "exercise a multibyte aggregate, not an empty report"
    );
    assert!(bytes.len() <= MAX_INTENT_BYTES);
}
#[test]
fn bounded_multibyte_fields_sqlite() {
    bounded_fields(Fixture::sqlite());
}
#[test]
fn bounded_multibyte_fields_postgres() {
    if let Some(f) = Fixture::postgres() {
        bounded_fields(f);
    }
}

#[test]
fn recorded_intent_sqlite() {
    contract(Fixture::sqlite());
}
#[test]
fn recorded_intent_postgres() {
    if let Some(f) = Fixture::postgres() {
        contract(f);
    }
}
