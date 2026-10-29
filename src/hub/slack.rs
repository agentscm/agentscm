//! Optional Slack app that tells people, by direct message as it happens, about the
//! coordination moments that need a person: an agent edited a file again after AgentSCM
//! refused it, a handoff was declined, or new work reads like work already under way.
//! Each binding links one Slack user to their forge identity in one repository through a
//! dedicated, revocable read-only token. A file's holder can answer a handoff for their own
//! agent with a button; nothing else is written from Slack.
use std::collections::{BTreeMap, BTreeSet};
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

use super::advice::sanitize;
use super::handoff::Answered;
use super::intent::{Intent, MAX_LIVE_INTENTS, key_of, overlaps};
use super::slack_protocol::{clip_plain_text, parse_form, plain_text, verify_request};
use super::store::{HubEvent, Principal, TokenDigest, agent_token_digest, unix_now};
use super::{HubError, HubState, Result};

pub const CONFIG_ENV: &str = "AGENTSCM_SLACK_FILE";
/// Journal rows kept at most; past this, collection waits for deliveries to drain.
const CAPACITY: i64 = 4096;
/// Hub events read per worker pass.
const BATCH: usize = 200;
/// A message is tried at most this many times.
const MAX_ATTEMPTS: i64 = 8;
const SENDS_PER_PASS: usize = 20;
/// The Hub events that become direct messages.
const KINDS: [&str; 3] = ["edit_repeated", "handoff_decline", "task_overlap"];
const NAME_CHARS: usize = 64;
const TEXT_CHARS: usize = 120;
/// List lines a message field shows before `+N more`.
const LISTED: usize = 5;
const YES: &str = "agentscm_handoff_yes";
const NO: &str = "agentscm_handoff_no";

pub mod setup;

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
    /// Whether the last collection found the journal full, so the log says it once.
    full: std::sync::atomic::AtomicBool,
    // Fixed in production. Tests use a private loopback fake only.
    api: String,
}

/// A bound person as the Hub authorizes them now.
struct Person {
    slack_user: String,
    login: String,
    repository: String,
    /// The binding's read-only token, for reads scoped to the repository.
    token: String,
    /// The person's agent tokens in the repository: id -> name.
    agents: BTreeMap<String, String>,
}

impl Person {
    fn label(&self, token: &str) -> String {
        label(
            &self.login,
            self.agents.get(token).map_or("agent", String::as_str),
        )
    }
}

/// One direct message to send: (journal id, Slack user, Block Kit body).
type Message = (String, String, Value);

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
fn private_file(path: &Path, max: u64) -> Result<Vec<u8>> {
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

fn secret(path: &Path) -> Result<String> {
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

/// `bob's claude-2`, filtered like advice text.
fn label(login: &str, agent: &str) -> String {
    format!(
        "{}'s {}",
        sanitize(login, NAME_CHARS),
        sanitize(agent, NAME_CHARS)
    )
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// At most `LISTED` lines, then `+N more`.
fn listed(lines: &[String]) -> String {
    let mut shown = lines.iter().take(LISTED).cloned().collect::<Vec<_>>();
    if lines.len() > LISTED {
        shown.push(format!("+{} more", lines.len() - LISTED));
    }
    shown.join("\n")
}

/// The files field of a message: `File` or `Files`, one path per line.
fn files(paths: &[String]) -> (&'static str, String) {
    let lines: Vec<String> = paths.iter().map(|p| sanitize(p, TEXT_CHARS)).collect();
    (
        if paths.len() == 1 { "File" } else { "Files" },
        listed(&lines),
    )
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
    json!({"type":"plain_text","text":out,"emoji":true})
}

/// Slack reads `<...>` in message text as links and mentions; `&` starts an escape.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A direct message, all plain text (never mrkdwn): a header, two-column label/value
/// fields, handoff buttons for `handoff` (holder token, holder session, path) beside a link
/// to the Hub, and one line of guidance. The notification text is the header and the first
/// field's first line.
fn message(
    headline: &str,
    fields: &[(&str, String)],
    guidance: &str,
    handoff: Option<(&str, Option<&str>, &str)>,
    hub_url: &str,
) -> Value {
    let mut elements = Vec::new();
    // Slack refuses a whole message whose button value passes 2000 characters; send it without buttons.
    let value = handoff
        .map(|(token, session, path)| json!({"t": token, "s": session, "p": path}).to_string())
        .filter(|value| value.chars().count() <= 2000);
    if let Some(value) = value {
        elements.push(json!({"type":"button","action_id":YES,"style":"primary",
            "text":plain_text("Hand it over",30),"value":value}));
        elements.push(json!({"type":"button","action_id":NO,
            "text":plain_text("Keep it",30),"value":value}));
    }
    elements.push(json!({"type":"button","action_id":"agentscm_open_hub",
        "text":plain_text("Open Hub",30),"url":hub_url}));
    let subject = fields
        .first()
        .and_then(|(_, value)| value.lines().next())
        .unwrap_or_default();
    let fields: Vec<Value> = fields
        .iter()
        .take(10)
        .map(|(name, value)| plain_lines(&format!("{name}\n{value}"), 2000))
        .collect();
    json!({
        "text": escape(&clip_plain_text(&format!("{headline}: {subject}"), 300)),
        "blocks": [
            {"type":"header","text":plain_text(headline,150)},
            {"type":"section","fields":fields},
            {"type":"actions","elements":elements},
            {"type":"context","elements":[plain_text(guidance,500)]},
        ],
    })
}

/// A clicked message's header and fields (or the text of an older message), rebuilt as
/// plain text; its buttons and guidance are dropped for the outcome.
fn kept(block: &Value) -> Option<Value> {
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
        _ => None,
    }
}

impl Slack {
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
            CREATE TABLE IF NOT EXISTS cursor (id INTEGER PRIMARY KEY CHECK(id=1), seq INTEGER NOT NULL);
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
        // A send interrupted by a restart is tried again: a direct message may arrive twice.
        db.execute(
            "UPDATE messages SET state='queued' WHERE state='sending'",
            [],
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
            full: std::sync::atomic::AtomicBool::new(false),
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

    fn person(&self, state: &HubState, binding: &Binding) -> Result<Person> {
        let who = self.principal(state, binding)?;
        let agents = state
            .store
            .list_agent_tokens(&who.owner.id)?
            .into_iter()
            .filter(|t| t.repository == who.repository && t.repository_id == who.repository_id)
            .map(|t| (t.id, t.name))
            .collect();
        Ok(Person {
            slack_user: binding.config.slack_user.clone(),
            login: who.owner.login,
            repository: who.repository,
            token: who.token_id,
            agents,
        })
    }

    /// Every bound person the Hub still authorizes; a revoked or changed binding is skipped.
    fn people(&self, state: &HubState) -> Result<Vec<Person>> {
        let mut people = Vec::new();
        for binding in &self.bindings {
            match self.person(state, binding) {
                Ok(person) => people.push(person),
                Err(HubError::Unauthorized) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(people)
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
            .map_err(|_| 5u64)?;
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
                5
            } else {
                0
            });
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| 5u64)? {
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

    /// Turns Hub events after the cursor into queued messages and advances the cursor. The
    /// first pass starts at the newest event: history from before installation is never sent.
    fn collect(&self, state: &HubState) -> Result<()> {
        self.active()?;
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let cursor: Option<i64> = self
            .database()?
            .query_row("SELECT seq FROM cursor WHERE id=1", [], |r| r.get(0))
            .optional()?;
        let Some(cursor) = cursor else {
            let latest = state.store.latest_event_seq()?;
            self.database()?
                .execute("INSERT INTO cursor(id,seq) VALUES(1,?1)", [latest])?;
            return Ok(());
        };
        let events = state.store.events_after(cursor, BATCH)?;
        if events.is_empty() {
            // A restored Hub database can end below the cursor; restart from its newest
            // event, or every event until it passes the old cursor would be skipped.
            let latest = state.store.latest_event_seq()?;
            if latest < cursor {
                self.database()?
                    .execute("UPDATE cursor SET seq=?1 WHERE id=1", [latest])?;
            }
            return Ok(());
        }
        let people = if events.iter().any(|e| KINDS.contains(&e.kind.as_str())) {
            self.people(state)?
        } else {
            Vec::new()
        };
        let mut batches = Vec::new();
        for event in &events {
            let messages = if KINDS.contains(&event.kind.as_str()) {
                self.messages_for(state, &people, event)?
            } else {
                Vec::new()
            };
            batches.push((event.seq, messages));
        }
        let now = unix_now();
        let mut db = self.database()?;
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE state IN ('done','failed') AND created < ?1",
            [now - 86400],
        )?;
        // Sent rows only remember what was sent; the cap bounds what is still waiting.
        let mut waiting: i64 = tx.query_row(
            "SELECT count(*) FROM messages WHERE state IN ('queued','sending')",
            [],
            |r| r.get(0),
        )?;
        let mut reached = None;
        for (seq, messages) in batches {
            if waiting + messages.len() as i64 > CAPACITY {
                break;
            }
            waiting += messages.len() as i64;
            for (id, user, body) in messages {
                tx.execute(
                    "INSERT OR IGNORE INTO messages(id,user,body,state,created,due) VALUES(?1,?2,?3,'queued',?4,?4)",
                    params![id, user, body.to_string(), now],
                )?;
            }
            reached = Some(seq);
        }
        // Events that do not fit wait at the cursor until sends make room.
        let full = reached != events.last().map(|event| event.seq);
        if full != self.full.swap(full, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "{}",
                if full {
                    "Slack message journal is full; new coordination messages wait until queued ones are sent"
                } else {
                    "Slack message journal has room again"
                }
            );
        }
        let Some(reached) = reached else {
            return Err(HubError::Busy);
        };
        tx.execute("UPDATE cursor SET seq=?1 WHERE id=1", [reached])?;
        tx.commit()?;
        Ok(())
    }

    /// The messages one event asks for. Malformed event data asks for none; only a store
    /// failure is an error, so the event is read again on the next pass.
    fn messages_for(
        &self,
        state: &HubState,
        people: &[Person],
        event: &HubEvent,
    ) -> Result<Vec<Message>> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Ok(Vec::new());
        };
        let messages = match event.kind.as_str() {
            "edit_repeated" => self.repeated(state, people, &event.subject, &data)?,
            "handoff_decline" => self.declined(state, people, &event.subject, &data)?,
            "task_overlap" => self.similar(state, people, &event.subject, &data)?,
            _ => Vec::new(),
        };
        Ok(messages
            .into_iter()
            .map(|(user, key, body)| {
                (
                    hash(format!("{}\n{user}\n{key}", event.id).as_bytes()),
                    user,
                    body,
                )
            })
            .collect())
    }

    /// The repositories an event by `token` can be about, with their live intents: the
    /// bound repository of its owner, or every bound repository where it holds an intent.
    fn repositories(
        state: &HubState,
        people: &[Person],
        token: &str,
    ) -> Result<Vec<(String, Vec<Intent>)>> {
        let bound = people.iter().find(|p| p.agents.contains_key(token));
        let names: BTreeSet<&str> = match bound {
            Some(person) => [person.repository.as_str()].into(),
            None => people.iter().map(|p| p.repository.as_str()).collect(),
        };
        let mut found = Vec::new();
        for name in names {
            let intents = state
                .store
                .live_intents(name, unix_now(), MAX_LIVE_INTENTS)?;
            if bound.is_some() || intents.iter().any(|i| i.token == token) {
                found.push((name.to_owned(), intents));
            }
        }
        Ok(found)
    }

    fn bound<'a>(people: &'a [Person], repository: &str, token: &str) -> Option<&'a Person> {
        people
            .iter()
            .find(|p| p.repository == repository && p.agents.contains_key(token))
    }

    /// `edit_repeated`: the repeating agent's person and each holder's person.
    fn repeated(
        &self,
        state: &HubState,
        people: &[Person],
        token: &str,
        data: &Value,
    ) -> Result<Vec<(String, String, Value)>> {
        let paths = strings(&data["paths"]);
        let mut out = Vec::new();
        for (repository, intents) in Self::repositories(state, people, token)? {
            let repeater = Self::bound(people, &repository, token);
            let who = intents
                .iter()
                .find(|i| i.token == token)
                .map(|i| label(&i.owner.login, &i.agent))
                .or_else(|| repeater.map(|p| p.label(token)))
                .unwrap_or_else(|| "another agent".into());
            let mut holders = Vec::new();
            for intent in intents.iter().filter(|i| i.token != token) {
                let held: Vec<String> = paths
                    .iter()
                    .filter(|path| intent.paths.iter().any(|spec| overlaps(path, key_of(spec))))
                    .cloned()
                    .collect();
                if held.is_empty() {
                    continue;
                }
                let holder = label(&intent.owner.login, &intent.agent);
                if let Some(person) = Self::bound(people, &repository, &intent.token)
                    && repeater.is_none_or(|r| r.slack_user != person.slack_user)
                {
                    // One message per holder and event; buttons answer one file only.
                    let (headline, guidance) = if held.len() == 1 {
                        (
                            "Another agent edited a file your agent holds",
                            "Hand it over, or keep it and agree who goes first.",
                        )
                    } else {
                        (
                            "Another agent edited files your agent holds",
                            "Hand them over, or keep them and agree who goes first.",
                        )
                    };
                    let fields = [
                        files(&held),
                        ("Repository", sanitize(&repository, TEXT_CHARS)),
                        ("Your agent", sanitize(&intent.agent, NAME_CHARS)),
                        ("Their agent", who.clone()),
                    ];
                    let handoff = match held.as_slice() {
                        [path] => Some((
                            intent.token.as_str(),
                            intent.session.as_deref(),
                            path.as_str(),
                        )),
                        _ => None,
                    };
                    out.push((
                        person.slack_user.clone(),
                        format!("holder\n{}", intent.token),
                        message(headline, &fields, guidance, handoff, &self.hub_url),
                    ));
                }
                holders.push(holder);
            }
            if let Some(person) = repeater
                && !holders.is_empty()
            {
                let fields = [
                    files(&paths),
                    ("Repository", sanitize(&repository, TEXT_CHARS)),
                    (
                        "Your agent",
                        sanitize(
                            person.agents.get(token).map_or("agent", String::as_str),
                            NAME_CHARS,
                        ),
                    ),
                    (
                        if holders.len() == 1 {
                            "Their agent"
                        } else {
                            "Their agents"
                        },
                        listed(&holders),
                    ),
                ];
                out.push((
                    person.slack_user.clone(),
                    "repeater".into(),
                    message(
                        if paths.len() == 1 {
                            "Your agent edited a file another agent holds"
                        } else {
                            "Your agent edited files another agent holds"
                        },
                        &fields,
                        "Agree with them who goes first.",
                        None,
                        &self.hub_url,
                    ),
                ));
            }
        }
        Ok(out)
    }

    /// `handoff_decline` (subject: the holder's token): both people.
    fn declined(
        &self,
        state: &HubState,
        people: &[Person],
        holder: &str,
        data: &Value,
    ) -> Result<Vec<(String, String, Value)>> {
        let paths = strings(&data["paths"]);
        let from = &data["from"];
        let (Some(requester), Some(agent), Some(login)) = (
            from["token"].as_str(),
            from["agent"].as_str(),
            from["login"].as_str(),
        ) else {
            return Ok(Vec::new());
        };
        let reason = data["reason"]
            .as_str()
            .map(|reason| sanitize(reason, TEXT_CHARS));
        let fields = |repository: &str, mine: String, theirs: String| {
            let mut fields = vec![
                files(&paths),
                ("Repository", sanitize(repository, TEXT_CHARS)),
                ("Your agent", mine),
                ("Their agent", theirs),
            ];
            fields.extend(reason.clone().map(|reason| ("Reason", reason)));
            fields
        };
        let mut out = Vec::new();
        for (repository, intents) in Self::repositories(state, people, holder)? {
            let holder_person = Self::bound(people, &repository, holder);
            let holder_label = holder_person
                .map(|p| p.label(holder))
                .or_else(|| {
                    intents
                        .iter()
                        .find(|i| i.token == holder)
                        .map(|i| label(&i.owner.login, &i.agent))
                })
                .unwrap_or_else(|| "the holder".into());
            let requester_person = Self::bound(people, &repository, requester);
            if let Some(person) = requester_person {
                let fields = fields(&repository, sanitize(agent, NAME_CHARS), holder_label);
                out.push((
                    person.slack_user.clone(),
                    "requester".into(),
                    message(
                        "Another agent kept a file your agent asked for",
                        &fields,
                        "Agree with them who goes first.",
                        None,
                        &self.hub_url,
                    ),
                ));
            }
            if let Some(person) = holder_person
                && requester_person.is_none_or(|r| r.slack_user != person.slack_user)
            {
                let fields = fields(
                    &repository,
                    sanitize(
                        person.agents.get(holder).map_or("agent", String::as_str),
                        NAME_CHARS,
                    ),
                    label(login, agent),
                );
                out.push((
                    person.slack_user.clone(),
                    "holder".into(),
                    message(
                        "Your agent kept a file",
                        &fields,
                        "Agree with them who goes first.",
                        None,
                        &self.hub_url,
                    ),
                ));
            }
        }
        Ok(out)
    }

    /// `task_overlap` (subject: the starting token): the starter's person and the person
    /// leading each matched work item.
    fn similar(
        &self,
        state: &HubState,
        people: &[Person],
        starter: &str,
        data: &Value,
    ) -> Result<Vec<(String, String, Value)>> {
        let Some(task) = data["task"].as_str() else {
            return Ok(Vec::new());
        };
        let ids = strings(&data["work"]);
        let pulls: Vec<i64> = data["pulls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let mut out = Vec::new();
        let mut done = BTreeSet::new();
        for reader in people {
            if !done.insert(reader.repository.as_str()) {
                continue;
            }
            let repository = &reader.repository;
            let now = unix_now();
            let mut items = Vec::new();
            for id in &ids {
                if let Some(item) = state.store.work_get(&reader.token, id, now)? {
                    items.push(item);
                }
            }
            let starter_person = Self::bound(people, repository, starter);
            if starter_person.is_none() && items.is_empty() {
                continue;
            }
            let starter_label = starter_person.map(|p| p.label(starter)).or_else(|| {
                state
                    .store
                    .work_overview(&reader.token, &Default::default(), now)
                    .ok()?
                    .current
                    .into_iter()
                    .find(|item| item.lead_token == starter)
                    .map(|item| label(&item.owner_login, &item.lead_agent))
            });
            let starter_label = starter_label.unwrap_or_else(|| "another agent".into());
            let mut like: Vec<String> = items
                .iter()
                .map(|item| {
                    format!(
                        "{}: {}",
                        label(&item.owner_login, &item.lead_agent),
                        sanitize(&item.title, TEXT_CHARS)
                    )
                })
                .collect();
            like.extend(pulls.iter().map(|n| format!("Open pull request #{n}")));
            let task = sanitize(task, TEXT_CHARS);
            let repository_name = sanitize(repository, TEXT_CHARS);
            if let Some(person) = starter_person
                && !like.is_empty()
            {
                let fields = [
                    ("Task", task.clone()),
                    ("Repository", repository_name.clone()),
                    (
                        "Your agent",
                        sanitize(
                            person.agents.get(starter).map_or("agent", String::as_str),
                            NAME_CHARS,
                        ),
                    ),
                    ("Reads like", listed(&like)),
                ];
                out.push((
                    person.slack_user.clone(),
                    "starter".into(),
                    message(
                        "Your agent started similar work",
                        &fields,
                        "Check whether it is the same work.",
                        None,
                        &self.hub_url,
                    ),
                ));
            }
            for item in &items {
                let Some(person) = Self::bound(people, repository, &item.lead_token) else {
                    continue;
                };
                if starter_person.is_some_and(|s| s.slack_user == person.slack_user) {
                    continue;
                }
                let fields = [
                    ("Task", task.clone()),
                    ("Repository", repository_name.clone()),
                    ("Their agent", starter_label.clone()),
                    ("Your agent", sanitize(&item.lead_agent, NAME_CHARS)),
                    ("Your work", sanitize(&item.title, TEXT_CHARS)),
                ];
                out.push((
                    person.slack_user.clone(),
                    format!("lead\n{}", item.id),
                    message(
                        "Similar work started",
                        &fields,
                        "Check with them whether it is the same work.",
                        None,
                        &self.hub_url,
                    ),
                ));
            }
        }
        Ok(out)
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
        let authorized = {
            let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
            self.bindings
                .iter()
                .find(|b| b.config.slack_user == user)
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
        self.database()?.execute(
            "UPDATE messages SET state='sending',attempts=attempts+1 WHERE id=?1",
            [&id],
        )?;
        let body = serde_json::from_str(&body).map_err(|_| unavailable())?;
        Ok(Some((id, user, body, attempts)))
    }

    /// Sends the next due message; `false` when none was due.
    async fn deliver(self: &Arc<Self>, state: &HubState) -> Result<bool> {
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
            Err(delay) if delay > 0 && attempts + 1 < MAX_ATTEMPTS => ("queued", delay as i64),
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
    fn answer(&self, state: &HubState, user: &str, value: &str, accept: bool) -> Result<String> {
        self.active()?;
        let button: Value = serde_json::from_str(value).map_err(|_| invalid())?;
        let (Some(token), Some(path)) = (button["t"].as_str(), button["p"].as_str()) else {
            return Err(invalid());
        };
        // The session holding the file: the request was filed to it, not to the token's default.
        let session = button["s"].as_str();
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let binding = self
            .bindings
            .iter()
            .find(|b| b.config.slack_user == user)
            .ok_or(HubError::Unauthorized)?;
        let person = self.person(state, binding)?;
        if !person.agents.contains_key(token) {
            return Err(HubError::Unauthorized);
        }
        Ok(
            match state
                .store
                .answer_handoff(token, session, path, accept, None, unix_now())?
            {
                Answered::Granted { requester } => {
                    format!("Handed to {}", label(&requester.1, &requester.0))
                }
                Answered::Declined { requester } => format!(
                    "Kept; {} is told on its next call",
                    label(&requester.1, &requester.0)
                ),
                Answered::NoRequest => "No open request: it was answered or lapsed".into(),
                Answered::NotHeld => "Your agent no longer holds this file".into(),
            },
        )
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
        original: Vec<Value>,
        outcome: String,
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
                original,
                outcome,
            } => {
                let mut blocks = original;
                blocks.push(json!({"type":"context","elements":[plain_text(&outcome,500)]}));
                let update = json!({"channel":channel,"ts":ts,"text":escape(&outcome),
                    "blocks":blocks});
                // The answer is committed, so acknowledge now rather than spend Slack's
                // acknowledgement window on the update; a failed update only leaves the buttons.
                tokio::spawn(async move {
                    // The update keeps the request's admission slot until it ends.
                    let _permit = permit;
                    let _ = slack.api("chat.update", update).await;
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
        if let Some("app_uninstalled" | "tokens_revoked") =
            event.pointer("/event/type").and_then(Value::as_str)
        {
            let db = slack.database()?;
            db.execute("UPDATE installation SET revoked=1 WHERE id=1", [])?;
            db.execute(
                "UPDATE messages SET state='failed' WHERE state IN ('queued','sending')",
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
    let accept = match action.get("action_id").and_then(Value::as_str) {
        Some("agentscm_open_hub") => return Ok(Prepared::Response(json!({}))),
        Some(YES) => true,
        Some(NO) => false,
        _ => return Err(invalid()),
    };
    let value = action
        .get("value")
        .and_then(Value::as_str)
        .filter(|v| v.len() <= 2000)
        .ok_or_else(invalid)?;
    let outcome = slack.answer(state, user, value, accept)?;
    let (Some(channel), Some(ts)) = (
        payload.pointer("/channel/id").and_then(Value::as_str),
        payload.pointer("/message/ts").and_then(Value::as_str),
    ) else {
        return Ok(Prepared::Response(json!({})));
    };
    let original = payload
        .pointer("/message/blocks")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |blocks| {
            blocks.iter().take(4).filter_map(kept).collect()
        });
    Ok(Prepared::Update {
        channel: channel.to_owned(),
        ts: ts.to_owned(),
        original,
        outcome,
    })
}

#[cfg(all(test, unix))]
mod tests;
