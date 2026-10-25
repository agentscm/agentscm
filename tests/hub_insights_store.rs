//! Work-insight storage conformance. Every case runs on SQLite and, when configured, PostgreSQL.
#[path = "common/hub_pg.rs"]
mod hub_pg;

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
    replay_and_authorization,
    observation_and_group_limits,
    zero_usage_is_explicitly_unknown,
    keyset_windows_and_cursor_bindings,
    compaction_retains_usage_and_marks_history_incomplete,
    publication_provenance_is_exact_scope_and_checkpoint_first,
);
