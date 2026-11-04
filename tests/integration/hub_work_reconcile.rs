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
    assert!(has(&ctx.read(&work), "closed_unmerged_observed"));
    ctx.observe_pull(7, "open", HEAD_A);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::OpenSameHead);
    assert!(!has(&out, "merge_and_open_observed"));

    // A recorded merge: the outcome is untouched and the reservation is still held.
    ctx.close_pull(7);
    ctx.merge(7);
    let out = ctx.read(&work);
    assert_eq!(out.provider.status, ProviderStatus::Merged);
    assert_eq!(out.provider.head_relation, HeadRelation::Same);
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
    assert_eq!(out.provider.head_relation, HeadRelation::Same);
    assert!(has(&out, "merge_and_open_observed") && !has(&out, "merge_head_not_compared"));
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
            .contains(&"closed_unmerged_observed".to_owned())
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

    // A bound token with no connected repository read no observation, so nothing says closed.
    let linked = ctx
        .store
        .work_start(&other.token_id, &start("other", "src/o", &[]), unix_now())
        .unwrap()
        .item;
    ctx.store
        .work_pr_link_set(
            &other.token_id,
            &WorkPrLinkUpdate {
                work_id: linked.id.clone(),
                operation_id: "link-other".into(),
                generation: linked.generation,
                session: linked.lead_session.clone(),
                expected_updated_seq: linked.updated_seq,
                link: Some(WorkPrLinkTarget {
                    repository_id: 202,
                    number: 9,
                    head_sha: "a".repeat(40),
                }),
            },
            unix_now(),
        )
        .unwrap();
    let out = work_reconcile::read(&*ctx.store, &other, &linked.id, unix_now())
        .unwrap()
        .unwrap();
    assert!(has(&out, "observation_unavailable") && !has(&out, "closed_state_not_readable"));
    assert_eq!(out.provider.status, ProviderStatus::Unknown);

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

// Exact merged head binding, provider namespaces, and replay/out-of-order observation behavior.
fn exact_evidence(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let stale = ctx.start("stale-merged-head", "src/stale-head", &[]);
    ctx.link(&stale, 41, HEAD_B);
    ctx.merge(41); // Provider merged A, while the caller linked B.
    let dependent = ctx.start("wait-stale", "src/wait-stale", &[&stale]);
    let report = ctx.read(&stale);
    assert_eq!(report.provider.status, ProviderStatus::Merged);
    assert_eq!(report.provider.head_relation, HeadRelation::Changed);
    assert!(has(&report, "merge_head_changed"));
    assert_eq!(
        ctx.read(&dependent).prerequisites[0].state,
        PrerequisiteState::Unknown
    );
    let merges = ctx.store.catchup_merges_for_pull(&scope(), 41).unwrap();
    assert_eq!(merges.len(), 1);
    assert_eq!(merges[0].head_commit, HEAD_A);
    assert_eq!(merges[0].occurred_at, Some(9));
    assert_eq!(merges[0].observed_at, 10);
    ctx.merge(41);
    assert_eq!(
        ctx.store.catchup_merges_for_pull(&scope(), 41).unwrap(),
        merges
    );
    let mut other = scope();
    other.host = "other.example".into();
    assert!(
        ctx.store
            .catchup_merges_for_pull(&other, 41)
            .unwrap()
            .is_empty()
    );
    other = scope();
    other.repository_id = Some(REPOSITORY_ID + 1);
    assert!(
        ctx.store
            .catchup_merges_for_pull(&other, 41)
            .unwrap()
            .is_empty()
    );
    other = scope();
    other.repository = "acme/renamed".into();
    assert_eq!(
        ctx.store.catchup_merges_for_pull(&other, 41).unwrap(),
        merges
    );

    let unknown_time = ctx.start("unknown-time", "src/time", &[]);
    ctx.link(&unknown_time, 43, HEAD_A);
    let mut fact = NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: "pr-43".into(),
        occurred_at: None,
        title: None,
        branch: "feature/time".into(),
        base: "main".into(),
        head_commit: HEAD_A.into(),
        merge_commit: "cccccccccccccccccccccccccccccccccccccccc".into(),
        pull_number: 43,
        paths: Vec::new(),
        paths_complete: false,
        same_repository_head: false,
    };
    ctx.store.record_catchup_merge(&scope(), &fact, 10).unwrap();
    assert!(has(&ctx.read(&unknown_time), "merge_time_unknown"));
    assert_eq!(
        ctx.read(&unknown_time).provider.head_relation,
        HeadRelation::Same
    );
    // Conflicting retained merge identities cannot prove one linked head.
    fact.merge_commit = "dddddddddddddddddddddddddddddddddddddddd".into();
    fact.head_commit = HEAD_B.into();
    ctx.store.record_catchup_merge(&scope(), &fact, 11).unwrap();
    let ambiguous = ctx.read(&unknown_time);
    assert_eq!(ambiguous.provider.head_relation, HeadRelation::Unknown);
    assert!(has(&ambiguous, "multiple_merge_observations"));
    assert_eq!(
        ctx.store.catchup_merges_for_pull(&scope(), 43).unwrap()[0].merge_commit,
        "cccccccccccccccccccccccccccccccccccccccc"
    );

    let open = ctx.start("observation-order", "src/order", &[]);
    ctx.link(&open, 42, HEAD_A);
    ctx.observe_pull(42, "open", HEAD_A);
    let time = ctx.clock;
    let open_data = json!({"number":42,"state":"open","head_sha":HEAD_A}).to_string();
    let closed_data = json!({"number":42,"state":"closed","merged":false}).to_string();
    let event_count = ctx.store.latest_event_seq().unwrap();
    ctx.store
        .observe(&ctx.repo.id, "pull_request", "42", &open_data, time)
        .unwrap();
    assert_eq!(ctx.store.latest_event_seq().unwrap(), event_count);
    ctx.store
        .forget(
            &ctx.repo.id,
            "pull_request",
            "42",
            time - 1,
            &closed_data,
            "pull_request_closed",
        )
        .unwrap();
    assert_eq!(
        ctx.read(&open).provider.status,
        ProviderStatus::OpenSameHead
    );
    // A conflicting equal read stamp cannot choose an open/closed answer by arrival order.
    ctx.store
        .forget(
            &ctx.repo.id,
            "pull_request",
            "42",
            time,
            &closed_data,
            "pull_request_closed",
        )
        .unwrap();
    assert!(has(&ctx.read(&open), "observation_order_ambiguous"));
    assert!(
        ctx.store
            .observation(&ctx.repo.id, "pull_request", "42")
            .unwrap()
            .is_none()
    );
    ctx.store
        .observe(&ctx.repo.id, "pull_request", "42", &open_data, time)
        .unwrap();
    assert!(has(&ctx.read(&open), "observation_order_ambiguous"));
    ctx.observe_pull(42, "open", HEAD_A); // A strictly newer read resolves uncertainty.
    assert_eq!(
        ctx.read(&open).provider.status,
        ProviderStatus::OpenSameHead
    );
    ctx.close_pull(42);
    assert!(has(&ctx.read(&open), "closed_unmerged_observed"));
    ctx.store
        .observe(&ctx.repo.id, "pull_request", "42", &open_data, ctx.clock)
        .unwrap();
    assert!(has(&ctx.read(&open), "observation_order_ambiguous"));
    ctx.observe_pull(42, "open", HEAD_A);
    assert_eq!(
        ctx.read(&open).provider.status,
        ProviderStatus::OpenSameHead
    );
}

#[test]
fn work_reconcile_exact_evidence_and_order() {
    run_both(exact_evidence);
}

fn durable_declarations(fixture: &StoreFixture) {
    use agentscm::hub::work_reconciliation::{
        DeclaredPrLink, PrerequisiteCondition, PrerequisiteRequirement, WorkDeclarations,
        WorkDeclarationsUpdate,
    };
    let ctx = setup(fixture);
    let prerequisite = ctx.start("decl-prereq", "src/dependency", &[]);
    let work = ctx.start("decl-work", "src/consumer", &[&prerequisite]);
    let initial = ctx
        .store
        .work_declarations_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(initial.schema_version, 2);
    assert_eq!(initial.declaration_seq, work.updated_seq);
    assert_eq!(initial.declarations.prerequisites.len(), 1);
    assert!(
        matches!(&initial.declarations.prerequisites[0].requirement,PrerequisiteRequirement::LegacyWork{work_id} if work_id==&prerequisite.id)
    );
    let target = |number, head: &str| WorkPrLinkTarget {
        repository_id: REPOSITORY_ID,
        number,
        head_sha: head.into(),
    };
    let update = WorkDeclarationsUpdate {
        work_id: work.id.clone(),
        operation_id: "declare-v2".into(),
        generation: work.generation,
        session: work.lead_session.clone(),
        expected_updated_seq: work.updated_seq,
        declarations: WorkDeclarations {
            links: vec![
                DeclaredPrLink {
                    id: "second".into(),
                    target: target(22, HEAD_B),
                },
                DeclaredPrLink {
                    id: "first".into(),
                    target: target(21, HEAD_A),
                },
            ],
            prerequisites: vec![PrerequisiteCondition {
                id: "merged-dependency".into(),
                requirement: PrerequisiteRequirement::MergedPullRequest {
                    work_id: prerequisite.id.clone(),
                    target: target(20, HEAD_A),
                },
            }],
        },
    };
    let result = ctx
        .store
        .work_declarations_set(ctx.token(), &update, unix_now())
        .unwrap();
    assert!(!result.receipt.replayed);
    assert_eq!(result.view.declarations.links[0].id, "first");
    assert_eq!(result.view.declaration_seq, result.receipt.event_seq);
    // An ordinary old client that omits depends_on cannot erase the independent declaration.
    let current = ctx.item(&work.id);
    let checkpoint = WorkCheckpoint {
        work_id: work.id.clone(),
        operation_id: "legacy-progress".into(),
        generation: current.generation,
        session: current.lead_session.clone(),
        expected_updated_seq: Some(current.updated_seq),
        kind: CheckpointKind::Progress,
        summary: "still implementing".into(),
        next_action: None,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    };
    ctx.store
        .work_checkpoint(ctx.token(), &checkpoint, unix_now())
        .unwrap();
    assert!(ctx.item(&work.id).depends_on.is_empty());
    let after = ctx
        .store
        .work_declarations_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(after.declarations, result.view.declarations);
    assert_eq!(after.declaration_seq, result.view.declaration_seq);
    // Legacy single-link mutation is a separate compatibility slot, not a set replacement.
    ctx.link(&ctx.item(&work.id), 23, HEAD_B);
    let after = ctx
        .store
        .work_declarations_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(after.declarations, result.view.declarations);
    assert_eq!(after.compatibility_link, Some(target(23, HEAD_B)));
    let replay = ctx
        .store
        .work_declarations_set(ctx.token(), &update, unix_now())
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, result.receipt.event_seq);
    assert_eq!(replay.view.updated_seq, after.updated_seq);
    let mut reordered = update.clone();
    reordered.declarations.links.reverse();
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &reordered, unix_now())
            .unwrap()
            .receipt
            .replayed
    );
    let mut mismatch = update.clone();
    mismatch.declarations.links.clear();
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &mismatch, unix_now())
            .is_err()
    );
    let mut stale = update.clone();
    stale.operation_id = "new-stale".into();
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &stale, unix_now())
            .is_err()
    );
    let mut clear = update.clone();
    clear.operation_id = "explicit-clear".into();
    clear.expected_updated_seq = ctx.item(&work.id).updated_seq;
    clear.declarations = WorkDeclarations::default();
    let cleared = ctx
        .store
        .work_declarations_set(ctx.token(), &clear, unix_now())
        .unwrap();
    assert!(
        cleared.view.declarations.links.is_empty()
            && cleared.view.declarations.prerequisites.is_empty()
    );
    assert_eq!(cleared.view.compatibility_link, Some(target(23, HEAD_B)));
    let encoded = serde_json::to_string(&cleared.view).unwrap();
    assert!(!encoded.contains(ctx.token()) && !encoded.contains(&ctx.secret));
    // The declarations operation never changes report, outcome or reservation.
    assert_eq!(
        ctx.item(&work.id).current.unwrap().summary,
        "still implementing"
    );
    assert_eq!(ctx.read(&work).outcome.value, OutcomeValue::None);
    assert_eq!(ctx.read(&work).reservation.status, ReservationStatus::Held);
}

#[test]
fn work_declarations_sqlite_persistence_and_compatibility() {
    durable_declarations(&StoreFixture::sqlite());
}
#[test]
fn postgres_work_declarations_persistence_and_compatibility() {
    if let Some(fixture) = StoreFixture::postgres() {
        durable_declarations(&fixture);
    }
}

fn declaration_authorization(fixture: &StoreFixture) {
    use agentscm::hub::work_reconciliation::{
        DeclaredPrLink, WorkDeclarations, WorkDeclarationsUpdate,
    };
    let ctx = setup(fixture);
    let work = ctx.start("decl-auth", "src/auth", &[]);
    let update = WorkDeclarationsUpdate {
        work_id: work.id.clone(),
        operation_id: "decl-auth".into(),
        generation: work.generation,
        session: work.lead_session.clone(),
        expected_updated_seq: work.updated_seq,
        declarations: WorkDeclarations {
            links: vec![DeclaredPrLink {
                id: "pr".into(),
                target: WorkPrLinkTarget {
                    repository_id: REPOSITORY_ID,
                    number: 1,
                    head_sha: HEAD_A.into(),
                },
            }],
            prerequisites: vec![],
        },
    };
    let (_, read_secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &ctx.principal.owner.id,
        "reader",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        true,
    )
    .unwrap();
    let reader = principal(&*ctx.store, read_secret.expose());
    assert!(
        ctx.store
            .work_declarations_get(&reader.token_id, &work.id, unix_now())
            .unwrap()
            .is_some()
    );
    assert!(
        ctx.store
            .work_declarations_set(&reader.token_id, &update, unix_now())
            .is_err()
    );
    let (_, helper_secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &ctx.principal.owner.id,
        "helper",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let helper = principal(&*ctx.store, helper_secret.expose());
    assert!(
        ctx.store
            .work_declarations_set(&helper.token_id, &update, unix_now())
            .is_err()
    );
    let mut wrong = update.clone();
    wrong.session = "another-session".into();
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &wrong, unix_now())
            .is_err()
    );
    wrong = update.clone();
    wrong.generation += 1;
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &wrong, unix_now())
            .is_err()
    );
    wrong = update.clone();
    wrong.declarations.links[0].target.repository_id += 1;
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &wrong, unix_now())
            .is_err()
    );
    wrong = update.clone();
    wrong
        .declarations
        .links
        .push(wrong.declarations.links[0].clone());
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &wrong, unix_now())
            .is_err()
    );
    wrong = update.clone();
    wrong.declarations.links = vec![wrong.declarations.links[0].clone(); 33];
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &wrong, unix_now())
            .is_err()
    );
    let other_owner = user(&*ctx.store, 999, "outsider");
    let (_, other_secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &other_owner,
        "foreign",
        "acme/other",
        999,
        3600,
        false,
    )
    .unwrap();
    let other = principal(&*ctx.store, other_secret.expose());
    assert!(
        ctx.store
            .work_declarations_get(&other.token_id, &work.id, unix_now())
            .unwrap()
            .is_none()
    );
    assert!(
        ctx.store
            .work_declarations_set(&other.token_id, &update, unix_now())
            .is_err()
    );
    assert_eq!(ctx.item(&work.id).updated_seq, work.updated_seq);
}
#[test]
fn work_declarations_sqlite_authorization() {
    declaration_authorization(&StoreFixture::sqlite());
}
#[test]
fn postgres_work_declarations_authorization() {
    if let Some(fixture) = StoreFixture::postgres() {
        declaration_authorization(&fixture);
    }
}

fn declarations_concurrent(fixture: &StoreFixture) {
    use agentscm::hub::work_reconciliation::{
        DeclaredPrLink, WorkDeclarations, WorkDeclarationsUpdate,
    };
    let ctx = setup(fixture);
    let work = ctx.start("declaration-race", "src/race", &[]);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (1..=2)
        .map(|number| {
            let store = ctx.store.clone();
            let token = ctx.token().to_owned();
            let barrier = barrier.clone();
            let request = WorkDeclarationsUpdate {
                work_id: work.id.clone(),
                operation_id: format!("race-{number}"),
                generation: work.generation,
                session: work.lead_session.clone(),
                expected_updated_seq: work.updated_seq,
                declarations: WorkDeclarations {
                    links: vec![DeclaredPrLink {
                        id: "pr".into(),
                        target: WorkPrLinkTarget {
                            repository_id: REPOSITORY_ID,
                            number,
                            head_sha: HEAD_A.into(),
                        },
                    }],
                    prerequisites: vec![],
                },
            };
            std::thread::spawn(move || {
                barrier.wait();
                store.work_declarations_set(&token, &request, unix_now())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    let current = ctx
        .store
        .work_declarations_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(current, winner.view);
}
#[test]
fn work_declarations_sqlite_concurrent_fence() {
    declarations_concurrent(&StoreFixture::sqlite());
}
#[test]
fn postgres_work_declarations_concurrent_fence() {
    if let Some(fixture) = StoreFixture::postgres() {
        declarations_concurrent(&fixture);
    }
}

#[test]
fn work_declarations_sqlite_migrates_legacy_once() {
    use agentscm::hub::work_reconciliation::{PrerequisiteRequirement, WorkDeclarations};
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE hub_work_items(id TEXT PRIMARY KEY, updated_seq INTEGER, provider TEXT,host TEXT,repository TEXT,repository_id INTEGER); CREATE TABLE hub_work_relations(work TEXT,related TEXT,kind TEXT); CREATE TABLE hub_work_usage(provider TEXT,host TEXT,repository TEXT,repository_id INTEGER,charged_bytes INTEGER); INSERT INTO hub_work_items VALUES ('work',42,'github','github.com','acme/repo',1); INSERT INTO hub_work_relations VALUES ('work','second','depends_on'),('work','first','depends_on'),('work','other','related'); INSERT INTO hub_work_usage VALUES ('github','github.com','acme/repo',1,100);").unwrap();
    conn.execute_batch(include_str!(
        "../../src/hub/migrations/named/work_reconciliation.sqlite.sql"
    ))
    .unwrap();
    let (snapshot, seq): (String, i64) = conn
        .query_row(
            "SELECT snapshot,event_seq FROM hub_work_declarations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let declared: WorkDeclarations = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(seq, 42);
    assert_eq!(declared.prerequisites.len(), 2);
    assert!(
        matches!(&declared.prerequisites[0].requirement,PrerequisiteRequirement::LegacyWork{work_id} if work_id=="first")
    );
    assert!(declared.links.is_empty());
    let charged: i64 = conn
        .query_row("SELECT charged_bytes FROM hub_work_usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(charged, 100 + snapshot.len() as i64 + 256);
    // Later relation changes cannot erase the imported declaration.
    conn.execute("DELETE FROM hub_work_relations", []).unwrap();
    assert_eq!(
        conn.query_row("SELECT snapshot FROM hub_work_declarations", [], |r| r
            .get::<_, String>(
            0
        ))
        .unwrap(),
        snapshot
    );
}

#[test]
fn postgres_work_declarations_migrates_legacy_once() {
    use agentscm::hub::work_reconciliation::WorkDeclarations;
    let Some(pg) = PgSchema::create() else {
        return;
    };
    let mut client = postgres::Client::connect(&pg.url, postgres::NoTls).unwrap();
    client.batch_execute("CREATE TABLE hub_work_items(id TEXT PRIMARY KEY,updated_seq BIGINT,provider TEXT,host TEXT,repository TEXT,repository_id BIGINT); CREATE TABLE hub_work_relations(work TEXT,related TEXT,kind TEXT); CREATE TABLE hub_work_usage(provider TEXT,host TEXT,repository TEXT,repository_id BIGINT,charged_bytes BIGINT); INSERT INTO hub_work_items VALUES ('work',42,'github','github.com','acme/repo',1); INSERT INTO hub_work_relations VALUES ('work','second','depends_on'),('work','first','depends_on'),('work','other','related'); INSERT INTO hub_work_usage VALUES ('github','github.com','acme/repo',1,100);").unwrap();
    client
        .batch_execute(include_str!(
            "../../src/hub/migrations/named/work_reconciliation.pg.sql"
        ))
        .unwrap();
    let row = client
        .query_one("SELECT snapshot,event_seq FROM hub_work_declarations", &[])
        .unwrap();
    let snapshot: String = row.get(0);
    let seq: i64 = row.get(1);
    let declared: WorkDeclarations = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(seq, 42);
    assert_eq!(declared.prerequisites.len(), 2);
    assert_eq!(declared.prerequisites[0].id, "legacy-first");
    let charged: i64 = client
        .query_one("SELECT charged_bytes FROM hub_work_usage", &[])
        .unwrap()
        .get(0);
    assert_eq!(charged, 100 + snapshot.len() as i64 + 256);
}
