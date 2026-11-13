//! One suite, run against SQLite always and PostgreSQL when AGENTSCM_TEST_DATABASE_URL is set.
use crate::hub_pg;

use std::sync::Arc;
use std::thread;

use agentscm::hub::connection::{
    ForgeCheck, ForgeCheckReserve, MergeGroup, NewConnection, NewForgeCheck, NewMergeGroup,
    Observed,
};
use agentscm::hub::external_runs::{
    ExternalKind, ExternalObserved, ExternalOutcome, ExternalRun, ExternalSource,
};
use agentscm::hub::intent::NewIntent;
use agentscm::hub::skills::{EvidenceRow, SkillAdoption};
use agentscm::hub::store::{
    self, ForgeIdentity, LocalCiDevice, LocalCiDeviceWrite, LocalCiRefusal, LocalCiRequest,
    MAX_AGENT_TOKEN_TTL_SECONDS, NewAgentToken, Store, agent_token_digest, mint_agent_token,
    mint_bound_agent_token, new_connection_id, unix_now,
};
use agentscm::local_ci::{DeviceAuthorization, DeviceKey, OwnerIdentity};
use agentscm::policy::issuers::issuer_key_id;
use hub_pg::PgSchema;
use serde_json::{Value, json};
use tempfile::TempDir;

const TTL: i64 = 3600;
const REPO: &str = "acme/widgets";

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

    fn exec(&self, sql: &str) -> Result<(), String> {
        if self.pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
            // Display of a postgres error omits the server's message.
            client.batch_execute(sql).map_err(|e| {
                e.as_db_error()
                    .map_or_else(|| e.to_string(), |db| db.message().to_owned())
            })
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch(sql).map_err(|e| e.to_string())
        }
    }

    fn versions(&self) -> Vec<i64> {
        let sql = "SELECT version FROM hub_schema_migrations ORDER BY version";
        if self.pg.is_some() {
            let mut client = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
            let rows = client.query(sql, &[]).unwrap();
            rows.iter().map(|row| row.get(0)).collect()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
    }

    fn named(&self) -> Vec<String> {
        let sql = "SELECT name FROM hub_named_migrations ORDER BY name";
        if self.pg.is_some() {
            let rows = postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query(sql, &[])
                .unwrap();
            rows.iter().map(|row| row.get(0)).collect()
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
    }

    fn count(&self, sql: &str) -> i64 {
        if self.pg.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(sql, &[])
                .unwrap()
                .get(0)
        } else {
            let path = self.url.strip_prefix("sqlite://").unwrap();
            rusqlite::Connection::open(path)
                .unwrap()
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        }
    }
}

fn identity(user_id: i64) -> ForgeIdentity {
    ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id,
    }
}

/// Returns (token id, digest).
fn mint(store: &dyn Store, owner: &str, name: &str, ttl: i64) -> (String, store::TokenDigest) {
    let (id, secret) = mint_agent_token(store, owner, name, REPO, ttl).unwrap();
    (id, agent_token_digest(secret.expose()).unwrap())
}

fn kinds(store: &dyn Store) -> Vec<String> {
    let events = store.events_after(0, 1000).unwrap();
    events.into_iter().map(|event| event.kind).collect()
}

fn assert_id(id: &str, prefix: &str) {
    let hex = id.strip_prefix(prefix).unwrap();
    assert_eq!(hex.len(), 32, "{id}");
    assert!(
        hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );
}

fn token_lookup(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    assert_id(&alice.id, "usr_");
    let (id, digest) = mint(&*store, &alice.id, "builder", TTL);
    assert_id(&id, "tok_");
    let now = unix_now();
    let principal = store.authenticate(&digest, now).unwrap().unwrap();
    assert_eq!(principal.token_id, id);
    assert_eq!(principal.name, "builder");
    assert_eq!(principal.repository, REPO);
    assert!((now + TTL - 5..=now + TTL + 5).contains(&principal.expires_at));
    assert_eq!(principal.owner, alice);
    let unknown = agent_token_digest(&"0".repeat(64)).unwrap();
    assert_eq!(store.authenticate(&unknown, now).unwrap(), None);
    store.ping().unwrap();
}

fn expiry(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (_, digest) = mint(&*store, &alice.id, "builder", 100);
    let expires_at = store
        .authenticate(&digest, unix_now())
        .unwrap()
        .unwrap()
        .expires_at;
    assert_eq!(store.authenticate(&digest, expires_at).unwrap(), None);
    assert!(
        store
            .authenticate(&digest, expires_at - 1)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.authenticate(&digest, expires_at + 1).unwrap(), None);
}

fn revocation(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (first, first_digest) = mint(&*store, &alice.id, "one", TTL);
    let (_, second_digest) = mint(&*store, &alice.id, "two", TTL);
    let now = unix_now();
    assert!(store.revoke_agent_token(&first).unwrap());
    assert!(!store.revoke_agent_token(&first).unwrap());
    assert!(!store.revoke_agent_token("tok_unknown").unwrap());
    assert_eq!(store.authenticate(&first_digest, now).unwrap(), None);
    assert!(store.authenticate(&second_digest, now).unwrap().is_some());
}

fn owner_disable_cascades(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let (_, a1) = mint(&*store, &alice.id, "one", TTL);
    let (_, a2) = mint(&*store, &alice.id, "two", TTL);
    let (_, b1) = mint(&*store, &bob.id, "one", TTL);
    let now = unix_now();
    store.set_user_disabled(&alice.id, true).unwrap();
    assert_eq!(store.authenticate(&a1, now).unwrap(), None);
    assert_eq!(store.authenticate(&a2, now).unwrap(), None);
    assert!(store.authenticate(&b1, now).unwrap().is_some());
    store.set_user_disabled(&alice.id, false).unwrap();
    assert!(store.authenticate(&a1, now).unwrap().is_some());
    assert!(store.authenticate(&a2, now).unwrap().is_some());
    assert!(store.set_user_disabled("usr_unknown", true).is_err());
}

fn rename_keeps_identity(db: &Db) {
    let store = db.open();
    let before = store.upsert_user(&identity(7), "alice").unwrap();
    let (_, digest) = mint(&*store, &before.id, "builder", TTL);
    let after = store.upsert_user(&identity(7), "alice-renamed").unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.login, "alice-renamed");
    let owner = store
        .authenticate(&digest, unix_now())
        .unwrap()
        .unwrap()
        .owner;
    assert_eq!(owner.id, before.id);
    assert_eq!(owner.login, "alice-renamed");
    assert_ne!(
        store.upsert_user(&identity(8), "alice").unwrap().id,
        before.id
    );
}

fn display_name_is_stored_and_cleared(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    assert_eq!(alice.name, None);
    let events = kinds(&*store).len();
    store
        .set_user_name(&alice.id, Some("Alice Liddell"))
        .unwrap();
    let named = Some("Alice Liddell".to_owned());
    assert_eq!(store.user_by_id(&alice.id).unwrap().unwrap().name, named);
    assert_eq!(
        store.upsert_user(&identity(7), "alice").unwrap().name,
        named
    );
    assert_eq!(store.list_users().unwrap()[0].name, named);
    assert!(store.user_created_at(&alice.id).unwrap().unwrap() > 0);
    assert_eq!(store.user_created_at("usr_missing").unwrap(), None);
    let digest = store::browser_session_digest(&format!("{:064}", 1)).unwrap();
    let now = unix_now();
    store
        .insert_browser_session(&alice.id, &digest, now + 60, now)
        .unwrap();
    let restored = store.browser_session(&digest, now, now).unwrap().unwrap().0;
    assert_eq!(restored.name, named);
    for bad in ["", " padded", "a\nb", &"x".repeat(101)] {
        assert!(
            store.set_user_name(&alice.id, Some(bad)).is_err(),
            "{bad:?}"
        );
    }
    assert!(store.set_user_name("usr_unknown", None).is_err());
    store.set_user_name(&alice.id, None).unwrap();
    assert_eq!(store.user_by_id(&alice.id).unwrap().unwrap().name, None);
    // The name is never an event.
    assert_eq!(kinds(&*store).len(), events);
}

fn idempotent_migrations(db: &Db) {
    for _ in 0..3 {
        db.open();
    }
    assert_eq!(db.versions(), store::integer_migration_versions());
    assert_eq!(
        db.named(),
        store::named_migration_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
    db.exec("INSERT INTO hub_schema_migrations (version, applied_at) VALUES (999, 0)")
        .unwrap();
    let error = store::open(&db.url).err().unwrap();
    assert!(
        error.to_string().contains("newer than this binary"),
        "{error}"
    );
}

fn concurrent_startup(db: &Db) {
    let opens: Vec<_> = (0..4)
        .map(|_| {
            let url = db.url.clone();
            thread::spawn(move || store::open(&url).map(drop))
        })
        .collect();
    for open in opens {
        open.join().unwrap().unwrap();
    }
    assert_eq!(db.versions(), store::integer_migration_versions());
}

fn restart_persistence(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let (revoked, revoked_digest) = mint(&*store, &alice.id, "one", TTL);
    let (_, live_digest) = mint(&*store, &alice.id, "two", TTL);
    let (_, bob_digest) = mint(&*store, &bob.id, "one", TTL);
    store.revoke_agent_token(&revoked).unwrap();
    store.set_user_disabled(&bob.id, true).unwrap();
    let events = store.events_after(0, 1000).unwrap();
    assert_eq!(events.len(), 7);
    drop(store);

    let reopened = db.open();
    let now = unix_now();
    assert_eq!(reopened.authenticate(&revoked_digest, now).unwrap(), None);
    assert!(reopened.authenticate(&live_digest, now).unwrap().is_some());
    assert_eq!(reopened.authenticate(&bob_digest, now).unwrap(), None);
    assert_eq!(reopened.events_after(0, 1000).unwrap(), events);
}

fn events_record_each_change(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    assert_eq!(store.upsert_user(&identity(7), "alice").unwrap(), alice);
    store.upsert_user(&identity(7), "alice2").unwrap();
    let (token, _) = mint(&*store, &alice.id, "builder", TTL);
    store.set_user_disabled(&alice.id, true).unwrap();
    store.set_user_disabled(&alice.id, true).unwrap();
    assert!(mint_agent_token(&*store, &alice.id, "late", REPO, TTL).is_err());
    store.set_user_disabled(&alice.id, false).unwrap();
    store.set_user_disabled(&alice.id, false).unwrap();
    assert!(store.revoke_agent_token(&token).unwrap());
    assert!(!store.revoke_agent_token(&token).unwrap());

    assert_eq!(
        kinds(&*store),
        [
            "user_created",
            "user_login_changed",
            "agent_token_created",
            "user_disabled",
            "user_enabled",
            "agent_token_revoked"
        ]
    );
    let events = store.events_after(0, 1000).unwrap();
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    for event in &events {
        assert_id(&event.id, "evt_");
    }
    assert_eq!(events[0].subject, alice.id);
    assert_eq!(
        events[0].data,
        r#"{"host":"github.com","login":"alice","provider":"github","user_id":7}"#
    );
    assert_eq!(events[1].data, r#"{"login":"alice2","previous":"alice"}"#);
    assert_eq!(events[2].subject, token);
    let created: serde_json::Value = serde_json::from_str(&events[2].data).unwrap();
    assert_eq!(created["owner"], json!(alice.id));
    assert_eq!(created["name"], "builder");
    assert_eq!(created["repository"], REPO);
    let keys: Vec<_> = created.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "expires_at",
            "name",
            "owner",
            "read_only",
            "repository",
            "repository_id"
        ]
    );
    assert!(created["repository_id"].is_null());
    assert_eq!(events[5].subject, token);

    let page = store.events_after(events[1].seq, 2).unwrap();
    assert_eq!(page, events[2..4]);
    assert!(store.events_after(events[5].seq, 10).unwrap().is_empty());
    assert_eq!(store.events_after(0, usize::MAX).unwrap(), events);

    let mut tampering = vec![
        "UPDATE hub_events SET kind = 'x'".to_owned(),
        "DELETE FROM hub_events".to_owned(),
    ];
    if db.pg.is_some() {
        // CASCADE gets past the new work-history foreign keys so PostgreSQL
        // reaches the hub_events append-only truncate trigger itself.
        tampering.push("TRUNCATE hub_events CASCADE".into());
    } else {
        tampering.push(format!(
            "INSERT OR REPLACE INTO hub_events (seq, id, at, kind, subject, data) \
             VALUES ({}, 'evt_forged', 0, 'forged', 'x', '{{}}')",
            events[0].seq
        ));
    }
    for sql in tampering {
        let error = db.exec(&sql).unwrap_err();
        assert!(error.contains("append-only"), "{sql}: {error}");
    }
    assert_eq!(store.events_after(0, 1000).unwrap(), events);
}

fn validation(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let long = "a".repeat(256);
    for repository in [
        "..",
        "/abs",
        "one-segment",
        "a/../b",
        "a//b",
        "a/b/",
        "a/b c",
        &long,
    ] {
        assert!(
            mint_agent_token(&*store, &alice.id, "builder", repository, TTL).is_err(),
            "{repository}"
        );
    }
    assert!(mint_agent_token(&*store, &alice.id, "builder", "group/sub/repo", TTL).is_ok());
    for name in ["", "has space", &"n".repeat(65)] {
        assert!(mint_agent_token(&*store, &alice.id, name, REPO, TTL).is_err());
    }
    for ttl in [0, -1, MAX_AGENT_TOKEN_TTL_SECONDS + 1] {
        assert!(mint_agent_token(&*store, &alice.id, "builder", REPO, ttl).is_err());
    }
    assert!(
        mint_agent_token(
            &*store,
            &alice.id,
            "builder",
            REPO,
            MAX_AGENT_TOKEN_TTL_SECONDS
        )
        .is_ok()
    );
    assert!(mint_agent_token(&*store, "usr_unknown", "builder", REPO, TTL).is_err());
    let stale = NewAgentToken {
        owner: alice.id.clone(),
        name: "builder".into(),
        repository: REPO.into(),
        digest: agent_token_digest(&"1".repeat(64)).unwrap(),
        expires_at: unix_now(),
        repository_id: None,
        read_only: false,
    };
    assert!(store.insert_agent_token(&stale).is_err());
    for id in [0, -5] {
        assert!(
            mint_bound_agent_token(&*store, &alice.id, "builder", REPO, id, TTL, false).is_err()
        );
    }
    let mut bad = identity(7);
    bad.provider = "bitbucket".into();
    assert!(store.upsert_user(&bad, "alice").is_err());
    for host in ["", "GitHub.com", "git hub", &"h".repeat(254)] {
        bad = identity(7);
        bad.host = host.into();
        assert!(store.upsert_user(&bad, "alice").is_err(), "{host}");
    }
    assert!(store.upsert_user(&identity(0), "alice").is_err());
    assert!(store.upsert_user(&identity(7), "").is_err());
    assert!(store.upsert_user(&identity(7), "a\nb").is_err());
    let before = kinds(&*store).len();
    assert!(mint_agent_token(&*store, &alice.id, "bad name", REPO, TTL).is_err());
    assert_eq!(kinds(&*store).len(), before);
}

fn repository_id_round_trips(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (plain, plain_secret) = mint_agent_token(&*store, &alice.id, "plain", REPO, TTL).unwrap();
    let (bound, bound_secret, _) =
        mint_bound_agent_token(&*store, &alice.id, "bound", REPO, 4242, TTL, false).unwrap();
    let now = unix_now();
    let digest =
        |secret: &agentscm::team::auth::Secret| agent_token_digest(secret.expose()).unwrap();
    let principal = store
        .authenticate(&digest(&bound_secret), now)
        .unwrap()
        .unwrap();
    assert_eq!(principal.repository_id, Some(4242));
    let principal = store
        .authenticate(&digest(&plain_secret), now)
        .unwrap()
        .unwrap();
    assert_eq!(principal.repository_id, None);
    let listed = store.list_agent_tokens(&alice.id).unwrap();
    let ids: Vec<_> = listed
        .iter()
        .map(|t| (t.id.clone(), t.repository_id))
        .collect();
    assert!(ids.contains(&(bound.clone(), Some(4242))));
    assert!(ids.contains(&(plain, None)));
    let created = &store.events_after(0, 1000).unwrap()[2];
    assert_eq!(created.subject, bound);
    let data: serde_json::Value = serde_json::from_str(&created.data).unwrap();
    assert_eq!(data["repository_id"], 4242);
}

fn read_only_round_trips(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (agent, agent_secret) = mint_agent_token(&*store, &alice.id, "agent", REPO, TTL).unwrap();
    let (page, page_secret, _) =
        mint_bound_agent_token(&*store, &alice.id, "page", REPO, 6, TTL, true).unwrap();
    let (bound, bound_secret, _) =
        mint_bound_agent_token(&*store, &alice.id, "bound", REPO, 5, TTL, true).unwrap();
    let now = unix_now();
    let read_only = |secret: &agentscm::team::auth::Secret| {
        let digest = agent_token_digest(secret.expose()).unwrap();
        store.authenticate(&digest, now).unwrap().unwrap().read_only
    };
    assert!(!read_only(&agent_secret) && read_only(&page_secret) && read_only(&bound_secret));
    let listed = store.list_agent_tokens(&alice.id).unwrap();
    for (id, expected) in [(&agent, false), (&page, true), (&bound, true)] {
        assert_eq!(
            listed.iter().find(|t| &t.id == id).unwrap().read_only,
            expected
        );
    }
    let created: Vec<_> = store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "agent_token_created")
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap()["read_only"].clone())
        .collect();
    assert_eq!(created, [json!(false), json!(true), json!(true)]);
}

fn list_and_owned_revoke(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let (old, _) = mint(&*store, &alice.id, "old", TTL);
    let (new, new_digest) = mint(&*store, &alice.id, "new", TTL);
    let (theirs, theirs_digest) = mint(&*store, &bob.id, "theirs", TTL);
    db.exec(&format!(
        "UPDATE hub_agent_tokens SET created_at = 100 WHERE id = '{old}'"
    ))
    .unwrap();
    let listed = store.list_agent_tokens(&alice.id).unwrap();
    assert_eq!(
        listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        [new.as_str(), old.as_str()]
    );
    assert_eq!(listed[1].created_at, 100);
    assert!(!listed[0].revoked);
    assert_eq!(listed[0].name, "new");
    assert!(store.list_agent_tokens("usr_unknown").unwrap().is_empty());

    let before = kinds(&*store).len();
    assert!(!store.revoke_owned_agent_token(&alice.id, &theirs).unwrap());
    assert!(
        !store
            .revoke_owned_agent_token(&alice.id, "tok_unknown")
            .unwrap()
    );
    assert!(
        store
            .authenticate(&theirs_digest, unix_now())
            .unwrap()
            .is_some()
    );
    assert_eq!(kinds(&*store).len(), before);

    assert!(store.revoke_owned_agent_token(&alice.id, &new).unwrap());
    assert!(!store.revoke_owned_agent_token(&alice.id, &new).unwrap());
    assert_eq!(store.authenticate(&new_digest, unix_now()).unwrap(), None);
    assert!(store.list_agent_tokens(&alice.id).unwrap()[0].revoked);
    let last = store.events_after(0, 1000).unwrap().pop().unwrap();
    assert_eq!(
        (last.kind.as_str(), last.subject.as_str()),
        ("agent_token_revoked", new.as_str())
    );
    assert_eq!(last.data, json!({"by": alice.id}).to_string());
}

fn admin_actions_and_sign_in(db: &Db) {
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let users = store.list_users().unwrap();
    assert_eq!(users.len(), 2);
    assert!(users.contains(&alice) && users.contains(&bob));
    store
        .set_user_disabled_by(&alice.id, &bob.id, true)
        .unwrap();
    store
        .set_user_disabled_by(&alice.id, &bob.id, true)
        .unwrap();
    assert!(
        store
            .list_users()
            .unwrap()
            .iter()
            .any(|u| u.id == bob.id && u.disabled)
    );
    store
        .set_user_disabled_by(&alice.id, &bob.id, false)
        .unwrap();
    assert!(
        store
            .set_user_disabled_by(&alice.id, "usr_unknown", true)
            .is_err()
    );
    store.record_sign_in(&alice.id, "browser", true).unwrap();
    store.record_sign_in(&bob.id, "device", false).unwrap();
    assert!(
        store
            .record_sign_in("usr_unknown", "browser", false)
            .is_err()
    );

    let events = store.events_after(0, 1000).unwrap();
    let tail: Vec<_> = events[2..]
        .iter()
        .map(|e| (e.kind.as_str(), e.subject.as_str(), e.data.clone()))
        .collect();
    let by = json!({"by": alice.id}).to_string();
    assert_eq!(
        tail,
        [
            ("user_disabled", bob.id.as_str(), by.clone()),
            ("user_enabled", bob.id.as_str(), by),
            (
                "user_signed_in",
                alice.id.as_str(),
                r#"{"admin":true,"method":"browser"}"#.into()
            ),
            (
                "user_signed_in",
                bob.id.as_str(),
                r#"{"admin":false,"method":"device"}"#.into()
            ),
        ]
    );
}

fn upgrade_from_migration_1(db: &Db) {
    use sha2::{Digest, Sha256};
    let core = if db.pg.is_some() {
        include_str!("../../src/hub/migrations/0001_core.pg.sql")
    } else {
        include_str!("../../src/hub/migrations/0001_core.sqlite.sql")
    };
    // A user and a token written by a version-1 binary.
    let raw = "1".repeat(64);
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm.hub.agent-token.v1\0");
    hasher.update(raw.as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    db.exec(&format!(
        "{core}
         CREATE TABLE hub_schema_migrations (version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL);
         INSERT INTO hub_schema_migrations (version, applied_at) VALUES (1, 0);
         INSERT INTO hub_users (id, provider, host, forge_user_id, login, created_at)
             VALUES ('usr_legacy', 'github', 'github.com', 7, 'alice', 0);
         INSERT INTO hub_agent_tokens (id, owner, name, repository, digest, expires_at, created_at)
             VALUES ('tok_legacy', 'usr_legacy', 'builder', 'acme/widgets', '{digest}', 4102444800, 0);"
    ))
    .unwrap();
    let store = db.open();
    assert_eq!(db.versions(), store::integer_migration_versions());
    let principal = store
        .authenticate(&agent_token_digest(&raw).unwrap(), unix_now())
        .unwrap()
        .unwrap();
    assert_eq!(principal.token_id, "tok_legacy");
    assert_eq!(principal.repository_id, None);
    let listed = store.list_agent_tokens("usr_legacy").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repository_id, None);
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    assert_eq!(alice.id, "usr_legacy");
    mint_bound_agent_token(&*store, &alice.id, "bound", REPO, 9, TTL, false).unwrap();
}

fn upgrade_from_hub_settings_migration(db: &Db) {
    let migrations = if db.pg.is_some() {
        [
            include_str!("../../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../../src/hub/migrations/0003_intents.pg.sql"),
            include_str!("../../src/hub/migrations/0004_github.pg.sql"),
            include_str!("../../src/hub/migrations/0005_publish.pg.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.pg.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.pg.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.pg.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.pg.sql"),
            include_str!("../../src/hub/migrations/0010_work.pg.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.pg.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.pg.sql"),
            include_str!("../../src/hub/migrations/0013_coordination_requests.pg.sql"),
            include_str!("../../src/hub/migrations/0014_coordination_retention.pg.sql"),
            include_str!("../../src/hub/migrations/0015_candidate_publications.pg.sql"),
            include_str!("../../src/hub/migrations/0016_forge_checks.pg.sql"),
            include_str!("../../src/hub/migrations/0017_external_runs.pg.sql"),
            include_str!("../../src/hub/migrations/0018_hub_settings.pg.sql"),
        ]
    } else {
        [
            include_str!("../../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../../src/hub/migrations/0003_intents.sqlite.sql"),
            include_str!("../../src/hub/migrations/0004_github.sqlite.sql"),
            include_str!("../../src/hub/migrations/0005_publish.sqlite.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.sqlite.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.sqlite.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.sqlite.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.sqlite.sql"),
            include_str!("../../src/hub/migrations/0010_work.sqlite.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.sqlite.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.sqlite.sql"),
            include_str!("../../src/hub/migrations/0013_coordination_requests.sqlite.sql"),
            include_str!("../../src/hub/migrations/0014_coordination_retention.sqlite.sql"),
            include_str!("../../src/hub/migrations/0015_candidate_publications.sqlite.sql"),
            include_str!("../../src/hub/migrations/0016_forge_checks.sqlite.sql"),
            include_str!("../../src/hub/migrations/0017_external_runs.sqlite.sql"),
            include_str!("../../src/hub/migrations/0018_hub_settings.sqlite.sql"),
        ]
    };
    for sql in migrations {
        db.exec(sql).unwrap();
    }
    db.exec(
        "CREATE TABLE hub_schema_migrations (version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL)",
    )
    .unwrap();
    for version in 1..=18 {
        db.exec(&format!(
            "INSERT INTO hub_schema_migrations VALUES ({version},0)"
        ))
        .unwrap();
    }

    db.open();
    assert_eq!(db.versions(), store::integer_migration_versions());
    db.exec("SELECT COUNT(*) FROM hub_external_runs; SELECT COUNT(*) FROM hub_settings; SELECT COUNT(*) FROM hub_work_model_usage;")
        .unwrap();
}

fn named_migrations_are_a_set(db: &Db) {
    db.open();
    assert_eq!(
        db.named(),
        store::named_migration_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    );
    let columns = if db.pg.is_some() {
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'hub_publications' \
         AND column_name = 'authorization_binding'"
    } else {
        "SELECT COUNT(*) FROM pragma_table_info('hub_publications') \
         WHERE name = 'authorization_binding'"
    };
    assert_eq!(db.count(columns), 1);
    assert_eq!(db.count("SELECT COUNT(*) FROM hub_change_shared_bodies"), 0);
    db.exec(
        "INSERT INTO hub_named_migrations (name, applied_at) VALUES ('from_a_newer_binary', 0)",
    )
    .unwrap();
    let error = store::open(&db.url).err().unwrap();
    assert!(
        error.to_string().contains("newer than this binary"),
        "{error}"
    );
}

fn device_token(owner: &str, repository: &str) -> (NewAgentToken, store::TokenDigest) {
    let secret = agentscm::team::auth::generate_secret().unwrap();
    let digest = agent_token_digest(secret.expose()).unwrap();
    let token = NewAgentToken {
        owner: owner.into(),
        name: "client-laptop".into(),
        repository: repository.into(),
        digest: digest.clone(),
        expires_at: unix_now() + TTL,
        repository_id: Some(4242),
        read_only: false,
    };
    (token, digest)
}

fn devices_and_refusals(db: &Db) {
    use agentscm::hub::device::{DEVICE_IDLE_SECONDS, Refusal};
    use store::device_digest;

    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let now = unix_now();
    let laptop = device_digest(&"1".repeat(64)).unwrap();
    let first = store
        .insert_device(&alice.id, "alice's MacBook Pro", &laptop, None, None, now)
        .unwrap();
    assert_id(&first, "dev_");
    // A credential digest is never an agent token, and the reverse.
    assert_eq!(store.authenticate(&laptop, now).unwrap(), None);
    let principal = store.authenticate_device(&laptop, now).unwrap().unwrap();
    assert_eq!(
        (principal.device_id.as_str(), &principal.owner),
        (first.as_str(), &alice)
    );
    assert_eq!(principal.app_id, None);
    assert!(
        store
            .insert_device(&alice.id, "", &laptop, None, None, now)
            .is_err()
    );

    // Minting through a computer records it; an agent token is never a computer credential.
    let (token, old_digest) = device_token(&alice.id, REPO);
    store.insert_device_token(&token, &first, true).unwrap();
    assert_eq!(store.authenticate_device(&old_digest, now).unwrap(), None);
    let (token, new_digest) = device_token(&alice.id, REPO);
    store.insert_device_token(&token, &first, false).unwrap();
    assert!(store.authenticate(&old_digest, now).unwrap().is_some());
    assert!(store.authenticate(&new_digest, now).unwrap().is_some());
    assert_eq!(events_of(&*store, "auto_connected").len(), 1);
    // Another person's computer cannot mint for alice.
    let bobs = store
        .insert_device(
            &bob.id,
            "bob's",
            &device_digest(&"2".repeat(64)).unwrap(),
            None,
            None,
            now,
        )
        .unwrap();
    assert!(
        store
            .insert_device_token(&device_token(&alice.id, REPO).0, &bobs, false)
            .is_err()
    );
    let (other_repo, _) = device_token(&alice.id, "acme/other");
    store
        .insert_device_token(&other_repo, &first, false)
        .unwrap();

    let listed = store.devices(Some(&alice.id), now).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repositories, ["acme/other", REPO]);
    assert_eq!(store.devices(None, now).unwrap().len(), 2);
    assert!(
        store
            .rename_device(&first, &alice.id, "work laptop")
            .unwrap()
    );
    assert!(!store.rename_device(&first, &bob.id, "stolen").unwrap());

    // Idle past the limit, disabled owner and revocation all stop the credential.
    assert_eq!(
        store
            .authenticate_device(&laptop, now + DEVICE_IDLE_SECONDS + 1)
            .unwrap(),
        None
    );
    store.set_user_disabled(&alice.id, true).unwrap();
    assert!(
        store
            .authenticate_device(&laptop, now)
            .unwrap()
            .unwrap()
            .owner
            .disabled
    );
    assert!(
        store
            .insert_device_token(&device_token(&alice.id, REPO).0, &first, false)
            .is_err()
    );
    store.set_user_disabled(&alice.id, false).unwrap();
    assert!(
        !store
            .revoke_device(&first, Some(&bob.id), &bob.id, now)
            .unwrap()
    );
    assert!(
        store
            .revoke_device(&first, Some(&alice.id), &bobs, now)
            .unwrap()
    );
    assert!(!store.revoke_device(&first, None, &alice.id, now).unwrap());
    assert_eq!(store.authenticate_device(&laptop, now).unwrap(), None);
    assert_eq!(store.authenticate(&new_digest, now).unwrap(), None);
    assert!(
        store.devices(Some(&alice.id), now).unwrap()[0]
            .revoked_at
            .is_some()
    );
    assert_eq!(events_of(&*store, "device_revoked"), [json!({"by": bobs})]);
    assert!(
        store
            .insert_device_token(&device_token(&alice.id, REPO).0, &first, false)
            .is_err()
    );

    // Signing in again with the old credential replaces that computer only.
    let second = device_digest(&"3".repeat(64)).unwrap();
    let replaced = store
        .insert_device(&bob.id, "bob's", &second, Some(&bobs), None, now)
        .unwrap();
    assert_ne!(replaced, bobs);
    assert_eq!(
        store
            .devices(Some(&bob.id), now)
            .unwrap()
            .iter()
            .filter(|device| device.revoked_at.is_none())
            .count(),
        1
    );

    // One row per person and reason; repeats bump the count.
    let refusal = |user_id: i64, reason: &str, at: i64| Refusal {
        provider: "github".into(),
        host: "github.com".into(),
        forge_user_id: user_id,
        login: format!("user{user_id}"),
        reason: reason.into(),
        via: "sign_in".into(),
        at,
        count: 1,
    };
    store
        .record_refusal(&refusal(9, "not_allowed", now - 10))
        .unwrap();
    store
        .record_refusal(&refusal(9, "not_allowed", now))
        .unwrap();
    store
        .record_refusal(&refusal(10, "no_repository_role", now - 5))
        .unwrap();
    let rows = store.refusals(now).unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.forge_user_id, r.count, r.at))
            .collect::<Vec<_>>(),
        [(9, 2, now), (10, 1, now - 5)]
    );
    for user in 100..320 {
        store
            .record_refusal(&refusal(user, "not_allowed", now + user))
            .unwrap();
    }
    assert_eq!(db.count("SELECT COUNT(*) FROM hub_sign_in_refusals"), 200);
    store
        .record_refusal(&refusal(1, "not_allowed", now + 31 * 24 * 60 * 60))
        .unwrap();
    assert_eq!(store.refusals(now + 31 * 24 * 60 * 60).unwrap().len(), 1);

    // Restart keeps computers.
    drop(store);
    let reopened = db.open();
    assert!(
        reopened
            .authenticate_device(&second, now)
            .unwrap()
            .is_some()
    );
    let carol = reopened.upsert_user(&identity(12), "carol").unwrap();
    // A connected app's credential is a computer row that names its app.
    let app_digest = device_digest(&"e".repeat(64)).unwrap();
    let app = reopened
        .insert_device(
            &carol.id,
            "Example App",
            &app_digest,
            None,
            Some("example-app"),
            now,
        )
        .unwrap();
    let principal = reopened
        .authenticate_device(&app_digest, now)
        .unwrap()
        .unwrap();
    assert_eq!(principal.app_id.as_deref(), Some("example-app"));
    let listed = reopened.devices(Some(&carol.id), now).unwrap();
    let listed: Vec<_> = listed
        .iter()
        .map(|d| (d.id.as_str(), d.app_id.as_deref()))
        .collect();
    assert!(
        listed.contains(&(app.as_str(), Some("example-app"))),
        "{listed:?}"
    );
    // Connecting again retires the earlier credential for that app, and only that one.
    let other_digest = device_digest(&"f".repeat(64)).unwrap();
    reopened
        .insert_device(
            &carol.id,
            "Other",
            &other_digest,
            None,
            Some("other-app"),
            now,
        )
        .unwrap();
    let again = device_digest(&"d".repeat(64)).unwrap();
    let app2 = reopened
        .insert_device(
            &carol.id,
            "Example App",
            &again,
            None,
            Some("example-app"),
            now,
        )
        .unwrap();
    assert!(
        reopened
            .authenticate_device(&app_digest, now)
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .authenticate_device(&other_digest, now)
            .unwrap()
            .is_some()
    );
    assert!(reopened.authenticate_device(&again, now).unwrap().is_some());
    // An app left out of the registered list loses its credentials.
    assert_eq!(
        reopened
            .revoke_unregistered_apps(&["example-app".to_owned()], "hub", now)
            .unwrap(),
        1
    );
    assert!(
        reopened
            .authenticate_device(&other_digest, now)
            .unwrap()
            .is_none()
    );
    assert!(reopened.authenticate_device(&again, now).unwrap().is_some());
    assert!(
        reopened
            .revoke_device(&app2, Some(&carol.id), &carol.id, now)
            .unwrap()
    );
    // Racing connects from separate hub processes still leave exactly one live credential.
    let start = Arc::new(std::sync::Barrier::new(8));
    let racers: Vec<_> = (0..8)
        .map(|i| {
            let (url, owner, start) = (db.url.clone(), carol.id.clone(), start.clone());
            thread::spawn(move || {
                let store = store::open(&url).unwrap();
                let digest = device_digest(&format!("{:064x}", 0xa9_0000 + i)).unwrap();
                start.wait();
                store
                    .insert_device(
                        &owner,
                        "Example App",
                        &digest,
                        None,
                        Some("example-app"),
                        now,
                    )
                    .unwrap();
                digest
            })
        })
        .collect();
    let raced: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    let live = raced
        .iter()
        .filter(|d| reopened.authenticate_device(d, now).unwrap().is_some())
        .count();
    assert_eq!(live, 1);
}

fn browser_sessions(db: &Db) {
    use store::{
        BROWSER_SESSION_MAX_SECONDS, MAX_BROWSER_SESSIONS, SLIDE_STEP_SECONDS,
        browser_session_digest,
    };

    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let now = unix_now();
    let digest = |n: usize| browser_session_digest(&format!("{n:064}")).unwrap();
    store
        .insert_browser_session(&alice.id, &digest(0), now + 100, now)
        .unwrap();
    let (user, expires) = store
        .browser_session(&digest(0), now, now)
        .unwrap()
        .unwrap();
    assert_eq!((user.id.as_str(), expires), (alice.id.as_str(), now + 100));
    // The expiry slides only by a real step, so most requests write nothing.
    let (_, expires) = store
        .browser_session(&digest(0), now, now + 100 + SLIDE_STEP_SECONDS - 1)
        .unwrap()
        .unwrap();
    assert_eq!(expires, now + 100);
    let later = now + 100 + SLIDE_STEP_SECONDS + 1;
    assert_eq!(
        store
            .browser_session(&digest(0), now, later)
            .unwrap()
            .unwrap()
            .1,
        later
    );
    assert_eq!(
        store.browser_session(&digest(0), later, later).unwrap(),
        None
    );
    // However often it is used, a sign-in ends a week after it began.
    let old = now - BROWSER_SESSION_MAX_SECONDS;
    store
        .insert_browser_session(&alice.id, &digest(20), now + 100, old)
        .unwrap();
    assert_eq!(store.browser_session(&digest(20), now, now).unwrap(), None);
    // Disabling the owner deletes their sign-ins, so enabling them again brings none back; a
    // disabled owner cannot get a new one.
    store.set_user_disabled(&alice.id, true).unwrap();
    assert_eq!(db.count("SELECT COUNT(*) FROM hub_browser_sessions"), 0);
    assert!(
        store
            .insert_browser_session(&alice.id, &digest(1), now + 100, now)
            .is_err()
    );
    store.set_user_disabled(&alice.id, false).unwrap();
    assert_eq!(store.browser_session(&digest(0), now, now).unwrap(), None);
    // At most eight per person: the oldest go first.
    for n in 1..=MAX_BROWSER_SESSIONS as usize + 1 {
        store
            .insert_browser_session(&alice.id, &digest(n), now + 1000, now + n as i64)
            .unwrap();
    }
    assert_eq!(
        db.count("SELECT COUNT(*) FROM hub_browser_sessions"),
        MAX_BROWSER_SESSIONS
    );
    assert_eq!(store.browser_session(&digest(1), now, now).unwrap(), None);
    assert!(store.delete_browser_session(&digest(9)).unwrap());
    assert!(!store.delete_browser_session(&digest(9)).unwrap());
    // A restart keeps them.
    drop(store);
    assert!(
        db.open()
            .browser_session(&digest(8), now, now)
            .unwrap()
            .is_some()
    );
}

fn app(host: &str) -> NewConnection {
    NewConnection {
        id: new_connection_id().unwrap(),
        provider: "github".into(),
        host: host.into(),
        kind: "app".into(),
        app_id: Some(42),
        app_slug: Some("agentscm-test".into()),
        acts_as: None,
    }
}

fn pat(host: &str, login: &str) -> NewConnection {
    NewConnection {
        kind: "pat".into(),
        app_id: None,
        app_slug: None,
        acts_as: Some(login.into()),
        ..app(host)
    }
}

fn events_of(store: &dyn Store, kind: &str) -> Vec<Value> {
    let events = store.events_after(0, 1000).unwrap();
    events
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| serde_json::from_str(&event.data).unwrap())
        .collect()
}

fn connections_and_repositories(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    assert_id(&con.id, "con_");
    assert_eq!(
        (con.status.as_str(), con.outbound, con.next_poll_at),
        ("enabled", false, 0)
    );
    assert_eq!(
        events_of(&*store, "connection_created"),
        [
            json!({"provider": "github", "host": "github.com", "kind": "app", "app_id": 42,
            "app_slug": "agentscm-test", "acts_as": null, "by": "usr_admin"})
        ]
    );
    assert!(store.insert_connection(&app("github.com"), "x").is_ok());
    let mut dup = app("github.com");
    dup.id = con.id.clone();
    assert!(store.insert_connection(&dup, "x").is_err());
    for bad in [
        NewConnection {
            kind: "oauth".into(),
            ..app("github.com")
        },
        NewConnection {
            provider: "gitlab".into(),
            ..app("github.com")
        },
        NewConnection {
            acts_as: Some("x".into()),
            ..app("github.com")
        },
        NewConnection {
            app_id: None,
            ..app("github.com")
        },
        NewConnection {
            id: "con_short".into(),
            ..app("github.com")
        },
        NewConnection {
            host: "Bad Host".into(),
            ..app("github.com")
        },
        NewConnection {
            acts_as: None,
            ..pat("github.com", "x")
        },
    ] {
        assert!(store.insert_connection(&bad, "x").is_err(), "{bad:?}");
    }
    assert_eq!(store.connection(&con.id).unwrap().unwrap(), con);
    assert_eq!(store.connection("con_unknown").unwrap(), None);
    assert_eq!(store.pat_connection("github.com").unwrap(), None);
    let token = store
        .insert_connection(&pat("github.com", "robot"), "usr_admin")
        .unwrap();
    assert_eq!(store.pat_connection("github.com").unwrap().unwrap(), token);
    assert_eq!(store.pat_connection("ghe.example.com").unwrap(), None);
    assert_eq!(store.connections().unwrap().len(), 3);

    // Status and outbound: an event only when something changes.
    let before = events_of(&*store, "connection_status").len();
    store
        .set_connection_status(&con.id, "auth_failed", Some("401 on app"))
        .unwrap();
    store
        .set_connection_status(&con.id, "auth_failed", Some("401 on app"))
        .unwrap();
    store
        .set_connection_status(&con.id, "auth_failed", Some("403 on repo"))
        .unwrap();
    store
        .set_connection_status(&con.id, "enabled", None)
        .unwrap();
    store
        .set_connection_status(&con.id, "enabled", None)
        .unwrap();
    assert_eq!(events_of(&*store, "connection_status").len(), before + 3);
    assert!(
        store
            .set_connection_status("con_unknown", "enabled", None)
            .is_err()
    );
    assert!(store.set_connection_status(&con.id, "weird", None).is_err());
    store
        .set_connection_outbound(&con.id, true, "usr_admin")
        .unwrap();
    store
        .set_connection_outbound(&con.id, true, "usr_admin")
        .unwrap();
    assert_eq!(
        events_of(&*store, "connection_outbound"),
        [json!({"enabled": true, "by": "usr_admin"})]
    );
    assert!(store.connection(&con.id).unwrap().unwrap().outbound);
    assert!(
        store
            .set_connection_outbound("con_unknown", true, "x")
            .is_err()
    );
    store.set_next_poll(&con.id, 1234).unwrap();
    assert_eq!(
        store.connection(&con.id).unwrap().unwrap().next_poll_at,
        1234
    );

    // Repositories are keyed by (host, repository id): a rename keeps the row.
    let repo = store
        .upsert_repository(
            &con.id,
            "github.com",
            4242,
            "Acme/Widgets",
            Some("R_node"),
            Some(7),
        )
        .unwrap();
    assert_id(&repo.id, "rep_");
    assert_eq!(
        events_of(&*store, "repository_connected"),
        [
            json!({"connection": con.id, "repository_id": 4242, "full_name": "Acme/Widgets",
            "node_id": "R_node", "installation_id": 7})
        ]
    );
    let again = store
        .upsert_repository(
            &con.id,
            "github.com",
            4242,
            "Acme/Widgets",
            Some("R_node"),
            Some(7),
        )
        .unwrap();
    assert_eq!(again, repo);
    assert_eq!(events_of(&*store, "repository_connected").len(), 1);
    assert!(events_of(&*store, "repository_renamed").is_empty());
    let renamed = store
        .upsert_repository(&con.id, "github.com", 4242, "Acme/Gadgets", None, None)
        .unwrap();
    assert_eq!(
        (
            &renamed.id,
            renamed.full_name.as_str(),
            renamed.node_id.as_deref(),
            renamed.installation_id
        ),
        (&repo.id, "Acme/Gadgets", Some("R_node"), Some(7))
    );
    assert_eq!(
        events_of(&*store, "repository_renamed"),
        [json!({"repository_id": 4242, "previous": "Acme/Widgets", "full_name": "Acme/Gadgets"})]
    );
    assert_eq!(
        store
            .repository_named("github.com", "acme/GADGETS")
            .unwrap(),
        Some(renamed.clone())
    );
    assert_eq!(
        store
            .repository_named("github.com", "acme/widgets")
            .unwrap(),
        None
    );
    assert_eq!(
        store.repository_named("other.com", "acme/gadgets").unwrap(),
        None
    );
    assert_eq!(
        store.repository_by_id("github.com", 4242).unwrap(),
        Some(renamed.clone())
    );
    assert_eq!(store.repositories(&con.id).unwrap(), vec![renamed.clone()]);
    assert_eq!(
        store.repositories_on("github.com", 50).unwrap(),
        vec![renamed.clone()]
    );
    // Another connection's claim on the same repository changes nothing.
    let claimed = store
        .upsert_repository(&token.id, "github.com", 4242, "evil/name", None, None)
        .unwrap();
    assert_eq!(claimed, renamed);
    assert!(
        store
            .upsert_repository(&con.id, "other.com", 1, "a/b", None, None)
            .is_err()
    );
    assert!(
        store
            .upsert_repository("con_unknown", "github.com", 1, "a/b", None, None)
            .is_err()
    );
    assert!(
        store
            .upsert_repository(&con.id, "github.com", 0, "a/b", None, None)
            .is_err()
    );
    assert!(
        store
            .upsert_repository(&con.id, "github.com", 9, "nonsense", None, None)
            .is_err()
    );
    store.set_pulls_etag(&repo.id, Some("W/\"abc\"")).unwrap();
    let stored = store.repository_by_id("github.com", 4242).unwrap().unwrap();
    assert_eq!(stored.pulls_etag.as_deref(), Some("W/\"abc\""));
    store.set_pulls_etag(&repo.id, None).unwrap();
    assert_eq!(
        store
            .repository_by_id("github.com", 4242)
            .unwrap()
            .unwrap()
            .pulls_etag,
        None
    );
}

fn prs(store: &dyn Store, repo: &str) -> Vec<agentscm::hub::connection::Observation> {
    store.observations(repo, "pull_request").unwrap()
}

fn observations(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(
            &con.id,
            "github.com",
            4242,
            "acme/widgets",
            Some("R_node"),
            None,
        )
        .unwrap();
    let pr = |sha: &str| json!({"number": 5, "node_id": "PR_node", "head_sha": sha}).to_string();
    let observe = |data: &str, at: i64| {
        store
            .observe(&repo.id, "pull_request", "5", data, at)
            .unwrap()
    };

    assert_eq!(
        observe(&pr("aaa"), 100),
        Observed::Changed { previous: None }
    );
    assert_eq!(
        events_of(&*store, "pull_request_observed"),
        [json!({"number": 5, "node_id": "PR_node", "head_sha": "aaa",
            "repository_id": 4242, "full_name": "acme/widgets"})]
    );
    // Same data: only the read time moves, no event.
    assert_eq!(observe(&pr("aaa"), 200), Observed::Unchanged);
    // An older read than the stored one is ignored, whatever it says.
    assert_eq!(observe(&pr("old"), 150), Observed::Stale);
    assert_eq!(events_of(&*store, "pull_request_observed").len(), 1);
    let changed = observe(&pr("bbb"), 300);
    assert_eq!(
        changed,
        Observed::Changed {
            previous: Some(pr("aaa"))
        }
    );
    let all = prs(&*store, &repo.id);
    assert_eq!(
        (all.len(), all[0].data.as_str(), all[0].observed_at),
        (1, pr("bbb").as_str(), 300)
    );

    // A close from an older read does nothing; a newer one forgets and tombstones.
    let closed = json!({"number": 5, "state": "closed", "merged": true, "merge_commit_sha": "ccc"});
    assert!(
        !store
            .forget(
                &repo.id,
                "pull_request",
                "5",
                250,
                &closed.to_string(),
                "pull_request_closed"
            )
            .unwrap()
    );
    assert!(
        store
            .forget(
                &repo.id,
                "pull_request",
                "5",
                400,
                &closed.to_string(),
                "pull_request_closed"
            )
            .unwrap()
    );
    assert!(
        !store
            .forget(
                &repo.id,
                "pull_request",
                "5",
                500,
                &closed.to_string(),
                "pull_request_closed"
            )
            .unwrap()
    );
    assert!(
        !store
            .forget(
                &repo.id,
                "pull_request",
                "6",
                500,
                &closed.to_string(),
                "pull_request_closed"
            )
            .unwrap()
    );
    assert!(prs(&*store, &repo.id).is_empty());
    let gone = events_of(&*store, "pull_request_closed");
    assert_eq!(gone.len(), 1);
    assert_eq!(
        (
            gone[0]["merge_commit_sha"].as_str(),
            gone[0]["repository_id"].as_i64()
        ),
        (Some("ccc"), Some(4242))
    );
    // A read that started before the close cannot bring the pull request back; a newer one can.
    assert_eq!(observe(&pr("late"), 450), Observed::Stale);
    assert!(prs(&*store, &repo.id).is_empty());
    assert_eq!(
        observe(&pr("reopened"), 600),
        Observed::Changed { previous: None }
    );
    assert_eq!(prs(&*store, &repo.id).len(), 1);

    // Branches use the same rules and their own events.
    let branch = |sha: &str| json!({"name": "feat/x", "sha": sha}).to_string();
    assert_eq!(
        store
            .observe(&repo.id, "branch", "feat/x", &branch("1"), 10)
            .unwrap(),
        Observed::Changed { previous: None }
    );
    assert!(
        store
            .forget(
                &repo.id,
                "branch",
                "feat/x",
                20,
                &json!({"name": "feat/x"}).to_string(),
                "branch_deleted"
            )
            .unwrap()
    );
    assert_eq!(events_of(&*store, "branch_observed").len(), 1);
    assert_eq!(events_of(&*store, "branch_deleted").len(), 1);
    assert_eq!(prs(&*store, &repo.id).len(), 1);
    assert!(store.observations(&repo.id, "branch").unwrap().is_empty());
    assert!(store.observations(&repo.id, "weird").is_err());

    // Not tracking a branch any more is its own event; a key never seen gets a marker, no event.
    let branch_data = |key: &str| json!({"name": key}).to_string();
    store
        .observe(&repo.id, "branch", "old", &branch_data("old"), 30)
        .unwrap();
    let gone = |key: &str, at: i64, event: &str| {
        store.forget(&repo.id, "branch", key, at, &branch_data(key), event)
    };
    assert!(gone("old", 40, "branch_unobserved").unwrap());
    assert_eq!(events_of(&*store, "branch_unobserved").len(), 1);
    assert!(gone("old", 50, "pull_request_closed").is_err());
    assert!(!gone("never", 60, "branch_deleted").unwrap());
    assert_eq!(events_of(&*store, "branch_deleted").len(), 1);
    let before = store.events_after(0, 1000).unwrap().len();
    let seen = |at| {
        store
            .observe(&repo.id, "branch", "never", &branch_data("never"), at)
            .unwrap()
    };
    assert_eq!(seen(55), Observed::Stale);
    assert_eq!(seen(70), Observed::Changed { previous: None });
    assert_eq!(store.events_after(0, 1000).unwrap().len(), before + 1);
    // Markers are pruned by age; live observations stay.
    assert!(gone("never", 800, "branch_deleted").unwrap());
    assert_eq!(store.observation_floor(&repo.id).unwrap(), 800);
    assert_eq!(store.prune_tombstones(801).unwrap(), 4);
    assert_eq!(store.observation_floor(&repo.id).unwrap(), 600);
    assert_eq!(prs(&*store, &repo.id).len(), 1);

    for bad in [
        ("weird", "k", "{}"),
        ("branch", "", "{}"),
        ("branch", "k", "[]"),
        ("branch", "k", "x"),
    ] {
        assert!(
            store.observe(&repo.id, bad.0, bad.1, bad.2, 1).is_err(),
            "{bad:?}"
        );
    }
    assert!(
        store
            .observe("rep_unknown", "branch", "k", "{}", 1)
            .is_err()
    );
}

fn deliveries_outbound_and_notices(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let other = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    assert!(store.record_delivery(&con.id, "d1", 1000).unwrap());
    assert!(!store.record_delivery(&con.id, "d1", 1001).unwrap());
    assert!(store.record_delivery(&other.id, "d1", 1001).unwrap());
    // Old ids are pruned, so a very late redelivery counts as new.
    assert!(
        store
            .record_delivery(&con.id, "d1", 1000 + 8 * 24 * 3600)
            .unwrap()
    );

    let repo = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
    assert_eq!(store.outbound(&repo.id, 5).unwrap(), None);
    store
        .set_outbound_status(&repo.id, 5, "sha1", "1 overlapping declared intents")
        .unwrap();
    store
        .set_outbound_comment(&repo.id, 5, 99, "digest")
        .unwrap();
    let out = store.outbound(&repo.id, 5).unwrap().unwrap();
    assert_eq!(
        (
            out.comment_id,
            out.body_sha256.as_deref(),
            out.status_sha.as_deref(),
            out.status_description.as_deref()
        ),
        (
            Some(99),
            Some("digest"),
            Some("sha1"),
            Some("1 overlapping declared intents")
        )
    );
    assert_eq!(
        events_of(&*store, "pr_comment_written"),
        [json!({"repository_id": 4242, "number": 5, "comment_id": 99})]
    );
    assert_eq!(
        events_of(&*store, "pr_status_written"),
        [json!({"repository_id": 4242, "number": 5, "sha": "sha1",
            "description": "1 overlapping declared intents"})]
    );
    assert!(
        store
            .set_outbound_comment("rep_unknown", 1, 1, "x")
            .is_err()
    );

    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (token, _) = mint_agent_token(&*store, &alice.id, "agent", REPO, TTL).unwrap();
    let intent = NewIntent {
        id: format!("int_{:032x}", 1),
        task: "t".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["src/a.rs".into()],
        ttl_seconds: 600,
    };
    store.announce_intent(&token, &intent, unix_now()).unwrap();
    store.append_notices("pr_notice", &[]).unwrap();
    store
        .append_notices(
            "pr_notice",
            &[(
                intent.id.clone(),
                json!({"about": "pull_request"}).to_string(),
            )],
        )
        .unwrap();
    let notices = store.notices_after(&token, 0, 10).unwrap();
    assert_eq!(
        notices
            .iter()
            .map(|n| (n.kind.as_str(), n.subject.as_str()))
            .collect::<Vec<_>>(),
        [("pr_notice", intent.id.as_str())]
    );
}

fn merge_group_store(db: &Db) -> (Arc<dyn Store>, String) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
    (store, repo.id)
}

fn group_sha(head: char) -> String {
    head.to_string().repeat(40)
}

fn merge_group(head: char, pr: i64) -> NewMergeGroup {
    NewMergeGroup {
        head_sha: group_sha(head),
        head_ref: format!("gh-readonly-queue/main/pr-{pr}-{}", "e".repeat(40)),
        base_ref: "main".into(),
        base_sha: "b".repeat(40),
        pr_number: pr,
        pr_head_sha: "e".repeat(40),
        context: "agentscm/workflow/main".into(),
    }
}

fn merge_group_check(head: char, state: &str) -> NewForgeCheck {
    NewForgeCheck {
        head_sha: group_sha(head),
        state: state.into(),
        binding_digest: (state == "success").then(|| "d".repeat(64)),
        description: "qualified".into(),
    }
}

/// The merge queue group journal: identities stick, an ended group never revives or reserves,
/// invalid input is refused, and the check compare-and-set matches the pull request journal.
fn merge_group_journal(db: &Db) {
    let (store, repo) = merge_group_store(db);
    let a = group_sha('a');

    let first = store
        .observe_merge_group(&repo, &merge_group('a', 1), None, 10)
        .unwrap();
    assert_eq!(
        (first.head_sha.as_str(), first.observed_at, first.generation),
        (a.as_str(), 10, 0)
    );
    assert!(first.check.is_none() && first.ended_reason.is_none());
    assert_eq!(store.merge_group(&repo, &a).unwrap(), Some(first.clone()));
    assert_eq!(store.merge_group(&repo, &group_sha('f')).unwrap(), None);
    assert!(store.merge_group(&repo, "short").is_err());
    // A repeat with other identities changes nothing.
    let other = NewMergeGroup {
        pr_number: 9,
        base_ref: "dev".into(),
        ..merge_group('a', 1)
    };
    assert_eq!(
        store.observe_merge_group(&repo, &other, None, 99).unwrap(),
        first
    );
    // Invalid input is refused and records nothing.
    let invalid = NewMergeGroup {
        head_sha: "short".into(),
        ..merge_group('d', 1)
    };
    assert!(store.observe_merge_group(&repo, &invalid, None, 1).is_err());
    assert!(
        store
            .observe_merge_group(&repo, &merge_group('d', 1), Some(""), 1)
            .is_err()
    );
    assert!(
        store
            .observe_merge_group("rep_unknown", &merge_group('d', 1), None, 1)
            .is_err()
    );
    assert_eq!(store.merge_group(&repo, &group_sha('d')).unwrap(), None);

    // The check journal: compare-and-set on the generation, then completion.
    let pending = merge_group_check('a', "pending");
    assert_eq!(
        store
            .reserve_merge_group_check(&repo, &a, 1, &pending, 12)
            .unwrap(),
        ForgeCheckReserve::Conflict
    );
    assert!(
        store
            .reserve_merge_group_check(&repo, &a, 0, &merge_group_check('c', "pending"), 12)
            .is_err()
    );
    assert_eq!(store.merge_group(&repo, &a).unwrap().unwrap().check, None);
    let ForgeCheckReserve::Reserved(generation) = store
        .reserve_merge_group_check(&repo, &a, 0, &merge_group_check('a', "success"), 12)
        .unwrap()
    else {
        panic!("reserve");
    };
    assert_eq!(generation, 1);
    assert_eq!(
        store.merge_group(&repo, &a).unwrap().unwrap().check,
        Some(ForgeCheck {
            head_sha: a.clone(),
            state: "success".into(),
            binding_digest: Some("d".repeat(64)),
            description: "qualified".into(),
            phase: "attempting".into(),
            generation,
            updated_at: 12,
        })
    );
    assert!(
        !store
            .complete_merge_group_check(&repo, &a, generation - 1, "posted", 13)
            .unwrap()
    );
    assert!(
        store
            .complete_merge_group_check(&repo, &a, generation, "bogus", 13)
            .is_err()
    );
    assert!(
        store
            .complete_merge_group_check(&repo, &a, generation, "posted", 13)
            .unwrap()
    );
    let posted = store.merge_group(&repo, &a).unwrap().unwrap();
    let shown = posted.check.clone().unwrap();
    assert_eq!(
        (shown.state.as_str(), shown.phase.as_str(), shown.updated_at),
        ("success", "posted", 13)
    );
    assert_eq!(posted.generation, generation + 1);

    // What the Hub saw of the ruleset and the status author; bad text is refused.
    assert!(
        store
            .note_merge_group_source(&repo, &a, "x\ny", "hub")
            .is_err()
    );
    store
        .note_merge_group_source(&repo, &a, "required_from_this_app", "hub")
        .unwrap();

    // Ending: true once, false for a repeat or an unknown group, an empty reason is refused.
    assert!(store.end_merge_group(&repo, &a, "", 14).is_err());
    assert!(
        !store
            .end_merge_group(&repo, &group_sha('f'), "merged", 14)
            .unwrap()
    );
    assert!(
        store
            .end_merge_group(&repo, &a, "merge group was rebuilt", 14)
            .unwrap()
    );
    assert!(!store.end_merge_group(&repo, &a, "merged", 15).unwrap());
    // An ended group refuses a reservation and never revives or changes its reason.
    assert_eq!(
        store
            .reserve_merge_group_check(
                &repo,
                &a,
                posted.generation,
                &merge_group_check('a', "failure"),
                16
            )
            .unwrap(),
        ForgeCheckReserve::Conflict
    );
    for (reason, at) in [(None, 17), (Some("merged"), 18)] {
        let ended = store
            .observe_merge_group(&repo, &merge_group('a', 1), reason, at)
            .unwrap();
        assert_eq!(
            (ended.ended_reason.as_deref(), ended.ended_at),
            (Some("merge group was rebuilt"), Some(14))
        );
        assert_eq!(
            (
                ended.ruleset_requirement.as_deref(),
                ended.status_source.as_deref()
            ),
            (Some("required_from_this_app"), Some("hub"))
        );
    }

    // A reply that arrives after the group ended is still recorded.
    let b = group_sha('b');
    store
        .observe_merge_group(&repo, &merge_group('b', 2), None, 20)
        .unwrap();
    let ForgeCheckReserve::Reserved(generation) = store
        .reserve_merge_group_check(&repo, &b, 0, &merge_group_check('b', "success"), 21)
        .unwrap()
    else {
        panic!("reserve");
    };
    assert!(store.end_merge_group(&repo, &b, "merged", 22).unwrap());
    assert!(
        store
            .complete_merge_group_check(&repo, &b, generation, "posted", 23)
            .unwrap()
    );
    let late = store.merge_group(&repo, &b).unwrap().unwrap();
    assert_eq!(late.check.unwrap().phase, "posted");
    assert_eq!(late.generation, generation + 1);

    // A destroy seen first is recorded ended and stays ended.
    let first = store
        .observe_merge_group(&repo, &merge_group('f', 3), Some("dequeued"), 30)
        .unwrap();
    assert_eq!(
        (
            first.ended_reason.as_deref(),
            first.ended_at,
            first.observed_at
        ),
        (Some("dequeued"), Some(30), 30)
    );
    let later = store
        .observe_merge_group(&repo, &merge_group('f', 3), None, 31)
        .unwrap();
    assert_eq!(later, first);
}

/// Listing groups: current ones oldest first with a limit, the groups of one pull request
/// newest first (ended ones included), and ended groups kept for 30 days.
fn merge_group_listings(db: &Db) {
    let (store, repo) = merge_group_store(db);
    // Pull request 1 has five groups (d and e observed at the same time); pull request 2 has one.
    for (head, pr, at) in [
        ('c', 1, 30),
        ('a', 1, 10),
        ('b', 1, 20),
        ('e', 1, 40),
        ('d', 1, 40),
        ('f', 2, 50),
    ] {
        store
            .observe_merge_group(&repo, &merge_group(head, pr), None, at)
            .unwrap();
    }
    assert!(
        store
            .end_merge_group(&repo, &group_sha('b'), "merged", 41)
            .unwrap()
    );
    let heads =
        |groups: Vec<MergeGroup>| groups.into_iter().map(|g| g.head_sha).collect::<Vec<_>>();
    let current = |limit| heads(store.active_merge_groups(&repo, limit).unwrap());
    let sha = |heads: &[char]| heads.iter().map(|c| group_sha(*c)).collect::<Vec<_>>();
    assert_eq!(current(10), sha(&['a', 'c', 'd', 'e', 'f']));
    assert_eq!(current(2), sha(&['a', 'c']));
    assert!(
        store
            .active_merge_groups("rep_unknown", 10)
            .unwrap()
            .is_empty()
    );

    let for_pull = |pr, limit| heads(store.merge_groups_for_pull(&repo, pr, limit).unwrap());
    // Newest first, the ended group among them, ties by head.
    assert_eq!(for_pull(1, 10), sha(&['d', 'e', 'c', 'b', 'a']));
    assert_eq!(for_pull(1, 3), sha(&['d', 'e', 'c']));
    assert_eq!(for_pull(2, 10), sha(&['f']));
    assert!(for_pull(3, 10).is_empty());
    assert!(
        store
            .merge_groups_for_pull("rep_unknown", 1, 10)
            .unwrap()
            .is_empty()
    );

    // Ended groups go 30 days after they end, on a later observation of the repository.
    const DAY: i64 = 86_400;
    let now = 100 * DAY;
    for (head, pr, at, ended) in [
        ('1', 7, now - 31 * DAY, Some("merged")),
        ('2', 7, now - 29 * DAY, Some("merged")),
        ('3', 7, now - 40 * DAY, None),
    ] {
        store
            .observe_merge_group(&repo, &merge_group(head, pr), ended, at)
            .unwrap();
    }
    store
        .observe_merge_group(&repo, &merge_group('4', 7), None, now)
        .unwrap();
    assert_eq!(store.merge_group(&repo, &group_sha('1')).unwrap(), None);
    // Within 30 days, and a group that has not ended, stay.
    assert!(store.merge_group(&repo, &group_sha('2')).unwrap().is_some());
    assert!(store.merge_group(&repo, &group_sha('3')).unwrap().is_some());
    assert_eq!(for_pull(7, 10), sha(&['4', '2', '3']));
}

fn forge_check_journal(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
    let ctx = "agentscm/workflow/main";
    let check = |state: &str, sha: char| NewForgeCheck {
        head_sha: sha.to_string().repeat(40),
        state: state.into(),
        binding_digest: (state == "success").then(|| "d".repeat(64)),
        description: "qualified".into(),
    };
    assert_eq!(store.forge_check(&repo.id, 5, ctx).unwrap(), None);

    // A stale or wrong expectation never creates or overwrites a row.
    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 5, ctx, Some(1), &check("pending", 'a'), 10)
            .unwrap(),
        ForgeCheckReserve::Conflict
    );
    assert_eq!(store.forge_check(&repo.id, 5, ctx).unwrap(), None);

    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 5, ctx, None, &check("pending", 'a'), 10)
            .unwrap(),
        ForgeCheckReserve::Reserved(1)
    );
    let row = store.forge_check(&repo.id, 5, ctx).unwrap().unwrap();
    assert_eq!(
        (
            row.state.as_str(),
            row.phase.as_str(),
            row.generation,
            row.updated_at
        ),
        ("pending", "attempting", 1, 10)
    );
    // A second absent-row writer loses.
    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 5, ctx, None, &check("failure", 'b'), 11)
            .unwrap(),
        ForgeCheckReserve::Conflict
    );
    assert!(
        store
            .complete_forge_check(&repo.id, 5, ctx, 1, "posted", 12)
            .unwrap()
    );
    let row = store.forge_check(&repo.id, 5, ctx).unwrap().unwrap();
    assert_eq!((row.phase.as_str(), row.generation), ("posted", 2));
    // Completing again with the old generation is refused.
    assert!(
        !store
            .complete_forge_check(&repo.id, 5, ctx, 1, "posted", 13)
            .unwrap()
    );

    // Two evaluations that read generation 2: one wins, the other must re-read.
    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 5, ctx, Some(2), &check("success", 'c'), 14)
            .unwrap(),
        ForgeCheckReserve::Reserved(3)
    );
    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 5, ctx, Some(2), &check("error", 'e'), 15)
            .unwrap(),
        ForgeCheckReserve::Conflict
    );
    // The completion of a superseded reservation is refused.
    assert!(
        !store
            .complete_forge_check(&repo.id, 5, ctx, 2, "posted", 16)
            .unwrap()
    );
    let row = store.forge_check(&repo.id, 5, ctx).unwrap().unwrap();
    assert_eq!(
        (
            row.head_sha,
            row.state,
            row.binding_digest,
            row.phase,
            row.generation
        ),
        (
            "c".repeat(40),
            "success".into(),
            Some("d".repeat(64)),
            "attempting".into(),
            3
        )
    );
    // Only a final phase can be recorded; a definite refusal is its own phase.
    assert!(
        store
            .complete_forge_check(&repo.id, 5, ctx, 3, "attempting", 17)
            .is_err()
    );
    assert!(
        store
            .complete_forge_check(&repo.id, 5, ctx, 3, "refused", 17)
            .unwrap()
    );
    let row = store.forge_check(&repo.id, 5, ctx).unwrap().unwrap();
    assert_eq!((row.phase.as_str(), row.generation), ("refused", 4));
    // Other contexts and pull requests are independent rows.
    assert_eq!(store.forge_check(&repo.id, 6, ctx).unwrap(), None);
    assert_eq!(
        store
            .forge_check(&repo.id, 5, "agentscm/workflow/dev")
            .unwrap(),
        None
    );

    // Rows that may show a success are listed by number then context, bounded.
    let dev = "agentscm/workflow/dev";
    for (number, context, state) in [
        (3, ctx, "pending"),
        (3, dev, "failure"),
        (9, ctx, "failure"),
    ] {
        store
            .reserve_forge_check(&repo.id, number, context, None, &check(state, 'a'), 20)
            .unwrap();
        if context == dev || number == 9 {
            store
                .complete_forge_check(&repo.id, number, context, 1, "posted", 21)
                .unwrap();
        }
    }
    let listed = store
        .forge_checks_showing_success(&repo.id, "acme/widgets", &[], 10)
        .unwrap();
    // 3/main is uncertain, 5/main says success; 3/dev and 9/main are settled failures.
    assert_eq!(
        listed
            .iter()
            .map(|(n, c, _)| (*n, c.as_str()))
            .collect::<Vec<_>>(),
        [(3, ctx), (5, ctx)]
    );
    assert_eq!(listed[1].2.state, "success");
    // An enabled context is not a retire candidate.
    assert!(
        store
            .forge_checks_showing_success(&repo.id, "acme/widgets", &[ctx.to_owned()], 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .forge_checks_showing_success(&repo.id, "acme/widgets", &[], 1)
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .forge_checks_showing_success("other", "acme/widgets", &[], 10)
            .unwrap()
            .is_empty()
    );

    // Validation happens on write.
    let bad = |new: NewForgeCheck| {
        store
            .reserve_forge_check(&repo.id, 7, ctx, None, &new, 1)
            .is_err()
    };
    assert!(bad(NewForgeCheck {
        head_sha: "A".repeat(40),
        ..check("pending", 'a')
    }));
    assert!(bad(NewForgeCheck {
        head_sha: "a".repeat(39),
        ..check("pending", 'a')
    }));
    assert!(bad(NewForgeCheck {
        state: "ok".into(),
        ..check("pending", 'a')
    }));
    assert!(bad(NewForgeCheck {
        binding_digest: None,
        ..check("success", 'a')
    }));
    assert!(bad(NewForgeCheck {
        binding_digest: Some("d".repeat(64)),
        ..check("pending", 'a')
    }));
    assert!(bad(NewForgeCheck {
        binding_digest: Some("D".repeat(64)),
        ..check("success", 'a')
    }));
    assert!(bad(NewForgeCheck {
        description: "x".repeat(141),
        ..check("pending", 'a')
    }));
    assert!(bad(NewForgeCheck {
        description: "a\nb".into(),
        ..check("pending", 'a')
    }));
    assert!(
        store
            .reserve_forge_check(
                &repo.id,
                7,
                &"c".repeat(121),
                None,
                &check("pending", 'a'),
                1
            )
            .is_err()
    );
    assert!(
        store
            .reserve_forge_check(&repo.id, 7, "bad context", None, &check("pending", 'a'), 1)
            .is_err()
    );
    assert!(
        store
            .reserve_forge_check(&repo.id, 0, ctx, None, &check("pending", 'a'), 1)
            .is_err()
    );
    assert!(
        store
            .reserve_forge_check("rep_unknown", 7, ctx, None, &check("pending", 'a'), 1)
            .is_err()
    );
    assert_eq!(store.forge_check(&repo.id, 7, ctx).unwrap(), None);
    let max = NewForgeCheck {
        description: "x".repeat(140),
        ..check("pending", 'a')
    };
    assert_eq!(
        store
            .reserve_forge_check(&repo.id, 7, &"c".repeat(120), None, &max, 1)
            .unwrap(),
        ForgeCheckReserve::Reserved(1)
    );
}

fn upgrade_from_migration_3(db: &Db) {
    let (one, two, three) = if db.pg.is_some() {
        (
            include_str!("../../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../../src/hub/migrations/0003_intents.pg.sql"),
        )
    } else {
        (
            include_str!("../../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../../src/hub/migrations/0003_intents.sqlite.sql"),
        )
    };
    db.exec(&format!(
        "{one} {two} {three}
         CREATE TABLE hub_schema_migrations (version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL);
         INSERT INTO hub_schema_migrations (version, applied_at) VALUES (1, 0), (2, 0), (3, 0);
         INSERT INTO hub_users (id, provider, host, forge_user_id, login, created_at)
             VALUES ('usr_legacy', 'github', 'github.com', 7, 'alice', 0);
         INSERT INTO hub_agent_tokens (id, owner, name, repository, digest, expires_at, created_at, repository_id)
             VALUES ('tok_legacy', 'usr_legacy', 'builder', 'acme/widgets', 'd', 4102444800, 0, 4242);
         INSERT INTO hub_intents (id, repository, token, task, paths, expires_at, created_at, updated_at)
             VALUES ('int_legacy', 'acme/widgets', 'tok_legacy', 't', '[\"a\"]', 4102444800, 0, 0);"
    ))
    .unwrap();
    let store = db.open();
    assert_eq!(db.versions(), store::integer_migration_versions());
    let scope = crate::repository_scope::github("acme/widgets", Some(4242));
    let intents = store.live_intents(&scope, unix_now(), 10).unwrap();
    assert_eq!(intents.len(), 1);
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
}

fn url_errors(_: &Db) {
    for url in [
        "mysql://u:s3cret@h/db",
        "sqlite://relative",
        "sqlite://",
        "sqlite:///",
        "sqlite:///tmp/s3cret/",
        "sqlite:///tmp/s3cret?mode=ro",
        "sqlite:///tmp/s3cret#frag",
        "sqlite:///tmp/s3cret%20x",
        "s3cret",
        "host=h password=pw://x",
        "postgres://u:pw@h:notaport/db",
        "postgres://u:pw@db.example.com/db",
    ] {
        let message = store::open(url).err().unwrap().to_string();
        for secret in ["s3cret", "pw", "mysql", "relative"] {
            assert!(!message.contains(secret), "{url}: {message}");
        }
    }
    let message = store::open("mysql://u:s3cret@h/db")
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains("must start with sqlite:// or postgres://"),
        "{message}"
    );
}

fn upgrade_from_migration_14_with_publication(db: &Db) {
    let migrations = if db.pg.is_some() {
        [
            include_str!("../../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../../src/hub/migrations/0003_intents.pg.sql"),
            include_str!("../../src/hub/migrations/0004_github.pg.sql"),
            include_str!("../../src/hub/migrations/0005_publish.pg.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.pg.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.pg.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.pg.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.pg.sql"),
            include_str!("../../src/hub/migrations/0010_work.pg.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.pg.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.pg.sql"),
            include_str!("../../src/hub/migrations/0013_coordination_requests.pg.sql"),
            include_str!("../../src/hub/migrations/0014_coordination_retention.pg.sql"),
        ]
    } else {
        [
            include_str!("../../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../../src/hub/migrations/0003_intents.sqlite.sql"),
            include_str!("../../src/hub/migrations/0004_github.sqlite.sql"),
            include_str!("../../src/hub/migrations/0005_publish.sqlite.sql"),
            include_str!("../../src/hub/migrations/0006_sessions.sqlite.sql"),
            include_str!("../../src/hub/migrations/0007_warn_once.sqlite.sql"),
            include_str!("../../src/hub/migrations/0008_read_only_tokens.sqlite.sql"),
            include_str!("../../src/hub/migrations/0009_handoffs.sqlite.sql"),
            include_str!("../../src/hub/migrations/0010_work.sqlite.sql"),
            include_str!("../../src/hub/migrations/0011_work_subscribers.sqlite.sql"),
            include_str!("../../src/hub/migrations/0012_work_subscriber_sessions.sqlite.sql"),
            include_str!("../../src/hub/migrations/0013_coordination_requests.sqlite.sql"),
            include_str!("../../src/hub/migrations/0014_coordination_retention.sqlite.sql"),
        ]
    };
    for sql in migrations {
        db.exec(sql).unwrap();
    }
    db.exec("CREATE TABLE hub_schema_migrations (version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL)").unwrap();
    for version in 1..=14 {
        db.exec(&format!(
            "INSERT INTO hub_schema_migrations VALUES ({version},0)"
        ))
        .unwrap();
    }
    let manifest: agentscm::hub::publish::Manifest = serde_json::from_value(json!({
        "revision":"a".repeat(64), "event_seq":3, "commit":"b".repeat(40), "tree":"c".repeat(40), "parent":"d".repeat(40),
        "branch":"feat/legacy", "base":"main", "title":"Legacy integrated publication", "body":"", "message":"snapshot",
        "author":{"name":"A","email":"a@example.invalid","date":"2026-05-28T20:26:40Z"},
        "committer":{"name":"A","email":"a@example.invalid","date":"2026-05-28T20:26:40Z"},
        "changes":[{"path":"removed","mode":"000000","blob":null,"sha256":null,"size":0}]
    })).unwrap();
    let id = agentscm::hub::publish::derive_id(REPO, 3, &manifest.commit, &manifest.branch);
    let canonical = manifest.canonical();
    agentscm::hub::publish::NewPublication {
        id: id.clone(),
        manifest: manifest.clone(),
    }
    .validate(REPO)
    .unwrap();
    db.exec(&format!("INSERT INTO hub_users (id,provider,host,forge_user_id,login,created_at) VALUES ('usr_legacy','github','github.com',7,'alice',0);
        INSERT INTO hub_agent_tokens (id,owner,name,repository,digest,expires_at,created_at) VALUES ('tok_legacy','usr_legacy','builder','acme/widgets','legacy',4102444800,0);
        INSERT INTO hub_publications (id,repository,token,revision,event_seq,commit_sha,branch,base,manifest,branch_state,pr_number,pr_node_id,pr_url,created_at,updated_at) VALUES ('{id}','acme/widgets','tok_legacy','{}',3,'{}','feat/legacy','main','{}','created',7,'PR_7','https://github.com/acme/widgets/pull/7',10,11);
        INSERT INTO hub_events (id,at,kind,subject,data) VALUES ('evt_legacy',10,'publication_pr_opened','{id}','{{\"number\":7}}')", manifest.revision,manifest.commit,canonical.replace('\'', "''"))).unwrap();
    let store = db.open();
    assert_eq!(db.versions(), store::integer_migration_versions());
    let p = store
        .publication(&crate::repository_scope::github(REPO, None), &id)
        .unwrap()
        .unwrap();
    assert_eq!(p.event_seq, Some(3));
    assert_eq!(p.manifest, canonical);
    assert_eq!((p.pr_number, p.created_at, p.updated_at), (Some(7), 10, 11));
    assert_eq!(store.events_after(0, 10).unwrap()[0].data, "{\"number\":7}");
    let index_exists = if db.pg.is_some() {
        postgres::Client::connect(&db.url, postgres::NoTls).unwrap().query_one("SELECT count(*) FROM pg_indexes WHERE schemaname=current_schema() AND indexname='hub_publications_repository'", &[]).unwrap().get::<_,i64>(0)
    } else {
        rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap()).unwrap().query_row("SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='hub_publications_repository'", [], |r| r.get::<_,i64>(0)).unwrap()
    };
    assert_eq!(index_exists, 1);
    // Corrupt retained data must return an error on both backends, never panic.
    let oversized = "x".repeat(2 * 1024 * 1024 + 1);
    if db.pg.is_some() {
        postgres::Client::connect(&db.url, postgres::NoTls)
            .unwrap()
            .execute(
                "UPDATE hub_publications SET manifest=$1 WHERE id=$2",
                &[&oversized, &id],
            )
            .unwrap();
    } else {
        rusqlite::Connection::open(db.url.strip_prefix("sqlite://").unwrap())
            .unwrap()
            .execute(
                "UPDATE hub_publications SET manifest=?1 WHERE id=?2",
                [&oversized, &id],
            )
            .unwrap();
    }
    assert!(matches!(
        store.publication(REPO, &id),
        Err(agentscm::hub::HubError::Invalid(message))
            if message == "retained publication exceeds manifest budget"
    ));
}
fn pull_publications_are_newest_per_pull_request(db: &Db) {
    let store = db.open();
    let row = |id: &str, repository: &str, pr: &str, merged: &str, created: i64| {
        format!(
            "INSERT INTO hub_publications (id,repository,token,revision,event_seq,commit_sha,branch,
                base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at,
                repository_scope)
             VALUES ('{id}','{repository}','tok_pp','{}',1,'{}','b-{id}','main','{{}}','created',{pr},
                {merged},{created},{created},'scope:{}');",
            "a".repeat(64),
            "b".repeat(40),
            repository.to_ascii_lowercase()
        )
    };
    let mut sql = String::from(
        "INSERT INTO hub_users (id,provider,host,forge_user_id,login,created_at)
            VALUES ('usr_pp','github','github.com',9,'alice',0);
         INSERT INTO hub_agent_tokens (id,owner,name,repository,digest,expires_at,created_at)
            VALUES ('tok_pp','usr_pp','builder','Acme/Widgets','pp',4102444800,0);",
    );
    for (id, repository, pr, merged, created) in [
        ("p1-old", "Acme/Widgets", "1", "NULL", 1),
        ("p1-new", "Acme/Widgets", "1", "NULL", 2),
        ("p2", "Acme/Widgets", "2", "NULL", 3),
        ("p3-old", "Acme/Widgets", "3", "NULL", 4),
        ("p3-merged", "Acme/Widgets", "3", "'c'", 5),
        ("p4", "Acme/Widgets", "4", "NULL", 6),
        ("other", "acme/other", "9", "NULL", 7),
        ("nopr", "Acme/Widgets", "NULL", "NULL", 8),
    ] {
        sql.push_str(&row(id, repository, pr, merged, created));
    }
    db.exec(&sql).unwrap();
    let ids = |after: i64, limit: usize| {
        store
            .pull_publications("scope:acme/widgets", after, limit)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
    };
    // Newest per number, the merged lineage omitted, only rows of the repository's scope.
    assert_eq!(ids(0, 50), ["p1-new", "p2", "p4"]);
    assert_eq!(ids(1, 50), ["p2", "p4"]);
    assert_eq!(ids(0, 2), ["p1-new", "p2"]);
    assert_eq!(ids(4, 50), Vec::<String>::new());
}

fn external_run_journal(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
    let sha = "a".repeat(40);
    let ci = |run_id: i64, attempt: i64, outcome: ExternalOutcome, at: i64| ExternalRun {
        provider: "github".into(),
        kind: ExternalKind::Ci,
        source: ExternalSource::Forge,
        run_id,
        attempt,
        commit_sha: sha.clone(),
        branch: Some("main".into()),
        environment: None,
        name: ".github/workflows/ci.yml".into(),
        outcome,
        provider_updated_at: at,
        html_url: Some(format!(
            "https://github.com/acme/widgets/actions/runs/{run_id}"
        )),
    };
    let deploy = |run_id: i64, environment: &str| ExternalRun {
        kind: ExternalKind::Deployment,
        branch: None,
        environment: Some(environment.into()),
        ..ci(run_id, 1, ExternalOutcome::Succeeded, 5)
    };
    let observe = |run: &ExternalRun| store.observe_external_run(&repo.id, run).unwrap();
    let events = || events_of(&*store, "external_run_observed").len();

    let running = ci(7, 1, ExternalOutcome::Running, 10);
    assert_eq!(observe(&running), ExternalObserved::Changed);
    assert_eq!(events(), 1);
    assert_eq!(observe(&running), ExternalObserved::Unchanged);
    assert_eq!(events(), 1);

    // An older provider update is ignored; a newer one replaces.
    assert_eq!(
        observe(&ci(7, 1, ExternalOutcome::Pending, 9)),
        ExternalObserved::Stale
    );
    let done = ci(7, 1, ExternalOutcome::Succeeded, 11);
    assert_eq!(observe(&done), ExternalObserved::Changed);
    assert_eq!(events(), 2);

    // A terminal outcome is not regressed, even by a later non-terminal read.
    assert_eq!(
        observe(&ci(7, 1, ExternalOutcome::Running, 12)),
        ExternalObserved::Stale
    );
    assert_eq!(events(), 2);

    // A higher attempt is a separate row.
    let retry = ci(7, 2, ExternalOutcome::Running, 13);
    assert_eq!(observe(&retry), ExternalObserved::Changed);
    assert_eq!(observe(&deploy(9, "production")), ExternalObserved::Changed);
    assert_eq!(observe(&deploy(8, "production")), ExternalObserved::Changed);
    assert_eq!(observe(&deploy(10, "dev")), ExternalObserved::Changed);
    let mut other = ci(3, 1, ExternalOutcome::Failed, 1);
    other.commit_sha = "b".repeat(40);
    assert_eq!(observe(&other), ExternalObserved::Changed);

    // Each kind is listed apart: environment, then run id, then attempt.
    let of = |kind, limit| store.external_runs(&repo.id, &sha, kind, limit).unwrap();
    assert_eq!(of(ExternalKind::Ci, 50), [done, retry]);
    assert_eq!(
        of(ExternalKind::Deployment, 50),
        [
            deploy(10, "dev"),
            deploy(8, "production"),
            deploy(9, "production")
        ]
    );
    assert_eq!(of(ExternalKind::Deployment, 2).len(), 2);
    // The source is part of the key: an id another source already uses is a separate row.
    // GitLab rows are only the Hub's own; a CI run is only ever read from the forge.
    let mut workflow = deploy(9, "production");
    workflow.source = ExternalSource::Workflow;
    assert_eq!(observe(&workflow), ExternalObserved::Changed);
    let mut recorded = deploy(1, "production");
    (recorded.provider, recorded.source) = ("gitlab".into(), ExternalSource::Recorded);
    assert_eq!(observe(&recorded), ExternalObserved::Changed);
    assert_eq!(of(ExternalKind::Deployment, 50).len(), 5);
    assert!(of(ExternalKind::Deployment, 50).contains(&workflow));
    let mut gitlab_read = recorded.clone();
    gitlab_read.source = ExternalSource::Forge;
    assert!(store.observe_external_run(&repo.id, &gitlab_read).is_err());
    let mut agent_ci = ci(1, 1, ExternalOutcome::Running, 1);
    agent_ci.source = ExternalSource::Agent;
    assert!(store.observe_external_run(&repo.id, &agent_ci).is_err());
    assert_eq!(
        store
            .external_runs(&repo.id, &"c".repeat(40), ExternalKind::Ci, 50)
            .unwrap(),
        []
    );

    // The event carries identifiers and the outcome, not provider text.
    let first = &events_of(&*store, "external_run_observed")[0];
    assert_eq!(first["outcome"], "running");
    assert_eq!(first["run_id"], 7);
    assert!(first.get("name").is_none() && first.get("html_url").is_none());

    // Invalid runs and unknown repositories are refused.
    let mut bad = ci(1, 1, ExternalOutcome::Running, 1);
    bad.html_url = Some("http://example.com".into());
    assert!(store.observe_external_run(&repo.id, &bad).is_err());
    assert!(
        store
            .observe_external_run("rep_missing", &ci(1, 1, ExternalOutcome::Running, 1))
            .is_err()
    );

    assert_eq!(store.external_reconciled(&repo.id, &sha).unwrap(), None);
    store
        .mark_external_reconciled(&repo.id, &sha, 100, false)
        .unwrap();
    assert_eq!(
        store.external_reconciled(&repo.id, &sha).unwrap(),
        Some((100, false))
    );
    store
        .mark_external_reconciled(&repo.id, &sha, 200, true)
        .unwrap();
    assert_eq!(
        store.external_reconciled(&repo.id, &sha).unwrap(),
        Some((200, true))
    );
}

fn merged_publications_skip_unmerged_ones(db: &Db) {
    let store = db.open();
    let mut sql = String::from(
        "INSERT INTO hub_users (id,provider,host,forge_user_id,login,created_at)
            VALUES ('usr_mp','github','github.com',9,'alice',0);
         INSERT INTO hub_agent_tokens (id,owner,name,repository,digest,expires_at,created_at)
            VALUES ('tok_mp','usr_mp','builder','Acme/Widgets','mp',4102444800,0);",
    );
    for (id, repository, merged) in [
        ("m2", "Acme/Widgets", "'c'"),
        ("m1", "Acme/Widgets", "'d'"),
        ("open", "Acme/Widgets", "NULL"),
        ("other", "acme/other", "'e'"),
        ("stale", "Acme/Widgets", "'f'"),
    ] {
        let updated = if id == "stale" { 0 } else { 5 };
        sql.push_str(&format!(
            "INSERT INTO hub_publications (id,repository,token,revision,event_seq,commit_sha,
                branch,base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at,
                repository_scope)
             VALUES ('{id}','{repository}','tok_mp','{}',1,'{}','b-{id}','main','{{}}','created',
                1,{merged},1,{updated},'scope:{}');",
            "a".repeat(64),
            "b".repeat(40),
            repository.to_ascii_lowercase()
        ));
    }
    db.exec(&sql).unwrap();
    let ids = |after: &str, limit: usize| {
        store
            .merged_publications("scope:acme/widgets", after, 1, limit)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
    };
    // The stale one is outside the window, so it cannot fill a page.
    assert_eq!(ids("", 50), ["m1", "m2"]);
    assert_eq!(
        store
            .merged_publications("scope:acme/widgets", "", 0, 50)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(ids("m1", 50), ["m2"]);
    assert_eq!(ids("", 1), ["m1"]);
    assert_eq!(ids("m2", 50), Vec::<String>::new());
}

fn forge_checks_showing_success_leave_out_merged_pull_requests(db: &Db) {
    let store = db.open();
    let con = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap();
    let mut sql = String::from(
        "INSERT INTO hub_users (id,provider,host,forge_user_id,login,created_at)
            VALUES ('usr_fm','github','github.com',9,'alice',0);
         INSERT INTO hub_agent_tokens (id,owner,name,repository,digest,expires_at,created_at)
            VALUES ('tok_fm','usr_fm','builder','Acme/Widgets','fm',4102444800,0);",
    );
    for (id, pr, merged, created) in [
        ("m1-old", 11, "NULL", 1),
        ("m1-new", 11, "'c'", 2),
        ("m2-old", 12, "'c'", 3),
        ("m2-new", 12, "NULL", 4),
        ("m3", 13, "NULL", 5),
    ] {
        sql.push_str(&format!(
            "INSERT INTO hub_publications (id,repository,token,revision,event_seq,commit_sha,branch,
                base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at,
                repository_scope)
             VALUES ('{id}','Acme/Widgets','tok_fm','{}',1,'{}','b-{id}','main','{{}}','created',{pr},
                {merged},{created},{created},'scope:acme/widgets');",
            "a".repeat(64),
            "b".repeat(40)
        ));
    }
    db.exec(&sql).unwrap();
    let new = NewForgeCheck {
        head_sha: "a".repeat(40),
        state: "success".into(),
        binding_digest: Some("d".repeat(64)),
        description: "qualified".into(),
    };
    for number in [11, 12, 13] {
        store
            .reserve_forge_check(&repo.id, number, "agentscm/workflow/main", None, &new, 20)
            .unwrap();
    }
    let numbers = store
        .forge_checks_showing_success(&repo.id, "scope:acme/widgets", &[], 10)
        .unwrap()
        .into_iter()
        .map(|(n, _, _)| n)
        .collect::<Vec<_>>();
    // 11: newest publication merged, excluded. 12: older merged, newer not, included.
    assert_eq!(numbers, [12, 13]);
}

fn skill_adoption_and_evidence(db: &Db) {
    let store = db.open();
    let adoption = |source: &str, lock: &str| SkillAdoption {
        repository: REPO.into(),
        repository_scope: REPO.into(),
        source: source.into(),
        lock_json: lock.into(),
        default_branch: (source == "default_branch").then(|| "main".to_owned()),
        observed_at: 10,
        reported_by: None,
        // Only the forge's default-branch read stores repository skills.
        local_skills: (source == "default_branch").then(|| "[{\"local\":1}]".to_owned()),
    };
    assert_eq!(store.skill_adoption(REPO).unwrap(), None);
    let checkout = store
        .skill_adoption_put(&adoption("reported_by_checkout", "{\"a\":1}"))
        .unwrap();
    assert_eq!(checkout.lock_json, "{\"a\":1}");
    // A checkout report stores no repository skills.
    assert_eq!(checkout.local_skills, None);
    // The forge replaces a checkout report; a checkout report never replaces the forge.
    let forge = store
        .skill_adoption_put(&adoption("default_branch", "{\"b\":2}"))
        .unwrap();
    assert_eq!(
        (forge.source.as_str(), forge.default_branch.as_deref()),
        ("default_branch", Some("main"))
    );
    // Repository skills are stored and returned with the forge row.
    assert_eq!(forge.local_skills.as_deref(), Some("[{\"local\":1}]"));
    let kept = store
        .skill_adoption_put(&adoption("reported_by_checkout", "{\"c\":3}"))
        .unwrap();
    assert_eq!(kept, forge);
    // Clearing one source leaves another; clearing every source drops a checkout report too.
    store
        .skill_adoption_clear(REPO, Some("reported_by_checkout"))
        .unwrap();
    assert_eq!(store.skill_adoption(REPO).unwrap(), Some(forge));
    store.skill_adoption_clear(REPO, None).unwrap();
    assert_eq!(store.skill_adoption(REPO).unwrap(), None);
    store
        .skill_adoption_put(&adoption("reported_by_checkout", "{\"d\":4}"))
        .unwrap();
    store
        .skill_adoption_clear(REPO, Some("reported_by_checkout"))
        .unwrap();
    assert_eq!(store.skill_adoption(REPO).unwrap(), None);
    store
        .skill_adoption_put(&adoption("reported_by_checkout", "{\"e\":5}"))
        .unwrap();
    store.skill_adoption_clear(REPO, None).unwrap();
    assert_eq!(store.skill_adoption(REPO).unwrap(), None);

    let row = |id: &str, run: &str, branch: &str, at: i64| EvidenceRow {
        id: id.into(),
        repository: REPO.into(),
        repository_scope: REPO.into(),
        branch: branch.into(),
        run_id: run.into(),
        skill: "pr-gate".into(),
        skill_sha256: "0".repeat(64),
        gate: "tests".into(),
        kind: "revision_tests".into(),
        head_sha: "a".repeat(40),
        assurance: "self_reported".into(),
        summary: "ok".into(),
        url: None,
        pull_request: Some(4),
        detail: Some("{\"exit_code\":0}".into()),
        token_id: "tok_a".into(),
        session: "s".into(),
        agent: "claude".into(),
        recorded_at: at,
    };
    store
        .skill_evidence_add(&row("evd_1", "run_a", "feat/a", 1))
        .unwrap();
    store
        .skill_evidence_add(&row("evd_2", "run_b", "feat/b", 2))
        .unwrap();
    store
        .skill_evidence_add(&row("evd_3", "run_a", "feat/a", 3))
        .unwrap();
    assert!(
        store
            .skill_evidence_add(&row("evd_3", "run_a", "feat/a", 4))
            .is_err()
    );
    let ids = |rows: Vec<EvidenceRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(store.skill_evidence(REPO, "run_a").unwrap()),
        ["evd_1", "evd_3"]
    );
    assert_eq!(
        store.skill_evidence(REPO, "run_a").unwrap()[0],
        row("evd_1", "run_a", "feat/a", 1)
    );
    assert!(
        store
            .skill_evidence("other/repo", "run_a")
            .unwrap()
            .is_empty()
    );
    let runs = |branch, run, limit| {
        store
            .skill_run_heads(REPO, branch, run, limit)
            .unwrap()
            .into_iter()
            .map(|h| h.run_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(runs(None, None, 10), ["run_a", "run_b"]);
    assert_eq!(runs(None, None, 1), ["run_a"]);
    assert_eq!(runs(Some("feat/b"), None, 10), ["run_b"]);
    assert_eq!(runs(None, Some("run_b"), 10), ["run_b"]);
    assert!(runs(Some("feat/b"), Some("run_a"), 10).is_empty());
}

const CI_LABEL: &str = "github.com/acme/widgets";

/// The administrator who acts in the device tests; not the device owner.
const CI_ADMIN: &str = "usr_admin";

struct Ci {
    store: Arc<dyn Store>,
    owner: String,
    repository: String,
}

fn ci_setup(db: &Db) -> Ci {
    let store = db.open();
    let owner = store.upsert_user(&identity(7), "alice").unwrap().id;
    let con = store.insert_connection(&app("github.com"), &owner).unwrap();
    let repository = store
        .upsert_repository(&con.id, "github.com", 4242, "acme/widgets", None, None)
        .unwrap()
        .id;
    Ci {
        store,
        owner,
        repository,
    }
}

fn ci_key(seed: u8) -> DeviceKey {
    let public_key_hex = format!("{seed:02x}").repeat(32);
    DeviceKey {
        key_id: issuer_key_id(&public_key_hex).unwrap(),
        public_key_hex,
        not_before_ms: 1_000,
        not_after_ms: 9_000_000_000_000,
        rotated_at_ms: None,
    }
}

fn ci_auth(device: &str, seed: u8) -> DeviceAuthorization {
    DeviceAuthorization {
        version: 1,
        device_id: device.into(),
        repository: CI_LABEL.into(),
        owner: OwnerIdentity {
            provider: "oidc".into(),
            host: "idp.example".into(),
            person_id: "person:1".into(),
        },
        generation: 1,
        revoked_at_ms: None,
        allowed_check_kinds: vec!["local-ci".into()],
        keys: vec![ci_key(seed)],
    }
}

fn ci_request(id: &str, fingerprint: u8) -> LocalCiRequest {
    LocalCiRequest {
        request_id: id.into(),
        fingerprint: format!("{fingerprint:02x}").repeat(32),
    }
}

fn ci_applied(write: LocalCiDeviceWrite) -> LocalCiDevice {
    match write {
        LocalCiDeviceWrite::Applied(device) => device,
        other => panic!("expected applied, got {other:?}"),
    }
}

fn ci_enroll(ci: &Ci, auth: &DeviceAuthorization, request: &str) -> LocalCiDeviceWrite {
    ci.store
        .enroll_local_ci_device(
            &ci.repository,
            &ci.owner,
            CI_ADMIN,
            auth,
            &ci_request(request, 1),
            unix_now(),
        )
        .unwrap()
}

fn ci_change(
    store: &dyn Store,
    ci: &Ci,
    id: &str,
    expected: u64,
    next: &DeviceAuthorization,
    request: &LocalCiRequest,
) -> LocalCiDeviceWrite {
    store
        .change_local_ci_device(id, expected, &ci.owner, next, request, unix_now())
        .unwrap()
}

fn local_ci_enroll_read_list_and_advance(db: &Db) {
    let ci = ci_setup(db);
    let b = ci_applied(ci_enroll(&ci, &ci_auth("lcd_b", 2), "req_b"));
    ci_applied(ci_enroll(&ci, &ci_auth("lcd_a", 1), "req_a"));
    assert_eq!(b.authorization, ci_auth("lcd_b", 2));
    assert_eq!(b.digest, ci_auth("lcd_b", 2).digest().unwrap());
    assert_eq!(
        (b.repository.as_str(), b.owner.as_str()),
        (ci.repository.as_str(), ci.owner.as_str())
    );
    assert_eq!(ci.store.local_ci_device("lcd_b").unwrap(), Some(b));
    assert_eq!(ci.store.local_ci_device("lcd_none").unwrap(), None);
    let listed = |store: &dyn Store, repo: &str| {
        store
            .local_ci_devices(repo, None)
            .unwrap()
            .into_iter()
            .map(|d| (d.authorization.device_id, d.authorization.generation))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(&*ci.store, &ci.repository),
        [("lcd_a".to_owned(), 1), ("lcd_b".to_owned(), 1)]
    );
    assert!(listed(&*ci.store, "rep_other").is_empty());
    let owned = |owner: &str| {
        ci.store
            .local_ci_devices(&ci.repository, Some(owner))
            .unwrap()
            .len()
    };
    assert_eq!((owned(&ci.owner), owned("usr_nobody")), (2, 0));

    // Rotation and revocation each advance the generation by one.
    let current = ci.store.local_ci_device("lcd_a").unwrap().unwrap();
    let rotated = current.authorization.rotate(ci_key(3), 5_000).unwrap();
    let write = ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        1,
        &rotated,
        &ci_request("req_a2", 2),
    );
    assert_eq!(ci_applied(write).authorization.generation, 2);
    let revoked = rotated.revoke(6_000).unwrap();
    let write = ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        2,
        &revoked,
        &ci_request("req_a3", 3),
    );
    let device = ci_applied(write);
    assert_eq!(device.authorization.generation, 3);
    assert_eq!(device.authorization.revoked_at_ms, Some(6_000));

    // A rename keeps the binding to the repository row; the label is not rewritten.
    let con = ci.store.connections().unwrap().remove(0);
    ci.store
        .upsert_repository(&con.id, "github.com", 4242, "acme/renamed", None, None)
        .unwrap();
    assert_eq!(listed(&*ci.store, &ci.repository).len(), 2);
    assert_eq!(
        ci.store
            .local_ci_device("lcd_b")
            .unwrap()
            .unwrap()
            .authorization
            .repository,
        CI_LABEL
    );

    // Signing in again on a Hub computer does not touch device authorizations.
    let now = unix_now();
    let first = ci
        .store
        .insert_device(
            &ci.owner,
            "laptop",
            &store::device_digest(&"1".repeat(64)).unwrap(),
            None,
            None,
            now,
        )
        .unwrap();
    ci.store
        .insert_device(
            &ci.owner,
            "laptop",
            &store::device_digest(&"2".repeat(64)).unwrap(),
            Some(&first),
            None,
            now,
        )
        .unwrap();
    assert_eq!(
        ci.store
            .local_ci_device("lcd_b")
            .unwrap()
            .unwrap()
            .authorization
            .revoked_at_ms,
        None
    );

    // Everything survives a reopen, revoked state included.
    drop(ci.store);
    let reopened = db.open();
    let after = reopened.local_ci_device("lcd_a").unwrap().unwrap();
    assert_eq!(after.digest, revoked.digest().unwrap());
    assert_eq!(after.authorization.generation, 3);
    assert_eq!(after.authorization.revoked_at_ms, Some(6_000));

    // Enrollment preconditions are errors, not refusals.
    let ci = Ci {
        store: reopened,
        ..ci
    };
    let enroll = |owner: &str, auth: &DeviceAuthorization, id: &str| {
        ci.store.enroll_local_ci_device(
            &ci.repository,
            owner,
            CI_ADMIN,
            auth,
            &ci_request(id, 9),
            1,
        )
    };
    let mut wrong_generation = ci_auth("lcd_c", 4);
    wrong_generation.generation = 2;
    assert!(enroll(&ci.owner, &wrong_generation, "req_c1").is_err());
    let mut wrong_label = ci_auth("lcd_c", 4);
    wrong_label.repository = "github.com/acme/other".into();
    assert!(enroll(&ci.owner, &wrong_label, "req_c2").is_err());
    assert!(enroll("usr_missing", &ci_auth("lcd_c", 4), "req_c3").is_err());
    assert!(
        ci.store
            .enroll_local_ci_device(
                &ci.repository,
                &ci.owner,
                CI_ADMIN,
                &ci_auth("lcd_c", 4),
                &ci_request("bad id", 1),
                1
            )
            .is_err()
    );
    ci.store.set_user_disabled(&ci.owner, true).unwrap();
    assert!(enroll(&ci.owner, &ci_auth("lcd_c", 4), "req_c4").is_err());
    assert_eq!(ci.store.local_ci_device("lcd_c").unwrap(), None);
}

fn local_ci_replay_and_conflicts(db: &Db) {
    let ci = ci_setup(db);
    let auth = ci_auth("lcd_a", 1);
    assert_eq!(ci.store.local_ci_request("req_a").unwrap(), None);
    let first = ci_applied(ci_enroll(&ci, &auth, "req_a"));
    assert_eq!(
        ci.store
            .local_ci_request("req_a")
            .unwrap()
            .map(|(device, _)| device),
        Some("lcd_a".to_owned())
    );
    // Same id and fingerprint: the stored result, with no second event.
    assert_eq!(
        ci_enroll(&ci, &auth, "req_a"),
        LocalCiDeviceWrite::Replayed(first.clone())
    );
    assert_eq!(
        kinds(&*ci.store)
            .iter()
            .filter(|k| k.starts_with("local_ci"))
            .count(),
        1
    );
    // Same id, other fingerprint or other device.
    let other = ci
        .store
        .enroll_local_ci_device(
            &ci.repository,
            &ci.owner,
            CI_ADMIN,
            &auth,
            &ci_request("req_a", 7),
            1,
        )
        .unwrap();
    assert_eq!(
        other,
        LocalCiDeviceWrite::Refused(LocalCiRefusal::RequestConflict)
    );
    let other = ci_enroll(&ci, &ci_auth("lcd_b", 2), "req_a");
    assert_eq!(
        other,
        LocalCiDeviceWrite::Refused(LocalCiRefusal::RequestConflict)
    );
    assert_eq!(ci.store.local_ci_device("lcd_b").unwrap(), None);
    // A new request for an existing device.
    assert_eq!(
        ci_enroll(&ci, &auth, "req_again"),
        LocalCiDeviceWrite::Refused(LocalCiRefusal::DeviceExists)
    );

    let rotated = auth.rotate(ci_key(2), 5_000).unwrap();
    let request = ci_request("req_rot", 4);
    let applied = ci_applied(ci_change(&*ci.store, &ci, "lcd_a", 1, &rotated, &request));
    // The retry is a replay even though the generation has moved on.
    assert_eq!(
        ci_change(&*ci.store, &ci, "lcd_a", 1, &rotated, &request),
        LocalCiDeviceWrite::Replayed(applied)
    );
    assert_eq!(
        ci_change(
            &*ci.store,
            &ci,
            "lcd_a",
            1,
            &rotated,
            &ci_request("req_rot", 5)
        ),
        LocalCiDeviceWrite::Refused(LocalCiRefusal::RequestConflict)
    );
    // Stale generation, unknown device, and a next document that is not one step ahead.
    let next = rotated.revoke(6_000).unwrap();
    assert_eq!(
        ci_change(
            &*ci.store,
            &ci,
            "lcd_a",
            1,
            &next,
            &ci_request("req_stale", 6)
        ),
        LocalCiDeviceWrite::Refused(LocalCiRefusal::StaleGeneration { current: 2 })
    );
    assert_eq!(
        ci_change(
            &*ci.store,
            &ci,
            "lcd_none",
            2,
            &next,
            &ci_request("req_none", 6)
        ),
        LocalCiDeviceWrite::Refused(LocalCiRefusal::UnknownDevice)
    );
    assert!(
        ci.store
            .change_local_ci_device(
                "lcd_a",
                2,
                &ci.owner,
                &rotated,
                &ci_request("req_skip", 6),
                1
            )
            .is_err()
    );
    let mut moved = next.clone();
    moved.owner.person_id = "person:2".into();
    assert!(
        ci.store
            .change_local_ci_device(
                "lcd_a",
                2,
                &ci.owner,
                &moved,
                &ci_request("req_owner", 6),
                1
            )
            .is_err()
    );
    // Refusals recorded nothing: the same request id works once the cause is gone.
    assert_eq!(
        ci_applied(ci_change(
            &*ci.store,
            &ci,
            "lcd_a",
            2,
            &next,
            &ci_request("req_stale", 6)
        ))
        .authorization
        .generation,
        3
    );
}

fn local_ci_keys_are_enrolled_once_ever(db: &Db) {
    let ci = ci_setup(db);
    let a = ci_auth("lcd_a", 1);
    ci_applied(ci_enroll(&ci, &a, "req_a"));
    let refused = LocalCiDeviceWrite::Refused(LocalCiRefusal::DuplicateKey);
    // Another device cannot enroll the same key, and a failed enroll leaves nothing.
    assert_eq!(ci_enroll(&ci, &ci_auth("lcd_b", 1), "req_b"), refused);
    assert_eq!(ci.store.local_ci_device("lcd_b").unwrap(), None);
    ci_applied(ci_enroll(&ci, &ci_auth("lcd_b", 2), "req_b"));
    // Nor can it rotate to it.
    let steal = ci
        .store
        .local_ci_device("lcd_b")
        .unwrap()
        .unwrap()
        .authorization
        .rotate(ci_key(1), 5_000)
        .unwrap();
    assert_eq!(
        ci_change(
            &*ci.store,
            &ci,
            "lcd_b",
            1,
            &steal,
            &ci_request("req_steal", 2)
        ),
        refused
    );
    // A superseded key cannot come back, on the same device or another.
    let rotated = a.rotate(ci_key(3), 5_000).unwrap();
    ci_applied(ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        1,
        &rotated,
        &ci_request("req_rot", 3),
    ));
    assert!(rotated.rotate(ci_key(1), 6_000).is_err());
    // Nor a revoked device's key.
    let revoked = rotated.revoke(7_000).unwrap();
    ci_applied(ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        2,
        &revoked,
        &ci_request("req_rev", 5),
    ));
    assert_eq!(ci_enroll(&ci, &ci_auth("lcd_c", 3), "req_c"), refused);
    // A key dropped at the key bound stays spent.
    let mut dropping = ci_auth("lcd_d", 20);
    ci_applied(ci_enroll(&ci, &dropping, "req_d"));
    for seed in 21..=28 {
        let next = dropping
            .rotate(ci_key(seed), 8_000 + u64::from(seed))
            .unwrap();
        let request = ci_request(&format!("req_d{seed}"), seed);
        ci_applied(ci_change(
            &*ci.store,
            &ci,
            "lcd_d",
            dropping.generation,
            &next,
            &request,
        ));
        dropping = next;
    }
    assert!(dropping.keys.iter().all(|k| k.key_id != ci_key(20).key_id));
    let reuse = dropping.rotate(ci_key(20), 9_000).unwrap();
    assert_eq!(
        ci_change(
            &*ci.store,
            &ci,
            "lcd_d",
            dropping.generation,
            &reuse,
            &ci_request("req_reuse", 30)
        ),
        refused
    );
    assert_eq!(
        ci.store
            .local_ci_device("lcd_a")
            .unwrap()
            .unwrap()
            .authorization
            .generation,
        3
    );
    assert_eq!(
        ci.store
            .local_ci_device("lcd_b")
            .unwrap()
            .unwrap()
            .authorization
            .generation,
        1
    );
}

fn local_ci_concurrent_changes_have_one_winner(db: &Db) {
    let ci = ci_setup(db);
    let auth = ci_auth("lcd_a", 1);
    ci_applied(ci_enroll(&ci, &auth, "req_a"));
    let race = |device: &'static str, jobs: Vec<(DeviceAuthorization, LocalCiRequest)>| {
        let barrier = Arc::new(std::sync::Barrier::new(jobs.len()));
        let runs: Vec<_> = jobs
            .into_iter()
            .map(|(next, request)| {
                let store = db.open();
                let barrier = Arc::clone(&barrier);
                let owner = ci.owner.clone();
                thread::spawn(move || {
                    barrier.wait();
                    store
                        .change_local_ci_device(
                            device,
                            next.generation - 1,
                            &owner,
                            &next,
                            &request,
                            unix_now(),
                        )
                        .unwrap()
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().unwrap())
            .collect::<Vec<_>>()
    };
    let count = |results: &[LocalCiDeviceWrite], want: &dyn Fn(&LocalCiDeviceWrite) -> bool| {
        results.iter().filter(|r| want(r)).count()
    };

    // Two rotations at one generation.
    let results = race(
        "lcd_a",
        vec![
            (
                auth.rotate(ci_key(2), 5_000).unwrap(),
                ci_request("req_r1", 1),
            ),
            (
                auth.rotate(ci_key(3), 5_000).unwrap(),
                ci_request("req_r2", 2),
            ),
        ],
    );
    assert_eq!(
        count(&results, &|r| matches!(r, LocalCiDeviceWrite::Applied(_))),
        1,
        "{results:?}"
    );
    assert_eq!(
        count(&results, &|r| *r
            == LocalCiDeviceWrite::Refused(
                LocalCiRefusal::StaleGeneration { current: 2 }
            )),
        1,
        "{results:?}"
    );
    let stored = ci
        .store
        .local_ci_device("lcd_a")
        .unwrap()
        .unwrap()
        .authorization;
    assert_eq!((stored.generation, stored.keys.len()), (2, 2));

    // Revoke racing rotate.
    let results = race(
        "lcd_a",
        vec![
            (
                stored.rotate(ci_key(4), 6_000).unwrap(),
                ci_request("req_r3", 3),
            ),
            (stored.revoke(6_000).unwrap(), ci_request("req_v1", 4)),
        ],
    );
    assert_eq!(
        count(&results, &|r| matches!(r, LocalCiDeviceWrite::Applied(_))),
        1,
        "{results:?}"
    );
    assert_eq!(
        count(&results, &|r| matches!(
            r,
            LocalCiDeviceWrite::Refused(LocalCiRefusal::StaleGeneration { .. })
        )),
        1,
        "{results:?}"
    );
    let after = ci
        .store
        .local_ci_device("lcd_a")
        .unwrap()
        .unwrap()
        .authorization;
    assert_eq!(after.generation, 3);

    // The same request sent twice at once applies once.
    let fresh = ci_auth("lcd_e", 6);
    ci_applied(ci_enroll(&ci, &fresh, "req_e"));
    let next = fresh.rotate(ci_key(7), 7_000).unwrap();
    let results = race(
        "lcd_e",
        vec![
            (next.clone(), ci_request("req_same", 5)),
            (next, ci_request("req_same", 5)),
        ],
    );
    assert_eq!(
        count(&results, &|r| matches!(r, LocalCiDeviceWrite::Applied(_))),
        1,
        "{results:?}"
    );
    assert_eq!(
        count(&results, &|r| matches!(r, LocalCiDeviceWrite::Replayed(_))),
        1,
        "{results:?}"
    );
    let changes = kinds(&*ci.store)
        .iter()
        .filter(|k| k.starts_with("local_ci"))
        .count();
    assert_eq!(changes, 5);
}

fn local_ci_events_carry_ids_only(db: &Db) {
    let ci = ci_setup(db);
    let auth = ci_auth("lcd_a", 1);
    ci_applied(ci_enroll(&ci, &auth, "req_a"));
    let rotated = auth.rotate(ci_key(2), 5_000).unwrap();
    ci_applied(ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        1,
        &rotated,
        &ci_request("req_rot", 2),
    ));
    // A refused write appends nothing.
    ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        1,
        &rotated,
        &ci_request("req_late", 3),
    );
    ci_applied(ci_change(
        &*ci.store,
        &ci,
        "lcd_a",
        2,
        &rotated.revoke(6_000).unwrap(),
        &ci_request("req_rev", 4),
    ));

    let events: Vec<_> = ci
        .store
        .events_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with("local_ci"))
        .collect();
    assert_eq!(
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        [
            "local_ci_device_enrolled",
            "local_ci_device_rotated",
            "local_ci_device_revoked"
        ]
    );
    let data: Vec<Value> = events
        .iter()
        .map(|e| serde_json::from_str(&e.data).unwrap())
        .collect();
    let by = ci.owner.as_str();
    assert_eq!(
        data,
        [
            json!({"repository": ci.repository, "generation": 1, "key_ids": [ci_key(1).key_id], "by": CI_ADMIN}),
            json!({"repository": ci.repository, "generation": 2, "key_ids": [ci_key(2).key_id], "by": by}),
            json!({"repository": ci.repository, "generation": 3, "key_ids": [], "by": by}),
        ]
    );
    for event in &events {
        assert_eq!(event.subject, "lcd_a");
        assert!(!event.data.contains(&ci_key(1).public_key_hex));
        assert!(!event.data.contains(&ci_key(2).public_key_hex));
    }
}

fn local_ci_corrupt_records_are_refused(db: &Db) {
    let ci = ci_setup(db);
    ci_applied(ci_enroll(&ci, &ci_auth("lcd_a", 1), "req_a"));
    ci_applied(ci_enroll(&ci, &ci_auth("lcd_b", 2), "req_b"));
    let good = ci.store.local_ci_device("lcd_b").unwrap().unwrap();
    let other = ci_auth("lcd_a", 1).rotate(ci_key(9), 5_000).unwrap();
    let text =
        |auth: &DeviceAuthorization| String::from_utf8(auth.canonical_bytes().unwrap()).unwrap();
    let cases = [
        "document = '{}'".to_owned(),
        "document = 'not json'".to_owned(),
        format!("document = '{}'", text(&other)),
        "digest = 'abc'".to_owned(),
        "generation = 7".to_owned(),
        format!("document = '{}'", text(&ci_auth("lcd_x", 2))),
        format!(
            "document = '{}'",
            text(&good.authorization).replacen('{', "{\"extra\":1,", 1)
        ),
    ];
    for set in cases {
        db.exec(&format!(
            "UPDATE local_ci_devices SET {set} WHERE device_id = 'lcd_a'"
        ))
        .unwrap();
        assert!(ci.store.local_ci_device("lcd_a").is_err(), "{set}");
        assert!(
            ci.store.local_ci_devices(&ci.repository, None).is_err(),
            "{set}"
        );
        let next = ci_auth("lcd_a", 1).revoke(8_000).unwrap();
        let change = ci.store.change_local_ci_device(
            "lcd_a",
            1,
            &ci.owner,
            &next,
            &ci_request("req_c", 1),
            1,
        );
        assert!(change.is_err(), "{set}");
        db.exec(&format!(
            "UPDATE local_ci_devices SET document = '{}', digest = '{}', generation = 1 WHERE device_id = 'lcd_a'",
            text(&ci_auth("lcd_a", 1)),
            ci_auth("lcd_a", 1).digest().unwrap()
        ))
        .unwrap();
        assert!(
            ci.store.local_ci_device("lcd_a").unwrap().is_some(),
            "{set}"
        );
    }
    assert_eq!(ci.store.local_ci_device("lcd_b").unwrap().unwrap(), good);
}

/// The `repository_scope` migration gives each existing row its writer's scope and keeps every
/// row. Rows are written through the store, the schema is put back as it was before the migration
/// (losing only the new column), and the reopened store runs the migration again.
fn repository_scope_backfill(db: &Db) {
    use agentscm::hub::publish::{Manifest, NewPublication, derive_id};
    let store = db.open();
    let github = |id: i64| ForgeIdentity {
        provider: "github".into(),
        host: "github.com".into(),
        user_id: id,
    };
    let alice = store.upsert_user(&github(1), "alice").unwrap();
    let dave = store.upsert_user(&github(4), "dave").unwrap();
    let bound = mint_bound_agent_token(&*store, &alice.id, "bound", REPO, 1001, TTL, false)
        .unwrap()
        .0;
    // No repository id: it takes the connected repository's; with none connected, its name.
    let connected = mint_agent_token(&*store, &dave.id, "connected", "acme/gadgets", TTL)
        .unwrap()
        .0;
    let unconnected = mint_agent_token(&*store, &dave.id, "unconnected", "acme/tools", TTL)
        .unwrap()
        .0;
    let connection = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    store
        .upsert_repository(
            &connection.id,
            "github.com",
            3003,
            "acme/gadgets",
            None,
            None,
        )
        .unwrap();
    let now = unix_now();
    for (n, token) in [&bound, &connected, &unconnected].into_iter().enumerate() {
        let intent = NewIntent {
            id: format!("int_{n:032x}"),
            task: "t".into(),
            branch: None,
            base: None,
            machine: None,
            paths: vec![format!("src/{n}.rs")],
            ttl_seconds: 600,
        };
        store.announce_intent(token, &intent, now).unwrap();
    }
    let manifest: Manifest = serde_json::from_value(json!({
        "revision": "a".repeat(64), "event_seq": 3, "commit": "b".repeat(40),
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": "feat/x", "base": "main",
        "title": "t", "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": "f".repeat(64), "size": 5}]
    }))
    .unwrap();
    let publication = derive_id(REPO, 3, &"b".repeat(40), "feat/x");
    store
        .create_publication(
            &bound,
            &NewPublication {
                id: publication.clone(),
                manifest,
            },
            now,
        )
        .unwrap();
    let sample = agentscm::hub::hook_overhead::Sample {
        session: "s".into(),
        agent: "agent".into(),
        hub_calls: 1,
        injected_start: 0,
        injected_refusal: 0,
        injected_other: 0,
        client: None,
    };
    store.hook_overhead(&bound, REPO, &sample, now).unwrap();
    // Evidence the Hub observed on the forge is recorded under no token.
    store
        .skill_evidence_add(&EvidenceRow {
            id: "evd_forge".into(),
            repository: "acme/gadgets".into(),
            repository_scope: "unused".into(),
            branch: "feat/x".into(),
            run_id: "run_forge".into(),
            skill: "pr-gate".into(),
            skill_sha256: "0".repeat(64),
            gate: "ci".into(),
            kind: "hosted_ci".into(),
            head_sha: "a".repeat(40),
            assurance: "observed".into(),
            summary: "ok".into(),
            url: None,
            pull_request: None,
            detail: None,
            token_id: "forge".into(),
            session: "forge".into(),
            agent: "GitHub".into(),
            recorded_at: 1,
        })
        .unwrap();
    let adoption = |repository: &str, source: &str, reported_by: Option<&str>| SkillAdoption {
        repository: repository.into(),
        repository_scope: format!("unused/{repository}"),
        source: source.into(),
        lock_json: "{}".into(),
        default_branch: None,
        observed_at: 1,
        reported_by: reported_by.map(str::to_owned),
        local_skills: None,
    };
    store
        .skill_adoption_put(&adoption("acme/gadgets", "default_branch", None))
        .unwrap();
    store
        .skill_adoption_put(&adoption(REPO, "reported_by_checkout", Some(&bound)))
        .unwrap();
    store
        .skill_adoption_put(&adoption("acme/unknown", "default_branch", None))
        .unwrap();
    drop(store);

    // The schema before the migration.
    db.exec(
        "DROP INDEX hub_intents_live;
         ALTER TABLE hub_intents DROP COLUMN repository_scope;
         CREATE INDEX hub_intents_live ON hub_intents (repository, expires_at);
         DROP INDEX hub_intent_paths_key;
         ALTER TABLE hub_intent_paths DROP COLUMN repository_scope;
         CREATE INDEX hub_intent_paths_key ON hub_intent_paths (repository, key);
         DROP INDEX hub_handoffs_scope;
         ALTER TABLE hub_handoffs DROP COLUMN repository_scope;
         DROP INDEX hub_contested_edits_scope;
         ALTER TABLE hub_contested_edits DROP COLUMN repository_scope;
         CREATE INDEX hub_contested_edits_scope ON hub_contested_edits (repository, observed_at);
         DROP INDEX hub_duplicate_starts_scope;
         ALTER TABLE hub_duplicate_starts DROP COLUMN repository_scope;
         CREATE INDEX hub_duplicate_starts_scope ON hub_duplicate_starts (repository, observed_at);
         DROP INDEX hub_hook_overhead_scope;
         ALTER TABLE hub_hook_overhead DROP COLUMN repository_scope;
         CREATE INDEX hub_hook_overhead_scope ON hub_hook_overhead (repository, updated_at);
         DROP INDEX hub_publications_repository;
         DROP INDEX hub_publications_pull;
         DROP INDEX hub_publications_commit;
         ALTER TABLE hub_publications DROP COLUMN repository_scope;
         CREATE INDEX hub_publications_repository ON hub_publications (repository, created_at);
         CREATE INDEX hub_publications_pull ON hub_publications (lower(repository), pr_number, created_at);
         CREATE INDEX hub_publications_commit ON hub_publications (repository, commit_sha);
         DROP TABLE hub_candidate_heads;
         CREATE TABLE hub_candidate_heads (repository TEXT NOT NULL, branch TEXT NOT NULL,
             publication TEXT NOT NULL, PRIMARY KEY (repository, branch));
         DROP INDEX hub_skill_evidence_branch;
         ALTER TABLE hub_skill_evidence DROP COLUMN repository_scope;
         CREATE INDEX hub_skill_evidence_branch ON hub_skill_evidence (repository, branch);
         CREATE TABLE hub_skill_adoptions_copy AS SELECT repository, source, lock_json,
             default_branch, observed_at, reported_by, local_skills FROM hub_skill_adoptions;
         DROP TABLE hub_skill_adoptions;
         CREATE TABLE hub_skill_adoptions (repository TEXT NOT NULL PRIMARY KEY,
             source TEXT NOT NULL, lock_json TEXT NOT NULL, default_branch TEXT,
             observed_at BIGINT NOT NULL, reported_by TEXT, local_skills TEXT);
         INSERT INTO hub_skill_adoptions SELECT * FROM hub_skill_adoptions_copy;
         DROP TABLE hub_skill_adoptions_copy;
         DELETE FROM hub_named_migrations WHERE name = 'repository_scope';",
    )
    .unwrap();
    // Rows only the old schema could hold: a candidate head, and contested-edit rows of a known
    // token and of one the Hub no longer has.
    db.exec(&format!(
        "INSERT INTO hub_candidate_heads VALUES ('{REPO}', 'feat/x', '{publication}');
         INSERT INTO hub_contested_edits (repository, token, session, path, hold, kind, outcome,
             observed_at) VALUES ('{REPO}', '{bound}', 's', 'src/a.rs', 'int_x', 'notice',
             'proceeded', 1), ('acme/gone', 'tok_gone', 's', 'src/a.rs', 'int_x', 'notice',
             'proceeded', 1);"
    ))
    .unwrap();

    let store = db.open();
    assert!(db.named().iter().any(|name| name == "repository_scope"));
    let bound_scope = store::repository_scope("github.com", REPO, Some(1001));
    let connected_scope = store::repository_scope("github.com", "acme/gadgets", Some(3003));
    let unconnected_scope = store::repository_scope("github.com", "acme/tools", None);
    let count = |table: &str, scope: &str| {
        db.count(&format!(
            "SELECT COUNT(*) FROM {table} WHERE repository_scope = '{scope}'"
        ))
    };
    for (table, scope, rows) in [
        ("hub_intents", &bound_scope, 1),
        ("hub_intents", &connected_scope, 1),
        ("hub_intents", &unconnected_scope, 1),
        ("hub_intent_paths", &bound_scope, 1),
        ("hub_intent_paths", &connected_scope, 1),
        ("hub_publications", &bound_scope, 1),
        ("hub_candidate_heads", &bound_scope, 1),
        ("hub_hook_overhead", &bound_scope, 1),
        ("hub_contested_edits", &bound_scope, 1),
        // No token accounts for it: it keeps its bare name, which no caller's scope equals.
        ("hub_contested_edits", &"acme/gone".to_owned(), 1),
        ("hub_skill_adoptions", &connected_scope, 1),
        ("hub_skill_evidence", &connected_scope, 1),
        ("hub_skill_adoptions", &bound_scope, 1),
        ("hub_skill_adoptions", &"acme/unknown".to_owned(), 1),
    ] {
        assert_eq!(count(table, scope), rows, "{table} {scope}");
    }
    for (table, rows) in [
        ("hub_intents", 3),
        ("hub_intent_paths", 3),
        ("hub_publications", 1),
        ("hub_candidate_heads", 1),
        ("hub_contested_edits", 2),
        ("hub_skill_adoptions", 3),
    ] {
        assert_eq!(
            db.count(&format!("SELECT COUNT(*) FROM {table}")),
            rows,
            "{table}"
        );
    }
    // The store reads the backfilled rows under the same scopes it now writes.
    assert_eq!(
        store.live_intents(&connected_scope, now, 10).unwrap().len(),
        1
    );
    assert!(
        store
            .publication(&bound_scope, &publication)
            .unwrap()
            .is_some()
    );
    assert!(store.skill_adoption(&connected_scope).unwrap().is_some());
}

/// Three repositories named `acme/widgets`: one on github.com, one on gitlab.com with the same
/// forge id, and one on github.com with another id (as after the first was renamed and its name
/// reused). Each reads only its own intents, publications, skill rows and hook overhead.
fn same_name_repositories_stay_apart(db: &Db) {
    use agentscm::hub::publish::{Create, Manifest, NewPublication, derive_id};
    let store = db.open();
    let user = |provider: &str, host: &str, id: i64| {
        let identity = ForgeIdentity {
            provider: provider.into(),
            host: host.into(),
            user_id: id,
        };
        store.upsert_user(&identity, &format!("user{id}")).unwrap()
    };
    let (alice, bob, carol) = (
        user("github", "github.com", 1),
        user("gitlab", "gitlab.com", 2),
        user("github", "github.com", 3),
    );
    let bound = |owner: &str, id: i64| {
        mint_bound_agent_token(&*store, owner, "agent", REPO, id, TTL, false)
            .unwrap()
            .0
    };
    let tokens = [
        bound(&alice.id, 1001),
        bound(&bob.id, 1001),
        bound(&carol.id, 2002),
    ];
    let scopes = [
        store::repository_scope("github.com", REPO, Some(1001)),
        store::repository_scope("gitlab.com", REPO, Some(1001)),
        store::repository_scope("github.com", REPO, Some(2002)),
    ];
    assert!(scopes[0] != scopes[1] && scopes[1] != scopes[2] && scopes[0] != scopes[2]);
    let now = unix_now();
    let intent = |n: u32| NewIntent {
        id: format!("int_{n:032x}"),
        task: "t".into(),
        branch: None,
        base: None,
        machine: None,
        paths: vec!["src/a.rs".into()],
        ttl_seconds: 600,
    };
    // The same file in each repository: no announce sees another repository's.
    for (n, token) in (1..).zip(&tokens) {
        let announced = store.announce_intent(token, &intent(n), now).unwrap();
        assert!(
            announced.overlaps.is_empty(),
            "{n}: {:?}",
            announced.overlaps
        );
    }
    for (n, scope) in (1..).zip(&scopes) {
        let live = store.live_intents(scope, now, 10).unwrap();
        assert_eq!(
            live.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            [intent(n).id]
        );
    }

    // A publication id comes from the name, so the second repository derives the first's: it
    // must not get the first's row back, nor read it by id, list or pull request.
    let manifest: Manifest = serde_json::from_value(json!({
        "revision": "a".repeat(64), "event_seq": 3, "commit": "b".repeat(40),
        "tree": "c".repeat(40), "parent": "d".repeat(40), "branch": "feat/x", "base": "main",
        "title": "t", "body": "", "message": "msg\n",
        "author": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "committer": {"name": "A", "email": "a@example.invalid", "date": "2026-05-28T20:26:40Z"},
        "changes": [{"path": "a.txt", "mode": "100644", "blob": "e".repeat(40),
            "sha256": "f".repeat(64), "size": 5}]
    }))
    .unwrap();
    let new = NewPublication {
        id: derive_id(REPO, 3, &"b".repeat(40), "feat/x"),
        manifest,
    };
    assert!(matches!(
        store.create_publication(&tokens[0], &new, now).unwrap(),
        Create::Done(_)
    ));
    assert!(matches!(
        store.create_publication(&tokens[1], &new, now).unwrap(),
        Create::Conflict
    ));
    assert!(store.publication(&scopes[0], &new.id).unwrap().is_some());
    for scope in &scopes[1..] {
        assert_eq!(store.publication(scope, &new.id).unwrap(), None);
        assert!(store.publications(scope, None, 10).unwrap().is_empty());
        assert!(store.pull_publications(scope, 0, 10).unwrap().is_empty());
        let found = store.publication_search(scope, None, "feat", 10).unwrap();
        assert!(found.rows.is_empty());
    }
    assert_eq!(store.publications(&scopes[0], None, 10).unwrap().len(), 1);

    // Skill adoptions and evidence.
    store
        .skill_adoption_put(&SkillAdoption {
            repository: REPO.into(),
            repository_scope: scopes[0].clone(),
            source: "default_branch".into(),
            lock_json: "{}".into(),
            default_branch: Some("main".into()),
            observed_at: 1,
            reported_by: None,
            local_skills: None,
        })
        .unwrap();
    store
        .skill_evidence_add(&EvidenceRow {
            id: "evd_1".into(),
            repository: REPO.into(),
            repository_scope: scopes[0].clone(),
            branch: "feat/x".into(),
            run_id: "run_a".into(),
            skill: "pr-gate".into(),
            skill_sha256: "0".repeat(64),
            gate: "tests".into(),
            kind: "revision_tests".into(),
            head_sha: "a".repeat(40),
            assurance: "self_reported".into(),
            summary: "ok".into(),
            url: None,
            pull_request: None,
            detail: None,
            token_id: tokens[0].clone(),
            session: "s".into(),
            agent: "agent".into(),
            recorded_at: 1,
        })
        .unwrap();
    assert!(store.skill_adoption(&scopes[0]).unwrap().is_some());
    assert_eq!(store.skill_evidence(&scopes[0], "run_a").unwrap().len(), 1);
    for scope in &scopes[1..] {
        assert_eq!(store.skill_adoption(scope).unwrap(), None);
        assert!(store.skill_evidence(scope, "run_a").unwrap().is_empty());
        assert!(
            store
                .skill_run_heads(scope, None, None, 10)
                .unwrap()
                .is_empty()
        );
    }

    // Hook overhead rows take the writing token's repository.
    let sample = agentscm::hub::hook_overhead::Sample {
        session: "s".into(),
        agent: "agent".into(),
        hub_calls: 1,
        injected_start: 0,
        injected_refusal: 0,
        injected_other: 0,
        client: None,
    };
    store.hook_overhead(&tokens[0], REPO, &sample, now).unwrap();
    let rows = |scope: &str| {
        db.count(&format!(
            "SELECT COUNT(*) FROM hub_hook_overhead WHERE repository_scope = '{scope}'"
        ))
    };
    assert_eq!(
        scopes.iter().map(|s| rows(s)).collect::<Vec<_>>(),
        [1, 0, 0]
    );

    // A token with no repository id reads the repository connected under its name on its host.
    let connection = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    store
        .upsert_repository(&connection.id, "github.com", 2002, REPO, None, None)
        .unwrap();
    let dave = user("github", "github.com", 4);
    let (_, secret) = mint_agent_token(&*store, &dave.id, "unbound", REPO, TTL).unwrap();
    let principal = store
        .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
        .unwrap()
        .unwrap();
    assert_eq!(principal.repository_id, None);
    assert_eq!(
        store::principal_scope(&*store, &principal).unwrap(),
        scopes[2]
    );
    let shared = store
        .announce_intent(&principal.token_id, &intent(4), now)
        .unwrap();
    assert_eq!(
        shared
            .overlaps
            .iter()
            .map(|o| o.intent.id.clone())
            .collect::<Vec<_>>(),
        [intent(3).id]
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

fn hand_off_pickups(db: &Db) {
    use agentscm::hub::pickup::{MAX_PICKUPS_PER_OWNER, PickupState};
    let store = db.open();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let bob = store.upsert_user(&identity(8), "bob").unwrap();
    let token = |owner: &str, name: &str| {
        mint_bound_agent_token(&*store, owner, name, REPO, 4242, TTL, false)
            .unwrap()
            .0
    };
    let (alice_agent, bob_agent) = (token(&alice.id, "claude-code"), token(&bob.id, "codex"));
    let start = |key: Option<&str>, work: Option<&str>, session: &str, path: &str| {
        serde_json::from_value::<agentscm::hub::work::WorkStart>(json!({
            "request_key": key, "work_id": work, "title": "Synthetic hand-off",
            "session": session, "scope": [path]}))
        .unwrap()
    };
    let now = unix_now();
    let state = |id: &str| {
        store
            .pickups_of_owner(&alice.id, REPO, now, 50)
            .unwrap()
            .into_iter()
            .find(|p| p.id == id)
            .unwrap()
    };

    let task = store
        .pickup_create(&alice.id, REPO, None, Some("Synthetic"), now)
        .unwrap();
    assert_id(&task.id, "pku_");
    assert_eq!(task.request_key.as_deref(), Some(task.id.as_str()));
    store
        .work_start(
            &bob_agent,
            &start(Some(&task.id), None, "b", "src/b.rs"),
            now,
        )
        .unwrap();
    assert_eq!(state(&task.id).state, PickupState::Waiting);
    let item = store
        .work_start(
            &alice_agent,
            &start(Some(&task.id), None, "a", "src/a.rs"),
            now,
        )
        .unwrap()
        .item;
    let picked = state(&task.id).picked_up.unwrap();
    assert_eq!(
        (
            picked.agent.as_str(),
            picked.session.as_str(),
            picked.work_id.as_str(),
            picked.at
        ),
        ("claude-code", "a", item.id.as_str(), now)
    );

    let handed = store
        .pickup_create(&alice.id, REPO, Some(&item.id), None, now)
        .unwrap();
    assert_eq!(handed.request_key, None);
    store
        .work_start(
            &alice_agent,
            &start(None, Some(&item.id), "a", "src/a.rs"),
            now,
        )
        .unwrap();
    assert_eq!(state(&handed.id).state, PickupState::PickedUp);

    let dropped = store
        .pickup_create(&alice.id, REPO, None, None, now)
        .unwrap();
    assert!(
        store
            .pickup_abandon(&bob.id, REPO, &dropped.id, now)
            .unwrap()
            .is_none()
    );
    let abandoned = store
        .pickup_abandon(&alice.id, REPO, &dropped.id, now)
        .unwrap()
        .unwrap();
    assert_eq!(abandoned.state, PickupState::Abandoned);
    store
        .work_start(
            &alice_agent,
            &start(Some(&dropped.id), None, "c", "src/c.rs"),
            now,
        )
        .unwrap();
    assert_eq!(state(&dropped.id).state, PickupState::Abandoned);
    assert!(
        store
            .pickups_of_owner(&bob.id, REPO, now, 50)
            .unwrap()
            .is_empty()
    );

    // A start that a live claim refused claimed nothing, so the hand-off keeps waiting.
    let held = store
        .pickup_create(&alice.id, REPO, Some(&item.id), None, now)
        .unwrap();
    let refused = store
        .work_start(
            &alice_agent,
            &start(None, Some(&item.id), "other", "src/a.rs"),
            now,
        )
        .unwrap();
    assert!(refused.conflict.is_some(), "{refused:?}");
    assert_eq!(state(&held.id).state, PickupState::Waiting);

    // Reads compare the repository name without regard to case.
    assert!(
        store
            .pickups_of_owner(&alice.id, "ACME/Widgets", now, 50)
            .unwrap()
            .iter()
            .any(|p| p.id == held.id)
    );

    // Old records are pruned on the next create, and only the newest bounded number stay.
    let old = now - agentscm::hub::pickup::PICKUP_RETAIN_SECONDS - 1;
    let stale = store
        .pickup_create(&alice.id, REPO, None, None, old)
        .unwrap();
    assert_eq!(stale.state, PickupState::Waiting);
    for _ in 0..MAX_PICKUPS_PER_OWNER {
        store
            .pickup_create(&alice.id, REPO, None, None, now)
            .unwrap();
    }
    let newest = store
        .pickup_create(&alice.id, REPO, None, None, now + 1)
        .unwrap();
    let all = store.pickups_of_owner(&alice.id, REPO, now, 1000).unwrap();
    assert_eq!(all.len() as i64, MAX_PICKUPS_PER_OWNER);
    assert!(all.iter().all(|p| p.id != stale.id));
    assert_eq!(all[0].id, newest.id);
    assert!(
        store
            .pickup_create(&alice.id, "not a repo", None, None, now)
            .is_err()
    );
    assert!(
        store
            .pickup_create(&alice.id, REPO, Some("W-12"), None, now)
            .is_err()
    );
}

/// Agent conversations are shared by default on a Hub with no one in it. A Hub that has people
/// when it gets that default keeps off, and a default somebody set is left alone.
fn conversation_sharing_default_stays_off_for_a_hub_that_has_people(db: &Db) {
    const KEY: &str = "conversation_sharing:default";
    let store = db.open();
    let migrate = || {
        db.exec(if db.pg.is_some() {
            include_str!("../../src/hub/migrations/named/conversation_sharing_default.pg.sql")
        } else {
            include_str!("../../src/hub/migrations/named/conversation_sharing_default.sqlite.sql")
        })
        .unwrap()
    };
    // Opening an empty Hub ran the migration and recorded nothing, as does running it again.
    assert_eq!(store.get_setting(KEY).unwrap(), None);
    migrate();
    assert_eq!(store.get_setting(KEY).unwrap(), None);

    store.upsert_user(&identity(7), "alice").unwrap();
    migrate();
    assert_eq!(store.get_setting(KEY).unwrap().as_deref(), Some("off"));

    store.set_setting(KEY, "on", "admin").unwrap();
    migrate();
    assert_eq!(store.get_setting(KEY).unwrap().as_deref(), Some("on"));
}

fn settings_read_write_and_create_once(db: &Db) {
    let store = db.open();
    assert_eq!(store.get_setting("k").unwrap(), None);
    // The first create stores its value; a later create returns it unchanged.
    assert_eq!(
        store.get_or_create_setting("k", "first", "a").unwrap(),
        "first"
    );
    assert_eq!(
        store.get_or_create_setting("k", "second", "b").unwrap(),
        "first"
    );
    assert_eq!(store.get_setting("k").unwrap().as_deref(), Some("first"));
    // A set replaces it, and only that key changes.
    store.set_setting("k", "it's $1", "c").unwrap();
    store.set_setting("other", "x", "c").unwrap();
    assert_eq!(store.get_setting("k").unwrap().as_deref(), Some("it's $1"));
    assert_eq!(store.get_setting("other").unwrap().as_deref(), Some("x"));
    assert_eq!(
        store.get_or_create_setting("k", "third", "d").unwrap(),
        "it's $1"
    );
}

fn work_payload_used_reads_the_charged_bytes(db: &Db) {
    use agentscm::hub::work::WorkRepositoryScope;

    let store = db.open();
    let scope = |repository_id: Option<i64>| WorkRepositoryScope {
        provider: "github".into(),
        host: "github.com".into(),
        repository: REPO.into(),
        repository_id,
    };
    // A repository that never wrote holds nothing.
    assert_eq!(store.work_payload_used(&scope(Some(101))).unwrap(), 0);
    db.exec(&format!(
        "INSERT INTO hub_work_usage (provider, host, repository, repository_id, charged_bytes, updated_at)
         VALUES ('github', 'github.com', '{REPO}', 101, 4096, 1),
                ('github', 'github.com', '{REPO}', NULL, 77, 1)"
    ))
    .unwrap();
    // The figure belongs to one repository identity: another id and a missing id are their own.
    assert_eq!(store.work_payload_used(&scope(Some(101))).unwrap(), 4096);
    assert_eq!(store.work_payload_used(&scope(Some(102))).unwrap(), 0);
    assert_eq!(store.work_payload_used(&scope(None)).unwrap(), 77);
    let other_host = WorkRepositoryScope {
        host: "gitlab.example.test".into(),
        ..scope(Some(101))
    };
    assert_eq!(store.work_payload_used(&other_host).unwrap(), 0);
}

conformance!(
    token_lookup,
    expiry,
    revocation,
    owner_disable_cascades,
    rename_keeps_identity,
    display_name_is_stored_and_cleared,
    idempotent_migrations,
    named_migrations_are_a_set,
    concurrent_startup,
    restart_persistence,
    events_record_each_change,
    validation,
    repository_id_round_trips,
    read_only_round_trips,
    list_and_owned_revoke,
    admin_actions_and_sign_in,
    upgrade_from_migration_1,
    upgrade_from_hub_settings_migration,
    connections_and_repositories,
    observations,
    provider_context_events_are_digest_summaries_of_retained_bytes,
    deliveries_outbound_and_notices,
    forge_check_journal,
    merge_group_journal,
    merge_group_listings,
    external_run_journal,
    merged_publications_skip_unmerged_ones,
    pull_publications_are_newest_per_pull_request,
    forge_checks_showing_success_leave_out_merged_pull_requests,
    upgrade_from_migration_3,
    upgrade_from_migration_14_with_publication,
    url_errors,
    skill_adoption_and_evidence,
    same_name_repositories_stay_apart,
    repository_scope_backfill,
    devices_and_refusals,
    browser_sessions,
    local_ci_enroll_read_list_and_advance,
    local_ci_replay_and_conflicts,
    local_ci_keys_are_enrolled_once_ever,
    local_ci_concurrent_changes_have_one_winner,
    local_ci_events_carry_ids_only,
    local_ci_corrupt_records_are_refused,
    hand_off_pickups,
    work_payload_used_reads_the_charged_bytes,
    conversation_sharing_default_stays_off_for_a_hub_that_has_people,
    settings_read_write_and_create_once,
);

#[cfg(unix)]
#[test]
fn sqlite_file_is_private_and_existing_mode_is_kept() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let fresh = dir.path().join("fresh.sqlite");
    store::open(&format!("sqlite://{}", fresh.display())).unwrap();
    assert_eq!(
        fresh.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    let existing = dir.path().join("existing.sqlite");
    std::fs::write(&existing, b"").unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o640)).unwrap();
    store::open(&format!("sqlite://{}", existing.display())).unwrap();
    assert_eq!(
        existing.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
fn postgres_reconnects_after_the_backend_is_terminated() {
    let Some(pg) = PgSchema::create() else { return };
    let store = store::open(&pg.url).unwrap();
    let alice = store.upsert_user(&identity(7), "alice").unwrap();
    let (_, digest) = mint(&*store, &alice.id, "builder", TTL);
    let mut admin = pg.admin();
    let mut terminate = || {
        let sql = "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                   WHERE application_name = $1 AND pid <> pg_backend_pid()";
        assert!(!admin.query(sql, &[&pg.schema]).unwrap().is_empty());
        let count = "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1";
        for _ in 0..100 {
            if admin
                .query_one(count, &[&pg.schema])
                .unwrap()
                .get::<_, i64>(0)
                == 0
            {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("backend did not exit");
    };
    terminate();
    store.ping().unwrap();
    terminate();
    assert!(store.authenticate(&digest, unix_now()).unwrap().is_some());
    terminate();
    assert_eq!(store.events_after(0, 10).unwrap().len(), 2);
    // A write is never retried, so it may fail on the dead connection; the next call reconnects.
    terminate();
    let _ = store.set_user_disabled(&alice.id, true);
    store.set_user_disabled(&alice.id, true).unwrap();
    assert_eq!(store.authenticate(&digest, unix_now()).unwrap(), None);
}

fn provider_context_events_are_digest_summaries_of_retained_bytes(db: &Db) {
    use sha2::{Digest, Sha256};

    let store = db.open();
    let connection = store
        .insert_connection(&app("github.com"), "usr_admin")
        .unwrap();
    let repo = store
        .upsert_repository(
            &connection.id,
            "github.com",
            4242,
            "acme/widgets",
            None,
            None,
        )
        .unwrap();
    let snapshot = json!({"version": 1, "provider": "github", "repository_id": 4242,
        "number": 17, "head_sha": "a".repeat(40), "base_sha": "b".repeat(40),
        "permissions": {"availability": "observed"},
        "reviews": {"availability": "available", "complete": true, "items": [{"provider_id": 1, "body": "private-event-marker".repeat(740)}]},
        "check_runs": {"availability": "incomplete", "complete": false, "items": [{"name": "test", "state": "in_progress"}]},
        "commit_statuses": {"availability": "permission_missing", "complete": false, "items": []}});
    // Deliberate whitespace makes the digest's exact-byte contract observable.
    let data = format!(" \n{}\n ", snapshot);
    assert!(data.len() > 14 * 1024);
    assert!(data.len() <= 16 * 1024);
    store
        .observe(&repo.id, "provider_context", "17", &data, 100)
        .unwrap();
    let retained = store
        .observation(&repo.id, "provider_context", "17")
        .unwrap()
        .unwrap();
    assert_eq!(retained.data, data);
    let expected_digest = format!("{:x}", Sha256::digest(data.as_bytes()));
    let canonical_digest = format!("{:x}", Sha256::digest(snapshot.to_string().as_bytes()));
    assert_ne!(expected_digest, canonical_digest);
    let event = store
        .events_after(0, 100)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "provider_context_observed")
        .unwrap();
    assert!(event.data.len() <= 1024);
    let summary: Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(summary["number"], 17);
    assert_eq!(summary["repository_id"], 4242);
    assert_eq!(summary["scope"], "pull_request");
    assert_eq!(summary["head_sha"], "a".repeat(40));
    assert_eq!(
        summary["categories"]["reviews"],
        json!({"availability": "available", "complete": true})
    );
    assert_eq!(
        summary["categories"]["check_runs"],
        json!({"availability": "incomplete", "complete": false})
    );
    assert_eq!(
        summary["categories"]["commit_statuses"],
        json!({"availability": "permission_missing", "complete": false})
    );
    assert_eq!(
        summary["snapshot_digest"],
        json!({"algorithm": "sha256", "encoding": "hex", "covered_bytes": "observation_data_utf8", "value": expected_digest})
    );
    assert!(!event.data.contains("private-event-marker"));
    assert!(summary.get("reviews").is_none());
    assert_eq!(
        store
            .observe(&repo.id, "provider_context", "17", &data, 200)
            .unwrap(),
        Observed::Unchanged
    );
    assert_eq!(events_of(&*store, "provider_context_observed").len(), 1);
    // Production forget passes a tombstone, not the old observation bytes.
    let tombstone = json!({"number": 17, "availability": "closed_or_gone"}).to_string();
    assert!(
        store
            .forget(
                &repo.id,
                "provider_context",
                "17",
                300,
                &tombstone,
                "provider_context_removed"
            )
            .unwrap()
    );
    let removed = store
        .events_after(event.seq, 100)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "provider_context_removed")
        .unwrap();
    assert!(removed.data.len() <= 1024);
    let removed_summary: Value = serde_json::from_str(&removed.data).unwrap();
    assert_eq!(removed_summary["number"], 17);
    assert_eq!(removed_summary["repository_id"], 4242);
    assert_eq!(removed_summary["change"], "removed");
    assert_eq!(removed_summary.get("snapshot_digest"), Some(&Value::Null));
    assert_eq!(removed_summary.get("head_sha"), Some(&Value::Null));
    assert_eq!(removed_summary["categories"], json!({}));
    assert!(!removed.data.contains("private-event-marker"));
}
