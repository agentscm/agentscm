//! Manage private checkout-bound Hub connections for repository-neutral client plugins.

use std::path::{Component, Path, PathBuf};

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

use agentscm::hub::forge_http::{self, check_base_url, send};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};

use crate::client_config::{
    Connection, RetiredFile, canonical_root, is_current, load, origin_identity, owned_token_path,
    remove_unreferenced_owned_file, update,
};
use crate::hub_cli::{Provider, call, has_session, load_session, map_hub_error, sign_in};
use crate::intent_cli::valid_repo;

const TOKEN_BYTES: u64 = 16 * 1024;
const REPLY_BYTES: usize = 64 * 1024;

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

#[derive(Debug, Subcommand)]
pub(crate) enum ClientCommand {
    /// Verify and save a checkout-bound Hub connection.
    Connect(ConnectArgs),
    /// Connect this checkout and install hooks for one local coding agent.
    #[cfg(unix)]
    Setup(SetupArgs),
    /// List saved connections without reading or printing token contents.
    List,
    /// Remove the connection for a checkout.
    Remove(RemoveArgs),
    /// Verify the saved connection for a checkout against its Hub.
    Check(RootArgs),
    /// Inspect onboarding proof, or classify a captured native-host error locally.
    Doctor(DoctorArgs),
    /// Serve the checkout-bound Hub MCP transport over standard input/output.
    Mcp(RootArgs),
    /// Control silent completion reporting for one saved checkout connection.
    Reporting {
        #[command(subcommand)]
        command: ReportingCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ReportingCommand {
    /// Enable completion reporting for native hooks and the Hub MCP proxy.
    On(RootArgs),
    /// Disable completion reporting.
    Off(RootArgs),
    /// Print only `on` or `off` for the selected checkout.
    Status(RootArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ConnectArgs {
    #[arg(long)]
    hub: String,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Forge sign-in provider (inferred for github.com and gitlab.com).
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Checkout root, or any existing path inside it (default: current directory).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Replace a different connection already bound to this checkout.
    #[arg(long)]
    replace: bool,
}

#[cfg(unix)]
#[derive(Debug, Args)]
pub(crate) struct SetupArgs {
    /// Coding agent whose native hooks should be installed.
    #[arg(long, value_enum)]
    agent: crate::client_setup::Agent,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Debug, Args)]
pub(crate) struct RootArgs {
    /// Checkout root, or any existing path inside it (default: current directory).
    #[arg(long)]
    root: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct DoctorArgs {
    /// Native coding-agent name.
    #[arg(long)]
    agent: String,
    /// Actual native hook session ID; omit for blockers before a session can start.
    #[arg(long)]
    session: Option<String>,
    /// Read up to 64 KiB of captured host diagnostics from stdin; only a blocker category leaves this machine.
    #[arg(long)]
    diagnostic_stdin: bool,
    #[command(flatten)]
    checkout: RootArgs,
}

#[derive(Debug, Args)]
pub(crate) struct RemoveArgs {
    /// Exact absolute registered root (default: current directory); it may no longer exist.
    #[arg(long)]
    root: Option<PathBuf>,
}

pub(crate) async fn run(command: ClientCommand) -> Result<()> {
    match command {
        ClientCommand::Connect(args) => {
            let root = selected_root(args.root.as_deref())?;
            connect(args, root).await
        }
        #[cfg(unix)]
        ClientCommand::Setup(args) => {
            let root = selected_root(args.connect.root.as_deref())?;
            crate::client_setup::preflight(args.agent, &root)?;
            connect(args.connect, root.clone()).await?;
            crate::client_setup::install(args.agent, &root)?;
            Ok(())
        }
        ClientCommand::List => list(),
        ClientCommand::Remove(args) => remove(args),
        ClientCommand::Check(args) => check(args).await,
        ClientCommand::Doctor(args) => doctor(args).await,
        ClientCommand::Mcp(args) => crate::client_mcp::run(args.root.as_deref()).await,
        ClientCommand::Reporting { command } => reporting(command),
    }
}

fn selected_root(given: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    canonical_root(given.unwrap_or(&cwd))
}

/// Lexically absolute path for local removal. It does not require a live checkout or resolve
/// symlinks, so only the exact stored root can be selected.
fn removal_root(given: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let path = given.map_or_else(
        || cwd.clone(),
        |path| {
            if path.is_absolute() {
                path.to_owned()
            } else {
                cwd.join(path)
            }
        },
    );
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(part.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid("--root is not a valid absolute path"));
                }
            }
        }
    }
    if !normalized.is_absolute() {
        return Err(invalid("--root is not a valid absolute path"));
    }
    Ok(normalized)
}

pub(crate) fn read_token(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| invalid("cannot read token file"))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("cannot inspect token file"))?;
    if !metadata.file_type().is_file() {
        return Err(invalid("token file must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(invalid(
                "token file must not be accessible by group or other users",
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take(TOKEN_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > TOKEN_BYTES {
        return Err(invalid("token file is too large"));
    }
    let token = String::from_utf8(bytes)
        .map_err(|_| invalid("token file is not valid UTF-8"))?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err(invalid("token file is empty"));
    }
    Ok(token)
}

fn write_owned_token(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("managed token path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|_| invalid("managed token path already exists or cannot be created"))?;
    if let Err(error) = file.write_all(format!("{token}\n").as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

struct Verified {
    repository: String,
    provider: String,
    host: String,
    login: String,
    expires_at: i64,
}

fn identity_hostname(host: &str) -> Option<String> {
    reqwest::Url::parse(&format!("https://{host}"))
        .ok()?
        .host_str()
        .map(|host| host.trim_matches(['[', ']']).to_ascii_lowercase())
}

async fn verify_token(connection: &Connection, token: &str) -> Result<Verified> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let request = client
        .request(Method::GET, format!("{}/v1/hub/whoami", connection.hub))
        .bearer_auth(token);
    let (status, reply) = send(request, REPLY_BYTES).await.map_err(map_hub_error)?;
    if status != 200 {
        return Err(invalid(
            "Hub rejected the agent token (expired, revoked, or for another Hub)",
        ));
    }
    let named = reply["token"]["repository"]
        .as_str()
        .ok_or_else(|| invalid("Hub returned an invalid token identity"))?;
    if !valid_repo(named) {
        return Err(invalid("Hub returned an invalid token identity"));
    }
    if !named.eq_ignore_ascii_case(&connection.repo) {
        return Err(invalid("agent token belongs to another repository"));
    }
    if reply["token"]["read_only"].as_bool() != Some(false) {
        return Err(invalid("agent token is read-only"));
    }
    let provider = reply["owner"]["provider"]
        .as_str()
        .ok_or_else(|| invalid("Hub returned an invalid owner identity"))?;
    let host = reply["owner"]["host"]
        .as_str()
        .ok_or_else(|| invalid("Hub returned an invalid owner identity"))?;
    let login = reply["owner"]["login"]
        .as_str()
        .ok_or_else(|| invalid("Hub returned an invalid owner identity"))?;
    let expires_at = reply["token"]["expires_at"]
        .as_i64()
        .ok_or_else(|| invalid("Hub returned an invalid token expiry"))?;
    if expires_at <= unix_now() {
        return Err(invalid("agent token is expired"));
    }
    if identity_hostname(host).as_deref() != Some(connection.origin_host.as_str()) {
        return Err(invalid(
            "agent identity host does not match checkout origin",
        ));
    }
    if (connection.origin_host == "github.com" && provider != "github")
        || (connection.origin_host == "gitlab.com" && provider != "gitlab")
    {
        return Err(invalid(
            "agent identity provider does not match checkout origin",
        ));
    }
    Ok(Verified {
        repository: named.to_owned(),
        provider: crate::hub_cli::clean(provider),
        host: crate::hub_cli::clean(host),
        login: crate::hub_cli::clean(login),
        expires_at,
    })
}

async fn verify(connection: &Connection) -> Result<Verified> {
    let token = read_token(&connection.token_file)?;
    verify_token(connection, &token).await
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Github => "github",
        Provider::Gitlab => "gitlab",
    }
}

fn inferred_provider(host: &str) -> Option<Provider> {
    match host {
        "github.com" => Some(Provider::Github),
        "gitlab.com" => Some(Provider::Gitlab),
        _ => None,
    }
}

fn token_name() -> Result<String> {
    let host: String = crate::intent_cli::hostname()
        .unwrap_or_else(|| "host".into())
        .split('.')
        .next()
        .unwrap_or("host")
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "-_".contains(*character))
        .take(40)
        .collect();
    let mut random = [0_u8; 4];
    getrandom::fill(&mut random).map_err(|_| invalid("random source unavailable"))?;
    let random: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "client-{}-{random}",
        if host.is_empty() { "host" } else { &host }
    ))
}

async fn mint_request(hub: &str, session: &str, repo: &str) -> Result<(u16, Value)> {
    let client = forge_http::client().map_err(map_hub_error)?;
    call(
        &client,
        Method::POST,
        &format!("{hub}/v1/hub/tokens"),
        Some(session),
        Some(json!({
            "name": token_name()?,
            "repository": repo,
            "ttl_seconds": 8 * 60 * 60,
        })),
    )
    .await
}

async fn session_for(hub: &str, provider: Provider, origin_host: &str) -> Result<String> {
    if has_session(Some(hub))? {
        let session = load_session(Some(hub))?.1;
        if hub_identity_matches(hub, &session, provider, origin_host).await? {
            return Ok(session);
        }
    }
    sign_in(hub, provider_name(provider), Some(origin_host)).await?;
    let session = load_session(Some(hub))?.1;
    if !hub_identity_matches(hub, &session, provider, origin_host).await? {
        return Err(invalid(
            "Hub sign-in identity does not match the checkout origin",
        ));
    }
    Ok(session)
}

async fn hub_identity_matches(
    hub: &str,
    session: &str,
    provider: Provider,
    origin_host: &str,
) -> Result<bool> {
    let client = forge_http::client().map_err(map_hub_error)?;
    let (status, reply) = call(
        &client,
        Method::GET,
        &format!("{hub}/v1/hub/session"),
        Some(session),
        None,
    )
    .await?;
    match status {
        200 => Ok(
            reply["user"]["provider"].as_str() == Some(provider_name(provider))
                && reply["user"]["host"]
                    .as_str()
                    .and_then(identity_hostname)
                    .as_deref()
                    == Some(origin_host),
        ),
        401 => Ok(false),
        _ => Err(invalid("Hub could not verify the saved sign-in")),
    }
}

struct Minted {
    id: String,
    token: String,
    session: String,
}

async fn mint_agent_token(connection: &Connection, provider: Provider) -> Result<Minted> {
    let mut session = session_for(&connection.hub, provider, &connection.origin_host).await?;
    let (mut status, mut reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
    if status == 401 {
        sign_in(
            &connection.hub,
            provider_name(provider),
            Some(&connection.origin_host),
        )
        .await?;
        session = session_for(&connection.hub, provider, &connection.origin_host).await?;
        (status, reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
    }
    match (status, reply["error"].as_str()) {
        (403, Some("not_connected")) => Err(invalid(
            "this repository is not connected to the Hub; ask its operator to connect it",
        )),
        (403, _) => Err(invalid(
            "your Hub membership cannot create an agent token for this repository",
        )),
        (401, _) => Err(invalid("Hub sign-in expired or was rejected")),
        (201, _) => {
            let id = reply["id"]
                .as_str()
                .filter(|id| {
                    id.starts_with("tok_")
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
                .ok_or_else(|| invalid("Hub returned an invalid token id"))?;
            let token = reply["token"]
                .as_str()
                .filter(|token| !token.is_empty() && token.len() <= TOKEN_BYTES as usize)
                .ok_or_else(|| invalid("Hub returned an invalid agent token"))?;
            Ok(Minted {
                id: id.to_owned(),
                token: token.to_owned(),
                session,
            })
        }
        _ => Err(invalid("Hub could not create an agent token")),
    }
}

async fn revoke_minted(connection: &Connection, minted: &Minted) {
    let Ok(client) = forge_http::client() else {
        return;
    };
    let _ = call(
        &client,
        Method::DELETE,
        &format!("{}/v1/hub/tokens/{}", connection.hub, minted.id),
        Some(&minted.session),
        None,
    )
    .await;
}

fn warn_unrevoked(connection: &Connection, id: &str, expiry: i64) {
    eprintln!(
        "warning: the replaced managed token {id} on {} could not be revoked; it remains valid until unix {expiry}. Revoke that token manually; no retry was queued.",
        connection.hub
    );
}

async fn retire_owned(connection: &Connection) {
    let (Some(id), Some(expiry)) = (
        connection.owned_token_id.as_deref(),
        connection.owned_token_expires_at,
    ) else {
        return;
    };
    match remove_unreferenced_owned_file(connection) {
        Ok(RetiredFile::Referenced) => {
            eprintln!(
                "warning: the replaced managed token {id} was preserved because another client connection references its token file; it remains valid until unix {expiry}"
            );
            return;
        }
        Ok(RetiredFile::Removed) => {}
        Err(error) => {
            eprintln!(
                "warning: the replaced managed token {id} and its file were preserved because AgentSCM could not verify that the file is unreferenced ({error}); the token remains valid until unix {expiry}. No retry was queued."
            );
            return;
        }
    }
    let revoked = if let Ok((hub, session)) = load_session(Some(&connection.hub)) {
        if hub == connection.hub {
            if let Ok(client) = forge_http::client() {
                call(
                    &client,
                    Method::DELETE,
                    &format!("{}/v1/hub/tokens/{id}", connection.hub),
                    Some(&session),
                    None,
                )
                .await
                .is_ok_and(|(status, _)| matches!(status, 204 | 404))
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if !revoked {
        warn_unrevoked(connection, id, expiry);
    }
}

async fn connect(args: ConnectArgs, root: PathBuf) -> Result<()> {
    let (origin_host, inferred_repo) = origin_identity(&root)?;
    let repo = match args.repo {
        Some(repo) if !valid_repo(&repo) => {
            return Err(invalid("--repo must look like owner/name"));
        }
        Some(repo) if !repo.eq_ignore_ascii_case(&inferred_repo) => {
            return Err(invalid("--repo does not match the checkout origin"));
        }
        Some(repo) => repo,
        None => inferred_repo,
    };
    let hub = check_base_url(&args.hub).map_err(map_hub_error)?;
    let supplied_token = match args.token_file {
        Some(path) => {
            let path = path
                .canonicalize()
                .map_err(|_| invalid("token file does not exist"))?;
            if !path.is_file() {
                return Err(invalid("token file is not a regular file"));
            }
            Some(path)
        }
        None => None,
    };
    let connections = load()?;
    let existing = connections.iter().find(|saved| saved.root == root).cloned();
    let changed = existing.as_ref().is_some_and(|saved| {
        saved.hub != hub
            || !saved.repo.eq_ignore_ascii_case(&repo)
            || !saved.origin_host.eq_ignore_ascii_case(&origin_host)
            || supplied_token
                .as_ref()
                .is_some_and(|path| path != &saved.token_file)
    });
    if changed && !args.replace {
        return Err(invalid(
            "a different client connection already exists for this checkout; review `agentscm client list`, then pass --replace",
        ));
    }
    if supplied_token.is_none()
        && !changed
        && let Some(saved) = existing.as_ref().filter(|saved| is_current(saved))
        && let Ok(verified) = verify(saved).await
    {
        if saved.repo == verified.repository {
            println!(
                "Connection is already valid for {} at {} as {}:{}@{}; token expires at {} and permits coordination.",
                saved.repo,
                saved.hub,
                verified.provider,
                verified.login,
                verified.host,
                verified.expires_at
            );
            return Ok(());
        }
        // A supplied token file is independent of the managed-token path hash, so its saved
        // repository spelling can be repaired without moving or reclassifying the token file.
        if saved.owned_token_id.is_none() {
            let mut corrected = saved.clone();
            corrected.repo = verified.repository.clone();
            update(&root, Some(saved), Some(corrected.clone()))?;
            println!(
                "Connection is already valid for {} at {} as {}:{}@{}; token expires at {} and permits coordination.",
                corrected.repo,
                corrected.hub,
                verified.provider,
                verified.login,
                verified.host,
                verified.expires_at
            );
            return Ok(());
        }
    }
    let mut connection = Connection {
        root: root.clone(),
        origin_host,
        hub,
        repo,
        token_file: supplied_token.clone().unwrap_or_default(),
        report_completion: existing
            .as_ref()
            .is_some_and(|saved| !changed && saved.report_completion),
        owned_token_id: None,
        owned_token_expires_at: None,
    };
    let mut owned_minted = None;
    let verified = if supplied_token.is_some() {
        let verified = verify(&connection).await?;
        connection.repo = verified.repository.clone();
        verified
    } else {
        let inferred = inferred_provider(&connection.origin_host);
        if let (Some(given), Some(expected)) = (args.provider, inferred)
            && provider_name(given) != provider_name(expected)
        {
            return Err(invalid(
                "--provider does not match the checkout origin host",
            ));
        }
        let provider = args.provider.or(inferred).ok_or_else(|| {
            invalid("cannot infer the forge sign-in provider for this origin; pass --provider github or --provider gitlab")
        })?;
        let minted = mint_agent_token(&connection, provider).await?;
        let verified = match verify_token(&connection, &minted.token).await {
            Ok(verified) => verified,
            Err(error) => {
                revoke_minted(&connection, &minted).await;
                return Err(error);
            }
        };
        connection.repo = verified.repository.clone();
        connection.token_file = match owned_token_path(&connection, &minted.id) {
            Ok(path) => path,
            Err(error) => {
                revoke_minted(&connection, &minted).await;
                return Err(error);
            }
        };
        connection.owned_token_id = Some(minted.id.clone());
        connection.owned_token_expires_at = Some(verified.expires_at);
        if let Err(error) = write_owned_token(&connection.token_file, &minted.token) {
            revoke_minted(&connection, &minted).await;
            return Err(error);
        }
        owned_minted = Some(minted);
        verified
    };
    if supplied_token.is_some()
        && let Some(saved) = existing.as_ref()
        && saved.token_file == connection.token_file
    {
        if saved.owned_token_id.is_some() && saved.repo != connection.repo {
            return Err(invalid(
                "managed token repository spelling changed; reconnect without --token-file",
            ));
        }
        connection.owned_token_id = saved.owned_token_id.clone();
        connection.owned_token_expires_at = saved.owned_token_expires_at;
    }
    if let Err(error) = update(&root, existing.as_ref(), Some(connection.clone())) {
        if owned_minted.is_some() {
            let _ = std::fs::remove_file(&connection.token_file);
        }
        if let Some(minted) = owned_minted.as_ref() {
            revoke_minted(&connection, minted).await;
        }
        return Err(error);
    }
    if let Some(saved) = existing.as_ref()
        && saved.token_file != connection.token_file
        && saved.owned_token_id.is_some()
    {
        retire_owned(saved).await;
    }
    println!(
        "Connected {} to {} for {} as {}:{}@{}; token expires at {} and permits coordination.",
        connection.root.display(),
        connection.hub,
        connection.repo,
        verified.provider,
        verified.login,
        verified.host,
        verified.expires_at
    );
    Ok(())
}

fn reporting(command: ReportingCommand) -> Result<()> {
    let (args, enabled) = match command {
        ReportingCommand::On(args) => (args, Some(true)),
        ReportingCommand::Off(args) => (args, Some(false)),
        ReportingCommand::Status(args) => (args, None),
    };
    let root = selected_root(args.root.as_deref())?;
    let connection = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("no client connection for this checkout"))?;
    if !is_current(&connection) {
        return Err(invalid(
            "the saved client connection no longer matches this checkout",
        ));
    }
    read_token(&connection.token_file)?;
    let Some(enabled) = enabled else {
        println!(
            "{}",
            if connection.report_completion {
                "on"
            } else {
                "off"
            }
        );
        return Ok(());
    };
    if connection.report_completion == enabled {
        println!("{}", if enabled { "on" } else { "off" });
        return Ok(());
    }
    let mut replacement = connection.clone();
    replacement.report_completion = enabled;
    update(&root, Some(&connection), Some(replacement))?;
    println!(
        "Completion reporting is {} for {}. A running AgentSCM MCP proxy will stop on its next request because the connection changed; restart it.",
        if enabled { "on" } else { "off" },
        root.display()
    );
    Ok(())
}

fn list() -> Result<()> {
    let connections = load()?;
    if connections.is_empty() {
        println!("No client connections configured.");
    }
    for connection in connections {
        println!(
            "{}  {}  origin {}  {}  token file {}  {}  {}",
            connection.root.display(),
            connection.repo,
            connection.origin_host,
            connection.hub,
            connection.token_file.display(),
            if is_current(&connection) {
                "checkout binding current"
            } else {
                "checkout binding stale"
            },
            match (
                connection.owned_token_id.as_deref(),
                connection.owned_token_expires_at,
            ) {
                (Some(id), Some(expiry)) => {
                    format!("managed token {id} expires={expiry}")
                }
                _ => "supplied token; lifecycle managed externally".to_owned(),
            }
        );
    }
    Ok(())
}

fn remove(args: RemoveArgs) -> Result<()> {
    let root = removal_root(args.root.as_deref())?;
    let connection = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("no client connection for this exact registered root"))?;
    update(&root, Some(&connection), None)?;
    println!(
        "Removed the local client connection for {}. The token file and remote token were left unchanged.",
        root.display()
    );
    if let (Some(id), Some(expiry)) = (
        connection.owned_token_id.as_deref(),
        connection.owned_token_expires_at,
    ) {
        println!(
            "Managed token {id} on {} remains active until unix {expiry}; revoke it manually if it is no longer needed.",
            connection.hub
        );
    }
    Ok(())
}

async fn check(args: RootArgs) -> Result<()> {
    let root = selected_root(args.root.as_deref())?;
    let connection = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("no client connection for this checkout"))?;
    if !is_current(&connection) {
        return Err(invalid(
            "checkout root or origin no longer matches the saved connection",
        ));
    }
    let verified = verify(&connection).await?;
    println!(
        "Connection is valid for {} at {} as {}:{}@{}; token expires at {} and permits coordination.",
        connection.repo,
        connection.hub,
        verified.provider,
        verified.login,
        verified.host,
        verified.expires_at
    );
    println!(
        "This checks only the Hub binding. Use client doctor and agentscm_setup_status in your native MCP client to verify hooks and MCP."
    );
    Ok(())
}

async fn doctor(args: DoctorArgs) -> Result<()> {
    use agentscm::hub::onboarding::{Agent, Blocker, CLIENT_HEADER};
    let agent = Agent::from_name(&args.agent)
        .ok_or_else(|| invalid("agent must be claude, codex, cursor, copilot or gemini"))?;
    let session = args
        .session
        .as_deref()
        .unwrap_or(agentscm::hub::onboarding::PENDING_SESSION);
    if session.is_empty() || session.len() > 128 || session.chars().any(char::is_control) {
        return Err(invalid(
            "session must be the native session ID, 1-128 bytes without controls",
        ));
    }
    let root = selected_root(args.checkout.root.as_deref())?;
    let connection = load()?.into_iter().find(|saved| saved.root == root)
        .ok_or_else(|| invalid("No Hub binding. Run client setup for this agent and Hub; complete forge sign-in when prompted."))?;
    if !is_current(&connection) {
        return Err(invalid(
            "checkout root or origin changed; review client setup before reconnecting",
        ));
    }
    verify(&connection).await?;
    let token = read_token(&connection.token_file)?;
    let client_id = crate::client_config::onboarding_client(&root, &token);
    let client = forge_http::client().map_err(map_hub_error)?;
    let endpoint = format!("{}/v1/hub/onboarding", connection.hub);
    let request = if args.diagnostic_stdin {
        use std::io::Read;
        let mut input = Vec::new();
        std::io::stdin().take(65537).read_to_end(&mut input)?;
        if input.len() > 65536 {
            return Err(invalid(
                "host diagnostic exceeds 64 KiB; nothing was reported",
            ));
        }
        let blocker = Blocker::detect(&String::from_utf8_lossy(&input));
        client
            .post(&endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(json!({"agent":agent,"session":session,"blocker":blocker}).to_string())
    } else {
        let request = client
            .get(&endpoint)
            .query(&[("agent", args.agent.as_str())]);
        if let Some(session) = args.session.as_deref() {
            request.query(&[("session", session)])
        } else {
            request
        }
    };
    let (status, reply) = send(
        request.bearer_auth(&token).header(CLIENT_HEADER, client_id),
        REPLY_BYTES,
    )
    .await
    .map_err(map_hub_error)?;
    if status != 200 {
        return Err(invalid(
            "Hub onboarding check failed; review the Hub binding and server version",
        ));
    }
    println!("{}", serde_json::to_string_pretty(&reply)?);
    if reply.as_array().is_some_and(Vec::is_empty) {
        println!(
            "Unverified: call agentscm_setup_status through native MCP using this agent and its actual native session ID, then use a native edit tool on a disposable file and check again. Missing proof does not distinguish pending trust from missing login or quota. Inspect the host's own status; pipe a captured error to client doctor --diagnostic-stdin to classify it locally."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    fn fake_hub(responses: Vec<(u16, Value)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let expected = loop {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break end + 4 + length;
                    }
                };
                while request.len() < expected {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    request.extend_from_slice(&chunk[..count]);
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();
                let body = response.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), receiver)
    }

    #[tokio::test]
    async fn mint_and_identity_checks_keep_secrets_in_authorization() {
        let expiry = unix_now() + 3600;
        let (hub, requests) = fake_hub(vec![
            (
                201,
                json!({"id":"tok_synthetic", "token":"agent-synthetic"}),
            ),
            (
                200,
                json!({
                    "token":{"repository":"Acme/Widgets", "read_only":false, "expires_at":expiry},
                    "owner":{"provider":"gitlab", "host":"127.0.0.1", "login":"member"}
                }),
            ),
        ]);
        let (status, minted) = mint_request(&hub, "session-synthetic", "acme/widgets")
            .await
            .unwrap();
        assert_eq!(status, 201);
        assert_eq!(minted["token"], "agent-synthetic");
        let connection = Connection {
            root: std::env::temp_dir(),
            origin_host: "127.0.0.1".into(),
            hub,
            repo: "acme/widgets".into(),
            token_file: std::env::temp_dir().join("unused-token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
        };
        let verified = verify_token(&connection, "agent-synthetic").await.unwrap();
        assert_eq!(verified.login, "member");
        assert_eq!(verified.repository, "Acme/Widgets");

        let mint = requests.recv().unwrap();
        let (headers, body) = mint.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("authorization: Bearer session-synthetic"));
        assert!(!body.contains("session-synthetic"));
        assert!(body.contains("\"repository\":\"acme/widgets\""));
        assert!(body.contains("\"ttl_seconds\":28800"));
        let whoami = requests.recv().unwrap();
        assert!(whoami.starts_with("GET /v1/hub/whoami HTTP/1.1"));
        assert!(whoami.contains("authorization: Bearer agent-synthetic"));
    }

    #[test]
    fn provider_inference_is_bounded_to_public_defaults() {
        assert!(matches!(
            inferred_provider("github.com"),
            Some(Provider::Github)
        ));
        assert!(matches!(
            inferred_provider("gitlab.com"),
            Some(Provider::Gitlab)
        ));
        assert!(inferred_provider("forge.example.test").is_none());
    }

    #[tokio::test]
    async fn saved_session_identity_must_match_provider_and_origin_host() {
        let (hub, requests) = fake_hub(vec![
            (
                200,
                json!({"user":{"provider":"gitlab", "host":"127.0.0.1"}}),
            ),
            (
                200,
                json!({"user":{"provider":"github", "host":"127.0.0.1"}}),
            ),
        ]);
        assert!(
            hub_identity_matches(&hub, "session-synthetic", Provider::Gitlab, "127.0.0.1")
                .await
                .unwrap()
        );
        assert!(
            !hub_identity_matches(&hub, "session-synthetic", Provider::Gitlab, "127.0.0.1")
                .await
                .unwrap()
        );
        for _ in 0..2 {
            let request = requests.recv().unwrap();
            assert!(request.starts_with("GET /v1/hub/session HTTP/1.1"));
            assert!(request.contains("authorization: Bearer session-synthetic"));
        }
    }

    #[test]
    fn removal_uses_the_exact_lexical_root_even_after_checkout_deletion() {
        let parent = tempfile::tempdir().unwrap();
        let deleted = parent.path().join("deleted-checkout");
        assert!(!deleted.exists());
        assert_eq!(removal_root(Some(&deleted)).unwrap(), deleted);
        assert_ne!(
            removal_root(Some(&parent.path().join("deleted-checkout/subdir"))).unwrap(),
            deleted
        );
    }
}
