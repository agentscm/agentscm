//! One suite, run against SQLite always and PostgreSQL when AGENTSCM_TEST_DATABASE_URL is set.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;
use std::thread;

use agentscm::hub::connection::{
    ForgeCheck, ForgeCheckReserve, MergeGroup, NewConnection, NewForgeCheck, NewMergeGroup,
    Observed,
};
use agentscm::hub::external_runs::{ExternalKind, ExternalObserved, ExternalOutcome, ExternalRun};
use agentscm::hub::intent::NewIntent;
use agentscm::hub::skills::{EvidenceRow, SkillAdoption};
use agentscm::hub::store::{
    self, ForgeIdentity, MAX_AGENT_TOKEN_TTL_SECONDS, NewAgentToken, Store, agent_token_digest,
    mint_agent_token, mint_bound_agent_token, new_connection_id, unix_now,
};
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
        include_str!("../src/hub/migrations/0001_core.pg.sql")
    } else {
        include_str!("../src/hub/migrations/0001_core.sqlite.sql")
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
            include_str!("../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../src/hub/migrations/0003_intents.pg.sql"),
            include_str!("../src/hub/migrations/0004_github.pg.sql"),
            include_str!("../src/hub/migrations/0005_publish.pg.sql"),
            include_str!("../src/hub/migrations/0006_sessions.pg.sql"),
            include_str!("../src/hub/migrations/0007_warn_once.pg.sql"),
            include_str!("../src/hub/migrations/0008_read_only_tokens.pg.sql"),
            include_str!("../src/hub/migrations/0009_handoffs.pg.sql"),
            include_str!("../src/hub/migrations/0010_work.pg.sql"),
            include_str!("../src/hub/migrations/0011_work_subscribers.pg.sql"),
            include_str!("../src/hub/migrations/0012_work_subscriber_sessions.pg.sql"),
            include_str!("../src/hub/migrations/0013_coordination_requests.pg.sql"),
            include_str!("../src/hub/migrations/0014_coordination_retention.pg.sql"),
            include_str!("../src/hub/migrations/0015_candidate_publications.pg.sql"),
            include_str!("../src/hub/migrations/0016_forge_checks.pg.sql"),
            include_str!("../src/hub/migrations/0017_external_runs.pg.sql"),
            include_str!("../src/hub/migrations/0018_hub_settings.pg.sql"),
        ]
    } else {
        [
            include_str!("../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../src/hub/migrations/0003_intents.sqlite.sql"),
            include_str!("../src/hub/migrations/0004_github.sqlite.sql"),
            include_str!("../src/hub/migrations/0005_publish.sqlite.sql"),
            include_str!("../src/hub/migrations/0006_sessions.sqlite.sql"),
            include_str!("../src/hub/migrations/0007_warn_once.sqlite.sql"),
            include_str!("../src/hub/migrations/0008_read_only_tokens.sqlite.sql"),
            include_str!("../src/hub/migrations/0009_handoffs.sqlite.sql"),
            include_str!("../src/hub/migrations/0010_work.sqlite.sql"),
            include_str!("../src/hub/migrations/0011_work_subscribers.sqlite.sql"),
            include_str!("../src/hub/migrations/0012_work_subscriber_sessions.sqlite.sql"),
            include_str!("../src/hub/migrations/0013_coordination_requests.sqlite.sql"),
            include_str!("../src/hub/migrations/0014_coordination_retention.sqlite.sql"),
            include_str!("../src/hub/migrations/0015_candidate_publications.sqlite.sql"),
            include_str!("../src/hub/migrations/0016_forge_checks.sqlite.sql"),
            include_str!("../src/hub/migrations/0017_external_runs.sqlite.sql"),
            include_str!("../src/hub/migrations/0018_hub_settings.sqlite.sql"),
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
    assert!(gone("never", 80, "branch_deleted").unwrap());
    assert_eq!(store.prune_tombstones(81).unwrap(), 3);
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
            include_str!("../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../src/hub/migrations/0003_intents.pg.sql"),
        )
    } else {
        (
            include_str!("../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../src/hub/migrations/0003_intents.sqlite.sql"),
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
    let intents = store.live_intents("acme/widgets", unix_now(), 10).unwrap();
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
            include_str!("../src/hub/migrations/0001_core.pg.sql"),
            include_str!("../src/hub/migrations/0002_login.pg.sql"),
            include_str!("../src/hub/migrations/0003_intents.pg.sql"),
            include_str!("../src/hub/migrations/0004_github.pg.sql"),
            include_str!("../src/hub/migrations/0005_publish.pg.sql"),
            include_str!("../src/hub/migrations/0006_sessions.pg.sql"),
            include_str!("../src/hub/migrations/0007_warn_once.pg.sql"),
            include_str!("../src/hub/migrations/0008_read_only_tokens.pg.sql"),
            include_str!("../src/hub/migrations/0009_handoffs.pg.sql"),
            include_str!("../src/hub/migrations/0010_work.pg.sql"),
            include_str!("../src/hub/migrations/0011_work_subscribers.pg.sql"),
            include_str!("../src/hub/migrations/0012_work_subscriber_sessions.pg.sql"),
            include_str!("../src/hub/migrations/0013_coordination_requests.pg.sql"),
            include_str!("../src/hub/migrations/0014_coordination_retention.pg.sql"),
        ]
    } else {
        [
            include_str!("../src/hub/migrations/0001_core.sqlite.sql"),
            include_str!("../src/hub/migrations/0002_login.sqlite.sql"),
            include_str!("../src/hub/migrations/0003_intents.sqlite.sql"),
            include_str!("../src/hub/migrations/0004_github.sqlite.sql"),
            include_str!("../src/hub/migrations/0005_publish.sqlite.sql"),
            include_str!("../src/hub/migrations/0006_sessions.sqlite.sql"),
            include_str!("../src/hub/migrations/0007_warn_once.sqlite.sql"),
            include_str!("../src/hub/migrations/0008_read_only_tokens.sqlite.sql"),
            include_str!("../src/hub/migrations/0009_handoffs.sqlite.sql"),
            include_str!("../src/hub/migrations/0010_work.sqlite.sql"),
            include_str!("../src/hub/migrations/0011_work_subscribers.sqlite.sql"),
            include_str!("../src/hub/migrations/0012_work_subscriber_sessions.sqlite.sql"),
            include_str!("../src/hub/migrations/0013_coordination_requests.sqlite.sql"),
            include_str!("../src/hub/migrations/0014_coordination_retention.sqlite.sql"),
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
    let p = store.publication(REPO, &id).unwrap().unwrap();
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
    assert!(store.publication(REPO, &id).is_err());
}
fn pull_publications_are_newest_per_pull_request(db: &Db) {
    let store = db.open();
    let row = |id: &str, repository: &str, pr: &str, merged: &str, created: i64| {
        format!(
            "INSERT INTO hub_publications (id,repository,token,revision,event_seq,commit_sha,branch,
                base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at)
             VALUES ('{id}','{repository}','tok_pp','{}',1,'{}','b-{id}','main','{{}}','created',{pr},
                {merged},{created},{created});",
            "a".repeat(64),
            "b".repeat(40)
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
            .pull_publications("acme/widgets", after, limit)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
    };
    // Newest per number, the merged lineage omitted, the repository matched ignoring case.
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
                branch,base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at)
             VALUES ('{id}','{repository}','tok_mp','{}',1,'{}','b-{id}','main','{{}}','created',
                1,{merged},1,{updated});",
            "a".repeat(64),
            "b".repeat(40)
        ));
    }
    db.exec(&sql).unwrap();
    let ids = |after: &str, limit: usize| {
        store
            .merged_publications("acme/widgets", after, 1, limit)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect::<Vec<_>>()
    };
    // The stale one is outside the window, so it cannot fill a page.
    assert_eq!(ids("", 50), ["m1", "m2"]);
    assert_eq!(
        store
            .merged_publications("acme/widgets", "", 0, 50)
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
                base,manifest,branch_state,pr_number,merge_commit,created_at,updated_at)
             VALUES ('{id}','Acme/Widgets','tok_fm','{}',1,'{}','b-{id}','main','{{}}','created',{pr},
                {merged},{created},{created});",
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
        .forge_checks_showing_success(&repo.id, "acme/widgets", &[], 10)
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
        source: source.into(),
        lock_json: lock.into(),
        default_branch: (source == "default_branch").then(|| "main".to_owned()),
        observed_at: 10,
        reported_by: None,
    };
    assert_eq!(store.skill_adoption(REPO).unwrap(), None);
    let checkout = store
        .skill_adoption_put(&adoption("reported_by_checkout", "{\"a\":1}"))
        .unwrap();
    assert_eq!(checkout.lock_json, "{\"a\":1}");
    // The forge replaces a checkout report; a checkout report never replaces the forge.
    let forge = store
        .skill_adoption_put(&adoption("default_branch", "{\"b\":2}"))
        .unwrap();
    assert_eq!(
        (forge.source.as_str(), forge.default_branch.as_deref()),
        ("default_branch", Some("main"))
    );
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
    token_lookup,
    expiry,
    revocation,
    owner_disable_cascades,
    rename_keeps_identity,
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
