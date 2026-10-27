use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentscm::hub::HubError;
use agentscm::hub::forge_http::{self, check_base_url, send};
use agentscm::hub::http::{self, HubConfig};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::{Subcommand, ValueEnum};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_DATABASE_URL: &str = "sqlite:///data/hub.sqlite";
const LOOPBACK_WAIT: Duration = Duration::from_secs(600);
const HUB_BODY_BYTES: usize = 4 * 1024 * 1024;
const SESSION_FILE_BYTES: u64 = 64 * 1024;
const MAX_SAVED_SESSIONS: usize = 32;
const MAX_SESSION_BYTES: usize = 4096;
const SESSION_LOCK_WAIT: Duration = Duration::from_secs(1);
const SESSION_LOCK_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Provider {
    Github,
    Gitlab,
}

#[derive(Debug, Subcommand)]
pub enum HubCommand {
    /// Configure and verify the optional private Slack integration.
    Slack {
        #[command(subcommand)]
        command: crate::slack_cli::SlackCommand,
    },
    /// Serve the hub API. The database comes from AGENTSCM_DATABASE_URL
    /// (default sqlite:///data/hub.sqlite), never from a flag. Sign-in is
    /// enabled by AGENTSCM_GITHUB_* and AGENTSCM_GITLAB_* variables.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7332")]
        bind: SocketAddr,
        /// Allow a non-loopback bind address.
        #[arg(long)]
        public: bool,
        #[arg(long, default_value = "/data/objects")]
        objects: PathBuf,
        /// URL browsers reach this hub at (default http://<bind>).
        #[arg(long)]
        public_url: Option<String>,
        /// Directory for GitHub App credentials (mode 0700, created if absent).
        #[arg(long, default_value = "/data/secrets")]
        secrets: PathBuf,
        /// Seconds between GitHub polls.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(10..))]
        poll_seconds: u64,
        /// How many days back a branch's last commit counts as recent.
        #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(i64).range(1..=365))]
        activity_days: i64,
        /// Days an uploaded shared-conversation body stays readable before it expires.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(i64).range(1..=3650))]
        shared_conversation_retention_days: i64,
    },
    /// Sign in to a hub; the session is saved for the token commands.
    Login {
        /// Hub URL, such as https://hub.example.com (default: AGENTSCM_HUB, then the only saved Hub).
        #[arg(long)]
        hub: Option<String>,
        /// Forge to sign in with (default: the Hub's only sign-in provider).
        #[arg(long, value_enum)]
        provider: Option<Provider>,
    },
    /// End one saved session on its hub and remove it locally.
    Logout {
        /// Hub to sign out of (default: the current saved hub).
        #[arg(long)]
        hub: Option<String>,
    },
    /// Inspect or remove saved Hub sign-ins without showing their secrets.
    Sessions {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage agent tokens with the saved session.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Connect GitHub (administrators).
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// List and manage forge connections (administrators).
    Connections {
        #[command(subcommand)]
        command: ConnectionCommand,
    },
    /// Publish a git-export mirror's exported revision as a GitHub branch and pull request,
    /// through the hub. Uses the agent token configured like `agentscm intent`.
    #[cfg(unix)]
    Publish(crate::publish_cli::PublishArgs),
}

#[derive(Debug, Subcommand)]
pub enum GithubCommand {
    /// Start creating a GitHub App; the setup URL is printed alone on stdout.
    Setup {
        #[arg(long)]
        name: String,
        /// Create the app in this organization instead of your account.
        #[arg(long)]
        org: Option<String>,
        #[arg(long)]
        hub: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(Debug, Subcommand)]
pub enum ConnectionCommand {
    List {
        #[arg(long)]
        hub: Option<String>,
    },
    /// Discover repositories and poll the connection now.
    Refresh {
        id: String,
        #[arg(long)]
        hub: Option<String>,
    },
    /// Turn the pull request comment and commit status on or off.
    Outbound {
        id: String,
        #[arg(value_enum)]
        state: Switch,
        #[arg(long)]
        hub: Option<String>,
    },
    /// Connect one repository to a PAT connection.
    AddRepo {
        id: String,
        /// owner/name
        repo: String,
        #[arg(long)]
        hub: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Create a token for one repository; the secret is printed alone on stdout.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        repo: String,
        /// Lifetime such as 90s, 30m, 8h or 7d (at most 30d).
        #[arg(long, default_value = "8h")]
        ttl: String,
        /// A token that may only read (for the status page); the hub refuses it on every write.
        #[arg(long)]
        read_only: bool,
        #[arg(long)]
        hub: Option<String>,
    },
    List {
        #[arg(long)]
        hub: Option<String>,
    },
    Revoke {
        id: String,
        #[arg(long)]
        hub: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// List saved Hub sign-ins and their local expiry.
    List,
    /// Remove a saved sign-in locally without contacting its Hub.
    Remove {
        #[arg(long)]
        hub: String,
    },
}

pub(crate) fn telemetry_action(command: &HubCommand) -> bool {
    match command {
        HubCommand::Serve { .. } | HubCommand::Slack { .. } => false,
        HubCommand::Token {
            command: TokenCommand::List { .. },
        }
        | HubCommand::Connections {
            command: ConnectionCommand::List { .. },
        } => false,
        HubCommand::Sessions {
            command: SessionCommand::List,
        } => false,
        HubCommand::Login { .. }
        | HubCommand::Logout { .. }
        | HubCommand::Sessions { .. }
        | HubCommand::Token { .. }
        | HubCommand::Github { .. }
        | HubCommand::Connections { .. } => true,
        #[cfg(unix)]
        HubCommand::Publish(_) => true,
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
pub async fn run(command: HubCommand, data_dir: &std::path::Path) -> Result<()> {
    match command {
        HubCommand::Slack { command } => crate::slack_cli::run(command).await,
        HubCommand::Serve {
            bind,
            public,
            objects,
            public_url,
            secrets,
            poll_seconds,
            activity_days,
            shared_conversation_retention_days,
        } => {
            let database_url = match std::env::var("AGENTSCM_DATABASE_URL") {
                Ok(url) => url,
                Err(std::env::VarError::NotPresent) => DEFAULT_DATABASE_URL.into(),
                Err(_) => {
                    return Err(Error::Invalid(
                        "AGENTSCM_DATABASE_URL is not valid unicode".into(),
                    ));
                }
            };
            http::serve(HubConfig {
                database_url,
                objects,
                bind,
                public,
                public_url,
                secrets,
                poll_seconds,
                activity_days,
                shared_conversation_retention_days,
            })
            .await
            .map_err(map_hub_error)
        }
        HubCommand::Login { hub, provider } => {
            let hub = match hub {
                Some(hub) => check_base_url(&hub).map_err(map_hub_error)?,
                None => default_hub()?,
            };
            let name = match provider {
                Some(provider) => provider
                    .to_possible_value()
                    .map(|v| v.get_name().to_owned())
                    .unwrap_or_default(),
                None => only_provider(&hub).await?,
            };
            login(&hub, &name).await
        }
        HubCommand::Logout { hub } => logout(hub.as_deref()).await,
        HubCommand::Sessions { command } => sessions(command),
        HubCommand::Token { command } => token(command).await,
        HubCommand::Github { command } => github(command).await,
        HubCommand::Connections { command } => connections(command).await,
        #[cfg(unix)]
        HubCommand::Publish(args) => crate::publish_cli::run(args, data_dir).await,
    }
}

pub(crate) fn map_hub_error(error: HubError) -> Error {
    match error {
        HubError::Invalid(message) => Error::Invalid(message),
        HubError::Unauthorized => Error::Invalid("hub authentication required".into()),
        HubError::Busy => Error::Invalid("hub busy".into()),
        HubError::WorkCapacity => Error::Invalid(
            "repository work storage capacity reached; operator compaction required".into(),
        ),
        HubError::WorkOverlap(_) => {
            Error::Invalid("declared work scope overlaps another assignment".into())
        }
        HubError::WorkCoordinationRequired { .. } => {
            Error::Invalid("work scope needs coordination; inspect the agent inbox".into())
        }
        HubError::WorkOverlapIncomplete => {
            Error::Invalid("work overlap coverage is incomplete; no scope was claimed".into())
        }
        HubError::WorkPreflight(_) => {
            Error::Invalid("work preflight requires review; no scope was claimed".into())
        }
        HubError::Unavailable(message) => Error::Invalid(format!("hub unavailable: {message}")),
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

/// One request to the hub; `body` is JSON.
pub(crate) async fn call(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, Value)> {
    let mut request = client.request(method, url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    send(request, HUB_BODY_BYTES).await.map_err(map_hub_error)
}

/// The Hub for a command run without `--hub`.
pub(crate) fn default_hub() -> Result<String> {
    let sessions = if session_files_present()? {
        load_sessions()?.sessions
    } else {
        Vec::new()
    };
    let saved: Vec<&str> = sessions.iter().map(|saved| saved.hub.as_str()).collect();
    pick_default_hub(env_hub(), &saved)
}

/// `AGENTSCM_HUB`, when set: the Hub for commands run without `--hub`.
fn env_hub() -> Option<String> {
    std::env::var("AGENTSCM_HUB")
        .ok()
        .map(|hub| hub.trim().to_owned())
        .filter(|hub| !hub.is_empty())
}

/// After `--hub`: `AGENTSCM_HUB`, then the one saved sign-in; with none or several, say how to choose.
fn pick_default_hub(env: Option<String>, saved: &[&str]) -> Result<String> {
    if let Some(hub) = env {
        return check_base_url(&hub).map_err(map_hub_error);
    }
    match saved {
        [only] => Ok((*only).to_owned()),
        [] => Err(invalid(
            "pass --hub with the Hub's URL, such as --hub https://hub.example.com, or set AGENTSCM_HUB",
        )),
        _ => Err(invalid(
            "several Hubs are saved; pass --hub or set AGENTSCM_HUB to choose one (see agentscm hub sessions list)",
        )),
    }
}

/// The Hub's only sign-in provider; with several, `--provider` must say which.
async fn only_provider(hub: &str) -> Result<String> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let (status, reply) = call(
        &client,
        Method::GET,
        &format!("{hub}/v1/hub/login"),
        None,
        None,
    )
    .await?;
    let names: Vec<&str> = reply["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| matches!(*name, "github" | "gitlab"))
        .collect();
    match (status, names.as_slice()) {
        (200, [only]) => Ok((*only).to_owned()),
        (200, []) => Err(invalid("this Hub has no sign-in provider configured")),
        (200, _) => Err(invalid(&format!(
            "this Hub signs in with {}; pass --provider to choose one",
            names.join(" or ")
        ))),
        // A Hub from before the provider list.
        _ => Err(invalid("pass --provider github or --provider gitlab")),
    }
}

async fn login(hub: &str, provider: &str) -> Result<()> {
    let who = sign_in(hub, provider, None).await?;
    eprintln!("signed in to {hub} as {who}");
    Ok(())
}

/// Signs in and saves the session; returns `provider:login`. With `open_host`, the browser
/// is pointed at the sign-in page where the platform allows it, but never at a forge page
/// on another host (the URL is printed either way).
pub(crate) async fn sign_in(hub: &str, provider: &str, open_host: Option<&str>) -> Result<String> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let start = format!("{hub}/v1/hub/login/{provider}/device");
    let (status, device) = call(&client, Method::POST, &start, None, None).await?;
    let signed = match status {
        200 => device_login(&client, hub, &device, open_host).await?,
        409 => loopback_login(&client, hub, provider, open_host.is_some()).await?,
        404 => return Err(invalid("this hub does not offer that provider")),
        _ => return Err(invalid("hub could not start sign-in")),
    };
    let secret = signed["session"]
        .as_str()
        .ok_or_else(|| invalid("hub returned no session"))?;
    save_session(hub, secret, signed["expires_at"].as_i64())?;
    let user = &signed["user"];
    Ok(format!(
        "{}:{}",
        clean(user["provider"].as_str().unwrap_or(provider)),
        clean(user["login"].as_str().unwrap_or("?"))
    ))
}

/// Whether `page` is an https page on exactly `host`.
pub(crate) fn opens_page(page: &str, host: &str) -> bool {
    reqwest::Url::parse(page)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some(host))
}

/// Best effort: ask the desktop to open `url`. The caller prints the URL as well.
pub(crate) fn open_url(url: &str) {
    // No shell on Windows: `cmd /C start` would parse `&`, `|`, `^` and `%` in the URL.
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        std::process::Command::new(if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        })
    };
    let _ = command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Forge-supplied text must not carry terminal escapes.
pub(crate) fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// The Hub's own sentence for any refusal kind that carries one (the reason and its one fix,
/// such as `insufficient_role`); `None` when the reply has none. Bounded, one terminal line.
pub(crate) fn refusal_message(reply: &Value) -> Option<String> {
    reply["error"].as_str()?;
    let message = clean(reply["message"].as_str()?);
    Some(message.chars().take(300).collect())
}

/// Ends the saved session when it belongs to `hub`: the hub is told (best effort) and only
/// that exact local session is removed. A concurrently replaced session is preserved.
pub(crate) async fn end_session(hub: &str) -> Result<bool> {
    let Ok((saved, session)) = load_saved_session(Some(hub)) else {
        return Ok(false);
    };
    if let Ok(client) = forge_http::client() {
        let url = format!("{saved}/v1/hub/session");
        let _ = call(&client, Method::DELETE, &url, Some(&session), None).await;
    }
    remove_saved_session_if(&saved, Some(&session))
}

async fn logout(hub: Option<&str>) -> Result<()> {
    if hub.is_none() && !session_files_present()? {
        eprintln!("not signed in");
        return Ok(());
    }
    let selected = load_saved_session(hub)?;
    // The hub may be unreachable; the local entry goes either way.
    if let Ok(client) = forge_http::client() {
        let url = format!("{}/v1/hub/session", selected.0);
        let _ = call(&client, Method::DELETE, &url, Some(&selected.1), None).await;
    }
    if remove_saved_session_if(&selected.0, Some(&selected.1))? {
        eprintln!("signed out of {}", selected.0);
    } else {
        eprintln!("a newer sign-in for {} was preserved", selected.0);
    }
    Ok(())
}

fn sessions(command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::List => {
            if !session_files_present()? {
                eprintln!("no saved Hub sessions");
                return Ok(());
            }
            let store = load_sessions()?;
            for line in session_lines(&store, unix_now()) {
                println!("{line}");
            }
            Ok(())
        }
        SessionCommand::Remove { hub } => {
            let normalized = check_base_url(&hub).map_err(map_hub_error)?;
            if !forget_session(&normalized)? {
                return Err(invalid("no saved session for this hub"));
            }
            eprintln!("removed local session for {normalized}");
            Ok(())
        }
    }
}

async fn device_login(
    client: &reqwest::Client,
    hub: &str,
    device: &Value,
    open_host: Option<&str>,
) -> Result<Value> {
    let handle = device["handle"]
        .as_str()
        .ok_or_else(|| invalid("bad hub reply"))?;
    let page = clean(
        device["verification_uri"]
            .as_str()
            .unwrap_or("the forge's device page"),
    );
    eprintln!(
        "open {page} and enter the code {}",
        clean(device["user_code"].as_str().unwrap_or("?"))
    );
    if open_host.is_some_and(|host| opens_page(&page, host)) {
        open_url(&page);
    }
    let deadline =
        Instant::now() + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(900));
    let mut interval = device["interval"].as_u64().unwrap_or(5).clamp(1, 60);
    let url = format!("{hub}/v1/hub/login/device/poll");
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let body = json!({"handle": handle});
        // A hub or forge outage mid-sign-in is retried until the code expires.
        let Ok((status, reply)) = call(client, Method::POST, &url, None, Some(body)).await else {
            continue;
        };
        if status >= 500 {
            continue;
        }
        if status != 200 {
            return Err(invalid("sign-in failed"));
        }
        interval = reply["interval"]
            .as_u64()
            .map_or(interval, |s| s.clamp(1, 60));
        match reply["status"].as_str() {
            Some("signed_in") => return Ok(reply),
            Some("pending" | "slow_down") => {}
            Some("expired") => return Err(invalid("the device code expired; run login again")),
            Some("denied") => return Err(invalid("sign-in was denied")),
            _ => return Err(invalid("bad hub reply")),
        }
    }
    Err(invalid("the device code expired; run login again"))
}

async fn loopback_login(
    client: &reqwest::Client,
    hub: &str,
    provider: &str,
    open: bool,
) -> Result<Value> {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|_| invalid("random source"))?;
    let verifier: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let url = format!("{hub}/v1/hub/login/{provider}?cli_port={port}&cli_challenge={challenge}");
    eprintln!("open this URL in your browser:\n{url}");
    if open {
        open_url(&url);
    }
    let code = tokio::task::spawn_blocking(move || wait_for_code(&listener))
        .await
        .map_err(|_| invalid("sign-in wait failed"))??;
    let url = format!("{hub}/v1/hub/login/exchange");
    let body = json!({"code": code, "verifier": verifier});
    match call(client, Method::POST, &url, None, Some(body)).await? {
        (200, signed) => Ok(signed),
        _ => Err(invalid("sign-in failed")),
    }
}

/// Serve loopback connections until one is `GET /callback?code=<hex>`.
fn wait_for_code(listener: &TcpListener) -> Result<String> {
    let deadline = Instant::now() + LOOPBACK_WAIT;
    while Instant::now() < deadline {
        let Ok((mut stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        // An accepted socket can inherit the listener's non-blocking mode.
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut head = [0_u8; 4096];
        let read = stream.read(&mut head).unwrap_or(0);
        let code = std::str::from_utf8(&head[..read])
            .ok()
            .and_then(|text| text.lines().next())
            .and_then(|line| line.strip_prefix("GET /callback?code="))
            .and_then(|rest| rest.split(' ').next())
            .filter(|code| code.len() == 64 && code.bytes().all(|b| b.is_ascii_hexdigit()));
        let (status, text) = if code.is_some() {
            ("200 OK", "Signed in. You can close this tab.")
        } else {
            ("404 Not Found", "not found")
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        );
        if let Some(code) = code {
            return Ok(code.to_owned());
        }
    }
    Err(invalid("timed out waiting for the browser sign-in"))
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config`.
pub(crate) fn config_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(PathBuf::from(
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE")) // Windows has no HOME
                .ok_or_else(|| invalid("HOME is not set"))?,
        )
        .join(".config")),
    }
}

/// The legacy singleton imported into [`sessions_path`] on the next session write.
pub(crate) fn session_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("hub-session.json"))
}

/// The bounded multi-Hub human-session store. Agent tokens live elsewhere.
pub(crate) fn sessions_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("hub-sessions.json"))
}

fn session_lock_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("hub-sessions.lock"))
}

#[derive(Clone)]
struct SessionFiles {
    current: PathBuf,
    legacy: PathBuf,
    lock: PathBuf,
}

fn session_files() -> Result<SessionFiles> {
    Ok(SessionFiles {
        current: sessions_path()?,
        legacy: session_path()?,
        lock: session_lock_path()?,
    })
}

struct SessionLock(std::fs::File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn acquire_session_lock(path: &std::path::Path) -> Result<SessionLock> {
    let dir = path
        .parent()
        .ok_or_else(|| invalid("bad session lock path"))?;
    std::fs::create_dir_all(dir)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(invalid("session lock must be a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(invalid("session lock must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let deadline = Instant::now() + SESSION_LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(SessionLock(file)),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(SESSION_LOCK_RETRY);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(invalid("saved Hub sessions are busy; try again"));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SavedSession {
    hub: String,
    session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SavedSessions {
    version: u8,
    default_hub: String,
    sessions: Vec<SavedSession>,
}

#[derive(Deserialize)]
struct LegacySession {
    hub: String,
    session: String,
    #[serde(default)]
    expires_at: Option<i64>,
}

fn checked_session(hub: &str, session: &str, expires_at: Option<i64>) -> Result<SavedSession> {
    let hub = check_base_url(hub).map_err(map_hub_error)?;
    if session.is_empty()
        || session.len() > MAX_SESSION_BYTES
        || session.chars().any(char::is_control)
    {
        return Err(invalid("bad session file"));
    }
    Ok(SavedSession {
        hub,
        session: session.to_owned(),
        expires_at,
    })
}

fn parse_sessions(text: &str, legacy: bool) -> Result<SavedSessions> {
    if legacy {
        let saved: LegacySession =
            serde_json::from_str(text).map_err(|_| invalid("bad session file"))?;
        let saved = checked_session(&saved.hub, &saved.session, saved.expires_at)?;
        return Ok(SavedSessions {
            version: 2,
            default_hub: saved.hub.clone(),
            sessions: vec![saved],
        });
    }
    let parsed: SavedSessions =
        serde_json::from_str(text).map_err(|_| invalid("bad session file"))?;
    if parsed.version != 2
        || parsed.sessions.is_empty()
        || parsed.sessions.len() > MAX_SAVED_SESSIONS
    {
        return Err(invalid("bad session file"));
    }
    let default_hub = check_base_url(&parsed.default_hub).map_err(map_hub_error)?;
    let mut sessions = Vec::with_capacity(parsed.sessions.len());
    for saved in parsed.sessions {
        let saved = checked_session(&saved.hub, &saved.session, saved.expires_at)?;
        if sessions
            .iter()
            .any(|existing: &SavedSession| existing.hub == saved.hub)
        {
            return Err(invalid("bad session file"));
        }
        sessions.push(saved);
    }
    sessions.sort_by(|left, right| left.hub.cmp(&right.hub));
    if !sessions.iter().any(|saved| saved.hub == default_hub) {
        return Err(invalid("bad session file"));
    }
    Ok(SavedSessions {
        version: 2,
        default_hub,
        sessions,
    })
}

fn read_session_file(path: &std::path::Path) -> Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(SESSION_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SESSION_FILE_BYTES {
        return Err(invalid("bad session file"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("bad session file"))
}

fn load_sessions_from(files: &SessionFiles) -> Result<Option<SavedSessions>> {
    if let Some(text) = read_session_file(&files.current)? {
        parse_sessions(&text, false).map(Some)
    } else if let Some(text) = read_session_file(&files.legacy)? {
        parse_sessions(&text, true).map(Some)
    } else {
        Ok(None)
    }
}

fn load_sessions() -> Result<SavedSessions> {
    load_sessions_from(&session_files()?)?
        .ok_or_else(|| invalid("not signed in; run agentscm hub login"))
}

fn session_files_present() -> Result<bool> {
    let files = session_files()?;
    Ok(files.current.try_exists()? || files.legacy.try_exists()?)
}

/// Private temp file renamed into place, so the contents are never readable by others.
pub(crate) fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    let dir = path.parent().ok_or_else(|| invalid("bad file path"))?;
    std::fs::create_dir_all(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| invalid("random source unavailable"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    name.push(format!(".tmp-{suffix}"));
    let temp = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        // Ordinary directory handles support syncing on Unix, but not on Windows.
        // The file contents are synced before publication on every platform.
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

fn write_sessions_to(files: &SessionFiles, store: &SavedSessions) -> Result<()> {
    let contents = serde_json::to_string(store).map_err(|_| invalid("bad session file"))?;
    if contents.len() as u64 > SESSION_FILE_BYTES {
        return Err(invalid("too many saved Hub sessions"));
    }
    write_private(&files.current, &contents)?;
    remove_file_if_present(&files.legacy)
}

fn mutate_sessions<T>(
    files: &SessionFiles,
    change: impl FnOnce(&mut Option<SavedSessions>) -> Result<T>,
) -> Result<T> {
    let _lock = acquire_session_lock(&files.lock)?;
    let mut store = load_sessions_from(files)?;
    let result = change(&mut store)?;
    match &store {
        Some(store) => write_sessions_to(files, store)?,
        None => {
            remove_file_if_present(&files.current)?;
            remove_file_if_present(&files.legacy)?;
        }
    }
    Ok(result)
}

fn save_session(hub: &str, secret: &str, expires_at: Option<i64>) -> Result<()> {
    let new = checked_session(hub, secret, expires_at)?;
    save_session_to(&session_files()?, new)
}

fn save_session_to(files: &SessionFiles, new: SavedSession) -> Result<()> {
    mutate_sessions(files, move |store| {
        *store = Some(upsert_saved_session(store.take(), new)?);
        Ok(())
    })
}

fn upsert_saved_session(store: Option<SavedSessions>, new: SavedSession) -> Result<SavedSessions> {
    let default_hub = new.hub.clone();
    let mut store = store.unwrap_or(SavedSessions {
        version: 2,
        default_hub: String::new(),
        sessions: Vec::new(),
    });
    if let Some(saved) = store.sessions.iter_mut().find(|saved| saved.hub == new.hub) {
        *saved = new;
    } else {
        if store.sessions.len() == MAX_SAVED_SESSIONS {
            return Err(invalid("too many saved Hub sessions; remove one first"));
        }
        store.sessions.push(new);
    }
    store.default_hub = default_hub;
    store
        .sessions
        .sort_by(|left, right| left.hub.cmp(&right.hub));
    Ok(store)
}

pub(crate) const SESSION_EXPIRED: &str = "hub session expired; run agentscm hub login";

/// The saved session for `hub` (or the default Hub), refusing one past its recorded expiry.
pub(crate) fn load_session(hub: Option<&str>) -> Result<(String, String)> {
    let store = load_sessions()?;
    let saved = selected_session(&store, hub, unix_now())?;
    Ok((saved.hub.clone(), saved.session.clone()))
}

/// Whether a saved, unexpired session exists for `hub` (or the default Hub).
pub(crate) fn has_session(hub: Option<&str>) -> Result<bool> {
    if !session_files_present()? {
        return Ok(false);
    }
    let store = load_sessions()?;
    if let Some(hub) = hub {
        check_base_url(hub).map_err(map_hub_error)?;
    }
    Ok(selected_session(&store, hub, unix_now()).is_ok())
}

fn selected_session<'a>(
    store: &'a SavedSessions,
    hub: Option<&str>,
    now: i64,
) -> Result<&'a SavedSession> {
    let saved = saved_session(store, hub)?;
    if saved.expires_at.is_some_and(|expires| expires <= now) {
        return Err(invalid(SESSION_EXPIRED));
    }
    Ok(saved)
}

/// The saved entry whether or not it has expired; only sign-out uses it, to clear stale entries.
fn saved_session<'a>(store: &'a SavedSessions, hub: Option<&str>) -> Result<&'a SavedSession> {
    let selected = match hub.map(str::to_owned).or_else(env_hub) {
        Some(hub) => check_base_url(&hub).map_err(map_hub_error)?,
        None => store.default_hub.clone(),
    };
    store
        .sessions
        .iter()
        .find(|saved| saved.hub == selected)
        .ok_or_else(|| invalid("no saved session for this hub; run agentscm hub login"))
}

fn load_saved_session(hub: Option<&str>) -> Result<(String, String)> {
    let store = load_sessions()?;
    let saved = saved_session(&store, hub)?;
    Ok((saved.hub.clone(), saved.session.clone()))
}

fn remove_file_if_present(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_saved_session_if(hub: &str, expected_secret: Option<&str>) -> Result<bool> {
    remove_saved_session_from(&session_files()?, hub, expected_secret)
}

fn remove_saved_session_from(
    files: &SessionFiles,
    hub: &str,
    expected_secret: Option<&str>,
) -> Result<bool> {
    mutate_sessions(files, |store| {
        let Some(current) = store.as_mut() else {
            return Ok(false);
        };
        if expected_secret.is_some_and(|secret| {
            !current
                .sessions
                .iter()
                .any(|saved| saved.hub == hub && saved.session == secret)
        }) {
            return Ok(false);
        }
        if !remove_from_store(current, hub) {
            return Ok(false);
        }
        if current.sessions.is_empty() {
            *store = None;
        }
        Ok(true)
    })
}

/// Remove one Hub's local human sign-in without contacting the Hub.
pub(crate) fn forget_session(hub: &str) -> Result<bool> {
    let hub = check_base_url(hub).map_err(map_hub_error)?;
    remove_saved_session_if(&hub, None)
}

fn remove_from_store(store: &mut SavedSessions, hub: &str) -> bool {
    let before = store.sessions.len();
    store.sessions.retain(|saved| saved.hub != hub);
    if store.sessions.len() == before {
        return false;
    }
    if !store.sessions.is_empty() && store.default_hub == hub {
        // Entries are stored in lexical Hub order, giving logout without `--hub` a stable next
        // default after the current one is removed.
        store.default_hub = store.sessions[0].hub.clone();
    }
    true
}

fn session_lines(store: &SavedSessions, now: i64) -> Vec<String> {
    store
        .sessions
        .iter()
        .map(|saved| {
            let default = if saved.hub == store.default_hub {
                " default"
            } else {
                ""
            };
            let expiry = match saved.expires_at {
                Some(expires) if expires <= now => format!("expired={expires}"),
                Some(expires) => format!("expires={expires}"),
                None => "expires=unknown".to_owned(),
            };
            format!("{}{} {expiry}", saved.hub, default)
        })
        .collect()
}

pub(crate) fn parse_ttl(ttl: &str) -> Result<i64> {
    let bad = || invalid("ttl must be <n>s, <n>m, <n>h or <n>d");
    let (digits, unit) = [("s", 1), ("m", 60), ("h", 3600), ("d", 86400)]
        .into_iter()
        .find_map(|(suffix, seconds)| Some((ttl.strip_suffix(suffix)?, seconds)))
        .ok_or_else(bad)?;
    digits
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(bad)
}

async fn token(command: TokenCommand) -> Result<()> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let (hub_arg, method, path, body) = match &command {
        TokenCommand::Create {
            name,
            repo,
            ttl,
            read_only,
            hub,
        } => {
            let mut body =
                json!({"name": name, "repository": repo, "ttl_seconds": parse_ttl(ttl)?});
            // Sent only when asked, so a hub that predates read-only tokens still accepts the rest.
            if *read_only {
                body["read_only"] = json!(true);
            }
            (hub, Method::POST, "/v1/hub/tokens".to_owned(), Some(body))
        }
        TokenCommand::List { hub } => (hub, Method::GET, "/v1/hub/tokens".to_owned(), None),
        TokenCommand::Revoke { id, hub } => {
            if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(invalid("token id must look like tok_..."));
            }
            (hub, Method::DELETE, format!("/v1/hub/tokens/{id}"), None)
        }
    };
    let (hub, session) = load_session(hub_arg.as_deref())?;
    let (status, reply) = call(
        &client,
        method,
        &format!("{hub}{path}"),
        Some(&session),
        body,
    )
    .await?;
    match (status, &command) {
        (401, _) => Err(invalid(SESSION_EXPIRED)),
        (403, _) if reply["error"] == "not_connected" => Err(invalid(
            "connect the repository first: install the GitHub App on it, then run \
             `agentscm hub connections refresh <id>` (for a PAT or GitLab token connection: \
             `agentscm hub connections add-repo <id> <owner/name>`)",
        )),
        (403, _) => Err(invalid(&refusal_message(&reply).unwrap_or_else(|| {
            "your forge account can't see that repository, or it was renamed or moved; check the name and your access".to_owned()
        }))),
        (404, TokenCommand::Revoke { .. }) => Err(invalid("no such token of yours")),
        (201, TokenCommand::Create { .. }) => {
            eprintln!(
                "created {} for {} (expires at unix {})",
                reply["id"].as_str().unwrap_or("?"),
                reply["repository"].as_str().unwrap_or("?"),
                reply["expires_at"]
            );
            println!("{}", reply["token"].as_str().unwrap_or_default());
            Ok(())
        }
        (200, TokenCommand::List { .. }) => {
            for t in reply["tokens"].as_array().into_iter().flatten() {
                println!(
                    "{} {} {} expires={}{}{}",
                    t["id"].as_str().unwrap_or("?"),
                    t["name"].as_str().unwrap_or("?"),
                    t["repository"].as_str().unwrap_or("?"),
                    t["expires_at"],
                    if t["read_only"] == true {
                        " read-only"
                    } else {
                        ""
                    },
                    if t["revoked"] == true { " revoked" } else { "" }
                );
            }
            Ok(())
        }
        (204, TokenCommand::Revoke { id, .. }) => {
            eprintln!("revoked {id}");
            Ok(())
        }
        _ => Err(Error::Invalid(format!(
            "hub replied {status}: {}",
            reply["error"].as_str().unwrap_or("error")
        ))),
    }
}

/// One request with the saved session; the reply is checked for the common failures.
async fn admin_call(
    hub: &Option<String>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<(u16, Value)> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let (hub, session) = load_session(hub.as_deref())?;
    let (status, reply) = call(
        &client,
        method,
        &format!("{hub}{path}"),
        Some(&session),
        body,
    )
    .await?;
    match status {
        401 => Err(invalid(SESSION_EXPIRED)),
        403 => Err(invalid(
            "refused: administrators only, or the connection cannot read that repository",
        )),
        404 => Err(invalid(
            "no such connection (or GitHub is not configured on this hub)",
        )),
        _ => Ok((status, reply)),
    }
}

fn unexpected(status: u16, reply: &Value) -> Error {
    Error::Invalid(format!(
        "hub replied {status}: {}",
        reply["error"].as_str().unwrap_or("error")
    ))
}

fn connection_id(id: &str) -> Result<()> {
    if id.starts_with("con_") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(())
    } else {
        Err(invalid("connection id must look like con_..."))
    }
}

async fn github(command: GithubCommand) -> Result<()> {
    let GithubCommand::Setup { name, org, hub } = command;
    let mut body = json!({"name": name});
    if let Some(org) = org {
        body["org"] = json!(org);
    }
    let (status, reply) = admin_call(&hub, Method::POST, "/v1/hub/github/apps", Some(body)).await?;
    let (Some(url), Some(connection)) = (reply["setup_url"].as_str(), reply["connection"].as_str())
    else {
        return Err(unexpected(status, &reply));
    };
    eprintln!(
        "open the URL in a browser signed in to GitHub within 10 minutes; connection {} will be created",
        clean(connection)
    );
    println!("{}", clean(url));
    Ok(())
}

async fn connections(command: ConnectionCommand) -> Result<()> {
    match command {
        ConnectionCommand::List { hub } => {
            let (status, reply) =
                admin_call(&hub, Method::GET, "/v1/hub/connections", None).await?;
            if status != 200 {
                return Err(unexpected(status, &reply));
            }
            for c in reply["connections"].as_array().into_iter().flatten() {
                let text = |key: &str| clean(c[key].as_str().unwrap_or("?"));
                println!(
                    "{} {} {} outbound={} repos={} {}{}",
                    text("id"),
                    text("kind"),
                    text("status"),
                    if c["outbound"] == true { "on" } else { "off" },
                    c["repositories"],
                    text("label"),
                    c["status_detail"]
                        .as_str()
                        .map_or(String::new(), |d| format!(" ({})", clean(d)))
                );
            }
        }
        ConnectionCommand::Refresh { id, hub } => {
            connection_id(&id)?;
            let path = format!("/v1/hub/connections/{id}/refresh");
            let (status, reply) = admin_call(&hub, Method::POST, &path, None).await?;
            if status != 200 {
                return Err(unexpected(status, &reply));
            }
            for r in reply["reports"].as_array().into_iter().flatten() {
                println!(
                    "{} pulls={} changed={} branches={} not_modified={} truncated={}",
                    clean(r["repository"].as_str().unwrap_or("?")),
                    r["pull_requests"],
                    r["changed"],
                    r["branches"],
                    r["not_modified"],
                    clean(&r["truncated"].to_string())
                );
            }
        }
        ConnectionCommand::Outbound { id, state, hub } => {
            connection_id(&id)?;
            let enabled = matches!(state, Switch::On);
            let path = format!("/v1/hub/connections/{id}/outbound");
            let body = Some(json!({"enabled": enabled}));
            let (status, reply) = admin_call(&hub, Method::POST, &path, body).await?;
            if status != 204 {
                return Err(unexpected(status, &reply));
            }
            eprintln!("outbound {} for {id}", if enabled { "on" } else { "off" });
        }
        ConnectionCommand::AddRepo { id, repo, hub } => {
            connection_id(&id)?;
            let plain = repo.split('/').count() == 2
                && repo
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
            if !plain {
                return Err(invalid("repository must look like owner/name"));
            }
            let path = format!("/v1/hub/connections/{id}/repositories");
            let (status, reply) =
                admin_call(&hub, Method::POST, &path, Some(json!({"full_name": repo}))).await?;
            if status != 200 {
                return Err(unexpected(status, &reply));
            }
            eprintln!(
                "connected {}",
                clean(reply["full_name"].as_str().unwrap_or("?"))
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hub_comes_from_the_flag_then_agentscm_hub_then_the_only_sign_in() {
        let env = Some("https://env.example/".to_owned());
        let one = ["https://saved.example"];
        let two = ["https://a.example", "https://b.example"];
        assert_eq!(
            pick_default_hub(env.clone(), &two).unwrap(),
            "https://env.example"
        );
        assert_eq!(
            pick_default_hub(None, &one).unwrap(),
            "https://saved.example"
        );
        assert!(
            pick_default_hub(None, &two)
                .unwrap_err()
                .to_string()
                .contains("AGENTSCM_HUB")
        );
        assert!(
            pick_default_hub(None, &[])
                .unwrap_err()
                .to_string()
                .contains("--hub")
        );
        assert!(pick_default_hub(Some("not a url".into()), &one).is_err());
    }

    #[test]
    fn a_refusal_shows_the_hub_sentence_without_control_characters() {
        let refusal = json!({"error": "insufficient_role",
            "message": "alice has Read on public repository acme/w;\u{1b}[2J ask for Triage"});
        assert_eq!(
            refusal_message(&refusal).as_deref(),
            Some("alice has Read on public repository acme/w;[2J ask for Triage")
        );
        assert_eq!(refusal_message(&json!({"error": "forbidden"})), None);
        assert_eq!(refusal_message(&json!({"message": "not a refusal"})), None);
    }

    #[test]
    fn private_write_creates_and_replaces_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/session.json");
        write_private(&path, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        write_private(&path, "replacement").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    fn temporary_session_files(label: &str) -> (PathBuf, SessionFiles) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agentscm-hub-cli-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let files = SessionFiles {
            current: dir.join("hub-sessions.json"),
            legacy: dir.join("hub-session.json"),
            lock: dir.join("hub-sessions.lock"),
        };
        (dir, files)
    }

    #[test]
    fn parse_ttl_accepts_units_and_never_panics() {
        assert_eq!(parse_ttl("90s").unwrap(), 90);
        assert_eq!(parse_ttl("30m").unwrap(), 1800);
        assert_eq!(parse_ttl("8h").unwrap(), 28800);
        assert_eq!(parse_ttl("7d").unwrap(), 604800);
        for bad in [
            "5é",
            "é",
            "",
            "5",
            "s",
            "5x",
            "9999999999999999999d",
            "1.5h",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_forge_host_is_opened() {
        assert!(opens_page("https://github.com/login/device", "github.com"));
        for bad in [
            "http://github.com/login/device",
            "https://github.com.evil.example/x",
            "https://evil.example/github.com",
            "https://user@evil.example/",
            "javascript:alert(1)",
        ] {
            assert!(!opens_page(bad, "github.com"), "{bad}");
        }
    }

    #[test]
    fn clean_strips_control_characters() {
        assert_eq!(clean("a\u{1b}[31mb\n\u{7}c"), "a[31mbc");
    }

    #[test]
    fn legacy_session_migrates_to_a_normalized_v2_entry() {
        let legacy = r#"{
            "hub":"https://team.example/",
            "session":"human-secret",
            "expires_at":1234
        }"#;
        let store = parse_sessions(legacy, true).unwrap();
        assert_eq!(store.version, 2);
        assert_eq!(store.default_hub, "https://team.example");
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.sessions[0].session, "human-secret");

        let migrated = serde_json::to_string(&store).unwrap();
        let reparsed = parse_sessions(&migrated, false).unwrap();
        assert_eq!(reparsed.default_hub, "https://team.example");
        assert_eq!(reparsed.sessions[0].session, "human-secret");
    }

    #[test]
    fn explicit_hub_selection_never_falls_back_to_the_default() {
        let first = checked_session("https://one.example/", "secret-one", Some(200)).unwrap();
        let second = checked_session("https://two.example", "secret-two", Some(300)).unwrap();
        let store =
            upsert_saved_session(Some(upsert_saved_session(None, first).unwrap()), second).unwrap();

        assert_eq!(
            selected_session(&store, None, 100).unwrap().session,
            "secret-two"
        );
        assert_eq!(
            selected_session(&store, Some("https://one.example/"), 100)
                .unwrap()
                .session,
            "secret-one"
        );
        assert!(selected_session(&store, Some("https://missing.example"), 100).is_err());
    }

    #[test]
    fn expired_sessions_are_not_selected_but_stay_removable() {
        let first = checked_session("https://one.example", "secret-one", Some(200)).unwrap();
        let second = checked_session("https://two.example", "secret-two", None).unwrap();
        let store =
            upsert_saved_session(Some(upsert_saved_session(None, second).unwrap()), first).unwrap();

        let Err(Error::Invalid(message)) = selected_session(&store, None, 200) else {
            panic!("an expired default session was selected");
        };
        assert_eq!(message, SESSION_EXPIRED);
        assert!(selected_session(&store, Some("https://one.example"), 199).is_ok());
        assert_eq!(
            selected_session(&store, Some("https://two.example"), i64::MAX)
                .unwrap()
                .session,
            "secret-two"
        );
        assert_eq!(saved_session(&store, None).unwrap().session, "secret-one");
    }

    #[test]
    fn removing_the_default_preserves_other_hubs_and_repairs_the_default() {
        let first = checked_session("https://one.example", "secret-one", None).unwrap();
        let second = checked_session("https://two.example", "secret-two", None).unwrap();
        let mut store =
            upsert_saved_session(Some(upsert_saved_session(None, first).unwrap()), second).unwrap();

        assert!(remove_from_store(&mut store, "https://two.example"));
        assert_eq!(store.default_hub, "https://one.example");
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.sessions[0].session, "secret-one");
        assert!(!remove_from_store(&mut store, "https://missing.example"));
    }

    #[test]
    fn session_store_rejects_ambiguous_or_unbounded_entries() {
        let duplicate = r#"{
            "version":2,
            "default_hub":"https://one.example",
            "sessions":[
                {"hub":"https://one.example","session":"one"},
                {"hub":"https://one.example/","session":"two"}
            ]
        }"#;
        assert!(parse_sessions(duplicate, false).is_err());
        assert!(checked_session("https://one.example", "", None).is_err());
        assert!(
            checked_session(
                "https://one.example",
                &"x".repeat(MAX_SESSION_BYTES + 1),
                None
            )
            .is_err()
        );

        let sessions: Vec<Value> = (0..=MAX_SAVED_SESSIONS)
            .map(|index| {
                json!({
                    "hub": format!("https://hub-{index}.example"),
                    "session": format!("secret-{index}")
                })
            })
            .collect();
        let oversized = json!({
            "version": 2,
            "default_hub": "https://hub-0.example",
            "sessions": sessions
        });
        assert!(parse_sessions(&oversized.to_string(), false).is_err());
    }

    #[test]
    fn session_diagnostics_never_include_secrets() {
        let current =
            checked_session("https://current.example", "private-current", Some(200)).unwrap();
        let expired =
            checked_session("https://expired.example", "private-expired", Some(50)).unwrap();
        let store =
            upsert_saved_session(Some(upsert_saved_session(None, expired).unwrap()), current)
                .unwrap();
        let output = session_lines(&store, 100).join("\n");

        assert!(output.contains("https://current.example default expires=200"));
        assert!(output.contains("https://expired.example expired=50"));
        assert!(!output.contains("private-current"));
        assert!(!output.contains("private-expired"));
    }

    #[test]
    fn locked_mutation_reloads_before_preserving_concurrent_hubs() {
        let (dir, files) = temporary_session_files("concurrent");
        save_session_to(
            &files,
            checked_session("https://one.example", "secret-one", None).unwrap(),
        )
        .unwrap();

        let held = acquire_session_lock(&files.lock).unwrap();
        let worker_files = files.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            save_session_to(
                &worker_files,
                checked_session("https://two.example", "secret-two", None).unwrap(),
            )
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let current = load_sessions_from(&files).unwrap().unwrap();
        let third = checked_session("https://three.example", "secret-three", None).unwrap();
        write_sessions_to(&files, &upsert_saved_session(Some(current), third).unwrap()).unwrap();
        drop(held);
        worker.join().unwrap().unwrap();

        let saved = load_sessions_from(&files).unwrap().unwrap();
        assert_eq!(saved.sessions.len(), 3);
        for hub in [
            "https://one.example",
            "https://two.example",
            "https://three.example",
        ] {
            assert!(saved.sessions.iter().any(|entry| entry.hub == hub));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conditional_removal_preserves_a_concurrent_replacement() {
        let (dir, files) = temporary_session_files("replacement");
        let hub = "https://team.example";
        save_session_to(&files, checked_session(hub, "old-secret", None).unwrap()).unwrap();
        let selected = load_sessions_from(&files).unwrap().unwrap().sessions[0].clone();

        save_session_to(&files, checked_session(hub, "new-secret", None).unwrap()).unwrap();
        assert!(
            !remove_saved_session_from(&files, hub, Some(&selected.session)).unwrap(),
            "old logout must not remove the replacement"
        );
        let saved = load_sessions_from(&files).unwrap().unwrap();
        assert_eq!(saved.sessions[0].session, "new-secret");

        assert!(remove_saved_session_from(&files, hub, None).unwrap());
        assert!(load_sessions_from(&files).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn locked_write_migrates_legacy_without_dropping_its_session() {
        let (dir, files) = temporary_session_files("legacy-lock");
        write_private(
            &files.legacy,
            r#"{"hub":"https://old.example","session":"old-secret","expires_at":10}"#,
        )
        .unwrap();

        save_session_to(
            &files,
            checked_session("https://new.example", "new-secret", Some(20)).unwrap(),
        )
        .unwrap();
        let saved = load_sessions_from(&files).unwrap().unwrap();
        assert_eq!(saved.sessions.len(), 2);
        assert!(!files.legacy.try_exists().unwrap());
        assert!(files.current.try_exists().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_lock_is_private_and_refuses_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (dir, files) = temporary_session_files("lock-file");
        {
            let _lock = acquire_session_lock(&files.lock).unwrap();
            assert_eq!(
                std::fs::metadata(&files.lock).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(&files.lock).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"").unwrap();
        symlink(&target, &files.lock).unwrap();
        assert!(acquire_session_lock(&files.lock).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
