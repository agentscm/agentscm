use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentscm::hub::HubError;
use agentscm::hub::forge_http::{self, check_base_url, send};
use agentscm::hub::http::{self, HubConfig};
use agentscm::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::{Subcommand, ValueEnum};
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_DATABASE_URL: &str = "sqlite:///data/hub.sqlite";
const LOOPBACK_WAIT: Duration = Duration::from_secs(600);
const HUB_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Provider {
    Github,
    Gitlab,
}

#[derive(Debug, Subcommand)]
pub enum HubCommand {
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
    },
    /// Sign in to a hub; the session is saved for the token commands.
    Login {
        #[arg(long)]
        hub: String,
        #[arg(long, value_enum)]
        provider: Provider,
    },
    /// End the saved session on the hub and delete the session file.
    Logout,
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

#[cfg_attr(not(unix), allow(unused_variables))]
pub async fn run(command: HubCommand, data_dir: &std::path::Path) -> Result<()> {
    match command {
        HubCommand::Serve {
            bind,
            public,
            objects,
            public_url,
            secrets,
            poll_seconds,
            activity_days,
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
            })
            .await
            .map_err(map_hub_error)
        }
        HubCommand::Login { hub, provider } => {
            let hub = check_base_url(&hub).map_err(map_hub_error)?;
            let name = provider
                .to_possible_value()
                .map(|v| v.get_name().to_owned());
            login(&hub, &name.unwrap_or_default()).await
        }
        HubCommand::Logout => logout().await,
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
    save_session(&json!({"hub": hub, "session": secret, "expires_at": signed["expires_at"]}))?;
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

/// Ends the saved session when it belongs to `hub`: the hub is told (best effort) and the
/// file is removed. Returns whether a session of that hub was found.
pub(crate) async fn end_session(hub: &str) -> Result<bool> {
    let Ok((saved, session)) = load_session(Some(hub)) else {
        return Ok(false);
    };
    let client = forge_http::client().map_err(map_hub_error)?;
    let url = format!("{saved}/v1/hub/session");
    let _ = call(&client, Method::DELETE, &url, Some(&session), None).await;
    match std::fs::remove_file(session_path()?) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

async fn logout() -> Result<()> {
    if let Ok((hub, session)) = load_session(None) {
        let client = forge_http::client().map_err(map_hub_error)?;
        let url = format!("{hub}/v1/hub/session");
        // The hub may be unreachable; the local file goes either way.
        let _ = call(&client, Method::DELETE, &url, Some(&session), None).await;
    }
    match std::fs::remove_file(session_path()?) {
        Ok(()) => eprintln!("signed out"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => eprintln!("not signed in"),
        Err(error) => return Err(error.into()),
    }
    Ok(())
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

pub(crate) fn session_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("hub-session.json"))
}

/// Private temp file renamed into place, so the contents are never readable by others.
pub(crate) fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    let dir = path.parent().ok_or_else(|| invalid("bad file path"))?;
    std::fs::create_dir_all(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".tmp-{}", std::process::id()));
    let temp = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let _ = std::fs::remove_file(&temp);
    options.open(&temp)?.write_all(contents.as_bytes())?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn save_session(session: &Value) -> Result<()> {
    write_private(&session_path()?, &session.to_string())
}

pub(crate) fn load_session(hub: Option<&str>) -> Result<(String, String)> {
    let text = std::fs::read_to_string(session_path()?)
        .map_err(|_| invalid("not signed in; run agentscm hub login"))?;
    let session: Value = serde_json::from_str(&text)?;
    let saved = check_base_url(session["hub"].as_str().unwrap_or("")).map_err(map_hub_error)?;
    let secret = session["session"]
        .as_str()
        .ok_or_else(|| invalid("bad session file"))?;
    // Never send a session secret to a hub other than the one that issued it.
    if let Some(hub) = hub
        && check_base_url(hub).map_err(map_hub_error)? != saved
    {
        return Err(invalid(
            "the saved session is for a different hub; run agentscm hub login",
        ));
    }
    Ok((saved, secret.to_owned()))
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
        (401, _) => Err(invalid("hub session expired; run agentscm hub login")),
        (403, _) if reply["error"] == "not_connected" => Err(invalid(
            "connect the repository first: install the GitHub App on it, then run \
             `agentscm hub connections refresh <id>` (for a PAT or GitLab token connection: \
             `agentscm hub connections add-repo <id> <owner/name>`)",
        )),
        (403, _) => Err(invalid("you cannot read that repository on the forge")),
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
        401 => Err(invalid("hub session expired; run agentscm hub login")),
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
}
