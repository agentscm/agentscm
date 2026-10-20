//! One suite, run against SQLite always and PostgreSQL when AGENTSCM_TEST_DATABASE_URL is set.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::sync::Arc;
use std::thread;

use agentscm::hub::connection::{NewConnection, Observed};
use agentscm::hub::intent::NewIntent;
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
    assert_eq!(
        db.versions(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
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
    assert_eq!(
        db.versions(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
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
    assert_eq!(
        db.versions(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
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
    store.append_pr_notices(&[]).unwrap();
    store
        .append_pr_notices(&[(
            intent.id.clone(),
            json!({"about": "pull_request"}).to_string(),
        )])
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
    assert_eq!(
        db.versions(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    );
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
    assert_eq!(db.versions(), (1..=15).collect::<Vec<_>>());
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
    concurrent_startup,
    restart_persistence,
    events_record_each_change,
    validation,
    repository_id_round_trips,
    read_only_round_trips,
    list_and_owned_revoke,
    admin_actions_and_sign_in,
    upgrade_from_migration_1,
    connections_and_repositories,
    observations,
    deliveries_outbound_and_notices,
    upgrade_from_migration_3,
    upgrade_from_migration_14_with_publication,
    url_errors,
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
