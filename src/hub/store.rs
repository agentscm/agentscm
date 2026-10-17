//! Hub persistence: one trait, SQLite and PostgreSQL implementations.
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::connection::{
    ConnectedRepository, Connection, NewConnection, Observation, Observed, Outbound,
};
use super::handoff::{Answered, Asked};
use super::intent::{Announced, Intent, NewIntent};
use super::publish::{Create, NewPublication, Publication, Update};
use super::session::{SessionCall, Touched};
use super::store_pg::PgStore;
use super::store_sqlite::SqliteStore;
use super::{HubError, Result};
use crate::team::auth::{Secret, generate_secret, lower_hex, token_digest};

const AGENT_TOKEN_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.agent-token.v1\0";
const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 255;
const MAX_REPOSITORY_BYTES: usize = 255;
const MAX_AGENT_NAME_BYTES: usize = 64;
const MAX_EVENTS_PER_READ: usize = 1000;
pub const MAX_AGENT_TOKEN_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

/// A person on a forge. The numeric id is the identity; the login is a cached
/// display value that may change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeIdentity {
    /// `github` or `gitlab`.
    pub provider: String,
    pub host: String,
    pub user_id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// Stable portable id: `usr_` + 32 lowercase hex.
    pub id: String,
    pub identity: ForgeIdentity,
    pub login: String,
    pub disabled: bool,
}

/// Digest of an agent token secret. The only constructor is
/// [`agent_token_digest`], so a raw secret cannot be stored as a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDigest(String);

impl TokenDigest {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAgentToken {
    pub owner: String,
    pub name: String,
    pub repository: String,
    pub digest: TokenDigest,
    /// Unix seconds; the token is valid while now < expires_at.
    pub expires_at: i64,
    /// The forge's stable repository id, when the forge confirmed access at minting.
    pub repository_id: Option<i64>,
    /// May only call the read routes.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// `tok_` + 32 lowercase hex.
    pub token_id: String,
    pub name: String,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub expires_at: i64,
    pub read_only: bool,
    pub owner: User,
}

/// A token as its owner sees it; never holds the secret or digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentToken {
    pub id: String,
    pub name: String,
    pub repository: String,
    pub repository_id: Option<i64>,
    pub expires_at: i64,
    pub revoked: bool,
    pub read_only: bool,
    pub created_at: i64,
}

/// One row of the append-only event log; `data` is canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubEvent {
    pub seq: i64,
    /// `evt_` + 32 lowercase hex.
    pub id: String,
    pub at: i64,
    pub kind: String,
    pub subject: String,
    pub data: String,
}

/// A live session as the status page shows it; never its id or notice cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionView {
    /// Stable per session and not its id; see [`session_key`].
    pub key: String,
    pub token: String,
    pub agent: String,
    pub owner: String,
    pub intent: Option<String>,
    pub task: Option<String>,
    /// Canonical JSON array of connection modes.
    pub via: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An event with the agent token and owner login it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEvent {
    pub event: HubEvent,
    pub token: String,
    pub agent: String,
    pub owner: String,
}

/// The event kinds the status page shows, as an SQL list.
pub(super) const ACTIVITY_KINDS: &str = "'session_started', 'session_connected', 'session_ended',
    'edit_denied', 'edit_repeated', 'hold_expired', 'intent_announced', 'intent_released',
    'intent_notice', 'pr_notice', 'handoff_request', 'handoff_grant', 'handoff_decline'";

/// Methods block; async callers use `spawn_blocking`. Every mutation appends
/// its event in the same transaction as the state change; a no-op appends nothing.
pub trait Store: Send + Sync {
    fn backend(&self) -> &'static str;
    fn ping(&self) -> Result<()>;
    /// Insert, or update the cached login of an existing identity. Never changes the id.
    fn upsert_user(&self, identity: &ForgeIdentity, login: &str) -> Result<User>;
    fn set_user_disabled(&self, user: &str, disabled: bool) -> Result<()>;
    /// The owner must exist and be enabled, and `expires_at` must be in the future.
    fn insert_agent_token(&self, token: &NewAgentToken) -> Result<String>;
    /// True if the token was active.
    fn revoke_agent_token(&self, token: &str) -> Result<bool>;
    /// Newest first, at most 1000.
    fn list_agent_tokens(&self, owner: &str) -> Result<Vec<AgentToken>>;
    /// Like `revoke_agent_token`, but only for a token `owner` owns; the event records `by`.
    fn revoke_owned_agent_token(&self, owner: &str, token: &str) -> Result<bool>;
    /// Like `set_user_disabled`; the event records the acting user.
    fn set_user_disabled_by(&self, actor: &str, user: &str, disabled: bool) -> Result<()>;
    /// Oldest first, at most 1000.
    fn list_users(&self) -> Result<Vec<User>>;
    /// `method` is `browser` or `device`. Unknown user is `Invalid`.
    fn record_sign_in(&self, user: &str, method: &str, admin: bool) -> Result<()>;
    /// None if unknown, revoked, expired, or the owner is disabled.
    fn authenticate(&self, digest: &TokenDigest, now: i64) -> Result<Option<Principal>>;
    /// Upsert intent `intent.id` for the active token `token`; returns it and every live overlapping intent.
    fn announce_intent(&self, token: &str, intent: &NewIntent, now: i64) -> Result<Announced>;
    /// None if unknown, not the caller's, released or expired.
    fn renew_intent(
        &self,
        token: &str,
        intent: &str,
        ttl_seconds: i64,
        now: i64,
    ) -> Result<Option<Intent>>;
    /// True if it was the caller's and not already released.
    fn release_intent(&self, token: &str, intent: &str, now: i64) -> Result<bool>;
    /// Upsert the session, add `call.paths` to its intent (creating a fresh intent when it has none,
    /// or it was released), renew the intent to now + `SESSION_TTL_SECONDS`, and collect new
    /// overlaps and notices.
    fn touch_session(&self, token: &str, call: &SessionCall, now: i64) -> Result<Touched>;
    /// Release the session's intent (if any) and delete the session row. `None` resolves like
    /// `SessionCall.session`. True if a session existed.
    fn end_session(&self, token: &str, session: Option<&str>, now: i64) -> Result<bool>;
    /// Asks the live session intent holding exactly the file `path` to hand it over to the
    /// requesting session (`session` resolves like `SessionCall.session`). `note` is filtered and
    /// cut to 200 chars. One request per file and session, at most 3 open per session.
    fn ask_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        note: Option<&str>,
        now: i64,
    ) -> Result<Asked>;
    /// The holder's answer to the oldest open request for `path` addressed to the token (to its
    /// session `session`, if given). Accepting moves the file to the requester's session; a
    /// declining `reason` is filtered like a note and ignored on accept.
    fn answer_handoff(
        &self,
        token: &str,
        session: Option<&str>,
        path: &str,
        accept: bool,
        reason: Option<&str>,
        now: i64,
    ) -> Result<Answered>;
    /// Live intents in `repository`, ordered by id, at most `limit`.
    fn live_intents(&self, repository: &str, now: i64, limit: usize) -> Result<Vec<Intent>>;
    /// `intent_notice` and `pr_notice` events addressed to the token's intents with seq > `after`, oldest first, at most `limit`.
    fn notices_after(&self, token: &str, after: i64, limit: usize) -> Result<Vec<HubEvent>>;
    /// Sessions of the active tokens in `who`'s scope (same repository name and forge id, same
    /// owner host and provider) updated within `SESSION_TTL_SECONDS` of `now`, newest first, at most `limit`.
    fn live_sessions(&self, who: &Principal, now: i64, limit: usize) -> Result<Vec<SessionView>>;
    /// The status page's event kinds for the tokens and intents in `who`'s scope (as for
    /// `live_sessions`, revoked tokens included), newest first, at most `limit`.
    fn repository_events(&self, who: &Principal, limit: usize) -> Result<Vec<ActivityEvent>>;
    /// Fails with `Invalid` on a bad shape or a taken id; the event records `actor`.
    fn insert_connection(&self, new: &NewConnection, actor: &str) -> Result<Connection>;
    /// Oldest first, at most 1000.
    fn connections(&self) -> Result<Vec<Connection>>;
    fn connection(&self, id: &str) -> Result<Option<Connection>>;
    /// The `pat` connection for `host`, if any.
    fn pat_connection(&self, host: &str) -> Result<Option<Connection>>;
    /// `enabled` or `auth_failed`; an event only when status or detail changes.
    fn set_connection_status(&self, id: &str, status: &str, detail: Option<&str>) -> Result<()>;
    /// An event only when the flag changes; unknown connection is `Invalid`.
    fn set_connection_outbound(&self, id: &str, enabled: bool, actor: &str) -> Result<()>;
    /// Unix seconds; no event.
    fn set_next_poll(&self, id: &str, at: i64) -> Result<()>;
    /// Keyed by (host, repository_id): a rename updates the row. A repository already held by
    /// another connection is returned unchanged.
    fn upsert_repository(
        &self,
        connection: &str,
        host: &str,
        repository_id: i64,
        full_name: &str,
        node_id: Option<&str>,
        installation_id: Option<i64>,
    ) -> Result<ConnectedRepository>;
    /// Oldest first, at most 1000.
    fn repositories(&self, connection: &str) -> Result<Vec<ConnectedRepository>>;
    /// `full_name` compares ASCII-case-insensitively.
    fn repository_named(&self, host: &str, full_name: &str) -> Result<Option<ConnectedRepository>>;
    fn repository_by_id(
        &self,
        host: &str,
        repository_id: i64,
    ) -> Result<Option<ConnectedRepository>>;
    fn repositories_on(&self, host: &str, limit: usize) -> Result<Vec<ConnectedRepository>>;
    /// No event.
    fn set_pulls_etag(&self, repo: &str, etag: Option<&str>) -> Result<()>;
    /// True when the delivery id is new for the connection. Ids older than a week are pruned.
    fn record_delivery(&self, connection: &str, delivery: &str, now: i64) -> Result<bool>;
    /// Record what a read of the forge (started at `observed_at`, unix ms) showed.
    fn observe(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        data: &str,
        observed_at: i64,
    ) -> Result<Observed>;
    /// Record that the thing is gone, with `event` (`pull_request_closed`, `branch_deleted`
    /// or `branch_unobserved`). True when an observation was forgotten. A key never seen
    /// gets a marker and no event; so does an already-forgotten one, and a newer stored read wins.
    fn forget(
        &self,
        repo: &str,
        kind: &str,
        key: &str,
        observed_at: i64,
        data: &str,
        event: &str,
    ) -> Result<bool>;
    /// Present (not forgotten) observations of `kind`, ordered by key, at most 1000.
    fn observations(&self, repo: &str, kind: &str) -> Result<Vec<Observation>>;
    /// Drops forgotten markers last read before `before` (unix ms); no event. Returns how many.
    fn prune_tombstones(&self, before: i64) -> Result<usize>;
    fn outbound(&self, repo: &str, number: i64) -> Result<Option<Outbound>>;
    fn set_outbound_comment(
        &self,
        repo: &str,
        number: i64,
        comment_id: i64,
        body_sha256: &str,
    ) -> Result<()>;
    fn set_outbound_status(
        &self,
        repo: &str,
        number: i64,
        sha: &str,
        description: &str,
    ) -> Result<()>;
    /// `pr_notice` events as `(intent id, data JSON)`, in one transaction.
    fn append_pr_notices(&self, notices: &[(String, String)]) -> Result<()>;
    /// Idempotent by `new.id`: the first call records the publication for the active token's
    /// repository and its `publication_requested` event; a repeat with the same manifest returns the
    /// row. A token may have at most `MAX_UNFINISHED` open publications.
    fn create_publication(&self, token: &str, new: &NewPublication, now: i64) -> Result<Create>;
    fn publication(&self, repository: &str, id: &str) -> Result<Option<Publication>>;
    /// Applies one forge fact (see [`Update`]); an event only when the state changes. Unknown id is `Invalid`.
    fn update_publication(&self, id: &str, update: &Update, now: i64) -> Result<Publication>;
    /// Events with `seq` greater than the argument, oldest first, at most 1000.
    fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<HubEvent>>;
}

/// On Windows the database path may be a drive path: `C:\x`, `C:/x`, or `/C:/x` in URL form.
#[cfg(windows)]
fn windows_drive_path(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let b = path.as_bytes();
    (b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
        .then_some(path)
}

#[cfg(not(windows))]
fn windows_drive_path(_path: &str) -> Option<&str> {
    None
}

pub fn open(url: &str) -> Result<Arc<dyn Store>> {
    // Errors never echo the input: it may hold a password.
    if let Some(path) = url.strip_prefix("sqlite://") {
        let path = windows_drive_path(path).unwrap_or(path);
        let plain = (path.starts_with('/') || windows_drive_path(path).is_some())
            && path.len() > 1
            && !path.ends_with(['/', std::path::MAIN_SEPARATOR])
            && !path.contains(['?', '#', '%']);
        if !plain {
            return Err(invalid(
                "sqlite database path must be absolute, without ?, # or %, and not end in /",
            ));
        }
        Ok(Arc::new(SqliteStore::open(Path::new(path))?))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(Arc::new(PgStore::open(url)?))
    } else {
        Err(invalid(
            "AGENTSCM_DATABASE_URL must start with sqlite:// or postgres://",
        ))
    }
}

pub fn agent_token_digest(raw: &str) -> Result<TokenDigest> {
    token_digest(AGENT_TOKEN_DIGEST_DOMAIN, raw)
        .map(TokenDigest)
        .map_err(|_| HubError::Unauthorized)
}

/// Create an agent token for `owner` that lives `ttl_seconds`; the secret is
/// returned once and only its digest is stored.
pub fn mint_agent_token(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    ttl_seconds: i64,
) -> Result<(String, Secret)> {
    mint(store, owner, name, repository, None, ttl_seconds, false)
        .map(|(id, secret, _)| (id, secret))
}

/// Like [`mint_agent_token`], recording the forge's repository id; also
/// returns the stored expiry (unix seconds).
pub fn mint_bound_agent_token(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    repository_id: i64,
    ttl_seconds: i64,
    read_only: bool,
) -> Result<(String, Secret, i64)> {
    mint(
        store,
        owner,
        name,
        repository,
        Some(repository_id),
        ttl_seconds,
        read_only,
    )
}

fn mint(
    store: &dyn Store,
    owner: &str,
    name: &str,
    repository: &str,
    repository_id: Option<i64>,
    ttl_seconds: i64,
    read_only: bool,
) -> Result<(String, Secret, i64)> {
    if !(1..=MAX_AGENT_TOKEN_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(invalid("token lifetime must be 1 second to 30 days"));
    }
    let secret = generate_secret().map_err(|_| HubError::Unavailable("random source".into()))?;
    let expires_at = unix_now() + ttl_seconds;
    let id = store.insert_agent_token(&NewAgentToken {
        owner: owner.to_owned(),
        name: name.to_owned(),
        repository: repository.to_owned(),
        digest: agent_token_digest(secret.expose())?,
        expires_at,
        repository_id,
        read_only,
    })?;
    Ok((id, secret, expires_at))
}

/// 16 hex characters naming one session of a token on the status page, without exposing its id.
pub(super) fn session_key(token: &str, session: &str) -> String {
    let input = format!("agentscm.hub.session-view.v1\0{token}\0{session}");
    lower_hex_of(input.as_bytes())[..16].to_owned()
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn lower_hex_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    lower_hex(&Sha256::digest(bytes))
}

/// A new `con_` id, for a connection that needs its id before it is stored.
pub fn new_connection_id() -> Result<String> {
    new_id("con")
}

/// `<prefix>_` + 32 lowercase hex from the OS random source.
pub(super) fn new_id(prefix: &str) -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| HubError::Unavailable("random source".into()))?;
    Ok(format!("{prefix}_{}", lower_hex(&bytes)))
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

impl ForgeIdentity {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "github" | "gitlab") {
            return Err(invalid("provider must be github or gitlab"));
        }
        let host_ok = !self.host.is_empty()
            && self.host.len() <= MAX_HOST_BYTES
            && self
                .host
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':'));
        if !host_ok {
            return Err(invalid("host must be a lowercase hostname"));
        }
        if self.user_id <= 0 {
            return Err(invalid("forge user id must be positive"));
        }
        Ok(())
    }
}

impl NewAgentToken {
    pub(super) fn validate(&self, now: i64) -> Result<()> {
        validate_agent_name(&self.name)?;
        validate_repository(&self.repository)?;
        if self.expires_at <= now {
            return Err(invalid("token expiry must be in the future"));
        }
        if self.repository_id.is_some_and(|id| id <= 0) {
            return Err(invalid("repository id must be positive"));
        }
        Ok(())
    }
}

/// A state change recorded in `hub_events`, appended in the change's transaction.
pub(super) struct Event {
    pub id: String,
    pub at: i64,
    pub kind: &'static str,
    pub subject: String,
    /// Canonical JSON: serde_json orders object keys. Never holds secrets or digests.
    pub data: String,
}

impl Event {
    pub(super) fn new(kind: &'static str, subject: &str, data: Value) -> Result<Self> {
        Ok(Self {
            id: new_id("evt")?,
            at: unix_now(),
            kind,
            subject: subject.to_owned(),
            data: data.to_string(),
        })
    }

    pub(super) fn user_created(user: &str, identity: &ForgeIdentity, login: &str) -> Result<Self> {
        let data = json!({"provider": identity.provider, "host": identity.host,
            "user_id": identity.user_id, "login": login});
        Self::new("user_created", user, data)
    }

    pub(super) fn user_login_changed(user: &str, previous: &str, login: &str) -> Result<Self> {
        Self::new(
            "user_login_changed",
            user,
            json!({"login": login, "previous": previous}),
        )
    }

    /// `actor` is None for the library call and the acting user for an admin action.
    pub(super) fn user_toggled(user: &str, disabled: bool, actor: Option<&str>) -> Result<Self> {
        let kind = if disabled {
            "user_disabled"
        } else {
            "user_enabled"
        };
        Self::new(
            kind,
            user,
            actor.map_or_else(|| json!({}), |by| json!({"by": by})),
        )
    }

    pub(super) fn user_signed_in(user: &str, method: &str, admin: bool) -> Result<Self> {
        Self::new(
            "user_signed_in",
            user,
            json!({"method": method, "admin": admin}),
        )
    }

    pub(super) fn token_created(token: &str, new: &NewAgentToken) -> Result<Self> {
        let data = json!({"owner": new.owner, "name": new.name,
            "repository": new.repository, "repository_id": new.repository_id,
            "expires_at": new.expires_at, "read_only": new.read_only});
        Self::new("agent_token_created", token, data)
    }

    /// `by` is None for the library call and the owner for a self-service revoke.
    pub(super) fn token_revoked(token: &str, by: Option<&str>) -> Result<Self> {
        let data = by.map_or_else(|| json!({}), |by| json!({"by": by}));
        Self::new("agent_token_revoked", token, data)
    }
}

pub(super) fn event_limit(limit: usize) -> i64 {
    limit.min(MAX_EVENTS_PER_READ) as i64
}

pub(super) const LIST_LIMIT: i64 = 1000;

pub fn validate_login(login: &str) -> Result<()> {
    if login.is_empty() || login.len() > MAX_LOGIN_BYTES || login.chars().any(char::is_control) {
        return Err(invalid(
            "login must be 1-255 bytes without control characters",
        ));
    }
    Ok(())
}

pub fn validate_agent_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_AGENT_NAME_BYTES || !name.bytes().all(name_byte) {
        return Err(invalid("agent name must be 1-64 bytes of [A-Za-z0-9._-]"));
    }
    Ok(())
}

/// `owner/name`, or deeper for GitLab groups.
pub fn validate_repository(repository: &str) -> Result<()> {
    let segments_ok = repository.split('/').count() >= 2
        && repository.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(name_byte)
        });
    if repository.len() > MAX_REPOSITORY_BYTES || !segments_ok {
        return Err(invalid(
            "repository must be owner/name using [A-Za-z0-9._-]",
        ));
    }
    Ok(())
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

pub(super) struct Migration {
    pub version: i64,
    pub sqlite: &'static str,
    pub postgres: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sqlite: include_str!("migrations/0001_core.sqlite.sql"),
        postgres: include_str!("migrations/0001_core.pg.sql"),
    },
    Migration {
        version: 2,
        sqlite: include_str!("migrations/0002_login.sqlite.sql"),
        postgres: include_str!("migrations/0002_login.pg.sql"),
    },
    Migration {
        version: 3,
        sqlite: include_str!("migrations/0003_intents.sqlite.sql"),
        postgres: include_str!("migrations/0003_intents.pg.sql"),
    },
    Migration {
        version: 4,
        sqlite: include_str!("migrations/0004_github.sqlite.sql"),
        postgres: include_str!("migrations/0004_github.pg.sql"),
    },
    Migration {
        version: 5,
        sqlite: include_str!("migrations/0005_publish.sqlite.sql"),
        postgres: include_str!("migrations/0005_publish.pg.sql"),
    },
    Migration {
        version: 6,
        sqlite: include_str!("migrations/0006_sessions.sqlite.sql"),
        postgres: include_str!("migrations/0006_sessions.pg.sql"),
    },
    Migration {
        version: 7,
        sqlite: include_str!("migrations/0007_warn_once.sqlite.sql"),
        postgres: include_str!("migrations/0007_warn_once.pg.sql"),
    },
    Migration {
        version: 8,
        sqlite: include_str!("migrations/0008_read_only_tokens.sqlite.sql"),
        postgres: include_str!("migrations/0008_read_only_tokens.pg.sql"),
    },
    Migration {
        version: 9,
        sqlite: include_str!("migrations/0009_handoffs.sqlite.sql"),
        postgres: include_str!("migrations/0009_handoffs.pg.sql"),
    },
];

/// Migrations not yet applied. The registry must be numbered 1..n and the
/// applied set must be a prefix of it; a newer database is refused.
pub(super) fn pending(applied: &[i64]) -> Result<&'static [Migration]> {
    if MIGRATIONS
        .iter()
        .zip(1..)
        .any(|(m, version)| m.version != version)
    {
        return Err(invalid("migration registry must be numbered 1..n"));
    }
    if applied
        .iter()
        .any(|version| *version > MIGRATIONS.len() as i64)
    {
        return Err(invalid("database schema is newer than this binary"));
    }
    let mut applied = applied.to_vec();
    applied.sort_unstable();
    if applied
        .iter()
        .zip(1..)
        .any(|(version, expected)| *version != expected)
    {
        return Err(invalid(
            "applied migrations are not a prefix of this binary's",
        ));
    }
    Ok(&MIGRATIONS[applied.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_requires_an_applied_prefix() {
        assert_eq!(pending(&[]).unwrap().len(), 9);
        assert_eq!(pending(&[1]).unwrap().len(), 8);
        assert_eq!(pending(&[1, 2, 3]).unwrap().len(), 6);
        assert_eq!(pending(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap().len(), 1);
        assert_eq!(pending(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap().len(), 0);
        assert!(pending(&[2]).is_err());
        let newer = pending(&[1, 999]).err().unwrap().to_string();
        assert!(newer.contains("newer than this binary"), "{newer}");
    }
}
