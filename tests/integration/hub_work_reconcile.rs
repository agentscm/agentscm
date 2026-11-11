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
use agentscm::hub::work_reconcile::{
    self, HeadRelation, LinkBasis, PrerequisiteState, ProviderStatus,
};
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
        issue_choice: None,
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: format!("Synthetic {key}"),
        description: None,
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
        (
            foreign.0,
            crate::hub_error_assert::blank_references(&foreign.1)
        ),
        (
            unknown.0,
            crate::hub_error_assert::blank_references(&unknown.1)
        ),
        "another repository must look like unknown Work"
    );
    assert_eq!(get(&work.id, "agt_not-a-token").0, 401);
}

fn branch_pull(ctx: &mut Ctx, number: i64, branch: &str, head: &str, head_repository: i64) {
    ctx.clock += 1;
    let data = json!({"number": number, "state": "open", "repository_id": REPOSITORY_ID,
        "head_ref": branch, "head_sha": head, "base_ref": "main",
        "head_repository_id": head_repository})
    .to_string();
    ctx.store
        .observe(
            &ctx.repo.id,
            "pull_request",
            &number.to_string(),
            &data,
            ctx.clock,
        )
        .unwrap();
}

fn branch_work(ctx: &Ctx, key: &str, scope: &str, branch: &str) -> WorkItem {
    let mut request = start(key, scope, &[]);
    request.branch = Some(branch.into());
    ctx.store
        .work_start(ctx.token(), &request, unix_now())
        .unwrap()
        .item
}

fn branch_inference(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    // A same-repository open pull request on the branch links by inference.
    let work = branch_work(&ctx, "a", "src/a", "feature/a");
    branch_pull(&mut ctx, 21, "feature/a", HEAD_A, REPOSITORY_ID);
    let out = ctx.read(&work);
    let link = out.link.as_ref().unwrap();
    assert_eq!(link.basis, LinkBasis::BranchObserved);
    assert_eq!(link.number, 21);
    assert!(!has(&out, "no_pr_link"));
    assert_eq!(out.provider.status, ProviderStatus::OpenSameHead);
    assert_eq!(out.provider.head_relation, HeadRelation::Unknown);
    assert!(
        serde_json::to_string(&out)
            .unwrap()
            .contains("branch_observed")
    );

    // A fork pull request never links.
    let fork = branch_work(&ctx, "b", "src/b", "feature/b");
    branch_pull(&mut ctx, 22, "feature/b", HEAD_B, 999);
    let out = ctx.read(&fork);
    assert!(out.link.is_none() && has(&out, "no_pr_link"));

    // An explicit link wins over a different pull request on the branch.
    let explicit = branch_work(&ctx, "c", "src/c", "feature/c");
    ctx.link(&explicit, 5, HEAD_A);
    branch_pull(&mut ctx, 23, "feature/c", HEAD_B, REPOSITORY_ID);
    let out = ctx.read(&explicit);
    let link = out.link.as_ref().unwrap();
    assert_eq!((link.basis, link.number), (LinkBasis::CallerReported, 5));

    // Two pull requests on one branch are a named limit.
    let many = branch_work(&ctx, "d", "src/d", "feature/d");
    branch_pull(&mut ctx, 24, "feature/d", HEAD_A, REPOSITORY_ID);
    branch_pull(&mut ctx, 25, "feature/d", HEAD_B, REPOSITORY_ID);
    let out = ctx.read(&many);
    assert!(out.link.is_none() && has(&out, "branch_pull_requests_multiple"));
}

fn branch_merge(ctx: &Ctx, number: i64, branch: &str, at: i64, same: bool) {
    let merge = NewCatchupMerge {
        source: CatchupMergeSource::ForgePull,
        source_id: format!("forge-pr-{number}"),
        occurred_at: Some(at),
        title: None,
        branch: branch.into(),
        base: "main".into(),
        head_commit: HEAD_B.into(),
        merge_commit: format!("{number:040x}"),
        pull_number: number,
        paths: vec!["src/a".into()],
        paths_complete: true,
        same_repository_head: same,
    };
    ctx.store
        .record_catchup_merge(&scope(), &merge, at + 1)
        .unwrap();
}

fn branch_merged(fixture: &StoreFixture) {
    let ctx = setup(fixture);
    let after = unix_now() + 5;
    let setup_observations = agentscm::hub::onboarding::Observations::default();

    let merged = branch_work(&ctx, "m", "src/m", "feature/m");
    branch_merge(&ctx, 41, "feature/m", after, true);
    let out = ctx.read(&merged);
    let link = out.link.as_ref().unwrap();
    assert_eq!((link.basis, link.number), (LinkBasis::BranchObserved, 41));
    assert_eq!(out.provider.status, ProviderStatus::Merged);
    assert!(!has(&out, "no_pr_link"));
    assert_eq!(out.outcome.value, OutcomeValue::None);

    let fork = branch_work(&ctx, "f", "src/f", "feature/f");
    branch_merge(&ctx, 42, "feature/f", after, false);
    let out = ctx.read(&fork);
    assert!(out.link.is_none() && has(&out, "no_pr_link"));

    let early = branch_work(&ctx, "e", "src/e", "feature/e");
    branch_merge(&ctx, 43, "feature/e", 1, true);
    let out = ctx.read(&early);
    assert!(out.link.is_none() && has(&out, "no_pr_link"));

    let status = agentscm::hub::work_http::actionable_status::read(
        &*ctx.store,
        &setup_observations,
        &ctx.principal,
        unix_now(),
    )
    .unwrap();
    let row = status.work.iter().find(|w| w.work_id == merged.id).unwrap();
    assert_eq!(
        serde_json::to_value(row.stage.as_ref().unwrap()).unwrap(),
        json!({"stage": "pull_request_merged", "pull_request": 41, "basis": "branch_observed"})
    );

    // The head repository round-trips, and a merge's base is a merge base.
    let (rows, bases) = ctx
        .store
        .catchup_branch_merges(&scope(), &["feature/m".into(), "feature/f".into()])
        .unwrap();
    let same: Vec<_> = rows
        .iter()
        .map(|(b, m)| (b.as_str(), m.same_repository_head))
        .collect();
    assert_eq!(
        same,
        [("feature/f", Some(false)), ("feature/m", Some(true))]
    );
    assert!(!bases.contains("feature/m"));
    let (_, bases) = ctx
        .store
        .catchup_branch_merges(&scope(), &["main".into()])
        .unwrap();
    assert!(bases.contains("main"));
}

#[test]
fn work_reconcile_branch_merged() {
    run_both(branch_merged);
}

fn status_stage(fixture: &StoreFixture) {
    let mut ctx = setup(fixture);
    let setup_observations = agentscm::hub::onboarding::Observations::default();
    let plain = branch_work(&ctx, "p", "src/p", "feature/p");
    let open = branch_work(&ctx, "o", "src/o", "feature/o");
    branch_pull(&mut ctx, 31, "feature/o", HEAD_A, REPOSITORY_ID);
    let many = branch_work(&ctx, "m", "src/m", "feature/m");
    branch_pull(&mut ctx, 32, "feature/m", HEAD_A, REPOSITORY_ID);
    branch_pull(&mut ctx, 33, "feature/m", HEAD_B, REPOSITORY_ID);
    let status = agentscm::hub::work_http::actionable_status::read(
        &*ctx.store,
        &setup_observations,
        &ctx.principal,
        unix_now(),
    )
    .unwrap();
    let stage = |work: &WorkItem| {
        let row = status.work.iter().find(|w| w.work_id == work.id).unwrap();
        serde_json::to_value(row.stage.as_ref().unwrap()).unwrap()
    };
    assert_eq!(
        stage(&plain),
        json!({"stage": "working", "pull_request": null, "basis": null})
    );
    assert_eq!(
        stage(&open),
        json!({"stage": "pull_request_open", "pull_request": 31, "basis": "branch_observed"})
    );
    assert_eq!(stage(&many)["stage"], "unknown");
}

#[test]
fn work_reconcile_status_stage() {
    run_both(status_stage);
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

fn initialize_unversioned_declarations(fixture: &StoreFixture) {
    use agentscm::hub::work_reconciliation::{WorkDeclarations, WorkDeclarationsUpdate};
    let ctx = setup(fixture);
    let work = ctx.start("unversioned-declarations", "src/unversioned", &[]);
    if let Some(pg) = &fixture._pg {
        postgres::Client::connect(&pg.url, postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE hub_work_declarations SET event_seq=0 WHERE work=$1",
                &[&work.id],
            )
            .unwrap();
    } else {
        rusqlite::Connection::open(
            fixture
                ._directory
                .as_ref()
                .unwrap()
                .path()
                .join("hub.sqlite"),
        )
        .unwrap()
        .execute(
            "UPDATE hub_work_declarations SET event_seq=0 WHERE work=?1",
            [&work.id],
        )
        .unwrap();
    }
    let unversioned = durable_request(&ctx, &work.id, "unversioned-apply");
    assert_eq!(unversioned.declaration_seq, 0);
    assert!(
        ctx.store
            .work_reconciliation_apply(ctx.token(), &unversioned, unix_now())
            .is_err()
    );
    let request = WorkDeclarationsUpdate {
        work_id: work.id.clone(),
        operation_id: "initialize-declarations".into(),
        generation: work.generation,
        session: work.lead_session.clone(),
        expected_updated_seq: work.updated_seq,
        declarations: WorkDeclarations::default(),
    };
    let initialized = ctx
        .store
        .work_declarations_set(ctx.token(), &request, unix_now())
        .unwrap();
    assert!(initialized.view.declaration_seq > work.updated_seq);
    assert_eq!(
        initialized.view.declaration_seq,
        initialized.receipt.event_seq
    );
    assert!(initialized.view.declarations.links.is_empty());
    assert!(initialized.view.declarations.prerequisites.is_empty());
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &request, unix_now())
            .unwrap()
            .receipt
            .replayed
    );
    let mut unchanged = request;
    unchanged.operation_id = "already-initialized".into();
    unchanged.expected_updated_seq = initialized.view.updated_seq;
    assert!(
        ctx.store
            .work_declarations_set(ctx.token(), &unchanged, unix_now())
            .is_err()
    );
    let fresh = durable_request(&ctx, &work.id, "initialized-apply");
    let applied = ctx
        .store
        .work_reconciliation_apply(ctx.token(), &fresh, unix_now())
        .unwrap();
    assert!(applied.view.current.is_some());
    assert_eq!(ctx.read(&work).outcome.value, OutcomeValue::None);
    assert_eq!(ctx.read(&work).reservation.status, ReservationStatus::Held);
}

#[test]
fn unversioned_declarations_sqlite_require_explicit_initialization() {
    initialize_unversioned_declarations(&StoreFixture::sqlite());
}

#[test]
fn postgres_unversioned_declarations_require_explicit_initialization() {
    if let Some(fixture) = StoreFixture::postgres() {
        initialize_unversioned_declarations(&fixture);
    }
}

fn durable_request(
    ctx: &Ctx,
    id: &str,
    operation: &str,
) -> agentscm::hub::work_reconciliation::ReconciliationApply {
    let view = ctx
        .store
        .work_declarations_get(ctx.token(), id, unix_now())
        .unwrap()
        .unwrap();
    let item = ctx.item(id);
    agentscm::hub::work_reconciliation::ReconciliationApply {
        work_id: id.into(),
        operation_id: operation.into(),
        generation: item.generation,
        session: item.lead_session,
        expected_updated_seq: item.updated_seq,
        declaration_seq: view.declaration_seq,
    }
}
fn durable_reconciliation(fixture: &StoreFixture) {
    use agentscm::hub::work_reconciliation::{
        ConditionState, DeclaredPrLink, PrerequisiteCondition, PrerequisiteRequirement,
        WorkDeclarations, WorkDeclarationsUpdate,
    };
    let mut ctx = setup(fixture);
    let prerequisite = ctx.start("durable-prereq", "src/prereq", &[]);
    let work = ctx.start("durable-main", "src/main", &[]);
    ctx.checkpoint(&work, CheckpointKind::Blocker);
    let item = ctx.item(&work.id);
    let target = |number, head: &str| WorkPrLinkTarget {
        repository_id: REPOSITORY_ID,
        number,
        head_sha: head.into(),
    };
    ctx.store
        .work_declarations_set(
            ctx.token(),
            &WorkDeclarationsUpdate {
                work_id: item.id.clone(),
                operation_id: "declare-durable".into(),
                generation: item.generation,
                session: item.lead_session.clone(),
                expected_updated_seq: item.updated_seq,
                declarations: WorkDeclarations {
                    links: vec![
                        DeclaredPrLink {
                            id: "partial-1".into(),
                            target: target(81, HEAD_A),
                        },
                        DeclaredPrLink {
                            id: "partial-2".into(),
                            target: target(82, HEAD_A),
                        },
                    ],
                    prerequisites: vec![
                        PrerequisiteCondition {
                            id: "required-a".into(),
                            requirement: PrerequisiteRequirement::MergedPullRequest {
                                work_id: prerequisite.id.clone(),
                                target: target(81, HEAD_A),
                            },
                        },
                        PrerequisiteCondition {
                            id: "required-b".into(),
                            requirement: PrerequisiteRequirement::MergedPullRequest {
                                work_id: prerequisite.id.clone(),
                                target: target(82, HEAD_A),
                            },
                        },
                        PrerequisiteCondition {
                            id: "wrong-head".into(),
                            requirement: PrerequisiteRequirement::MergedPullRequest {
                                work_id: prerequisite.id.clone(),
                                target: target(81, HEAD_B),
                            },
                        },
                    ],
                },
            },
            unix_now(),
        )
        .unwrap();
    ctx.observe_pull(81, "open", HEAD_A);
    ctx.observe_pull(82, "open", HEAD_A);
    ctx.merge(81);
    let before = ctx.item(&work.id);
    let request = durable_request(&ctx, &work.id, "evaluate-1");
    let result = ctx
        .store
        .work_reconciliation_apply(ctx.token(), &request, unix_now())
        .unwrap();
    let current = result.view.current.as_ref().unwrap();
    assert_eq!(
        current
            .conditions
            .iter()
            .find(|c| c.id == "required-a")
            .unwrap()
            .state,
        ConditionState::Satisfied
    );
    assert_eq!(
        current
            .conditions
            .iter()
            .find(|c| c.id == "required-b")
            .unwrap()
            .state,
        ConditionState::NotSatisfied
    );
    assert_eq!(
        current
            .conditions
            .iter()
            .find(|c| c.id == "wrong-head")
            .unwrap()
            .state,
        ConditionState::Unknown
    );
    assert_eq!(current.reassessments.len(), 1);
    assert_eq!(
        current.reservation_disposition,
        "held_owner_decision_required"
    );
    let after = ctx.item(&work.id);
    assert_eq!(before.current, after.current);
    assert_eq!(before.blocker, after.blocker);
    assert_eq!(before.state, after.state);
    assert_eq!(ctx.read(&work).outcome.value, OutcomeValue::None);
    assert_eq!(ctx.read(&work).reservation.status, ReservationStatus::Held);
    let replay = ctx
        .store
        .work_reconciliation_apply(ctx.token(), &request, unix_now())
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.event_seq, result.receipt.event_seq);
    assert_eq!(
        replay.view.retained.as_ref().unwrap().reassessments,
        current.reassessments
    );
    let raw = serde_json::to_string(&result.view).unwrap();
    assert!(!raw.contains(ctx.token()));
    assert!(!raw.contains(&ctx.secret));
    let mut mismatch = request.clone();
    mismatch.declaration_seq += 1;
    assert!(
        ctx.store
            .work_reconciliation_apply(ctx.token(), &mismatch, unix_now())
            .is_err()
    );
    ctx.close_pull(82);
    let stale = ctx
        .store
        .work_reconciliation_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert!(stale.current.is_none());
    assert!(stale.retained.is_some());
    assert!(
        stale
            .stale_reasons
            .contains(&"provider_or_prerequisite_evidence_changed".into())
    );
    let result = ctx
        .store
        .work_reconciliation_apply(
            ctx.token(),
            &durable_request(&ctx, &work.id, "evaluate-closed"),
            unix_now(),
        )
        .unwrap();
    assert_eq!(
        result
            .view
            .current
            .unwrap()
            .conditions
            .iter()
            .find(|c| c.id == "required-b")
            .unwrap()
            .state,
        ConditionState::NotSatisfied
    );
    ctx.observe_pull(82, "open", HEAD_A);
    ctx.set_coverage(CatchupCoverageState::Unavailable);
    let result = ctx
        .store
        .work_reconciliation_apply(
            ctx.token(),
            &durable_request(&ctx, &work.id, "evaluate-outage"),
            unix_now(),
        )
        .unwrap();
    let current = result.view.current.unwrap();
    assert_eq!(
        current
            .conditions
            .iter()
            .find(|c| c.id == "required-a")
            .unwrap()
            .state,
        ConditionState::Satisfied
    );
    assert_eq!(
        current
            .conditions
            .iter()
            .find(|c| c.id == "required-b")
            .unwrap()
            .state,
        ConditionState::Unknown
    );
    ctx.checkpoint(&work, CheckpointKind::Progress);
    let stale = ctx
        .store
        .work_reconciliation_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert!(stale.current.is_none());
    assert!(stale.stale_reasons.contains(&"work_changed".into()));
    assert!(
        ctx.store
            .work_reconciliation_apply(
                ctx.token(),
                &durable_request(&ctx, &work.id, "evaluate-outage"),
                unix_now()
            )
            .is_err()
    );
    let old = durable_request(&ctx, &work.id, "stale-apply");
    ctx.checkpoint(&work, CheckpointKind::Decision);
    assert!(
        ctx.store
            .work_reconciliation_apply(ctx.token(), &old, unix_now())
            .is_err()
    );
    let (_, secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &ctx.principal.owner.id,
        "durable-reader",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        true,
    )
    .unwrap();
    let reader = principal(&*ctx.store, secret.expose());
    assert!(
        ctx.store
            .work_reconciliation_get(&reader.token_id, &work.id, unix_now())
            .unwrap()
            .is_some()
    );
    assert!(
        ctx.store
            .work_reconciliation_apply(
                &reader.token_id,
                &durable_request(&ctx, &work.id, "readonly"),
                unix_now()
            )
            .is_err()
    );
}
#[test]
fn durable_reconciliation_sqlite() {
    durable_reconciliation(&StoreFixture::sqlite());
}
#[test]
fn postgres_durable_reconciliation() {
    if let Some(f) = StoreFixture::postgres() {
        durable_reconciliation(&f);
    }
}

fn reconciliation_concurrent(fixture: &StoreFixture) {
    let ctx = setup(fixture);
    let work = ctx.start("reconcile-race", "src/race", &[]);
    let request = durable_request(&ctx, &work.id, "race-1");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|index| {
            let store = ctx.store.clone();
            let token = ctx.token().to_owned();
            let barrier = barrier.clone();
            let mut update = request.clone();
            update.operation_id = format!("race-{index}");
            std::thread::spawn(move || {
                barrier.wait();
                store.work_reconciliation_apply(&token, &update, unix_now())
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    let current = ctx
        .store
        .work_reconciliation_get(ctx.token(), &work.id, unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(current, winner.view);
    // A reopened connection retains both the snapshot and the exact retry receipt.
    let reopened = if let Some(directory) = &fixture._directory {
        store::open(&format!(
            "sqlite://{}",
            directory.path().join("hub.sqlite").display()
        ))
        .unwrap()
    } else {
        store::open(&fixture._pg.as_ref().unwrap().url).unwrap()
    };
    assert_eq!(
        reopened
            .work_reconciliation_get(ctx.token(), &work.id, unix_now())
            .unwrap()
            .unwrap(),
        current
    );
}
#[test]
fn durable_reconciliation_sqlite_concurrent_restart() {
    reconciliation_concurrent(&StoreFixture::sqlite());
}
#[test]
fn postgres_durable_reconciliation_concurrent_restart() {
    if let Some(f) = StoreFixture::postgres() {
        reconciliation_concurrent(&f);
    }
}

fn reconciliation_actors(fixture: &StoreFixture) {
    let ctx = setup(fixture);
    let work = ctx.start("reconcile-auth", "src/auth", &[]);
    let update = durable_request(&ctx, &work.id, "auth");
    let (_, helper_secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &ctx.principal.owner.id,
        "reconcile-helper",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let helper = principal(&*ctx.store, helper_secret.expose());
    assert!(
        ctx.store
            .work_reconciliation_apply(&helper.token_id, &update, unix_now())
            .is_err()
    );
    let owner = user(&*ctx.store, 852, "foreign-reconcile");
    let (_, foreign_secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &owner,
        "foreign-reconcile",
        "acme/foreign",
        999,
        3600,
        false,
    )
    .unwrap();
    let foreign = principal(&*ctx.store, foreign_secret.expose());
    assert!(
        ctx.store
            .work_reconciliation_get(&foreign.token_id, &work.id, unix_now())
            .unwrap()
            .is_none()
    );
    assert!(
        ctx.store
            .work_reconciliation_apply(&foreign.token_id, &update, unix_now())
            .is_err()
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let a = {
        let barrier = barrier.clone();
        let store = ctx.store.clone();
        let token = ctx.token().to_owned();
        std::thread::spawn(move || {
            barrier.wait();
            store
                .work_reconciliation_apply(&token, &update, unix_now())
                .map(|r| r.receipt.event_seq)
        })
    };
    let b = {
        let barrier = barrier.clone();
        let store = ctx.store.clone();
        let token = ctx.token().to_owned();
        let request = WorkCheckpoint {
            work_id: work.id.clone(),
            operation_id: "concurrent-report".into(),
            generation: work.generation,
            session: work.lead_session.clone(),
            expected_updated_seq: Some(work.updated_seq),
            kind: CheckpointKind::Decision,
            summary: "new report must survive".into(),
            next_action: None,
            scope: None,
            branch: None,
            worktree: None,
            resolve_blocker: false,
            source_refs: vec![],
            depends_on: vec![],
            related_work: vec![],
        };
        std::thread::spawn(move || {
            barrier.wait();
            store
                .work_checkpoint(&token, &request, unix_now())
                .map(|r| r.receipt.event_seq)
        })
    };
    let results = [a.join().unwrap(), b.join().unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
}
#[test]
fn durable_reconciliation_sqlite_actors_checkpoint_race() {
    reconciliation_actors(&StoreFixture::sqlite());
}
#[test]
fn postgres_durable_reconciliation_actors_checkpoint_race() {
    if let Some(f) = StoreFixture::postgres() {
        reconciliation_actors(&f);
    }
}

/// Exercise the real publication-update projection, whose saved commit is not a provider head.
fn publication_head_provenance(fixture: &StoreFixture) {
    use agentscm::hub::publish::{Create, NewPublication, Update, derive_id};
    use agentscm::hub::work_reconciliation::{
        ConditionState, PrerequisiteCondition, PrerequisiteRequirement, WorkDeclarations,
        WorkDeclarationsUpdate,
    };
    let mut ctx = setup(fixture);
    let cases = [
        ("absent", None, false, false),
        ("legacy-proof", Some((HEAD_A, true)), false, false),
        ("wrong-repository", Some((HEAD_A, true)), false, false),
        ("actual-b", Some((HEAD_B, true)), false, false),
        ("actual-a", Some((HEAD_A, true)), false, false),
        ("wrong-merge", Some((HEAD_A, false)), false, false),
        ("malformed", Some(("short", true)), false, false),
        ("wrong-number", Some((HEAD_A, true)), true, false),
        ("ambiguous", Some((HEAD_A, true)), false, true),
    ];
    for (index, (name, observation, wrong_number, ambiguous)) in cases.into_iter().enumerate() {
        let number = 910 + index as i64;
        let branch = format!("publication-{number}");
        let merge_sha = format!("{number:040x}");
        let prerequisite = ctx.start(
            &format!("prereq-{name}"),
            &format!("src/prereq-{name}"),
            &[],
        );
        let work = ctx.start(
            &format!("work-{name}"),
            &format!("src/work-{name}"),
            &[&prerequisite],
        );
        ctx.link(&prerequisite, number, HEAD_A);
        let publication = NewPublication {
            id: derive_id(REPOSITORY, 3, HEAD_A, &branch),
            manifest: serde_json::from_value(json!({
                "revision":"a".repeat(64),"event_seq":3,"commit":HEAD_A,
                "tree":"c".repeat(40),"parent":"d".repeat(40),"branch":branch,
                "base":"main","title":"Synthetic publication","body":"","message":"message\n",
                "author":{"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
                "committer":{"name":"A","email":"a@example.invalid","date":"2026-10-25T00:00:00Z"},
                "changes":[{"path":"src/a.rs","mode":"100644","blob":"e".repeat(40),"sha256":"f".repeat(64),"size":5}]
            })).unwrap(),
        };
        assert!(matches!(
            ctx.store
                .create_publication(ctx.token(), &publication, unix_now())
                .unwrap(),
            Create::Done(_)
        ));
        for update in [
            Update::BranchAttempt,
            Update::BranchCreated,
            Update::PullRequest {
                number,
                node_id: format!("PR_{number}"),
                html_url: format!("https://github.com/acme/widgets/pull/{number}"),
            },
            Update::Merged {
                merge_commit: merge_sha.clone(),
            },
        ] {
            ctx.store
                .update_publication(&publication.id, &update, unix_now())
                .unwrap();
        }
        let rows = ctx.store.catchup_merges_for_pull(&scope(), number).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, CatchupMergeSource::TrackedPublication);
        assert_eq!(rows[0].head_commit, HEAD_A);
        // The immutable publication fact deduplicates subsequent provider merge ingestion.
        let later = NewCatchupMerge {
            source: CatchupMergeSource::ForgePull,
            source_id: format!("forge-{number}"),
            occurred_at: Some(10),
            title: None,
            branch: branch.clone(),
            base: "main".into(),
            head_commit: HEAD_B.into(),
            merge_commit: merge_sha.clone(),
            pull_number: number,
            paths: vec![],
            paths_complete: false,
            same_repository_head: true,
        };
        let retained = ctx
            .store
            .record_catchup_merge(&scope(), &later, 11)
            .unwrap();
        assert_eq!(retained.source, CatchupMergeSource::TrackedPublication);
        assert_eq!(retained.head_commit, HEAD_A);
        // A normal individual reread leaves the independent provider tombstone needed to qualify it.
        if let Some((head, same_merge)) = observation {
            ctx.clock += 1;
            let data = json!({"number":if wrong_number {number+1}else{number},"state":"closed","merged":true,
                "provider_number":if wrong_number {number+1}else{number},
                "provider_base_repository_id":if name=="wrong-repository" {REPOSITORY_ID+1}else{REPOSITORY_ID},
                "head_sha":head,"merge_commit_sha":if same_merge {merge_sha.clone()}else{"f".repeat(40)}});
            let mut data = data;
            if name == "legacy-proof" {
                data.as_object_mut().unwrap().remove("provider_number");
                data.as_object_mut()
                    .unwrap()
                    .remove("provider_base_repository_id");
            }
            let data = data.to_string();
            ctx.store
                .forget(
                    &ctx.repo.id,
                    "pull_request",
                    &number.to_string(),
                    ctx.clock,
                    &data,
                    "pull_request_closed",
                )
                .unwrap();
            if ambiguous {
                let conflicting =
                    json!({"number":number,"state":"open","head_sha":HEAD_A}).to_string();
                ctx.store
                    .observe(
                        &ctx.repo.id,
                        "pull_request",
                        &number.to_string(),
                        &conflicting,
                        ctx.clock,
                    )
                    .unwrap();
            }
        }
        let expected_a = if name == "actual-a" {
            ConditionState::Satisfied
        } else {
            ConditionState::Unknown
        };
        let expected_b = if name == "actual-b" {
            ConditionState::Satisfied
        } else {
            ConditionState::Unknown
        };
        let v1 = ctx.read(&work);
        assert_eq!(
            v1.prerequisites[0].state,
            if expected_a == ConditionState::Satisfied {
                PrerequisiteState::Satisfied
            } else {
                PrerequisiteState::Unknown
            },
            "{name}"
        );
        if name == "actual-b" {
            assert!(
                v1.prerequisites[0]
                    .limits
                    .iter()
                    .any(|l| l == "merge_head_changed")
            );
        }
        if name == "absent" {
            assert!(
                v1.prerequisites[0]
                    .limits
                    .iter()
                    .any(|l| l == "merge_head_unconfirmed")
            );
        }
        let item = ctx.item(&work.id);
        ctx.store
            .work_declarations_set(
                ctx.token(),
                &WorkDeclarationsUpdate {
                    work_id: item.id.clone(),
                    operation_id: format!("declare-{name}"),
                    generation: item.generation,
                    session: item.lead_session,
                    expected_updated_seq: item.updated_seq,
                    declarations: WorkDeclarations {
                        links: vec![],
                        prerequisites: [("published", HEAD_A), ("provider-b", HEAD_B)]
                            .into_iter()
                            .map(|(id, head)| PrerequisiteCondition {
                                id: id.into(),
                                requirement: PrerequisiteRequirement::MergedPullRequest {
                                    work_id: prerequisite.id.clone(),
                                    target: WorkPrLinkTarget {
                                        repository_id: REPOSITORY_ID,
                                        number,
                                        head_sha: head.into(),
                                    },
                                },
                            })
                            .collect(),
                    },
                },
                unix_now(),
            )
            .unwrap();
        let result = ctx
            .store
            .work_reconciliation_apply(
                ctx.token(),
                &durable_request(&ctx, &work.id, &format!("apply-{name}")),
                unix_now(),
            )
            .unwrap();
        let current = result.view.current.unwrap();
        for (id, expected) in [("published", expected_a), ("provider-b", expected_b)] {
            assert_eq!(
                current
                    .conditions
                    .iter()
                    .find(|c| c.id == id)
                    .unwrap()
                    .state,
                expected,
                "{name}/{id}"
            );
        }
        if name == "absent" {
            assert!(
                current.conditions[0]
                    .limits
                    .iter()
                    .any(|l| l == "merge_head_unconfirmed")
            );
        }
    }
}
#[test]
fn publication_heads_require_provider_confirmation_sqlite() {
    publication_head_provenance(&StoreFixture::sqlite());
}
#[test]
fn postgres_publication_heads_require_provider_confirmation() {
    if let Some(fixture) = StoreFixture::postgres() {
        publication_head_provenance(&fixture);
    }
}

fn behavior_dependencies_without_shared_paths(fixture: &StoreFixture) {
    use agentscm::hub::work_dependencies::WorkDependencyNoticeQuery;
    use agentscm::hub::work_reconciliation::{
        ConditionState, PrerequisiteCondition, PrerequisiteRequirement, WorkDeclarations,
        WorkDeclarationsUpdate,
    };
    let ctx = setup(fixture);
    let prerequisite = ctx.start("behavior-provider", "src/provider.rs", &[]);
    let proposal = start("behavior-consumer", "docs/consumer.md", &[&prerequisite]);
    let checked = ctx
        .store
        .work_preflight(ctx.token(), &proposal, unix_now())
        .unwrap();
    assert!(checked.scope_overlaps.is_empty());
    let conditions = &checked.dependencies.unwrap().conditions;
    assert_eq!(conditions.len(), 1);
    assert_eq!(conditions[0].disposition.state, ConditionState::Unknown);
    let admitted = ctx
        .store
        .work_start(ctx.token(), &proposal, unix_now())
        .unwrap();
    assert_eq!(
        admitted.preflight.unwrap().dependencies.unwrap().conditions[0]
            .disposition
            .state,
        ConditionState::Unknown
    );
    let work = admitted.item;
    let read = |session: &str, after| {
        ctx.store
            .work_dependency_notices(
                ctx.token(),
                &WorkDependencyNoticeQuery {
                    session: session.into(),
                    after,
                    through: None,
                    limit: 100,
                },
                unix_now(),
            )
            .unwrap()
    };
    let consumer = read(&work.lead_session, 0);
    let provider = read(&prerequisite.lead_session, 0);
    assert_eq!(consumer.notices.len(), 1);
    assert_eq!(consumer.notices[0].sequence, provider.notices[0].sequence);
    assert!(read("unrelated-session", 0).notices.is_empty());
    assert!(
        read(&work.lead_session, consumer.next_after)
            .notices
            .is_empty()
    );
    assert_eq!(
        read(&work.lead_session, 0).notices[0].sequence,
        consumer.notices[0].sequence
    );
    // Completion is an agent report, never proof of delivery of an unbound prerequisite.
    ctx.checkpoint(&prerequisite, CheckpointKind::Finish);
    let after_finish = read(&work.lead_session, 0);
    assert!(after_finish.notices[0].sequence > consumer.notices[0].sequence);
    assert_eq!(
        after_finish.notices[0].current.conditions[0]
            .disposition
            .state,
        ConditionState::Unknown
    );
    // An exact retained provider merge qualifies the existing structured condition.
    ctx.merge(321);
    let current = ctx.item(&work.id);
    let update = WorkDeclarationsUpdate {
        work_id: work.id.clone(),
        operation_id: "declare-behavior".into(),
        generation: current.generation,
        session: current.lead_session.clone(),
        expected_updated_seq: current.updated_seq,
        declarations: WorkDeclarations {
            links: vec![],
            prerequisites: vec![PrerequisiteCondition {
                id: "behavior-change".into(),
                requirement: PrerequisiteRequirement::MergedPullRequest {
                    work_id: prerequisite.id.clone(),
                    target: WorkPrLinkTarget {
                        repository_id: REPOSITORY_ID,
                        number: 321,
                        head_sha: HEAD_A.into(),
                    },
                },
            }],
        },
    };
    ctx.store
        .work_declarations_set(ctx.token(), &update, unix_now())
        .unwrap();
    let qualified = read(&work.lead_session, 0);
    assert_eq!(
        qualified.notices[0].current.conditions[0].disposition.state,
        ConditionState::Satisfied
    );
    let same = ctx
        .store
        .work_declarations_set(ctx.token(), &update, unix_now())
        .unwrap();
    assert!(same.receipt.replayed);
    assert_eq!(
        read(&work.lead_session, 0).notices[0].sequence,
        qualified.notices[0].sequence
    );
    let mut existing = proposal;
    existing.work_id = Some(work.id.clone());
    existing.request_key = None;
    let preflight = ctx
        .store
        .work_preflight(ctx.token(), &existing, unix_now())
        .unwrap();
    assert_eq!(
        preflight.dependencies.unwrap().conditions[0]
            .disposition
            .state,
        ConditionState::Satisfied
    );

    // Fresh reads expose provider changes separately from the retained notice; an explicit
    // reconciliation mutation records that evidence version for both leads.
    let mut changed = update.clone();
    changed.operation_id = "new-provider-head".into();
    changed.expected_updated_seq = ctx.item(&work.id).updated_seq;
    if let PrerequisiteRequirement::MergedPullRequest { target, .. } =
        &mut changed.declarations.prerequisites[0].requirement
    {
        target.number = 322;
    }
    ctx.store
        .work_declarations_set(ctx.token(), &changed, unix_now())
        .unwrap();
    let pending = read(&work.lead_session, 0).notices.remove(0);
    assert_eq!(
        pending.recorded.conditions[0].state,
        ConditionState::Unknown
    );
    ctx.merge(322);
    let fresh = read(&work.lead_session, 0).notices.remove(0);
    assert_eq!(fresh.sequence, pending.sequence);
    assert_eq!(fresh.recorded.conditions[0].state, ConditionState::Unknown);
    assert_eq!(
        fresh.current.conditions[0].disposition.state,
        ConditionState::Satisfied
    );
    let reconcile = durable_request(&ctx, &work.id, "record-provider-version");
    ctx.store
        .work_reconciliation_apply(ctx.token(), &reconcile, unix_now())
        .unwrap();
    let applied = read(&work.lead_session, 0).notices.remove(0);
    assert!(applied.sequence > fresh.sequence);
    assert_eq!(
        applied.recorded.conditions[0].state,
        ConditionState::Satisfied
    );
    ctx.store
        .work_reconciliation_apply(ctx.token(), &reconcile, unix_now())
        .unwrap();
    assert_eq!(
        read(&work.lead_session, 0).notices[0].sequence,
        applied.sequence
    );
}

fn dependency_routing_removal_and_legacy_checkpoint(fixture: &StoreFixture) {
    use agentscm::hub::work_dependencies::WorkDependencyNoticeQuery;
    use agentscm::hub::work_reconciliation::{WorkDeclarations, WorkDeclarationsUpdate};
    let ctx = setup(fixture);
    let prerequisite = ctx.start("routing-provider", "src/routing-provider", &[]);
    let work = ctx.start("routing-consumer", "src/routing-consumer", &[&prerequisite]);
    let read = |token: &str, session: &str| {
        ctx.store
            .work_dependency_notices(
                token,
                &WorkDependencyNoticeQuery {
                    session: session.into(),
                    after: 0,
                    through: None,
                    limit: 100,
                },
                unix_now(),
            )
            .unwrap()
    };
    let second = ctx.start(
        "routing-second-consumer",
        "docs/routing-second",
        &[&prerequisite],
    );
    let page = ctx
        .store
        .work_dependency_notices(
            ctx.token(),
            &WorkDependencyNoticeQuery {
                session: prerequisite.lead_session.clone(),
                after: 0,
                through: None,
                limit: 1,
            },
            unix_now(),
        )
        .unwrap();
    assert!(!page.complete);
    let next = ctx
        .store
        .work_dependency_notices(
            ctx.token(),
            &WorkDependencyNoticeQuery {
                session: prerequisite.lead_session.clone(),
                after: page.next_after,
                through: Some(page.through),
                limit: 1,
            },
            unix_now(),
        )
        .unwrap();
    assert!(next.complete);
    assert_ne!(
        page.notices[0].recorded.work_id,
        next.notices[0].recorded.work_id
    );
    // Remove the extra edge so subsequent routing assertions concern exactly one relationship.
    ctx.store
        .work_declarations_set(
            ctx.token(),
            &WorkDeclarationsUpdate {
                work_id: second.id.clone(),
                operation_id: "remove-extra".into(),
                generation: second.generation,
                session: second.lead_session.clone(),
                expected_updated_seq: second.updated_seq,
                declarations: WorkDeclarations::default(),
            },
            unix_now(),
        )
        .unwrap();
    ctx.checkpoint(&prerequisite, CheckpointKind::Handover);
    let mut takeover = start("routing-provider", "src/routing-provider", &[]);
    takeover.work_id = Some(prerequisite.id.clone());
    takeover.request_key = None;
    takeover.session = "replacement-lead".into();
    takeover.takeover_generation = Some(1);
    let replaced = ctx
        .store
        .work_start(ctx.token(), &takeover, unix_now())
        .unwrap();
    assert!(replaced.took_over);
    assert!(
        read(ctx.token(), &prerequisite.lead_session)
            .notices
            .is_empty()
    );
    assert_eq!(read(ctx.token(), "replacement-lead").notices.len(), 1);
    // A current lead credential carry redirects the mailbox; an old valid token is not enough.
    let (_, secret, _) = mint_bound_agent_token(
        &*ctx.store,
        &ctx.principal.owner.id,
        "replacement-agent",
        REPOSITORY,
        REPOSITORY_ID,
        3600,
        false,
    )
    .unwrap();
    let new = principal(&*ctx.store, secret.expose());
    let current = ctx.item(&prerequisite.id);
    let carry = WorkCheckpoint {
        work_id: current.id.clone(),
        operation_id: "carry-dependency-lead".into(),
        generation: current.generation,
        session: "replacement-lead".into(),
        expected_updated_seq: Some(current.updated_seq),
        kind: CheckpointKind::Progress,
        summary: "Continue the same dependency Work session".into(),
        next_action: None,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![],
        related_work: vec![],
    };
    let carried = ctx
        .store
        .work_checkpoint(&new.token_id, &carry, unix_now())
        .unwrap();
    assert_eq!(carried.item.lead_token, new.token_id);
    assert!(read(ctx.token(), "replacement-lead").notices.is_empty());
    assert_eq!(read(&new.token_id, "replacement-lead").notices.len(), 1);
    // Explicit replacement removes routing and reverse edges despite stale legacy hints.
    let item = ctx.item(&work.id);
    ctx.store
        .work_declarations_set(
            ctx.token(),
            &WorkDeclarationsUpdate {
                work_id: item.id.clone(),
                operation_id: "remove-behavior".into(),
                generation: item.generation,
                session: item.lead_session.clone(),
                expected_updated_seq: item.updated_seq,
                declarations: WorkDeclarations::default(),
            },
            unix_now(),
        )
        .unwrap();
    assert!(read(&new.token_id, "replacement-lead").notices.is_empty());
    let removed = read(ctx.token(), &work.lead_session);
    assert!(removed.notices[0].current.conditions.is_empty());
    // An unchanged stale compatibility hint cannot resurrect an explicit removal.
    ctx.checkpoint(&work, CheckpointKind::Progress);
    assert!(
        read(ctx.token(), &work.lead_session).notices[0]
            .current
            .conditions
            .is_empty()
    );
    // Omission changes only legacy hints. A later newly added hint declares a fresh unbound edge.
    let item = ctx.item(&work.id);
    let checkpoint = WorkCheckpoint {
        work_id: item.id.clone(),
        operation_id: "legacy-add".into(),
        generation: item.generation,
        session: item.lead_session.clone(),
        expected_updated_seq: Some(item.updated_seq),
        kind: CheckpointKind::Progress,
        summary: "Declare behavior dependency".into(),
        next_action: None,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: vec![],
        depends_on: vec![prerequisite.id.clone()],
        related_work: vec![],
    };
    let mut clearing = checkpoint.clone();
    clearing.operation_id = "legacy-clear-hints".into();
    clearing.depends_on.clear();
    let cleared = ctx
        .store
        .work_checkpoint(ctx.token(), &clearing, unix_now())
        .unwrap();
    let mut checkpoint = checkpoint;
    checkpoint.expected_updated_seq = Some(cleared.item.updated_seq);
    let added = ctx
        .store
        .work_checkpoint(ctx.token(), &checkpoint, unix_now())
        .unwrap();
    let mut omission = checkpoint;
    omission.operation_id = "legacy-omission".into();
    omission.expected_updated_seq = Some(added.item.updated_seq);
    omission.depends_on.clear();
    ctx.store
        .work_checkpoint(ctx.token(), &omission, unix_now())
        .unwrap();
    assert_eq!(
        read(ctx.token(), &work.lead_session).notices[0]
            .current
            .conditions
            .len(),
        1
    );
    assert_eq!(read(&new.token_id, "replacement-lead").notices.len(), 1);
}

#[test]
fn behavior_dependencies_sqlite_without_shared_paths() {
    behavior_dependencies_without_shared_paths(&StoreFixture::sqlite());
}
#[test]
fn postgres_behavior_dependencies_without_shared_paths() {
    if let Some(f) = StoreFixture::postgres() {
        behavior_dependencies_without_shared_paths(&f);
    }
}
#[test]
fn behavior_dependency_routing_sqlite() {
    dependency_routing_removal_and_legacy_checkpoint(&StoreFixture::sqlite());
}
#[test]
fn postgres_behavior_dependency_routing() {
    if let Some(f) = StoreFixture::postgres() {
        dependency_routing_removal_and_legacy_checkpoint(&f);
    }
}

fn dependency_fanout_does_not_block_cleanup(fixture: &StoreFixture) {
    use agentscm::hub::work_dependencies::WorkDependencyNoticeQuery;
    use agentscm::hub::work_transition::{
        WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest,
    };
    let ctx = setup(fixture);
    let provider = ctx.start("fanout-provider", "src/fanout-provider", &[]);
    // Released unfinished Work remains a declared dependent. More than one notification batch
    // must never stop the provider's existing finish/release operation.
    for index in 0..101 {
        let work = ctx.start(
            &format!("fanout-{index}"),
            &format!("src/fanout-{index}"),
            &[&provider],
        );
        ctx.store
            .work_transition(
                &WorkTransitionActor::Agent { token: ctx.token() },
                &WorkTransitionRequest {
                    work_id: work.id.clone(),
                    operation_id: "release-for-fanout".into(),
                    generation: work.generation,
                    expected_updated_seq: work.updated_seq,
                    session: Some(work.lead_session),
                    transition: WorkTransitionKind::Release,
                    reason: "Keep the declared dependency while releasing scope".into(),
                    superseded_by: None,
                },
                unix_now(),
            )
            .unwrap();
    }
    ctx.checkpoint(&provider, CheckpointKind::Finish);
    assert_eq!(
        ctx.item(&provider.id).state,
        agentscm::hub::work::WorkState::Finished
    );
    let page = ctx
        .store
        .work_dependency_notices(
            ctx.token(),
            &WorkDependencyNoticeQuery {
                session: provider.lead_session.clone(),
                after: 0,
                through: None,
                limit: 100,
            },
            unix_now(),
        )
        .unwrap();
    assert!(!page.coverage_complete);
    assert!(
        page.notices
            .iter()
            .any(|notice| !notice.recorded.notification_fanout_complete)
    );
}
#[test]
fn dependency_fanout_sqlite_preserves_cleanup() {
    dependency_fanout_does_not_block_cleanup(&StoreFixture::sqlite());
}
#[test]
fn postgres_dependency_fanout_preserves_cleanup() {
    if let Some(f) = StoreFixture::postgres() {
        dependency_fanout_does_not_block_cleanup(&f);
    }
}

fn dependency_summary_attempt_is_serialized(fixture: &StoreFixture) {
    use agentscm::hub::session::SessionCall;
    use agentscm::hub::work_dependencies::WorkDependencyNoticeQuery;
    let ctx = setup(fixture);
    let prerequisite = ctx.start("delivery-source", "src/source.rs", &[]);
    ctx.store
        .touch_session(
            ctx.token(),
            &SessionCall {
                session: Some(prerequisite.lead_session.clone()),
                via: "mcp".into(),
                task: None,
                paths: vec![],
                deny: false,
                report: false,
            },
            unix_now(),
        )
        .unwrap();
    let _dependent = ctx.start("delivery-dependent", "docs/dependent.md", &[&prerequisite]);
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let store = ctx.store.clone();
            let token = ctx.token().to_owned();
            let session = prerequisite.lead_session.clone();
            std::thread::spawn(move || {
                store
                    .work_dependency_notice_delivery(&token, &session, unix_now())
                    .unwrap()
            })
        })
        .collect();
    let pages: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].notices.len(), 1);
    assert!(
        ctx.store
            .work_dependency_notice_delivery(ctx.token(), &prerequisite.lead_session, unix_now())
            .unwrap()
            .is_none()
    );
    // Explicit history reads are independent of attempted advisory delivery.
    let page = ctx
        .store
        .work_dependency_notices(
            ctx.token(),
            &WorkDependencyNoticeQuery {
                session: prerequisite.lead_session,
                after: 0,
                through: None,
                limit: 20,
            },
            unix_now(),
        )
        .unwrap();
    assert_eq!(page.notices.len(), 1);
}

#[test]
fn dependency_summary_delivery_sqlite() {
    dependency_summary_attempt_is_serialized(&StoreFixture::sqlite());
}
#[test]
fn postgres_dependency_summary_delivery() {
    let Some(fixture) = StoreFixture::postgres() else {
        return;
    };
    dependency_summary_attempt_is_serialized(&fixture);
}

#[test]
fn work_reconcile_branch_inference() {
    run_both(branch_inference);
}
