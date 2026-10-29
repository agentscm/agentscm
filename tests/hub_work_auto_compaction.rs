//! The Hub's automatic stand-up compaction removes exactly what the operator command removes
//! at the automatic cutoff, with the same guards. Runs on SQLite and, when configured,
//! PostgreSQL.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, unix_now};
use agentscm::hub::work::{
    BriefingQuery, CheckpointKind, WorkCheckpoint, WorkCompaction, WorkRole, WorkStart,
};
use agentscm::hub::{HubError, HubState, WORK_AUTO_COMPACTION_AGE_SECONDS, compact_work_history};
use hub_pg::PgSchema;
use tempfile::TempDir;

const DAY: i64 = 86_400;

struct Db {
    url: String,
    pg: Option<PgSchema>,
    dir: TempDir,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
            pg: None,
            dir,
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            pg: Some(pg),
            dir: TempDir::new().unwrap(),
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    /// Rows of integer columns, for comparing retained history between databases.
    fn rows(&self, sql: &str) -> Vec<Vec<i64>> {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query(sql, &[])
                .unwrap()
                .iter()
                .map(|row| (0..row.len()).map(|i| row.get(i)).collect())
                .collect()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let width = statement.column_count();
            statement
                .query_map([], |row| (0..width).map(|i| row.get(i)).collect())
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
    }

    fn retained(&self) -> Vec<Vec<Vec<i64>>> {
        [
            "SELECT seq FROM hub_events ORDER BY seq",
            "SELECT event_seq FROM hub_work_versions ORDER BY event_seq",
            "SELECT event_seq FROM hub_work_checkpoint_ops ORDER BY event_seq",
            "SELECT event_floor_seq, snapshot_floor_seq, updated_at FROM hub_work_retention
             ORDER BY repository_id",
            "SELECT charged_bytes FROM hub_work_usage ORDER BY repository_id",
        ]
        .iter()
        .map(|sql| self.rows(sql))
        .collect()
    }
}

struct Agent {
    token: String,
}

fn agent(store: &dyn Store, user_id: i64, repository: &str, repository_id: i64) -> Agent {
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id,
            },
            &format!("user{user_id}"),
        )
        .unwrap();
    let (token, _, _) = mint_bound_agent_token(
        store,
        &owner.id,
        &format!("agent{user_id}"),
        repository,
        repository_id,
        3600,
        false,
    )
    .unwrap();
    Agent { token }
}

fn start(key: &str, session: &str) -> WorkStart {
    WorkStart {
        work_id: None,
        source: None,
        request_key: Some(key.into()),
        title: format!("auto compaction {key}"),
        session: session.into(),
        role: WorkRole::Lead,
        scope: vec!["src/**".into()],
        branch: None,
        worktree: None,
        next_action: None,
        depends_on: vec![],
        related_work: vec![],
        takeover_generation: None,
        preflight: None,
    }
}

/// Records checkpoints of one Work at each time and returns the event of the last.
fn history(store: &dyn Store, agent: &Agent, key: &str, started: i64, at: &[i64]) -> i64 {
    let session = format!("{key}-session");
    let item = store
        .work_start(&agent.token, &start(key, &session), started)
        .unwrap()
        .item;
    let mut updated_seq = item.updated_seq;
    let mut event_seq = 0;
    for (index, time) in at.iter().enumerate() {
        let result = store
            .work_checkpoint(
                &agent.token,
                &WorkCheckpoint {
                    work_id: item.id.clone(),
                    operation_id: format!("{key}-{index}"),
                    generation: item.generation,
                    session: session.clone(),
                    expected_updated_seq: Some(updated_seq),
                    kind: CheckpointKind::Progress,
                    summary: format!("progress {index}"),
                    next_action: None,
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: vec![],
                    related_work: vec![],
                },
                *time,
            )
            .unwrap();
        updated_seq = result.item.updated_seq;
        event_seq = result.receipt.event_seq;
    }
    event_seq
}

/// One repository with old, three-day-old and recent history; one whose briefing delivery is
/// still unacknowledged, which the compaction guard refuses.
fn fixture(db: &Db, now: i64) -> Arc<dyn Store> {
    let store = db.open();
    let lead = agent(&*store, 1, "acme/widgets", 101);
    history(
        &*store,
        &lead,
        "widgets",
        now - 30 * DAY,
        &[now - 30 * DAY + 1, now - 20 * DAY, now - 3 * DAY, now - 10],
    );
    let other = agent(&*store, 2, "acme/other", 102);
    history(&*store, &other, "other", now - 30 * DAY, &[now - 20 * DAY]);
    let delivery = store
        .work_briefing(&other.token, &BriefingQuery::default(), now)
        .unwrap()
        .delivery;
    assert!(delivery.is_some());
    store
}

/// The operator command, run for each scope with the automatic cutoff.
fn operator_compaction(store: &dyn Store, now: i64) -> (Vec<WorkCompaction>, usize) {
    let mut applied = Vec::new();
    let mut skipped = 0;
    for scope in store.work_compaction_scopes().unwrap() {
        match store.work_compact(&scope, now - WORK_AUTO_COMPACTION_AGE_SECONDS, true, now) {
            Ok(result) => applied.push(result),
            Err(HubError::Invalid(message)) => {
                assert!(message.contains("unacknowledged"), "{message}");
                skipped += 1;
            }
            Err(error) => panic!("{error}"),
        }
    }
    (applied, skipped)
}

fn automatic_compaction(db: &Db, store: Arc<dyn Store>, now: i64) -> (Vec<WorkCompaction>, usize) {
    let objects = LocalBackend::open(db.dir.path().join("objects")).unwrap();
    let state = HubState::new(store, Arc::new(objects));
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(compact_work_history(&state, now))
        .unwrap()
}

fn removes_what_the_operator_command_removes(cli: &Db, auto: &Db) {
    let now = unix_now();
    let cli_store = fixture(cli, now);
    let auto_store = fixture(auto, now);
    assert_eq!(cli.retained(), auto.retained());

    let expected = operator_compaction(&*cli_store, now);
    let actual = automatic_compaction(auto, auto_store.clone(), now);
    assert_eq!(actual, expected);
    assert_eq!(
        actual.1, 1,
        "the unacknowledged delivery blocks its repository"
    );
    assert_eq!(actual.0.len(), 1);
    assert!(actual.0[0].applied);
    assert_eq!(actual.0[0].scope.repository, "acme/widgets");
    assert_eq!(actual.0[0].deleted_operations, 2);
    assert_eq!(actual.0[0].deleted_versions, 2);
    assert_eq!(cli.retained(), auto.retained());

    // History younger than the automatic cutoff stays, though the operator may remove it.
    let widgets = &actual.0[0].scope;
    let shorter = auto_store
        .work_compact(widgets, now - DAY, false, now)
        .unwrap();
    assert_eq!(
        (shorter.deleted_operations, shorter.deleted_versions),
        (1, 1)
    );

    // A repeated pass finds nothing more to remove.
    let again = automatic_compaction(auto, auto_store, now);
    assert_eq!(again.1, 1);
    assert_eq!(
        (again.0[0].deleted_operations, again.0[0].deleted_versions),
        (0, 0)
    );
    assert_eq!(cli.retained(), auto.retained());
}

#[test]
fn sqlite_automatic_compaction_removes_what_the_operator_command_removes() {
    removes_what_the_operator_command_removes(&Db::sqlite(), &Db::sqlite());
}

#[test]
fn postgres_automatic_compaction_removes_what_the_operator_command_removes() {
    let (Some(cli), Some(auto)) = (Db::postgres(), Db::postgres()) else {
        return;
    };
    removes_what_the_operator_command_removes(&cli, &auto);
}
