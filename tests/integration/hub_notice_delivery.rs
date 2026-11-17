//! Source changes, enqueue attempts and model acknowledgements are separate durable facts.
use agentscm::hub::agent_messages::{Message, MessageState, ReceiptCoverage};
use agentscm::hub::catchup::{
    CatchupCoverageState, CatchupMergeSource, CatchupRepositoryScope, CatchupSourceStatus,
    NewCatchupMerge,
};
use agentscm::hub::connection::NewConnection;
use agentscm::hub::notices::{
    AttemptOutcome, DependencyNoticeAck, DependencyRefresh, NewOwnerNotice, NoticeAttempt,
    NoticeOfferRequest, NoticeReceipt, NoticeRecipient, NoticeSource, NoticeTransportReport,
    OwnerNoticeDeliveryProof, OwnerNoticeDeliveryReport, OwnerNoticeRecipient,
    OwnerNoticeRecipientStatus, OwnerNoticeSourceKind, SourceKind,
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
use agentscm::hub::work_transition::WorkLifecycleReader;
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

fn projection_sql(database: &str, sql: &str) {
    if let Some(path) = database.strip_prefix("sqlite://") {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    } else {
        postgres::Client::connect(database, postgres::NoTls)
            .unwrap()
            .batch_execute(sql)
            .unwrap();
    }
}

fn projection_rows(database: &str) -> Vec<(String, String)> {
    let sql = "SELECT id,snapshot FROM hub_notice_deliveries ORDER BY id";
    if let Some(path) = database.strip_prefix("sqlite://") {
        let client = rusqlite::Connection::open(path).unwrap();
        let mut statement = client.prepare(sql).unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    } else {
        postgres::Client::connect(database, postgres::NoTls)
            .unwrap()
            .query(sql, &[])
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }
}

fn projection_insert(
    database: &str,
    owner: &str,
    repository: &str,
    repository_id: i64,
    receipt: &NoticeReceipt,
) {
    let kind = serde_json::to_string(&receipt.source.kind).unwrap();
    let snapshot = serde_json::to_string(receipt).unwrap();
    if let Some(path) = database.strip_prefix("sqlite://") {
        rusqlite::Connection::open(path).unwrap().execute(
            "INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,session,source_kind,source_id,source_version,snapshot) VALUES (?1,?2,'github','github.com',?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![receipt.id,owner,repository,repository_id,receipt.recipient.token,
                receipt.recipient.session,kind,receipt.source.id,receipt.source.version,snapshot],
        ).unwrap();
    } else {
        postgres::Client::connect(database, postgres::NoTls).unwrap().execute(
            "INSERT INTO hub_notice_deliveries(id,owner,provider,host,repository,repository_id,token,session,source_kind,source_id,source_version,snapshot) VALUES ($1,$2,'github','github.com',$3,$4,$5,$6,$7,$8,$9,$10)",
            &[&receipt.id,&owner,&repository,&repository_id,&receipt.recipient.token,
                &receipt.recipient.session,&kind,&receipt.source.id,&receipt.source.version,&snapshot],
        ).unwrap();
    }
}

fn projection_message(token: &str, id: &str, now: i64) -> Message {
    Message {
        id: id.into(),
        token: token.into(),
        session: "receipt-session".into(),
        work_id: None,
        note: "Synthetic receipt history".into(),
        from: "reader".into(),
        state: MessageState::Delivered,
        via: Some("notice".into()),
        mode: None,
        created_at: now,
        delivered_at: Some(now),
        offered_at: Some(now),
        read_at: None,
        done_at: None,
        answered_at: None,
        deadline_at: Some(now + 600),
    }
}

fn projection_receipt(message: &Message, repository: &str, now: i64) -> NoticeReceipt {
    NoticeReceipt::new(
        repository,
        NoticeSource {
            kind: SourceKind::HumanMessage,
            id: message.id.clone(),
            version: "1".into(),
        },
        NoticeRecipient {
            token: message.token.clone(),
            session: message.session.clone(),
            work_id: None,
            generation: None,
        },
        now,
    )
}

fn message_receipt_projection_contract(database: &str) {
    let Fixture {
        store,
        owner,
        token,
        now,
        ..
    } = fixture(database);
    let missing = projection_message(&token, "msg_missing", now);
    let admitted = projection_message(&token, "msg_admitted", now);
    let mut historical = projection_message(&token, "msg_historical", now);
    historical.state = MessageState::Withdrawn;
    let ambiguous = projection_message(&token, "msg_ambiguous", now);
    let admitted_receipt = projection_receipt(&admitted, REPO, now);
    projection_insert(database, &owner.id, REPO, REPO_ID, &admitted_receipt);
    let old_repository = "former/widgets";
    let mut history = projection_receipt(&historical, old_repository, now);
    history.offered_at = Some(0);
    history.read_at = Some(now);
    history.resolved_at = Some(0);
    history.withdrawn = true;
    let outcomes = [
        AttemptOutcome::NotAttempted,
        AttemptOutcome::ProvenNoDispatch,
        AttemptOutcome::PossiblyEnqueued,
        AttemptOutcome::Unknown,
        AttemptOutcome::TransportReported,
    ];
    history.attempts = outcomes
        .into_iter()
        .cycle()
        .take(8)
        .enumerate()
        .map(|(i, outcome)| NoticeAttempt {
            id: format!("private-attempt-{i}"),
            incarnation: "private-incarnation".into(),
            generation: Some(5),
            outcome,
            created_at: 8 - i as i64,
            reported_at: Some(0),
        })
        .collect();
    projection_insert(database, &owner.id, old_repository, REPO_ID, &history);
    for repository in [REPO, old_repository] {
        projection_insert(
            database,
            &owner.id,
            repository,
            REPO_ID,
            &projection_receipt(&ambiguous, repository, now),
        );
    }
    let mode_key = format!("agent_message_mode:{}:{REPO_ID}", owner.id);
    store
        .set_setting(&mode_key, "\"ignore\"", &owner.id)
        .unwrap();
    let messages = vec![missing, historical, admitted, ambiguous];
    let before = projection_rows(database);
    let receipts = store
        .notice_message_receipts(&owner, REPO, REPO_ID, &messages, now)
        .unwrap();
    assert_eq!(receipts.len(), messages.len());
    assert_eq!(
        receipts
            .iter()
            .map(|r| r.coverage.clone())
            .collect::<Vec<_>>(),
        vec![
            ReceiptCoverage::LegacyUnknown,
            ReceiptCoverage::Ledger,
            ReceiptCoverage::Ledger,
            ReceiptCoverage::Unavailable,
        ],
        "each result retains its input position, including unavailable history"
    );
    assert_eq!(
        serde_json::to_value(&receipts[0]).unwrap(),
        json!({"coverage":"legacy_unknown","delivery_id":null,"pointer_offer_recorded_at":null,
            "transport_attempts":[],"presentation":"unknown","resolved_at":null})
    );
    assert_eq!(
        receipts[1].delivery_id.as_deref(),
        Some(history.id.as_str())
    );
    assert_eq!(receipts[1].pointer_offer_recorded_at, Some(0));
    assert_eq!(receipts[1].resolved_at, Some(0));
    assert_eq!(receipts[1].presentation, "unknown");
    assert_eq!(receipts[1].transport_attempts.len(), 8);
    for (projected, attempt) in receipts[1].transport_attempts.iter().zip(&history.attempts) {
        assert_eq!(projected.outcome, attempt.outcome);
        assert_eq!(projected.created_at, attempt.created_at);
        assert_eq!(projected.reported_at, attempt.reported_at);
    }
    let exposed = serde_json::to_string(&receipts[1]).unwrap();
    assert!(!exposed.contains("private-"));
    assert!(!exposed.contains("read_at"));
    assert!(!exposed.contains("withdrawn"));
    assert_eq!(receipts[2].delivery_id, Some(admitted_receipt.id));
    assert_eq!(
        receipts[2].pointer_offer_recorded_at, None,
        "legacy Delivered and offered_at do not manufacture a recorded pointer offer"
    );
    assert_eq!(receipts[3].delivery_id, None);
    assert_eq!(
        projection_rows(database),
        before,
        "history reads never mutate the ledger"
    );
    assert_eq!(
        store.get_setting(&mode_key).unwrap().as_deref(),
        Some("\"ignore\"")
    );
    assert!(
        store
            .notice_message_receipts(&owner, REPO, REPO_ID, &[], now)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .notice_message_receipts(&owner, REPO, REPO_ID, &vec![messages[2].clone(); 50], now)
            .unwrap()
            .len(),
        50
    );
    assert!(matches!(
        store.notice_message_receipts(&owner, REPO, REPO_ID, &vec![messages[2].clone(); 51], now),
        Err(agentscm::hub::HubError::Invalid(_))
    ));
    assert!(matches!(
        store.notice_message_receipts(&owner, REPO, 0, &messages, now),
        Err(agentscm::hub::HubError::Invalid(_))
    ));

    let exact = projection_message(&token, "msg_exact", now);
    projection_insert(
        database,
        &owner.id,
        REPO,
        REPO_ID,
        &projection_receipt(&exact, REPO, now),
    );
    let mut other_session = exact.clone();
    other_session.session = "another-session".into();
    let other_source = projection_message(&token, "msg_other", now);
    let wrong_kind = projection_message(&token, "msg_wrong_kind", now);
    let mut receipt = projection_receipt(&wrong_kind, REPO, now);
    receipt.source.kind = SourceKind::Coordination;
    projection_insert(database, &owner.id, REPO, REPO_ID, &receipt);
    let wrong_version = projection_message(&token, "msg_wrong_version", now);
    let mut receipt = projection_receipt(&wrong_version, REPO, now);
    receipt.source.version = "2".into();
    projection_insert(database, &owner.id, REPO, REPO_ID, &receipt);
    let wrong_repository = projection_message(&token, "msg_wrong_repo", now);
    projection_insert(
        database,
        &owner.id,
        REPO,
        REPO_ID + 1,
        &projection_receipt(&wrong_repository, REPO, now),
    );
    let stranger = store
        .upsert_user(
            &ForgeIdentity {
                user_id: 999,
                ..owner.identity.clone()
            },
            "stranger",
        )
        .unwrap();
    let wrong_owner = projection_message(&token, "msg_wrong_owner", now);
    projection_insert(
        database,
        &stranger.id,
        REPO,
        REPO_ID,
        &projection_receipt(&wrong_owner, REPO, now),
    );
    let wrong_host = projection_message(&token, "msg_wrong_host", now);
    projection_insert(
        database,
        &owner.id,
        REPO,
        REPO_ID,
        &projection_receipt(&wrong_host, REPO, now),
    );
    let wrong_provider = projection_message(&token, "msg_wrong_provider", now);
    projection_insert(
        database,
        &owner.id,
        REPO,
        REPO_ID,
        &projection_receipt(&wrong_provider, REPO, now),
    );
    projection_sql(
        database,
        "UPDATE hub_notice_deliveries SET host='other.example' WHERE source_id='msg_wrong_host'; UPDATE hub_notice_deliveries SET provider='gitlab' WHERE source_id='msg_wrong_provider'",
    );
    let (other_token, _, _) = mint_bound_agent_token(
        &*store,
        &owner.id,
        "other-message-agent",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let mut other_recipient = exact.clone();
    other_recipient.token = other_token;
    let mismatches = [
        other_session,
        other_source,
        wrong_kind,
        wrong_version,
        wrong_repository,
        wrong_owner,
        wrong_host,
        wrong_provider,
        other_recipient,
    ];
    let receipts = store
        .notice_message_receipts(&owner, REPO, REPO_ID, &mismatches, now)
        .unwrap();
    assert_eq!(receipts.len(), mismatches.len());
    assert!(
        receipts
            .iter()
            .all(|r| r.coverage == ReceiptCoverage::LegacyUnknown)
    );

    let mint = |user: &User, repository: &str, repository_id, read_only, ttl| {
        mint_bound_agent_token(
            &*store,
            &user.id,
            "projection-agent",
            repository,
            repository_id,
            ttl,
            read_only,
        )
        .unwrap()
    };
    let (read_only, _, _) = mint(&owner, REPO, REPO_ID, true, 3600);
    let (revoked, _, _) = mint(&owner, REPO, REPO_ID, false, 3600);
    store.revoke_agent_token(&revoked).unwrap();
    let (expired, _, expires) = mint(&owner, REPO, REPO_ID, false, 1);
    let (foreign, _, _) = mint(&stranger, REPO, REPO_ID, false, 3600);
    let (other_repo, _, _) = mint(&owner, "acme/other", REPO_ID + 1, false, 3600);
    let invalid_tokens = [
        read_only,
        revoked,
        expired,
        foreign,
        other_repo,
        "missing-token".into(),
    ];
    let mut batch = vec![exact.clone()];
    batch.extend(
        invalid_tokens
            .iter()
            .map(|token| projection_message(token, &exact.id, now)),
    );
    let receipts = store
        .notice_message_receipts(&owner, REPO, REPO_ID, &batch, expires)
        .unwrap();
    assert_eq!(receipts.len(), batch.len());
    assert_eq!(receipts[0].coverage, ReceiptCoverage::Ledger);
    assert!(
        receipts[1..]
            .iter()
            .all(|r| r.coverage == ReceiptCoverage::Unavailable && r.delivery_id.is_none())
    );
    let mut forged_owner = owner.clone();
    forged_owner.identity.host = "other.example".into();
    assert!(matches!(
        store.notice_message_receipts(&forged_owner, REPO, REPO_ID, &[], now),
        Err(agentscm::hub::HubError::Unauthorized)
    ));
    store.set_user_disabled(&owner.id, true).unwrap();
    assert!(matches!(
        store.notice_message_receipts(&owner, REPO, REPO_ID, &batch, now),
        Err(agentscm::hub::HubError::Unauthorized)
    ));
    store.set_user_disabled(&owner.id, false).unwrap();

    projection_sql(
        database,
        "UPDATE hub_notice_deliveries SET snapshot='malformed' WHERE source_id='msg_exact'",
    );
    let failure_batch = [messages[2].clone(), exact, messages[0].clone()];
    let receipts = store
        .notice_message_receipts(&owner, REPO, REPO_ID, &failure_batch, now)
        .unwrap();
    assert_eq!(receipts.len(), failure_batch.len());
    assert!(
        receipts
            .iter()
            .all(|r| r.coverage == ReceiptCoverage::Unavailable)
    );
    projection_sql(
        database,
        "ALTER TABLE hub_notice_deliveries RENAME TO retained_notice_deliveries",
    );
    let receipts = store
        .notice_message_receipts(&owner, REPO, REPO_ID, &failure_batch, now)
        .unwrap();
    assert!(
        receipts
            .iter()
            .all(|r| r.coverage == ReceiptCoverage::Unavailable)
    );
    forged_owner = owner.clone();
    forged_owner.disabled = true;
    assert!(matches!(
        store.notice_message_receipts(&forged_owner, REPO, REPO_ID, &failure_batch, now),
        Err(agentscm::hub::HubError::Unauthorized)
    ));
    projection_sql(
        database,
        "ALTER TABLE retained_notice_deliveries RENAME TO hub_notice_deliveries; ALTER TABLE hub_users RENAME TO retained_users",
    );
    assert!(
        store
            .notice_message_receipts(&owner, REPO, REPO_ID, &failure_batch, now)
            .is_err(),
        "owner storage failure cannot become implicit authorization"
    );
    projection_sql(database, "ALTER TABLE retained_users RENAME TO hub_users");
}

#[test]
fn sqlite_historical_message_receipt_projection() {
    let directory = tempfile::tempdir().unwrap();
    message_receipt_projection_contract(&format!(
        "sqlite://{}",
        directory.path().join("hub.sqlite").display()
    ));
}

#[test]
fn postgres_historical_message_receipt_projection() {
    let Some(schema) = crate::hub_pg::PgSchema::create() else {
        return;
    };
    message_receipt_projection_contract(&schema.url);
}

fn owner_mailbox_contract(database: &str) {
    let store = store::open(database).unwrap();
    let sender = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 501,
            },
            "sender",
        )
        .unwrap();
    let recipient = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 502,
            },
            "recipient",
        )
        .unwrap();
    let bystander = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 503,
            },
            "bystander",
        )
        .unwrap();
    let (sender_token, _, _) = mint_bound_agent_token(
        &*store,
        &sender.id,
        "sender-agent",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let (recipient_token, _, _) = mint_bound_agent_token(
        &*store,
        &recipient.id,
        "recipient-agent",
        REPO,
        REPO_ID,
        3600,
        false,
    )
    .unwrap();
    let now = unix_now();
    let recipient_address = OwnerNoticeRecipient {
        owner: recipient.id.clone(),
        provider: "github".into(),
        host: "github.com".into(),
    };
    let request = NewOwnerNotice {
        idempotency_key: "send-pr-42-v1".into(),
        source_kind: OwnerNoticeSourceKind::Addressed,
        source_id: "pr-42-note-1".into(),
        recipient: recipient_address.clone(),
        body: "PR #42 needs a fresh attestation.".into(),
    };
    let created = store
        .owner_notice_put(&sender_token, &request, now)
        .unwrap();
    assert_eq!(
        store
            .owner_notice_put(&sender_token, &request, now + 1)
            .unwrap(),
        created,
        "exact retry returns the immutable original"
    );
    let mut changed = request.clone();
    changed.body = "Changed body must not reuse the key.".into();
    assert!(
        store
            .owner_notice_put(&sender_token, &changed, now + 1)
            .is_err()
    );
    let mut same_source = request.clone();
    same_source.idempotency_key = "another-key".into();
    assert_eq!(
        store
            .owner_notice_put(&sender_token, &same_source, now + 1)
            .unwrap(),
        created,
        "the recipient/source pair deduplicates an equivalent send"
    );
    changed.idempotency_key = "another-key".into();
    assert!(
        store
            .owner_notice_put(&sender_token, &changed, now + 1)
            .is_err(),
        "the recipient/source pair is deduplicated independently of retries"
    );

    let reader = WorkLifecycleReader::Person {
        owner: &recipient,
        repository: REPO,
        repository_id: REPO_ID,
    };
    let page = store.owner_notice_list(&reader, None, 100, now).unwrap();
    assert_eq!(page.notices.len(), 1);
    assert_eq!(page.notices[0], created);
    assert!(page.next_after.is_none());
    assert!(!page.notices[0].delivery_proven());
    let bystander_reader = WorkLifecycleReader::Person {
        owner: &bystander,
        repository: REPO,
        repository_id: REPO_ID,
    };
    assert!(
        store
            .owner_notice_recipient_status(
                &bystander_reader,
                &created.id,
                OwnerNoticeRecipientStatus::Acknowledged,
                now + 2,
            )
            .is_err()
    );
    let proof = OwnerNoticeDeliveryProof {
        channel: "slack_dm".into(),
        external_id: "synthetic-message-1".into(),
    };
    assert!(
        store
            .owner_notice_delivered(&recipient_token, &created.id, &proof, now + 2)
            .is_err(),
        "the recipient cannot forge transport delivery"
    );
    let delivered = store
        .owner_notice_delivered(&sender_token, &created.id, &proof, now + 2)
        .unwrap();
    assert!(delivered.delivery_proven());
    assert!(delivered.acknowledged_at.is_none());
    let mut conflicting = proof.clone();
    conflicting.external_id = "another-message".into();
    assert!(
        store
            .owner_notice_delivered(&sender_token, &created.id, &conflicting, now + 3)
            .is_err()
    );
    let acknowledged = store
        .owner_notice_recipient_status(
            &reader,
            &created.id,
            OwnerNoticeRecipientStatus::Acknowledged,
            now + 3,
        )
        .unwrap();
    assert_eq!(acknowledged.acknowledged_at, Some(now + 3));
    assert!(acknowledged.acted_at.is_none());
    let acted = store
        .owner_notice_recipient_status(
            &reader,
            &created.id,
            OwnerNoticeRecipientStatus::Acted,
            now + 4,
        )
        .unwrap();
    assert_eq!(acted.acknowledged_at, Some(now + 3));
    assert_eq!(acted.acted_at, Some(now + 4));

    let automatic = NewOwnerNotice {
        idempotency_key: "validation-pr-42-policy-v2".into(),
        source_kind: OwnerNoticeSourceKind::AttestationInvalidation,
        source_id: "pr:42:local-validation:policy:evidence:policy-v2".into(),
        recipient: recipient_address.clone(),
        body: "PR #42 has an attestation invalidated by a policy change.".into(),
    };
    let automatic_created = store
        .owner_notice_invalidation_put(REPO, REPO_ID, &automatic, now + 4)
        .unwrap();
    assert_eq!(automatic_created.sender_owner, "hub");
    assert_eq!(automatic_created.sender_token, "hub:validation");
    assert_eq!(
        store
            .owner_notice_invalidation_put(REPO, REPO_ID, &automatic, now + 5)
            .unwrap(),
        automatic_created,
        "automatic producer retries deduplicate on the stable transition source"
    );
    let mut changed_automatic = automatic.clone();
    changed_automatic.body.push_str(" changed");
    assert!(
        store
            .owner_notice_invalidation_put(REPO, REPO_ID, &changed_automatic, now + 5)
            .is_err()
    );
    let claim = store
        .owner_notice_delivery_claim(&reader, &automatic_created.id, "slack", now + 5)
        .unwrap()
        .unwrap();
    assert!(
        store
            .owner_notice_delivery_claim(&reader, &automatic_created.id, "slack", now + 6)
            .unwrap()
            .is_none(),
        "an atomic possibly-enqueued claim cannot be duplicated"
    );
    let uncertain = OwnerNoticeDeliveryReport {
        attempt_id: claim.attempt_id,
        outcome: AttemptOutcome::Unknown,
        proof: None,
    };
    store
        .owner_notice_delivery_report(&reader, &automatic_created.id, &uncertain, now + 6)
        .unwrap();
    assert!(
        store
            .owner_notice_delivery_claim(&reader, &automatic_created.id, "slack", now + 60)
            .unwrap()
            .is_none(),
        "uncertain transport is never blindly retried"
    );

    let retryable_request = NewOwnerNotice {
        idempotency_key: "validation-pr-43-policy-v2".into(),
        source_kind: OwnerNoticeSourceKind::AttestationInvalidation,
        source_id: "pr:43:local-validation:policy:evidence:policy-v2".into(),
        recipient: recipient_address.clone(),
        body: "PR #43 has an attestation invalidated by a policy change.".into(),
    };
    let retryable = store
        .owner_notice_invalidation_put(REPO, REPO_ID, &retryable_request, now + 6)
        .unwrap();
    let first = store
        .owner_notice_delivery_claim(&reader, &retryable.id, "slack", now + 6)
        .unwrap()
        .unwrap();
    store
        .owner_notice_delivery_report(
            &reader,
            &retryable.id,
            &OwnerNoticeDeliveryReport {
                attempt_id: first.attempt_id,
                outcome: AttemptOutcome::ProvenNoDispatch,
                proof: None,
            },
            now + 7,
        )
        .unwrap();
    let second = store
        .owner_notice_delivery_claim(&reader, &retryable.id, "slack", now + 8)
        .unwrap()
        .unwrap();
    let proof = OwnerNoticeDeliveryProof {
        channel: "slack".into(),
        external_id: "C1:123.456".into(),
    };
    let proven = store
        .owner_notice_delivery_report(
            &reader,
            &retryable.id,
            &OwnerNoticeDeliveryReport {
                attempt_id: second.attempt_id,
                outcome: AttemptOutcome::TransportReported,
                proof: Some(proof.clone()),
            },
            now + 9,
        )
        .unwrap();
    assert!(proven.delivery_proven());
    assert_eq!(proven.delivery.as_ref(), Some(&proof));
    assert!(proven.acknowledged_at.is_none());

    for index in 1..20 {
        store
            .owner_notice_put(
                &sender_token,
                &NewOwnerNotice {
                    idempotency_key: format!("automatic-{index}"),
                    source_kind: OwnerNoticeSourceKind::AttestationInvalidation,
                    source_id: format!("pr-{}-digest-v2", 100 + index),
                    recipient: recipient_address.clone(),
                    body: format!("PR #{} has an obsolete attestation.", 100 + index),
                },
                now + 5,
            )
            .unwrap();
    }
    let limited = store.owner_notice_list(&reader, None, 1, now + 5).unwrap();
    assert_eq!(limited.notices.len(), 1);
    assert!(limited.next_after.is_some());
    let after = limited.next_after.as_deref().unwrap();
    assert!(
        store
            .owner_notice_list(&reader, Some(after), 100, now + 5)
            .unwrap()
            .notices
            .iter()
            .all(|notice| notice.id.as_str() > after)
    );
    assert!(
        store
            .owner_notice_put(
                &sender_token,
                &NewOwnerNotice {
                    idempotency_key: "over-rate-bound".into(),
                    source_kind: OwnerNoticeSourceKind::Addressed,
                    source_id: "pr-999-note".into(),
                    recipient: recipient_address,
                    body: "This send exceeds the bounded recipient window.".into(),
                },
                now + 5,
            )
            .is_err()
    );
}

#[test]
fn sqlite_owner_mailbox_contract() {
    let directory = tempfile::tempdir().unwrap();
    owner_mailbox_contract(&format!(
        "sqlite://{}",
        directory.path().join("hub.sqlite").display()
    ));
}

#[test]
fn postgres_owner_mailbox_contract() {
    let Some(schema) = crate::hub_pg::PgSchema::create() else {
        return;
    };
    owner_mailbox_contract(&schema.url);
}
