//! Source changes, enqueue attempts and model acknowledgements are separate durable facts.
use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::connection::NewConnection;
use agentscm::hub::notices::{
    AttemptOutcome, DependencyNoticeAck, DependencyRefresh, NoticeOfferRequest, NoticeRecipient,
    NoticeSource, NoticeTransportReport, SourceKind,
};
use agentscm::hub::session_lifecycle::LifecycleRegister;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, User, mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::hub::work::{WorkItem, WorkRole, WorkStart};
use agentscm::hub::work_links::WorkPrLinkTarget;
use agentscm::hub::work_reconciliation::{
    PrerequisiteCondition, PrerequisiteRequirement, WorkDeclarations, WorkDeclarationsUpdate,
};
use serde_json::json;

const REPO: &str = "acme/widgets";
const REPO_ID: i64 = 101;
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn scope() -> CatchupRepositoryScope {
    CatchupRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id: Some(REPO_ID),
    }
}

fn start(store: &dyn Store, token: &str, key: &str) -> WorkItem {
    store
        .work_start(
            token,
            &WorkStart {
                issue_choice: None,
                work_id: None,
                source: None,
                request_key: Some(key.into()),
                title: format!("Synthetic {key}"),
                description: None,
                session: key.into(),
                role: WorkRole::Lead,
                scope: vec![format!("src/{key}.rs")],
                branch: None,
                worktree: None,
                next_action: None,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            unix_now(),
        )
        .unwrap()
        .item
}

fn register(store: &dyn Store, token: &str, session: &str) -> String {
    store
        .lifecycle_register(
            token,
            &LifecycleRegister {
                session: session.into(),
                start_nonce: format!("nonce-{session}"),
                operation_id: format!("register-{session}"),
                observed_at: unix_now(),
                expected_incarnation: None,
                qualified: false,
            },
            unix_now(),
        )
        .unwrap()
        .incarnation
}

fn coverage(store: &dyn Store, state: CatchupCoverageState, at: i64) {
    for source in [
        CatchupMergeSource::TrackedPublication,
        CatchupMergeSource::ForgePull,
    ] {
        store
            .record_catchup_source_status(
                &scope(),
                &CatchupSourceStatus {
                    source,
                    state,
                    checked_at: at,
                    unread_pages: 0,
                    deferred: 0,
                    window_from: Some(at - 86400),
                },
            )
            .unwrap();
    }
}

fn fixture(database: &str) -> Fixture {
    let store = store::open(database).unwrap();
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 77,
            },
            "reader",
        )
        .unwrap();
    let (token, _, _) =
        mint_bound_agent_token(&*store, &owner.id, "test-agent", REPO, REPO_ID, 3600, false)
            .unwrap();
    let connection = store
        .insert_connection(
            &NewConnection {
                id: new_connection_id().unwrap(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("reader".into()),
            },
            &owner.id,
        )
        .unwrap();
    let repo = store
        .upsert_repository(&connection.id, "github.com", REPO_ID, REPO, None, None)
        .unwrap();
    let now = unix_now();
    coverage(&*store, CatchupCoverageState::ObservedOnly, now);
    store
        .observe(
            &repo.id,
            "pull_request",
            "1",
            &json!({"number":1,"state":"open","head_sha":HEAD}).to_string(),
            1000,
        )
        .unwrap();
    let prerequisite = start(&*store, &token, "prerequisite");
    let dependent = start(&*store, &token, "dependent");
    let incarnation = register(&*store, &token, "dependent");
    store
        .work_declarations_set(
            &token,
            &WorkDeclarationsUpdate {
                work_id: dependent.id.clone(),
                operation_id: "declare".into(),
                generation: dependent.generation,
                session: "dependent".into(),
                expected_updated_seq: dependent.updated_seq,
                declarations: WorkDeclarations {
                    links: vec![],
                    prerequisites: vec![PrerequisiteCondition {
                        id: "merge".into(),
                        requirement: PrerequisiteRequirement::MergedPullRequest {
                            work_id: prerequisite.id.clone(),
                            target: WorkPrLinkTarget {
                                repository_id: REPO_ID,
                                number: 1,
                                head_sha: HEAD.into(),
                            },
                        },
                    }],
                },
            },
            now,
        )
        .unwrap();
    let query = DependencyRefresh {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        after: None,
        limit: 10,
    };
    Fixture {
        store,
        owner,
        token,
        dependent,
        incarnation,
        now,
        query,
    }
}

struct Fixture {
    store: std::sync::Arc<dyn Store>,
    owner: User,
    token: String,
    dependent: WorkItem,
    incarnation: String,
    now: i64,
    query: DependencyRefresh,
}

fn contract(database: &str) {
    let Fixture {
        store,
        owner,
        token,
        dependent,
        incarnation,
        now,
        query,
    } = fixture(database);
    assert!(
        store
            .notice_refresh(&token, &query, now)
            .unwrap()
            .notices
            .is_empty(),
        "baseline is not a success event"
    );
    coverage(&*store, CatchupCoverageState::Unavailable, now + 1);
    let first = store.notice_refresh(&token, &query, now + 1).unwrap();
    assert_eq!(first.notices.len(), 1);
    coverage(&*store, CatchupCoverageState::ObservedOnly, now + 2);
    let second = store.notice_refresh(&token, &query, now + 2).unwrap();
    assert_eq!(second.notices.len(), 1);
    assert_ne!(
        first.notices[0].id, second.notices[0].id,
        "A-B-A has distinct change versions"
    );
    coverage(&*store, CatchupCoverageState::ObservedOnly, now + 3);
    let repeat = store.notice_refresh(&token, &query, now + 3).unwrap();
    assert_eq!(
        second.notices[0].id, repeat.notices[0].id,
        "timestamps do not create another logical delivery"
    );
    assert_eq!(second.notices[0].deadline_at, repeat.notices[0].deadline_at);
    store
        .record_catchup_merge(
            &scope(),
            &NewCatchupMerge {
                source: CatchupMergeSource::ForgePull,
                source_id: "forge-pr-1".into(),
                occurred_at: Some(now),
                title: None,
                branch: "feature/1".into(),
                base: "main".into(),
                head_commit: HEAD.into(),
                merge_commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                pull_number: 1,
                paths: vec!["src/prerequisite.rs".into()],
                paths_complete: true,
                same_repository_head: true,
            },
            now + 4,
        )
        .unwrap();
    let merged = store.notice_refresh(&token, &query, now + 4).unwrap();
    assert_eq!(
        merged.notices.len(),
        1,
        "forge facts refresh without a Work mutation"
    );
    let offer_query = NoticeOfferRequest {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        automatic: true,
    };
    let offer = store
        .notice_offer(&token, &offer_query, now + 4)
        .unwrap()
        .unwrap();
    let report = NoticeTransportReport {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        delivery_id: offer.delivery_id.clone(),
        attempt_id: offer.attempt_id.unwrap(),
        outcome: AttemptOutcome::Unknown,
        observed_at: now - 5,
    };
    store.notice_transport(&token, &report, now + 4).unwrap();
    store.notice_transport(&token, &report, now + 4).unwrap();
    let reopened = store::open(database).unwrap();
    assert!(
        reopened
            .notice_offer(&token, &offer_query, now + 70)
            .unwrap()
            .is_none(),
        "unknown enqueue survives restart without resend"
    );
    let ordinary = NoticeOfferRequest {
        automatic: false,
        ..offer_query.clone()
    };
    assert!(
        reopened
            .notice_offer(&token, &ordinary, now + 70)
            .unwrap()
            .is_some(),
        "ordinary next-call pointer may recover uptake"
    );
    let record = &merged.notices[0];
    let declarations = reopened
        .work_declarations_get(&token, &dependent.id, now + 70)
        .unwrap()
        .unwrap();
    let ack = DependencyNoticeAck {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        delivery_id: record.id.clone(),
        work_id: dependent.id,
        generation: dependent.generation,
        declaration_seq: declarations.declaration_seq,
        semantic_version: 3,
    };
    assert!(
        reopened
            .dependency_notice_ack(&token, &ack, now + 70)
            .unwrap()
            .newly_acknowledged
    );
    assert!(
        !reopened
            .dependency_notice_ack(&token, &ack, now + 70)
            .unwrap()
            .newly_acknowledged
    );
    let mut wrong = ack.clone();
    wrong.incarnation = "wrong".into();
    assert!(
        reopened
            .dependency_notice_ack(&token, &wrong, now + 70)
            .is_err()
    );

    let source = NoticeSource {
        kind: SourceKind::Operational,
        id: "synthetic-episode".into(),
        version: "1".into(),
    };
    let original = reopened
        .notice_record(
            &token,
            &source,
            &NoticeRecipient {
                token: token.clone(),
                session: "dependent".into(),
                work_id: None,
                generation: None,
            },
            now + 80,
        )
        .unwrap();
    assert!(
        reopened
            .notice_offer(&token, &offer_query, now + 80)
            .unwrap()
            .is_some()
    );
    let (rotated, _, _) = mint_bound_agent_token(
        &*reopened,
        &owner.id,
        "replacement",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let other_incarnation = register(&*reopened, &rotated, "dependent");
    let replacement = reopened
        .notice_record(
            &rotated,
            &source,
            &NoticeRecipient {
                token: rotated.clone(),
                session: "dependent".into(),
                work_id: None,
                generation: None,
            },
            now + 81,
        )
        .unwrap();
    assert_ne!(original.id, replacement.id);
    assert!(
        !replacement.automatic_eligible(),
        "token rotation preserves prior potentially-enqueued budget"
    );
    assert!(
        reopened
            .notice_offer(
                &rotated,
                &NoticeOfferRequest {
                    session: "dependent".into(),
                    incarnation: other_incarnation,
                    automatic: true
                },
                now + 81
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn sqlite_source_delivery_contract() {
    let directory = tempfile::tempdir().unwrap();
    contract(&format!(
        "sqlite://{}",
        directory.path().join("hub.sqlite").display()
    ));
}

#[test]
fn postgres_source_delivery_contract() {
    let Some(schema) = crate::hub_pg::PgSchema::create() else {
        return;
    };
    contract(&schema.url);
}

fn human_consent_contract(database: &str) {
    use agentscm::hub::agent_messages::{
        Answer, Message, MessageState, Mode, answer_with_transport,
    };
    let store = store::open(database).unwrap();
    let owner = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 88,
            },
            "consent-reader",
        )
        .unwrap();
    let (token, secret, _) = mint_bound_agent_token(
        &*store,
        &owner.id,
        "message-agent",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let who = store
        .authenticate(
            &store::agent_token_digest(secret.expose()).unwrap(),
            unix_now(),
        )
        .unwrap()
        .unwrap();
    let incarnation = register(&*store, &token, "messages");
    let now = unix_now();
    let setting = format!("agent_messages:{}:{REPO_ID}", owner.id);
    let mode_key = format!("agent_message_mode:{}:{REPO_ID}", owner.id);
    let source = NoticeSource {
        kind: SourceKind::HumanMessage,
        id: "msg_consent".into(),
        version: "1".into(),
    };
    let mut message = Message {
        id: source.id.clone(),
        token: token.clone(),
        session: "messages".into(),
        work_id: None,
        note: "Synthetic consent note".into(),
        from: "reader".into(),
        state: MessageState::Delivered,
        via: Some("notice".into()),
        mode: Some(Mode::Auto),
        created_at: now,
        delivered_at: Some(now),
        offered_at: Some(now),
        read_at: None,
        done_at: None,
        answered_at: None,
        deadline_at: Some(now + 600),
    };
    let original = serde_json::to_string(&vec![message.clone()]).unwrap();
    store.set_setting(&setting, &original, &owner.id).unwrap();
    store.set_setting(&mode_key, "\"auto\"", &owner.id).unwrap();
    let receipt = store
        .notice_record(
            &token,
            &source,
            &NoticeRecipient {
                token: token.clone(),
                session: "messages".into(),
                work_id: None,
                generation: None,
            },
            now,
        )
        .unwrap();
    store
        .set_setting(&mode_key, "\"ignore\"", &owner.id)
        .unwrap();
    let offer = NoticeOfferRequest {
        session: "messages".into(),
        incarnation: incarnation.clone(),
        automatic: true,
    };
    assert!(store.notice_offer(&token, &offer, now).unwrap().is_none());
    assert!(
        store
            .notice_pending(&token, "messages", now)
            .unwrap()
            .is_empty()
    );
    assert!(
        answer_with_transport(
            &*store,
            &who,
            "messages",
            &source.id,
            Answer::Accepted,
            Some(&incarnation),
            now
        )
        .is_err()
    );
    assert_eq!(
        store.get_setting(&setting).unwrap().as_deref(),
        Some(original.as_str())
    );
    store
        .set_setting(&mode_key, "\"manual\"", &owner.id)
        .unwrap();
    let restored = store.notice_offer(&token, &offer, now).unwrap().unwrap();
    assert_eq!(
        restored.delivery_id, receipt.id,
        "Ignore preserved pending attempt eligibility"
    );
    // The domain record still carries the old Auto delivery mode. Only the transaction's
    // current Manual setting may determine the acceptance authority returned to the caller.
    let accepted = answer_with_transport(
        &*store,
        &who,
        "messages",
        &source.id,
        Answer::Accepted,
        Some(&incarnation),
        now,
    )
    .unwrap()
    .text(&source.id);
    assert!(accepted.contains("their yes"), "{accepted}");
    assert!(!accepted.contains("act on it now"), "{accepted}");
    message.id = "msg_cas_mode".into();
    message.state = MessageState::Delivered;
    let before = serde_json::to_string(&vec![message.clone()]).unwrap();
    store.set_setting(&setting, &before, &owner.id).unwrap();
    message.state = MessageState::Accepted;
    let replacement = serde_json::to_string(&vec![message.clone()]).unwrap();
    let source = NoticeSource {
        id: message.id,
        ..source
    };
    let selected = store
        .notice_message_update(
            &token,
            "messages",
            &setting,
            Some(&before),
            &replacement,
            &source,
            Some(&incarnation),
            now,
        )
        .unwrap();
    assert_eq!(
        selected,
        Some(Mode::Manual),
        "CAS exposes the actual locked mode"
    );
    assert_eq!(
        store
            .notice_message_update(
                &token,
                "messages",
                &setting,
                Some(&before),
                &replacement,
                &source,
                Some(&incarnation),
                now
            )
            .unwrap(),
        None,
        "CAS conflict remains distinct from a selected mode"
    );
    let mut withdrawn: Message = serde_json::from_str::<Vec<Message>>(&replacement)
        .unwrap()
        .remove(0);
    withdrawn.id = "msg_withdrawn".into();
    withdrawn.state = MessageState::Delivered;
    let source = NoticeSource {
        kind: SourceKind::HumanMessage,
        id: withdrawn.id.clone(),
        version: "1".into(),
    };
    store
        .set_setting(
            &setting,
            &serde_json::to_string(&vec![withdrawn.clone()]).unwrap(),
            &owner.id,
        )
        .unwrap();
    store
        .notice_record(
            &token,
            &source,
            &NoticeRecipient {
                token: token.clone(),
                session: "messages".into(),
                work_id: None,
                generation: None,
            },
            now,
        )
        .unwrap();
    withdrawn.state = MessageState::Withdrawn;
    store
        .set_setting(
            &setting,
            &serde_json::to_string(&vec![withdrawn]).unwrap(),
            &owner.id,
        )
        .unwrap();
    assert!(
        store.notice_offer(&token, &offer, now).unwrap().is_none(),
        "withdrawal invalidates an already recorded source"
    );
}

#[test]
fn sqlite_human_notice_consent_contract() {
    let directory = tempfile::tempdir().unwrap();
    human_consent_contract(&format!(
        "sqlite://{}",
        directory.path().join("hub.sqlite").display()
    ));
}

#[test]
fn postgres_human_notice_consent_contract() {
    let Some(schema) = crate::hub_pg::PgSchema::create() else {
        return;
    };
    human_consent_contract(&schema.url);
}

#[test]
fn postgres_owner_and_recipient_refresh_serialize_semantic_versions() {
    use agentscm::hub::work_dependencies::DependencyNoticeRecord;
    use agentscm::hub::work_transition::WorkLifecycleReader;
    use postgres::{Client, NoTls};
    use std::time::{Duration, Instant};
    for has_baseline in [true, false] {
        let Some(schema) = crate::hub_pg::PgSchema::create() else {
            return;
        };
        let Fixture {
            store,
            owner,
            token,
            dependent,
            now,
            query,
            ..
        } = fixture(&schema.url);
        // Each PgStore serializes its own connection. Distinct instances exercise the
        // database lock while the original fixture connection can still change evidence.
        let recipient_store = store::open(&schema.url).unwrap();
        let owner_store = store::open(&schema.url).unwrap();
        let mut gate = Client::connect(&schema.url, NoTls).unwrap();
        gate.batch_execute("SET statement_timeout='8s'").unwrap();
        if !has_baseline {
            gate.execute(
                "DELETE FROM hub_work_dependency_notices WHERE work=$1",
                &[&dependent.id],
            )
            .unwrap();
        }
        // Stop the first refresh after its evidence/prior read, at the durable write. The
        // competing owner refresh must wait before reading that same prior/evidence pair.
        gate.batch_execute("CREATE FUNCTION pause_notice_write() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(8821615); RETURN NEW; END $$; CREATE TRIGGER pause_notice_write BEFORE INSERT OR UPDATE ON hub_work_dependency_notices FOR EACH ROW EXECUTE FUNCTION pause_notice_write()").unwrap();
        gate.query_one("SELECT pg_advisory_lock(8821615)", &[])
            .unwrap();
        coverage(&*store, CatchupCoverageState::Unavailable, now + 1);
        let first = {
            let store = recipient_store;
            let token = token.clone();
            let query = query.clone();
            std::thread::spawn(move || store.notice_refresh(&token, &query, now + 1))
        };
        let wait_for_blocked = |gate: &mut Client, count: i64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let blocked:i64=gate.query_one("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND wait_event_type='Lock'",&[]).unwrap().get(0);
                if blocked >= count {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected {count} blocked refreshes, saw {blocked}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_for_blocked(&mut gate, 1);
        coverage(&*store, CatchupCoverageState::ObservedOnly, now + 2);
        let second = {
            let store = owner_store;
            std::thread::spawn(move || {
                store.notice_owner_refresh(
                    &WorkLifecycleReader::Person {
                        owner: &owner,
                        repository: REPO,
                        repository_id: REPO_ID,
                    },
                    None,
                    100,
                    now + 2,
                )
            })
        };
        wait_for_blocked(&mut gate, 2);
        gate.query_one("SELECT pg_advisory_unlock(8821615)", &[])
            .unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        let snapshot: String = gate
            .query_one(
                "SELECT snapshot FROM hub_work_dependency_notices WHERE work=$1",
                &[&dependent.id],
            )
            .unwrap()
            .get(0);
        let record: DependencyNoticeRecord = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(
            record.semantic_version,
            if has_baseline { 2 } else { 1 },
            "two successive meanings remain distinct, including absent baseline"
        );
        // An unchanged follow-up cannot manufacture or repair a collapsed version.
        let repeated = store.notice_refresh(&token, &query, now + 3).unwrap();
        assert_eq!(
            repeated.notices[0].source.version,
            format!(
                "{}:{}:{}",
                record.generation, record.declaration_seq, record.semantic_version
            )
        );
    }
}

fn seed_receipt_history(
    database: &str,
    owner: &str,
    token: &str,
    session: &str,
    prefix: &str,
    settled: bool,
) {
    use agentscm::hub::notices::NoticeReceipt;
    let mut values = Vec::new();
    for n in 0..4097 {
        let mut receipt = NoticeReceipt::new(
            REPO,
            NoticeSource {
                kind: SourceKind::HumanMessage,
                id: format!("msg_{prefix}_{n}"),
                version: "1".into(),
            },
            NoticeRecipient {
                token: token.into(),
                session: session.into(),
                work_id: None,
                generation: None,
            },
            unix_now(),
        );
        if settled {
            receipt.read_at = Some(unix_now());
            receipt.resolved_at = receipt.read_at;
        }
        values.push((
            receipt.id.clone(),
            receipt.source.id.clone(),
            serde_json::to_string(&receipt).unwrap(),
        ));
    }
    if let Some(path) = database.strip_prefix("sqlite://") {
        let mut client = rusqlite::Connection::open(path).unwrap();
        let tx = client.transaction().unwrap();
        {
            let mut insert=tx.prepare("INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,session,source_kind,source_id,source_version,snapshot) VALUES (?1,?2,'github','github.com',?3,?4,?5,?6,'\"human_message\"',?7,'1',?8)").unwrap();
            for (id, source, snapshot) in values {
                insert
                    .execute(rusqlite::params![
                        id, owner, REPO, REPO_ID, token, session, source, snapshot
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    } else {
        let mut client = postgres::Client::connect(database, postgres::NoTls).unwrap();
        let mut tx = client.transaction().unwrap();
        let insert=tx.prepare("INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,session,source_kind,source_id,source_version,snapshot) VALUES ($1,$2,'github','github.com',$3,$4,$5,$6,'\"human_message\"',$7,'1',$8)").unwrap();
        for (id, source, snapshot) in values {
            tx.execute(
                &insert,
                &[
                    &id, &owner, &REPO, &REPO_ID, &token, &session, &source, &snapshot,
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
}

fn ledger_history_and_terminal_contract(database: &str) {
    use agentscm::hub::session_lifecycle::{EndReason, LifecycleEnd, LifecycleState};
    use agentscm::hub::work::WorkState;
    use agentscm::hub::work_transition::WorkLifecycleReader;
    let Fixture {
        store,
        owner,
        token,
        dependent,
        incarnation,
        now,
        ..
    } = fixture(database);
    let reader = WorkLifecycleReader::Person {
        owner: &owner,
        repository: REPO,
        repository_id: REPO_ID,
    };
    seed_receipt_history(database, &owner.id, &token, "dependent", "settled", true);
    assert!(
        store.notice_owner_pending(&reader, now).unwrap().is_empty(),
        "settled lifetime history does not consume the pending read bound"
    );
    let source = NoticeSource {
        kind: SourceKind::Operational,
        id: "synthetic-current".into(),
        version: "1".into(),
    };
    let recipient = NoticeRecipient {
        token: token.clone(),
        session: "dependent".into(),
        work_id: None,
        generation: None,
    };
    let receipt = store
        .notice_record(&token, &source, &recipient, now)
        .unwrap();
    let offered = store
        .notice_offer(
            &token,
            &NoticeOfferRequest {
                session: "dependent".into(),
                incarnation: incarnation.clone(),
                automatic: true,
            },
            now,
        )
        .unwrap()
        .unwrap();
    assert_eq!(offered.delivery_id, receipt.id);
    let report = NoticeTransportReport {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        delivery_id: receipt.id.clone(),
        attempt_id: offered.attempt_id.unwrap(),
        outcome: AttemptOutcome::Unknown,
        observed_at: now,
    };
    store.notice_transport(&token, &report, now).unwrap();
    store
        .notice_resolve(&token, "dependent", &source, now)
        .unwrap();
    let reopened = store::open(database).unwrap();
    let retained = reopened
        .notice_record(&token, &source, &recipient, now + 1)
        .unwrap();
    assert_eq!(retained.id, receipt.id);
    assert_eq!(retained.attempts.len(), 1);
    assert_eq!(retained.attempts[0].outcome, AttemptOutcome::Unknown);
    assert!(
        !retained.automatic_eligible(),
        "history growth cannot reset a source-version automatic budget"
    );
    let backlog_incarnation = register(&*store, &token, "backlog");
    seed_receipt_history(database, &owner.id, &token, "backlog", "unresolved", false);
    assert!(
        store.notice_pending(&token, "backlog", now).is_err(),
        "pending overflow is explicit, never a truncated empty result"
    );
    assert!(store.notice_owner_pending(&reader, now).is_err());
    let end = LifecycleEnd {
        session: "dependent".into(),
        incarnation: incarnation.clone(),
        operation_id: "full-history-end".into(),
        observed_at: now + 2,
        reason: EndReason::ExplicitEnd,
        work: store
            .lifecycle_fences(&token, "dependent", now + 2)
            .unwrap(),
    };
    let ended = store.lifecycle_end(&token, &end, now + 2).unwrap();
    assert_eq!(ended.lifecycle.state, LifecycleState::Terminal);
    assert_eq!(
        store
            .work_get(&token, &dependent.id, now + 2)
            .unwrap()
            .unwrap()
            .state,
        WorkState::Paused
    );
    assert_eq!(
        store
            .notice_owner_terminal(&reader, std::slice::from_ref(&dependent.id), now + 2)
            .unwrap(),
        vec![dependent.id.clone()]
    );
    let query = NoticeOfferRequest {
        session: "backlog".into(),
        incarnation: backlog_incarnation,
        automatic: true,
    };
    assert!(
        store.notice_offer(&token, &query, now + 3).is_err(),
        "first stale window reports incomplete coverage"
    );
    assert!(
        store
            .notice_offer(&token, &query, now + 3)
            .unwrap()
            .is_none(),
        "committed stale withdrawals let the next call finish the backlog"
    );
    assert!(
        store
            .notice_pending(&token, "backlog", now + 3)
            .unwrap()
            .is_empty()
    );
    let pending = store.notice_owner_pending(&reader, now + 3).unwrap();
    assert!(
        pending
            .iter()
            .any(|receipt| receipt.source.kind == SourceKind::Operational
                && receipt.source.id == dependent.id
                && receipt.terminal_at.is_some()),
        "terminal recovery receipt remains durable despite lifetime history and pending overflow"
    );
    assert_eq!(store.lifecycle_end(&token, &end, now + 3).unwrap(), ended);
}

#[test]
fn sqlite_notice_history_does_not_block_delivery_or_terminal() {
    let directory = tempfile::tempdir().unwrap();
    ledger_history_and_terminal_contract(&format!(
        "sqlite://{}",
        directory.path().join("hub.sqlite").display()
    ));
}

#[test]
fn postgres_notice_history_does_not_block_delivery_or_terminal() {
    let Some(schema) = crate::hub_pg::PgSchema::create() else {
        return;
    };
    ledger_history_and_terminal_contract(&schema.url);
}
