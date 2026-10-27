//! Manage private checkout-bound Hub connections for repository-neutral client plugins.

use std::path::{Component, Path, PathBuf};

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

use agentscm::hub::forge_http::{self, check_base_url, send};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};

#[cfg(target_os = "macos")]
use crate::client_config::onboarding_client;
use crate::client_config::{
    Connection, RetiredFile, canonical_root, is_current, load, origin_identity, owned_token_path,
    remove_unreferenced_owned_file, update,
};
use crate::hub_cli::{Provider, call, has_session, load_session, map_hub_error, sign_in};
use crate::intent_cli::valid_repo;
use crate::prompt::Ui;

const TOKEN_BYTES: u64 = 16 * 1024;
/// Hub maximum. Nothing renews agent tokens, so a shorter life stops every agent each day
/// until someone signs in again; the token stays per repository and revocable.
const AGENT_TOKEN_TTL: i64 = agentscm::hub::store::MAX_AGENT_TOKEN_TTL_SECONDS;
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
    /// Show a sanitized, machine-readable status snapshot for native integrations.
    Status(StatusArgs),
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

#[derive(Debug, Default, Args)]
pub(crate) struct ConnectArgs {
    /// Hub URL, such as https://hub.example.com (default: the saved Hub, when there is one).
    #[arg(long)]
    pub(crate) hub: Option<String>,
    /// Forge repository as owner/name (default: read from the checkout's origin remote).
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// File holding an agent token to use instead of creating one with your Hub sign-in.
    #[arg(long)]
    pub(crate) token_file: Option<PathBuf>,
    /// Forge sign-in provider (inferred for github.com and gitlab.com).
    #[arg(long, value_enum)]
    pub(crate) provider: Option<Provider>,
    /// Checkout root, or any existing path inside it (default: current directory).
    #[arg(long)]
    pub(crate) root: Option<PathBuf>,
    /// Replace a different connection already bound to this checkout.
    #[arg(long)]
    pub(crate) replace: bool,
    /// Refuse instead of starting a browser or device sign-in (quickstart with `--yes` or no terminal).
    #[arg(skip)]
    pub(crate) no_sign_in: bool,
}

#[cfg(unix)]
#[derive(Debug, Args)]
pub(crate) struct SetupArgs {
    /// Coding agent whose native hooks should be installed.
    #[arg(long, value_enum)]
    agent: crate::client_setup::Agent,
    /// Write the agent's hooks, MCP entry and skill into this checkout only, not your user
    /// configuration, so they run only here. Claude Code is always set up through its
    /// user-scope plugin, and Codex hooks always come from one.
    #[arg(long)]
    checkout_only: bool,
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
pub(crate) struct StatusArgs {
    /// Exact configured checkout selected from any existing path inside it.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Perform bounded authenticated Hub checks; omitted means local state only.
    #[arg(long)]
    check: bool,
    /// Print the stable versioned JSON contract.
    #[arg(long)]
    json: bool,
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
            let ui = Ui { yes: false };
            connect(args, root, terminal_wait(&ui).as_ref()).await
        }
        #[cfg(unix)]
        ClientCommand::Setup(args) => {
            let root = selected_root(args.connect.root.as_deref())?;
            let ui = Ui { yes: false };
            let program = std::env::current_exe()?;
            let scope = if args.checkout_only {
                crate::client_setup::Scope::Checkout
            } else {
                crate::client_setup::Scope::User
            };
            setup(
                &[(args.agent, scope)],
                args.connect,
                root,
                &program,
                terminal_wait(&ui).as_ref(),
            )
            .await
        }
        ClientCommand::List => list(),
        ClientCommand::Remove(args) => remove(args).await,
        ClientCommand::Check(args) => check(args).await,
        ClientCommand::Status(args) => status(args).await,
        ClientCommand::Doctor(args) => doctor(args).await,
        ClientCommand::Mcp(args) => crate::client_mcp::run(args.root.as_deref()).await,
        ClientCommand::Reporting { command } => reporting(command),
    }
}

async fn status(args: StatusArgs) -> Result<()> {
    let root = args
        .root
        .as_deref()
        .map(|root| selected_root(Some(root)))
        .transpose()?;
    let snapshot = crate::client_status::snapshot_for(args.check, root.as_deref()).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    if snapshot.connections.is_empty() {
        println!("No client connections configured.");
    }
    for connection in snapshot.connections {
        let now = unix_now();
        println!(
            "{}  {}  {}",
            connection.root.display(),
            connection.repository,
            connection.hub,
        );
        println!(
            "  binding: {}; token: {}; Hub: {}",
            label(&connection.binding.state),
            match connection.credential.expires_at {
                Some(at) => format!(
                    "{}, {} {}",
                    label(&connection.credential.state),
                    if at < now { "expired" } else { "expires" },
                    crate::hub_cli::when(at, now)
                ),
                None => label(&connection.credential.state),
            },
            hub_check_text(&connection.hub_check, now),
        );
        if !connection.agents.is_empty() {
            println!("  configured agents: {}", connection.agents.join(", "));
        }
        for activity in connection.recent_participation {
            println!(
                "  {}: {} (MCP={}, native={}); {}",
                activity.agent,
                activity.state,
                activity.mcp_verified,
                activity.native_verified,
                activity.next_action
            );
        }
    }
    Ok(())
}

/// A status enum's stable snake_case name, as the JSON form spells it.
fn label(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(|text| text.replace('_', " ")))
        .unwrap_or_default()
}

fn hub_check_text(check: &crate::client_status::HubCheck, now: i64) -> String {
    use crate::client_status::HubCheck;
    match check {
        HubCheck::Skipped => "not checked (pass --check)".to_owned(),
        HubCheck::Valid {
            login, expires_at, ..
        } => format!(
            "signed in as {}, sign-in expires {}",
            crate::hub_cli::clean(login),
            crate::hub_cli::when(*expires_at, now)
        ),
        HubCheck::Rejected { .. } => "rejected the saved sign-in or token".to_owned(),
        HubCheck::Unavailable { .. } => "did not answer".to_owned(),
        HubCheck::InvalidResponse { .. } => "gave an answer AgentSCM could not read".to_owned(),
    }
}

/// Waiting for the GitHub App install needs someone at a terminal to press Enter.
fn terminal_wait(ui: &Ui) -> Option<AppWait<'_>> {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal().then_some(AppWait {
        ui,
        install_url: None,
    })
}

/// The one setup path, for `client setup` and quickstart: check every agent's files first so
/// nothing is minted for a setup that would be refused, connect the checkout, then install.
/// `program` is the stable executable the hooks and MCP entries run.
#[cfg(unix)]
pub(crate) async fn setup(
    agents: &[(crate::client_setup::Agent, crate::client_setup::Scope)],
    args: ConnectArgs,
    root: PathBuf,
    program: &Path,
    wait: Option<&AppWait<'_>>,
) -> Result<()> {
    connect_inner(
        args,
        root,
        wait,
        Some(SetupInstallation { agents, program }),
    )
    .await
}

#[cfg(unix)]
struct SetupInstallation<'a> {
    agents: &'a [(crate::client_setup::Agent, crate::client_setup::Scope)],
    program: &'a Path,
}

#[cfg(not(unix))]
type SetupInstallation<'a> = std::marker::PhantomData<&'a ()>;

#[cfg(unix)]
fn validate_setup_installation(setup: &SetupInstallation<'_>, root: &Path) -> Result<()> {
    validate_setup_program(setup.program)?;
    for (agent, scope) in setup.agents {
        crate::client_setup::preflight(*agent, root, setup.program, *scope)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_setup_installation(setup: &SetupInstallation<'_>, _root: &Path) -> Result<()> {
    let _ = setup;
    Ok(())
}

#[cfg(unix)]
fn install_setup(setup: &SetupInstallation<'_>, root: &Path) -> Result<()> {
    // Recorded first, so `client remove` also takes out a plugin a failed install left behind.
    for (agent, scope) in setup.agents {
        record_agent(root, *agent)?;
        crate::client_setup::install(*agent, root, setup.program, *scope)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_setup(setup: &SetupInstallation<'_>, _root: &Path) -> Result<()> {
    let _ = setup;
    Ok(())
}

/// Remembers which agents were set up in a checkout, so `client remove` can take them out.
#[cfg(unix)]
fn record_agent(root: &Path, agent: crate::client_setup::Agent) -> Result<()> {
    let saved = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("the client connection disappeared during setup; retry"))?;
    let name = agent.name().to_owned();
    if saved.agents.contains(&name) {
        return Ok(());
    }
    let mut replacement = saved.clone();
    replacement.agents.push(name);
    update(root, Some(&saved), Some(replacement))
}

/// Install one agent into an already bound checkout without minting credentials or contacting Hub.
#[cfg(target_os = "macos")]
pub(crate) fn setup_existing_agent(
    agent: crate::client_setup::Agent,
    root: &Path,
    expected_hub: &str,
    expected_repo: &str,
    expected_generation: &str,
    running_program: &Path,
) -> Result<()> {
    let _lifecycle = lifecycle_lock()?;
    let saved = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("this checkout is no longer connected"))?;
    let token = read_token(&saved.token_file)?;
    integration_replacement(
        &saved,
        &token,
        expected_hub,
        expected_repo,
        expected_generation,
        agent,
    )?;
    let stable = installed_program(running_program)?;
    // The tray asks nothing, so it keeps an agent's existing scope.
    let scope = tray_scope(agent, root);
    crate::client_setup::preflight(agent, root, &stable, scope)?;
    let latest = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("this checkout is no longer connected"))?;
    let latest_token = read_token(&latest.token_file)?;
    let replacement = integration_replacement(
        &latest,
        &latest_token,
        expected_hub,
        expected_repo,
        expected_generation,
        agent,
    )?;
    // Record first so `client remove` can clean up a partially completed install.
    // The compare-and-swap also fences reapplication if the connection changed during preflight.
    update(root, Some(&latest), Some(replacement))?;
    crate::client_setup::install_quiet(agent, root, &stable, scope)
}

/// The scope the tray sets an agent up in: for all projects only once that was chosen.
#[cfg(target_os = "macos")]
fn tray_scope(agent: crate::client_setup::Agent, root: &Path) -> crate::client_setup::Scope {
    if crate::client_setup::user_scope_installed(agent, root) {
        crate::client_setup::Scope::User
    } else {
        crate::client_setup::Scope::Checkout
    }
}

/// `client setup` for a checkout the tray saw an agent use without a connection. The tray has
/// no terminal, so this never starts a sign-in; it uses the saved session for `hub`.
#[cfg(target_os = "macos")]
pub(crate) async fn setup_unconnected(
    agent: crate::client_setup::Agent,
    root: &Path,
    hub: &str,
    running_program: &Path,
) -> Result<()> {
    let root = canonical_root(root)?;
    let stable = installed_program(running_program)?;
    let args = ConnectArgs {
        hub: Some(hub.to_owned()),
        no_sign_in: true,
        ..ConnectArgs::default()
    };
    setup(
        &[(agent, tray_scope(agent, &root))],
        args,
        root,
        &stable,
        None,
    )
    .await
}

/// The stable `~/.agentscm/bin/agentscm` link, which hooks must run, when it is this program.
#[cfg(target_os = "macos")]
fn installed_program(running_program: &Path) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?;
    let stable = PathBuf::from(home).join(".agentscm/bin/agentscm");
    let installed = std::fs::canonicalize(&stable).map_err(|_| {
        invalid("restart AgentSCM from the installed ~/.agentscm/bin/agentscm link")
    })?;
    if installed != std::fs::canonicalize(running_program)? {
        return Err(invalid(
            "restart AgentSCM from the installed ~/.agentscm/bin/agentscm link",
        ));
    }
    Ok(stable)
}

pub(crate) fn lifecycle_lock() -> Result<std::fs::File> {
    lifecycle_lock_at(&crate::hub_cli::config_dir()?.join("agentscm/client-lifecycle.lock"))
}

fn lifecycle_lock_at(path: &Path) -> Result<std::fs::File> {
    crate::client_config::acquire_lock(path)
}

#[cfg(unix)]
fn validate_setup_program(program: &Path) -> Result<()> {
    if !program.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(invalid(
            "the installed AgentSCM executable disappeared; reinstall it before client setup",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn integration_replacement(
    saved: &Connection,
    token: &str,
    expected_hub: &str,
    expected_repo: &str,
    expected_generation: &str,
    agent: crate::client_setup::Agent,
) -> Result<Connection> {
    integration_replacement_if(
        saved,
        token,
        expected_hub,
        expected_repo,
        expected_generation,
        agent,
        is_current(saved),
    )
}

#[cfg(target_os = "macos")]
fn integration_replacement_if(
    saved: &Connection,
    token: &str,
    expected_hub: &str,
    expected_repo: &str,
    expected_generation: &str,
    agent: crate::client_setup::Agent,
    current: bool,
) -> Result<Connection> {
    if !current
        || saved.hub != expected_hub
        || saved.repo != expected_repo
        || onboarding_client(&saved.root, token) != expected_generation
    {
        return Err(invalid(
            "the checkout connection changed; refresh and try again",
        ));
    }
    let mut replacement = saved.clone();
    if !replacement.agents.iter().any(|name| name == agent.name()) {
        replacement.agents.push(agent.name().to_owned());
    }
    Ok(replacement)
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
            "ttl_seconds": AGENT_TOKEN_TTL,
        })),
    )
    .await
}

fn cannot_sign_in() -> Error {
    invalid(
        "the saved hub sign-in has expired or isn't accepted, and this run can't sign in again (no terminal, or --yes). Run quickstart at a terminal, or pass --agent-token-file",
    )
}

async fn session_for(
    hub: &str,
    provider: Provider,
    origin_host: &str,
    no_sign_in: bool,
) -> Result<String> {
    if has_session(Some(hub))? {
        let session = load_session(Some(hub))?.1;
        if hub_identity_matches(hub, &session, provider, origin_host).await? {
            return Ok(session);
        }
    }
    if no_sign_in {
        return Err(cannot_sign_in());
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

/// How `connect` reports, and at a terminal waits out, a repository the hub's GitHub App is not
/// installed on.
pub(crate) struct AppWait<'a> {
    pub(crate) ui: &'a Ui,
    /// The app's install page, when the caller knows it.
    pub(crate) install_url: Option<String>,
}

fn install_help(install_url: Option<&str>, repo: &str) -> String {
    let repo = crate::hub_cli::clean(repo);
    match install_url {
        Some(link) => format!(
            "The hub's GitHub App isn't installed on {repo}. Open {link}, choose {repo}, and install."
        ),
        None => format!(
            "The hub's GitHub App isn't installed on {repo}. Open GitHub, then Settings, Applications, the hub's app, Configure, and add {repo}."
        ),
    }
}

fn not_connected(status: u16, reply: &Value) -> bool {
    status == 403 && reply["error"] == "not_connected"
}

/// Best effort, as the signed-in user: ask every connection to look at its repositories again.
async fn refresh_connections(hub: &str, session: &str) {
    let Ok(client) = forge_http::client() else {
        return;
    };
    let list = format!("{hub}/v1/hub/connections");
    let Ok((200, reply)) = call(&client, Method::GET, &list, Some(session), None).await else {
        return;
    };
    for id in reply["connections"].as_array().into_iter().flatten() {
        if let Some(id) = id["id"].as_str().filter(|id| id.starts_with("con_")) {
            let target = format!("{hub}/v1/hub/connections/{id}/refresh");
            let _ = call(&client, Method::POST, &target, Some(session), None).await;
        }
    }
}

async fn mint_agent_token(
    connection: &Connection,
    provider: Provider,
    wait: Option<&AppWait<'_>>,
    no_sign_in: bool,
) -> Result<Minted> {
    let mut session = session_for(
        &connection.hub,
        provider,
        &connection.origin_host,
        no_sign_in,
    )
    .await?;
    let (mut status, mut reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
    if status == 401 && no_sign_in {
        return Err(cannot_sign_in());
    }
    if status == 401 {
        sign_in(
            &connection.hub,
            provider_name(provider),
            Some(&connection.origin_host),
        )
        .await?;
        session = session_for(&connection.hub, provider, &connection.origin_host, false).await?;
        (status, reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
    }
    if not_connected(status, &reply) {
        refresh_connections(&connection.hub, &session).await;
        (status, reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
        if let Some(wait) = wait
            && not_connected(status, &reply)
        {
            println!(
                "{}",
                install_help(wait.install_url.as_deref(), &connection.repo)
            );
            // With --yes nobody is there to press Enter.
            while !wait.ui.yes
                && wait
                    .ui
                    .retry("Press Enter after installing, or type skip to stop.")
            {
                refresh_connections(&connection.hub, &session).await;
                (status, reply) = mint_request(&connection.hub, &session, &connection.repo).await?;
                if !not_connected(status, &reply) {
                    break;
                }
                println!("Still not connected.");
            }
        }
    }
    match (status, reply["error"].as_str()) {
        (403, Some("not_connected")) => Err(invalid(
            "this repository isn't connected to this Hub yet; ask a Hub admin to connect it",
        )),
        (403, _) => Err(invalid(
            &crate::hub_cli::refusal_message(&reply).unwrap_or_else(|| {
                "your forge account can't see that repository, or it was renamed or moved; check the name and your access".to_owned()
            }),
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
        "warning: the managed token {id} on {} could not be revoked; it remains valid until {until}. Revoke that token manually; no retry was queued.",
        connection.hub,
        until = crate::hub_cli::when(expiry, unix_now()),
    );
}

/// Revokes a managed token with the token itself (no sign-in needed), else with a saved
/// sign-in for its hub. A 401 to the token means it is already revoked or expired.
async fn revoke_owned(connection: &Connection, id: &str, token: Option<&str>) -> bool {
    let Ok(client) = forge_http::client() else {
        return false;
    };
    if let Some(token) = token {
        let target = format!("{}/v1/hub/token", connection.hub);
        if let Ok((204 | 401, _)) = call(&client, Method::DELETE, &target, Some(token), None).await
        {
            return true;
        }
    }
    let Ok((hub, session)) = load_session(Some(&connection.hub)) else {
        return false;
    };
    hub == connection.hub
        && call(
            &client,
            Method::DELETE,
            &format!("{}/v1/hub/tokens/{id}", connection.hub),
            Some(&session),
            None,
        )
        .await
        .is_ok_and(|(status, _)| matches!(status, 204 | 404))
}

/// Retires the managed token of a connection that `connect` just replaced.
async fn retire_owned(connection: &Connection) {
    let (Some(id), Some(expiry)) = (
        connection.owned_token_id.as_deref(),
        connection.owned_token_expires_at,
    ) else {
        return;
    };
    let token = read_token(&connection.token_file).ok();
    match remove_unreferenced_owned_file(connection) {
        Ok(RetiredFile::Referenced) => {
            eprintln!(
                "warning: the managed token {id} was preserved because another client connection references its token file; it remains valid until {until}",
                until = crate::hub_cli::when(expiry, unix_now()),
            );
            return;
        }
        Ok(RetiredFile::Removed) => {}
        Err(error) => {
            eprintln!(
                "warning: the managed token {id} and its file were preserved because AgentSCM could not verify that the file is unreferenced ({error}); the token remains valid until {until}. No retry was queued.",
                until = crate::hub_cli::when(expiry, unix_now()),
            );
            return;
        }
    }
    if revoke_owned(connection, id, token.as_deref()).await {
        println!(
            "Deleted its token file and revoked token {id} on {}.",
            connection.hub
        );
    } else {
        warn_unrevoked(connection, id, expiry);
    }
}

pub(crate) async fn connect(
    args: ConnectArgs,
    root: PathBuf,
    wait: Option<&AppWait<'_>>,
) -> Result<()> {
    connect_inner(args, root, wait, None).await
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProgramIdentity {
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn program_identity(path: &Path) -> Result<ProgramIdentity> {
    let metadata = path.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("the AgentSCM executable is not a file"));
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(ProgramIdentity {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn optional_program_identity(path: &Path) -> Result<Option<ProgramIdentity>> {
    match path.metadata() {
        Ok(_) => program_identity(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct ProgramFence {
    paths: Vec<(PathBuf, Option<ProgramIdentity>)>,
}

fn invocation_program_fence() -> Result<ProgramFence> {
    let running = std::env::current_exe()?;
    let identity = program_identity(&running)?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()));
    let stable = home.map(|home| {
        PathBuf::from(home)
            .join(".agentscm/bin")
            .join(if cfg!(windows) {
                "agentscm.exe"
            } else {
                "agentscm"
            })
    });
    let manifest = crate::hub_cli::config_dir()?.join("agentscm/quickstart.json");
    let manifest_identity = optional_program_identity(&manifest)?;
    let mut paths = vec![(running, Some(identity)), (manifest, manifest_identity)];
    if let Some(stable) = stable {
        let stable_identity = optional_program_identity(&stable)?;
        paths.push((stable, stable_identity));
    }
    Ok(ProgramFence { paths })
}

fn setup_program_identity(setup: &SetupInstallation<'_>) -> Result<ProgramIdentity> {
    #[cfg(unix)]
    return program_identity(setup.program);
    #[cfg(not(unix))]
    {
        let _ = setup;
        Ok(ProgramIdentity {
            length: 0,
            modified: None,
        })
    }
}

fn revalidate_after_wait(
    root: &Path,
    origin: (&str, &str),
    existing: Option<&Connection>,
    checked_token: Option<(&Path, Option<&str>)>,
    setup: Option<&SetupInstallation<'_>>,
    expected_setup_program: Option<&ProgramIdentity>,
    invocation_program: &ProgramFence,
) -> Result<()> {
    if root.canonicalize().ok().as_deref() != Some(root)
        || canonical_root(root).ok().as_deref() != Some(root)
        || !origin_identity(root).is_ok_and(|(host, repo)| {
            host.eq_ignore_ascii_case(origin.0) && repo.eq_ignore_ascii_case(origin.1)
        })
    {
        return Err(invalid("the checkout origin changed during setup; retry"));
    }
    let latest = load()?
        .into_iter()
        .find(|connection| connection.root == root);
    if latest.as_ref() != existing {
        return Err(invalid("the client connection changed during setup; retry"));
    }
    if let Some((path, expected)) = checked_token
        && read_token(path).ok().as_deref() != expected
    {
        return Err(invalid("the client token changed during setup; retry"));
    }
    if invocation_program
        .paths
        .iter()
        .any(|(path, expected)| optional_program_identity(path).ok().as_ref() != Some(expected))
    {
        return Err(invalid(
            "the AgentSCM installation changed during setup; retry",
        ));
    }
    if let Some(setup) = setup {
        validate_setup_installation(setup, root)?;
        if setup_program_identity(setup).ok().as_ref() != expected_setup_program {
            return Err(invalid(
                "the installed AgentSCM executable changed during setup; retry",
            ));
        }
    }
    Ok(())
}

async fn connect_inner(
    args: ConnectArgs,
    root: PathBuf,
    wait: Option<&AppWait<'_>>,
    setup: Option<SetupInstallation<'_>>,
) -> Result<()> {
    let initial_lifecycle = lifecycle_lock()?;
    let invocation_program = invocation_program_fence()?;
    if let Some(setup) = setup.as_ref() {
        validate_setup_installation(setup, &root)?;
    }
    let setup_program_identity = setup.as_ref().map(setup_program_identity).transpose()?;
    let (origin_host, inferred_repo) = origin_identity(&root)?;
    let repo = match args.repo {
        Some(repo) if !valid_repo(&repo) => {
            return Err(invalid("--repo must look like owner/name"));
        }
        Some(repo) if !repo.eq_ignore_ascii_case(&inferred_repo) => {
            return Err(invalid("--repo does not match the checkout origin"));
        }
        Some(repo) => repo,
        None => inferred_repo.clone(),
    };
    let hub = match args.hub.as_deref() {
        Some(hub) => check_base_url(hub).map_err(map_hub_error)?,
        None => crate::hub_cli::only_saved_hub()?,
    };
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
    let supplied_secret = supplied_token.as_deref().map(read_token).transpose()?;
    let connections = load()?;
    let existing = connections.iter().find(|saved| saved.root == root).cloned();
    let existing_token = existing
        .as_ref()
        .map(|saved| (saved.token_file.clone(), read_token(&saved.token_file).ok()));
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
    drop(initial_lifecycle);
    if supplied_token.is_none()
        && !changed
        && let Some(saved) = existing.as_ref().filter(|saved| is_current(saved))
        && let Some(saved_token) = existing_token
            .as_ref()
            .and_then(|(_, token)| token.as_ref())
        && let Ok(verified) = verify_token(saved, saved_token).await
    {
        let final_lifecycle = lifecycle_lock()?;
        revalidate_after_wait(
            &root,
            (&origin_host, &inferred_repo),
            existing.as_ref(),
            Some((&saved.token_file, Some(saved_token))),
            setup.as_ref(),
            setup_program_identity.as_ref(),
            &invocation_program,
        )?;
        if saved.repo == verified.repository {
            if let Some(setup) = setup.as_ref() {
                install_setup(setup, &root)?;
            }
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
            if let Some(setup) = setup.as_ref() {
                install_setup(setup, &root)?;
            }
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
        drop(final_lifecycle);
    }
    let mut connection = Connection {
        root: root.clone(),
        origin_host: origin_host.clone(),
        hub,
        repo,
        token_file: supplied_token.clone().unwrap_or_default(),
        report_completion: existing
            .as_ref()
            .is_some_and(|saved| !changed && saved.report_completion),
        owned_token_id: None,
        owned_token_expires_at: None,
        // The agents' files stay in the checkout whichever hub it connects to.
        agents: existing
            .as_ref()
            .map(|saved| saved.agents.clone())
            .unwrap_or_default(),
    };
    let mut owned_minted = None;
    let verified = if supplied_token.is_some() {
        let verified = verify_token(
            &connection,
            supplied_secret
                .as_deref()
                .ok_or_else(|| invalid("cannot read token file"))?,
        )
        .await?;
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
        let minted = mint_agent_token(&connection, provider, wait, args.no_sign_in).await?;
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
    let final_lifecycle = match lifecycle_lock() {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            if let Some(minted) = owned_minted.as_ref() {
                revoke_minted(&connection, minted).await;
            }
            return Err(error);
        }
    };
    let checked_token = supplied_token
        .as_deref()
        .map(|path| (path, supplied_secret.as_deref()))
        .or_else(|| {
            existing_token
                .as_ref()
                .map(|(path, token)| (path.as_path(), token.as_deref()))
        });
    if let Err(error) = revalidate_after_wait(
        &root,
        (&origin_host, &inferred_repo),
        existing.as_ref(),
        checked_token,
        setup.as_ref(),
        setup_program_identity.as_ref(),
        &invocation_program,
    ) {
        drop(final_lifecycle);
        if let Some(minted) = owned_minted.as_ref() {
            revoke_minted(&connection, minted).await;
        }
        return Err(error);
    }
    if let Some(minted) = owned_minted.as_ref()
        && let Err(error) = write_owned_token(&connection.token_file, &minted.token)
    {
        drop(final_lifecycle);
        revoke_minted(&connection, minted).await;
        return Err(error);
    }
    if let Err(error) = update(&root, existing.as_ref(), Some(connection.clone())) {
        if owned_minted.is_some() {
            let _ = std::fs::remove_file(&connection.token_file);
        }
        drop(final_lifecycle);
        if let Some(minted) = owned_minted.as_ref() {
            revoke_minted(&connection, minted).await;
        }
        return Err(error);
    }
    let install_result = setup
        .as_ref()
        .map_or(Ok(()), |setup| install_setup(setup, &root));
    let retired = existing.as_ref().filter(|saved| {
        saved.token_file != connection.token_file && saved.owned_token_id.is_some()
    });
    drop(final_lifecycle);
    if let Some(saved) = retired {
        retire_owned(saved).await;
    }
    install_result?;
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

async fn remove(args: RemoveArgs) -> Result<()> {
    remove_checkout(&removal_root(args.root.as_deref())?).await
}

/// Undoes `client setup` for one checkout: its agents' files (and a plugin no other checkout
/// uses), the connection, and its managed token, which is revoked. `agentscm uninstall` runs
/// this for every checkout quickstart connected.
pub(crate) async fn remove_checkout(root: &Path) -> Result<()> {
    let _lifecycle = lifecycle_lock()?;
    let connections = load()?;
    let connection = connections
        .iter()
        .find(|connection| connection.root == root)
        .cloned()
        .ok_or_else(|| invalid("no client connection for this exact registered root"))?;
    // Agents first: if one cannot be taken out, the connection stays so removal can be retried.
    #[cfg(unix)]
    for name in &connection.agents {
        let agent = <crate::client_setup::Agent as clap::ValueEnum>::from_str(name, false)
            .map_err(|_| invalid("connections.json names an unknown agent"))?;
        let last = !connections
            .iter()
            .any(|other| other.root != root && other.agents.contains(name));
        crate::client_setup::remove(agent, root, last)?;
    }
    if let (Some(id), Some(expiry)) = (
        connection.owned_token_id.as_deref(),
        connection.owned_token_expires_at,
    ) {
        let shared = connections
            .iter()
            .any(|other| other.root != root && other.token_file == connection.token_file);
        let token = read_token(&connection.token_file).ok();
        if !shared && !revoke_owned(&connection, id, token.as_deref()).await {
            // The connection and token file stay, so running `client remove` again retries.
            let mut kept = connection.clone();
            kept.agents.clear();
            update(root, Some(&connection), Some(kept))?;
            return Err(invalid(&format!(
                "the agents' files were removed, but token {id} on {} could not be revoked (the hub did not answer or refused); it stays valid until {until}. Its connection and token file are kept: run `agentscm client remove --root {}` again when the hub answers (signing in with `agentscm hub login` first also lets it revoke)",
                connection.hub,
                root.display(),
                until = crate::hub_cli::when(expiry, unix_now()),
            )));
        }
    }
    update(root, Some(&connection), None)?;
    println!("Removed the client connection for {}.", root.display());
    if let (Some(id), Some(expiry)) = (
        connection.owned_token_id.as_deref(),
        connection.owned_token_expires_at,
    ) {
        match remove_unreferenced_owned_file(&connection) {
            Ok(RetiredFile::Removed) => println!(
                "Deleted its token file and revoked token {id} on {}.",
                connection.hub
            ),
            Ok(RetiredFile::Referenced) => println!(
                "Kept token {id} and its file: another client connection uses them (valid until {until}).",
                until = crate::hub_cli::when(expiry, unix_now()),
            ),
            Err(error) => eprintln!(
                "warning: token {id} was revoked, but its file {} could not be deleted ({error})",
                connection.token_file.display()
            ),
        }
    } else {
        println!(
            "The token file {} was given to setup, so it and its token were left unchanged.",
            connection.token_file.display()
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

    #[cfg(target_os = "macos")]
    fn integration_connection(agents: Vec<String>) -> Connection {
        Connection {
            root: PathBuf::from("/synthetic/checkout"),
            origin_host: "github.com".into(),
            hub: "https://hub.example.test".into(),
            repo: "acme/widgets".into(),
            token_file: PathBuf::from("/synthetic/token"),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn existing_integration_requires_the_exact_current_binding() {
        let saved = integration_connection(Vec::new());
        let token = "opaque-token";
        let generation = onboarding_client(&saved.root, token);
        for (hub, repo, expected_generation, current) in [
            (
                "https://other.example.test",
                saved.repo.as_str(),
                generation.as_str(),
                true,
            ),
            (saved.hub.as_str(), "other/repo", generation.as_str(), true),
            (
                saved.hub.as_str(),
                saved.repo.as_str(),
                "other-generation",
                true,
            ),
            (
                saved.hub.as_str(),
                saved.repo.as_str(),
                generation.as_str(),
                false,
            ),
        ] {
            assert!(
                integration_replacement_if(
                    &saved,
                    token,
                    hub,
                    repo,
                    expected_generation,
                    crate::client_setup::Agent::Codex,
                    current,
                )
                .is_err()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn recording_an_existing_integration_is_idempotent() {
        let saved = integration_connection(vec!["codex".into()]);
        let token = "opaque-token";
        let generation = onboarding_client(&saved.root, token);
        let replacement = integration_replacement_if(
            &saved,
            token,
            &saved.hub,
            &saved.repo,
            &generation,
            crate::client_setup::Agent::Codex,
            true,
        )
        .unwrap();
        assert_eq!(replacement, saved);
    }

    #[test]
    fn lifecycle_lease_serializes_client_mutations_and_releases() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client-lifecycle.lock");
        let first = lifecycle_lock_at(&path).unwrap();
        assert!(lifecycle_lock_at(&path).is_err());
        drop(first);
        assert!(lifecycle_lock_at(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn setup_program_is_rechecked_after_lifecycle_acquisition() {
        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("agentscm");
        std::fs::write(&program, b"program").unwrap();
        assert!(validate_setup_program(&program).is_ok());
        std::fs::remove_file(&program).unwrap();
        assert!(validate_setup_program(&program).is_err());
    }

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
            agents: Vec::new(),
        };
        let verified = verify_token(&connection, "agent-synthetic").await.unwrap();
        assert_eq!(verified.login, "member");
        assert_eq!(verified.repository, "Acme/Widgets");

        let mint = requests.recv().unwrap();
        let (headers, body) = mint.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("authorization: Bearer session-synthetic"));
        assert!(!body.contains("session-synthetic"));
        assert!(body.contains("\"repository\":\"acme/widgets\""));
        assert!(body.contains("\"ttl_seconds\":2592000"));
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
