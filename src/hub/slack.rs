//! Optional Slack client of the existing Hub coordination services.
//! Slack identity is operator-linked to a dedicated, revocable scoped credential.
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

use super::slack_protocol::{parse_form, plain_text, verify_request};
use super::store::{Principal, TokenDigest, agent_token_digest, unix_now};
use super::work::{CheckpointKind, WorkCheckpoint, WorkRole, WorkStart};
use super::{HubError, HubState, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const CONFIG_ENV: &str = "AGENTSCM_SLACK_FILE";
const SESSION: &str = "slack-human";
const CAPACITY: i64 = 4096;

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
    #[serde(default)]
    pub allow_coordination_writes: bool,
}

struct Binding {
    config: BindingConfig,
    digest: TokenDigest,
    requests: Arc<Semaphore>,
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

struct ActionRow {
    user: String,
    binding: String,
    kind: String,
    work: Option<String>,
    generation: Option<i64>,
    seq: Option<i64>,
    expires: i64,
    request_hash: Option<String>,
    state: String,
    outcome: String,
}

#[derive(Debug, PartialEq, Eq)]
enum SubmitOutcome {
    Saved,
    Coordination(String),
    Conflict,
}

impl SubmitOutcome {
    fn code(&self) -> String {
        match self {
            Self::Saved => "work".into(),
            Self::Coordination(id) => format!("coordination:{id}"),
            Self::Conflict => "conflict".into(),
        }
    }
    fn restored(code: &str) -> Result<Self> {
        match code {
            "work" => Ok(Self::Saved),
            "conflict" => Ok(Self::Conflict),
            s if s.starts_with("coordination:") => Ok(Self::Coordination(s[13..].to_owned())),
            _ => Err(unavailable()),
        }
    }
    fn page(&self) -> &str {
        if matches!(self, Self::Saved) {
            "work"
        } else {
            "coordination"
        }
    }
}

enum Submission {
    Start(WorkStart),
    Checkpoint(WorkCheckpoint),
}

enum Prepared {
    Response(Value),
    Modal {
        payload: Value,
        response: Value,
        _user_slot: OwnedSemaphorePermit,
    },
}

struct Delivery {
    id: String,
    user: String,
    binding: String,
    attempts: i64,
    view: Result<Value>,
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

fn slack_id(value: &str, prefix: char) -> bool {
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

impl Slack {
    pub fn from_file(path: &Path) -> Result<Self> {
        let config: SlackConfig =
            serde_json::from_slice(&private_file(path, 64 * 1024)?).map_err(|_| invalid())?;
        Self::open(config)
    }

    fn open(config: SlackConfig) -> Result<Self> {
        let validated = validate_config(&config)?;
        let mut bindings = Vec::new();
        for (binding, digest) in config.bindings.into_iter().zip(validated.digests) {
            bindings.push(Binding {
                config: binding,
                digest,
                requests: Arc::new(Semaphore::new(1)),
            });
        }
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
            CREATE TABLE IF NOT EXISTS deliveries (id TEXT PRIMARY KEY, user TEXT NOT NULL, binding TEXT NOT NULL, page TEXT NOT NULL, state TEXT NOT NULL, created INTEGER NOT NULL, due INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS private_views (user TEXT NOT NULL, binding TEXT NOT NULL, PRIMARY KEY(user,binding));
            CREATE TABLE IF NOT EXISTS actions (id TEXT PRIMARY KEY, user TEXT NOT NULL, binding TEXT NOT NULL, kind TEXT NOT NULL, work TEXT, generation INTEGER, seq INTEGER, expires INTEGER NOT NULL, request_hash TEXT, state TEXT NOT NULL DEFAULT 'open', outcome TEXT NOT NULL DEFAULT '');")?;
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
        db.execute(
            "UPDATE deliveries SET state='queued' WHERE state='sending'",
            [],
        )?;
        db.execute(
            "UPDATE actions SET state='uncertain' WHERE state='running'",
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

    fn user_slot(&self, user: &str) -> Result<OwnedSemaphorePermit> {
        self.bindings
            .iter()
            .find(|b| b.config.slack_user == user)
            .ok_or(HubError::Unauthorized)?
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| HubError::Busy)
    }

    fn principal(&self, state: &HubState, user: &str, write: bool) -> Result<Principal> {
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        self.active()?;
        let binding = self
            .bindings
            .iter()
            .find(|b| b.config.slack_user == user)
            .ok_or(HubError::Unauthorized)?;
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
            || (write && !c.allow_coordination_writes)
        {
            return Err(HubError::Unauthorized);
        }
        Ok(who)
    }

    fn fingerprint(who: &Principal) -> String {
        hash(
            format!(
                "{}:{}:{}:{:?}",
                who.owner.id, who.token_id, who.repository, who.repository_id
            )
            .as_bytes(),
        )
    }

    fn enqueue(&self, id: &str, user: &str, who: &Principal, page: &str) -> Result<()> {
        let db = self.database()?;
        let now = unix_now();
        db.execute("DELETE FROM deliveries WHERE created < ?1", [now - 86400])?;
        if db.query_row::<i64, _, _>("SELECT count(*) FROM deliveries", [], |r| r.get(0))?
            >= CAPACITY
        {
            return Err(HubError::Busy);
        }
        let inserted=db.execute("INSERT OR IGNORE INTO deliveries(id,user,binding,page,state,created,due) VALUES(?1,?2,?3,?4,'queued',?5,?5)", params![id,user,Self::fingerprint(who),page,now])?;
        if inserted > 0 {
            db.execute(
                "UPDATE deliveries SET state='done' WHERE user=?1 AND state IN ('queued','sending') AND id!=?2",
                params![user, id],
            )?;
        }
        Ok(())
    }

    fn action(
        &self,
        user: &str,
        who: &Principal,
        work: Option<&super::work::WorkItem>,
    ) -> Result<String> {
        let db = self.database()?;
        let now = unix_now();
        db.execute("DELETE FROM actions WHERE expires < ?1", [now])?;
        if db.query_row::<i64, _, _>("SELECT count(*) FROM actions", [], |r| r.get(0))? >= CAPACITY
        {
            return Err(HubError::Busy);
        }
        let mut random = [0; 32];
        getrandom::fill(&mut random).map_err(|_| unavailable())?;
        let id = hash(&random);
        db.execute("INSERT INTO actions(id,user,binding,kind,work,generation,seq,expires) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id,user,Self::fingerprint(who),if work.is_some(){"checkpoint"}else{"start"},work.map(|w|&w.id),work.map(|w|w.generation),work.map(|w|w.updated_seq),now+600])?;
        Ok(id)
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

    async fn api(&self, method: &str, body: Value) -> std::result::Result<(), u64> {
        // Verify the bot token's actual installation before any outbound view. The
        // configured signing secret is app-scoped, whereas bot tokens are team-scoped.
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

    fn modal(
        &self,
        state: &HubState,
        user: &str,
        trigger: &str,
        work: Option<&str>,
    ) -> Result<Value> {
        let who = self.principal(state, user, true)?;
        let item = {
            let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
            match work {
                Some(id) => Some(
                    state
                        .store
                        .work_get(&who.token_id, id, unix_now())?
                        .ok_or(HubError::Unauthorized)?,
                ),
                None => None,
            }
        };
        if item.as_ref().is_some_and(|w| {
            w.lead_token != who.token_id
                || w.lead_session != SESSION
                || !matches!(
                    w.state,
                    super::work::WorkState::Active | super::work::WorkState::Blocked
                )
        }) {
            return Err(HubError::Unauthorized);
        }
        let nonce = self.action(user, &who, item.as_ref())?;
        let mut blocks = vec![
            json!({"type":"section","text":plain_text("This form writes an AgentSCM work record under your dedicated Slack coordination identity. Do not paste Slack conversations, credentials or private message content. This does not start or stop a coding agent.",1000)}),
        ];
        if let Some(item) = &item {
            blocks.push(json!({"type":"section","text":plain_text(&format!("{} · generation {} · update {}",item.title,item.generation,item.updated_seq),1000)}));
            blocks.push(json!({"type":"input","block_id":"kind","label":plain_text("Checkpoint",50),"element":{"type":"static_select","action_id":"value","options":(["progress","decision","blocker","pause","finish","handover"].map(|k|json!({"text":plain_text(k,30),"value":k})))}}));
            blocks.push(input("summary", "Summary", false, false));
            blocks.push(json!({"type":"input","block_id":"resolve","optional":true,"label":plain_text("Resolve the recorded blocker?",100),"element":{"type":"static_select","action_id":"value","options":[{"text":plain_text("Keep blocker",50),"value":"no"},{"text":plain_text("Resolve blocker",50),"value":"yes"}]}}));
        } else {
            blocks.push(input("title", "Work title", false, false));
            blocks.push(input(
                "scope",
                "Repository-relative scope (one path per line)",
                true,
                true,
            ));
        }
        blocks.push(input("next", "Next action", true, false));
        let view = json!({"type":"modal","callback_id":"agentscm_submit","private_metadata":nonce,"title":plain_text(if item.is_some(){"Record checkpoint"}else{"Create scoped work"},24),"submit":plain_text("Save to AgentSCM",24),"close":plain_text("Cancel",24),"blocks":blocks});
        Ok(json!({"trigger_id":trigger,"view":view}))
    }

    fn submission(
        who: &Principal,
        nonce: &str,
        row: &ActionRow,
        values: &Value,
    ) -> Result<Submission> {
        let next = field(values, "next").filter(|s| !s.is_empty());
        if let Some(next) = &next {
            super::intent::text("next action", next, 500)
                .map_err(|_| HubError::Invalid("slack_form_next".into()))?;
        }
        if row.kind == "start" {
            let title = field(values, "title")
                .ok_or_else(|| HubError::Invalid("slack_form_title".into()))?;
            super::intent::text("title", &title, 500)
                .map_err(|_| HubError::Invalid("slack_form_title".into()))?;
            let scope = field(values, "scope")
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            let request = WorkStart {
                work_id: None,
                source: None,
                request_key: Some(format!("slack-{nonce}")),
                title,
                session: SESSION.into(),
                role: WorkRole::Lead,
                scope,
                branch: None,
                worktree: None,
                next_action: next,
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
            };
            let validate_as = super::work::WorkWho {
                token: who.token_id.clone(),
                repository: who.repository.clone(),
                repository_id: who.repository_id,
                agent: who.name.clone(),
                read_only: who.read_only,
                owner: who.owner.clone(),
            };
            request
                .validate(&validate_as)
                .map_err(|_| HubError::Invalid("slack_form_scope".into()))?;
            Ok(Submission::Start(request))
        } else if row.kind == "checkpoint" {
            let kind: CheckpointKind = serde_json::from_value(
                values
                    .pointer("/kind/value/selected_option/value")
                    .cloned()
                    .ok_or_else(|| HubError::Invalid("slack_form_kind".into()))?,
            )
            .map_err(|_| HubError::Invalid("slack_form_kind".into()))?;
            let summary = field(values, "summary")
                .ok_or_else(|| HubError::Invalid("slack_form_summary".into()))?;
            super::intent::text("summary", &summary, 500)
                .map_err(|_| HubError::Invalid("slack_form_summary".into()))?;
            let request = WorkCheckpoint {
                work_id: row.work.clone().ok_or_else(invalid)?,
                operation_id: format!("slack-{nonce}"),
                generation: row.generation.ok_or_else(invalid)?,
                session: SESSION.into(),
                expected_updated_seq: row.seq,
                kind,
                summary,
                next_action: next,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: values
                    .pointer("/resolve/value/selected_option/value")
                    .and_then(Value::as_str)
                    == Some("yes"),
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            };
            request.validate()?;
            Ok(Submission::Checkpoint(request))
        } else {
            Err(HubError::Unauthorized)
        }
    }

    fn submit(&self, state: &HubState, user: &str, view: &Value) -> Result<SubmitOutcome> {
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let who = self.principal(state, user, true)?;
        let nonce = view
            .get("private_metadata")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64)
            .ok_or_else(invalid)?;
        let values = view.pointer("/state/values").ok_or_else(invalid)?;
        let digest = hash(&serde_json::to_vec(values).map_err(|_| invalid())?);
        let submission = {
            let mut db = self.database()?;
            let tx = db.transaction()?;
            let row=tx.query_row("SELECT user,binding,kind,work,generation,seq,expires,request_hash,state,outcome FROM actions WHERE id=?1",[nonce],|r|Ok(ActionRow{user:r.get(0)?,binding:r.get(1)?,kind:r.get(2)?,work:r.get(3)?,generation:r.get(4)?,seq:r.get(5)?,expires:r.get(6)?,request_hash:r.get(7)?,state:r.get(8)?,outcome:r.get(9)?})).optional()?.ok_or(HubError::Unauthorized)?;
            if row.user != user
                || row.binding != Self::fingerprint(&who)
                || row.expires <= unix_now()
                || row.request_hash.as_ref().is_some_and(|h| h != &digest)
            {
                return Err(HubError::Unauthorized);
            }
            if row.state == "done" {
                return SubmitOutcome::restored(&row.outcome);
            }
            if matches!(row.state.as_str(), "running" | "uncertain") {
                return Err(HubError::Busy);
            }
            if row.state != "open" {
                return Err(invalid());
            }
            // Core form validation happens before claiming the operation; corrected input
            // can reuse this open nonce until a core operation has actually been admitted.
            let submission = Self::submission(&who, nonce, &row, values)?;
            tx.execute(
                "UPDATE actions SET request_hash=?1,state='running' WHERE id=?2",
                params![digest, nonce],
            )?;
            tx.commit()?;
            submission
        };
        let grant = super::delegation::CoordinationGrant::for_principal(&who)?;
        let result = match submission {
            Submission::Start(request) => state
                .store
                .work_start_delegated(&grant, &request, unix_now())
                .map(|result| {
                    if let Some(request) = result.coordination_request {
                        SubmitOutcome::Coordination(request.id)
                    } else if result.conflict.is_some() {
                        SubmitOutcome::Conflict
                    } else {
                        SubmitOutcome::Saved
                    }
                }),
            Submission::Checkpoint(request) => state
                .store
                .work_checkpoint_delegated(&grant, &request, unix_now())
                .map(|_| SubmitOutcome::Saved),
        };
        let status = match &result {
            Ok(_) => "done",
            Err(HubError::Unavailable(_)) => "uncertain",
            Err(_) => "rejected",
        };
        let outcome = result
            .as_ref()
            .ok()
            .map(SubmitOutcome::code)
            .unwrap_or_default();
        self.database()?.execute(
            "UPDATE actions SET state=?1,outcome=?2 WHERE id=?3",
            params![status, outcome, nonce],
        )?;
        self.enqueue(
            &format!("submit-{nonce}"),
            user,
            &who,
            result.as_ref().map(SubmitOutcome::page).unwrap_or("work"),
        )?;
        super::coordination_http::CHANGED.notify_waiters();
        result
    }

    fn prepare_delivery(&self, state: &HubState) -> Result<Option<Delivery>> {
        self.database()?.execute(
            "DELETE FROM deliveries WHERE created < ?1",
            [unix_now() - 86400],
        )?;
        self.database()?
            .execute("DELETE FROM actions WHERE expires < ?1", [unix_now()])?;
        let job: Option<(String, String, String, String, i64)> = {
            let db = self.database()?;
            db.query_row("SELECT id,user,binding,page,attempts FROM deliveries WHERE state='queued' AND due<=?1 ORDER BY created,id LIMIT 1",[unix_now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?
        };
        let Some((id, user, binding, page, attempts)) = job else {
            return Ok(None);
        };
        self.database()?.execute(
            "UPDATE deliveries SET state='sending',attempts=attempts+1 WHERE id=?1",
            [&id],
        )?;
        let rendered = (|| {
            let who = self.principal(state, &user, false)?;
            if Self::fingerprint(&who) != binding {
                return Err(HubError::Unauthorized);
            }
            let mut projection_actor = who;
            projection_actor.read_only = self.principal(state, &user, true).is_err();
            let mut view = {
                let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
                super::slack_views::render(state, &projection_actor, &page, unix_now())?
            };
            let uncertain: i64 = self.database()?.query_row(
                "SELECT count(*) FROM actions WHERE user=?1 AND state IN ('running','uncertain')",
                [&user],
                |r| r.get(0),
            )?;
            if uncertain > 0
                && let Some(blocks) = view["blocks"].as_array_mut()
                && let Some(title) = blocks.first_mut()
            {
                *title = json!({"type":"section","text":plain_text("AgentSCM · A previous save has no confirmed result. Inspect current work before submitting another action.",500)});
            }
            if let Some(elements) = view
                .pointer_mut("/blocks/1/elements")
                .and_then(Value::as_array_mut)
            {
                elements.push(json!({"type":"button","action_id":"agentscm_open_hub","text":plain_text("Open Hub",40),"url":self.hub_url}));
            }
            Ok(view)
        })();
        Ok(Some(Delivery {
            id,
            user,
            binding,
            attempts,
            view: rendered,
        }))
    }

    async fn deliver(self: &Arc<Self>, state: &HubState) -> Result<()> {
        let service = self.clone();
        let context = state.clone();
        let Some(job) = offload(move || service.prepare_delivery(&context)).await? else {
            return Ok(());
        };
        let Delivery {
            id,
            user,
            binding,
            attempts,
            view,
        } = job;
        let outcome = match view {
            Ok(view) => {
                let service = self.clone();
                let context = state.clone();
                let recipient = user.clone();
                let expected = binding.clone();
                let operation = id.clone();
                let admission = offload(move || {
                    let who = service.principal(&context, &recipient, false)?;
                    if Self::fingerprint(&who) != expected {
                        return Err(HubError::Unauthorized);
                    }
                    let db = service.database()?;
                    let status: String = db.query_row(
                        "SELECT state FROM deliveries WHERE id=?1",
                        [operation],
                        |r| r.get(0),
                    )?;
                    if status != "sending" {
                        return Err(invalid());
                    }
                    // A lost Slack reply still leaves this possible disclosure tracked.
                    db.execute(
                        "INSERT OR IGNORE INTO private_views(user,binding) VALUES(?1,?2)",
                        params![recipient, expected],
                    )?;
                    Ok(())
                })
                .await;
                match admission {
                    Ok(()) => {
                        self.api("views.publish", json!({"user_id":user,"view":view}))
                            .await
                    }
                    Err(HubError::Busy | HubError::Unavailable(_)) => Err(5),
                    Err(_) => Err(0),
                }
            }
            Err(HubError::Busy | HubError::Unavailable(_)) => Err(5),
            Err(_) => Err(0),
        };
        if outcome.is_ok() {
            let service = self.clone();
            let recipient = user.clone();
            let expected = binding.clone();
            offload(move || {
                service.database()?.execute(
                    "DELETE FROM private_views WHERE user=?1 AND binding<>?2",
                    params![recipient, expected],
                )?;
                Ok(())
            })
            .await?;
        }
        let (status, delay) = match outcome {
            Ok(()) => ("done", 0),
            Err(delay) if delay > 0 && attempts < 7 => ("queued", delay as i64),
            Err(_) => ("failed", 0),
        };
        let service = self.clone();
        offload(move || {
            service.database()?.execute(
                "UPDATE deliveries SET state=?1,due=?2 WHERE id=?3 AND state='sending'",
                params![status, unix_now() + delay, id],
            )?;
            Ok(())
        })
        .await
    }

    async fn clear_revoked_views(self: &Arc<Self>, state: &HubState) -> Result<()> {
        let service = self.clone();
        let rows: Vec<(String, String)> = offload(move || {
            service.active()?;
            let db = service.database()?;
            let mut query = db.prepare("SELECT user,binding FROM private_views LIMIT 64")?;
            Ok(query
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?)
        })
        .await?;
        for (user, binding) in rows {
            let service = self.clone();
            let context = state.clone();
            let recipient = user.clone();
            let expected = binding.clone();
            let clear = offload(move || {
                Ok(match service.principal(&context, &recipient, false) {
                    Ok(who) => Self::fingerprint(&who) != expected,
                    Err(HubError::Unauthorized) => true,
                    Err(_) => false,
                })
            })
            .await?;
            if clear && self.api("views.publish",json!({"user_id":user,"view":{"type":"home","blocks":[{"type":"section","text":plain_text("AgentSCM view cleared after an authorization change. Refresh Home; if access is unavailable, ask the Hub operator to verify your repository binding. Previously disclosed copies cannot be recalled.",500)}]}})).await.is_ok(){
                let service=self.clone();offload(move||{service.database()?.execute("DELETE FROM private_views WHERE user=?1 AND binding=?2",params![user,binding])?;Ok(())}).await?;
            }
        }
        Ok(())
    }

    pub fn spawn_worker(self: Arc<Self>, state: HubState) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut last = tokio::time::Instant::now() - Duration::from_secs(30);
            loop {
                if last.elapsed() >= Duration::from_secs(30) {
                    match self.clear_revoked_views(&state).await {
                        Ok(()) | Err(HubError::Unauthorized) => {}
                        Err(_) => eprintln!("Slack revocation reconciliation unavailable"),
                    };
                    last = tokio::time::Instant::now();
                }
                if self.deliver(&state).await.is_err() {
                    eprintln!("Slack delivery journal unavailable");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    }
}

fn field(values: &Value, id: &str) -> Option<String> {
    values
        .get(id)?
        .get("value")?
        .get("value")?
        .as_str()
        .map(str::to_owned)
}
fn input(id: &str, label: &str, optional: bool, multiline: bool) -> Value {
    json!({"type":"input","block_id":id,"optional":optional,"label":plain_text(label,100),"element":{"type":"plain_text_input","action_id":"value","multiline":multiline,"max_length":if multiline{4000}else{500}}})
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
    // Home-open notifications carry no mutation authority. A valid installation event
    // must be acknowledged even when this user is unbound or local refresh capacity is
    // unavailable, so ordinary member activity cannot drive Events API delivery failures.
    // Lifecycle events still retain retryable failures until their revocation is durable.
    let home_event = header(&headers, "content-type")
        .is_some_and(|content| content.starts_with("application/json"))
        && serde_json::from_slice::<Value>(&body).is_ok_and(|event| {
            event.get("type").and_then(Value::as_str) == Some("event_callback")
                && event.get("team_id").and_then(Value::as_str) == Some(&slack.team)
                && event.get("api_app_id").and_then(Value::as_str) == Some(&slack.app)
                && event.pointer("/event/type").and_then(Value::as_str) == Some("app_home_opened")
        });
    // Dedicated admission bounds Slack waits independently of the Hub store budget.
    // Blocking database calls never run on the async reactor. A timeout does not cancel
    // an admitted core write: its journal remains the source for later reconciliation.
    let permit = match slack.requests.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) if home_event => return axum::Json(json!({})).into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let context = state.clone();
    let work = async move {
        let request_slot = Arc::new(permit);
        let prep_slot = request_slot.clone();
        let service = slack.clone();
        let prepared = offload(move || {
            let _slot = prep_slot;
            prepare(&context, &service, &headers, &body)
        })
        .await?;
        match prepared {
            Prepared::Response(value) => Ok(value),
            Prepared::Modal {
                payload,
                response,
                _user_slot,
            } => {
                slack
                    .api("views.open", payload)
                    .await
                    .map_err(|_| unavailable())?;
                Ok(response)
            }
        }
    };
    let outcome = tokio::time::timeout(Duration::from_millis(2500), work).await;
    match outcome {
        Ok(Ok(value)) => axum::Json(value).into_response(),
        _ if home_event => axum::Json(json!({})).into_response(),
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
        match event.pointer("/event/type").and_then(Value::as_str) {
            Some("app_uninstalled" | "tokens_revoked") => {
                slack
                    .database()?
                    .execute("UPDATE installation SET revoked=1 WHERE id=1", [])?;
                slack.database()?.execute(
                    "UPDATE deliveries SET state='failed' WHERE state IN ('queued','sending')",
                    [],
                )?;
            }
            Some("app_home_opened") => {
                let user = event
                    .pointer("/event/user")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                let _user_slot = slack.user_slot(user)?;
                let who = slack.principal(state, user, false)?;
                let id = event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .filter(|s| s.len() <= 128)
                    .ok_or_else(invalid)?;
                slack.enqueue(&format!("event-{id}"), user, &who, "status")?;
            }
            _ => {}
        }
        return Ok(Prepared::Response(json!({})));
    }
    if !content.starts_with("application/x-www-form-urlencoded") {
        return Err(invalid());
    }
    let form = parse_form(body).map_err(|_| invalid())?;
    if let Some(payload) = form.get("payload") {
        let payload: Value = serde_json::from_str(payload).map_err(|_| invalid())?;
        if payload.pointer("/team/id").and_then(Value::as_str) != Some(&slack.team)
            || payload.get("api_app_id").and_then(Value::as_str) != Some(&slack.app)
        {
            return Err(HubError::Unauthorized);
        }
        let user = payload
            .pointer("/user/id")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let user_slot = slack.user_slot(user)?;
        let who = slack.principal(state, user, false)?;
        match payload.get("type").and_then(Value::as_str) {
            Some("view_submission")
                if payload.pointer("/view/callback_id").and_then(Value::as_str)
                    == Some("agentscm_submit") =>
            {
                let view = payload.get("view").ok_or_else(invalid)?.clone();
                let result = slack.submit(state, user, &view);
                return Ok(Prepared::Response(match result {
                    Ok(SubmitOutcome::Saved) => json!({"response_action":"clear"}),
                    Ok(SubmitOutcome::Coordination(id)) => {
                        json!({"response_action":"update","view":{"type":"modal","title":plain_text("Coordination requested",24),"close":plain_text("Close",24),"blocks":[{"type":"section","text":plain_text(&format!("AgentSCM recorded coordination request {id}. The work claim was not granted. Inspect Coordination in App Home; the request is not evidence that an agent changed its work."),1000)}]}})
                    }
                    Ok(SubmitOutcome::Conflict) => {
                        json!({"response_action":"update","view":{"type":"modal","title":plain_text("Claim remains held",24),"close":plain_text("Close",24),"blocks":[{"type":"section","text":plain_text("AgentSCM confirmed that this work could not be claimed. Inspect current ownership in App Home before creating another proposal.",1000)}]}})
                    }
                    Err(HubError::Invalid(code)) if code.starts_with("slack_form_") => {
                        let field = code.strip_prefix("slack_form_").unwrap_or("next");
                        let message = match field {
                            "scope" => {
                                "Use at most 256 valid repository-relative paths, one per line."
                            }
                            "kind" => "Choose a checkpoint kind.",
                            _ => {
                                "Use nonempty text of at most 500 UTF-8 bytes without control characters (emoji can use multiple bytes)."
                            }
                        };
                        json!({"response_action":"errors","errors":{field:message}})
                    }
                    Err(_) => {
                        json!({"response_action":"errors","errors":{"next":"Save not confirmed. Refresh App Home, inspect current work and reopen the form. Permission, version or scope may have changed."}})
                    }
                }));
            }
            Some("block_actions") => {
                let actions = payload
                    .get("actions")
                    .and_then(Value::as_array)
                    .filter(|a| a.len() == 1)
                    .ok_or_else(invalid)?;
                let action = &actions[0];
                let value = action
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match action.get("action_id").and_then(Value::as_str) {
                    Some(id) if id.starts_with("agentscm_page:") => {
                        validate_page(value)?;
                        slack.enqueue(&format!("action-{}", hash(body)), user, &who, value)?;
                    }
                    Some("agentscm_open_hub") => {}
                    Some("agentscm_start" | "agentscm_checkpoint") => {
                        let work = (action.get("action_id").and_then(Value::as_str)
                            == Some("agentscm_checkpoint"))
                        .then_some(value);
                        return Ok(Prepared::Modal {
                            payload: slack.modal(
                                state,
                                user,
                                payload
                                    .get("trigger_id")
                                    .and_then(Value::as_str)
                                    .ok_or_else(invalid)?,
                                work,
                            )?,
                            response: json!({}),
                            _user_slot: user_slot,
                        });
                    }
                    _ => return Err(invalid()),
                }
            }
            _ => return Err(invalid()),
        }
    } else {
        if form.get("team_id") != Some(&slack.team)
            || form.get("api_app_id") != Some(&slack.app)
            || form.get("command").map(String::as_str) != Some("/agentscm")
        {
            return Err(HubError::Unauthorized);
        }
        let user = form.get("user_id").ok_or_else(invalid)?;
        let user_slot = slack.user_slot(user)?;
        let who = slack.principal(state, user, false)?;
        let text = form
            .get("text")
            .map(String::as_str)
            .unwrap_or_default()
            .trim();
        super::slack_protocol::parse_command(text).map_err(|_| invalid())?;
        let trigger = form
            .get("trigger_id")
            .map(String::as_str)
            .unwrap_or_default();
        let modal = if text == "start" {
            Some(slack.modal(state, user, trigger, None)?)
        } else if let Some(work) = text.strip_prefix("checkpoint ") {
            Some(slack.modal(state, user, trigger, Some(work.trim()))?)
        } else {
            let page = if text.is_empty() {
                "status".to_owned()
            } else if let Some(work) = text.strip_prefix("work ") {
                format!("work:{}", work.trim())
            } else {
                text.to_owned()
            };
            validate_page(&page)?;
            slack.enqueue(&format!("command-{}", hash(body)), user, &who, &page)?;
            None
        };
        let response = json!({"response_type":"ephemeral","text":"Open AgentSCM Home to inspect current work. Forms confirm saves separately.","blocks":[{"type":"section","text":plain_text("Open AgentSCM Home to inspect current state. A requested refresh is not a delivery or execution receipt.",500)},{"type":"actions","elements":[{"type":"button","text":plain_text("Open AgentSCM",30),"url":format!("slack://app?team={}&id={}&tab=home",slack.team,slack.app)}]}]});
        return Ok(match modal {
            Some(payload) => Prepared::Modal {
                payload,
                response,
                _user_slot: user_slot,
            },
            None => Prepared::Response(response),
        });
    }
    Ok(Prepared::Response(json!({})))
}

fn validate_page(page: &str) -> Result<()> {
    if matches!(
        page,
        "status" | "work" | "briefing" | "coordination" | "handoffs" | "workflows" | "help"
    ) {
        return Ok(());
    }
    if let Some(id) = page.strip_prefix("work:") {
        return super::work::validate_id("work", id, "wrk");
    }
    if let Some(id) = page.strip_prefix("workflow:") {
        return crate::skill_workflow_service::validate_run_id(id).map_err(|_| invalid());
    }
    Err(invalid())
}

#[cfg(all(test, unix))]
#[path = "../../tests/common/hub_pg.rs"]
mod slack_pg;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::backend::LocalBackend;
    use crate::hub::store::{self, ForgeIdentity, mint_bound_agent_token};
    use axum::extract::OriginalUri;
    use axum::routing::post;
    use ring::hmac;
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        state: HubState,
        token: String,
        owner: String,
        address: std::net::SocketAddr,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        replies: Arc<Mutex<Vec<(u16, Value)>>>,
        arrived: Arc<tokio::sync::Notify>,
        auth_team: Arc<Mutex<String>>,
        auth_bot: Arc<Mutex<String>>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
    fn private(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    impl Fixture {
        async fn new(read_only: bool, write: bool) -> Self {
            Self::with_store(read_only, write, None).await
        }
        async fn with_store(read_only: bool, write: bool, database: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            let (store, user, token, secret) = tokio::task::block_in_place(|| {
                let local_url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
                let store = store::open(database.unwrap_or(&local_url)).unwrap();
                let user = store
                    .upsert_user(
                        &ForgeIdentity {
                            provider: "github".into(),
                            host: "github.com".into(),
                            user_id: 42,
                        },
                        "contributor",
                    )
                    .unwrap();
                let (token, secret, _) = mint_bound_agent_token(
                    &*store,
                    &user.id,
                    "slack-coordination",
                    "acme/widgets",
                    101,
                    3600,
                    read_only,
                )
                .unwrap();
                (store, user, token, secret)
            });
            private(&dir.path().join("agent"), secret.expose());
            private(&dir.path().join("signing"), "synthetic-signing-secret");
            private(&dir.path().join("bot"), "xoxb-synthetic");
            let config = json!({"version":1,"app_id":"A123","bot_id":"B123","team_id":"T123","hub_url":"https://hub.example.test/","signing_secret_file":dir.path().join("signing"),"bot_token_file":dir.path().join("bot"),"database":dir.path().join("slack.sqlite"),"bindings":[{"slack_user":"U123","provider":"github","host":"github.com","forge_user_id":42,"repository":"acme/widgets","repository_id":101,"agent_token_file":dir.path().join("agent"),"allow_coordination_writes":write}]});
            private(&dir.path().join("config"), &config.to_string());
            let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
            let replies = Arc::new(Mutex::new(Vec::<(u16, Value)>::new()));
            let capture = calls.clone();
            let outcomes = replies.clone();
            let arrived = Arc::new(tokio::sync::Notify::new());
            let seen = arrived.clone();
            let auth_team = Arc::new(Mutex::new("T123".to_owned()));
            let expected_team = auth_team.clone();
            let auth_bot = Arc::new(Mutex::new("B123".to_owned()));
            let expected_bot = auth_bot.clone();
            let fake = Router::new().route(
                "/api/{method}",
                post(move |OriginalUri(uri): OriginalUri, body: Bytes| {
                    let capture = capture.clone();
                    let outcomes = outcomes.clone();
                    let seen = seen.clone();
                    let expected_team=expected_team.clone();
                    let expected_bot=expected_bot.clone();
                    async move {
                        if uri.path()=="/api/auth.test" {return (StatusCode::OK,axum::Json(json!({"ok":true,"team_id":expected_team.lock().unwrap().clone(),"bot_id":expected_bot.lock().unwrap().clone()})));}
                        capture.lock().unwrap().push((
                            uri.path().to_owned(),
                            serde_json::from_slice(&body).unwrap(),
                        ));
                        seen.notify_one();
                        let result = outcomes
                            .lock()
                            .unwrap()
                            .pop()
                            .unwrap_or((200, json!({"ok":true})));
                        if let Some(delay) = result.1.get("__test_delay_ms").and_then(Value::as_u64)
                        {
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                        }
                        (
                            StatusCode::from_u16(result.0).unwrap(),
                            axum::Json(result.1),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fake_address = listener.local_addr().unwrap();
            let fake_task = tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
            let mut slack = Slack::from_file(&dir.path().join("config")).unwrap();
            slack.api = format!("http://{fake_address}/api/");
            let state = HubState::new(
                store,
                Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
            )
            .with_slack(slack);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = super::super::router(state.clone());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                dir,
                state,
                token,
                owner: user.id,
                address,
                calls,
                replies,
                arrived,
                auth_team,
                auth_bot,
                tasks: vec![fake_task, server],
            }
        }
        fn slack(&self) -> Arc<Slack> {
            self.state.slack.clone().unwrap()
        }
        async fn post(&self, body: String, json_body: bool) -> (StatusCode, Value) {
            let timestamp = unix_now().to_string();
            let signed = format!("v0:{timestamp}:{body}");
            let signature = format!(
                "v0={}",
                crate::team::auth::lower_hex(
                    hmac::sign(
                        &hmac::Key::new(hmac::HMAC_SHA256, b"synthetic-signing-secret"),
                        signed.as_bytes()
                    )
                    .as_ref()
                )
            );
            let response = reqwest::Client::new()
                .post(format!("http://{}/v1/hub/slack", self.address))
                .header(
                    "content-type",
                    if json_body {
                        "application/json"
                    } else {
                        "application/x-www-form-urlencoded"
                    },
                )
                .header("x-slack-request-timestamp", timestamp)
                .header("x-slack-signature", signature)
                .body(body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.bytes().await.unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }
        async fn command(&self, text: &str) -> (StatusCode, Value) {
            let body = reqwest::Url::parse_with_params(
                "http://localhost",
                [
                    ("team_id", "T123"),
                    ("api_app_id", "A123"),
                    ("user_id", "U123"),
                    ("command", "/agentscm"),
                    ("text", text),
                    ("trigger_id", "synthetic-trigger"),
                ],
            )
            .unwrap()
            .query()
            .unwrap()
            .to_owned();
            self.post(body, false).await
        }
        async fn interaction(&self, payload: Value) -> (StatusCode, Value) {
            let value = payload.to_string();
            let url =
                reqwest::Url::parse_with_params("http://localhost", [("payload", value)]).unwrap();
            self.post(url.query().unwrap().into(), false).await
        }
        fn submission(&self, nonce: &str, fields: Value) -> Value {
            json!({"type":"view_submission","api_app_id":"A123","team":{"id":"T123"},"user":{"id":"U123"},"view":{"callback_id":"agentscm_submit","private_metadata":nonce,"state":{"values":fields}}})
        }
        fn last_modal(&self) -> Value {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(p, _)| p == "/api/views.open")
                .unwrap()
                .1["view"]
                .clone()
        }
        fn items(&self) -> Vec<super::super::work::WorkItem> {
            tokio::task::block_in_place(|| {
                self.state
                    .store
                    .work_overview(&self.token, &Default::default(), unix_now())
                    .unwrap()
                    .current
            })
        }
        async fn start(&self) -> super::super::work::WorkItem {
            assert_eq!(self.command("start").await.0, StatusCode::OK);
            let nonce = self.last_modal()["private_metadata"]
                .as_str()
                .unwrap()
                .to_owned();
            let payload=self.submission(&nonce,json!({"title":{"value":{"value":"Synthetic scoped task"}},"scope":{"value":{"value":"src/widget.rs"}},"next":{"value":{"value":"Inspect source"}}}));
            let result = self.interaction(payload).await;
            assert_eq!(result.1["response_action"], "clear", "{result:?}");
            self.items().pop().unwrap()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingress_checks_signature_workspace_app_user_and_request_shape() {
        let f = Fixture::new(true, false).await;
        let url = format!("http://{}/v1/hub/slack", f.address);
        assert_eq!(
            reqwest::Client::new()
                .post(&url)
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(f.command("status").await.0, StatusCode::OK);
        for (key, value) in [
            ("team_id", "T999"),
            ("api_app_id", "A999"),
            ("user_id", "U999"),
        ] {
            let mut form = std::collections::BTreeMap::from([
                ("team_id", "T123"),
                ("api_app_id", "A123"),
                ("user_id", "U123"),
                ("command", "/agentscm"),
                ("text", "status"),
            ]);
            form.insert(key, value);
            let url = reqwest::Url::parse_with_params("http://localhost", form).unwrap();
            assert_eq!(
                f.post(url.query().unwrap().into(), false).await.0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(f.command("start").await.0, StatusCode::FORBIDDEN);
        assert_eq!(f.command("status extra").await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            f.post("team_id=T123&team_id=T123".into(), false).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.post("x".repeat(65537), true).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            f.post(
                json!({"type":"url_verification","challenge":"synthetic"}).to_string(),
                true
            )
            .await
            .1["challenge"],
            "synthetic"
        );
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_private_refresh_deduplicates_and_rechecks_revocation() {
        let f = Fixture::new(true, false).await;
        let event=json!({"type":"event_callback","team_id":"T123","api_app_id":"A123","event_id":"Ev123","event":{"type":"app_home_opened","user":"U123","tab":"home","text":"SHOULD_NOT_BE_STORED"}}).to_string();
        for _ in 0..2 {
            assert_eq!(f.post(event.clone(), true).await.0, StatusCode::OK);
        }
        let slack = f.slack();
        assert_eq!(
            slack
                .database()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                .unwrap(),
            1
        );
        slack.deliver(&f.state).await.unwrap();
        let calls = f.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/api/views.publish");
        assert_eq!(calls[0].1["user_id"], "U123");
        assert!(calls[0].1.get("channel").is_none());
        assert!(!calls[0].1.to_string().contains(&f.token));
        assert!(
            !String::from_utf8_lossy(&fs::read(f.dir.path().join("slack.sqlite")).unwrap())
                .contains("SHOULD_NOT_BE_STORED")
        );
        f.command("work").await;
        f.state.store.revoke_agent_token(&f.token).unwrap();
        slack.deliver(&f.state).await.unwrap();
        assert_eq!(f.calls.lock().unwrap().len(), 1);
        assert_eq!(f.command("status").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn typed_work_and_checkpoint_are_idempotent_scoped_and_version_fenced() {
        let f = Fixture::new(true, true).await;
        let item = f.start().await;
        assert_eq!(item.lead_session, SESSION);
        assert_eq!(item.scope, ["src/widget.rs"]);
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let values = json!({"title":{"value":{"value":"Synthetic scoped task"}},"scope":{"value":{"value":"src/widget.rs"}},"next":{"value":{"value":"Inspect source"}}});
        let payload = f.submission(&nonce, values.clone());
        assert_eq!(f.interaction(payload).await.1["response_action"], "clear");
        assert_eq!(f.items().len(), 1);
        let changed = f.submission(
            &nonce,
            json!({"title":{"value":{"value":"Different task"}}}),
        );
        assert_eq!(f.interaction(changed).await.1["response_action"], "errors");
        assert_eq!(
            f.command(&format!("checkpoint {}", item.id)).await.0,
            StatusCode::OK
        );
        let stale = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        f.state
            .store
            .work_checkpoint_delegated(
                &super::super::delegation::CoordinationGrant::for_principal(
                    &f.slack().principal(&f.state, "U123", true).unwrap(),
                )
                .unwrap(),
                &WorkCheckpoint {
                    work_id: item.id.clone(),
                    operation_id: format!("slack-{}", "e".repeat(64)),
                    generation: item.generation,
                    session: SESSION.into(),
                    expected_updated_seq: Some(item.updated_seq),
                    kind: CheckpointKind::Progress,
                    summary: "Newer authoritative update".into(),
                    next_action: None,
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![],
                    depends_on: vec![],
                    related_work: vec![],
                },
                unix_now(),
            )
            .unwrap();
        let checkpoint = json!({"kind":{"value":{"selected_option":{"value":"blocker"}}},"summary":{"value":{"value":"Synthetic blocker"}},"next":{"value":{"value":"Ask for help"}}});
        assert_eq!(
            f.interaction(f.submission(&stale, checkpoint.clone()))
                .await
                .1["response_action"],
            "errors"
        );
        assert_eq!(f.items()[0].state, super::super::work::WorkState::Active);
        f.command(&format!("checkpoint {}", item.id)).await;
        let fresh = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            f.interaction(f.submission(&fresh, checkpoint)).await.1["response_action"],
            "clear"
        );
        let item = f.items().pop().unwrap();
        assert_eq!(item.state, super::super::work::WorkState::Blocked);
        assert_eq!(item.blocker.unwrap().summary, "Synthetic blocker");
        // User-entered content is retained only by the authoritative work service.
        assert!(
            !String::from_utf8_lossy(&fs::read(f.dir.path().join("slack.sqlite")).unwrap())
                .contains("Synthetic blocker")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forms_reject_foreign_claims_expiry_and_changed_binding() {
        let f = Fixture::new(true, true).await;
        let item = f.start().await;
        f.command(&format!("checkpoint {}", item.id)).await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let slack = f.slack();
        slack
            .database()
            .unwrap()
            .execute(
                "UPDATE actions SET binding='another-credential' WHERE id=?1",
                [&nonce],
            )
            .unwrap();
        assert!(
            slack
                .submit(&f.state, "U123", &f.submission(&nonce, json!({}))["view"])
                .is_err()
        );
        slack
            .database()
            .unwrap()
            .execute("UPDATE actions SET expires=0 WHERE id=?1", [&nonce])
            .unwrap();
        assert!(
            slack
                .submit(&f.state, "U123", &f.submission(&nonce, json!({}))["view"])
                .is_err()
        );
        let (other, _, _) = mint_bound_agent_token(
            &*f.state.store,
            &f.owner,
            "coding-agent",
            "acme/widgets",
            101,
            3600,
            false,
        )
        .unwrap();
        let other_item = f
            .state
            .store
            .work_start(
                &other,
                &WorkStart {
                    work_id: None,
                    source: None,
                    request_key: Some("other-task".into()),
                    title: "Other agent task".into(),
                    session: "actual-agent-session".into(),
                    role: WorkRole::Lead,
                    scope: vec!["src/other.rs".into()],
                    branch: None,
                    worktree: None,
                    next_action: None,
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                },
                unix_now(),
            )
            .unwrap()
            .item;
        assert_eq!(
            f.command(&format!("checkpoint {}", other_item.id)).await.0,
            StatusCode::FORBIDDEN
        );
        f.state.store.set_user_disabled(&f.owner, true).unwrap();
        assert!(slack.principal(&f.state, "U123", false).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recoverable_view_delivery_and_uninstall_tombstone_survive_restart() {
        let f = Fixture::new(true, false).await;
        f.command("workflows").await;
        f.replies.lock().unwrap().push((429, json!({"ok":false})));
        f.slack().deliver(&f.state).await.unwrap();
        assert_eq!(
            f.slack()
                .database()
                .unwrap()
                .query_row::<String, _, _>("SELECT state FROM deliveries", [], |r| r.get(0))
                .unwrap(),
            "queued"
        );
        f.slack()
            .database()
            .unwrap()
            .execute("UPDATE deliveries SET state='sending',due=0", [])
            .unwrap();
        let mut reopened = Slack::from_file(&f.dir.path().join("config")).unwrap();
        reopened.api = f.slack().api.clone();
        let reopened = Arc::new(reopened);
        reopened.deliver(&f.state).await.unwrap();
        assert_eq!(f.calls.lock().unwrap().len(), 2);
        let event = json!({"type":"event_callback","team_id":"T123","api_app_id":"A123","event_id":"EvRemoved","event":{"type":"app_uninstalled"}});
        assert_eq!(f.post(event.to_string(), true).await.0, StatusCode::OK);
        let reopened = Slack::from_file(&f.dir.path().join("config")).unwrap();
        assert!(reopened.active().is_err());
        assert_eq!(f.command("status").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delegated_write_flag_and_exact_repository_identity_are_required() {
        let f = Fixture::new(false, false).await;
        assert_eq!(f.command("start").await.0, StatusCode::FORBIDDEN);
        let mut config: Value =
            serde_json::from_slice(&fs::read(f.dir.path().join("config")).unwrap()).unwrap();
        config["bindings"][0]["repository_id"] = json!(999);
        private(&f.dir.path().join("config"), &config.to_string());
        let slack = Slack::from_file(&f.dir.path().join("config")).unwrap();
        assert!(slack.principal(&f.state, "U123", false).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(f.dir.path().join("bot"), fs::Permissions::from_mode(0o644))
                .unwrap();
            assert!(Slack::from_file(&f.dir.path().join("config")).is_err());
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_private_pages_render_and_revoked_views_are_cleared_without_source_data() {
        let f = Fixture::new(true, false).await;
        for page in [
            "status",
            "work",
            "briefing",
            "coordination",
            "handoffs",
            "workflows",
            "help",
        ] {
            assert_eq!(f.command(page).await.0, StatusCode::OK);
            f.slack().deliver(&f.state).await.unwrap();
            let calls = f.calls.lock().unwrap();
            let view = &calls.last().unwrap().1["view"];
            assert_eq!(view["type"], "home");
            assert!(view["blocks"].as_array().unwrap().len() <= 90);
            assert!(!view.to_string().contains("agentscm_start"));
            assert!(view.to_string().contains("https://hub.example.test/ui/"));
        }
        let before = f.calls.lock().unwrap().len();
        f.state.store.revoke_agent_token(&f.token).unwrap();
        f.slack().clear_revoked_views(&f.state).await.unwrap();
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), before + 1);
        let cleared = calls.last().unwrap().1.to_string();
        assert!(cleared.contains("access is unavailable"));
        assert!(!cleared.contains("acme/widgets"));
        assert!(!cleared.contains(&f.token));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_view_supersedes_retries_and_pending_metadata_expires() {
        let f = Fixture::new(true, false).await;
        assert_eq!(f.command("status").await.0, StatusCode::OK);
        assert_eq!(f.command("work").await.0, StatusCode::OK);
        f.slack().deliver(&f.state).await.unwrap();
        assert_eq!(f.calls.lock().unwrap().len(), 1);
        assert!(
            f.calls.lock().unwrap()[0]
                .1
                .to_string()
                .contains("Shared work")
        );
        assert_eq!(f.command("coordination").await.0, StatusCode::OK);
        f.slack()
            .database()
            .unwrap()
            .execute("UPDATE deliveries SET created=0", [])
            .unwrap();
        f.slack().deliver(&f.state).await.unwrap();
        assert_eq!(f.calls.lock().unwrap().len(), 1);
        assert_eq!(
            f.slack()
                .database()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_never_replays_uncertain_mutations_and_blockers_can_be_resolved() {
        let f = Fixture::new(true, true).await;
        let item = f.start().await;
        f.command(&format!("checkpoint {}", item.id)).await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        f.slack()
            .database()
            .unwrap()
            .execute("UPDATE actions SET state='running' WHERE id=?1", [&nonce])
            .unwrap();
        let reopened = Slack::from_file(&f.dir.path().join("config")).unwrap();
        let fields = json!({"kind":{"value":{"selected_option":{"value":"blocker"}}},"summary":{"value":{"value":"Need input"}}});
        assert!(
            reopened
                .submit(
                    &f.state,
                    "U123",
                    &f.submission(&nonce, fields.clone())["view"]
                )
                .is_err()
        );
        assert_eq!(f.items()[0].state, super::super::work::WorkState::Active);
        f.command(&format!("checkpoint {}", item.id)).await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            f.interaction(f.submission(&nonce, fields)).await.1["response_action"],
            "clear"
        );
        f.command(&format!("checkpoint {}", item.id)).await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let fields = json!({"kind":{"value":{"selected_option":{"value":"progress"}}},"summary":{"value":{"value":"Input received"}},"resolve":{"value":{"selected_option":{"value":"yes"}}}});
        assert_eq!(
            f.interaction(f.submission(&nonce, fields)).await.1["response_action"],
            "clear"
        );
        let item = f.items().pop().unwrap();
        assert_eq!(item.state, super::super::work::WorkState::Active);
        assert!(item.blocker.is_none());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn correctable_utf8_and_scope_errors_keep_the_modal_open() {
        let f = Fixture::new(true, true).await;
        f.command("start").await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let too_long = json!({"title":{"value":{"value":"🙂".repeat(126)}},"scope":{"value":{"value":"src/a.rs"}}});
        let result = f.interaction(f.submission(&nonce, too_long)).await;
        assert!(result.1["errors"].get("title").is_some());
        let many = (0..257)
            .map(|n| format!("f{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let invalid_scope =
            json!({"title":{"value":{"value":"Valid task"}},"scope":{"value":{"value":many}}});
        assert!(
            f.interaction(f.submission(&nonce, invalid_scope)).await.1["errors"]
                .get("scope")
                .is_some()
        );
        let corrected = json!({"title":{"value":{"value":"Valid task"}},"scope":{"value":{"value":"src/a.rs"}}});
        assert_eq!(
            f.interaction(f.submission(&nonce, corrected)).await.1["response_action"],
            "clear"
        );
        assert_eq!(f.items().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overlap_is_a_confirmed_idempotent_coordination_result() {
        let f = Fixture::new(true, true).await;
        let (agent, _, _) = mint_bound_agent_token(
            &*f.state.store,
            &f.owner,
            "actual-agent",
            "acme/widgets",
            101,
            3600,
            false,
        )
        .unwrap();
        let start = WorkStart {
            work_id: None,
            source: None,
            request_key: Some("agent-task".into()),
            title: "Existing agent work".into(),
            session: "native-session".into(),
            role: WorkRole::Lead,
            scope: vec!["src/widget.rs".into()],
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: vec![],
            related_work: vec![],
            takeover_generation: None,
        };
        f.state
            .store
            .work_start(&agent, &start, unix_now())
            .unwrap();
        f.command("start").await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let payload=f.submission(&nonce,json!({"title":{"value":{"value":"Related proposal"}},"scope":{"value":{"value":"src/widget.rs"}}}));
        for _ in 0..2 {
            let response = f.interaction(payload.clone()).await;
            assert_eq!(response.1["response_action"], "update");
            assert!(
                response
                    .1
                    .to_string()
                    .contains("work claim was not granted")
            );
        }
        let inbox = f
            .state
            .store
            .coordination_list(&f.token, &Default::default(), unix_now())
            .unwrap();
        assert_eq!(inbox.requests.len(), 1);
        assert_eq!(f.items().len(), 1);
        f.slack().deliver(&f.state).await.unwrap();
        assert!(
            f.calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .to_string()
                .contains("Active coordination")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_old_retry_cannot_replace_a_newer_requested_page() {
        let f = Fixture::new(true, false).await;
        f.command("status").await;
        f.replies
            .lock()
            .unwrap()
            .push((429, json!({"ok":false,"__test_delay_ms":100})));
        let slack = f.slack();
        let context = f.state.clone();
        let delivery = tokio::spawn(async move { slack.deliver(&context).await.unwrap() });
        f.arrived.notified().await;
        f.command("work").await;
        delivery.await.unwrap();
        f.slack()
            .database()
            .unwrap()
            .execute("UPDATE deliveries SET due=0", [])
            .unwrap();
        f.slack().deliver(&f.state).await.unwrap();
        f.slack().deliver(&f.state).await.unwrap();
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.last().unwrap().1.to_string().contains("Shared work"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uncertain_publication_is_tracked_before_dispatch_and_cleared_after_revocation() {
        let f = Fixture::new(true, false).await;
        f.command("status").await;
        f.replies
            .lock()
            .unwrap()
            .push((200, json!({"ok":true,"__test_delay_ms":2500})));
        f.slack().deliver(&f.state).await.unwrap();
        assert_eq!(
            f.slack()
                .database()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM private_views", [], |r| r.get(0))
                .unwrap(),
            1
        );
        f.state.store.revoke_agent_token(&f.token).unwrap();
        f.slack().clear_revoked_views(&f.state).await.unwrap();
        let calls = f.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .last()
                .unwrap()
                .1
                .to_string()
                .contains("access is unavailable")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slack_network_waits_do_not_hold_hub_store_permits() {
        let f = Fixture::new(true, true).await;
        f.replies
            .lock()
            .unwrap()
            .push((200, json!({"ok":true,"__test_delay_ms":300})));
        let (command, ()) = tokio::join!(f.command("start"), async {
            f.arrived.notified().await;
            assert_eq!(
                f.state.permits.available_permits(),
                super::super::MAX_CONCURRENT_STORE_CALLS
            );
            let response = reqwest::Client::new()
                .get(format!("http://{}/v1/hub/health", f.address))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(f.command("start").await.0, StatusCode::SERVICE_UNAVAILABLE);
        });
        assert_eq!(command.0, StatusCode::OK);
    }

    async fn grant_journey(f: &Fixture) {
        let item = f.start().await;
        tokio::task::block_in_place(|| {
            let principal = f.slack().principal(&f.state, "U123", true).unwrap();
            let grant =
                super::super::delegation::CoordinationGrant::for_principal(&principal).unwrap();
            let start = json!({"request_key":format!("slack-{}", "e".repeat(64)),"title":"Unsupported relationship","session":SESSION,"role":"lead","scope":[]});
            for field in ["depends_on", "related_work"] {
                let mut request = start.clone();
                request[field] = json!([item.id]);
                let request: WorkStart = serde_json::from_value(request).unwrap();
                assert!(matches!(
                    f.state
                        .store
                        .work_start_delegated(&grant, &request, unix_now()),
                    Err(HubError::Invalid(_))
                ));
            }
            let checkpoint = json!({"work_id":item.id,"operation_id":format!("slack-{}", "d".repeat(64)),"generation":item.generation,"session":SESSION,"expected_updated_seq":item.updated_seq,"kind":"progress","summary":"Unsupported mutation"});
            for (field, value) in [
                ("scope", json!(["other/path.rs"])),
                ("branch", json!("other-branch")),
                ("worktree", json!("other-worktree")),
                (
                    "source_refs",
                    json!([{"kind":"repository_path","path":"other/path.rs"}]),
                ),
                ("depends_on", json!([item.id])),
                ("related_work", json!([item.id])),
            ] {
                let mut request = checkpoint.clone();
                request[field] = value;
                let request: WorkCheckpoint = serde_json::from_value(request).unwrap();
                assert!(matches!(
                    f.state
                        .store
                        .work_checkpoint_delegated(&grant, &request, unix_now()),
                    Err(HubError::Invalid(_))
                ));
            }
        });
        assert_eq!(f.items(), vec![item.clone()]);
        let bearer = fs::read_to_string(f.dir.path().join("agent")).unwrap();
        for path in [
            "/v1/hub/standup/start",
            "/v1/hub/repos/acme%2Fwidgets/publications",
            "/v1/hub/repos/acme%2Fwidgets/intents",
        ] {
            let response = reqwest::Client::new()
                .post(format!("http://{}{path}", f.address))
                .bearer_auth(&bearer)
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
        f.command(&format!("checkpoint {}", item.id)).await;
        let nonce = f.last_modal()["private_metadata"]
            .as_str()
            .unwrap()
            .to_owned();
        let fields = json!({"kind":{"value":{"selected_option":{"value":"handover"}}},"summary":{"value":{"value":"Ready for supported-agent pickup"}},"next":{"value":{"value":"Continue in the native coding session"}}});
        assert_eq!(
            f.interaction(f.submission(&nonce, fields)).await.1["response_action"],
            "clear"
        );
        let handed = f.items().pop().unwrap();
        assert_eq!(handed.state, super::super::work::WorkState::Handover);
        assert_eq!(
            f.command(&format!("checkpoint {}", item.id)).await.0,
            StatusCode::FORBIDDEN
        );
        let (agent, _, _) = tokio::task::block_in_place(|| {
            mint_bound_agent_token(
                &*f.state.store,
                &f.owner,
                "supported-agent",
                "acme/widgets",
                101,
                3600,
                false,
            )
            .unwrap()
        });
        let request = WorkStart {
            work_id: Some(item.id.clone()),
            source: None,
            request_key: None,
            title: item.title.clone(),
            session: "native-agent".into(),
            role: WorkRole::Lead,
            scope: item.scope.clone(),
            branch: None,
            worktree: None,
            next_action: item.next_action.clone(),
            depends_on: vec![],
            related_work: vec![],
            takeover_generation: Some(handed.generation),
        };
        let taken = tokio::task::block_in_place(|| {
            f.state
                .store
                .work_start(&agent, &request, unix_now())
                .unwrap()
        });
        assert!(taken.took_over);
        assert_eq!(taken.item.generation, handed.generation + 1);
        f.command("work").await;
        f.slack().deliver(&f.state).await.unwrap();
        assert!(
            f.calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .to_string()
                .contains("supported-agent")
        );
        let principal =
            tokio::task::block_in_place(|| f.slack().principal(&f.state, "U123", true).unwrap());
        let grant = super::super::delegation::CoordinationGrant::for_principal(&principal).unwrap();
        tokio::task::block_in_place(|| f.state.store.revoke_agent_token(&f.token).unwrap());
        let request = WorkStart {
            work_id: None,
            source: None,
            request_key: Some(format!("slack-{}", "f".repeat(64))),
            title: "Not authorized".into(),
            session: SESSION.into(),
            role: WorkRole::Lead,
            scope: vec![],
            branch: None,
            worktree: None,
            next_action: None,
            depends_on: vec![],
            related_work: vec![],
            takeover_generation: None,
        };
        assert!(tokio::task::block_in_place(|| f
            .state
            .store
            .work_start_delegated(&grant, &request, unix_now())
            .is_err()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn human_handover_to_supported_agent_preserves_authority() {
        let f = Fixture::new(true, true).await;
        grant_journey(&f).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn human_handover_and_grant_revocation_on_postgres() {
        let Some(pg) = tokio::task::block_in_place(super::slack_pg::PgSchema::create) else {
            return;
        };
        let f = Fixture::with_store(true, true, Some(&pg.url)).await;
        grant_journey(&f).await;
        tokio::task::block_in_place(|| {
            drop(f);
            drop(pg);
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inactive_work_cannot_be_reactivated_by_a_delegated_checkpoint() {
        for kind in [
            CheckpointKind::Pause,
            CheckpointKind::Handover,
            CheckpointKind::Finish,
        ] {
            let f = Fixture::new(true, true).await;
            let item = f.start().await;
            f.command(&format!("checkpoint {}", item.id)).await;
            let nonce = f.last_modal()["private_metadata"]
                .as_str()
                .unwrap()
                .to_owned();
            let value = serde_json::to_value(kind).unwrap();
            let fields = json!({"kind":{"value":{"selected_option":{"value":value}}},"summary":{"value":{"value":"Human lifecycle transition"}}});
            assert_eq!(
                f.interaction(f.submission(&nonce, fields)).await.1["response_action"],
                "clear"
            );
            let current = f
                .state
                .store
                .work_get(&f.token, &item.id, unix_now())
                .unwrap()
                .unwrap();
            let grant = super::super::delegation::CoordinationGrant::for_principal(
                &f.slack().principal(&f.state, "U123", true).unwrap(),
            )
            .unwrap();
            let checkpoint = WorkCheckpoint {
                work_id: item.id.clone(),
                operation_id: format!("slack-{}", "c".repeat(64)),
                generation: current.generation,
                session: SESSION.into(),
                expected_updated_seq: Some(current.updated_seq),
                kind: CheckpointKind::Progress,
                summary: "Must not resume".into(),
                next_action: None,
                scope: None,
                branch: None,
                worktree: None,
                resolve_blocker: false,
                source_refs: vec![],
                depends_on: vec![],
                related_work: vec![],
            };
            assert!(
                f.state
                    .store
                    .work_checkpoint_delegated(&grant, &checkpoint, unix_now())
                    .is_err()
            );
            assert_eq!(
                f.command(&format!("checkpoint {}", item.id)).await.0,
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                f.state
                    .store
                    .work_get(&f.token, &item.id, unix_now())
                    .unwrap()
                    .unwrap()
                    .state,
                current.state
            );
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn home_notifications_acknowledge_unbound_revoked_and_busy_users() {
        let f = Fixture::new(true, true).await;
        let event = |user: &str, id: &str| json!({"type":"event_callback","team_id":"T123","api_app_id":"A123","event_id":id,"event":{"type":"app_home_opened","user":user,"tab":"home"}});
        assert_eq!(
            f.post(event("U999", "EvUnbound").to_string(), true).await.0,
            StatusCode::OK
        );
        let permits = f
            .slack()
            .requests
            .clone()
            .acquire_many_owned(8)
            .await
            .unwrap();
        assert_eq!(
            f.post(event("U123", "EvBusy").to_string(), true).await.0,
            StatusCode::OK
        );
        drop(permits);
        let user_permit = f.slack().user_slot("U123").unwrap();
        assert_eq!(
            f.post(event("U123", "EvUserBusy").to_string(), true)
                .await
                .0,
            StatusCode::OK
        );
        drop(user_permit);
        f.state.store.revoke_agent_token(&f.token).unwrap();
        assert_eq!(
            f.post(event("U123", "EvRevoked").to_string(), true).await.0,
            StatusCode::OK
        );
        let mut wrong_app = event("U999", "EvWrongApp");
        wrong_app["api_app_id"] = json!("A999");
        assert_eq!(
            f.post(wrong_app.to_string(), true).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.slack()
                .database()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                .unwrap(),
            0
        );
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn manifest_requests_only_command_scope() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../examples/slack-manifest.json")).unwrap();
        assert_eq!(
            manifest["oauth_config"]["scopes"],
            json!({"bot":["commands"]})
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mismatched_bot_installation_cannot_receive_repository_views() {
        for same_team in [false, true] {
            let f = Fixture::new(true, true).await;
            if same_team {
                *f.auth_bot.lock().unwrap() = "B999".into();
            } else {
                *f.auth_team.lock().unwrap() = "T999".into();
            }
            f.command("work").await;
            f.slack().deliver(&f.state).await.unwrap();
            assert!(f.calls.lock().unwrap().is_empty());
            assert_eq!(
                f.slack()
                    .database()
                    .unwrap()
                    .query_row::<String, _, _>("SELECT state FROM deliveries", [], |r| r.get(0))
                    .unwrap(),
                "failed"
            );
            assert_eq!(f.command("start").await.0, StatusCode::SERVICE_UNAVAILABLE);
            assert!(f.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ambiguous_new_binding_does_not_erase_old_disclosure_tracking() {
        let f = Fixture::new(true, false).await;
        f.slack()
            .database()
            .unwrap()
            .execute(
                "INSERT INTO private_views(user,binding) VALUES('U123','older-authorization')",
                [],
            )
            .unwrap();
        f.command("status").await;
        f.replies.lock().unwrap().push((503, json!({"ok":false})));
        f.slack().deliver(&f.state).await.unwrap();
        assert_eq!(
            f.slack()
                .database()
                .unwrap()
                .query_row::<i64, _, _>("SELECT count(*) FROM private_views", [], |r| r.get(0))
                .unwrap(),
            2
        );
        f.slack().clear_revoked_views(&f.state).await.unwrap();
        assert!(
            f.calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .to_string()
                .contains("access is unavailable")
        );
    }
}
