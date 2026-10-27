//! Durable promise store parity and authenticated workspace authority.
#[path = "common/hub_pg.rs"]
mod hub_pg;
#[path = "common/workspace_hub.rs"]
mod workspace_hub;

use std::sync::{Arc, Barrier};
use std::thread;

use agentscm::hub::promise::{
    PromiseAdoptV1, PromiseChangeActionV1, PromiseChangeAuthority, PromiseChangeV1,
    PromiseConstraintV1, PromiseCreateV1, PromiseCriterionV1, PromiseCurrentAdmissionsV1,
    PromiseDefinitionV1, PromiseHistoryPageV1, PromiseInputPageV1, PromiseInputReceiptV1,
    PromiseInputV1, PromiseInputViewV1, PromiseMutableContentV1, PromisePageV1, PromiseReceiptV1,
    PromiseRepositoryAdmissionV1, PromiseRepositoryV1, PromiseVersionKindV1, PromiseViewV1,
    admissions_digest,
};
use agentscm::hub::store::{
    self, ForgeIdentity, NewAgentToken, PromiseInputScope, Store, agent_token_digest,
    mint_agent_token, unix_now,
};
use agentscm::hub::work::WorkRepositoryScope;
use agentscm::team::auth::generate_secret;
use hub_pg::PgSchema;
use reqwest::Method;
use serde_json::{Value, json};
use tempfile::TempDir;

const WORKSPACE: &str = "promise-workspace";
const REPO_ONE: &str = "synthetic/project";
const REPO_TWO: &str = "synthetic/other";

fn unbound(repositories: &[PromiseRepositoryV1]) -> Vec<PromiseRepositoryAdmissionV1> {
    let mut admissions = repositories
        .iter()
        .map(|repository| PromiseRepositoryAdmissionV1::unbound(&repository.repository_id))
        .collect::<Vec<_>>();
    admissions.sort_by(|a, b| a.repository_id.cmp(&b.repository_id));
    admissions
}

fn current() -> PromiseCurrentAdmissionsV1 {
    PromiseCurrentAdmissionsV1::new()
}

fn admission_digest(admissions: &[PromiseRepositoryAdmissionV1]) -> String {
    admissions_digest(WORKSPACE, admissions).unwrap()
}

fn unbound_digest(keys: &[&str]) -> String {
    let mut admissions = keys
        .iter()
        .map(|key| PromiseRepositoryAdmissionV1::unbound(key))
        .collect::<Vec<_>>();
    admissions.sort_by(|a, b| a.repository_id.cmp(&b.repository_id));
    admission_digest(&admissions)
}

/// Existing parity scenarios exercise unbound scopes. The explicit admission scenarios below
/// call Store directly so that mapping changes, cursors, and exact replay are checked separately.
trait UnboundPromiseStore {
    fn test_promise_create(
        &self,
        workspace: &str,
        actor: &str,
        request: &PromiseCreateV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1>;
    fn test_promise_adopt(
        &self,
        workspace: &str,
        actor: &str,
        request: &PromiseAdoptV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1>;
    fn test_promise_change(
        &self,
        workspace: &str,
        actor: &str,
        promise: &str,
        authority: PromiseChangeAuthority,
        request: &PromiseChangeV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1>;
    fn test_promise_change_replay(
        &self,
        workspace: &str,
        actor: &str,
        promise: &str,
        request: &PromiseChangeV1,
        now: i64,
    ) -> agentscm::hub::Result<Option<PromiseReceiptV1>>;
    fn test_promise_input(
        &self,
        workspace: &str,
        actor: &str,
        token: Option<&str>,
        request: &PromiseInputV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseInputReceiptV1>;
    fn test_promise_get(
        &self,
        workspace: &str,
        promise: &str,
    ) -> agentscm::hub::Result<Option<PromiseViewV1>>;
    fn test_promise_input_get(
        &self,
        workspace: &str,
        input: &str,
    ) -> agentscm::hub::Result<Option<PromiseInputViewV1>>;
    fn test_promise_inputs(
        &self,
        workspace: &str,
        kind: &str,
        scope: &str,
        after: Option<&str>,
        limit: usize,
    ) -> agentscm::hub::Result<PromiseInputPageV1>;
    fn test_promise_history(
        &self,
        workspace: &str,
        promise: &str,
        before: Option<u64>,
        limit: usize,
    ) -> agentscm::hub::Result<Option<PromiseHistoryPageV1>>;
    fn test_promise_list(
        &self,
        workspace: &str,
        permitted: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
    ) -> agentscm::hub::Result<PromisePageV1>;
}

impl UnboundPromiseStore for Arc<dyn Store> {
    fn test_promise_create(
        &self,
        workspace: &str,
        actor: &str,
        request: &PromiseCreateV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1> {
        self.as_ref().promise_create(
            workspace,
            actor,
            request,
            &unbound(&request.definition.repositories),
            now,
        )
    }
    fn test_promise_adopt(
        &self,
        workspace: &str,
        actor: &str,
        request: &PromiseAdoptV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1> {
        self.as_ref().promise_adopt(
            workspace,
            actor,
            request,
            &unbound(&request.definition.repositories),
            now,
        )
    }
    fn test_promise_change(
        &self,
        workspace: &str,
        actor: &str,
        promise: &str,
        authority: PromiseChangeAuthority,
        request: &PromiseChangeV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseReceiptV1> {
        let repositories = match &request.action {
            PromiseChangeActionV1::Amend { content }
            | PromiseChangeActionV1::Correct { content } => content.repositories.clone(),
            _ => {
                self.as_ref()
                    .promise_get(workspace, promise, None, &current())?
                    .ok_or_else(|| agentscm::hub::HubError::Invalid("promise_not_found".into()))?
                    .current
                    .definition
                    .repositories
            }
        };
        self.as_ref().promise_change(
            workspace,
            actor,
            promise,
            authority,
            request,
            &unbound(&repositories),
            &current(),
            now,
        )
    }
    fn test_promise_change_replay(
        &self,
        workspace: &str,
        actor: &str,
        promise: &str,
        request: &PromiseChangeV1,
        now: i64,
    ) -> agentscm::hub::Result<Option<PromiseReceiptV1>> {
        self.as_ref()
            .promise_change_replay(workspace, actor, promise, request, &current(), now)
    }
    fn test_promise_input(
        &self,
        workspace: &str,
        actor: &str,
        token: Option<&str>,
        request: &PromiseInputV1,
        now: i64,
    ) -> agentscm::hub::Result<PromiseInputReceiptV1> {
        self.as_ref().promise_input(
            workspace,
            actor,
            token,
            request,
            &PromiseRepositoryAdmissionV1::unbound(request.repository_id()),
            now,
        )
    }
    fn test_promise_get(
        &self,
        workspace: &str,
        promise: &str,
    ) -> agentscm::hub::Result<Option<PromiseViewV1>> {
        self.as_ref()
            .promise_get(workspace, promise, None, &current())
    }
    fn test_promise_input_get(
        &self,
        workspace: &str,
        input: &str,
    ) -> agentscm::hub::Result<Option<PromiseInputViewV1>> {
        self.as_ref()
            .promise_input_get(workspace, input, None, &current())
    }
    fn test_promise_inputs(
        &self,
        workspace: &str,
        kind: &str,
        scope: &str,
        after: Option<&str>,
        limit: usize,
    ) -> agentscm::hub::Result<PromiseInputPageV1> {
        let scope = match kind {
            "proposal" => PromiseInputScope::Proposal(scope),
            "report" => PromiseInputScope::Report(scope),
            _ => {
                return Err(agentscm::hub::HubError::Invalid(
                    "invalid promise input kind".into(),
                ));
            }
        };
        self.as_ref()
            .promise_inputs(workspace, scope, None, after, limit, &current())
    }
    fn test_promise_history(
        &self,
        workspace: &str,
        promise: &str,
        before: Option<u64>,
        limit: usize,
    ) -> agentscm::hub::Result<Option<PromiseHistoryPageV1>> {
        self.as_ref()
            .promise_history(workspace, promise, None, before, limit, &current())
    }
    fn test_promise_list(
        &self,
        workspace: &str,
        permitted: Option<&[String]>,
        after: Option<&str>,
        limit: usize,
    ) -> agentscm::hub::Result<PromisePageV1> {
        self.as_ref()
            .promise_list(workspace, permitted, after, limit, &current())
    }
}

struct Db {
    url: String,
    pg: Option<PgSchema>,
    _dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        Self {
            url: format!("sqlite://{}", dir.path().join("hub.sqlite").display()),
            pg: None,
            _dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            pg: Some(pg),
            _dir: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    fn scalar(&self, sql: &str) -> i64 {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(sql, &[])
                .unwrap()
                .get(0)
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        }
    }

    fn exec(&self, sql: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .batch_execute(sql)
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
    }

    fn set_promise_version_payload(&self, promise_id: &str, payload: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promise_versions SET payload=$1 WHERE promise_id=$2 AND version=1",
                    &[&payload, &promise_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promise_versions SET payload=?1 WHERE promise_id=?2 AND version=1",
                    rusqlite::params![payload, promise_id],
                )
                .unwrap();
        }
    }

    fn set_promise_version_scope(&self, promise_id: &str, scope: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promise_versions SET current_scope=($1::text)::jsonb WHERE promise_id=$2 AND version=1",
                    &[&scope, &promise_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promise_versions SET current_scope=?1 WHERE promise_id=?2 AND version=1",
                    rusqlite::params![scope, promise_id],
                )
                .unwrap();
        }
    }

    fn promise_version_scope(&self, promise_id: &str) -> String {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(
                    "SELECT current_scope::text FROM hub_promise_versions WHERE promise_id=$1 AND version=1",
                    &[&promise_id],
                )
                .unwrap()
                .get(0)
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .query_row(
                    "SELECT current_scope FROM hub_promise_versions WHERE promise_id=?1 AND version=1",
                    rusqlite::params![promise_id],
                    |row| row.get(0),
                )
                .unwrap()
        }
    }

    fn set_promise_lifetime_scope(&self, promise_id: &str, scope: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promises SET lifetime_scope=($1::text)::jsonb WHERE id=$2",
                    &[&scope, &promise_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promises SET lifetime_scope=?1 WHERE id=?2",
                    rusqlite::params![scope, promise_id],
                )
                .unwrap();
        }
    }

    fn delete_promise_repository_index(&self, promise_id: &str, repository: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "DELETE FROM hub_promise_version_repositories WHERE promise_id=$1 AND version=1 AND repository_id=$2",
                    &[&promise_id, &repository],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "DELETE FROM hub_promise_version_repositories WHERE promise_id=?1 AND version=1 AND repository_id=?2",
                    rusqlite::params![promise_id, repository],
                )
                .unwrap();
        }
    }

    fn set_promise_input_admission(&self, input_id: &str, admission: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promise_inputs SET admission=$1 WHERE id=$2",
                    &[&admission, &input_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promise_inputs SET admission=?1 WHERE id=?2",
                    rusqlite::params![admission, input_id],
                )
                .unwrap();
        }
    }

    fn set_promise_input_payload(&self, input_id: &str, payload: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promise_inputs SET payload=$1 WHERE id=$2",
                    &[&payload, &input_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promise_inputs SET payload=?1 WHERE id=?2",
                    rusqlite::params![payload, input_id],
                )
                .unwrap();
        }
    }

    fn set_promise_current_digest(&self, promise_id: &str, digest: &str) {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .execute(
                    "UPDATE hub_promises SET current_digest=$1 WHERE id=$2",
                    &[&digest, &promise_id],
                )
                .unwrap();
        } else {
            rusqlite::Connection::open(self.url.strip_prefix("sqlite://").unwrap())
                .unwrap()
                .execute(
                    "UPDATE hub_promises SET current_digest=?1 WHERE id=?2",
                    rusqlite::params![digest, promise_id],
                )
                .unwrap();
        }
    }
}

fn user(store: &dyn Store, id: i64) -> String {
    store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: id,
            },
            &format!("user{id}"),
        )
        .unwrap()
        .id
}

fn definition(repositories: &[&str], suffix: &str) -> PromiseDefinitionV1 {
    PromiseDefinitionV1 {
        original_request: "Existing customers keep their settings.".into(),
        commitment: format!("Preserve existing customer settings {suffix}"),
        criteria: vec![PromiseCriterionV1 {
            id: "settings_preserved".into(),
            text: "An existing customer's settings remain unchanged.".into(),
            evidence_requirements: vec![],
        }],
        constraints: vec![PromiseConstraintV1 {
            id: "no_rewrite".into(),
            text: "Do not rewrite existing settings.".into(),
        }],
        repositories: repositories
            .iter()
            .map(|repository| PromiseRepositoryV1 {
                repository_id: (*repository).into(),
                relevance: format!("{repository} owns part of the behavior."),
            })
            .collect(),
    }
}

fn content(repositories: &[&str], suffix: &str) -> PromiseMutableContentV1 {
    let definition = definition(repositories, suffix);
    PromiseMutableContentV1 {
        commitment: definition.commitment,
        criteria: definition.criteria,
        constraints: definition.constraints,
        repositories: definition.repositories,
    }
}

fn change(
    operation_id: &str,
    version: u64,
    digest: &str,
    action: PromiseChangeActionV1,
) -> PromiseChangeV1 {
    PromiseChangeV1 {
        operation_id: operation_id.into(),
        expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
        expected_version: version,
        expected_digest: digest.into(),
        reason: "Owner-confirmed lifecycle change.".into(),
        action,
    }
}

fn lifecycle_and_store_parity(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 1);
    let bob = user(&*store, 2);
    let carol = user(&*store, 3);
    let create = PromiseCreateV1 {
        operation_id: "create_01".into(),
        expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
        definition: definition(&[REPO_ONE, REPO_TWO], "v1"),
    };
    let first = store
        .test_promise_create(WORKSPACE, &alice, &create, 10)
        .unwrap();
    assert!(!first.replayed);
    assert!(
        store
            .test_promise_create(WORKSPACE, &alice, &create, 99)
            .unwrap()
            .replayed
    );
    let mut changed_body = create.clone();
    changed_body.definition.commitment.push_str(" changed");
    assert!(
        store
            .test_promise_create(WORKSPACE, &alice, &changed_body, 11)
            .unwrap_err()
            .to_string()
            .contains("idempotency")
    );

    let barrier = Arc::new(Barrier::new(3));
    let writes = ["amend_a", "amend_b"].map(|operation| {
        let store = store.clone();
        let barrier = barrier.clone();
        let alice = alice.clone();
        let promise = first.reference.promise_id.clone();
        let digest = first.reference.digest.clone();
        thread::spawn(move || {
            let request = change(
                operation,
                1,
                &digest,
                PromiseChangeActionV1::Amend {
                    content: content(&[REPO_ONE, REPO_TWO], operation),
                },
            );
            barrier.wait();
            store.test_promise_change(
                WORKSPACE,
                &alice,
                &promise,
                PromiseChangeAuthority::Steward,
                &request,
                20,
            )
        })
    });
    barrier.wait();
    let results = writes.map(|write| write.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);

    let mut current = store
        .test_promise_get(WORKSPACE, &first.reference.promise_id)
        .unwrap()
        .unwrap()
        .current;
    assert_eq!(current.reference.version, 2);
    assert_eq!(
        current.definition.original_request,
        create.definition.original_request
    );
    let winning_operation = if current.definition.commitment.ends_with("amend_a") {
        "amend_a"
    } else {
        "amend_b"
    };
    let winning_request = change(
        winning_operation,
        1,
        &first.reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_ONE, REPO_TWO], winning_operation),
        },
    );
    let correction = change(
        "correct_01",
        current.reference.version,
        &current.reference.digest,
        PromiseChangeActionV1::Correct {
            content: content(&[REPO_ONE, REPO_TWO], "corrected"),
        },
    );
    let corrected = store
        .test_promise_change(
            WORKSPACE,
            &alice,
            &current.reference.promise_id,
            PromiseChangeAuthority::Steward,
            &correction,
            30,
        )
        .unwrap()
        .reference;
    current = store
        .test_promise_get(WORKSPACE, &corrected.promise_id)
        .unwrap()
        .unwrap()
        .current;
    let replay = store
        .test_promise_change(
            WORKSPACE,
            &alice,
            &current.reference.promise_id,
            PromiseChangeAuthority::Steward,
            &winning_request,
            35,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.reference.version, 2);
    let recovered = store
        .test_promise_change_replay(
            WORKSPACE,
            &alice,
            &current.reference.promise_id,
            &winning_request,
            35,
        )
        .unwrap()
        .unwrap();
    assert!(recovered.replayed);
    assert_eq!(recovered.reference, replay.reference);
    let second = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "create_second".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
                definition: definition(&[REPO_ONE, REPO_TWO], "second"),
            },
            35,
        )
        .unwrap();
    assert!(
        store
            .test_promise_change_replay(
                WORKSPACE,
                &alice,
                &second.reference.promise_id,
                &winning_request,
                35,
            )
            .unwrap_err()
            .to_string()
            .contains("idempotency")
    );
    assert!(
        store
            .test_promise_change(
                WORKSPACE,
                &alice,
                &second.reference.promise_id,
                PromiseChangeAuthority::Steward,
                &winning_request,
                35,
            )
            .unwrap_err()
            .to_string()
            .contains("idempotency")
    );
    assert_eq!(
        store
            .test_promise_get(WORKSPACE, &second.reference.promise_id)
            .unwrap()
            .unwrap()
            .current
            .reference
            .version,
        1
    );
    let mut changed_replay = winning_request;
    changed_replay.reason.push_str(" changed");
    assert!(
        store
            .test_promise_change(
                WORKSPACE,
                &alice,
                &current.reference.promise_id,
                PromiseChangeAuthority::Steward,
                &changed_replay,
                36,
            )
            .is_err()
    );
    let transfer = change(
        "transfer_01",
        current.reference.version,
        &current.reference.digest,
        PromiseChangeActionV1::Transfer {
            to_user: bob.clone(),
        },
    );
    store
        .test_promise_change(
            WORKSPACE,
            &alice,
            &current.reference.promise_id,
            PromiseChangeAuthority::Steward,
            &transfer,
            40,
        )
        .unwrap();
    current = store
        .test_promise_get(WORKSPACE, &current.reference.promise_id)
        .unwrap()
        .unwrap()
        .current;
    assert_eq!(current.steward_user, bob);
    let recovery = change(
        "recovery_01",
        current.reference.version,
        &current.reference.digest,
        PromiseChangeActionV1::RecoveryTransfer {
            to_user: carol.clone(),
        },
    );
    store
        .test_promise_change(
            WORKSPACE,
            &alice,
            &current.reference.promise_id,
            PromiseChangeAuthority::RecoveryAdministrator,
            &recovery,
            50,
        )
        .unwrap();
    current = store
        .test_promise_get(WORKSPACE, &current.reference.promise_id)
        .unwrap()
        .unwrap()
        .current;
    assert_eq!(current.steward_user, carol);
    let retire = change(
        "retire_01",
        current.reference.version,
        &current.reference.digest,
        PromiseChangeActionV1::Retire,
    );
    store
        .test_promise_change(
            WORKSPACE,
            &carol,
            &current.reference.promise_id,
            PromiseChangeAuthority::Steward,
            &retire,
            60,
        )
        .unwrap();

    let history = store
        .test_promise_history(WORKSPACE, &current.reference.promise_id, None, 100)
        .unwrap()
        .unwrap();
    assert_eq!(history.versions.len(), 6);
    assert_eq!(history.versions[0].kind, PromiseVersionKindV1::Retire);
    assert_eq!(
        history.versions[1].kind,
        PromiseVersionKindV1::RecoveryTransfer
    );
    assert_eq!(history.versions[2].kind, PromiseVersionKindV1::Transfer);
    assert_eq!(history.versions[3].kind, PromiseVersionKindV1::Correct);
    assert_eq!(history.versions[5].kind, PromiseVersionKindV1::Create);
    assert!(history.versions.iter().all(|version| {
        version.definition.original_request == create.definition.original_request
    }));

    assert!(
        store
            .test_promise_list(WORKSPACE, Some(&[REPO_ONE.into()]), None, 20)
            .unwrap()
            .promises
            .is_empty()
    );
    assert_eq!(
        store
            .test_promise_list(
                WORKSPACE,
                Some(&[REPO_ONE.into(), REPO_TWO.into()]),
                None,
                20,
            )
            .unwrap()
            .promises
            .len(),
        2
    );

    store
        .work_compact(
            &WorkRepositoryScope {
                provider: "github".into(),
                host: "github.com".into(),
                repository: "acme/widgets".into(),
                repository_id: Some(1),
            },
            1,
            true,
            86_401,
        )
        .unwrap();
    assert_eq!(
        store
            .test_promise_history(WORKSPACE, &current.reference.promise_id, None, 100)
            .unwrap()
            .unwrap()
            .versions
            .len(),
        6
    );
    drop(store);
    assert_eq!(
        db.open()
            .test_promise_history(WORKSPACE, &current.reference.promise_id, None, 100)
            .unwrap()
            .unwrap()
            .versions
            .len(),
        6
    );
}

fn combined_original_and_mutable_definition_limit_is_atomic(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 114);
    let mut initial = definition(&[REPO_ONE], "large retained original");
    initial.original_request = "O".repeat(16 * 1024);
    initial.validate().unwrap();
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "large_original_initial".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: initial.clone(),
            },
            1,
        )
        .unwrap();
    let mut mutable = content(&[REPO_ONE], "large mutable body");
    mutable.criteria = (0..26)
        .map(|index| PromiseCriterionV1 {
            id: format!("large_criterion_{index}"),
            text: "C".repeat(1800),
            evidence_requirements: vec![],
        })
        .collect();
    mutable.constraints = (0..5)
        .map(|index| PromiseConstraintV1 {
            id: format!("large_constraint_{index}"),
            text: "N".repeat(1600),
        })
        .collect();
    mutable.validate().unwrap();
    assert!(serde_json::to_vec(&mutable).unwrap().len() < 64 * 1024);
    let combined = PromiseDefinitionV1 {
        original_request: initial.original_request,
        commitment: mutable.commitment.clone(),
        criteria: mutable.criteria.clone(),
        constraints: mutable.constraints.clone(),
        repositories: mutable.repositories.clone(),
    };
    assert!(serde_json::to_vec(&combined).unwrap().len() > 64 * 1024);
    let versions_before = db.scalar("SELECT COUNT(*) FROM hub_promise_versions");
    let operations_before = db.scalar("SELECT COUNT(*) FROM hub_promise_operations");
    for (operation_id, action) in [
        (
            "large_original_amend",
            PromiseChangeActionV1::Amend {
                content: mutable.clone(),
            },
        ),
        (
            "large_original_correct",
            PromiseChangeActionV1::Correct {
                content: mutable.clone(),
            },
        ),
    ] {
        let mut request = change(
            operation_id,
            created.reference.version,
            &created.reference.digest,
            action,
        );
        request.expected_admissions_digest = unbound_digest(&[REPO_ONE]);
        request.validate().unwrap();
        assert!(matches!(
            store.test_promise_change(
                WORKSPACE,
                &alice,
                &created.reference.promise_id,
                PromiseChangeAuthority::Steward,
                &request,
                2,
            ),
            Err(HubError::Invalid(_))
        ));
        assert_eq!(
            db.scalar("SELECT COUNT(*) FROM hub_promise_versions"),
            versions_before
        );
        assert_eq!(
            db.scalar("SELECT COUNT(*) FROM hub_promise_operations"),
            operations_before
        );
        assert_eq!(
            store
                .test_promise_get(WORKSPACE, &created.reference.promise_id)
                .unwrap()
                .unwrap()
                .current
                .reference,
            created.reference
        );
    }
}

fn proposal_report_and_rollback(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 11);
    let proposal = PromiseInputV1::Proposal {
        operation_id: "proposal_01".into(),
        repository_id: REPO_ONE.into(),
        original_request: "Keep account settings.".into(),
        commitment: "Keep them.".into(),
        criteria: definition(&[REPO_ONE], "proposal").criteria,
        constraints: vec![],
    };
    let proposed = store
        .test_promise_input(WORKSPACE, &alice, None, &proposal, 1)
        .unwrap();
    let preview = store
        .test_promise_input_get(WORKSPACE, &proposed.input_id)
        .unwrap()
        .unwrap();
    assert_eq!(preview.payload, proposal);
    assert!(!preview.submitted_by_agent);
    assert_eq!(preview.actor_user, alice);
    let proposal_page = store
        .test_promise_inputs(WORKSPACE, "proposal", REPO_ONE, None, 1)
        .unwrap();
    assert_eq!(proposal_page.inputs.len(), 1);
    assert_eq!(proposal_page.inputs[0].input_id, proposed.input_id);
    assert!(
        store
            .test_promise_inputs(WORKSPACE, "proposal", REPO_TWO, Some(&proposed.input_id), 1)
            .is_err()
    );
    assert!(
        store
            .test_promise_input(WORKSPACE, &alice, None, &proposal, 2)
            .unwrap()
            .replayed
    );
    let adopted = store
        .test_promise_adopt(
            WORKSPACE,
            &alice,
            &agentscm::hub::promise::PromiseAdoptV1 {
                operation_id: "adopt_01".into(),
                proposal_id: proposed.input_id.clone(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "adopted"),
            },
            3,
        )
        .unwrap();
    assert_eq!(
        store
            .test_promise_input_get(WORKSPACE, &proposed.input_id)
            .unwrap()
            .unwrap()
            .adopted_promise,
        Some(adopted.reference.promise_id.clone())
    );
    let report = PromiseInputV1::Report {
        operation_id: "report_01".into(),
        promise_id: adopted.reference.promise_id.clone(),
        promise_version: adopted.reference.version,
        promise_digest: adopted.reference.digest.clone(),
        repository_id: REPO_ONE.into(),
        summary: "A scoped caller observed a possible regression; this is not evidence.".into(),
    };
    let reported = store
        .test_promise_input(WORKSPACE, &alice, None, &report, 4)
        .unwrap();
    let report_page = store
        .test_promise_inputs(WORKSPACE, "report", &adopted.reference.promise_id, None, 1)
        .unwrap();
    assert_eq!(report_page.inputs[0].input_id, reported.input_id);
    let mut stale = report;
    if let PromiseInputV1::Report { promise_digest, .. } = &mut stale {
        *promise_digest = "f".repeat(64);
    }
    assert!(
        store
            .test_promise_input(WORKSPACE, &alice, None, &stale, 5)
            .is_err()
    );
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_inputs"), 2);
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_operations"), 3);

    store.set_user_disabled(&alice, true).unwrap();
    assert!(
        store
            .test_promise_input(WORKSPACE, &alice, None, &proposal, 6)
            .is_err()
    );
    let mut disabled_proposal = proposal.clone();
    match &mut disabled_proposal {
        PromiseInputV1::Proposal { operation_id, .. } => {
            *operation_id = "proposal_disabled".into();
        }
        PromiseInputV1::Report { .. } => unreachable!("the fixture is a proposal"),
    }
    assert!(
        store
            .test_promise_input(WORKSPACE, &alice, None, &disabled_proposal, 7)
            .is_err()
    );
    assert!(
        store
            .test_promise_history(WORKSPACE, &adopted.reference.promise_id, None, 10)
            .unwrap()
            .is_some()
    );
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_inputs"), 2);
}

fn migration_upgrades_a_populated_store(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 91);
    drop(store);
    if db.pg.is_some() {
        db.exec(
            "DROP TABLE hub_promise_operations;
             DROP TABLE hub_promise_inputs;
             DROP TABLE hub_promise_version_repositories;
             DROP TABLE hub_promise_versions;
             DROP TABLE hub_promises;
             DROP FUNCTION hub_promise_immutable();
             DELETE FROM hub_named_migrations WHERE name='living_promises';",
        );
    } else {
        db.exec(
            "DROP TRIGGER hub_promise_versions_no_update;
             DROP TRIGGER hub_promise_versions_no_delete;
             DROP TRIGGER hub_promise_repositories_no_update;
             DROP TRIGGER hub_promise_repositories_no_delete;
             DROP TABLE hub_promise_operations;
             DROP TABLE hub_promise_inputs;
             DROP TABLE hub_promise_version_repositories;
             DROP TABLE hub_promise_versions;
             DROP TABLE hub_promises;
             DELETE FROM hub_named_migrations WHERE name='living_promises';",
        );
    }
    let store = db.open();
    assert!(store.user_by_id(&alice).unwrap().is_some());
    store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "after_migration".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "after migration"),
            },
            1,
        )
        .unwrap();
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promises"), 1);
    assert_eq!(
        db.scalar("SELECT count(*) FROM hub_named_migrations WHERE name='living_promises'"),
        1
    );
}

fn input_pagination_and_scope(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 81);
    let proposal = |operation: &str| PromiseInputV1::Proposal {
        operation_id: operation.into(),
        repository_id: REPO_ONE.into(),
        original_request: "Keep the current settings.".into(),
        commitment: "Keep settings.".into(),
        criteria: definition(&[REPO_ONE], operation).criteria,
        constraints: vec![],
    };
    let first = store
        .test_promise_input(WORKSPACE, &alice, None, &proposal("page_one"), 1)
        .unwrap();
    let second = store
        .test_promise_input(WORKSPACE, &alice, None, &proposal("page_two"), 2)
        .unwrap();
    let first_page = store
        .test_promise_inputs(WORKSPACE, "proposal", REPO_ONE, None, 1)
        .unwrap();
    assert_eq!(first_page.inputs[0].input_id, first.input_id);
    assert_eq!(first_page.next.as_deref(), Some(first.input_id.as_str()));
    let second_page = store
        .test_promise_inputs(
            WORKSPACE,
            "proposal",
            REPO_ONE,
            first_page.next.as_deref(),
            1,
        )
        .unwrap();
    assert_eq!(second_page.inputs[0].input_id, second.input_id);
    assert!(second_page.next.is_none());
    assert!(
        store
            .test_promise_inputs(WORKSPACE, "proposal", REPO_TWO, Some(&first.input_id), 1)
            .is_err()
    );
    assert!(
        store
            .test_promise_inputs(WORKSPACE, "report", REPO_ONE, Some(&first.input_id), 1)
            .is_err()
    );
    assert_eq!(
        store
            .test_promise_input_get(WORKSPACE, &second.input_id)
            .unwrap()
            .unwrap()
            .payload,
        proposal("page_two")
    );
}

fn concurrent_identical_operation_replays_one_receipt(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 92);
    let request = PromiseCreateV1 {
        operation_id: "concurrent_create".into(),
        expected_admissions_digest: unbound_digest(&[REPO_ONE]),
        definition: definition(&[REPO_ONE], "concurrent"),
    };
    let barrier = Arc::new(Barrier::new(3));
    let calls = [(), ()].map(|()| {
        let store = store.clone();
        let alice = alice.clone();
        let request = request.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            store.test_promise_create(WORKSPACE, &alice, &request, 1)
        })
    });
    barrier.wait();
    let receipts = calls.map(|call| call.join().unwrap().unwrap());
    assert_eq!(receipts[0].reference, receipts[1].reference);
    assert_eq!(
        receipts.iter().filter(|receipt| receipt.replayed).count(),
        1
    );
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promises"), 1);
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_versions"), 1);
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_operations"), 1);
}

fn admission_remap_fences_versions_inputs_cursors_and_replay(db: &Db) {
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 93);
    let admitted = |numeric| PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a = admitted(101);
    let b = admitted(202);
    let map = |admission: PromiseRepositoryAdmissionV1| {
        PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), admission)])
    };
    let create = PromiseCreateV1 {
        operation_id: "admitted_create".into(),
        expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
        definition: definition(&[REPO_ONE], "bound"),
    };
    let uncommitted = PromiseCreateV1 {
        operation_id: "admitted_no_receipt".into(),
        expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
        definition: definition(&[REPO_ONE], "uncommitted"),
    };
    let before = db.scalar("SELECT count(*) FROM hub_promises");
    assert!(
        store
            .promise_create(WORKSPACE, &alice, &uncommitted, std::slice::from_ref(&b), 1)
            .is_err()
    );
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promises"), before);
    let first_attempt = store
        .promise_create(WORKSPACE, &alice, &uncommitted, std::slice::from_ref(&a), 1)
        .unwrap();
    assert!(!first_attempt.replayed);
    assert!(
        store
            .promise_create(WORKSPACE, &alice, &uncommitted, std::slice::from_ref(&b), 2)
            .is_err()
    );
    let exact_retry = store
        .promise_create(WORKSPACE, &alice, &uncommitted, std::slice::from_ref(&a), 2)
        .unwrap();
    assert!(exact_retry.replayed);
    assert_eq!(exact_retry.reference, first_attempt.reference);
    let created = store
        .promise_create(WORKSPACE, &alice, &create, std::slice::from_ref(&a), 1)
        .unwrap();
    let id = &created.reference.promise_id;
    assert_eq!(
        store
            .promise_get(WORKSPACE, id, None, &map(a.clone()))
            .unwrap()
            .unwrap()
            .current
            .repository_admissions,
        vec![a.clone()]
    );
    assert!(
        store
            .promise_get(WORKSPACE, id, None, &map(b.clone()))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, None, None, 10, &map(b.clone()))
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(
        store
            .promise_history(WORKSPACE, id, None, Some(1), 10, &map(b.clone()))
            .is_err()
    );
    assert!(
        store
            .promise_list(WORKSPACE, None, Some(id), 10, &map(b.clone()))
            .is_err()
    );
    assert!(
        store
            .promise_create(WORKSPACE, &alice, &create, std::slice::from_ref(&b), 2)
            .is_err()
    );
    assert!(
        store
            .promise_create(WORKSPACE, &alice, &create, std::slice::from_ref(&a), 2)
            .unwrap()
            .replayed
    );

    let mut amend = change(
        "admitted_amend",
        1,
        &created.reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_ONE], "bound amended"),
        },
    );
    amend.expected_admissions_digest = admission_digest(std::slice::from_ref(&a));
    assert!(
        store
            .promise_change(
                WORKSPACE,
                &alice,
                id,
                PromiseChangeAuthority::Steward,
                &amend,
                std::slice::from_ref(&a),
                &map(b.clone()),
                2,
            )
            .is_err()
    );
    let amended = store
        .promise_change(
            WORKSPACE,
            &alice,
            id,
            PromiseChangeAuthority::Steward,
            &amend,
            std::slice::from_ref(&a),
            &map(a.clone()),
            2,
        )
        .unwrap();
    assert!(
        store
            .promise_change_replay(WORKSPACE, &alice, id, &amend, &map(b.clone()), 3)
            .is_err()
    );
    assert_eq!(
        store
            .promise_change_replay(WORKSPACE, &alice, id, &amend, &map(a.clone()), 3)
            .unwrap()
            .unwrap()
            .reference,
        amended.reference
    );

    let proposal = PromiseInputV1::Proposal {
        operation_id: "admitted_proposal".into(),
        repository_id: REPO_ONE.into(),
        original_request: "Keep the settings.".into(),
        commitment: "Keep settings.".into(),
        criteria: definition(&[REPO_ONE], "proposal").criteria,
        constraints: vec![],
    };
    let input = store
        .promise_input(WORKSPACE, &alice, None, &proposal, &a, 3)
        .unwrap();
    assert!(
        store
            .promise_input_get(WORKSPACE, &input.input_id, None, &map(b.clone()))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Proposal(REPO_ONE),
                None,
                None,
                10,
                &map(b.clone())
            )
            .unwrap()
            .inputs
            .is_empty()
    );
    assert!(
        store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Proposal(REPO_ONE),
                None,
                Some(&input.input_id),
                10,
                &map(b.clone())
            )
            .is_err()
    );
    assert!(
        store
            .promise_input(WORKSPACE, &alice, None, &proposal, &b, 4)
            .is_err()
    );
    assert!(
        store
            .promise_input(WORKSPACE, &alice, None, &proposal, &a, 4)
            .unwrap()
            .replayed
    );
    assert!(
        store
            .promise_input_get(WORKSPACE, &input.input_id, None, &map(a.clone()))
            .unwrap()
            .is_some()
    );

    let native = PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: None,
        native_repository_ids: vec!["native-a".into()],
    };
    let native_request = PromiseCreateV1 {
        operation_id: "native_create".into(),
        expected_admissions_digest: admission_digest(std::slice::from_ref(&native)),
        definition: definition(&[REPO_ONE], "native"),
    };
    let native_receipt = store
        .promise_create(
            WORKSPACE,
            &alice,
            &native_request,
            std::slice::from_ref(&native),
            5,
        )
        .unwrap();
    let changed_native = PromiseRepositoryAdmissionV1 {
        native_repository_ids: vec!["native-b".into()],
        ..native.clone()
    };
    assert!(
        store
            .promise_get(
                WORKSPACE,
                &native_receipt.reference.promise_id,
                None,
                &map(changed_native)
            )
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_get(
                WORKSPACE,
                &native_receipt.reference.promise_id,
                None,
                &map(native)
            )
            .unwrap()
            .is_some()
    );
}

fn admission_pages_skip_hidden_rows_without_exposing_cursors(db: &Db) {
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 94);
    let admitted = |key: &str, numeric| PromiseRepositoryAdmissionV1 {
        repository_id: key.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a = admitted(REPO_ONE, 101);
    let b = admitted(REPO_ONE, 202);
    let other = admitted(REPO_TWO, 303);
    let map = |first: PromiseRepositoryAdmissionV1| {
        PromiseCurrentAdmissionsV1::from([
            (REPO_ONE.into(), first),
            (REPO_TWO.into(), other.clone()),
        ])
    };

    let mut hidden_ids = Vec::new();
    for index in 0..12 {
        let request = PromiseCreateV1 {
            operation_id: format!("hidden_list_{index}"),
            expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
            definition: definition(&[REPO_ONE], "A"),
        };
        hidden_ids.push(
            store
                .promise_create(
                    WORKSPACE,
                    &alice,
                    &request,
                    std::slice::from_ref(&a),
                    index + 1,
                )
                .unwrap()
                .reference
                .promise_id,
        );
    }
    let mut visible_ids = Vec::new();
    for index in 0..2 {
        let request = PromiseCreateV1 {
            operation_id: format!("visible_list_{index}"),
            expected_admissions_digest: admission_digest(std::slice::from_ref(&b)),
            definition: definition(&[REPO_ONE], "B"),
        };
        visible_ids.push(
            store
                .promise_create(
                    WORKSPACE,
                    &alice,
                    &request,
                    std::slice::from_ref(&b),
                    index + 20,
                )
                .unwrap()
                .reference
                .promise_id,
        );
    }
    visible_ids.sort();
    let first = store
        .promise_list(WORKSPACE, None, None, 1, &map(b.clone()))
        .unwrap();
    assert_eq!(
        first.promises[0].current.reference.promise_id,
        visible_ids[0]
    );
    let cursor = first.next.as_deref().unwrap();
    assert_eq!(cursor, visible_ids[0]);
    let second = store
        .promise_list(WORKSPACE, None, Some(cursor), 1, &map(b.clone()))
        .unwrap();
    assert_eq!(
        second.promises[0].current.reference.promise_id,
        visible_ids[1]
    );
    assert!(second.next.is_none());
    assert!(
        store
            .promise_list(WORKSPACE, None, Some(&hidden_ids[0]), 1, &map(b.clone()))
            .is_err()
    );
    let restored = store
        .promise_list(WORKSPACE, None, None, 20, &map(a.clone()))
        .unwrap();
    assert_eq!(restored.promises.len(), hidden_ids.len());
    assert!(restored.next.is_none());

    let proposal = |operation: String| PromiseInputV1::Proposal {
        operation_id: operation,
        repository_id: REPO_ONE.into(),
        original_request: "Keep the settings.".into(),
        commitment: "Keep settings.".into(),
        criteria: definition(&[REPO_ONE], "input").criteria,
        constraints: vec![],
    };
    let mut hidden_inputs = Vec::new();
    for index in 0..12 {
        hidden_inputs.push(
            store
                .promise_input(
                    WORKSPACE,
                    &alice,
                    None,
                    &proposal(format!("hidden_input_{index}")),
                    &a,
                    index + 1,
                )
                .unwrap()
                .input_id,
        );
    }
    let mut visible_inputs = Vec::new();
    for index in 0..2 {
        visible_inputs.push(
            store
                .promise_input(
                    WORKSPACE,
                    &alice,
                    None,
                    &proposal(format!("visible_input_{index}")),
                    &b,
                    index + 20,
                )
                .unwrap()
                .input_id,
        );
    }
    let first = store
        .promise_inputs(
            WORKSPACE,
            PromiseInputScope::Proposal(REPO_ONE),
            None,
            None,
            1,
            &map(b.clone()),
        )
        .unwrap();
    assert_eq!(first.inputs[0].input_id, visible_inputs[0]);
    let cursor = first.next.as_deref().unwrap();
    let second = store
        .promise_inputs(
            WORKSPACE,
            PromiseInputScope::Proposal(REPO_ONE),
            None,
            Some(cursor),
            1,
            &map(b.clone()),
        )
        .unwrap();
    assert_eq!(second.inputs[0].input_id, visible_inputs[1]);
    assert!(second.next.is_none());
    assert!(
        store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Proposal(REPO_ONE),
                None,
                Some(&hidden_inputs[0]),
                1,
                &map(b.clone())
            )
            .is_err()
    );
    assert_eq!(
        store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Proposal(REPO_ONE),
                None,
                None,
                20,
                &map(a.clone())
            )
            .unwrap()
            .inputs
            .len(),
        hidden_inputs.len()
    );

    let initial = PromiseCreateV1 {
        operation_id: "mixed_history_initial".into(),
        expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
        definition: definition(&[REPO_ONE], "history 1"),
    };
    let mut reference = store
        .promise_create(WORKSPACE, &alice, &initial, std::slice::from_ref(&a), 100)
        .unwrap()
        .reference;
    for version in 2..=5 {
        let (key, admission) = if version % 2 == 0 {
            (REPO_TWO, other.clone())
        } else {
            (REPO_ONE, a.clone())
        };
        let mut action = change(
            &format!("mixed_history_{version}"),
            reference.version,
            &reference.digest,
            PromiseChangeActionV1::Amend {
                content: content(&[key], &format!("history {version}")),
            },
        );
        action.expected_admissions_digest = admission_digest(std::slice::from_ref(&admission));
        reference = store
            .promise_change(
                WORKSPACE,
                &alice,
                &reference.promise_id,
                PromiseChangeAuthority::Steward,
                &action,
                &[admission],
                &map(a.clone()),
                100 + version as i64,
            )
            .unwrap()
            .reference;
    }
    assert!(
        store
            .promise_history(
                WORKSPACE,
                &reference.promise_id,
                None,
                None,
                1,
                &map(b.clone())
            )
            .unwrap()
            .is_none()
    );
    let first = store
        .promise_history(
            WORKSPACE,
            &reference.promise_id,
            None,
            None,
            1,
            &map(a.clone()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.versions[0].reference.version, 5);
    assert_eq!(first.next, Some(5));
    let second = store
        .promise_history(
            WORKSPACE,
            &reference.promise_id,
            None,
            first.next,
            1,
            &map(a.clone()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.versions[0].reference.version, 4);
    assert_eq!(second.next, Some(4));
    assert!(
        store
            .promise_history(
                WORKSPACE,
                &reference.promise_id,
                None,
                Some(5),
                1,
                &map(b.clone())
            )
            .is_err()
    );
    assert_eq!(
        store
            .promise_history(
                WORKSPACE,
                &reference.promise_id,
                None,
                None,
                10,
                &map(a.clone())
            )
            .unwrap()
            .unwrap()
            .versions
            .len(),
        5
    );
}

fn hidden_corrupt_cursors_match_missing_and_visible_corruption_surfaces(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 95);
    let admitted = |key: &str, numeric| PromiseRepositoryAdmissionV1 {
        repository_id: key.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a_one = admitted(REPO_ONE, 101);
    let a_two = admitted(REPO_TWO, 303);
    let map_a = PromiseCurrentAdmissionsV1::from([
        (REPO_ONE.into(), a_one.clone()),
        (REPO_TWO.into(), a_two.clone()),
    ]);
    let map_b = PromiseCurrentAdmissionsV1::from([
        (REPO_ONE.into(), admitted(REPO_ONE, 202)),
        (REPO_TWO.into(), admitted(REPO_TWO, 404)),
    ]);
    let mut corrupt_ids = Vec::new();
    for (index, suffix) in ["malformed", "wrong_digest", "wrong_pointer"]
        .iter()
        .enumerate()
    {
        let receipt = store
            .promise_create(
                WORKSPACE,
                &alice,
                &PromiseCreateV1 {
                    operation_id: format!("corrupt_cursor_{suffix}"),
                    expected_admissions_digest: admission_digest(std::slice::from_ref(&a_one)),
                    definition: definition(&[REPO_ONE], suffix),
                },
                std::slice::from_ref(&a_one),
                index as i64 + 1,
            )
            .unwrap();
        corrupt_ids.push(receipt.reference.promise_id);
    }
    let valid = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "restored_cursor_control".into(),
                expected_admissions_digest: admission_digest(std::slice::from_ref(&a_two)),
                definition: definition(&[REPO_TWO], "valid"),
            },
            &[a_two],
            4,
        )
        .unwrap();
    let original = store
        .promise_get(WORKSPACE, &corrupt_ids[1], None, &map_a)
        .unwrap()
        .unwrap();
    let mut wrong_digest = serde_json::to_value(&original.current).unwrap();
    wrong_digest["definition"]["commitment"] = json!("A valid JSON body with the wrong digest");
    let wrong_digest = serde_json::to_string(&wrong_digest).unwrap();

    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let original_target = store
        .promise_get(WORKSPACE, &corrupt_ids[0], None, &map_a)
        .unwrap()
        .unwrap()
        .current;
    db.set_promise_version_payload(
        &corrupt_ids[0],
        &serde_json::to_string(&original.current).unwrap(),
    );
    assert!(
        store
            .promise_get(WORKSPACE, &corrupt_ids[0], None, &map_a)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, None, None, 20, &map_a)
            .unwrap()
            .promises
            .iter()
            .all(|view| view.current.reference.promise_id != corrupt_ids[0])
    );
    assert!(matches!(
        store.promise_list(WORKSPACE, None, Some(&corrupt_ids[0]), 1, &map_a),
        Err(HubError::Invalid(_))
    ));
    db.set_promise_version_payload(
        &corrupt_ids[0],
        &serde_json::to_string(&original_target).unwrap(),
    );
    db.set_promise_current_digest(&corrupt_ids[2], &"0".repeat(64));
    assert!(matches!(
        store.promise_list(WORKSPACE, None, None, 20, &map_a),
        Err(HubError::Unavailable(_))
    ));
    let mut malformed_definition = serde_json::to_value(&original_target).unwrap();
    malformed_definition["definition"] = json!("not a promise definition");
    db.set_promise_version_payload(&corrupt_ids[0], &malformed_definition.to_string());
    db.set_promise_version_payload(&corrupt_ids[1], &wrong_digest);

    for (index, id) in corrupt_ids.iter().enumerate() {
        assert!(
            store
                .promise_get(WORKSPACE, id, None, &map_b)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .promise_get(WORKSPACE, id, Some(&[REPO_TWO.into()]), &map_a)
                .unwrap()
                .is_none()
        );
        if index < 2 {
            assert!(
                store
                    .promise_get(WORKSPACE, id, None, &map_a)
                    .unwrap()
                    .is_none()
            );
        } else {
            assert!(matches!(
                store.promise_get(WORKSPACE, id, None, &map_a),
                Err(HubError::Unavailable(_))
            ));
        }
        assert!(matches!(
            store.promise_list(WORKSPACE, None, Some(id), 1, &map_b),
            Err(HubError::Invalid(_))
        ));
        assert!(matches!(
            store.promise_list(WORKSPACE, Some(&[REPO_TWO.into()]), Some(id), 1, &map_a),
            Err(HubError::Invalid(_))
        ));
        if index < 2 {
            assert!(matches!(
                store.promise_list(WORKSPACE, None, Some(id), 1, &map_a),
                Err(HubError::Invalid(_))
            ));
        } else {
            assert!(matches!(
                store.promise_list(WORKSPACE, None, Some(id), 1, &map_a),
                Err(HubError::Unavailable(_))
            ));
        }
    }
    assert!(matches!(
        store.promise_list(WORKSPACE, None, Some("prm_missing"), 1, &map_b),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .promise_get(WORKSPACE, "prm_missing", None, &map_b)
            .unwrap()
            .is_none()
    );
    for id in &corrupt_ids[..2] {
        assert!(matches!(
            store.promise_history(WORKSPACE, id, None, Some(1), 1, &map_b),
            Err(HubError::Invalid(_))
        ));
        assert!(matches!(
            store.promise_history(WORKSPACE, id, None, Some(99), 1, &map_b),
            Err(HubError::Invalid(_))
        ));
        assert!(matches!(
            store.promise_history(WORKSPACE, id, None, Some(1), 1, &map_a),
            Err(HubError::Invalid(_))
        ));
        assert!(
            store
                .promise_history(WORKSPACE, id, None, None, 1, &map_a)
                .unwrap()
                .is_none()
        );
    }
    let valid_id = &valid.reference.promise_id;
    assert!(
        store
            .promise_get(WORKSPACE, valid_id, None, &map_b)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_get(WORKSPACE, valid_id, Some(&[REPO_TWO.into()]), &map_a)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        store.promise_list(
            WORKSPACE,
            Some(&[REPO_TWO.into()]),
            Some(valid_id),
            1,
            &map_b
        ),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .promise_list(
                WORKSPACE,
                Some(&[REPO_TWO.into()]),
                Some(valid_id),
                1,
                &map_a
            )
            .is_ok()
    );
    assert!(matches!(
        store.promise_history(WORKSPACE, valid_id, None, Some(1), 1, &map_b),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .promise_history(WORKSPACE, valid_id, None, Some(1), 1, &map_a)
            .is_ok()
    );
}

fn invalid_or_transplanted_payload_cannot_narrow_original_scope(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 111);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "unknown_payload_source".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
                definition: definition(&[REPO_ONE, REPO_TWO], "unknown payload source"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    let original = store
        .promise_get(WORKSPACE, id, None, &current())
        .unwrap()
        .unwrap()
        .current;
    let report = store
        .test_promise_input(
            WORKSPACE,
            &alice,
            None,
            &PromiseInputV1::Report {
                operation_id: "unknown_payload_source_report".into(),
                promise_id: id.clone(),
                promise_version: created.reference.version,
                promise_digest: created.reference.digest.clone(),
                repository_id: REPO_ONE.into(),
                summary: "Report under the original two-key promise.".into(),
            },
            2,
        )
        .unwrap();
    let grants = [REPO_ONE.to_owned()];
    assert!(
        store
            .promise_get(WORKSPACE, id, Some(&grants), &current())
            .unwrap()
            .is_none()
    );
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
        db.exec(
            "DROP TRIGGER hub_promise_repositories_no_change ON hub_promise_version_repositories",
        );
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
        db.exec("DROP TRIGGER hub_promise_repositories_no_delete");
    }
    let narrowed =
        serde_json::to_string(&[PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]).unwrap();
    db.set_promise_version_scope(id, &narrowed);
    db.set_promise_lifetime_scope(id, &narrowed);
    db.delete_promise_repository_index(id, REPO_TWO);
    assert_eq!(
        serde_json::from_str::<Vec<PromiseRepositoryAdmissionV1>>(&db.promise_version_scope(id))
            .unwrap(),
        vec![PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]
    );
    assert_eq!(
        db.scalar("SELECT COUNT(*) FROM hub_promise_version_repositories"),
        1
    );
    let mut correlated = serde_json::to_value(&original).unwrap();
    correlated["repository_admissions"] = json!([PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]);
    let retained_repository = original
        .definition
        .repositories
        .iter()
        .find(|repository| repository.repository_id == REPO_ONE)
        .unwrap()
        .clone();
    correlated["definition"]["repositories"] = json!([retained_repository]);
    assert_eq!(
        correlated["ref"]["digest"].as_str(),
        Some(created.reference.digest.as_str())
    );
    for payload in ["{".to_owned(), correlated.to_string()] {
        db.set_promise_version_payload(id, &payload);
        for permitted in [None, Some(grants.as_slice())] {
            let hidden = store
                .promise_get(WORKSPACE, id, permitted, &current())
                .unwrap();
            let missing = store
                .promise_get(WORKSPACE, "prm_missing", permitted, &current())
                .unwrap();
            assert_eq!(hidden, missing);
            assert!(hidden.is_none());
            let page = store
                .promise_list(WORKSPACE, permitted, None, 1, &current())
                .unwrap();
            assert!(page.promises.is_empty());
            assert!(page.next.is_none());
            let hidden_cursor = store
                .promise_list(WORKSPACE, permitted, Some(id), 1, &current())
                .unwrap_err();
            let missing_cursor = store
                .promise_list(WORKSPACE, permitted, Some("prm_missing"), 1, &current())
                .unwrap_err();
            assert!(matches!(&hidden_cursor, HubError::Invalid(_)));
            assert_eq!(hidden_cursor.to_string(), missing_cursor.to_string());
            let hidden_history = store
                .promise_history(WORKSPACE, id, permitted, None, 1, &current())
                .unwrap();
            let missing_history = store
                .promise_history(WORKSPACE, "prm_missing", permitted, None, 1, &current())
                .unwrap();
            assert_eq!(hidden_history, missing_history);
            assert!(hidden_history.is_none());
            let hidden_history_cursor = store
                .promise_history(WORKSPACE, id, permitted, Some(1), 1, &current())
                .unwrap_err();
            let missing_history_cursor = store
                .promise_history(WORKSPACE, "prm_missing", permitted, Some(1), 1, &current())
                .unwrap_err();
            assert_eq!(
                hidden_history_cursor.to_string(),
                missing_history_cursor.to_string()
            );
            let hidden_report = store
                .promise_input_get(WORKSPACE, &report.input_id, permitted, &current())
                .unwrap();
            let missing_report = store
                .promise_input_get(WORKSPACE, "prr_missing", permitted, &current())
                .unwrap();
            assert_eq!(hidden_report, missing_report);
            assert!(hidden_report.is_none());
            for after in [None, Some(report.input_id.as_str())] {
                let hidden_reports = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report(id),
                        permitted,
                        after,
                        1,
                        &current(),
                    )
                    .unwrap_err();
                let missing_reports = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report("prm_missing"),
                        permitted,
                        after,
                        1,
                        &current(),
                    )
                    .unwrap_err();
                assert_eq!(hidden_reports.to_string(), missing_reports.to_string());
            }
        }
    }

    let source = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "transplant_source_a".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "valid source payload"),
            },
            3,
        )
        .unwrap();
    let source_payload = store
        .test_promise_get(WORKSPACE, &source.reference.promise_id)
        .unwrap()
        .unwrap()
        .current;
    let report_free = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "transplant_report_free_target".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
                definition: definition(&[REPO_ONE, REPO_TWO], "report-free hidden target"),
            },
            4,
        )
        .unwrap();
    db.set_promise_version_scope(&report_free.reference.promise_id, &narrowed);
    db.set_promise_lifetime_scope(&report_free.reference.promise_id, &narrowed);
    db.delete_promise_repository_index(&report_free.reference.promise_id, REPO_TWO);
    let transplanted = serde_json::to_string(&source_payload).unwrap();
    db.set_promise_version_payload(id, &transplanted);
    db.set_promise_version_payload(&report_free.reference.promise_id, &transplanted);
    for target in [id, report_free.reference.promise_id.as_str()] {
        for permitted in [None, Some(grants.as_slice())] {
            let hidden = store
                .promise_get(WORKSPACE, target, permitted, &current())
                .unwrap();
            let missing = store
                .promise_get(WORKSPACE, "prm_missing", permitted, &current())
                .unwrap();
            assert_eq!(hidden, missing);
            assert!(hidden.is_none());
            let page = store
                .promise_list(WORKSPACE, permitted, None, 10, &current())
                .unwrap();
            assert_eq!(page.promises.len(), 1);
            assert_eq!(
                page.promises[0].current.reference.promise_id,
                source.reference.promise_id
            );
            let hidden_cursor = store
                .promise_list(WORKSPACE, permitted, Some(target), 1, &current())
                .unwrap_err();
            let missing_cursor = store
                .promise_list(WORKSPACE, permitted, Some("prm_missing"), 1, &current())
                .unwrap_err();
            assert_eq!(hidden_cursor.to_string(), missing_cursor.to_string());
            let hidden_history = store
                .promise_history(WORKSPACE, target, permitted, None, 1, &current())
                .unwrap();
            let missing_history = store
                .promise_history(WORKSPACE, "prm_missing", permitted, None, 1, &current())
                .unwrap();
            assert_eq!(hidden_history, missing_history);
            assert!(hidden_history.is_none());
            let hidden_history_cursor = store
                .promise_history(WORKSPACE, target, permitted, Some(1), 1, &current())
                .unwrap_err();
            let missing_history_cursor = store
                .promise_history(WORKSPACE, "prm_missing", permitted, Some(1), 1, &current())
                .unwrap_err();
            assert_eq!(
                hidden_history_cursor.to_string(),
                missing_history_cursor.to_string()
            );
            for after in [None, Some(report.input_id.as_str())] {
                let hidden_reports = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report(target),
                        permitted,
                        after,
                        1,
                        &current(),
                    )
                    .unwrap_err();
                let missing_reports = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report("prm_missing"),
                        permitted,
                        after,
                        1,
                        &current(),
                    )
                    .unwrap_err();
                assert_eq!(hidden_reports.to_string(), missing_reports.to_string());
            }
            if target == id {
                let hidden_report = store
                    .promise_input_get(WORKSPACE, &report.input_id, permitted, &current())
                    .unwrap();
                let missing_report = store
                    .promise_input_get(WORKSPACE, "prr_missing", permitted, &current())
                    .unwrap();
                assert_eq!(hidden_report, missing_report);
                assert!(hidden_report.is_none());
            }
        }
    }
}

fn self_invalid_payload_admissions_hide_before_integrity(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 113);
    let bound = |key: &str, numeric| PromiseRepositoryAdmissionV1 {
        repository_id: key.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let one = bound(REPO_ONE, 101);
    let two = bound(REPO_TWO, 303);
    assert!(two.repository_id < one.repository_id);
    let admissions = [two.clone(), one.clone()];
    let current = PromiseCurrentAdmissionsV1::from([
        (REPO_ONE.into(), one.clone()),
        (REPO_TWO.into(), two.clone()),
    ]);
    let created = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "payload_admission_aggregate".into(),
                expected_admissions_digest: admission_digest(&admissions),
                definition: definition(&[REPO_ONE, REPO_TWO], "admission aggregate"),
            },
            &admissions,
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    let original = store
        .promise_get(WORKSPACE, id, None, &current)
        .unwrap()
        .unwrap()
        .current;
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let grants = [REPO_ONE.to_owned(), REPO_TWO.to_owned()];
    for entries in [
        vec![two.clone(), two.clone()],
        vec![one.clone(), two.clone()],
    ] {
        let mut payload = serde_json::to_value(&original).unwrap();
        payload["repository_admissions"] = json!(entries);
        db.set_promise_version_payload(id, &payload.to_string());
        for permitted in [None, Some(grants.as_slice())] {
            assert!(
                store
                    .promise_get(WORKSPACE, id, permitted, &current)
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                store.promise_list(WORKSPACE, permitted, Some(id), 1, &current),
                Err(HubError::Invalid(_))
            ));
            assert!(
                store
                    .promise_history(WORKSPACE, id, permitted, None, 1, &current)
                    .unwrap()
                    .is_none()
            );
        }
    }

    let stale_two = bound(REPO_TWO, 404);
    let mut payload = serde_json::to_value(&original).unwrap();
    payload["repository_admissions"] = json!([stale_two, one]);
    db.set_promise_version_payload(id, &payload.to_string());
    for permitted in [None, Some(grants.as_slice())] {
        assert!(
            store
                .promise_get(WORKSPACE, id, permitted, &current)
                .unwrap()
                .is_none()
        );
        let page = store
            .promise_list(WORKSPACE, permitted, None, 1, &current)
            .unwrap();
        assert!(page.promises.is_empty());
        assert!(page.next.is_none());
        assert!(matches!(
            store.promise_list(WORKSPACE, permitted, Some(id), 1, &current),
            Err(HubError::Invalid(_))
        ));
        assert!(
            store
                .promise_history(WORKSPACE, id, permitted, None, 1, &current)
                .unwrap()
                .is_none()
        );
    }
}

fn typed_numeric_source_admissions_follow_retained_encoding(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 112);
    let bound = |numeric| PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let exact = bound(9_007_199_254_740_993);
    let adjacent = bound(9_007_199_254_740_992);
    let exact_map = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), exact.clone())]);
    let adjacent_map = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), adjacent)]);
    let large = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "large_numeric_admission".into(),
                expected_admissions_digest: admission_digest(std::slice::from_ref(&exact)),
                definition: definition(&[REPO_ONE], "large exact numeric identity"),
            },
            std::slice::from_ref(&exact),
            1,
        )
        .unwrap();
    let large_id = &large.reference.promise_id;
    let grants = [REPO_ONE.to_owned()];
    for permitted in [None, Some(grants.as_slice())] {
        assert_eq!(
            store
                .promise_get(WORKSPACE, large_id, permitted, &exact_map)
                .unwrap()
                .unwrap()
                .current
                .reference,
            large.reference
        );
        assert!(
            store
                .promise_list(WORKSPACE, permitted, None, 1, &exact_map)
                .unwrap()
                .promises
                .iter()
                .any(|view| view.current.reference == large.reference)
        );
        assert!(
            store
                .promise_history(WORKSPACE, large_id, permitted, None, 1, &exact_map)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .promise_get(WORKSPACE, large_id, permitted, &adjacent_map)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .promise_list(WORKSPACE, permitted, None, 1, &adjacent_map)
                .unwrap()
                .promises
                .is_empty()
        );
        assert!(matches!(
            store.promise_list(WORKSPACE, permitted, Some(large_id), 1, &adjacent_map),
            Err(HubError::Invalid(_))
        ));
        assert!(
            store
                .promise_history(WORKSPACE, large_id, permitted, None, 1, &adjacent_map)
                .unwrap()
                .is_none()
        );
    }

    let small = bound(101);
    let small_map = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), small.clone())]);
    let created = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "numeric_wire_encoding".into(),
                expected_admissions_digest: admission_digest(std::slice::from_ref(&small)),
                definition: definition(&[REPO_ONE], "numeric wire encoding"),
            },
            std::slice::from_ref(&small),
            2,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let canonical = serde_json::to_string(std::slice::from_ref(&small)).unwrap();
    let number = "\"forge_repository_id\":101";
    assert!(canonical.contains(number));
    let decimal = canonical.replacen(number, "\"forge_repository_id\":101.0", 1);
    db.set_promise_version_scope(id, &decimal);
    let retained_decimal = db.promise_version_scope(id);
    assert!(
        serde_json::from_str::<Vec<PromiseRepositoryAdmissionV1>>(&retained_decimal).is_err(),
        "decimal admission unexpectedly became typed: {retained_decimal}"
    );
    for permitted in [None, Some(grants.as_slice())] {
        assert!(
            store
                .promise_get(WORKSPACE, id, permitted, &small_map)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .promise_list(WORKSPACE, permitted, None, 10, &small_map)
                .unwrap()
                .promises
                .is_empty()
        );
        assert!(matches!(
            store.promise_list(WORKSPACE, permitted, Some(id), 1, &small_map),
            Err(HubError::Invalid(_))
        ));
    }

    let exponent = canonical.replacen(number, "\"forge_repository_id\":101e0", 1);
    db.set_promise_version_scope(id, &exponent);
    let retained_exponent = db.promise_version_scope(id);
    if db.pg.is_none() {
        assert!(retained_exponent.contains("101e0"));
    }
    let retained_typed =
        serde_json::from_str::<Vec<PromiseRepositoryAdmissionV1>>(&retained_exponent)
            .is_ok_and(|scope| scope == vec![small.clone()]);
    for permitted in [None, Some(grants.as_slice())] {
        let observed = store
            .promise_get(WORKSPACE, id, permitted, &small_map)
            .unwrap();
        assert_eq!(observed.is_some(), retained_typed);
    }
}

fn incomplete_index_and_scope_cannot_narrow_a_two_repository_promise(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 96);
    let request = PromiseCreateV1 {
        operation_id: "two_repository_scope".into(),
        expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
        definition: definition(&[REPO_ONE, REPO_TWO], "two repository scope"),
    };
    let created = store
        .test_promise_create(WORKSPACE, &alice, &request, 1)
        .unwrap();
    let id = &created.reference.promise_id;
    let report = PromiseInputV1::Report {
        operation_id: "two_repository_report".into(),
        promise_id: id.clone(),
        promise_version: 1,
        promise_digest: created.reference.digest.clone(),
        repository_id: REPO_ONE.into(),
        summary: "Observed the first repository only.".into(),
    };
    let input = store
        .test_promise_input(WORKSPACE, &alice, None, &report, 2)
        .unwrap();
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions;
                 DROP TRIGGER hub_promise_repositories_no_change ON hub_promise_version_repositories");
    } else {
        db.exec(
            "DROP TRIGGER hub_promise_versions_no_update;
                 DROP TRIGGER hub_promise_repositories_no_delete",
        );
    }
    db.set_promise_version_scope(
        id,
        &serde_json::to_string(&vec![PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]).unwrap(),
    );
    db.delete_promise_repository_index(id, REPO_TWO);
    let one_grant = [REPO_ONE.to_owned()];
    let both_grants = [REPO_ONE.to_owned(), REPO_TWO.to_owned()];
    assert!(
        store
            .promise_get(WORKSPACE, id, Some(&one_grant), &current())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, Some(&one_grant), None, 10, &current())
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(matches!(
        store.promise_list(WORKSPACE, Some(&one_grant), Some(id), 10, &current()),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .promise_history(WORKSPACE, id, Some(&one_grant), None, 10, &current())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_input_get(WORKSPACE, &input.input_id, Some(&one_grant), &current())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.promise_inputs(
            WORKSPACE,
            PromiseInputScope::Report(id),
            Some(&one_grant),
            None,
            10,
            &current()
        ),
        Err(HubError::Invalid(_))
    ));
    assert!(matches!(
        store.promise_get(WORKSPACE, id, Some(&both_grants), &current()),
        Err(HubError::Unavailable(_))
    ));
    assert!(matches!(
        store.promise_history(WORKSPACE, id, Some(&both_grants), Some(1), 10, &current()),
        Err(HubError::Unavailable(_))
    ));
}

fn scoped_list_skips_damaged_narrowed_row_and_fills_pages(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 101);
    // The middle ID remains two-key. Amending the first and last through the store makes
    // genuinely visible current versions on both sides of the damaged candidate.
    let mut created = (0..3)
        .map(|index| {
            store
                .test_promise_create(
                    WORKSPACE,
                    &alice,
                    &PromiseCreateV1 {
                        operation_id: format!("scoped_created_{index}"),
                        expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
                        definition: definition(&[REPO_ONE, REPO_TWO], "two-key"),
                    },
                    index + 1,
                )
                .unwrap()
                .reference
        })
        .collect::<Vec<_>>();
    created.sort_by(|left, right| left.promise_id.cmp(&right.promise_id));
    let damaged_id = &created[1].promise_id;
    let mut visible_ids = Vec::new();
    for (index, reference) in [created.first().unwrap(), created.last().unwrap()]
        .into_iter()
        .enumerate()
    {
        let mut narrow = change(
            &format!("scoped_visible_{index}"),
            reference.version,
            &reference.digest,
            PromiseChangeActionV1::Amend {
                content: content(&[REPO_ONE], "visible"),
            },
        );
        narrow.expected_admissions_digest = unbound_digest(&[REPO_ONE]);
        visible_ids.push(
            store
                .test_promise_change(
                    WORKSPACE,
                    &alice,
                    &reference.promise_id,
                    PromiseChangeAuthority::Steward,
                    &narrow,
                    4 + index as i64,
                )
                .unwrap()
                .reference
                .promise_id,
        );
    }
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions;
                 DROP TRIGGER hub_promise_repositories_no_change ON hub_promise_version_repositories");
    } else {
        db.exec(
            "DROP TRIGGER hub_promise_versions_no_update;
                 DROP TRIGGER hub_promise_repositories_no_delete",
        );
    }
    db.set_promise_version_scope(
        damaged_id,
        &json!([PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]).to_string(),
    );
    db.delete_promise_repository_index(damaged_id, REPO_TWO);
    let grants = [REPO_ONE.to_owned()];
    assert!(
        store
            .promise_get(WORKSPACE, damaged_id, Some(&grants), &current())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.promise_get(WORKSPACE, damaged_id, None, &current()),
        Err(HubError::Unavailable(_))
    ));
    assert!(matches!(
        store.promise_list(WORKSPACE, Some(&grants), Some(damaged_id), 1, &current()),
        Err(HubError::Invalid(_))
    ));
    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..=visible_ids.len() {
        let page = store
            .promise_list(WORKSPACE, Some(&grants), cursor.as_deref(), 1, &current())
            .unwrap();
        assert_eq!(
            page.promises.len(),
            1,
            "an authorized page must not end at a damaged row"
        );
        seen.push(page.promises[0].current.reference.promise_id.clone());
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert_eq!(seen, visible_ids);
}

fn qualified_malformed_current_scope_is_an_integrity_error(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 102);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "duplicate_scope".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "duplicate scope"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let admission = PromiseRepositoryAdmissionV1::unbound(REPO_ONE);
    db.set_promise_version_scope(id, &json!([admission.clone(), admission]).to_string());
    let permitted = [REPO_ONE.to_owned()];
    for grants in [None, Some(permitted.as_slice())] {
        assert!(matches!(
            store.promise_get(WORKSPACE, id, grants, &current()),
            Err(HubError::Unavailable(_))
        ));
        assert!(matches!(
            store.promise_list(WORKSPACE, grants, None, 1, &current()),
            Err(HubError::Unavailable(_))
        ));
        assert!(matches!(
            store.promise_list(WORKSPACE, grants, Some(id), 1, &current()),
            Err(HubError::Unavailable(_))
        ));
        assert!(matches!(
            store.promise_history(WORKSPACE, id, grants, None, 1, &current()),
            Err(HubError::Unavailable(_))
        ));
        assert!(matches!(
            store.promise_history(WORKSPACE, id, grants, Some(1), 1, &current()),
            Err(HubError::Unavailable(_))
        ));
        assert!(matches!(
            store.promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(id),
                grants,
                None,
                1,
                &current(),
            ),
            Err(HubError::Unavailable(_))
        ));
    }
    let denied = [REPO_TWO.to_owned()];
    assert!(
        store
            .promise_get(WORKSPACE, id, Some(&denied), &current())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, Some(&denied), None, 1, &current())
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(matches!(
        store.promise_list(WORKSPACE, Some(&denied), Some(id), 1, &current()),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .promise_history(WORKSPACE, id, Some(&denied), None, 1, &current())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.promise_history(WORKSPACE, id, Some(&denied), Some(1), 1, &current()),
        Err(HubError::Invalid(_))
    ));
    for hidden in [id.as_str(), "prm_missing"] {
        assert!(matches!(
            store.promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(hidden),
                Some(&denied),
                None,
                1,
                &current(),
            ),
            Err(HubError::Invalid(message)) if message == "promise_not_found"
        ));
    }
}

fn damaged_historical_version_hides_whole_history_and_report_page(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 114);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "historical_damage_create".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "historical damage"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    let report = store
        .test_promise_input(
            WORKSPACE,
            &alice,
            None,
            &PromiseInputV1::Report {
                operation_id: "historical_damage_report".into(),
                promise_id: id.clone(),
                promise_version: created.reference.version,
                promise_digest: created.reference.digest.clone(),
                repository_id: REPO_ONE.into(),
                summary: "Retained report on the original commitment.".into(),
            },
            2,
        )
        .unwrap();
    let mut amendment = change(
        "historical_damage_amend",
        created.reference.version,
        &created.reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_ONE], "amended after report"),
        },
    );
    amendment.expected_admissions_digest = unbound_digest(&[REPO_ONE]);
    let amended = store
        .test_promise_change(
            WORKSPACE,
            &alice,
            id,
            PromiseChangeAuthority::Steward,
            &amendment,
            3,
        )
        .unwrap();
    assert_eq!(amended.reference.version, 2);
    assert!(
        store
            .test_promise_input_get(WORKSPACE, &report.input_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .test_promise_inputs(WORKSPACE, "report", id, None, 2)
            .unwrap()
            .inputs
            .len(),
        1
    );
    let original = store
        .promise_history(WORKSPACE, id, None, None, 2, &current())
        .unwrap()
        .unwrap()
        .versions
        .into_iter()
        .find(|version| version.reference.version == 1)
        .unwrap();
    let mut damaged = serde_json::to_value(original).unwrap();
    damaged["definition"]["commitment"] = json!("Changed without updating its digest.");
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    db.set_promise_version_payload(id, &damaged.to_string());

    let grants = [REPO_ONE.to_owned()];
    for permitted in [None, Some(grants.as_slice())] {
        let current_view = store
            .promise_get(WORKSPACE, id, permitted, &current())
            .unwrap()
            .unwrap();
        assert_eq!(current_view.current.reference, amended.reference);
        assert!(
            store
                .promise_history(WORKSPACE, id, permitted, None, 2, &current())
                .unwrap()
                .is_none(),
            "history cannot silently skip an unreadable retained version"
        );
        assert!(matches!(
            store.promise_history(WORKSPACE, id, permitted, Some(1), 2, &current()),
            Err(HubError::Invalid(_))
        ));
        assert!(
            store
                .promise_input_get(WORKSPACE, &report.input_id, permitted, &current())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(id),
                permitted,
                None,
                2,
                &current(),
            ),
            Err(HubError::Invalid(message)) if message == "promise_not_found"
        ));
        assert!(matches!(
            store.promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(id),
                permitted,
                Some(&report.input_id),
                2,
                &current(),
            ),
            Err(HubError::Invalid(message)) if message == "invalid promise input cursor"
        ));
    }
}

fn reordered_valid_scope_remains_visible_before_payload_decode(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 103);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "reordered_valid_scope".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "reordered"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let admission = PromiseRepositoryAdmissionV1::unbound(REPO_ONE);
    let reordered = json!([admission.clone()]).to_string();
    assert_ne!(reordered, serde_json::to_string(&[admission]).unwrap());
    db.set_promise_version_scope(id, &reordered);
    let grants = [REPO_ONE.to_owned()];
    assert_eq!(
        store
            .promise_get(WORKSPACE, id, Some(&grants), &current())
            .unwrap()
            .unwrap()
            .current
            .reference,
        created.reference
    );
    let page = store
        .promise_list(WORKSPACE, Some(&grants), None, 1, &current())
        .unwrap();
    assert_eq!(page.promises.len(), 1);
    assert_eq!(page.promises[0].current.reference, created.reference);
    assert!(page.next.is_none());
    assert!(
        store
            .promise_list(WORKSPACE, Some(&grants), Some(id), 1, &current())
            .unwrap()
            .promises
            .is_empty()
    );

    db.set_promise_version_payload(id, "{");
    let stale = PromiseCurrentAdmissionsV1::from([(
        REPO_ONE.into(),
        PromiseRepositoryAdmissionV1 {
            repository_id: REPO_ONE.into(),
            forge: Some(PromiseForgeAdmissionV1 {
                provider: "github".into(),
                host: "github.com".into(),
                forge_repository_id: 123,
            }),
            native_repository_ids: vec![],
        },
    )]);
    assert!(
        store
            .promise_get(WORKSPACE, id, Some(&grants), &stale)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, Some(&grants), None, 1, &stale)
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(matches!(
        store.promise_list(WORKSPACE, Some(&grants), Some(id), 1, &stale),
        Err(HubError::Invalid(_))
    ));
}

fn missing_sole_index_is_undiscoverable_but_authorized_reads_detect_it(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 104);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "missing_sole_index".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "missing index"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    if db.pg.is_some() {
        db.exec(
            "DROP TRIGGER hub_promise_repositories_no_change ON hub_promise_version_repositories",
        );
    } else {
        db.exec("DROP TRIGGER hub_promise_repositories_no_delete");
    }
    db.delete_promise_repository_index(id, REPO_ONE);
    let granted = [REPO_ONE.to_owned()];
    let denied = [REPO_TWO.to_owned()];
    assert!(
        store
            .promise_list(WORKSPACE, Some(&granted), None, 1, &current())
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(matches!(
        store.promise_get(WORKSPACE, id, Some(&granted), &current()),
        Err(HubError::Unavailable(_))
    ));
    assert!(matches!(
        store.promise_list(WORKSPACE, Some(&granted), Some(id), 1, &current()),
        Err(HubError::Unavailable(_))
    ));
    assert!(
        store
            .promise_get(WORKSPACE, id, Some(&denied), &current())
            .unwrap()
            .is_none()
    );
    for cursor in [id.as_str(), "prm_missing"] {
        assert!(matches!(
            store.promise_list(WORKSPACE, Some(&denied), Some(cursor), 1, &current()),
            Err(HubError::Invalid(_))
        ));
    }
    assert!(
        store
            .promise_get(WORKSPACE, "prm_missing", Some(&granted), &current())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.promise_list(
            WORKSPACE,
            Some(&granted),
            Some("prm_missing"),
            1,
            &current()
        ),
        Err(HubError::Invalid(_))
    ));
}

fn same_repository_key_in_two_workspaces_stays_isolated(db: &Db) {
    use agentscm::hub::HubError;

    const OTHER_WORKSPACE: &str = "other-promise-workspace";
    let store = db.open();
    let alice = user(&*store, 105);
    let admission = PromiseRepositoryAdmissionV1::unbound(REPO_ONE);
    let first = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "same_key_first_workspace".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "first workspace"),
            },
            1,
        )
        .unwrap();
    let second = store
        .promise_create(
            OTHER_WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "same_key_second_workspace".into(),
                expected_admissions_digest: admissions_digest(
                    OTHER_WORKSPACE,
                    std::slice::from_ref(&admission),
                )
                .unwrap(),
                definition: definition(&[REPO_ONE], "second workspace"),
            },
            &[admission],
            2,
        )
        .unwrap();
    let grants = [REPO_ONE.to_owned()];
    for (workspace, visible, hidden) in [
        (WORKSPACE, &first.reference, &second.reference),
        (OTHER_WORKSPACE, &second.reference, &first.reference),
    ] {
        let page = store
            .promise_list(workspace, Some(&grants), None, 1, &current())
            .unwrap();
        assert_eq!(page.promises.len(), 1);
        assert_eq!(&page.promises[0].current.reference, visible);
        assert!(page.next.is_none());
        assert!(
            store
                .promise_get(workspace, &hidden.promise_id, Some(&grants), &current())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.promise_list(
                workspace,
                Some(&grants),
                Some(&hidden.promise_id),
                1,
                &current()
            ),
            Err(HubError::Invalid(_))
        ));
    }
}

fn mixed_case_repository_keys_keep_exact_index_order(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 106);
    let keys = ["synthetic/Zeta", "synthetic/alpha"];
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "mixed_case_repository_keys".into(),
                expected_admissions_digest: unbound_digest(&keys),
                definition: definition(&keys, "mixed case"),
            },
            1,
        )
        .unwrap();
    let grants = keys.map(str::to_owned);
    let id = &created.reference.promise_id;
    assert_eq!(
        store
            .promise_get(WORKSPACE, id, Some(&grants), &current())
            .unwrap()
            .unwrap()
            .current
            .reference,
        created.reference
    );
    let page = store
        .promise_list(WORKSPACE, Some(&grants), None, 1, &current())
        .unwrap();
    assert_eq!(page.promises.len(), 1);
    assert_eq!(page.promises[0].current.reference, created.reference);
    assert_eq!(
        store
            .promise_history(WORKSPACE, id, Some(&grants), None, 10, &current())
            .unwrap()
            .unwrap()
            .versions[0]
            .reference,
        created.reference
    );
}

fn malformed_string_keyed_noncurrent_scope_is_hidden(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 107);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "malformed_noncurrent_scope".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                definition: definition(&[REPO_ONE], "malformed noncurrent"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let grants = [REPO_ONE.to_owned()];
    for malformed_scope in [
        json!([{"repository_id":REPO_ONE,"forge":null,"native_repository_ids":"x"}]),
        json!([{"repository_id":"","forge":null,"native_repository_ids":[]}]),
    ] {
        db.set_promise_version_scope(id, &malformed_scope.to_string());
        for permitted in [None, Some(grants.as_slice())] {
            assert!(
                store
                    .promise_get(WORKSPACE, id, permitted, &current())
                    .unwrap()
                    .is_none()
            );
            assert!(
                store
                    .promise_list(WORKSPACE, permitted, None, 1, &current())
                    .unwrap()
                    .promises
                    .is_empty()
            );
            assert!(matches!(
                store.promise_list(WORKSPACE, permitted, Some(id), 1, &current()),
                Err(HubError::Invalid(_))
            ));
            assert!(
                store
                    .promise_history(WORKSPACE, id, permitted, None, 1, &current())
                    .unwrap()
                    .is_none()
            );
            for hidden in [id.as_str(), "prm_missing"] {
                assert!(matches!(
                    store.promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report(hidden),
                        permitted,
                        None,
                        1,
                        &current(),
                    ),
                    Err(HubError::Invalid(message)) if message == "promise_not_found"
                ));
            }
        }
    }
}

fn remapped_source_does_not_expose_retained_admission_through_metadata(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 108);
    let bound = |numeric| PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a = bound(101);
    let b = bound(202);
    let current_a = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), a.clone())]);
    let current_b = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), b.clone())]);
    let mut cases = Vec::new();
    for (index, (version_scope_current, wrong_digest)) in
        [(false, false), (true, false), (true, true)]
            .into_iter()
            .enumerate()
    {
        let created = store
            .promise_create(
                WORKSPACE,
                &alice,
                &PromiseCreateV1 {
                    operation_id: format!("remapped_retained_{index}"),
                    expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
                    definition: definition(&[REPO_ONE], "retained source A"),
                },
                std::slice::from_ref(&a),
                index as i64 + 1,
            )
            .unwrap();
        let report = store
            .promise_input(
                WORKSPACE,
                &alice,
                None,
                &PromiseInputV1::Report {
                    operation_id: format!("remapped_report_{index}"),
                    promise_id: created.reference.promise_id.clone(),
                    promise_version: created.reference.version,
                    promise_digest: created.reference.digest.clone(),
                    repository_id: REPO_ONE.into(),
                    summary: "Caller report under source A.".into(),
                },
                &a,
                3 + index as i64,
            )
            .unwrap();
        cases.push((
            created.reference.promise_id,
            report.input_id,
            version_scope_current,
            wrong_digest,
        ));
    }
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let encoded_b = serde_json::to_string(&[b]).unwrap();
    let granted = [REPO_ONE.to_owned()];
    for (id, report_id, version_scope_current, wrong_digest) in cases {
        if wrong_digest {
            let mut payload = serde_json::to_value(
                store
                    .promise_get(WORKSPACE, &id, None, &current_a)
                    .unwrap()
                    .unwrap()
                    .current,
            )
            .unwrap();
            payload["definition"]["commitment"] = json!("Tampered source A commitment.");
            db.set_promise_version_payload(&id, &payload.to_string());
        }
        db.set_promise_lifetime_scope(&id, &encoded_b);
        if version_scope_current {
            db.set_promise_version_scope(&id, &encoded_b);
        }
        for permitted in [None, Some(granted.as_slice())] {
            let hidden = store
                .promise_get(WORKSPACE, &id, permitted, &current_b)
                .unwrap();
            let missing = store
                .promise_get(WORKSPACE, "prm_missing", permitted, &current_b)
                .unwrap();
            assert_eq!(hidden, missing);
            assert!(hidden.is_none());
            let page = store
                .promise_list(WORKSPACE, permitted, None, 10, &current_b)
                .unwrap();
            assert!(page.promises.is_empty());
            assert!(page.next.is_none());
            assert!(matches!(
                store.promise_list(WORKSPACE, permitted, Some(&id), 1, &current_b),
                Err(HubError::Invalid(_))
            ));
            let hidden_history = store
                .promise_history(WORKSPACE, &id, permitted, None, 10, &current_b)
                .unwrap();
            let missing_history = store
                .promise_history(WORKSPACE, "prm_missing", permitted, None, 10, &current_b)
                .unwrap();
            assert_eq!(hidden_history, missing_history);
            assert!(hidden_history.is_none());
            assert!(matches!(
                store.promise_history(WORKSPACE, &id, permitted, Some(1), 10, &current_b),
                Err(HubError::Invalid(_))
            ));
            let hidden_report = store
                .promise_input_get(WORKSPACE, &report_id, permitted, &current_b)
                .unwrap();
            let missing_report = store
                .promise_input_get(WORKSPACE, "prr_missing", permitted, &current_b)
                .unwrap();
            assert_eq!(hidden_report, missing_report);
            assert!(hidden_report.is_none());
            for after in [None, Some(report_id.as_str())] {
                let hidden = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report(&id),
                        permitted,
                        after,
                        10,
                        &current_b,
                    )
                    .unwrap_err();
                let missing = store
                    .promise_inputs(
                        WORKSPACE,
                        PromiseInputScope::Report("prm_missing"),
                        permitted,
                        after,
                        10,
                        &current_b,
                    )
                    .unwrap_err();
                assert!(matches!(&hidden, HubError::Invalid(_)));
                assert_eq!(hidden.to_string(), missing.to_string());
            }
        }
    }
}

fn stale_report_admission_hides_damaged_current_promise(db: &Db) {
    use agentscm::hub::HubError;
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 110);
    let bound = |numeric| PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a = bound(101);
    let b = bound(202);
    let current_b = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), b.clone())]);
    let created = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "stale_report_damaged_parent".into(),
                expected_admissions_digest: admission_digest(std::slice::from_ref(&b)),
                definition: definition(&[REPO_ONE], "current source B"),
            },
            std::slice::from_ref(&b),
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    let report = store
        .promise_input(
            WORKSPACE,
            &alice,
            None,
            &PromiseInputV1::Report {
                operation_id: "stale_report_damaged_parent_input".into(),
                promise_id: id.clone(),
                promise_version: created.reference.version,
                promise_digest: created.reference.digest.clone(),
                repository_id: REPO_ONE.into(),
                summary: "Observed under source B.".into(),
            },
            &b,
            2,
        )
        .unwrap();
    let report_free = store
        .promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "damaged_report_free_parent".into(),
                expected_admissions_digest: admission_digest(std::slice::from_ref(&b)),
                definition: definition(&[REPO_ONE], "no report rows"),
            },
            std::slice::from_ref(&b),
            3,
        )
        .unwrap();
    let grants = [REPO_ONE.to_owned()];
    assert_eq!(
        store
            .promise_get(WORKSPACE, id, Some(&grants), &current_b)
            .unwrap()
            .unwrap()
            .current
            .reference,
        created.reference
    );
    assert!(
        store
            .promise_input_get(WORKSPACE, &report.input_id, Some(&grants), &current_b)
            .unwrap()
            .is_some()
    );

    db.set_promise_input_admission(&report.input_id, &serde_json::to_string(&a).unwrap());
    if db.pg.is_some() {
        db.exec(
            "DROP TRIGGER hub_promise_repositories_no_change ON hub_promise_version_repositories",
        );
    } else {
        db.exec("DROP TRIGGER hub_promise_repositories_no_delete");
    }
    db.delete_promise_repository_index(id, REPO_ONE);
    db.delete_promise_repository_index(&report_free.reference.promise_id, REPO_ONE);
    assert!(matches!(
        store.promise_get(WORKSPACE, id, Some(&grants), &current_b),
        Err(HubError::Unavailable(_))
    ));

    for permitted in [None, Some(grants.as_slice())] {
        let hidden = store
            .promise_input_get(WORKSPACE, &report.input_id, permitted, &current_b)
            .unwrap();
        let missing = store
            .promise_input_get(WORKSPACE, "prr_missing", permitted, &current_b)
            .unwrap();
        assert_eq!(hidden, missing);
        assert!(hidden.is_none());
        for parent in [id.as_str(), report_free.reference.promise_id.as_str()] {
            assert!(matches!(
                store.promise_inputs(
                    WORKSPACE,
                    PromiseInputScope::Report(parent),
                    permitted,
                    None,
                    10,
                    &current_b,
                ),
                Err(HubError::Unavailable(_))
            ));
        }
        let hidden_cursor = store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(id),
                permitted,
                Some(&report.input_id),
                10,
                &current_b,
            )
            .unwrap_err();
        let missing_cursor = store
            .promise_inputs(
                WORKSPACE,
                PromiseInputScope::Report(id),
                permitted,
                Some("prr_missing"),
                10,
                &current_b,
            )
            .unwrap_err();
        assert!(matches!(&hidden_cursor, HubError::Invalid(_)));
        assert_eq!(hidden_cursor.to_string(), missing_cursor.to_string());
    }
}

fn narrowed_lifetime_cannot_expose_historical_two_key_version(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 109);
    let created = store
        .test_promise_create(
            WORKSPACE,
            &alice,
            &PromiseCreateV1 {
                operation_id: "lifetime_two_key_initial".into(),
                expected_admissions_digest: unbound_digest(&[REPO_ONE, REPO_TWO]),
                definition: definition(&[REPO_ONE, REPO_TWO], "two-key history"),
            },
            1,
        )
        .unwrap();
    let id = &created.reference.promise_id;
    let mut narrow = change(
        "lifetime_two_key_to_one",
        created.reference.version,
        &created.reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_ONE], "one-key current"),
        },
    );
    narrow.expected_admissions_digest = unbound_digest(&[REPO_ONE]);
    let amended = store
        .test_promise_change(
            WORKSPACE,
            &alice,
            id,
            PromiseChangeAuthority::Steward,
            &narrow,
            2,
        )
        .unwrap();
    db.set_promise_lifetime_scope(
        id,
        &json!([PromiseRepositoryAdmissionV1::unbound(REPO_ONE)]).to_string(),
    );
    let one = [REPO_ONE.to_owned()];
    let both = [REPO_ONE.to_owned(), REPO_TWO.to_owned()];
    assert!(
        store
            .promise_history(WORKSPACE, id, Some(&one), None, 10, &current())
            .unwrap()
            .is_none()
    );
    for permitted in [None, Some(both.as_slice())] {
        let history = store
            .promise_history(WORKSPACE, id, permitted, None, 10, &current())
            .unwrap()
            .unwrap();
        assert_eq!(history.versions.len(), 2);
        assert_eq!(history.versions[0].reference, amended.reference);
        assert_eq!(history.versions[1].reference, created.reference);
    }
}

fn lifetime_scope_is_bounded_and_failed_extension_is_atomic(db: &Db) {
    let store = db.open();
    let alice = user(&*store, 97);
    let first_key = "synthetic/repo01";
    let create = PromiseCreateV1 {
        operation_id: "bounded_lifetime_initial".into(),
        expected_admissions_digest: unbound_digest(&[first_key]),
        definition: definition(&[first_key], "first"),
    };
    let mut reference = store
        .test_promise_create(WORKSPACE, &alice, &create, 1)
        .unwrap()
        .reference;
    for number in 2..=64 {
        let key = format!("synthetic/repo{number:02}");
        let mut request = change(
            &format!("bounded_lifetime_{number}"),
            reference.version,
            &reference.digest,
            PromiseChangeActionV1::Amend {
                content: content(&[&key], &key),
            },
        );
        request.expected_admissions_digest = unbound_digest(&[&key]);
        reference = store
            .promise_change(
                WORKSPACE,
                &alice,
                &reference.promise_id,
                PromiseChangeAuthority::Steward,
                &request,
                &[PromiseRepositoryAdmissionV1::unbound(&key)],
                &current(),
                number,
            )
            .unwrap()
            .reference;
    }
    let next_key = "synthetic/repo65";
    let mut overflow = change(
        "bounded_lifetime_overflow",
        reference.version,
        &reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[next_key], "overflow"),
        },
    );
    overflow.expected_admissions_digest = unbound_digest(&[next_key]);
    assert!(
        store
            .promise_change(
                WORKSPACE,
                &alice,
                &reference.promise_id,
                PromiseChangeAuthority::Steward,
                &overflow,
                &[PromiseRepositoryAdmissionV1::unbound(next_key)],
                &current(),
                65,
            )
            .is_err()
    );
    assert_eq!(
        store
            .promise_get(WORKSPACE, &reference.promise_id, None, &current())
            .unwrap()
            .unwrap()
            .current
            .reference,
        reference
    );
    assert_eq!(db.scalar("SELECT count(*) FROM hub_promise_operations"), 64);
}

fn retained_key_cannot_acquire_a_different_source_identity(db: &Db) {
    use agentscm::hub::promise::PromiseForgeAdmissionV1;

    let store = db.open();
    let alice = user(&*store, 98);
    let admitted = |numeric| PromiseRepositoryAdmissionV1 {
        repository_id: REPO_ONE.into(),
        forge: Some(PromiseForgeAdmissionV1 {
            provider: "github".into(),
            host: "github.com".into(),
            forge_repository_id: numeric,
        }),
        native_repository_ids: vec![],
    };
    let a = admitted(101);
    let b = admitted(202);
    let first = PromiseCreateV1 {
        operation_id: "retained_key_initial".into(),
        expected_admissions_digest: admission_digest(std::slice::from_ref(&a)),
        definition: definition(&[REPO_ONE], "first"),
    };
    let mut reference = store
        .promise_create(WORKSPACE, &alice, &first, std::slice::from_ref(&a), 1)
        .unwrap()
        .reference;
    let mut remove = change(
        "retained_key_remove",
        reference.version,
        &reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_TWO], "removed"),
        },
    );
    remove.expected_admissions_digest = unbound_digest(&[REPO_TWO]);
    reference = store
        .promise_change(
            WORKSPACE,
            &alice,
            &reference.promise_id,
            PromiseChangeAuthority::Steward,
            &remove,
            &[PromiseRepositoryAdmissionV1::unbound(REPO_TWO)],
            &PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), a.clone())]),
            2,
        )
        .unwrap()
        .reference;
    let mut readd = change(
        "retained_key_readd",
        reference.version,
        &reference.digest,
        PromiseChangeActionV1::Amend {
            content: content(&[REPO_ONE], "readded"),
        },
    );
    readd.expected_admissions_digest = admission_digest(std::slice::from_ref(&b));
    let b_map = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), b.clone())]);
    assert!(
        store
            .promise_change(
                WORKSPACE,
                &alice,
                &reference.promise_id,
                PromiseChangeAuthority::Steward,
                &readd,
                &[b],
                &b_map,
                3,
            )
            .is_err()
    );
    let a_map = PromiseCurrentAdmissionsV1::from([(REPO_ONE.into(), a.clone())]);
    assert!(
        store
            .promise_history(WORKSPACE, &reference.promise_id, None, None, 10, &a_map)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .promise_history(WORKSPACE, &reference.promise_id, None, None, 10, &b_map)
            .unwrap()
            .is_none()
    );
}

fn malformed_input_admission_is_hidden_like_missing(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 99);
    let proposal = PromiseInputV1::Proposal {
        operation_id: "malformed_admission_proposal".into(),
        repository_id: REPO_ONE.into(),
        original_request: "Keep customer settings.".into(),
        commitment: "Keep settings stable.".into(),
        criteria: definition(&[REPO_ONE], "proposal").criteria,
        constraints: vec![],
    };
    let proposed = store
        .test_promise_input(WORKSPACE, &alice, None, &proposal, 1)
        .unwrap();
    let create = PromiseCreateV1 {
        operation_id: "malformed_admission_promise".into(),
        expected_admissions_digest: unbound_digest(&[REPO_ONE]),
        definition: definition(&[REPO_ONE], "report"),
    };
    let created = store
        .test_promise_create(WORKSPACE, &alice, &create, 2)
        .unwrap();
    let report = PromiseInputV1::Report {
        operation_id: "malformed_admission_report".into(),
        promise_id: created.reference.promise_id.clone(),
        promise_version: 1,
        promise_digest: created.reference.digest.clone(),
        repository_id: REPO_ONE.into(),
        summary: "Observed behavior.".into(),
    };
    let reported = store
        .test_promise_input(WORKSPACE, &alice, None, &report, 3)
        .unwrap();
    let mut invalid_stored_report = serde_json::to_value(&report).unwrap();
    invalid_stored_report["summary"] = json!("");
    db.set_promise_input_payload(&reported.input_id, &invalid_stored_report.to_string());
    assert!(matches!(
        store.promise_input_get(WORKSPACE, &reported.input_id, None, &current()),
        Err(HubError::Unavailable(_))
    ));
    assert!(matches!(
        store.test_promise_inputs(WORKSPACE, "report", &created.reference.promise_id, None, 10),
        Err(HubError::Unavailable(_))
    ));
    let invalid_new_report = PromiseInputV1::Report {
        operation_id: "invalid_new_report".into(),
        promise_id: created.reference.promise_id.clone(),
        promise_version: created.reference.version,
        promise_digest: created.reference.digest.clone(),
        repository_id: REPO_ONE.into(),
        summary: "".into(),
    };
    assert!(matches!(
        store.test_promise_input(WORKSPACE, &alice, None, &invalid_new_report, 4),
        Err(HubError::Invalid(_))
    ));
    db.set_promise_input_payload(&reported.input_id, &serde_json::to_string(&report).unwrap());
    db.set_promise_input_admission(&proposed.input_id, "{");
    db.set_promise_input_admission(&reported.input_id, "{");
    for id in [&proposed.input_id, &reported.input_id] {
        assert!(
            store
                .promise_input_get(WORKSPACE, id, None, &current())
                .unwrap()
                .is_none()
        );
    }
    assert!(
        store
            .promise_input_get(WORKSPACE, "prp_missing", None, &current())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .test_promise_inputs(WORKSPACE, "proposal", REPO_ONE, None, 10)
            .unwrap()
            .inputs
            .is_empty()
    );
    assert!(matches!(
        store.test_promise_inputs(
            WORKSPACE,
            "proposal",
            REPO_ONE,
            Some(&proposed.input_id),
            10
        ),
        Err(HubError::Invalid(_))
    ));
    assert!(
        store
            .test_promise_inputs(WORKSPACE, "report", &created.reference.promise_id, None, 10)
            .unwrap()
            .inputs
            .is_empty()
    );
    assert!(matches!(
        store.test_promise_inputs(
            WORKSPACE,
            "report",
            &created.reference.promise_id,
            Some(&reported.input_id),
            10
        ),
        Err(HubError::Invalid(_))
    ));
}

fn scalar_current_scopes_are_hidden_before_payload_decode(db: &Db) {
    use agentscm::hub::HubError;

    let store = db.open();
    let alice = user(&*store, 100);
    let mut ids = Vec::new();
    for index in 0..7 {
        let receipt = store
            .test_promise_create(
                WORKSPACE,
                &alice,
                &PromiseCreateV1 {
                    operation_id: format!("scalar_scope_{index}"),
                    expected_admissions_digest: unbound_digest(&[REPO_ONE]),
                    definition: definition(&[REPO_ONE], &format!("scope {index}")),
                },
                index + 1,
            )
            .unwrap();
        ids.push(receipt.reference.promise_id);
    }
    ids.sort();
    if db.pg.is_some() {
        db.exec("DROP TRIGGER hub_promise_versions_no_change ON hub_promise_versions");
    } else {
        db.exec("DROP TRIGGER hub_promise_versions_no_update");
    }
    let malformed_scopes = [
        json!(["plain string"]).to_string(),
        json!([r#"{"repository_id":"synthetic/project"}"#]).to_string(),
        json!([null]).to_string(),
        json!([7]).to_string(),
        json!([{"repository_id": 7}]).to_string(),
        json!([{"other": REPO_ONE}]).to_string(),
    ];
    let grants = [REPO_ONE.to_owned()];
    for (id, scope) in ids.iter().zip(malformed_scopes) {
        db.set_promise_version_scope(id, &scope);
        assert!(
            store
                .promise_get(WORKSPACE, id, None, &current())
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .promise_get(WORKSPACE, id, Some(&grants), &current())
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.promise_list(WORKSPACE, None, Some(id), 1, &current()),
            Err(HubError::Invalid(_))
        ));
    }
    let visible_id = ids.last().unwrap();
    for permitted in [None, Some(grants.as_slice())] {
        let page = store
            .promise_list(WORKSPACE, permitted, None, 1, &current())
            .unwrap();
        assert_eq!(page.promises.len(), 1);
        assert_eq!(&page.promises[0].current.reference.promise_id, visible_id);
        assert!(page.next.is_none());
    }

    let mut wrong_digest = serde_json::to_value(
        store
            .promise_get(WORKSPACE, visible_id, None, &current())
            .unwrap()
            .unwrap()
            .current,
    )
    .unwrap();
    wrong_digest["definition"]["commitment"] = json!("Changed without a matching digest");
    db.set_promise_version_payload(visible_id, &wrong_digest.to_string());
    assert!(
        store
            .promise_get(WORKSPACE, visible_id, Some(&grants), &current())
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .promise_list(WORKSPACE, Some(&grants), None, 1, &current())
            .unwrap()
            .promises
            .is_empty()
    );
    assert!(matches!(
        store.promise_list(WORKSPACE, None, Some(visible_id), 1, &current()),
        Err(HubError::Invalid(_))
    ));
}

macro_rules! parity {
    ($($case:ident),* $(,)?) => {
        mod sqlite {
            $(#[test] fn $case() { super::$case(&super::Db::sqlite()); })*
        }
        mod on_postgres {
            $(#[test] fn $case() {
                if let Some(db) = super::Db::postgres() { super::$case(&db); }
            })*
        }
    };
}

parity!(
    lifecycle_and_store_parity,
    combined_original_and_mutable_definition_limit_is_atomic,
    proposal_report_and_rollback,
    migration_upgrades_a_populated_store,
    concurrent_identical_operation_replays_one_receipt,
    input_pagination_and_scope,
    admission_remap_fences_versions_inputs_cursors_and_replay,
    admission_pages_skip_hidden_rows_without_exposing_cursors,
    hidden_corrupt_cursors_match_missing_and_visible_corruption_surfaces,
    invalid_or_transplanted_payload_cannot_narrow_original_scope,
    self_invalid_payload_admissions_hide_before_integrity,
    typed_numeric_source_admissions_follow_retained_encoding,
    incomplete_index_and_scope_cannot_narrow_a_two_repository_promise,
    scoped_list_skips_damaged_narrowed_row_and_fills_pages,
    qualified_malformed_current_scope_is_an_integrity_error,
    damaged_historical_version_hides_whole_history_and_report_page,
    reordered_valid_scope_remains_visible_before_payload_decode,
    missing_sole_index_is_undiscoverable_but_authorized_reads_detect_it,
    same_repository_key_in_two_workspaces_stays_isolated,
    mixed_case_repository_keys_keep_exact_index_order,
    malformed_string_keyed_noncurrent_scope_is_hidden,
    remapped_source_does_not_expose_retained_admission_through_metadata,
    stale_report_admission_hides_damaged_current_promise,
    narrowed_lifetime_cannot_expose_historical_two_key_version,
    scalar_current_scopes_are_hidden_before_payload_decode,
    lifetime_scope_is_bounded_and_failed_extension_is_atomic,
    retained_key_cannot_acquire_a_different_source_identity,
    malformed_input_admission_is_hidden_like_missing,
);

fn http_definition(repositories: &[&str], suffix: &str) -> Value {
    serde_json::to_value(definition(repositories, suffix)).unwrap()
}

fn http_content(repositories: &[&str], suffix: &str) -> Value {
    let mut content = http_definition(repositories, suffix);
    content.as_object_mut().unwrap().remove("original_request");
    content
}

#[tokio::test]
async fn workspace_authority_hides_cross_repository_promises_and_recovers_stewardship() {
    let env = workspace_hub::Env::new().await;
    let (_, owner_cookie) = env.signin(101).await;
    let owner_session = env.session(&owner_cookie).await;
    let owner_csrf = owner_session["csrf_token"].as_str().unwrap();
    let (status, one_admission) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"repository_ids":[REPO_ONE]})),
        )
        .await;
    assert_eq!(status, 200, "{one_admission}");
    assert_eq!(one_admission["admissions"][0]["repository_id"], REPO_ONE);
    let one_digest = one_admission["digest"].as_str().unwrap();
    assert_eq!(one_digest.len(), 64);
    let (status, two_admissions) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"repository_ids":[REPO_TWO,REPO_ONE]})),
        )
        .await;
    assert_eq!(status, 200, "{two_admissions}");
    let two_digest = two_admissions["digest"].as_str().unwrap();
    assert_eq!(two_admissions["admissions"].as_array().unwrap().len(), 2);
    assert_ne!(one_digest, two_digest);
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&owner_cookie),
            None,
            None,
            Some(json!({"repository_ids":[REPO_ONE]})),
        )
        .await;
    assert_ne!(status, 200);

    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"strict_unknown","expected_admissions_digest":one_digest,"definition":http_definition(&[REPO_ONE],"strict"),"unexpected":true})),
        )
        .await;
    assert_eq!(status, 400);
    let mut oversized = http_definition(&[REPO_ONE], "oversized");
    oversized["commitment"] = json!("x".repeat(16 * 1024 + 1));
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"strict_oversized","expected_admissions_digest":one_digest,"definition":oversized})),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises?limit=101",
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 400);

    let (status, created) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"create_http_01","expected_admissions_digest":two_digest,"definition":http_definition(&[REPO_ONE,REPO_TWO],"http")})),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    let promise = created["ref"]["promise_id"].as_str().unwrap().to_owned();
    let (status, single) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"create_http_02","expected_admissions_digest":one_digest,"definition":http_definition(&[REPO_ONE],"single")})),
        )
        .await;
    assert_eq!(status, 200, "{single}");
    let single_promise = single["ref"]["promise_id"].as_str().unwrap().to_owned();
    let (status, historical) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({"operation_id":"create_http_history","expected_admissions_digest":two_digest,"definition":http_definition(&[REPO_ONE,REPO_TWO],"historical")})),
        )
        .await;
    assert_eq!(status, 200, "{historical}");
    let historical_promise = historical["ref"]["promise_id"].as_str().unwrap().to_owned();
    let (status, _) = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises?after=bad%20cursor",
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}/history?before_version=0"),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 400);

    let generation = owner_session["workspace"]["generation"].as_i64().unwrap();
    let member = json!({
        "request_id":"member_202",
        "expected_generation":generation,
        "identity":env.identity(202),
        "roles":["administrator","reader","contributor"],
        "repository_ids":[REPO_ONE],
        "enabled":true
    });
    let (status, body) = env
        .request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(member),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, member_cookie) = env.signin(202).await;
    let member_session = env.session(&member_cookie).await;
    let member_csrf = member_session["csrf_token"].as_str().unwrap();
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(json!({"repository_ids":[REPO_TWO]})),
        )
        .await;
    assert_eq!(status, 404);
    let (status, member_admission) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/admission-preflight",
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(json!({"repository_ids":[REPO_ONE]})),
        )
        .await;
    assert_eq!(status, 200, "{member_admission}");
    let member_digest = member_admission["digest"].as_str().unwrap();
    assert_eq!(member_digest, one_digest);

    let historical_current = env
        .store
        .test_promise_get("synthetic-workspace", &historical_promise)
        .unwrap()
        .unwrap()
        .current;
    let (status, narrowed) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{historical_promise}/changes"),
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(json!({
                "operation_id":"narrow_historical_scope",
                "expected_version":historical_current.reference.version,
                "expected_digest":historical_current.reference.digest,
                "expected_admissions_digest":one_digest,
                "reason":"The second repository is no longer relevant.",
                "action":{"kind":"amend","content":http_content(&[REPO_ONE],"historical narrowed")}
            })),
        )
        .await;
    assert_eq!(status, 200, "{narrowed}");
    let (status, body) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{historical_promise}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{historical_promise}/history?limit=1"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{historical_promise}/history?before_version=1"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 400);

    let (unknown_status, _) = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises/prm_unknown",
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    let (hidden_status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{promise}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!((unknown_status, hidden_status), (404, 404));
    let (status, page) = env
        .request(
            Method::GET,
            "/v1/hub/workspace/promises",
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{page}");
    let visible = page["promises"].as_array().unwrap();
    assert_eq!(visible.len(), 2);
    assert!(
        visible
            .iter()
            .any(|view| view["current"]["ref"] == single["ref"])
    );
    assert!(
        visible
            .iter()
            .any(|view| view["current"]["ref"] == narrowed["ref"])
    );
    assert!(
        visible
            .iter()
            .all(|view| view["current"]["ref"]["promise_id"] != promise)
    );
    assert!(page["next"].is_null());

    let member_user = member_session["actor"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let single_current = env
        .store
        .test_promise_get("synthetic-workspace", &single_promise)
        .unwrap()
        .unwrap()
        .current;
    let unauthorized_change = json!({
        "operation_id":"reader_change",
        "expected_version":single_current.reference.version,
        "expected_digest":single_current.reference.digest,
        "expected_admissions_digest":member_digest,
        "reason":"Not the steward.",
        "action":{"kind":"retire"}
    });
    let (status, body) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{single_promise}/changes"),
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(unauthorized_change),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, capabilities) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}/capabilities"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{capabilities}");
    assert_eq!(capabilities["amend"], false);
    assert_eq!(capabilities["recovery_transfer"], false);
    let (status, transfer_created) = env
        .request(Method::POST, "/v1/hub/workspace/promises", Some(&owner_cookie), Some(owner_csrf), None,
            Some(json!({"operation_id":"transfer_create","expected_admissions_digest":one_digest,"definition":http_definition(&[REPO_ONE],"transfer")})))
        .await;
    assert_eq!(status, 200, "{transfer_created}");
    let transfer_promise = transfer_created["ref"]["promise_id"].as_str().unwrap();
    let transfer_request = json!({
        "operation_id":"transfer_lost_response",
        "expected_version":transfer_created["ref"]["version"],
        "expected_digest":transfer_created["ref"]["digest"],
        "expected_admissions_digest":one_digest,
        "reason":"Owner handoff.",
        "action":{"kind":"transfer","to_user":member_user}
    });
    let transfer_url = format!("/v1/hub/workspace/promises/{transfer_promise}/changes");
    let (status, transferred) = env
        .request(
            Method::POST,
            &transfer_url,
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(transfer_request.clone()),
        )
        .await;
    assert_eq!(status, 200, "{transferred}");
    let (status, transfer_replay) = env
        .request(
            Method::POST,
            &transfer_url,
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(transfer_request.clone()),
        )
        .await;
    assert_eq!(status, 200, "{transfer_replay}");
    assert_eq!(transfer_replay["replayed"], true);
    assert_eq!(transfer_replay["ref"], transferred["ref"]);
    let mut transfer_changed = transfer_request.clone();
    transfer_changed["reason"] = json!("Different handoff.");
    let (status, _) = env
        .request(
            Method::POST,
            &transfer_url,
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(transfer_changed),
        )
        .await;
    assert_eq!(status, 409);

    let (token_id, secret) =
        mint_agent_token(&*env.store, &member_user, "promise-agent", REPO_ONE, 3600).unwrap();
    let (status, proposal) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            None,
            None,
            Some(secret.expose()),
            Some(json!({"kind":"proposal","operation_id":"agent_proposal","repository_id":REPO_ONE,"original_request":"Keep the behavior.","commitment":"Keep the behavior.","criteria":[{"id":"behavior","text":"The behavior remains.","evidence_requirements":[]}],"constraints":[]})),
        )
        .await;
    assert_eq!(status, 200, "{proposal}");
    let (status, hidden_proposal) = env.request(Method::POST, "/v1/hub/workspace/promises/inputs", Some(&owner_cookie), Some(owner_csrf), None,
        Some(json!({"kind":"proposal","operation_id":"owner_repo_two_proposal","repository_id":REPO_TWO,
            "original_request":"Keep the second behavior.","commitment":"Keep it.",
            "criteria":[{"id":"second_behavior","text":"Behavior remains.","evidence_requirements":[]}],"constraints":[]}))).await;
    assert_eq!(status, 200, "{hidden_proposal}");
    let hidden_input_id = hidden_proposal["input_id"].as_str().unwrap();
    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/inputs/{hidden_input_id}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let proposal_id = proposal["input_id"].as_str().unwrap();
    let (status, preview) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/inputs/{proposal_id}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["payload"]["kind"], "proposal");
    assert_eq!(preview["submitted_by_agent"], true);
    assert!(preview.get("actor_token").is_none());
    let (status, page) = env
        .request(
            Method::GET,
            &format!(
                "/v1/hub/workspace/promises/inputs/proposals?repository_id={REPO_ONE}&limit=1"
            ),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["inputs"][0]["input_id"], proposal_id);
    let (status, ungranted) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/inputs/proposals?repository_id={REPO_TWO}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{ungranted}");
    assert_eq!(ungranted["inputs"], json!([]));
    assert!(ungranted["next"].is_null());
    let (status, ungranted_after) = env
        .request(Method::GET, &format!("/v1/hub/workspace/promises/inputs/proposals?repository_id={REPO_TWO}&after=prp_unknown"), Some(&member_cookie), None, None, None)
        .await;
    assert_eq!(status, 200, "{ungranted_after}");
    assert_eq!(ungranted_after, ungranted);
    for (suffix, expected) in [("", &ungranted), ("&after=prp_unknown", &ungranted_after)] {
        let (status, missing) = env
            .request(
                Method::GET,
                &format!(
                    "/v1/hub/workspace/promises/inputs/proposals?repository_id=synthetic/missing{suffix}"
                ),
                Some(&member_cookie),
                None,
                None,
                None,
            )
            .await;
        assert_eq!(status, 200, "{missing}");
        assert_eq!(&missing, expected);
    }
    let (status, adopted) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/adopt",
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(
                json!({"operation_id":"adopt_visible_proposal","expected_admissions_digest":member_digest,"proposal_id":proposal_id,
            "definition":http_definition(&[REPO_ONE],"owner adopted")}),
            ),
        )
        .await;
    assert_eq!(status, 200, "{adopted}");
    let (status, adopted_preview) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/inputs/{proposal_id}"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{adopted_preview}");
    assert_eq!(
        adopted_preview["adopted_promise"],
        adopted["ref"]["promise_id"]
    );
    let (status, body) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises",
            None,
            None,
            Some(secret.expose()),
            Some(json!({"operation_id":"agent_create","expected_admissions_digest":member_digest,"definition":http_definition(&[REPO_ONE],"agent")})),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, report) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            None,
            None,
            Some(secret.expose()),
            Some(json!({"kind":"report","operation_id":"agent_report","promise_id":single_promise,"promise_version":single_current.reference.version,"promise_digest":single_current.reference.digest,"repository_id":REPO_ONE,"summary":"Possible issue; caller report only."})),
        )
        .await;
    assert_eq!(status, 200, "{report}");
    let (status, reports) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}/reports"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{reports}");
    assert_eq!(reports["inputs"][0]["input_id"], report["input_id"]);
    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{historical_promise}/reports"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 404);
    let read_only_secret = generate_secret().unwrap();
    env.store
        .insert_agent_token(&NewAgentToken {
            owner: member_user.clone(),
            name: "promise-reader".into(),
            repository: REPO_ONE.into(),
            digest: agent_token_digest(read_only_secret.expose()).unwrap(),
            expires_at: unix_now() + 3600,
            repository_id: None,
            read_only: true,
        })
        .unwrap();
    let (status, body) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}"),
            None,
            None,
            Some(read_only_secret.expose()),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            None,
            None,
            Some(read_only_secret.expose()),
            Some(json!({"kind":"proposal","operation_id":"reader_proposal","repository_id":REPO_ONE,"original_request":"Keep the behavior.","commitment":"Keep the behavior.","criteria":[{"id":"behavior","text":"The behavior remains.","evidence_requirements":[]}],"constraints":[]})),
        )
        .await;
    assert_eq!(status, 403);
    assert!(
        env.store
            .revoke_owned_agent_token(&member_user, &token_id)
            .unwrap()
    );
    let (status, _) = env
        .request(
            Method::POST,
            "/v1/hub/workspace/promises/inputs",
            None,
            None,
            Some(secret.expose()),
            Some(json!({"kind":"proposal","operation_id":"revoked_proposal","repository_id":REPO_ONE,"original_request":"Keep the behavior.","commitment":"Keep the behavior.","criteria":[{"id":"behavior","text":"The behavior remains.","evidence_requirements":[]}],"constraints":[]})),
        )
        .await;
    assert_eq!(status, 401);

    let latest_owner = env.session(&owner_cookie).await;
    let disable = json!({
        "request_id":"disable_owner",
        "expected_generation":latest_owner["workspace"]["generation"],
        "identity":env.identity(101),
        "roles":["administrator","reader","contributor"],
        "repository_ids":["*"],
        "enabled":false
    });
    let (status, body) = env
        .request(
            Method::PUT,
            "/v1/hub/workspace/members",
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(disable),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let (status, _) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}"),
            Some(&owner_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 401);
    let (status, _) = env
        .request(
            Method::POST,
            &transfer_url,
            Some(&owner_cookie),
            Some(owner_csrf),
            None,
            Some(transfer_request),
        )
        .await;
    assert_eq!(status, 401);

    let current_single = env
        .store
        .test_promise_get("synthetic-workspace", &single_promise)
        .unwrap()
        .unwrap()
        .current;
    let (status, capabilities) = env
        .request(
            Method::GET,
            &format!("/v1/hub/workspace/promises/{single_promise}/capabilities"),
            Some(&member_cookie),
            None,
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "{capabilities}");
    assert_eq!(capabilities["recovery_transfer"], true);
    assert_eq!(capabilities["amend"], false);
    let recovery_single = json!({
        "operation_id":"recover_http_success",
        "expected_version":current_single.reference.version,
        "expected_digest":current_single.reference.digest,
        "expected_admissions_digest":member_digest,
        "reason":"The prior steward is no longer eligible.",
        "action":{"kind":"recovery_transfer","to_user":member_user}
    });
    let (status, body) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{single_promise}/changes"),
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(recovery_single.clone()),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ref"]["version"], json!(2));
    let (status, replay) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{single_promise}/changes"),
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(recovery_single.clone()),
        )
        .await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["ref"], body["ref"]);
    let mut altered = recovery_single.clone();
    altered["reason"] = json!("Different reason");
    let (status, _) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{single_promise}/changes"),
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(altered),
        )
        .await;
    assert_eq!(status, 409);

    let current = env
        .store
        .test_promise_get("synthetic-workspace", &promise)
        .unwrap()
        .unwrap()
        .current;
    let recovery = json!({
        "operation_id":"recover_http_01",
        "expected_version":current.reference.version,
        "expected_digest":current.reference.digest,
        "expected_admissions_digest":two_digest,
        "reason":"The prior steward is no longer eligible.",
        "action":{"kind":"recovery_transfer","to_user":member_user}
    });
    let (status, body) = env
        .request(
            Method::POST,
            &format!("/v1/hub/workspace/promises/{promise}/changes"),
            Some(&member_cookie),
            Some(member_csrf),
            None,
            Some(recovery),
        )
        .await;
    // The recipient lacks repo_two, so recovery fails closed without content change.
    assert_eq!(status, 404, "{body}");
    let after = env
        .store
        .test_promise_get("synthetic-workspace", &promise)
        .unwrap()
        .unwrap()
        .current;
    assert_eq!(after.reference.version, current.reference.version);
}
