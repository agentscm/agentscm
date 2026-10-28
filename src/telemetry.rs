//! Bounded, pseudonymous product telemetry. No source, repository, path, account or text data
//! enters this module. Foreground commands only update local state and start a separate sender.

use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentscm::{Error, Result};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub_cli::config_dir;

const CONFIG_SCHEMA: u32 = 1;
const NOTICE_VERSION: u32 = 1;
const MAX_QUEUE: usize = 64;
const MAX_SENT_KEYS: usize = 512;
const MAX_BATCH_BYTES: usize = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 128 * 1024;
const MAX_ATTEMPTS: u8 = 3;
const MAX_SENDER_BATCHES: usize = 2;
const MAX_SENDER_LIFETIME: Duration = Duration::from_secs(4);
const MIN_HTTP_BUDGET: Duration = Duration::from_secs(1);
const ENDPOINT_PATH: &str = "/v1/telemetry/batches";

#[cfg(test)]
static TEST_DIRECTORY: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub(crate) const DISCLOSURE: &str = "AgentSCM can send pseudonymous daily feature-use indicators. Events contain a random local ID, AgentSCM version, operating system, architecture, feature category, outcome and UTC day. They never contain repository, source, path, account, command arguments or free text. Disable before any send with --no-telemetry, AGENTSCM_TELEMETRY=off, DO_NOT_TRACK=1, or `agentscm telemetry off`.";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Profile {
    PermittedDefault,
    ConsentRequired,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Preference {
    Unset,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    Installation,
    Hub,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Feature {
    Installation,
    FirstUse,
    Quickstart,
    Uninstall,
    InitRepo,
    Collisions,
    Autopsy,
    Intent,
    Work,
    NativeSource,
    Workflow,
    Team,
    Hub,
    GitBridge,
    SkillWorkflow,
    Setup,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Platform {
    Linux,
    Macos,
    Windows,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Architecture {
    X86_64,
    Aarch64,
    Other,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u32,
    profile: Profile,
    preference: Preference,
    endpoint: Option<String>,
    notice_version: Option<u32>,
    notice_seen_at: Option<i64>,
    preference_changed_at: Option<i64>,
    consent_granted_at: Option<i64>,
    installation_id: Option<String>,
    hub_id: Option<String>,
    #[serde(default)]
    queue: Vec<QueuedEvent>,
    #[serde(default)]
    sent_keys: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            profile: Profile::Unknown,
            preference: Preference::Unset,
            endpoint: None,
            notice_version: None,
            notice_seen_at: None,
            preference_changed_at: None,
            consent_granted_at: None,
            installation_id: None,
            hub_id: None,
            queue: Vec::new(),
            sent_keys: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QueuedEvent {
    event_id: String,
    day: String,
    version: String,
    os: Platform,
    arch: Architecture,
    feature: Feature,
    outcome: Outcome,
    count: u16,
    source: SourceKind,
    dedup_key: String,
    attempts: u8,
    created_at: i64,
    next_attempt_at: i64,
}

#[derive(Serialize)]
struct WireEvent<'a> {
    event_id: &'a str,
    day: &'a str,
    version: &'a str,
    os: Platform,
    arch: Architecture,
    feature: Feature,
    outcome: Outcome,
    count: u16,
}

#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Accepted,
    Retry(u64),
    Drop,
}

fn classify_status(status: reqwest::StatusCode, retry_after: Option<&str>) -> Delivery {
    if status == reqwest::StatusCode::ACCEPTED {
        Delivery::Accepted
    } else if status.as_u16() == 429 || status.is_server_error() {
        let delay = retry_after
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60)
            .clamp(30, 3_600);
        Delivery::Retry(delay)
    } else {
        Delivery::Drop
    }
}

struct Lock {
    _file: File,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn directory() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(directory) = TEST_DIRECTORY
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
    {
        return Ok(directory);
    }
    Ok(config_dir()?.join("agentscm"))
}

fn path() -> Result<PathBuf> {
    Ok(directory()?.join("telemetry.json"))
}

fn lock_path(name: &str) -> Result<PathBuf> {
    Ok(directory()?.join(name))
}

fn acquire_lock(name: &str, wait: bool) -> Result<Option<Lock>> {
    let lock_path = lock_path(name)?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A waiting operation may be fencing a sender whose HTTP request has a three-second bound.
    let attempts = if wait && name == "telemetry-sender.lock" {
        320
    } else if wait {
        200
    } else {
        1
    };
    for _ in 0..attempts {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&lock_path)?;
        match file.try_lock() {
            Ok(()) => return Ok(Some(Lock { _file: file })),
            Err(TryLockError::WouldBlock) => {
                if wait {
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    if wait {
        Err(Error::Conflict(
            "another telemetry settings operation is active".into(),
        ))
    } else {
        Ok(None)
    }
}

fn load() -> Result<Config> {
    let path = path()?;
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid("telemetry settings exceed the size limit"));
    }
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| {
        invalid("telemetry settings are invalid; inspect or replace them explicitly")
    })?;
    if config.schema != CONFIG_SCHEMA {
        return Err(invalid("unsupported telemetry settings schema"));
    }
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<()> {
    let id = |value: &str| {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if config.queue.len() > MAX_QUEUE || config.sent_keys.len() > MAX_SENT_KEYS {
        return Err(invalid("telemetry settings exceed collection bounds"));
    }
    if config
        .installation_id
        .as_deref()
        .is_some_and(|value| !id(value))
        || config.hub_id.as_deref().is_some_and(|value| !id(value))
    {
        return Err(invalid(
            "telemetry settings contain an invalid pseudonymous ID",
        ));
    }
    if let Some(value) = &config.endpoint {
        endpoint(value)?;
    }
    for event in &config.queue {
        if !id(&event.event_id)
            || event.count != 1
            || event.attempts > MAX_ATTEMPTS
            || event.created_at < 0
            || event.next_attempt_at < event.created_at
            || event.version.is_empty()
            || event.version.len() > 64
            || !event.version.is_ascii()
            || !valid_day(&event.day)
            || event.dedup_key.len() > 128
            || !event.dedup_key.is_ascii()
        {
            return Err(invalid(
                "telemetry settings contain an invalid queued event",
            ));
        }
        if matches!(event.source, SourceKind::Installation) && config.installation_id.is_none()
            || matches!(event.source, SourceKind::Hub) && config.hub_id.is_none()
        {
            return Err(invalid("telemetry queued event has no source identifier"));
        }
    }
    if config
        .sent_keys
        .iter()
        .any(|key| key.len() > 128 || !key.is_ascii())
    {
        return Err(invalid(
            "telemetry settings contain an invalid deduplication key",
        ));
    }
    Ok(())
}

fn valid_day(day: &str) -> bool {
    day.len() == 10
        && day.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

fn save(config: &Config) -> Result<()> {
    let encoded = serde_json::to_vec(config)?;
    durable_replace(&path()?, &encoded)
}

/// Atomically replace telemetry state after syncing the new file. On Unix, also sync the
/// containing directory so an acknowledged opt-out survives a subsequent system crash.
fn durable_replace(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| invalid("bad telemetry state path"))?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|_| invalid("telemetry randomness unavailable"))?;
    let suffix = suffix
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid("bad telemetry state path"))?
        .to_string_lossy();
    let temp = dir.join(format!(".{file_name}.tmp-{}-{suffix}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn utc_day(timestamp: i64) -> String {
    let (year, month, day, ..) = agentscm::civil::utc_civil(timestamp);
    format!("{year:04}-{month:02}-{day:02}")
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| invalid("telemetry randomness unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
fn isolated_unit_test() -> bool {
    TEST_DIRECTORY
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
}

fn env_off() -> bool {
    #[cfg(test)]
    if isolated_unit_test() {
        return false;
    }
    matches!(std::env::var("AGENTSCM_TELEMETRY").as_deref(), Ok("off"))
        || matches!(std::env::var("DO_NOT_TRACK").as_deref(), Ok("1"))
}

fn ci() -> bool {
    #[cfg(test)]
    if isolated_unit_test() {
        return false;
    }
    std::env::var_os("CI").is_some() || std::env::var_os("AGENTSCM_TELEMETRY_TEST").is_some()
}

fn effective(config: &Config, invocation_off: bool) -> (&'static str, &'static str) {
    if env_off() || ci() {
        return ("disabled", "environment_override");
    }
    if invocation_off {
        return ("disabled", "invocation_override");
    }
    if config.preference == Preference::Off {
        return ("disabled", "persistent_off");
    }
    if config.endpoint.is_none() {
        return ("waiting", "endpoint_unset");
    }
    if matches!(config.profile, Profile::Unknown) {
        return ("waiting", "profile_unknown");
    }
    if config.notice_version != Some(NOTICE_VERSION) || config.notice_seen_at.is_none() {
        return ("waiting", "notice_required");
    }
    if matches!(config.profile, Profile::ConsentRequired)
        && (config.preference != Preference::On || config.consent_granted_at.is_none())
    {
        return ("waiting", "consent_required");
    }
    ("enabled", "enabled")
}

fn endpoint(value: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(value).map_err(|_| invalid("invalid telemetry endpoint"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != ENDPOINT_PATH
    {
        return Err(invalid(format!(
            "telemetry endpoint must be an absolute HTTPS URL with path {ENDPOINT_PATH} and no credentials, query or fragment"
        )));
    }
    Ok(parsed.to_string())
}

pub(crate) fn status(invocation_off: bool, inspect: bool) -> Result<Value> {
    let config = load()?;
    let (state, reason) = effective(&config, invocation_off);
    let mut value = json!({
        "ok": true,
        "schema": config.schema,
        "requested": config.preference,
        "profile": config.profile,
        "endpoint_configured": config.endpoint.is_some(),
        "notice_version": config.notice_version,
        "notice_seen_at": config.notice_seen_at,
        "preference_changed_at": config.preference_changed_at,
        "consent_granted_at": config.consent_granted_at,
        "effective": state,
        "reason": reason,
        "pending": config.queue.len(),
    });
    if inspect {
        value["endpoint"] = json!(config.endpoint);
        value["installation_id"] = json!(config.installation_id);
        value["hub_id"] = json!(config.hub_id);
        value["queued_events"] = json!(
            config
                .queue
                .iter()
                .map(|event| json!({
                    "day":event.day,"feature":event.feature,"outcome":event.outcome,
                    "source":event.source,"attempts":event.attempts
                }))
                .collect::<Vec<_>>()
        );
        value["retained_dedup_keys"] = json!(config.sent_keys.len());
    }
    Ok(value)
}

pub(crate) fn configure(
    endpoint_value: Option<&str>,
    clear_endpoint: bool,
    profile: Option<Profile>,
) -> Result<Value> {
    if endpoint_value.is_some() && clear_endpoint {
        return Err(invalid("--endpoint conflicts with --clear-endpoint"));
    }
    let settings_lock = acquire_lock("telemetry.lock", true)?.expect("waiting lock returns a lock");
    let mut config = load()?;
    let mut binding_changed = false;
    if let Some(value) = endpoint_value {
        let value = endpoint(value)?;
        binding_changed |= config.endpoint.as_deref() != Some(value.as_str());
        config.endpoint = Some(value);
    }
    if clear_endpoint {
        binding_changed = true;
        config.endpoint = None;
    }
    if let Some(profile) = profile {
        binding_changed |= config.profile != profile;
        config.profile = profile;
    }
    if binding_changed {
        clear_collection(&mut config);
        config.notice_version = None;
        config.notice_seen_at = None;
        config.consent_granted_at = None;
        if config.preference != Preference::Off {
            config.preference = Preference::Unset;
            config.preference_changed_at = None;
        }
    }
    save(&config)?;
    drop(settings_lock);
    if binding_changed {
        persisted_fence(
            fence_sender(),
            "telemetry configuration was saved, but confirmation that an older sender completed failed",
        )?;
    }
    status(false, false)
}

pub(crate) fn set_on() -> Result<Value> {
    let _lock = acquire_lock("telemetry.lock", true)?.expect("waiting lock returns a lock");
    let mut config = load()?;
    eprintln!("{DISCLOSURE}");
    let timestamp = now();
    config.preference = Preference::On;
    config.preference_changed_at = Some(timestamp);
    config.consent_granted_at = Some(timestamp);
    config.notice_version = Some(NOTICE_VERSION);
    config.notice_seen_at = Some(timestamp);
    save(&config)?;
    status(false, false)
}

pub(crate) fn set_off() -> Result<Value> {
    let settings_lock = acquire_lock("telemetry.lock", true)?.expect("waiting lock returns a lock");
    // Opt-out is recovery-safe: a corrupt prior telemetry file cannot prevent disabling.
    let mut config = load().unwrap_or_default();
    config.preference = Preference::Off;
    config.preference_changed_at = Some(now());
    config.consent_granted_at = None;
    clear_collection(&mut config);
    save(&config)?;
    drop(settings_lock);
    // The sender owns this fence across HTTP. Once this returns, no sender started from older
    // state can still initiate a request.
    persisted_fence(
        fence_sender(),
        "the telemetry off setting was saved, but confirmation that an older sender completed failed",
    )?;
    status(false, false)
}

fn clear_collection(config: &mut Config) {
    config.installation_id = None;
    config.hub_id = None;
    config.queue.clear();
    config.sent_keys.clear();
}

fn fence_sender() -> Result<()> {
    let fence =
        acquire_lock("telemetry-sender.lock", true)?.expect("waiting sender lock returns a lock");
    drop(fence);
    Ok(())
}

fn persisted_fence(result: Result<()>, context: &str) -> Result<()> {
    result.map_err(|error| Error::Conflict(format!("{context}: {error}")))
}

fn source_id(config: &mut Config, source: SourceKind) -> Result<String> {
    let slot = match source {
        SourceKind::Installation => &mut config.installation_id,
        SourceKind::Hub => &mut config.hub_id,
    };
    if slot.is_none() {
        *slot = Some(random_id()?);
    }
    Ok(slot.clone().expect("source identifier initialized"))
}

fn dedup_key(source: SourceKind, feature: Feature, day: &str) -> String {
    if matches!(feature, Feature::Installation | Feature::FirstUse) {
        format!("{source:?}:{feature:?}:once")
    } else {
        format!("{source:?}:{feature:?}:{day}")
    }
}

fn push(
    config: &mut Config,
    source: SourceKind,
    feature: Feature,
    outcome: Outcome,
) -> Result<bool> {
    let timestamp = now();
    let day = utc_day(timestamp);
    let key = dedup_key(source, feature, &day);
    if config.sent_keys.contains(&key)
        || config.queue.iter().any(|event| event.dedup_key == key)
        || config.queue.len() >= MAX_QUEUE
    {
        return Ok(false);
    }
    let _ = source_id(config, source)?;
    config.queue.push(QueuedEvent {
        event_id: random_id()?,
        day,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: match std::env::consts::OS {
            "linux" => Platform::Linux,
            "macos" => Platform::Macos,
            "windows" => Platform::Windows,
            _ => Platform::Other,
        },
        arch: match std::env::consts::ARCH {
            "x86_64" => Architecture::X86_64,
            "aarch64" => Architecture::Aarch64,
            _ => Architecture::Other,
        },
        feature,
        outcome,
        count: 1,
        source,
        dedup_key: key,
        attempts: 0,
        created_at: timestamp,
        next_attempt_at: timestamp,
    });
    config.sent_keys.push(
        config
            .queue
            .last()
            .expect("event appended")
            .dedup_key
            .clone(),
    );
    trim_dedup_keys(config);
    Ok(true)
}

fn trim_dedup_keys(config: &mut Config) {
    while config.sent_keys.len() > MAX_SENT_KEYS {
        let removable = config
            .sent_keys
            .iter()
            .position(|key| !key.ends_with(":once"));
        match removable {
            Some(index) => {
                config.sent_keys.remove(index);
            }
            None => break,
        }
    }
}

pub(crate) fn record(feature: Feature, outcome: Outcome, invocation_off: bool) {
    // The lock creates its parent directory, so first confirm collection could be effective.
    let Ok(config) = load() else { return };
    if effective(&config, invocation_off).0 != "enabled" {
        return;
    }
    let Ok(Some(_lock)) = acquire_lock("telemetry.lock", false) else {
        return;
    };
    let Ok(mut config) = load() else { return };
    if effective(&config, invocation_off).0 != "enabled" {
        return;
    }
    let Ok(added) = push_observation(&mut config, feature, outcome) else {
        return;
    };
    let due = config
        .queue
        .iter()
        .any(|event| event.next_attempt_at <= now());
    if added && save(&config).is_err() {
        return;
    }
    drop(_lock);
    if added || due {
        spawn_sender(invocation_off);
    }
}

fn push_observation(config: &mut Config, feature: Feature, outcome: Outcome) -> Result<bool> {
    // CLI activity belongs to this installation. Hub IDs are reserved for a future reporter
    // running inside one concrete Hub authority; a client managing remote Hubs cannot invent one.
    let source = SourceKind::Installation;
    let first_use = if activation_feature(feature) && matches!(outcome, Outcome::Succeeded) {
        push(config, source, Feature::FirstUse, Outcome::Succeeded)?
    } else {
        false
    };
    Ok(push(config, source, feature, outcome)? || first_use)
}

fn activation_feature(feature: Feature) -> bool {
    !matches!(
        feature,
        Feature::Installation | Feature::Quickstart | Feature::Uninstall | Feature::Setup
    )
}

pub(crate) fn record_installation(invocation_off: bool) {
    record(Feature::Installation, Outcome::Succeeded, invocation_off);
}

fn spawn_sender(invocation_off: bool) {
    if invocation_off || env_off() || ci() {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .args(["telemetry", "send"])
        .env_remove("AGENTSCM_VERIFIED_INSTALL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x08000000);
    let _ = command.spawn();
}

trait Transport: Send + Sync {
    fn post(
        &self,
        endpoint: String,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + '_>>;
}

struct HttpTransport;

impl Transport for HttpTransport {
    fn post(
        &self,
        endpoint: String,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + '_>> {
        Box::pin(async move {
            let client = match reqwest::Client::builder()
                .redirect(Policy::none())
                .connect_timeout(timeout.min(Duration::from_secs(1)))
                .timeout(timeout.min(Duration::from_secs(3)))
                .build()
            {
                Ok(client) => client,
                Err(_) => return Delivery::Retry(60),
            };
            match client
                .post(endpoint)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(response) => classify_status(
                    response.status(),
                    response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok()),
                ),
                Err(_) => Delivery::Retry(60),
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DispatchBinding {
    endpoint: String,
    profile: Profile,
    preference: Preference,
    notice_version: Option<u32>,
    notice_seen_at: Option<i64>,
    consent_granted_at: Option<i64>,
    source: SourceKind,
    source_id: String,
}

struct PreparedBatch {
    binding: DispatchBinding,
    ids: Vec<String>,
    body: Vec<u8>,
}

fn dispatch_binding(config: &Config, source: SourceKind) -> Option<DispatchBinding> {
    let source_id = match source {
        SourceKind::Installation => config.installation_id.clone(),
        SourceKind::Hub => config.hub_id.clone(),
    }?;
    Some(DispatchBinding {
        endpoint: config.endpoint.clone()?,
        profile: config.profile,
        preference: config.preference,
        notice_version: config.notice_version,
        notice_seen_at: config.notice_seen_at,
        consent_granted_at: config.consent_granted_at,
        source,
        source_id,
    })
}

fn prepare_batch(sender_started: Instant) -> Result<Option<PreparedBatch>> {
    let Some(_settings_lock) = acquire_lock("telemetry.lock", false)? else {
        return Ok(None);
    };
    let mut config = load()?;
    if effective(&config, false).0 != "enabled" || config.queue.is_empty() {
        return Ok(None);
    }
    let timestamp = now();
    let before = config.queue.len();
    config.queue.retain(|event| {
        event.created_at >= timestamp.saturating_sub(7 * 86_400) && event.attempts < MAX_ATTEMPTS
    });
    let cleaned = config.queue.len() != before;
    let Some(source) = config
        .queue
        .iter()
        .find(|event| event.next_attempt_at <= timestamp)
        .map(|event| event.source)
    else {
        if cleaned {
            save(&config)?;
        }
        return Ok(None);
    };
    let binding = dispatch_binding(&config, source)
        .ok_or_else(|| invalid("telemetry batch has no dispatch binding"))?;
    let ids = config
        .queue
        .iter()
        .filter(|event| event.source == source && event.next_attempt_at <= timestamp)
        .take(MAX_QUEUE)
        .map(|event| event.event_id.clone())
        .collect::<Vec<_>>();
    if MAX_SENDER_LIFETIME
        .checked_sub(sender_started.elapsed())
        .is_none_or(|remaining| remaining < MIN_HTTP_BUDGET)
    {
        return Ok(None);
    }
    // Reserve the attempt before network dispatch. A killed sender or contended finish step can
    // lose an observation, but it cannot retry the same identity beyond the configured bound.
    for event in &mut config.queue {
        if ids.contains(&event.event_id) {
            event.attempts = event.attempts.saturating_add(1);
            event.next_attempt_at = timestamp.saturating_add(60);
        }
    }
    save(&config)?;
    let chosen = config
        .queue
        .iter()
        .filter(|event| ids.contains(&event.event_id))
        .collect::<Vec<_>>();
    let body = json!({
        "schema":"agentscm.telemetry.batch/v1",
        "source":{"kind":source,"id":binding.source_id},
        "events":chosen.iter().map(|event| WireEvent {
            event_id:&event.event_id, day:&event.day, version:&event.version,
            os:event.os, arch:event.arch, feature:event.feature,
            outcome:event.outcome, count:event.count,
        }).collect::<Vec<_>>()
    });
    let body = serde_json::to_vec(&body)?;
    if body.len() > MAX_BATCH_BYTES {
        return Err(invalid("telemetry batch exceeds the size limit"));
    }
    Ok(Some(PreparedBatch { binding, ids, body }))
}

fn binding_still_current(config: &Config, batch: &PreparedBatch) -> bool {
    effective(config, false).0 == "enabled"
        && dispatch_binding(config, batch.binding.source).as_ref() == Some(&batch.binding)
        && batch
            .ids
            .iter()
            .all(|id| config.queue.iter().any(|event| &event.event_id == id))
}

pub(crate) async fn send(invocation_off: bool) {
    send_with(&HttpTransport, invocation_off).await;
}

async fn send_with(transport: &dyn Transport, invocation_off: bool) {
    if invocation_off {
        return;
    }
    let Ok(Some(_sender_lock)) = acquire_lock("telemetry-sender.lock", false) else {
        return;
    };
    let started = Instant::now();
    for _ in 0..MAX_SENDER_BATCHES {
        if started.elapsed() >= MAX_SENDER_LIFETIME {
            break;
        }
        let mut batch = match prepare_batch(started) {
            Ok(Some(batch)) => batch,
            _ => break,
        };

        let Some(remaining) = MAX_SENDER_LIFETIME.checked_sub(started.elapsed()) else {
            break;
        };
        let delivery = transport
            .post(
                batch.binding.endpoint.clone(),
                std::mem::take(&mut batch.body),
                remaining,
            )
            .await;
        let timestamp = now();
        let result = (|| -> Result<bool> {
            let Some(_finish_lock) = acquire_lock("telemetry.lock", false)? else {
                return Ok(false);
            };
            let mut config = load()?;
            if !binding_still_current(&config, &batch) {
                return Ok(true);
            }
            config
                .queue
                .retain(|event| event.created_at >= timestamp.saturating_sub(7 * 86_400));
            if matches!(delivery, Delivery::Accepted | Delivery::Drop) {
                config
                    .queue
                    .retain(|event| !batch.ids.contains(&event.event_id));
            } else if let Delivery::Retry(delay) = delivery {
                for event in &mut config.queue {
                    if batch.ids.contains(&event.event_id) {
                        let backoff = delay.saturating_mul(1u64 << event.attempts.min(5));
                        event.next_attempt_at = timestamp.saturating_add(backoff.min(3_600) as i64);
                    }
                }
                config.queue.retain(|event| event.attempts < MAX_ATTEMPTS);
            }
            save(&config)?;
            Ok(true)
        })();
        if !matches!(result, Ok(true)) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, MutexGuard, mpsc};

    use tokio::sync::Notify;

    use super::*;

    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TestDirectoryGuard {
        _serial: MutexGuard<'static, ()>,
    }

    impl TestDirectoryGuard {
        fn new(path: &Path) -> Self {
            let serial = TEST_SERIAL
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *TEST_DIRECTORY
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(path.to_path_buf());
            Self { _serial: serial }
        }
    }

    impl Drop for TestDirectoryGuard {
        fn drop(&mut self) {
            *TEST_DIRECTORY
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
    }

    struct BlockingTransport {
        calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
    }

    impl BlockingTransport {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                first_started: Notify::new(),
                release_first: Notify::new(),
            }
        }
    }

    impl Transport for BlockingTransport {
        fn post(
            &self,
            _endpoint: String,
            _body: Vec<u8>,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = Delivery> + Send + '_>> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    self.first_started.notify_one();
                    self.release_first.notified().await;
                }
                Delivery::Accepted
            })
        }
    }

    fn enabled_config(feature: Feature) -> Config {
        let timestamp = now();
        let mut config = Config {
            profile: Profile::PermittedDefault,
            preference: Preference::On,
            endpoint: Some("https://telemetry.example/v1/telemetry/batches".into()),
            notice_version: Some(NOTICE_VERSION),
            notice_seen_at: Some(timestamp),
            preference_changed_at: Some(timestamp),
            ..Config::default()
        };
        push(
            &mut config,
            SourceKind::Installation,
            feature,
            Outcome::Succeeded,
        )
        .unwrap();
        config
    }

    #[test]
    fn utc_days_are_zero_padded_dates() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn endpoint_is_narrow() {
        assert!(endpoint("https://telemetry.example/v1/telemetry/batches").is_ok());
        for bad in [
            "http://telemetry.example/v1/telemetry/batches",
            "https://user@telemetry.example/v1/telemetry/batches",
            "https://telemetry.example/v1/telemetry/batch",
            "https://telemetry.example/v1/telemetry/batches?q=x",
        ] {
            assert!(endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn feature_day_is_deduplicated_and_first_outcome_wins() {
        let mut config = Config::default();
        push(
            &mut config,
            SourceKind::Installation,
            Feature::Work,
            Outcome::Failed,
        )
        .unwrap();
        push(
            &mut config,
            SourceKind::Installation,
            Feature::Work,
            Outcome::Succeeded,
        )
        .unwrap();
        assert_eq!(config.queue.len(), 1);
        assert!(matches!(config.queue[0].outcome, Outcome::Failed));
    }

    #[test]
    fn retry_classification_is_bounded_and_redirects_are_permanent() {
        assert_eq!(
            classify_status(reqwest::StatusCode::ACCEPTED, None),
            Delivery::Accepted
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::OK, None),
            Delivery::Drop
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::NO_CONTENT, None),
            Delivery::Drop
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("99999")),
            Delivery::Retry(3_600)
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, Some("1")),
            Delivery::Retry(30)
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::TEMPORARY_REDIRECT, None),
            Delivery::Drop
        );
        assert_eq!(
            classify_status(reqwest::StatusCode::BAD_REQUEST, None),
            Delivery::Drop
        );
    }

    #[test]
    fn first_use_requires_a_successful_meaningful_action() {
        let mut config = Config::default();
        push_observation(&mut config, Feature::Installation, Outcome::Succeeded).unwrap();
        assert_eq!(config.queue.len(), 1);
        assert!(matches!(config.queue[0].feature, Feature::Installation));

        push_observation(&mut config, Feature::Work, Outcome::Failed).unwrap();
        assert_eq!(config.queue.len(), 2);
        assert!(
            !config
                .queue
                .iter()
                .any(|event| matches!(event.feature, Feature::FirstUse))
        );

        push_observation(&mut config, Feature::Work, Outcome::Succeeded).unwrap();
        assert!(
            config
                .queue
                .iter()
                .any(|event| matches!(event.feature, Feature::FirstUse))
        );
    }

    #[test]
    fn setup_lifecycle_successes_do_not_count_as_first_use() {
        let mut config = Config::default();
        for feature in [Feature::Quickstart, Feature::Uninstall, Feature::Setup] {
            push_observation(&mut config, feature, Outcome::Succeeded).unwrap();
        }
        assert!(
            !config
                .queue
                .iter()
                .any(|event| matches!(event.feature, Feature::FirstUse))
        );

        push_observation(&mut config, Feature::Work, Outcome::Succeeded).unwrap();
        assert!(
            config
                .queue
                .iter()
                .any(|event| matches!(event.feature, Feature::FirstUse))
        );
    }

    #[test]
    fn persisted_fence_errors_report_the_saved_control_state() {
        let off = persisted_fence(
            Err(Error::Conflict("sender still active".into())),
            "the telemetry off setting was saved, but confirmation that an older sender completed failed",
        )
        .unwrap_err()
        .to_string();
        assert!(off.contains("the telemetry off setting was saved"));
        assert!(off.contains("older sender completed failed"));

        let configured = persisted_fence(
            Err(Error::Conflict("sender still active".into())),
            "telemetry configuration was saved, but confirmation that an older sender completed failed",
        )
        .unwrap_err()
        .to_string();
        assert!(configured.contains("telemetry configuration was saved"));
    }

    #[test]
    fn once_markers_survive_the_rolling_dedup_bound() {
        let mut config = Config::default();
        config
            .sent_keys
            .push("Installation:Installation:once".into());
        config.sent_keys.push("Installation:FirstUse:once".into());
        for index in 0..MAX_SENT_KEYS {
            config.sent_keys.push(format!("ordinary:{index}"));
        }
        trim_dedup_keys(&mut config);
        assert_eq!(config.sent_keys.len(), MAX_SENT_KEYS);
        assert!(
            config
                .sent_keys
                .iter()
                .any(|key| key.ends_with("Installation:once"))
        );
        assert!(
            config
                .sent_keys
                .iter()
                .any(|key| key.ends_with("FirstUse:once"))
        );
    }

    #[test]
    fn durable_replace_cleans_up_after_failed_rename() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        assert!(durable_replace(&destination, b"new state").is_err());
        let leftovers = std::fs::read_dir(temporary.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("destination")]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opt_out_waits_for_the_sender_fence() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();

        let transport = Arc::new(BlockingTransport::new());
        let sender_transport = Arc::clone(&transport);
        let sender = tokio::spawn(async move { send_with(sender_transport.as_ref(), false).await });
        tokio::time::timeout(Duration::from_secs(10), transport.first_started.notified())
            .await
            .expect("sender did not start");

        let (done_tx, done_rx) = mpsc::channel();
        let opt_out = std::thread::spawn(move || {
            done_tx.send(set_off()).unwrap();
        });
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        transport.release_first.notify_one();
        sender.await.unwrap();
        let status = done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        opt_out.join().unwrap();
        assert_eq!(status["requested"], "off");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let config = load().unwrap();
        assert_eq!(config.preference, Preference::Off);
        assert!(config.queue.is_empty());
        assert!(config.installation_id.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sender_drains_an_event_enqueued_while_the_first_batch_is_pending() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();

        let transport = Arc::new(BlockingTransport::new());
        let sender_transport = Arc::clone(&transport);
        let sender = tokio::spawn(async move { send_with(sender_transport.as_ref(), false).await });
        tokio::time::timeout(Duration::from_secs(10), transport.first_started.notified())
            .await
            .expect("sender did not start");

        {
            let _settings_lock = acquire_lock("telemetry.lock", true).unwrap().unwrap();
            let mut config = load().unwrap();
            push(
                &mut config,
                SourceKind::Installation,
                Feature::Work,
                Outcome::Failed,
            )
            .unwrap();
            save(&config).unwrap();
        }
        transport.release_first.notify_one();
        sender.await.unwrap();

        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
        assert!(load().unwrap().queue.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sender_drops_immediately_when_the_common_state_lock_is_busy() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();
        let settings_lock = acquire_lock("telemetry.lock", true).unwrap().unwrap();
        let transport = BlockingTransport::new();

        let started = Instant::now();
        send_with(&transport, false).await;
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        drop(settings_lock);
        let config = load().unwrap();
        assert_eq!(config.queue.len(), 1);
        assert_eq!(config.queue[0].attempts, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invocation_opt_out_skips_sender_without_touching_the_queue() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();
        let before = std::fs::read(path().unwrap()).unwrap();
        let transport = BlockingTransport::new();

        send_with(&transport, true).await;

        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(path().unwrap()).unwrap(), before);
    }

    #[test]
    #[cfg(unix)]
    fn duplicate_without_due_work_does_not_rewrite_state() {
        use std::os::unix::fs::MetadataExt;

        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        let mut config = enabled_config(Feature::Installation);
        config.queue[0].next_attempt_at = now().saturating_add(3_600);
        save(&config).unwrap();
        let inode = std::fs::metadata(path().unwrap()).unwrap().ino();

        record(Feature::Installation, Outcome::Succeeded, false);

        assert_eq!(std::fs::metadata(path().unwrap()).unwrap().ino(), inode);
        assert_eq!(load().unwrap().queue.len(), 1);
    }

    #[test]
    fn dispatch_attempts_are_reserved_before_delivery() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();

        for expected in 1..=MAX_ATTEMPTS {
            let batch = prepare_batch(Instant::now()).unwrap();
            assert!(batch.is_some());
            let mut config = load().unwrap();
            assert_eq!(config.queue[0].attempts, expected);
            config.queue[0].next_attempt_at = now();
            save(&config).unwrap();
        }
        assert!(prepare_batch(Instant::now()).unwrap().is_none());
        assert!(load().unwrap().queue.is_empty());
    }

    #[test]
    fn exhausted_http_budget_does_not_reserve_an_attempt() {
        let temporary = tempfile::tempdir().unwrap();
        let _directory = TestDirectoryGuard::new(temporary.path());
        save(&enabled_config(Feature::Installation)).unwrap();

        let exhausted = Instant::now() - MAX_SENDER_LIFETIME;
        assert!(prepare_batch(exhausted).unwrap().is_none());
        let config = load().unwrap();
        assert_eq!(config.queue.len(), 1);
        assert_eq!(config.queue[0].attempts, 0);
    }
}
