//! Work-insight storage conformance. Every case runs on SQLite and, when configured, PostgreSQL.
use crate::hub_pg;

use std::sync::Arc;

use agentscm::hub::HubError;
use agentscm::hub::connection::{ForgeCheckReserve, NewConnection, NewForgeCheck};
use agentscm::hub::insights::{
    AttemptOutcome, AttemptPhase, CostKind, InsightQuery, MAX_MEASUREMENT, MAX_OBSERVATIONS,
    ReportedCost, UsageBasis, UsageRecord,
};
use agentscm::hub::publish::{Change, Manifest, NewPublication, Person, Update, derive_id};
use agentscm::hub::store;
use agentscm::hub::store::{
    ForgeIdentity, Store, mint_agent_token, mint_bound_agent_token, unix_now,
};
use agentscm::hub::work::{
    CheckpointKind, SourceRef, WorkCheckpoint, WorkRepositoryScope, WorkRole, WorkStart,
};
use hub_pg::PgSchema;
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
const TTL: i64 = 3600;

struct Db {
    url: String,
    _pg: Option<PgSchema>,
    _directory: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let directory = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", directory.path().join("hub.sqlite").display()),
            _pg: None,
            _directory: Some(directory),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            _pg: Some(pg),
            _directory: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    fn mark_legacy_created_seq(&self, work: &str) {
        if self._pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
            client.execute("UPDATE hub_work_items SET created_seq=NULL, snapshot=jsonb_set(snapshot::jsonb,'{created_seq}','0')::text WHERE id=$1", &[&work]).unwrap();
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let connection = rusqlite::Connection::open(path).unwrap();
            connection.execute("UPDATE hub_work_items SET created_seq=NULL, snapshot=json_set(snapshot,'$.created_seq',0) WHERE id=?1", [work]).unwrap();
        }
    }
}

#[derive(Clone)]
struct Agent {
    token: String,
}

struct AgentSpec<'a> {
    provider: &'a str,
    host: &'a str,
    user_id: i64,
    repository: &'a str,
    repository_id: i64,
    read_only: bool,
}

fn agent(store: &dyn Store, spec: AgentSpec<'_>) -> Agent {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: spec.provider.into(),
                host: spec.host.into(),
                user_id: spec.user_id,
            },
            &format!("user-{}", spec.user_id),
        )
        .unwrap();
    let (token, _, _) = mint_bound_agent_token(
        store,
        &owner.id,
        &format!("agent-{}", spec.user_id),
        spec.repository,
        spec.repository_id,
        TTL,
        spec.read_only,
    )
    .unwrap();
    Agent { token }
}

fn ordinary_agent(store: &dyn Store, user_id: i64) -> Agent {
    agent(
        store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID,
            read_only: false,
        },
    )
}

fn start(key: &str, title: &str, session: &str) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: title.into(),
        session: session.into(),
        role: WorkRole::Lead,
        scope: vec![format!("src/{key}.rs")],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

fn join(work_id: &str, session: &str, role: WorkRole) -> WorkStart {
    WorkStart {
        work_id: Some(work_id.into()),
        source: None,
        request_key: None,
        title: "join work".into(),
        session: session.into(),
        role,
        scope: vec![],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

fn checkpoint(
    work_id: &str,
    operation_id: &str,
    generation: i64,
    session: &str,
    kind: CheckpointKind,
) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: work_id.into(),
        operation_id: operation_id.into(),
        generation,
        session: session.into(),
        expected_updated_seq: None,
        kind,
        summary: "synthetic checkpoint".into(),
        next_action: None,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    }
}

fn apply_checkpoint(
    store: &dyn Store,
    token: &str,
    request: &WorkCheckpoint,
    now: i64,
) -> Result<agentscm::hub::work::WorkCheckpointResult, HubError> {
    let mut request = request.clone();
    request.expected_updated_seq = Some(
        store
            .work_get(token, &request.work_id, now)?
            .unwrap()
            .updated_seq,
    );
    store.work_checkpoint(token, &request, now)
}

fn publication(
    repository: &str,
    token: &str,
    store: &dyn Store,
    event: i64,
    commit: &str,
    branch: &str,
    now: i64,
) -> String {
    let person = Person {
        name: "Synthetic".into(),
        email: "synthetic@example.invalid".into(),
        date: "2026-10-22T00:00:00Z".into(),
    };
    let manifest = Manifest {
        revision: "a".repeat(64),
        event_seq: Some(event),
        candidate: None,
        commit: commit.into(),
        tree: "b".repeat(40),
        parent: "c".repeat(40),
        branch: branch.into(),
        base: "main".into(),
        title: "Synthetic publication".into(),
        body: String::new(),
        author: person.clone(),
        committer: person,
        message: "Synthetic publication\n".into(),
        changes: vec![Change {
            path: "synthetic.txt".into(),
            mode: "100644".into(),
            blob: Some("d".repeat(40)),
            sha256: Some("e".repeat(64)),
            size: 1,
        }],
    };
    let id = derive_id(repository, event as u64, commit, branch);
    store
        .create_publication(
            token,
            &NewPublication {
                id: id.clone(),
                manifest,
            },
            now,
        )
        .unwrap();
    id
}

fn usage(work_id: &str, observation_id: &str, generation: i64, session: &str) -> UsageRecord {
    UsageRecord {
        work_id: work_id.into(),
        observation_id: observation_id.into(),
        generation,
        session: session.into(),
        phase: AttemptPhase::Implementation,
        outcome: AttemptOutcome::Succeeded,
        agent: "custom-mcp".into(),
        provider: "provider-x".into(),
        model: "model-y".into(),
        usage_basis: UsageBasis::Reported,
        source_commit: None,
        input_tokens: Some(7),
        output_tokens: Some(11),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        action_id: None,
        attempt_id: None,
        provider_request_id: None,
        provider_event_id: None,
        measurement_scope: None,
        cost: None,
    }
}

fn invalid<T>(result: Result<T, HubError>, expected: &str) {
    assert!(
        matches!(result, Err(HubError::Invalid(ref message)) if message == expected),
        "expected {expected:?}"
    );
}

fn replay_and_authorization(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 1);
    let helper = ordinary_agent(&*store, 2);
    let successor = ordinary_agent(&*store, 3);
    let now = unix_now();
    let item = store
        .work_start(&lead.token, &start("replay", "Replay", "lead"), now)
        .unwrap()
        .item;
    store
        .work_start(
            &helper.token,
            &join(&item.id, "helper", WorkRole::Helper),
            now + 1,
        )
        .unwrap();

    let lead_usage = usage(&item.id, "stable-observation", 1, "lead");
    assert!(
        !store
            .work_usage(&lead.token, &lead_usage, now + 2)
            .unwrap()
            .replayed
    );
    let helper_usage = usage(&item.id, "helper-observation", 1, "helper");
    store
        .work_usage(&helper.token, &helper_usage, now + 2)
        .unwrap();
    invalid(
        store.work_usage(
            &successor.token,
            &usage(&item.id, "outsider", 1, "successor"),
            now + 2,
        ),
        "usage author is not a current work participant",
    );

    apply_checkpoint(
        &*store,
        &lead.token,
        &checkpoint(&item.id, "handover", 1, "lead", CheckpointKind::Handover),
        now + 3,
    )
    .unwrap();
    let mut takeover = join(&item.id, "successor", WorkRole::Lead);
    takeover.takeover_generation = Some(1);
    let taken = store
        .work_start(&successor.token, &takeover, now + 4)
        .unwrap();
    assert_eq!(taken.item.generation, 2);
    assert!(
        store
            .work_usage(&lead.token, &lead_usage, now + 5)
            .unwrap()
            .replayed
    );
    invalid(
        store.work_usage(&successor.token, &lead_usage, now + 5),
        "observation_id is already used by different usage",
    );
    let mut changed = lead_usage.clone();
    changed.output_tokens = Some(12);
    invalid(
        store.work_usage(&lead.token, &changed, now + 5),
        "observation_id is already used by different usage",
    );
    invalid(
        store.work_usage(
            &helper.token,
            &usage(&item.id, "old-helper", 1, "helper"),
            now + 5,
        ),
        "usage author is not a current work participant",
    );

    apply_checkpoint(
        &*store,
        &successor.token,
        &checkpoint(&item.id, "finish", 2, "successor", CheckpointKind::Finish),
        now + 6,
    )
    .unwrap();
    store
        .work_usage(
            &successor.token,
            &usage(&item.id, "finished-lead", 2, "successor"),
            now + 7,
        )
        .unwrap();
    invalid(
        store.work_usage(
            &successor.token,
            &usage(&item.id, "wrong-session", 2, "other"),
            now + 7,
        ),
        "usage author is not a current work participant",
    );

    let reader = agent(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 4,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID,
            read_only: true,
        },
    );
    assert!(matches!(
        store.work_usage(
            &reader.token,
            &usage(&item.id, "reader", 2, "successor"),
            now + 8
        ),
        Err(HubError::Unauthorized)
    ));
    assert!(
        store
            .work_insight(&reader.token, &item.id, now + 8)
            .unwrap()
            .is_some()
    );
    assert!(store.revoke_agent_token(&helper.token).unwrap());
    assert!(matches!(
        store.work_insight(&helper.token, &item.id, now + 8),
        Err(HubError::Unauthorized)
    ));

    for (index, spec) in [
        AgentSpec {
            provider: "github",
            host: "other.example",
            user_id: 5,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID,
            read_only: false,
        },
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 6,
            repository: "acme/other",
            repository_id: REPOSITORY_ID,
            read_only: false,
        },
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 7,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID + 1,
            read_only: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let outside = agent(&*store, spec);
        assert!(
            store
                .work_insight(&outside.token, &item.id, now + 9)
                .unwrap()
                .is_none(),
            "scope {index}"
        );
    }
}

fn observation_and_group_limits(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 11);
    let now = unix_now();
    let old = now - 3 * 86_400;
    let item = store
        .work_start(&lead.token, &start("limits", "Limits", "lead"), old)
        .unwrap()
        .item;
    for index in 0..MAX_OBSERVATIONS {
        let mut report = usage(&item.id, &format!("observation-{index:03}"), 1, "lead");
        report.agent = format!("agent-{index:03}");
        report.provider = format!("provider-{index:03}");
        report.model = format!("model-{index:03}");
        report.usage_basis = UsageBasis::ProviderResult;
        report.input_tokens = Some(MAX_MEASUREMENT);
        report.output_tokens = Some(MAX_MEASUREMENT);
        report.cost = Some(ReportedCost {
            kind: CostKind::ReportedCharge,
            currency: "USD".into(),
            amount_micros: MAX_MEASUREMENT,
            basis: format!("basis-{index:03}"),
        });
        store
            .work_usage(&lead.token, &report, old + index as i64 + 1)
            .unwrap();
    }
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    store.work_compact(&scope, now - 86_400, true, now).unwrap();
    invalid(
        store.work_usage(&lead.token, &usage(&item.id, "over-limit", 1, "lead"), now),
        "work has reached the usage observation limit",
    );
    let value = store
        .value_metrics(&lead.token, &Default::default(), now)
        .unwrap();
    assert!(
        value
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("cap rejected"))
    );

    let detail = store
        .work_insight(&lead.token, &item.id, now)
        .unwrap()
        .unwrap();
    assert_eq!(detail.usage.observations, MAX_OBSERVATIONS);
    assert_eq!(detail.usage.token_groups.len(), MAX_OBSERVATIONS);
    assert_eq!(detail.usage.cost_groups.len(), MAX_OBSERVATIONS);
    assert!(detail.usage.groups_complete);
    assert_eq!(
        detail.usage.input_tokens,
        Some(MAX_MEASUREMENT * MAX_OBSERVATIONS as i64)
    );
    assert_eq!(
        detail.usage.output_tokens,
        Some(MAX_MEASUREMENT * MAX_OBSERVATIONS as i64)
    );
    let page = store
        .work_insights(
            &lead.token,
            &InsightQuery {
                since: Some(old),
                until: Some(now),
                after: None,
                through: None,
                limit: 20,
            },
            now,
        )
        .unwrap();
    let summary = &page.items[0].usage;
    assert_eq!(summary.observations, detail.usage.observations);
    assert_eq!(summary.input_tokens, detail.usage.input_tokens);
    assert_eq!(summary.output_tokens, detail.usage.output_tokens);
    assert_eq!(summary.token_groups.len(), 20);
    assert_eq!(summary.cost_groups.len(), 20);
    assert!(!summary.groups_complete);
}

fn zero_usage_is_explicitly_unknown(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 12);
    let now = unix_now();
    let item = store
        .work_start(&lead.token, &start("zero-usage", "Zero usage", "lead"), now)
        .unwrap()
        .item;
    let insight = store
        .work_insight(&lead.token, &item.id, now)
        .unwrap()
        .unwrap();
    assert_eq!(insight.usage.observations, 0);
    assert_eq!(insight.usage.coverage, "unknown");
    assert_eq!(insight.usage.input_tokens, None);
    assert_eq!(insight.usage.output_tokens, None);
    assert_eq!(insight.usage.input_known_observations, 0);
    assert_eq!(insight.usage.output_known_observations, 0);
    assert_eq!(insight.usage.missing_cost_observations, 0);
    assert_eq!(insight.usage.reported_failed_attempts, 0);
    assert!(insight.usage.token_groups.is_empty());
    assert!(insight.usage.cost_groups.is_empty());
    assert!(insight.usage.groups_complete);
}

fn keyset_windows_and_cursor_bindings(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 21);
    let now = unix_now();
    let first = store
        .work_start(&lead.token, &start("keyset-a", "A", "a"), now)
        .unwrap()
        .item;
    let second = store
        .work_start(&lead.token, &start("keyset-b", "B", "b"), now)
        .unwrap()
        .item;
    db.mark_legacy_created_seq(&first.id);
    let page1 = store
        .work_insights(
            &lead.token,
            &InsightQuery {
                since: Some(now),
                until: Some(now),
                after: None,
                through: None,
                limit: 1,
            },
            now,
        )
        .unwrap();
    assert_eq!(page1.items.len(), 1);
    assert!(!page1.complete);
    let cursor = page1.next_after.clone().unwrap();
    let later = store
        .work_start(&lead.token, &start("keyset-later", "Later", "later"), now)
        .unwrap()
        .item;
    let page2 = store
        .work_insights(
            &lead.token,
            &InsightQuery {
                since: Some(now),
                until: Some(now),
                after: Some(cursor.clone()),
                through: Some(page1.through),
                limit: 1,
            },
            now,
        )
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.complete);
    let mut ids = vec![
        page1.items[0].work_id.clone(),
        page2.items[0].work_id.clone(),
    ];
    ids.sort();
    let mut expected = vec![first.id.clone(), second.id.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(page2.items.iter().all(|item| item.work_id != later.id));
    let fresh = store
        .work_insights(
            &lead.token,
            &InsightQuery {
                since: Some(now),
                until: Some(now),
                after: None,
                through: None,
                limit: 20,
            },
            now,
        )
        .unwrap();
    assert!(fresh.items.iter().any(|item| item.work_id == later.id));
    let legacy = store
        .work_insight(&lead.token, &first.id, now)
        .unwrap()
        .unwrap();
    assert!(legacy.history_retention_gap);
    assert!(!legacy.history_complete);
    assert!(matches!(
        store.work_insights(
            &lead.token,
            &InsightQuery {
                since: Some(now - 1),
                until: Some(now),
                after: Some(cursor),
                through: Some(page1.through),
                limit: 1
            },
            now
        ),
        Err(HubError::Invalid(_))
    ));
}

fn compaction_retains_usage_and_marks_history_incomplete(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 31);
    let now = unix_now();
    let old = now - 3 * 86_400;
    let item = store
        .work_start(&lead.token, &start("compact", "Compact", "lead"), old)
        .unwrap()
        .item;
    let report = usage(&item.id, "retained", 1, "lead");
    store.work_usage(&lead.token, &report, old + 1).unwrap();
    apply_checkpoint(
        &*store,
        &lead.token,
        &checkpoint(
            &item.id,
            "old-progress",
            1,
            "lead",
            CheckpointKind::Progress,
        ),
        old + 2,
    )
    .unwrap();
    let scope = WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    };
    let compacted = store.work_compact(&scope, now - 86_400, true, now).unwrap();
    assert!(compacted.deleted_versions > 0);
    assert!(
        store
            .work_usage(&lead.token, &report, now)
            .unwrap()
            .replayed
    );
    let insight = store
        .work_insight(&lead.token, &item.id, now)
        .unwrap()
        .unwrap();
    assert_eq!(insight.usage.observations, 1);
    assert!(!insight.history_complete);
    assert!(insight.history_retention_gap);
}

fn publication_provenance_is_exact_scope_and_checkpoint_first(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 41);
    let now = unix_now();
    let item = store
        .work_start(
            &lead.token,
            &start("publications", "Publications", "lead"),
            now,
        )
        .unwrap()
        .item;
    let checkpoint_commit = "1".repeat(40);
    let valid_publication = publication(
        REPOSITORY,
        &lead.token,
        &*store,
        1,
        &checkpoint_commit,
        "feat/valid",
        now + 1,
    );
    let mut progress = checkpoint(
        &item.id,
        "publication-checkpoint",
        1,
        "lead",
        CheckpointKind::Progress,
    );
    progress.source_refs = vec![SourceRef::Commit {
        oid: checkpoint_commit.clone(),
        path: None,
    }];
    apply_checkpoint(&*store, &lead.token, &progress, now + 2).unwrap();

    let disconnected = store
        .work_insight(&lead.token, &item.id, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(disconnected.publications.len(), 1);
    assert_eq!(
        disconnected.publications[0].publication_id,
        valid_publication
    );
    assert_eq!(
        disconnected.publications[0].association,
        "claimed_by_checkpoint"
    );
    assert!(disconnected.publications[0].checks.is_empty());
    assert!(disconnected.publications_complete);
    store
        .update_publication(&valid_publication, &Update::BranchCreated, now + 3)
        .unwrap();
    store
        .update_publication(&valid_publication, &Update::PullRequestAttempt, now + 3)
        .unwrap();
    store
        .update_publication(
            &valid_publication,
            &Update::PullRequest {
                number: 5,
                node_id: "synthetic-pr-node".into(),
                html_url: "https://github.com/acme/widgets/pull/5".into(),
            },
            now + 3,
        )
        .unwrap();
    let missing_check_source = store
        .work_insight(&lead.token, &item.id, now + 3)
        .unwrap()
        .unwrap();
    assert_eq!(missing_check_source.publications.len(), 1);
    assert!(missing_check_source.publications[0].checks.is_empty());
    assert!(!missing_check_source.publications_complete);
    let legacy_owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 44,
            },
            "legacy",
        )
        .unwrap();
    let (legacy_token, _) =
        mint_agent_token(&*store, &legacy_owner.id, "legacy-agent", REPOSITORY, TTL).unwrap();
    let legacy_work = store
        .work_start(
            &legacy_token,
            &start("legacy-scope", "Legacy", "legacy"),
            now,
        )
        .unwrap()
        .item;
    publication(
        REPOSITORY,
        &legacy_token,
        &*store,
        4,
        &checkpoint_commit,
        "feat/legacy",
        now + 1,
    );
    let legacy = store
        .work_insight(&legacy_token, &legacy_work.id, now + 3)
        .unwrap()
        .unwrap();
    assert!(legacy.publications.is_empty());
    assert!(!legacy.publications_complete);
    let connection = store
        .insert_connection(
            &NewConnection {
                id: format!("con_{}", "a".repeat(32)),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("synthetic-bot".into()),
            },
            "synthetic-test",
        )
        .unwrap();
    let connected = store
        .upsert_repository(
            &connection.id,
            "github.com",
            REPOSITORY_ID,
            REPOSITORY,
            None,
            None,
        )
        .unwrap();
    let check = NewForgeCheck {
        head_sha: checkpoint_commit.clone(),
        state: "success".into(),
        binding_digest: Some("f".repeat(64)),
        description: "synthetic check".into(),
    };
    assert_eq!(
        store
            .reserve_forge_check(
                &connected.id,
                5,
                "agentscm/synthetic",
                None,
                &check,
                now + 3
            )
            .unwrap(),
        ForgeCheckReserve::Reserved(1)
    );
    assert!(
        store
            .complete_forge_check(&connected.id, 5, "agentscm/synthetic", 1, "posted", now + 3)
            .unwrap()
    );
    let different_head = NewForgeCheck {
        head_sha: "9".repeat(40),
        state: "failure".into(),
        binding_digest: None,
        description: "different head".into(),
    };
    assert_eq!(
        store
            .reserve_forge_check(
                &connected.id,
                5,
                "agentscm/different-head",
                None,
                &different_head,
                now + 3
            )
            .unwrap(),
        ForgeCheckReserve::Reserved(1)
    );
    assert!(
        store
            .complete_forge_check(
                &connected.id,
                5,
                "agentscm/different-head",
                1,
                "posted",
                now + 3
            )
            .unwrap()
    );

    let wrong_host = agent(
        &*store,
        AgentSpec {
            provider: "github",
            host: "other.example",
            user_id: 42,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID,
            read_only: false,
        },
    );
    publication(
        REPOSITORY,
        &wrong_host.token,
        &*store,
        2,
        &checkpoint_commit,
        "feat/wrong-host",
        now + 2,
    );
    let wrong_id = agent(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 43,
            repository: REPOSITORY,
            repository_id: REPOSITORY_ID + 1,
            read_only: false,
        },
    );
    publication(
        REPOSITORY,
        &wrong_id.token,
        &*store,
        3,
        &checkpoint_commit,
        "feat/wrong-id",
        now + 2,
    );

    for index in 0..16_i64 {
        let commit = format!("{:040x}", index + 16);
        let mut report = usage(
            &item.id,
            &format!("publication-usage-{index:02}"),
            1,
            "lead",
        );
        report.source_commit = Some(commit.clone());
        store
            .work_usage(&lead.token, &report, now + 4 + index)
            .unwrap();
        publication(
            REPOSITORY,
            &lead.token,
            &*store,
            10 + index,
            &commit,
            &format!("feat/usage-{index:02}"),
            now + 4 + index,
        );
    }
    let insight = store
        .work_insight(&lead.token, &item.id, now + 100)
        .unwrap()
        .unwrap();
    assert_eq!(insight.publications.len(), 16);
    assert!(!insight.publications_complete);
    assert!(insight.commits_truncated);
    assert_eq!(insight.publications[0].publication_id, valid_publication);
    assert_eq!(insight.publications[0].commit, checkpoint_commit);
    assert_eq!(insight.publications[0].association, "claimed_by_checkpoint");
    assert_eq!(insight.publications[0].checks.len(), 1);
    assert_eq!(
        insight.publications[0].checks[0].head_sha,
        checkpoint_commit
    );
    assert_eq!(insight.publications[0].checks[0].state, "success");
    assert_eq!(insight.publications[0].checks[0].phase, "posted");
    assert_eq!(
        insight
            .publications
            .iter()
            .filter(|evidence| evidence.commit == checkpoint_commit)
            .count(),
        1,
        "wrong-host and wrong-repository-id publications must not associate"
    );

    let overflow_lead = ordinary_agent(&*store, 45);
    let overflow = store
        .work_start(
            &overflow_lead.token,
            &start("checkpoint-overflow", "Checkpoint overflow", "overflow"),
            now + 200,
        )
        .unwrap()
        .item;
    let distinct = (0..17_i64)
        .map(|index| format!("{:040x}", 1_000 + index))
        .collect::<Vec<_>>();
    let mut overflow_checkpoint = checkpoint(
        &overflow.id,
        "seventeen-distinct",
        1,
        "overflow",
        CheckpointKind::Progress,
    );
    overflow_checkpoint.source_refs = distinct
        .iter()
        .map(|oid| SourceRef::Commit {
            oid: oid.clone(),
            path: None,
        })
        .collect();
    apply_checkpoint(
        &*store,
        &overflow_lead.token,
        &overflow_checkpoint,
        now + 201,
    )
    .unwrap();
    for (index, commit) in distinct.iter().enumerate() {
        publication(
            REPOSITORY,
            &overflow_lead.token,
            &*store,
            100 + index as i64,
            commit,
            &format!("feat/distinct-{index:02}"),
            now + 202 + index as i64,
        );
    }
    let overflow_insight = store
        .work_insight(&overflow_lead.token, &overflow.id, now + 300)
        .unwrap()
        .unwrap();
    assert!(overflow_insight.commits_truncated);
    assert!(!overflow_insight.publications_complete);
    assert_eq!(overflow_insight.publications.len(), 16);
    assert_eq!(
        overflow_insight
            .publications
            .iter()
            .map(|evidence| evidence.commit.clone())
            .collect::<Vec<_>>(),
        distinct[..16]
    );

    let duplicate_lead = ordinary_agent(&*store, 46);
    let duplicate = store
        .work_start(
            &duplicate_lead.token,
            &start("checkpoint-duplicate", "Checkpoint duplicate", "duplicate"),
            now + 400,
        )
        .unwrap()
        .item;
    let repeated = "8".repeat(40);
    let mut duplicate_checkpoint = checkpoint(
        &duplicate.id,
        "seventeen-repeats",
        1,
        "duplicate",
        CheckpointKind::Progress,
    );
    duplicate_checkpoint.source_refs = (0..17)
        .map(|_| SourceRef::Commit {
            oid: repeated.clone(),
            path: None,
        })
        .collect();
    apply_checkpoint(
        &*store,
        &duplicate_lead.token,
        &duplicate_checkpoint,
        now + 401,
    )
    .unwrap();
    let repeated_publication = publication(
        REPOSITORY,
        &duplicate_lead.token,
        &*store,
        200,
        &repeated,
        "feat/repeated",
        now + 402,
    );
    let duplicate_insight = store
        .work_insight(&duplicate_lead.token, &duplicate.id, now + 403)
        .unwrap()
        .unwrap();
    assert!(!duplicate_insight.commits_truncated);
    assert!(duplicate_insight.publications_complete);
    assert_eq!(duplicate_insight.publications.len(), 1);
    assert_eq!(
        duplicate_insight.publications[0].publication_id,
        repeated_publication
    );
}

fn value_projection_and_receipt_conformance(db: &Db) {
    use agentscm::hub::insights::UsageMeasurementScope;
    use agentscm::hub::value_metrics::{ValueMetricsQuery, ValueObservation};
    let store = db.open();
    let lead = ordinary_agent(&*store, 501);
    let other = ordinary_agent(&*store, 502);
    let now = unix_now();
    let a = store
        .work_start(&lead.token, &start("value-a", "Value A", "session-a"), now)
        .unwrap()
        .item;
    let b = store
        .work_start(&other.token, &start("value-b", "Value B", "session-b"), now)
        .unwrap()
        .item;
    let mut u = usage(&a.id, "first", 1, "session-a");
    u.provider_request_id = Some("account/request".into());
    u.measurement_scope = Some(UsageMeasurementScope::Request);
    u.input_tokens = Some(MAX_MEASUREMENT);
    store.work_usage(&lead.token, &u, now).unwrap();
    u.work_id = b.id.clone();
    u.observation_id = "second".into();
    u.session = "session-b".into();
    store.work_usage(&other.token, &u, now).unwrap();
    u.work_id = a.id.clone();
    u.observation_id = "a-second-report".into();
    u.session = "session-a".into();
    store.work_usage(&lead.token, &u, now).unwrap();
    let p = store
        .value_metrics(&lead.token, &ValueMetricsQuery::default(), now)
        .unwrap();
    assert_eq!(p.resources.input_tokens.as_deref(), Some("1000000000000"));
    assert_eq!(p.resources.unallocated_usage_records, 0);
    let one = store
        .value_metrics(
            &lead.token,
            &ValueMetricsQuery {
                work_id: Some(a.id.clone()),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(one.resources.input_tokens, None);
    assert_eq!(one.resources.unallocated_usage_records, 2);
    let observation = ValueObservation {
        work_id: Some(a.id.clone()),
        generation: Some(1),
        identity: "exact-episode".into(),
        kind: "issue".into(),
        state: "unresolved".into(),
        branch: "branch".into(),
        source: "a".repeat(40),
        recipe: "recipe".into(),
        policy: "policy".into(),
        gate: "review".into(),
        evidence_id: None,
        reason: "Review independence refusal".into(),
    };
    store.value_observe(&lead.token, &observation, now).unwrap();
    store.value_observe(&lead.token, &observation, now).unwrap();
    let mut resolved = observation.clone();
    resolved.state = "resolved".into();
    resolved.evidence_id = Some("evidence".into());
    store.value_observe(&other.token, &resolved, now).unwrap();
    let p = store
        .value_metrics(&lead.token, &ValueMetricsQuery::default(), now)
        .unwrap();
    assert_eq!(p.issues.resolved, 1);
    assert_eq!(p.issues.unresolved, 0);
    store
        .receipt_work_remember(&lead.token, "session-a", &a.id, now)
        .unwrap();
    assert_eq!(
        store
            .receipt_work_ids(&lead.token, "session-a", now)
            .unwrap(),
        vec![a.id.clone()]
    );
    assert!(
        store
            .receipt_work_ids(&other.token, "session-a", now)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .receipt_announce(&lead.token, "session-a", &a.id, 1, "version", now)
            .unwrap()
    );
    assert!(
        !store
            .receipt_announce(&lead.token, "session-a", &a.id, 1, "version", now)
            .unwrap()
    );
    assert!(
        store
            .receipt_announce(&other.token, "session-a", &a.id, 1, "version", now)
            .is_err()
    );
    assert!(
        store
            .receipt_announce(&lead.token, "session-a", &a.id, 2, "version", now)
            .is_err()
    );
    for n in 1..64 {
        assert!(
            store
                .receipt_announce(
                    &lead.token,
                    "session-a",
                    &a.id,
                    1,
                    &format!("version-{n}"),
                    now
                )
                .unwrap()
        );
    }
    assert!(
        !store
            .receipt_announce(&lead.token, "session-a", &a.id, 1, "version", now)
            .unwrap()
    );
    assert!(matches!(
        store.receipt_announce(&lead.token, "session-a", &a.id, 1, "new-after-cap", now),
        Err(HubError::Unavailable(_))
    ));
    store.revoke_agent_token(&lead.token).unwrap();
    assert!(matches!(
        store.value_metrics(&lead.token, &ValueMetricsQuery::default(), now),
        Err(HubError::Unauthorized)
    ));
    assert!(
        store
            .receipt_work_ids(&lead.token, "session-a", now)
            .is_err()
    );
}

fn value_receipt_preference_limits_expiry_and_deletion(db: &Db) {
    let store = db.open();
    let lead = ordinary_agent(&*store, 601);
    let other = ordinary_agent(&*store, 602);
    let now = unix_now();
    assert_eq!(
        store.receipt_mode_get(&lead.token, "session", now).unwrap(),
        None
    );
    store
        .receipt_mode_set(&lead.token, "expired", "automatic", now - 31 * 86400)
        .unwrap();
    assert_eq!(
        store.receipt_mode_get(&lead.token, "expired", now).unwrap(),
        None
    );
    for n in 0..32 {
        store
            .receipt_mode_set(&lead.token, &format!("session-{n}"), "disabled", now)
            .unwrap();
    }
    assert!(matches!(
        store.receipt_mode_set(&lead.token, "over-cap", "automatic", now),
        Err(HubError::Unavailable(_))
    ));
    store
        .receipt_mode_set(&lead.token, "session-0", "requested_only", now)
        .unwrap();
    assert_eq!(
        store
            .receipt_mode_get(&lead.token, "session-0", now)
            .unwrap()
            .as_deref(),
        Some("requested_only")
    );
    assert_eq!(
        store
            .receipt_mode_get(&other.token, "session-0", now)
            .unwrap(),
        None
    );
    assert!(
        store
            .receipt_mode_set(&lead.token, &"x".repeat(256), "automatic", now)
            .is_err()
    );
    assert!(
        store
            .receipt_mode_get(&lead.token, &"x".repeat(256), now)
            .is_err()
    );
    store
        .receipt_mode_set(&other.token, "delete-linked", "disabled", now)
        .unwrap();
    let remaining: i64 = if db._pg.is_some() {
        let mut client = postgres::Client::connect(&db.url, postgres::NoTls).unwrap();
        client
            .execute("DELETE FROM hub_agent_tokens WHERE id=$1", &[&other.token])
            .unwrap();
        client
            .query_one(
                "SELECT COUNT(*)::bigint FROM hub_value_receipt_preferences WHERE token=$1",
                &[&other.token],
            )
            .unwrap()
            .get(0)
    } else {
        let connection =
            rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap()).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute("DELETE FROM hub_agent_tokens WHERE id=?1", [&other.token])
            .unwrap();
        connection
            .query_row(
                "SELECT COUNT(*) FROM hub_value_receipt_preferences WHERE token=?1",
                [&other.token],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(remaining, 0);
}

fn value_usage_enrichment_and_delivery_retention(db: &Db) {
    use agentscm::hub::catchup::{CatchupAck, CatchupQuery};
    use agentscm::hub::insights::UsageMeasurementScope;
    use agentscm::hub::value_metrics::ValueMetricsQuery;
    let store = db.open();
    let lead = ordinary_agent(&*store, 603);
    let now = unix_now();
    for reverse in [false, true] {
        let key = if reverse {
            "enrich-reverse"
        } else {
            "enrich-forward"
        };
        let work = store
            .work_start(&lead.token, &start(key, key, key), now)
            .unwrap()
            .item;
        let mut first = usage(&work.id, "missing-cost", 1, key);
        first.measurement_scope = Some(UsageMeasurementScope::Request);
        first.provider_request_id = Some(format!("request-{key}"));
        let mut enriched = first.clone();
        enriched.observation_id = "known-cost-cache".into();
        enriched.cache_read_input_tokens = Some(5);
        enriched.cost = Some(ReportedCost {
            kind: CostKind::ReportedCharge,
            currency: "USD".into(),
            amount_micros: 2000,
            basis: "provider_statement".into(),
        });
        let mut estimate = enriched.clone();
        estimate.observation_id = "reference-price".into();
        estimate.cost.as_mut().unwrap().kind = CostKind::ListPriceEstimate;
        estimate.cost.as_mut().unwrap().amount_micros = 3000;
        estimate.cost.as_mut().unwrap().basis = "reference-v1".into();
        let reports = if reverse {
            vec![estimate, enriched, first]
        } else {
            vec![first, enriched, estimate]
        };
        for report in reports {
            store.work_usage(&lead.token, &report, now).unwrap();
        }
        let q = ValueMetricsQuery {
            work_id: Some(work.id),
            ..Default::default()
        };
        let p = store.value_metrics(&lead.token, &q, now).unwrap();
        assert_eq!(p.resources.input_tokens.as_deref(), Some("7"));
        assert_eq!(p.resources.costs.len(), 2);
        assert_eq!(
            p.resources.groups[0].cache_read_input_tokens.as_deref(),
            Some("5")
        );
        assert_eq!(p.resources.missing_cost_requests, 0);
    }
    // Use a retained older delivery, then exercise the existing cleanup path. The fixed
    // metrics window must disclose loss of delivery observations rather than a healthy zero.
    let old = now - 2 * 86400;
    let work = store
        .work_start(
            &lead.token,
            &start("old-context", "Old context", "old-context"),
            old,
        )
        .unwrap()
        .item;
    let query = CatchupQuery {
        subscriber: "agent:old-context".into(),
        delivery_client: "mcp".into(),
        ..Default::default()
    };
    let delivery = store.work_catchup(&lead.token, &query, old).unwrap();
    assert!(delivery.work_events.iter().any(|e| e.work_id == work.id));
    store
        .work_catchup_ack(
            &lead.token,
            &CatchupAck {
                delivery: delivery.delivery.clone(),
                expected_event_from: delivery.event_from,
                expected_merge_from: delivery.merge_from,
            },
            old,
        )
        .unwrap();
    let q = ValueMetricsQuery {
        since: Some(now - 3 * 86400),
        until: Some(now),
        work_id: Some(work.id),
        ..Default::default()
    };
    let before = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(before.context.returned > 0);
    assert!(before.context.acknowledged > 0);
    // A fresh read uses the acknowledged cursors. Explicit cursor fields belong only to
    // continuations of an unexpired delivery and cannot restart an expired delivery.
    store.work_catchup(&lead.token, &query, now).unwrap();
    let after = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(after.context.returned, 0);
    assert!(!after.coverage.complete);
    assert!(after.coverage.gaps.iter().any(|g| g.contains("24 hours")));
}

// Populate retained synthetic history in one transaction. These rows qualify only the
// bounded read projection, not workflow execution or authority.
fn seed_value_history(
    db: &Db,
    token: &str,
    template: &agentscm::hub::work::WorkItem,
    now: i64,
    with_usage: bool,
) -> String {
    use agentscm::hub::insights::UsageMeasurementScope;
    use agentscm::hub::work::{WorkSource, WorkState};
    let target = "wrk_fffffffffffffffffffffffffffffffe".to_owned();
    let mut records = Vec::new();
    for n in 0..=agentscm::hub::value_metrics::MAX_FACTS + 2 {
        let mut item = template.clone();
        item.id = if n == agentscm::hub::value_metrics::MAX_FACTS + 1 {
            target.clone()
        } else if n == agentscm::hub::value_metrics::MAX_FACTS + 2 {
            "wrk_ffffffffffffffffffffffffffffffff".into()
        } else {
            format!("wrk_{n:032x}")
        };
        let key = format!("retained-history-{n}");
        item.source = WorkSource::Native { request_key: key };
        item.state = WorkState::Finished;
        item.created_at = now - 400 * 86400;
        item.updated_at = item.created_at;
        let mut report = usage(&item.id, "retained-report", 1, "history");
        report.measurement_scope = Some(UsageMeasurementScope::Request);
        report.provider_request_id = Some(if n > agentscm::hub::value_metrics::MAX_FACTS {
            "shared-after-cut".into()
        } else {
            format!("retained-request-{n}")
        });
        records.push((item, report));
    }
    let columns = "id,provider,host,repository,repository_id,owner,lead_token,lead_agent,lead_session,generation,request_key,title,state,scope,branch,current_checkpoint,snapshot,created_at,updated_at,created_seq,updated_seq";
    if db._pg.is_some() {
        let mut client = postgres::Client::connect(&db.url, postgres::NoTls).unwrap();
        let mut tx = client.transaction().unwrap();
        let work_sql = format!(
            "INSERT INTO hub_work_items ({columns}) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)"
        );
        for (item, report) in records {
            let key = match &item.source {
                WorkSource::Native { request_key } => request_key,
                _ => unreachable!(),
            };
            let scope = serde_json::to_string(&item.scope).unwrap();
            let snapshot = serde_json::to_string(&item).unwrap();
            let current = item
                .current
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap());
            tx.execute(
                &work_sql,
                &[
                    &item.id,
                    &"github",
                    &"github.com",
                    &REPOSITORY,
                    &REPOSITORY_ID,
                    &item.owner,
                    &token,
                    &item.lead_agent,
                    &item.lead_session,
                    &item.generation,
                    &key,
                    &item.title,
                    &"finished",
                    &scope,
                    &item.branch,
                    &current,
                    &snapshot,
                    &item.created_at,
                    &item.updated_at,
                    &item.created_seq,
                    &item.updated_seq,
                ],
            )
            .unwrap();
            if !with_usage {
                continue;
            }
            let at = if item.id == target {
                now
            } else {
                now - 400 * 86400
            };
            let payload = serde_json::to_string(&report).unwrap();
            tx.execute("INSERT INTO hub_work_model_usage(work,observation_id,author_token,request,generation,session,phase,outcome,agent,model_provider,model,usage_basis,input_tokens,output_tokens,received_at) VALUES($1,$2,$3,$4,1,$5,'implementation','succeeded',$6,$7,$8,'reported',$9,$10,$11)",&[&item.id,&report.observation_id,&token,&payload,&report.session,&report.agent,&report.provider,&report.model,&report.input_tokens,&report.output_tokens,&at]).unwrap();
        }
        tx.commit().unwrap();
    } else {
        let mut connection =
            rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap()).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        let tx = connection.transaction().unwrap();
        let work_sql = format!(
            "INSERT INTO hub_work_items ({columns}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)"
        );
        for (item, report) in records {
            let key = match &item.source {
                WorkSource::Native { request_key } => request_key,
                _ => unreachable!(),
            };
            let scope = serde_json::to_string(&item.scope).unwrap();
            let snapshot = serde_json::to_string(&item).unwrap();
            let current = item
                .current
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap());
            tx.execute(
                &work_sql,
                rusqlite::params![
                    item.id,
                    "github",
                    "github.com",
                    REPOSITORY,
                    REPOSITORY_ID,
                    item.owner,
                    token,
                    item.lead_agent,
                    item.lead_session,
                    item.generation,
                    key,
                    item.title,
                    "finished",
                    scope,
                    item.branch,
                    current,
                    snapshot,
                    item.created_at,
                    item.updated_at,
                    item.created_seq,
                    item.updated_seq
                ],
            )
            .unwrap();
            if !with_usage {
                continue;
            }
            let at = if item.id == target {
                now
            } else {
                now - 400 * 86400
            };
            let payload = serde_json::to_string(&report).unwrap();
            tx.execute("INSERT INTO hub_work_model_usage(work,observation_id,author_token,request,generation,session,phase,outcome,agent,model_provider,model,usage_basis,input_tokens,output_tokens,received_at) VALUES(?1,?2,?3,?4,1,?5,'implementation','succeeded',?6,?7,?8,'reported',?9,?10,?11)",rusqlite::params![item.id,report.observation_id,token,payload,report.session,report.agent,report.provider,report.model,report.input_tokens,report.output_tokens,at]).unwrap();
        }
        tx.commit().unwrap();
    }
    target
}

fn value_history_membership_is_bounded_and_work_target_explicit(db: &Db) {
    use agentscm::hub::catchup::{CatchupMergeSource, CatchupRepositoryScope, NewCatchupMerge};
    use agentscm::hub::insights::UsageMeasurementScope;
    use agentscm::hub::value_metrics::{ValueMetricsQuery, ValueObservation};
    let store = db.open();
    let lead = ordinary_agent(&*store, 701);
    let now = unix_now();
    let old = now - 10 * 86400;
    let mut template = store
        .work_start(
            &lead.token,
            &start("membership-template", "Membership template", "template"),
            now,
        )
        .unwrap()
        .item;
    let before = store
        .work_start(
            &lead.token,
            &start("membership-before", "Before window", "before"),
            old,
        )
        .unwrap()
        .item;
    let mut own = usage(&template.id, "own", 1, "template");
    own.measurement_scope = Some(UsageMeasurementScope::Request);
    own.provider_request_id = Some("shared-before-since".into());
    store.work_usage(&lead.token, &own, now).unwrap();
    let mut historical = usage(&before.id, "historical", 1, "before");
    historical.measurement_scope = Some(UsageMeasurementScope::Request);
    historical.provider_request_id = own.provider_request_id.clone();
    store.work_usage(&lead.token, &historical, old).unwrap();
    let query = ValueMetricsQuery {
        since: Some(now - 3600),
        until: Some(now),
        work_id: Some(template.id.clone()),
        ..Default::default()
    };
    let shared = store.value_metrics(&lead.token, &query, now).unwrap();
    assert_eq!(shared.resources.input_tokens, None);
    assert_eq!(shared.resources.unallocated_usage_records, 1);
    assert!(
        shared
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("Shared provider"))
    );
    let mut checkpoint = checkpoint(
        &template.id,
        "source",
        1,
        "template",
        CheckpointKind::Progress,
    );
    checkpoint.source_refs = vec![SourceRef::Commit {
        oid: "a".repeat(40),
        path: None,
    }];
    template = apply_checkpoint(&*store, &lead.token, &checkpoint, now)
        .unwrap()
        .item;
    let target = seed_value_history(db, &lead.token, &template, now, true);
    let mut exclusive = usage(&target, "exclusive-current", 1, "template");
    exclusive.measurement_scope = Some(UsageMeasurementScope::Request);
    exclusive.provider_request_id = Some("exclusive-after-unrelated-history".into());
    exclusive.cost = Some(ReportedCost {
        kind: CostKind::ReportedCharge,
        currency: "USD".into(),
        amount_micros: 2000,
        basis: "reported".into(),
    });
    store.work_usage(&lead.token, &exclusive, now).unwrap();
    let issue = ValueObservation {
        work_id: Some(target.clone()),
        generation: Some(1),
        identity: "outside-old-cut".into(),
        kind: "issue".into(),
        state: "unresolved".into(),
        branch: "branch".into(),
        source: "a".repeat(40),
        recipe: "recipe".into(),
        policy: "policy".into(),
        gate: "review".into(),
        evidence_id: None,
        reason: "Synthetic retained unresolved episode".into(),
    };
    store.value_observe(&lead.token, &issue, old).unwrap();
    store
        .record_catchup_merge(
            &CatchupRepositoryScope {
                provider: "github".into(),
                host: "github.com".into(),
                repository: REPOSITORY.into(),
                repository_id: Some(REPOSITORY_ID),
            },
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "synthetic-merge".into(),
                occurred_at: Some(now),
                title: Some("Synthetic merge".into()),
                branch: "feat/synthetic".into(),
                base: "main".into(),
                head_commit: "a".repeat(40),
                merge_commit: "b".repeat(40),
                pull_number: 42,
                paths: vec![],
                paths_complete: true,
            },
            now,
        )
        .unwrap();
    let q = ValueMetricsQuery {
        work_id: Some(target.clone()),
        ..query
    };
    let allocated = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(allocated.resources.input_tokens.as_deref(), Some("7"));
    assert_eq!(allocated.resources.costs[0].amount_micros, "2000");
    assert_eq!(allocated.resources.unallocated_usage_records, 1);
    assert!(
        !allocated
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("membership across retained repository history is incomplete"))
    );
    // Scope the inner target-identity selection as well as the outer history. An internal
    // browser principal for a different authorized repository must not read target identities.
    let other = agent(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 707,
            repository: "other/membership",
            repository_id: 9002,
            read_only: false,
        },
    );
    let other_work = store
        .work_start(
            &other.token,
            &start("other-membership", "Other membership", "other-membership"),
            now,
        )
        .unwrap()
        .item;
    let mut foreign = usage(&other_work.id, "same-opaque-request", 1, "other-membership");
    foreign.measurement_scope = Some(UsageMeasurementScope::Request);
    foreign.provider_request_id = exclusive.provider_request_id.clone();
    store.work_usage(&other.token, &foreign, now).unwrap();
    let principal = agentscm::hub::store::Principal {
        token_id: String::new(),
        name: String::new(),
        repository: "other/membership".into(),
        repository_id: Some(9002),
        expires_at: now + 1,
        read_only: true,
        owner: store.user_by_id(&other_work.owner).unwrap().unwrap(),
    };
    let empty_scope = store.repository_value_facts(&principal, &q, now).unwrap();
    assert!(empty_scope.works.is_empty());
    assert!(empty_scope.usage.is_empty());
    assert!(empty_scope.request_membership.is_empty());
    assert!(store.value_metrics(&other.token, &q, now).is_err());
    // Now make the previously unrelated retained rows genuinely relevant. More than2000
    // matching rows must still fail closed rather than allocating from a partial sample.
    let last_filler = format!("wrk_{:032x}", agentscm::hub::value_metrics::MAX_FACTS);
    if db._pg.is_some() {
        let mut client = postgres::Client::connect(&db.url, postgres::NoTls).unwrap();
        client.execute("UPDATE hub_work_model_usage SET request=jsonb_set(request::jsonb,'{provider_request_id}',to_jsonb($1::text))::text WHERE work IN(SELECT id FROM hub_work_items WHERE request_key LIKE 'retained-history-%' AND id<=$2)",&[&"exclusive-after-unrelated-history",&last_filler]).unwrap();
    } else {
        let connection =
            rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap()).unwrap();
        connection.execute("UPDATE hub_work_model_usage SET request=json_set(request,'$.provider_request_id',?1) WHERE work IN(SELECT id FROM hub_work_items WHERE request_key LIKE 'retained-history-%' AND id<=?2)",rusqlite::params!["exclusive-after-unrelated-history",last_filler]).unwrap();
    }
    let capped = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(capped.resources.input_tokens, None);
    assert!(capped.resources.costs.is_empty());
    assert_eq!(capped.resources.unallocated_usage_records, 2);
    assert!(!capped.coverage.complete);
    assert!(
        capped
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("membership across retained repository history is incomplete"))
    );
    assert_eq!(capped.issues.unresolved, 1);
    assert_eq!(
        capped
            .evidence
            .iter()
            .find(|e| e.kind == "issue")
            .unwrap()
            .work_id
            .as_deref(),
        Some(target.as_str())
    );
    assert!(capped.linked_merge.is_some());
    let all = store
        .value_metrics(
            &lead.token,
            &ValueMetricsQuery {
                since: q.since,
                until: q.until,
                ..Default::default()
            },
            now,
        )
        .unwrap();
    assert_eq!(all.resources.input_tokens.as_deref(), Some("21"));
}

fn value_delivery_count_pruning_is_durable(db: &Db) {
    use agentscm::hub::catchup::{CatchupAck, CatchupQuery};
    use agentscm::hub::value_metrics::ValueMetricsQuery;
    let store = db.open();
    let lead = ordinary_agent(&*store, 702);
    let now = unix_now();
    let item = store
        .work_start(
            &lead.token,
            &start("prune-value", "Prune value", "prune"),
            now,
        )
        .unwrap()
        .item;
    let query = CatchupQuery {
        subscriber: "agent:prune".into(),
        delivery_client: "mcp".into(),
        ..Default::default()
    };
    for n in 1..=12 {
        apply_checkpoint(
            &*store,
            &lead.token,
            &checkpoint(
                &item.id,
                &format!("prune-{n}"),
                1,
                "prune",
                CheckpointKind::Progress,
            ),
            now + n,
        )
        .unwrap();
        let page = store.work_catchup(&lead.token, &query, now + n).unwrap();
        store
            .work_catchup_ack(
                &lead.token,
                &CatchupAck {
                    delivery: page.delivery,
                    expected_event_from: page.event_from,
                    expected_merge_from: page.merge_from,
                },
                now + n,
            )
            .unwrap();
    }
    let q = ValueMetricsQuery {
        since: Some(now - 1),
        until: Some(now + 15),
        work_id: Some(item.id),
        ..Default::default()
    };
    let reopened = db.open();
    let metrics = reopened.value_metrics(&lead.token, &q, now + 15).unwrap();
    assert!(metrics.context.acknowledged > 0);
    assert!(!metrics.coverage.complete);
    assert!(
        metrics
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("pruned by count"))
    );
    assert!(!metrics.coverage.gaps.iter().any(|g| g.contains("24 hours")));
}

fn value_capture_diagnostics_and_unsupported_outcomes(db: &Db) {
    // The fallback diagnostic is intentionally process-wide and persistent. Isolate this
    // control even under Cargo's shared-process/Windows test harness, without a reset API.
    let case = if db._pg.is_some() {
        "hub_insights_store::on_postgres::value_capture_diagnostics_and_unsupported_outcomes"
    } else {
        "hub_insights_store::on_sqlite::value_capture_diagnostics_and_unsupported_outcomes"
    };
    const CHILD: &str = "AGENTSCM_TEST_CAPTURE_DIAGNOSTIC_CHILD";
    if std::env::var(CHILD).as_deref() != Ok(case) {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", case, "--nocapture"])
            .env(CHILD, case)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "isolated capture diagnostic failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use agentscm::hub::store::Principal;
    use agentscm::hub::value_metrics::{self, ValueMetricsQuery, ValueObservation};
    let store = db.open();
    let lead = ordinary_agent(&*store, 703);
    let reader = ordinary_agent(&*store, 704);
    let unrelated = agent(
        &*store,
        AgentSpec {
            provider: "github",
            host: "github.com",
            user_id: 705,
            repository: "other/repository",
            repository_id: 9001,
            read_only: false,
        },
    );
    let now = unix_now();
    let item = store
        .work_start(
            &lead.token,
            &start("capture-gap", "Capture gap", "capture"),
            now,
        )
        .unwrap()
        .item;
    let invalid = ValueObservation {
        work_id: None,
        generation: None,
        identity: String::new(),
        kind: "issue".into(),
        state: "unresolved".into(),
        branch: "branch".into(),
        source: "source".into(),
        recipe: "recipe".into(),
        policy: "policy".into(),
        gate: "gate".into(),
        evidence_id: None,
        reason: "Synthetic capture failure".into(),
    };
    value_metrics::observe(&*store, &lead.token, &invalid, now);
    let token = store
        .value_metrics(&reader.token, &ValueMetricsQuery::default(), now)
        .unwrap();
    assert!(
        token
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("Observation capture failed"))
    );
    let owner = store.user_by_id(&item.owner).unwrap().unwrap();
    let principal = Principal {
        token_id: String::new(),
        name: String::new(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
        expires_at: now + 1,
        read_only: true,
        owner,
    };
    let facts = store
        .repository_value_facts(&principal, &ValueMetricsQuery::default(), now)
        .unwrap();
    assert!(facts.capture_failures);
    let browser = value_metrics::project(vec![facts], &Default::default(), now).unwrap();
    assert!(
        browser
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("Observation capture failed"))
    );
    assert!(
        browser
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("signed-in owner"))
    );
    assert!(
        token
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("calling repository token"))
    );
    let plain = store
        .value_metrics(&unrelated.token, &Default::default(), now)
        .unwrap();
    assert!(
        !plain
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("Observation capture failed"))
    );
    let wire = serde_json::to_value(browser).unwrap();
    for path in [
        "/issues/dismissed",
        "/issues/false_trigger",
        "/followups/failed",
        "/followups/cancelled",
    ] {
        assert!(wire.pointer(path).unwrap().is_null());
    }
    value_metrics::observe(&*store, "unknown-token", &invalid, now);
    let unknown = store
        .value_metrics(&unrelated.token, &Default::default(), now)
        .unwrap();
    assert!(
        unknown
            .coverage
            .gaps
            .iter()
            .any(|g| g.contains("Observation capture failed"))
    );
}

fn value_historical_work_metadata_does_not_expand_eligibility(db: &Db) {
    use agentscm::hub::value_metrics::ValueMetricsQuery;
    let store = db.open();
    let lead = ordinary_agent(&*store, 706);
    let now = unix_now();
    let work = store
        .work_start(
            &lead.token,
            &start(
                "historical-eligibility",
                "Historical eligibility",
                "historical",
            ),
            now,
        )
        .unwrap()
        .item;
    let q = ValueMetricsQuery {
        since: Some(now - 3600),
        until: Some(now - 1),
        work_id: Some(work.id),
        ..Default::default()
    };
    let metrics = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(metrics.coverage.complete);
    assert_eq!(metrics.coverage.eligible_works, Some(0));
    assert_eq!(metrics.coverage.observed_works, 0);
    let unknown = ValueMetricsQuery {
        work_id: Some("wrk_ffffffffffffffffffffffffffffffff".into()),
        ..q
    };
    assert!(matches!(
        store.value_metrics(&lead.token, &unknown, now),
        Err(HubError::Invalid(_))
    ));
}

fn value_aged_finished_followups_keep_actionable_and_period_cohorts_separate(db: &Db) {
    use agentscm::hub::catchup::CatchupQuery;
    use agentscm::hub::skills::SkillAdoption;
    use agentscm::hub::value_metrics::{self, ValueMetricsQuery, ValueObservation};
    use agentscm::skill_workflow::supported::{SkillLock, SkillPin, catalog_entry};
    let store = db.open();
    let lead = ordinary_agent(&*store, 708);
    let now = unix_now();
    let old = now - 3 * 3600;
    let entry = catalog_entry("pr-gate", 1).unwrap();
    let lock = SkillLock {
        version: 1,
        skills: vec![SkillPin::first_party(entry, None)],
    };
    store
        .skill_adoption_put(&SkillAdoption {
            repository: REPOSITORY.into(),
            source: "default_branch".into(),
            lock_json: serde_json::to_string(&lock).unwrap(),
            default_branch: Some("main".into()),
            observed_at: old,
            reported_by: None,
        })
        .unwrap();
    let policy = value_metrics::identity(&["null"]);
    let make = |key: &str, state: &str| {
        let item = store
            .work_start(&lead.token, &start(key, key, key), old)
            .unwrap()
            .item;
        let mut finish = checkpoint(
            &item.id,
            &format!("finish-{key}"),
            1,
            key,
            CheckpointKind::Finish,
        );
        finish.source_refs = vec![SourceRef::Commit {
            oid: "a".repeat(40),
            path: None,
        }];
        apply_checkpoint(&*store, &lead.token, &finish, old).unwrap();
        store
            .value_observe(
                &lead.token,
                &ValueObservation {
                    work_id: Some(item.id.clone()),
                    generation: Some(1),
                    identity: format!("retained-{key}"),
                    kind: "followup".into(),
                    state: state.into(),
                    branch: "branch".into(),
                    source: if key == "aged-stale" { "b" } else { "a" }.repeat(40),
                    recipe: entry.sha256.clone(),
                    policy: policy.clone(),
                    gate: "tests".into(),
                    evidence_id: Some(format!("evidence-{key}")),
                    reason: "Synthetic retained configured gate".into(),
                },
                old,
            )
            .unwrap();
        item
    };
    let historical = make("aged-current", "current");
    // More than the Work cap in unrelated old finished history must not hide
    // current metadata needed to classify retained followups or fresh context.
    seed_value_history(db, &lead.token, &historical, now, false);
    let q = ValueMetricsQuery {
        since: Some(now - 3600),
        until: Some(now),
        ..Default::default()
    };
    let satisfied = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(satisfied.followups.unknown, 0);
    assert_eq!(satisfied.followups.current, 0);
    assert_eq!(satisfied.coverage.observed_works, 0);
    assert_eq!(satisfied.coverage.eligible_works, Some(0));
    assert!(satisfied.coverage.complete);
    let pending = make("aged-pending", "pending");
    let retained = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(retained.followups.pending, 1);
    assert_eq!(retained.followups.unknown, 0);
    assert_eq!(retained.coverage.observed_works, 0);
    assert_eq!(retained.coverage.eligible_works, Some(0));
    assert!(retained.coverage.complete);
    let detail = store
        .value_metrics(
            &lead.token,
            &ValueMetricsQuery {
                work_id: Some(pending.id),
                ..q.clone()
            },
            now,
        )
        .unwrap();
    assert_eq!(detail.coverage.observed_works, 0);
    assert_eq!(detail.coverage.eligible_works, Some(0));
    assert_eq!(detail.followups.pending, 1);
    assert!(detail.coverage.complete);
    make("aged-stale", "current");
    let stale = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(stale.followups.stale, 1);
    assert_eq!(stale.followups.pending, 1);
    assert_eq!(stale.coverage.observed_works, 0);
    assert_eq!(stale.coverage.eligible_works, Some(0));
    assert!(stale.coverage.complete);
    // A fresh context response observes these three old Works in the period. They
    // join the metadata-backed cohort, keeping numerator and denominator comparable.
    store
        .work_catchup(
            &lead.token,
            &CatchupQuery {
                subscriber: "agent:aged-context".into(),
                delivery_client: "mcp".into(),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    let context = store.value_metrics(&lead.token, &q, now).unwrap();
    assert_eq!(context.coverage.observed_works, 3);
    assert_eq!(context.coverage.eligible_works, Some(3));
    assert!(context.coverage.complete);
    // The same >2,000 Works genuinely enter the eligible cohort after period
    // updates. Their metadata cap must still make population unknown.
    if db._pg.is_some() {
        let mut client = postgres::Client::connect(&db.url, postgres::NoTls).unwrap();
        client.execute("UPDATE hub_work_items SET updated_at=$1,snapshot=jsonb_set(snapshot::jsonb,'{updated_at}',to_jsonb($1::bigint))::text WHERE request_key LIKE 'retained-history-%'", &[&now]).unwrap();
    } else {
        let connection =
            rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap()).unwrap();
        connection.execute("UPDATE hub_work_items SET updated_at=?1,snapshot=json_set(snapshot,'$.updated_at',?1) WHERE request_key LIKE 'retained-history-%'", [now]).unwrap();
    }
    let capped = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(!capped.coverage.complete);
    assert_eq!(capped.coverage.eligible_works, None);
    assert!(
        capped
            .coverage
            .gaps
            .iter()
            .any(|gap| gap.contains("bounded or unavailable"))
    );
}

fn value_history_merge_linkage_retains_aged_candidates_and_rejects_incomplete_uniqueness(db: &Db) {
    use agentscm::hub::catchup::{CatchupMergeSource, CatchupRepositoryScope, NewCatchupMerge};
    use agentscm::hub::value_metrics::ValueMetricsQuery;
    let store = db.open();
    let lead = ordinary_agent(&*store, 709);
    let now = unix_now();
    let old = now - 3 * 3600;
    let finish = |key: &str, head: &str, at| {
        let work = store
            .work_start(&lead.token, &start(key, key, key), at)
            .unwrap()
            .item;
        let mut checkpoint = checkpoint(
            &work.id,
            &format!("finish-{key}"),
            1,
            key,
            CheckpointKind::Finish,
        );
        checkpoint.source_refs = vec![SourceRef::Commit {
            oid: head.into(),
            path: None,
        }];
        apply_checkpoint(&*store, &lead.token, &checkpoint, at)
            .unwrap()
            .item
    };
    let merge = |key: &str, head: &str, pull_number| {
        store
            .record_catchup_merge(
                &CatchupRepositoryScope {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: REPOSITORY.into(),
                    repository_id: Some(REPOSITORY_ID),
                },
                &NewCatchupMerge {
                    source: CatchupMergeSource::ForgePull,
                    source_id: key.into(),
                    occurred_at: Some(now),
                    title: Some("Synthetic retained merge".into()),
                    branch: "feat/synthetic".into(),
                    base: "main".into(),
                    head_commit: head.into(),
                    merge_commit: "e".repeat(40),
                    pull_number,
                    paths: vec![],
                    paths_complete: true,
                },
                now,
            )
            .unwrap();
    };
    let unique_head = "c".repeat(40);
    let shared_head = "d".repeat(40);
    // This old finished Work has no observation, period usage or context delivery.
    let unique = finish("aged-unique-merge", &unique_head, old);
    merge("aged-unique-merge", &unique_head, 101);
    let q = ValueMetricsQuery {
        since: Some(now - 3600),
        until: Some(now),
        ..Default::default()
    };
    let linked = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(linked.coverage.complete);
    assert_eq!(linked.coverage.eligible_works, Some(0));
    assert_eq!(linked.merge_observations, 1);
    assert_eq!(
        linked
            .evidence
            .iter()
            .find(|e| e.kind == "provider_merge")
            .unwrap()
            .work_id
            .as_deref(),
        Some(unique.id.as_str())
    );
    // A cohort Work cannot appear uniquely linked when an aged finished Work
    // outside the cohort declares the same head.
    finish("aged-shared-merge", &shared_head, old);
    let cohort = finish("cohort-shared-merge", &shared_head, now);
    merge("shared-merge", &shared_head, 102);
    let ambiguous = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(ambiguous.coverage.complete);
    assert_eq!(ambiguous.coverage.eligible_works, Some(1));
    assert_eq!(ambiguous.merge_observations, 2);
    assert!(
        ambiguous
            .evidence
            .iter()
            .find(
                |e| e.kind == "provider_merge" && e.source.as_deref() == Some(shared_head.as_str())
            )
            .unwrap()
            .work_id
            .is_none()
    );
    // All synthetic old finished rows share the retained merge's head: they are
    // genuine candidates even without observations, usage or deliveries.
    seed_value_history(db, &lead.token, &cohort, now, false);
    let capped = store.value_metrics(&lead.token, &q, now).unwrap();
    assert!(!capped.coverage.complete);
    assert_eq!(capped.coverage.eligible_works, None);
    assert_eq!(capped.merge_observations, 2);
    // The unique-head Work is retained, but an incomplete aggregate candidate
    // set must not establish uniqueness even for its one visible candidate.
    assert!(
        capped
            .evidence
            .iter()
            .filter(|e| e.kind == "provider_merge")
            .all(|e| e.work_id.is_none())
    );
    let explicit = store
        .value_metrics(
            &lead.token,
            &ValueMetricsQuery {
                work_id: Some(unique.id.clone()),
                ..q
            },
            now,
        )
        .unwrap();
    assert!(explicit.linked_merge.is_some());
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        mod on_sqlite {
            $(#[test] fn $case() { super::$case(&super::Db::sqlite()); })*
        }
        mod on_postgres {
            $(#[test] fn $case() {
                if let Some(db) = super::Db::postgres() { super::$case(&db); }
            })*
        }
    };
}

conformance!(
    value_history_merge_linkage_retains_aged_candidates_and_rejects_incomplete_uniqueness,
    value_aged_finished_followups_keep_actionable_and_period_cohorts_separate,
    value_historical_work_metadata_does_not_expand_eligibility,
    value_history_membership_is_bounded_and_work_target_explicit,
    value_delivery_count_pruning_is_durable,
    value_capture_diagnostics_and_unsupported_outcomes,
    value_receipt_preference_limits_expiry_and_deletion,
    value_usage_enrichment_and_delivery_retention,
    value_projection_and_receipt_conformance,
    replay_and_authorization,
    observation_and_group_limits,
    zero_usage_is_explicitly_unknown,
    keyset_windows_and_cursor_bindings,
    compaction_retains_usage_and_marks_history_incomplete,
    publication_provenance_is_exact_scope_and_checkpoint_first,
);
