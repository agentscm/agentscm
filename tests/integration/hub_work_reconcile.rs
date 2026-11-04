//! Read-only PR evidence reconciliation through real stores with synthetic data.

use crate::hub_pg::PgSchema;

use std::sync::Arc;

use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::connection::{ConnectedRepository, NewConnection};
use agentscm::hub::store::{
    self, ForgeIdentity, Principal, Store, agent_token_digest, mint_agent_token,
    mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::hub::work::{CheckpointKind, WorkCheckpoint, WorkItem, WorkRole, WorkStart};
use agentscm::hub::work_lifecycle::{OutcomeValue, ReservationStatus};
use agentscm::hub::work_links::{WorkPrLinkTarget, WorkPrLinkUpdate};
use agentscm::hub::work_reconcile::WorkReconciliationV1;
use agentscm::hub::work_reconcile::{self, HeadRelation, PrerequisiteState, ProviderStatus};
use serde_json::json;
use tempfile::TempDir;

const REPOSITORY: &str = "acme/widgets";
const REPOSITORY_ID: i64 = 101;
const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct StoreFixture {
    store: Arc<dyn Store>,
    _directory: Option<TempDir>,
    _pg: Option<PgSchema>,
}

impl StoreFixture {
    fn sqlite() -> Self {
        let directory = TempDir::new().unwrap();
        let database = format!("sqlite://{}", directory.path().join("hub.sqlite").display());
        Self {
            store: store::open(&database).unwrap(),
            _directory: Some(directory),
            _pg: None,
        }
    }

    /// `None` without `AGENTSCM_TEST_DATABASE_URL`, so the PostgreSQL cases skip.
    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            store: store::open(&pg.url).unwrap(),
            _directory: None,
            _pg: Some(pg),
        })
    }
}

struct Ctx {
    store: Arc<dyn Store>,
    principal: Principal,
    secret: String,
    repo: ConnectedRepository,
    /// Strictly increasing forge read times in milliseconds.
    clock: i64,
}

fn principal(store: &dyn Store, secret: &str) -> Principal {
    store
        .authenticate(&agent_token_digest(secret).unwrap(), unix_now())
        .unwrap()
        .unwrap()
}

fn user(store: &dyn Store, id: i64, name: &str) -> String {
    store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: id,
            },
            name,
        )
        .unwrap()
        .id
}

fn scope() -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPOSITORY.into(),
        repository_id: Some(REPOSITORY_ID),
    }
}

fn setup(fixture: &StoreFixture) -> Ctx {
    let store = fixture.store.clone();
    let connection = store
        .insert_connection(
            &NewConnection {
                id: new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("robot".into()),
            },
            "usr_admin",
        )
        .unwrap();
    let repo = store
        .upsert_repository(
            &connection.id,
            "github.com",
            REPOSITORY_ID,
            REPOSITORY,
            None,
            None,
        )
        .unwrap();
    for source in [
        CatchupMergeSource::TrackedPublication,
        CatchupMergeSource::ForgePull,
    ] {
        store
            .record_catchup_source_status(
                &scope(),
                &CatchupSourceStatus {
                    source,
                    state: CatchupCoverageState::ObservedOnly,
                    checked_at: unix_now(),
                    unread_pages: 0,
                    deferred: 0,
                    window_from: None,
                },
            )
            .unwrap();
    }
    let owner = user(&*store, 61, "reconciler");
    let (_, secret, _) = mint_bound_agent_token(
        &*store,
        &owner,
        "writer",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let principal = principal(&*store, secret.expose());
    Ctx {
        store,
        principal,
        secret: secret.expose().to_owned(),
        repo,
        clock: 1_000,
    }
}

fn start(key: &str, scope: &str, depends_on: &[&WorkItem]) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: format!("Synthetic {key}"),
        session: format!("session-{key}"),
        role: WorkRole::Lead,
        scope: vec![scope.into()],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: depends_on.iter().map(|w| w.id.clone()).collect(),
        related_work: Vec::new(),
        takeover_generation: None,
        preflight: None,
    }
}

impl Ctx {
    fn token(&self) -> &str {
        &self.principal.token_id
    }

    fn start(&self, key: &str, scope: &str, depends_on: &[&WorkItem]) -> WorkItem {
        self.store
            .work_start(self.token(), &start(key, scope, depends_on), unix_now())
            .unwrap()
            .item
    }

    fn item(&self, id: &str) -> WorkItem {
        self.store
            .work_get(self.token(), id, unix_now())
            .unwrap()
            .unwrap()
    }

    fn link(&self, work: &WorkItem, number: i64, head: &str) {
        let work = self.item(&work.id);
        self.store
            .work_pr_link_set(
                self.token(),
                &WorkPrLinkUpdate {
                    work_id: work.id.clone(),
                    operation_id: format!("link-{}", work.id),
                    generation: work.generation,
                    session: work.lead_session.clone(),
                    expected_updated_seq: work.updated_seq,
                    link: Some(WorkPrLinkTarget {
                        repository_id: REPOSITORY_ID,
                        number,
                        head_sha: head.into(),
                    }),
                },
                unix_now(),
            )
            .unwrap();
    }

    fn checkpoint(&self, work: &WorkItem, kind: CheckpointKind) {
        let work = self.item(&work.id);
        self.store
            .work_checkpoint(
                self.token(),
                &WorkCheckpoint {
                    work_id: work.id.clone(),
                    operation_id: format!("cp-{}-{kind:?}", work.id),
                    generation: work.generation,
                    session: work.lead_session.clone(),
                    expected_updated_seq: Some(work.updated_seq),
                    kind,
                    summary: "synthetic checkpoint".into(),
                    next_action: None,
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: Vec::new(),
                    depends_on: work.depends_on.clone(),
                    related_work: Vec::new(),
                },
                unix_now(),
            )
            .unwrap();
    }

    fn observe_pull(&mut self, number: i64, state: &str, head: &str) {
        self.clock += 1;
        let data = json!({"number": number, "state": state, "head_sha": head}).to_string();
        self.store
            .observe(
                &self.repo.id,
                "pull_request",
                &number.to_string(),
                &data,
                self.clock,
            )
            .unwrap();
    }

    /// A closed pull request, as ingestion records it: the observation is tombstoned.
    fn close_pull(&mut self, number: i64) {
        self.clock += 1;
        let data = json!({"number": number, "state": "closed", "merged": false}).to_string();
        self.store
            .forget(
                &self.repo.id,
                "pull_request",
                &number.to_string(),
                self.clock,
                &data,
                "pull_request_closed",
            )
            .unwrap();
    }

    fn set_coverage(&self, state: CatchupCoverageState) {
        for source in [
            CatchupMergeSource::TrackedPublication,
            CatchupMergeSource::ForgePull,
        ] {
            self.store
                .record_catchup_source_status(
                    &scope(),
                    &CatchupSourceStatus {
                        source,
                        state,
                        checked_at: unix_now(),
                        unread_pages: 0,
                        deferred: 0,
                        window_from: None,
                    },
                )
                .unwrap();
        }
    }

    fn merge(&self, number: i64) {
        let merge = NewCatchupMerge {
            source: CatchupMergeSource::ForgePull,
            source_id: format!("forge-pr-{number}"),
            occurred_at: Some(9),
            title: None,
            branch: format!("feature/{number}"),
            base: "main".into(),
            head_commit: HEAD_A.into(),
            merge_commit: format!("{number:040x}"),
            pull_number: number,
            paths: vec!["src/a".into()],
            paths_complete: true,
            same_repository_head: true,
        };
        self.store
            .record_catchup_merge(&scope(), &merge, 10)
            .unwrap();
    }

    fn read(&self, work: &WorkItem) -> WorkReconciliationV1 {
        work_reconcile::read(&*self.store, &self.principal, &work.id, unix_now())
            .unwrap()
            .unwrap()
    }
}

fn has(out: &WorkReconciliationV1, limit: &str) -> bool {
    out.limits.iter().any(|l| l == limit)
}

fn provider_states(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let work = ctx.start("a", "src/a", &[]);

    // Legacy Work with no link.
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::NotLinked);
    assert!(out.link.is_none() && has(&out, "no_pr_link") && !has(&out, "pr_link_unavailable"));
    assert!(!out.atomic_snapshot);
    assert_eq!(out.provider.coverage, CatchupCoverageState::ObservedOnly);
    assert!(!has(&out, "work_changed_during_read"));

    // Linked, nothing observed: closed state is unreadable, never "not merged".
    ctx.link(&work, 7, HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Unknown);
    assert!(has(&out, "closed_state_not_readable"));
    assert_eq!(out.link.as_ref().unwrap().number, 7);

    // Open observations at the same and at a changed head.
    ctx.observe_pull(7, "open", HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::OpenSameHead);
    assert_eq!(out.provider.head_relation, HeadRelation::Same);
    ctx.observe_pull(7, "open", HEAD_B);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::OpenHeadChanged);
    assert_eq!(out.provider.head_relation, HeadRelation::Changed);

    // The open status carries its read time, and head comparison ignores case.
    assert_eq!(out.provider.open_observed_at_ms, Some(ctx.clock));
    ctx.observe_pull(7, "open", &HEAD_A.to_uppercase());
    assert_eq!(
        ctx.read(&work).provider.status,
        ProviderStatus::OpenSameHead
    );

    // A malformed observation gives no head and says so, and is not "closed".
    ctx.observe_pull(7, "open", "not-a-sha");
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Unknown);
    assert!(has(&out, "observation_malformed") && !has(&out, "closed_state_not_readable"));

    // An observation whose state is not open is not treated as open.
    ctx.observe_pull(7, "closed", HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Unknown);
    assert!(has(&out, "closed_state_not_readable"));

    // Closed unmerged, then genuinely reopened: no merge row, so the open status applies.
    ctx.close_pull(7);
    assert_eq!(ctx.read(&work).provider.status, ProviderStatus::Unknown);
    ctx.observe_pull(7, "open", HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::OpenSameHead);
    assert!(!has(&out, "merge_and_open_observed"));

    // A recorded merge: the outcome is untouched and the reservation is still held.
    ctx.close_pull(7);
    ctx.merge(7);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Merged);
    assert_eq!(out.provider.head_relation, HeadRelation::Unknown);
    assert_eq!(out.outcome.value, OutcomeValue::None);
    assert_eq!(out.reservation.status, ReservationStatus::Held);
    assert!(out.merged_holding_reservation);

    // Recording the same merge again changes nothing, including the digest.
    ctx.merge(7);
    let again = ctx.read(&work);
    assert_eq!(out.digest, again.digest);
    assert_eq!(out, again);

    // A merged PR cannot reopen: a merge row outranks a stale open observation.
    ctx.observe_pull(7, "open", HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Merged);
    assert_eq!(out.provider.head_relation, HeadRelation::Unknown);
    assert!(has(&out, "merge_and_open_observed") && has(&out, "merge_head_not_compared"));
    assert_eq!(out.provider.open_observed_at_ms, None);
    assert!(out.merged_holding_reservation);

    // The caller-reported link token never reaches the output.
    let link_token = ctx
        .store
        .work_pr_link_get(ctx.token(), &work.id, &Default::default(), unix_now())
        .unwrap()
        .unwrap()
        .link
        .unwrap()
        .token;
    let json = serde_json::to_string(&out).unwrap();
    assert!(!link_token.is_empty() && !json.contains(&link_token));
}

fn prerequisites(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let merged = ctx.start("merged", "src/merged", &[]);
    ctx.link(&merged, 8, HEAD_A);
    ctx.close_pull(8);
    ctx.merge(8);
    let reported = ctx.start("reported", "src/reported", &[]);
    ctx.link(&reported, 9, HEAD_A);
    ctx.close_pull(9);
    ctx.checkpoint(&reported, CheckpointKind::Finish);
    let dependent = ctx.start("dependent", "src/dependent", &[&merged, &reported]);
    ctx.checkpoint(&dependent, CheckpointKind::Blocker);

    let out = ctx.read(&dependent);
    let by_id = |id: &str| out.prerequisites.iter().find(|p| p.work_id == id).unwrap();
    assert_eq!(by_id(&merged.id).state, PrerequisiteState::Satisfied);
    assert_eq!(by_id(&merged.id).provider, ProviderStatus::Merged);
    // Reported Completed without provider evidence is not satisfied.
    let done = by_id(&reported.id);
    assert_eq!(done.state, PrerequisiteState::Unknown);
    assert_eq!(done.reported_outcome, Some(OutcomeValue::Completed));
    // This Work keeps its blocker and outcome; the caller decides what to do.
    assert!(out.reassess.needed);
    assert_eq!(out.reassess.blocker_kind, Some(CheckpointKind::Blocker));
    assert_eq!(out.outcome.value, OutcomeValue::None);
    assert_eq!(out.provider.status, ProviderStatus::NotLinked);
    assert_eq!(by_id(&merged.id).link.as_ref().unwrap().number, 8);
    assert!(
        by_id(&reported.id)
            .limits
            .contains(&"closed_state_not_readable".to_owned())
    );
    let text = serde_json::to_string(&out).unwrap();
    assert!(!text.contains("Synthetic merged") && !text.contains("synthetic checkpoint"));
}

fn prerequisite_evidence(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let stale = ctx.start("stale", "src/stale", &[]);
    ctx.link(&stale, 11, HEAD_A);
    ctx.close_pull(11);
    ctx.merge(11);
    ctx.observe_pull(11, "open", HEAD_A);
    let open = ctx.start("open", "src/open", &[]);
    ctx.link(&open, 12, HEAD_A);
    ctx.observe_pull(12, "open", HEAD_A);
    let bad = ctx.start("bad", "src/bad", &[]);
    ctx.link(&bad, 13, HEAD_A);
    ctx.observe_pull(13, "open", "not-a-sha");
    let dependent = ctx.start("dependent", "src/dependent", &[&stale, &open, &bad]);
    ctx.checkpoint(&dependent, CheckpointKind::Blocker);

    let find = |out: &WorkReconciliationV1, id: &str| {
        out.prerequisites
            .iter()
            .find(|p| p.work_id == id)
            .cloned()
            .unwrap()
    };
    let out = ctx.read(&dependent);
    // Merge evidence outranks the stale open observation and satisfies the prerequisite.
    let stale_row = find(&out, &stale.id);
    assert_eq!(stale_row.state, PrerequisiteState::Satisfied);
    assert_eq!(stale_row.provider, ProviderStatus::Merged);
    assert!(
        stale_row
            .limits
            .contains(&"merge_and_open_observed".to_owned())
    );
    assert!(out.reassess.needed);
    assert_eq!(find(&out, &open.id).state, PrerequisiteState::NotSatisfied);
    // A malformed observation lands on its prerequisite row, not the Work's limits.
    let bad_row = find(&out, &bad.id);
    assert!(bad_row.limits.contains(&"observation_malformed".to_owned()));
    assert!(!has(&out, "observation_malformed"));

    // During an outage an open status may be stale; a retained merge still counts.
    for state in [
        CatchupCoverageState::Incomplete,
        CatchupCoverageState::Unavailable,
    ] {
        ctx.set_coverage(state);
        let out = ctx.read(&dependent);
        assert_eq!(out.provider.coverage, state);
        assert_eq!(find(&out, &open.id).state, PrerequisiteState::Unknown);
        assert_eq!(find(&out, &open.id).provider, ProviderStatus::OpenSameHead);
        assert_eq!(find(&out, &stale.id).state, PrerequisiteState::Satisfied);
    }
}

fn prerequisite_cap(fixture: &StoreFixture) {
    let ctx = setup(fixture);
    let deps: Vec<WorkItem> = (0..17)
        .map(|n| ctx.start(&format!("dep{n}"), &format!("src/dep{n}"), &[]))
        .collect();
    let refs: Vec<&WorkItem> = deps.iter().collect();
    let work = ctx.start("many", "src/many", &refs);
    let out = ctx.read(&work);
    assert!(has(&out, "prerequisites_truncated"));
    assert_eq!(out.prerequisites.len(), 17);
    let read = out
        .prerequisites
        .iter()
        .filter(|p| p.reported_outcome.is_some())
        .count();
    assert_eq!(read, 16);
    let unread: Vec<_> = out
        .prerequisites
        .iter()
        .filter(|p| p.reported_outcome.is_none())
        .collect();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].limits, ["prerequisite_not_read"]);
    assert!(
        out.prerequisites
            .iter()
            .all(|p| p.state == PrerequisiteState::Unknown)
    );
}

fn other_repository_and_unbound(fixture: &StoreFixture) {
    let ctx = setup(fixture);
    let work = ctx.start("a", "src/a", &[]);
    let owner = user(&*ctx.store, 62, "stranger");

    // Another repository's token cannot read this Work or learn anything about it.
    let (_, secret, _) =
        mint_bound_agent_token(&*ctx.store, &owner, "other", "other/repo", 202, 3600, false)
            .unwrap();
    let other = principal(&*ctx.store, secret.expose());
    let result = work_reconcile::read(&*ctx.store, &other, &work.id, unix_now());
    assert!(matches!(result, Ok(None)), "{result:?}");
    let unknown = work_reconcile::read(&*ctx.store, &ctx.principal, "wrk_missing", unix_now());
    assert!(matches!(unknown, Ok(None)), "{unknown:?}");

    // An unbound repository has no readable link, which is not an error.
    let (_, secret) = mint_agent_token(&*ctx.store, &owner, "legacy", "legacy/repo", 3600).unwrap();
    let legacy = principal(&*ctx.store, secret.expose());
    let item = ctx
        .store
        .work_start(&legacy.token_id, &start("legacy", "src/l", &[]), unix_now())
        .unwrap()
        .item;
    let out = work_reconcile::read(&*ctx.store, &legacy, &item.id, unix_now())
        .unwrap()
        .unwrap();
    assert!(has(&out, "pr_link_unavailable") && !has(&out, "no_pr_link"));
    assert_eq!(out.provider.status, ProviderStatus::Unknown);
    assert!(out.link.is_none());
}

/// The route, over HTTP, for one store: success, unknown Work and another repository answer.
fn http_route(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let work = ctx.start("a", "src/a", &[]);
    ctx.link(&work, 7, HEAD_A);
    ctx.close_pull(7);
    ctx.merge(7);
    let link_token = ctx
        .store
        .work_pr_link_get(ctx.token(), &work.id, &Default::default(), unix_now())
        .unwrap()
        .unwrap()
        .link
        .unwrap()
        .token;
    let owner = user(&*ctx.store, 62, "stranger");
    let (_, other, _) =
        mint_bound_agent_token(&*ctx.store, &owner, "other", "other/repo", 202, 3600, false)
            .unwrap();
    let other = other.expose().to_owned();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = TempDir::new().unwrap();
    let objects = agentscm::backend::LocalBackend::open(dir.path().join("objects")).unwrap();
    let app = agentscm::hub::router(agentscm::hub::HubState::new(
        ctx.store.clone(),
        Arc::new(objects),
    ));
    let base = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    });
    let get = |id: &str, secret: &str| {
        runtime.block_on(async {
            let response = reqwest::Client::new()
                .get(format!("{base}/v1/hub/standup/work/{id}/reconciliation"))
                .bearer_auth(secret)
                .send()
                .await
                .unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        })
    };

    let (status, body) = get(&work.id, &ctx.secret);
    assert_eq!(status, 200, "{body}");
    let report: WorkReconciliationV1 = serde_json::from_str(&body).unwrap();
    assert_eq!(report.work_id, work.id);
    assert_eq!(report.provider.status, ProviderStatus::Merged);
    assert!(!report.atomic_snapshot);
    assert!(!link_token.is_empty() && !body.contains(&link_token));

    let unknown = get("wrk_00000000000000000000000000000001", &ctx.secret);
    let foreign = get(&work.id, &other);
    assert_eq!(unknown.0, 404, "{}", unknown.1);
    assert_eq!(
        foreign, unknown,
        "another repository must look like unknown Work"
    );
    assert_eq!(get(&work.id, "agt_not-a-token").0, 401);
}

fn run_both(case: fn(&StoreFixture)) {
    case(&StoreFixture::sqlite());
    if let Some(fixture) = StoreFixture::postgres() {
        case(&fixture);
    }
}

#[test]
fn work_reconcile_provider_states() {
    run_both(provider_states);
}

#[test]
fn work_reconcile_prerequisites() {
    run_both(prerequisites);
}

#[test]
fn work_reconcile_prerequisite_evidence() {
    run_both(prerequisite_evidence);
}

#[test]
fn work_reconcile_prerequisite_cap() {
    run_both(prerequisite_cap);
}

#[test]
fn work_reconcile_other_repository_and_unbound() {
    run_both(other_repository_and_unbound);
}

#[test]
fn work_reconcile_http_route() {
    run_both(http_route);
}
