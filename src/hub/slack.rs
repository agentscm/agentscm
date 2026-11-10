//! Optional Slack app that tells people, by direct message, about the moments that need a
//! person: one of their own agent sessions has waited for them, and, if they opt in, blocked
//! or handover Work. Moments between agents are never sent. Each binding links one Slack
//! user to their forge identity in one repository through a dedicated, revocable read-only
//! token. The weekly digest (`digest.rs`) lets a person release or complete their own stale
//! Work and stop their own digest; buttons on handoff messages sent by earlier versions still
//! answer for the clicking person's own agent; nothing else is written from Slack.
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::advice::{agent_name, sanitize};
use super::handoff::{Answered, Via};
use super::slack_protocol::{clip_plain_text, parse_form, plain_text, verify_request};
use super::store::session_key;
use super::store::{Principal, TokenDigest, agent_token_digest, unix_now};
use super::{HubError, HubState, Result};

pub const CONFIG_ENV: &str = "AGENTSCM_SLACK_FILE";
/// Journal rows kept at most; past this, collection waits for deliveries to drain.
const CAPACITY: i64 = 4096;
/// A message is tried at most this many times.
const MAX_ATTEMPTS: i64 = 8;
/// The retry delay `api_data` reports for a server error or network failure; `retry_delay`
/// grows it with each attempt, so an outage of Slack is waited out, not burned through.
const TRANSIENT: u64 = 5;
/// The longest wait between two tries.
const RETRY_CAP: i64 = 300;
const SENDS_PER_PASS: usize = 20;

/// Seconds before try `attempts + 2`: a server's own request (`Retry-After`) as given, a
/// transient failure doubling from 5 s up to [`RETRY_CAP`], so eight tries span about ten minutes.
fn retry_delay(delay: u64, attempts: i64) -> i64 {
    let seconds = if delay == TRANSIENT {
        (TRANSIENT as i64) << attempts.clamp(0, 6)
    } else {
        delay as i64
    };
    seconds.min(RETRY_CAP)
}
/// The moments that become direct messages without an opt-in.
const KINDS: [&str; 1] = ["session_waiting"];
const NAME_CHARS: usize = 64;
const TEXT_CHARS: usize = 120;
/// List lines a message field shows before `+N more`.
const LISTED: usize = 5;
const YES: &str = "agentscm_handoff_yes";
const NO: &str = "agentscm_handoff_no";
const OPEN: &str = "agentscm_open_hub";

mod digest;
pub mod setup;
mod status;
pub use status::ReminderPolicy;

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlackConfig {
    pub version: u32,
    pub app_id: String,
    pub bot_id: String,
    pub team_id: String,
    pub hub_url: String,
    pub signing_secret_file: PathBuf,
    pub bot_token_file: PathBuf,
    pub database: PathBuf,
    pub bindings: Vec<BindingConfig>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindingConfig {
    pub slack_user: String,
    pub provider: String,
    pub host: String,
    pub forge_user_id: i64,
    pub repository: String,
    pub repository_id: i64,
    pub agent_token_file: PathBuf,
    #[serde(default)]
    pub reminders: ReminderPolicy,
}

struct Binding {
    config: BindingConfig,
    digest: TokenDigest,
}

pub struct Slack {
    app: String,
    expected_bot: String,
    team: String,
    hub_url: String,
    signing: String,
    bot: String,
    bindings: Vec<Binding>,
    db: Mutex<Connection>,
    client: reqwest::Client,
    requests: Arc<Semaphore>,
    verified_until: tokio::sync::Mutex<i64>,
    // Fixed in production. Tests use a private loopback fake only.
    api: String,
}

async fn offload<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable())?
}

fn invalid() -> HubError {
    HubError::Invalid("invalid Slack configuration or request".into())
}

fn unavailable() -> HubError {
    HubError::Unavailable("Slack service unavailable".into())
}

fn hash(bytes: &[u8]) -> String {
    crate::team::auth::lower_hex(&Sha256::digest(bytes))
}

/// Whether `value` is a Slack ID of the kind `prefix` names (`T` workspace, `U` user...).
pub fn slack_id(value: &str, prefix: char) -> bool {
    value.starts_with(prefix)
        && (2..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Config and credentials are trusted operator inputs, never selected by Slack payloads.
/// Connected apps read their config file the same way.
pub(super) fn private_file(path: &Path, max: u64) -> Result<Vec<u8>> {
    if !path.is_absolute() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    validate_private_metadata(&file.metadata()?, max)?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid());
    }
    Ok(bytes)
}

fn validate_private_metadata(metadata: &fs::Metadata, max: u64) -> Result<()> {
    if !metadata.is_file() || metadata.len() > max {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != effective_uid() {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments, pointer access or mutable state requirements.
    unsafe { libc::geteuid() }
}

pub(super) fn secret(path: &Path) -> Result<String> {
    let bytes = private_file(path, 4096)?;
    let value = std::str::from_utf8(&bytes).map_err(|_| invalid())?.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    Ok(value.to_owned())
}

struct ValidatedConfig {
    hub_url: String,
    callback_url: String,
    signing: String,
    bot: String,
    digests: Vec<TokenDigest>,
}

fn origin(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid());
    }
    Ok(url)
}

fn validate_config(config: &SlackConfig) -> Result<ValidatedConfig> {
    if cfg!(not(unix)) {
        return Err(HubError::Invalid(
            "Slack adapter requires Unix private-file permissions".into(),
        ));
    }
    if config.version != 1
        || !slack_id(&config.app_id, 'A')
        || !slack_id(&config.bot_id, 'B')
        || !slack_id(&config.team_id, 'T')
        || config.bindings.is_empty()
        || config.bindings.len() > 32
        || !config.database.is_absolute()
    {
        return Err(invalid());
    }
    let url = origin(&config.hub_url)?;
    let hub_url = url.join("ui/").map_err(|_| invalid())?.to_string();
    let callback_url = url.join("v1/hub/slack").map_err(|_| invalid())?.to_string();
    let signing = secret(&config.signing_secret_file)?;
    let bot = secret(&config.bot_token_file)?;
    let mut users = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut binding_digests = Vec::with_capacity(config.bindings.len());
    for binding in &config.bindings {
        if !slack_id(&binding.slack_user, 'U')
            || !users.insert(binding.slack_user.clone())
            || binding.forge_user_id <= 0
            || binding.repository_id <= 0
        {
            return Err(invalid());
        }
        super::store::validate_repository(&binding.repository)?;
        binding.reminders.validate(binding, &config.bindings)?;
        let digest = agent_token_digest(&secret(&binding.agent_token_file)?)?;
        if !digests.insert(digest.as_str().to_owned()) {
            return Err(invalid());
        }
        binding_digests.push(digest);
    }
    let parent = config.database.parent().ok_or_else(invalid)?;
    let meta = fs::symlink_metadata(parent)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 || meta.uid() != effective_uid() {
            return Err(invalid());
        }
    }
    if config.database.exists() {
        let metadata = fs::symlink_metadata(&config.database)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid());
        }
        validate_private_metadata(&metadata, 128 * 1024 * 1024)?;
    }
    Ok(ValidatedConfig {
        hub_url,
        callback_url,
        signing,
        bot,
        digests: binding_digests,
    })
}

/// Plain text that keeps its line breaks: each line is clipped and control-free like
/// `plain_text`, and the whole stays within `max_chars`.
fn plain_lines(text: &str, max_chars: usize) -> Value {
    let mut out = String::new();
    for line in text.split('\n') {
        let room = max_chars.saturating_sub(out.chars().count() + usize::from(!out.is_empty()));
        if room == 0 {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&clip_plain_text(line, room));
    }
    json!({"type":"plain_text","text":out,"emoji":false})
}

/// Slack reads `<...>` in message text as links and mentions; `&` starts an escape.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn label(login: &str, agent: &str) -> String {
    format!("{}’s {}", sanitize(login, NAME_CHARS), agent_name(agent))
}

fn listed(lines: &[String]) -> String {
    let mut shown = lines.iter().take(LISTED).cloned().collect::<Vec<_>>();
    if lines.len() > LISTED {
        shown.push(format!("+{} more", lines.len() - LISTED));
    }
    shown.join("\n")
}

/// The button that opens the Hub at `url`.
fn open_button(url: &str) -> Value {
    json!({"type":"button","action_id":OPEN,"text":plain_text("Open Hub",30),"url":url})
}

/// A direct message, all plain text (never mrkdwn): a header, two-column label/value
/// fields, a button to the Hub page `link` is about, and one line of guidance. The
/// notification text is the header, followed by the first field's first line unless the
/// header already says it.
fn message(headline: &str, fields: &[(&str, String)], guidance: &str, link: &str) -> Value {
    let elements = vec![open_button(link)];
    let subject = fields
        .first()
        .and_then(|(_, value)| value.lines().next())
        .unwrap_or_default();
    let fields: Vec<Value> = fields
        .iter()
        .take(10)
        .map(|(name, value)| plain_lines(&format!("{name}\n{value}"), 2000))
        .collect();
    let text = if headline.contains(subject) {
        headline.to_owned()
    } else {
        format!("{headline}: {subject}")
    };
    json!({
        "text": escape(&clip_plain_text(&text, 300)),
        "blocks": [
            {"type":"header","text":plain_text(headline,150)},
            {"type":"section","fields":fields},
            {"type":"actions","elements":elements},
            {"type":"context","elements":[plain_text(guidance,500)]},
        ],
    })
}

/// A clicked message's header, fields and link to the Hub (or the text of an older message),
/// rebuilt as plain text; its handoff buttons and guidance are dropped for the outcome. The
/// link stays only if it still points into this Hub.
fn kept(block: &Value, hub_url: &str) -> Option<Value> {
    let text = block.pointer("/text/text").and_then(Value::as_str);
    match (block["type"].as_str()?, text) {
        ("header", Some(text)) => Some(json!({"type":"header","text":plain_text(text,150)})),
        ("section", Some(text)) => Some(json!({"type":"section","text":plain_text(text,2900)})),
        ("section", None) => {
            let fields: Vec<Value> = block["fields"]
                .as_array()?
                .iter()
                .filter_map(|field| field["text"].as_str())
                .take(10)
                .map(|text| plain_lines(text, 2000))
                .collect();
            (!fields.is_empty()).then(|| json!({"type":"section","fields":fields}))
        }
        ("actions", _) => {
            let link = block["elements"]
                .as_array()?
                .iter()
                .find(|element| element["action_id"] == OPEN)?;
            let url = link["url"].as_str().filter(|url| url.starts_with(hub_url));
            Some(json!({"type":"actions","elements":[open_button(url.unwrap_or(hub_url))]}))
        }
        _ => None,
    }
}

impl Slack {
    /// What the Hub's Slack direct messages do for `who` in their token's repository, for a
    /// read-only reader: the event kinds it sends, whether `who` has a binding, and that
    /// binding's reminder policy. Slack IDs, files and tokens never appear.
    ///
    /// It answers as a send would: `configured` is false once the app is uninstalled or its
    /// tokens revoked, and a binding counts only while its own token still authenticates for the
    /// same person, repository and read-only kind. Reads the store, so call it off the runtime.
    pub fn reader_view(&self, state: &HubState, who: &Principal) -> Value {
        if self.active().is_err() {
            return json!({"repository": who.repository, "configured": false});
        }
        let identity = &who.owner.identity;
        let binding = self
            .bindings
            .iter()
            .filter(|b| {
                let c = &b.config;
                c.provider == identity.provider
                    && c.host == identity.host
                    && c.forge_user_id == identity.user_id
                    && c.repository == who.repository
                    && Some(c.repository_id) == who.repository_id
            })
            .find(|b| {
                self.principal(state, b).is_ok_and(|live| {
                    live.owner.id == who.owner.id
                        && live.repository == who.repository
                        && live.repository_id == who.repository_id
                })
            })
            .map(|b| &b.config);
        json!({
            "repository": who.repository,
            "configured": true,
            "events": KINDS,
            "bound": binding.is_some(),
            "reminders": binding.map(|c| json!({
                "enabled": c.reminders.enabled,
                "interval_seconds": c.reminders.interval_seconds,
                "max_reminders": c.reminders.max_reminders,
                "send_resolution": c.reminders.send_resolution,
                "escalation": c.reminders.escalation_user.as_ref()
                    .map(|_| json!({"after_seconds": c.reminders.escalation_after_seconds})),
            })),
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let config: SlackConfig =
            serde_json::from_slice(&private_file(path, 64 * 1024)?).map_err(|_| invalid())?;
        Self::open(config)
    }

    fn open(config: SlackConfig) -> Result<Self> {
        let validated = validate_config(&config)?;
        let bindings = config
            .bindings
            .into_iter()
            .zip(validated.digests)
            .map(|(config, digest)| Binding { config, digest })
            .collect();
        // A separate private journal is intentionally independent of the Hub database backend.
        // The operator owns its parent directory and backups; no multiwriter deployment.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if !config.database.exists() {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&config.database)?;
            }
        }
        let db = Connection::open(&config.database)?;
        db.busy_timeout(Duration::from_millis(250))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS installation (id INTEGER PRIMARY KEY CHECK(id=1), team TEXT NOT NULL, app TEXT NOT NULL, revoked INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, user TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL, created INTEGER NOT NULL, due INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);")?;
        db.execute(
            "INSERT OR IGNORE INTO installation(id,team,app) VALUES(1,?1,?2)",
            params![config.team_id, config.app_id],
        )?;
        let identity: (String, String) =
            db.query_row("SELECT team,app FROM installation WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        if identity != (config.team_id.clone(), config.app_id.clone()) {
            return Err(invalid());
        }
        status::initialize(&db)?;
        // A send interrupted by a restart is tried again: a direct message may arrive twice.
        db.execute(
            "UPDATE messages SET state='queued' WHERE state='sending'",
            [],
        )?;
        // Earlier versions queued other direct messages; only wait messages are sent now.
        db.execute(
            "UPDATE messages SET state='failed' WHERE state='queued' AND body NOT LIKE ?1",
            [format!("%\"{}\"%", status::WAIT_HEADLINE)],
        )?;
        Ok(Self {
            app: config.app_id,
            expected_bot: config.bot_id,
            team: config.team_id,
            hub_url: validated.hub_url,
            signing: validated.signing,
            bot: validated.bot,
            bindings,
            db: Mutex::new(db),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .map_err(|_| unavailable())?,
            requests: Arc::new(Semaphore::new(8)),
            verified_until: tokio::sync::Mutex::new(0),
            api: "https://slack.com/api/".into(),
        })
    }

    fn database(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db.lock().map_err(|_| unavailable())
    }

    fn active(&self) -> Result<()> {
        let revoked: bool =
            self.database()?
                .query_row("SELECT revoked FROM installation WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        if revoked {
            Err(HubError::Unauthorized)
        } else {
            Ok(())
        }
    }

    /// The binding's token as the Hub authorizes it now: same forge identity, same
    /// repository, read-only, not revoked or expired.
    fn principal(&self, state: &HubState, binding: &Binding) -> Result<Principal> {
        let who = state
            .store
            .authenticate(&binding.digest, unix_now())?
            .ok_or(HubError::Unauthorized)?;
        let c = &binding.config;
        if who.owner.identity.provider != c.provider
            || who.owner.identity.host != c.host
            || who.owner.identity.user_id != c.forge_user_id
            || who.repository != c.repository
            || who.repository_id != Some(c.repository_id)
            || !who.read_only
        {
            return Err(HubError::Unauthorized);
        }
        Ok(who)
    }

    async fn api_data(&self, method: &str, body: Value) -> std::result::Result<Value, u64> {
        let response = self
            .client
            .post(format!("{}{method}", self.api))
            .bearer_auth(&self.bot)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(serde_json::to_vec(&body).map_err(|_| 0u64)?)
            .send()
            .await
            .map_err(|_| TRANSIENT)?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
                .clamp(1, 300));
        }
        if !response.status().is_success() {
            return Err(if response.status().is_server_error() {
                TRANSIENT
            } else {
                0
            });
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| TRANSIENT)? {
            if bytes.len() + chunk.len() > 512 * 1024 {
                return Err(0);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| 0u64)?;
        if value.get("ok") == Some(&json!(true)) {
            Ok(value)
        } else {
            Err(match value.get("error").and_then(Value::as_str) {
                Some("ratelimited" | "internal_error" | "fatal_error") => 30,
                _ => 0,
            })
        }
    }

    /// Calls `method` once the bot token is verified as the configured installation's;
    /// `Err` is the retry delay in seconds, 0 for never.
    async fn api(&self, method: &str, body: Value) -> std::result::Result<(), u64> {
        // The configured signing secret is app-scoped, whereas bot tokens are team-scoped.
        let mut verified = self.verified_until.lock().await;
        if *verified <= unix_now() {
            let identity = self.api_data("auth.test", json!({})).await?;
            if identity.get("team_id").and_then(Value::as_str) != Some(&self.team)
                || identity.get("bot_id").and_then(Value::as_str) != Some(&self.expected_bot)
                || identity
                    .get("app_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != self.app)
            {
                return Err(0);
            }
            *verified = unix_now() + 60;
        }
        drop(verified);
        self.api_data(method, body).await.map(|_| ())
    }

    /// Queues what is due: the status reminders and one message per wait. A sent message's row
    /// is the memory that stops a wait from being told twice, and is dropped after a day, which
    /// is longer than the Hub believes a wait.
    fn collect(&self, state: &HubState) -> Result<()> {
        self.active()?;
        let now = unix_now();
        self.database()?.execute(
            "DELETE FROM messages WHERE state IN ('done','failed') AND created < ?1",
            [now - 86400],
        )?;
        if self.collect_status(state, now).is_err() {
            eprintln!("Slack status collection unavailable");
        }
        if self.collect_waits(state, now).is_err() {
            eprintln!("Slack wait collection unavailable");
        }
        Ok(())
    }

    /// The Hub page at hash route `route`, as the UI's `#box-needs` anchor names it.
    fn link(&self, route: &str) -> String {
        format!("{}#{route}", self.hub_url)
    }

    fn work_link(&self, id: &str) -> String {
        let plain = !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        if plain {
            self.link(&format!("work/{id}"))
        } else {
            self.hub_url.clone()
        }
    }

    /// The oldest due message whose person is still authorized, marked as sending; a message
    /// for a revoked binding fails instead.
    fn next_message(&self, state: &HubState) -> Result<Option<(String, String, Value, i64)>> {
        self.active()?;
        let job: Option<(String, String, String, i64)> = self
            .database()?
            .query_row(
                "SELECT id,user,body,attempts FROM messages WHERE state='queued' AND due<=?1 ORDER BY created,id LIMIT 1",
                [unix_now()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, user, body, attempts)) = job else {
            return Ok(None);
        };
        let retained_binding: Option<String> = self
            .database()?
            .query_row(
                "SELECT binding FROM message_bindings WHERE message=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?;
        let authorized = {
            let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
            self.bindings
                .iter()
                .find(|b| {
                    b.config.slack_user == user
                        && retained_binding.as_deref() == Some(status::binding_key(b).as_str())
                })
                .map(|binding| self.principal(state, binding))
        };
        match authorized {
            Some(Ok(_)) => {}
            None | Some(Err(HubError::Unauthorized)) => {
                self.database()?
                    .execute("UPDATE messages SET state='failed' WHERE id=?1", [&id])?;
                return Ok(None);
            }
            Some(Err(error)) => return Err(error),
        }
        // shortcut: a queued wait message is not re-checked against its wait, so a long Slack
        // rate limit can deliver it after the person answered; store the wait key and skip
        // ended waits if that shows.
        self.database()?.execute(
            "UPDATE messages SET state='sending',attempts=attempts+1 WHERE id=?1",
            [&id],
        )?;
        let body = serde_json::from_str(&body).map_err(|_| unavailable())?;
        Ok(Some((id, user, body, attempts)))
    }

    /// Sends the next due message; `false` when none was due.
    async fn deliver(self: &Arc<Self>, state: &HubState) -> Result<bool> {
        match self.deliver_status(state).await {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(_) => eprintln!("Slack status delivery unavailable"),
        }
        let service = self.clone();
        let context = state.clone();
        let Some((id, user, mut body, attempts)) =
            offload(move || service.next_message(&context)).await?
        else {
            return Ok(false);
        };
        // A user id as the channel is the app's direct message with that user.
        body["channel"] = json!(user);
        let (status, delay) = match self.api("chat.postMessage", body).await {
            Ok(()) => ("done", 0),
            Err(delay) if delay > 0 && attempts + 1 < MAX_ATTEMPTS => {
                ("queued", retry_delay(delay, attempts))
            }
            Err(_) => ("failed", 0),
        };
        let service = self.clone();
        offload(move || {
            service.database()?.execute(
                "UPDATE messages SET state=?1,due=?2 WHERE id=?3 AND state='sending'",
                params![status, unix_now() + delay, id],
            )?;
            Ok(true)
        })
        .await
    }

    pub fn spawn_worker(self: Arc<Self>, state: HubState) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let service = self.clone();
                let context = state.clone();
                match offload(move || service.collect(&context)).await {
                    Ok(()) | Err(HubError::Unauthorized | HubError::Busy) => {}
                    Err(_) => eprintln!("Slack event collection unavailable"),
                }
                // Several sends per pass, so a burst is not paced at one message per sleep.
                let mut sent = false;
                for _ in 0..SENDS_PER_PASS {
                    match self.deliver(&state).await {
                        Ok(true) => sent = true,
                        Ok(false) | Err(HubError::Unauthorized | HubError::Busy) => break,
                        Err(_) => {
                            eprintln!("Slack delivery journal unavailable");
                            break;
                        }
                    }
                }
                // Back off when the pass delivered nothing.
                let idle = if sent { 500 } else { 2000 };
                tokio::time::sleep(Duration::from_millis(idle)).await;
            }
        })
    }

    /// A handoff button: the clicking person must own the holder token named in the button.
    /// `Ok` says what happened, in the words of the Hub's `Handed over.` and `Kept for now.`.
    fn answer(
        &self,
        state: &HubState,
        user: &str,
        value: &str,
        accept: bool,
    ) -> std::result::Result<String, Refusal> {
        self.active()?;
        let button: Value = serde_json::from_str(value).map_err(|_| invalid())?;
        let (Some(token), Some(path)) = (button["t"].as_str(), button["p"].as_str()) else {
            return Err(invalid().into());
        };
        // The session holding the file: the request was filed to it, not to the token's default.
        let session = button["s"].as_str();
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let binding = self
            .bindings
            .iter()
            .find(|b| b.config.slack_user == user)
            .ok_or(HubError::Unauthorized)?;
        let who = self.principal(state, binding)?;
        let agent = state
            .store
            .list_agent_tokens(&who.owner.id)?
            .into_iter()
            .find(|t| {
                t.id == token
                    && !t.revoked
                    && t.repository == who.repository
                    && t.repository_id == who.repository_id
            })
            .ok_or(Refusal::NotYours)?;
        let file = sanitize(path, TEXT_CHARS);
        let answered = state.store.answer_handoff_via(
            token,
            session,
            path,
            (accept, None),
            Via::Slack,
            unix_now(),
        )?;
        Ok(match answered {
            Answered::Granted { requester, .. } => format!(
                "Handed {file} to {}. They see it on their next edit.",
                label(&requester.1, &requester.0)
            ),
            Answered::Declined { requester, .. } => format!(
                "Kept {file} for now. {} sees that on their next edit.",
                label(&requester.1, &requester.0)
            ),
            Answered::NoRequest => format!(
                "Nothing changed. There is no open request for {file} any more: it was answered or lapsed."
            ),
            Answered::NotHeld => format!(
                "Nothing changed. Your {} no longer holds {file}, so there is nothing to hand over.",
                agent_name(&agent.name)
            ),
        })
    }
}

/// Why a button click did nothing, in the Hub's own sentences (`sendError` in
/// `ui/src/inboxApi.ts`): what happened and what to do next.
#[derive(Debug)]
enum Refusal {
    /// The binding's token, or the whole installation, no longer works.
    Ended,
    NotYours,
    /// A digest button that carries Work the clicking person does not own.
    NotYourWork,
    Busy,
    Rejected,
    Unavailable,
}

impl Refusal {
    fn sentence(&self) -> &'static str {
        match self {
            Self::Ended => {
                "Your Slack link to the Hub has ended. Nothing changed. Ask whoever runs this Hub to renew your read-only Slack token, or answer in the Hub."
            }
            Self::NotYours => {
                "The Hub did not accept this: it is not your agent, or it is no longer connected. Nothing changed. Answer for your own agent in the Hub."
            }
            Self::NotYourWork => {
                "The Hub did not accept this: that work is not yours. Nothing changed. Open the Hub to see whose it is."
            }
            Self::Busy => {
                "The Hub is busy right now. Nothing changed; press the button again in a moment."
            }
            Self::Rejected => {
                "The Hub did not accept that answer. Nothing changed. Answer in the Hub instead."
            }
            Self::Unavailable => {
                "The Hub could not record that answer. Nothing changed; press the button again in a moment, or answer in the Hub."
            }
        }
    }
}

impl From<HubError> for Refusal {
    fn from(error: HubError) -> Self {
        match error {
            HubError::Unauthorized => Self::Ended,
            HubError::Busy => Self::Busy,
            HubError::Invalid(_) => Self::Rejected,
            _ => Self::Unavailable,
        }
    }
}

pub fn routes() -> Router<HubState> {
    Router::new().route("/v1/hub/slack", post(ingress))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}

/// What a verified request asks for once its local work is done.
enum Prepared {
    Response(Value),
    /// Replace the clicked message's buttons with the outcome.
    Update {
        channel: String,
        ts: String,
        user: String,
        original: Vec<Value>,
        outcome: String,
    },
    /// Tell the person who clicked, in a message only they see, why it did nothing.
    Notice {
        channel: String,
        user: String,
        text: &'static str,
    },
}

async fn ingress(State(state): State<HubState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(slack) = state.slack.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let verified = match (
        header(&headers, "x-slack-request-timestamp"),
        header(&headers, "x-slack-signature"),
    ) {
        (Some(t), Some(s)) => verify_request(&slack.signing, t, s, &body, unix_now()).is_ok(),
        _ => false,
    };
    if !verified {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Dedicated admission bounds Slack waits independently of the Hub store budget.
    let Ok(permit) = slack.requests.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let context = state.clone();
    let work = async move {
        let service = slack.clone();
        let prepared = offload(move || prepare(&context, &service, &headers, &body)).await?;
        match prepared {
            Prepared::Response(value) => Ok(value),
            Prepared::Update {
                channel,
                ts,
                user,
                original,
                outcome,
            } => {
                let mut blocks = original;
                blocks.push(json!({"type":"context","elements":[plain_text(&outcome,500)]}));
                let update = json!({"channel":channel,"ts":ts,"text":escape(&outcome),
                    "blocks":blocks});
                // The answer is committed, so acknowledge now rather than spend Slack's
                // acknowledgement window on the update.
                tokio::spawn(async move {
                    // The update keeps the request's admission slot until it ends.
                    let _permit = permit;
                    if slack.api("chat.update", update).await.is_err() {
                        // The buttons are still on the message: say what happened in a new one,
                        // or a second press would find the request already answered.
                        let notice = json!({"channel":user,"text":escape(&format!(
                            "{outcome} Slack could not update the original message, so its buttons may still show."))});
                        if slack.api("chat.postMessage", notice).await.is_err() {
                            eprintln!("Slack could not report a button answer to its person");
                        }
                    }
                });
                Ok(json!({}))
            }
            Prepared::Notice {
                channel,
                user,
                text,
            } => {
                tokio::spawn(async move {
                    let _permit = permit;
                    let notice = json!({"channel":channel,"user":user,"text":text});
                    if slack.api("chat.postEphemeral", notice).await.is_err() {
                        eprintln!("Slack could not tell a person why their click did nothing");
                    }
                });
                Ok(json!({}))
            }
        }
    };
    match tokio::time::timeout(Duration::from_millis(2500), work).await {
        Ok(Ok(value)) => axum::Json(value).into_response(),
        Ok(Err(HubError::Unauthorized)) => StatusCode::FORBIDDEN.into_response(),
        Ok(Err(HubError::Invalid(_))) => StatusCode::BAD_REQUEST.into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn prepare(
    state: &HubState,
    slack: &Arc<Slack>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Prepared> {
    let content = header(headers, "content-type").unwrap_or_default();
    if content.starts_with("application/json") {
        let event: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
        if event.get("type").and_then(Value::as_str) == Some("url_verification") {
            return Ok(Prepared::Response(
                json!({"challenge":event.get("challenge").and_then(Value::as_str).filter(|s|s.len()<=1024).ok_or_else(invalid)?}),
            ));
        }
        if event.get("team_id").and_then(Value::as_str) != Some(&slack.team)
            || event.get("api_app_id").and_then(Value::as_str) != Some(&slack.app)
        {
            return Err(HubError::Unauthorized);
        }
        if event.pointer("/event/type").and_then(Value::as_str) == Some("app_mention") {
            slack.enqueue_status_query(state, &event, unix_now())?;
        }
        if let Some("app_uninstalled" | "tokens_revoked") =
            event.pointer("/event/type").and_then(Value::as_str)
        {
            let db = slack.database()?;
            db.execute("UPDATE installation SET revoked=1 WHERE id=1", [])?;
            db.execute(
                "UPDATE messages SET state='failed' WHERE state IN ('queued','sending')",
                [],
            )?;
            db.execute(
                "UPDATE status_jobs SET state='failed' WHERE state IN ('queued','sending')",
                [],
            )?;
        }
        return Ok(Prepared::Response(json!({})));
    }
    if !content.starts_with("application/x-www-form-urlencoded") {
        return Err(invalid());
    }
    let form = parse_form(body).map_err(|_| invalid())?;
    let payload: Value =
        serde_json::from_str(form.get("payload").ok_or_else(invalid)?).map_err(|_| invalid())?;
    if payload.pointer("/team/id").and_then(Value::as_str) != Some(&slack.team)
        || payload.get("api_app_id").and_then(Value::as_str) != Some(&slack.app)
    {
        return Err(HubError::Unauthorized);
    }
    if payload.get("type").and_then(Value::as_str) != Some("block_actions") {
        return Err(invalid());
    }
    let user = payload
        .pointer("/user/id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let [action] = payload
        .get("actions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(invalid)?
    else {
        return Err(invalid());
    };
    if action["action_id"].as_str().is_some_and(digest::is_button) {
        return digest::prepare_click(state, slack, &payload, user, action);
    }
    let accept = match action.get("action_id").and_then(Value::as_str) {
        Some(OPEN) => return Ok(Prepared::Response(json!({}))),
        Some(YES) => true,
        Some(NO) => false,
        _ => return Err(invalid()),
    };
    let value = action
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| v.len() <= 2000)
        .ok_or_else(invalid)?;
    let channel = payload.pointer("/channel/id").and_then(Value::as_str);
    let outcome = match slack.answer(state, user, value, accept) {
        Ok(outcome) => outcome,
        // A person, not the protocol, was refused: answer them where they clicked.
        Err(refusal) => {
            return Ok(Prepared::Notice {
                channel: channel.unwrap_or(user).to_owned(),
                user: user.to_owned(),
                text: refusal.sentence(),
            });
        }
    };
    let (Some(channel), Some(ts)) = (
        channel,
        payload.pointer("/message/ts").and_then(Value::as_str),
    ) else {
        return Ok(Prepared::Response(json!({})));
    };
    let original = payload
        .pointer("/message/blocks")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |blocks| {
            blocks
                .iter()
                .take(4)
                .filter_map(|block| kept(block, &slack.hub_url))
                .collect()
        });
    Ok(Prepared::Update {
        channel: channel.to_owned(),
        ts: ts.to_owned(),
        user: user.to_owned(),
        original,
        outcome,
    })
}

#[cfg(all(test, unix))]
mod tests;
