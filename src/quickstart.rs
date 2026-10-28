//! `agentscm quickstart` and `agentscm uninstall`: guided setup of early collision
//! warnings (and, optionally, live warnings through a hub and coding agents set up by `client setup`).
//! Every write is announced first and recorded in `quickstart.json`, which `uninstall`
//! reads to take back exactly what was done. Nothing here is telemetry.
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscm::hub::forge_http::{self, check_base_url};
use agentscm::hub::slack::setup as slack_setup;
use agentscm::{Error, Result};
use clap::{Args, ValueEnum};
use reqwest::Method;

use crate::hub_cli::{self, Provider, call, clean};

mod binary;
pub(crate) mod detect;
mod hub_local;
mod hub_native;
mod hub_setup;
#[cfg(any(target_os = "macos", test))]
mod launcher;
mod manifest;
pub(crate) mod process;

use crate::prompt::Ui;
use detect::{Detected, Forge};
#[cfg(unix)]
use manifest::SessionRecord;
use manifest::{Manifest, file_hash, sha_hex};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum HubChoice {
    /// Start a managed native hub, or explicitly choose Docker (GitHub only).
    Local,
    /// Connect to a hub that already runs; needs --hub-url.
    Existing,
    /// Only the collision check; with no hub, no agents are set up and there are no live warnings.
    Skip,
    /// Print the hosted guide, then connect only if --hub-url is given (chosen with --hosted).
    #[value(skip)]
    Hosted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HubRuntime {
    /// Run the installed native binary as a managed user service (macOS ARM64).
    Native,
    /// Run the matching Linux container image with Docker Compose.
    Docker,
}

#[derive(Debug, Args)]
pub struct QuickstartArgs {
    /// Answer every question with the recommended choice (needed without a terminal).
    /// The local hub's administrator must then be given with --github-login.
    #[arg(long)]
    yes: bool,
    /// Run everything on this machine (recommended for the alpha); same as --hub local.
    #[arg(hide = true, long, conflicts_with_all = ["hub", "hosted"])]
    local: bool,
    /// Use a shared hub on a server: prints the agent-guided hosted guide (unsupported in
    /// the alpha), then connects only when --hub-url is given.
    #[arg(hide = true, long, conflicts_with = "hub")]
    hosted: bool,
    /// How to get live warnings: local, existing or skip, or the hub's URL (the same as
    /// --hub-url), so `--hub URL` means one thing in every command.
    #[arg(hide = true, id = "hub", long = "hub", value_name = "local|existing|skip|URL", value_parser = parse_hub)]
    hub_arg: Option<HubArg>,
    /// Set from `--hub` once parsed.
    #[arg(skip)]
    hub: Option<HubChoice>,
    /// The Hub to join, such as https://hub.example.com.
    #[arg(long)]
    hub_url: Option<String>,
    /// Local hub runtime; native is recommended on qualified macOS ARM64 hosts.
    #[arg(hide = true, long, value_enum)]
    hub_runtime: Option<HubRuntime>,
    /// Loopback port for the local hub; reuse the recorded port on resume.
    #[arg(hide = true, long, value_parser = clap::value_parser!(u16).range(1..))]
    hub_port: Option<u16>,
    /// Instance name for an isolated local service or Docker Compose project.
    #[arg(hide = true, long)]
    hub_project: Option<String>,
    /// Your GitHub login, to make you the local hub's administrator.
    #[arg(hide = true, long)]
    github_login: Option<String>,
    /// An agent token you already have; it is checked and used instead of signing in.
    #[arg(hide = true, long)]
    agent_token_file: Option<PathBuf>,
    /// Prepared private Slack adapter configuration for a managed local Hub.
    #[arg(hide = true, long)]
    slack_config_file: Option<PathBuf>,
    /// Which forge to sign in with (default: the one in origin's URL).
    #[arg(hide = true, long, value_enum)]
    provider: Option<Provider>,
    /// Coding agent to set up (repeatable; default: every one installed on this computer).
    #[cfg(unix)]
    #[arg(
        long = "only",
        alias = "agent",
        value_name = "AGENT",
        value_enum,
        conflicts_with = "no_agents"
    )]
    agent: Vec<crate::client_setup::Agent>,
    /// Set up no coding agent.
    #[arg(hide = true, long)]
    no_agents: bool,
    /// Accepted for older scripts: setup no longer checks for colliding branches.
    #[arg(hide = true, long)]
    no_collisions: bool,
    /// Also write the repository's agent instructions (agentscm init-repo). Without it, setup
    /// writes nothing to the repository.
    #[arg(hide = true, long)]
    init: bool,
    /// Open the optional native status-bar companion after setup succeeds (macOS only).
    #[arg(hide = true, long)]
    tray: bool,
    /// Print every file and setting this would write, then stop without writing.
    #[arg(long)]
    dry_run: bool,
    /// Print one JSON object on stdout when done; the progress text goes to stderr.
    #[arg(long)]
    json: bool,
}

/// `--hub` is a choice or, when it looks like a URL, the hub itself.
#[derive(Clone, Debug)]
pub enum HubArg {
    Choice(HubChoice),
    Url(String),
}

fn parse_hub(value: &str) -> std::result::Result<HubArg, String> {
    if value.contains("://") {
        return Ok(HubArg::Url(value.to_owned()));
    }
    HubChoice::from_str(value, true)
        .map(HubArg::Choice)
        .map_err(|_| "expected local, existing, skip or the hub's URL".to_owned())
}

impl QuickstartArgs {
    /// Splits `--hub` into the choice and `--hub-url`; a URL in both must be the same one.
    fn resolve_hub(&mut self) -> Result<()> {
        match self.hub_arg.take() {
            Some(HubArg::Choice(choice)) => self.hub = Some(choice),
            Some(HubArg::Url(url)) => {
                if self.hub_url.as_ref().is_some_and(|given| *given != url) {
                    return Err(invalid("--hub and --hub-url name different hubs; pass one"));
                }
                self.hub = Some(HubChoice::Existing);
                self.hub_url = Some(url);
            }
            None => {}
        }
        Ok(())
    }
}

#[derive(Debug, Args)]
pub struct UninstallArgs {
    /// Do not ask; take back everything quickstart recorded.
    #[arg(long)]
    yes: bool,
    /// Also delete the recorded local hub data (native directory or Docker volume).
    #[arg(long)]
    remove_hub_data: bool,
}

const NEEDS_TERMINAL: &str = "agentscm quickstart asks questions, and there is no terminal here. \
To run it without one, pass --yes and choose with flags: --local (recommended) or --hosted, \
--hub local|existing|skip|URL, --hub-url URL, \
--agent-token-file PATH, --provider github|gitlab, --github-login LOGIN, \
--hub-runtime native|docker, --hub-port PORT, --hub-project NAME, \
--slack-config-file ABSOLUTE_PATH, --agent claude|codex|cursor|copilot|gemini (repeatable) or --no-agents, --init, --tray";

const HUB_EXPLAINED: &str =
    "A hub is a small shared server that lets agents on different machines see each other's work.";

const HOSTED_GUIDE: &str = "\
Hosted (a shared hub on a server) is not supported in the alpha: it will likely break and is not \
for production. Quickstart does not set up a server. Hand this guide to your coding agent and work \
through it together:
  https://github.com/agentscm/agentscm/blob/main/docs/hosted.md
If your team's hub already runs, give its URL to connect this machine to it.";

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

/// `~/...` for paths under the home directory.
fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| invalid("HOME is not set"))
}

fn short(error: &Error) -> String {
    let text = error.to_string();
    text.strip_prefix("invalid: ").unwrap_or(&text).to_owned()
}

pub(crate) struct SlackBundle {
    pub(crate) config: PathBuf,
    pub(crate) files: Vec<(String, Vec<u8>)>,
}

fn slack_bundle(
    source: Option<&Path>,
    target: &Path,
) -> std::result::Result<Option<SlackBundle>, String> {
    let Some(source) = source else {
        return Ok(None);
    };
    let files = slack_setup::bundle(source, target)
        .map_err(|_| "The prepared Slack configuration is invalid.".to_owned())?;
    if !files.iter().any(|(name, _)| name == "config.json") {
        return Err("The prepared Slack bundle has no config.json.".into());
    }
    Ok(Some(SlackBundle {
        config: target.join("config.json"),
        files,
    }))
}

fn preflight_slack(args: &QuickstartArgs) -> Result<()> {
    let Some(path) = args.slack_config_file.as_deref() else {
        return Ok(());
    };
    if !path.is_absolute() {
        return Err(invalid("--slack-config-file must be an absolute path"));
    }
    if args.hub_url.is_some() || matches!(args.hub, Some(HubChoice::Existing | HubChoice::Skip)) {
        return Err(invalid(
            "--slack-config-file requires a managed local Hub (--hub local)",
        ));
    }
    slack_setup::check(path).map_err(|_| invalid("the prepared Slack configuration is invalid"))?;
    Ok(())
}

// ---- files ----

/// Flushes a written file to disk. Windows flushes only through a writable handle.
fn sync_file(path: &Path) -> std::result::Result<(), String> {
    #[cfg(unix)]
    let file = std::fs::File::open(path);
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new().write(true).open(path);
    file.and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())
}

/// Makes a rename or removal in `path` durable. Windows cannot open a directory as a file and has
/// no such step.
fn sync_dir(path: &Path) -> std::result::Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(dir)
}

// Windows has no mode bits; a new directory under the user's profile inherits its ACL.
#[cfg(not(unix))]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

// ---- hub helpers ----

fn client() -> Result<reqwest::Client> {
    forge_http::client().map_err(hub_cli::map_hub_error)
}

async fn healthy(client: &reqwest::Client, url: &str) -> bool {
    let url = format!("{url}/v1/hub/health");
    matches!(
        call(client, Method::GET, &url, None, None).await,
        Ok((200, _))
    )
}

async fn wait_healthy(url: &str, limit: Duration) -> bool {
    let Ok(client) = client() else { return false };
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if healthy(&client, url).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

// ---- the flow ----

/// What is true about live warnings when the run ends.
#[derive(Default)]
struct Live {
    hub: Option<String>,
    hub_up: bool,
    /// The repository this checkout is connected to.
    repo: Option<String>,
    /// Agents set up in this checkout.
    agents: Vec<String>,
    /// Agents set up for all projects, connected or not.
    #[cfg(unix)]
    installed: Vec<crate::client_setup::Agent>,
    /// Agents that could not be set up, with the reason.
    #[cfg(unix)]
    failed: Vec<(crate::client_setup::Agent, String)>,
    /// This run tried to set up at least one agent.
    wanted_agents: bool,
    /// Why setting up the coding agents failed.
    agent_error: Option<String>,
    /// Why a requested local hub isn't running.
    hub_error: Option<String>,
    pending_local: Option<String>,
}

impl Live {
    /// The last line of every run; `cli` is the program to run again (see `cli_command`).
    fn line(&self, cli: &str) -> String {
        let off = |reason: &str, todo: &str| format!("Live warnings: off — {reason} ({todo})");
        if let Some(command) = &self.pending_local {
            return off(
                "the local hub is waiting for GitHub App setup",
                &format!("finish creating and installing the app, then run `{command}`"),
            );
        }
        if let Some(error) = &self.agent_error {
            return off(
                &format!("coding agent setup failed: {error}"),
                &format!("fix that, then run `{cli} quickstart` again"),
            );
        }
        match (&self.hub, &self.repo) {
            (None, _) if self.hub_error.is_some() => off(
                &format!(
                    "local hub not started: {}",
                    self.hub_error.as_deref().unwrap_or_default()
                ),
                &format!("fix that, then run `{cli} quickstart --local` again"),
            ),
            (None, _) => off(
                "no hub chosen",
                &format!(
                    "run `{cli} quickstart --local` for a hub on this machine, or `{cli} quickstart --hub-url URL` for your team's hub"
                ),
            ),
            (Some(url), _) if !self.hub_up => off(
                &format!("the hub at {url} isn't answering"),
                &format!("start it, then run `{cli} quickstart` again"),
            ),
            #[cfg(unix)]
            (Some(url), None) if !self.installed.is_empty() => format!(
                "Live warnings: on for repositories on {url}; each connects when an agent starts in it"
            ),
            (Some(_), None) if !self.wanted_agents => off(
                "no coding agent was set up",
                &format!("run `{cli} quickstart --agent claude|codex|cursor|copilot|gemini`"),
            ),
            (Some(_), None) => off(
                "this checkout isn't connected to the hub",
                &format!("run `{cli} quickstart` again, or pass `--agent-token-file`"),
            ),
            (Some(_), Some(_)) if self.agents.is_empty() => off(
                "no coding agent was set up",
                &format!("run `{cli} quickstart --agent claude`"),
            ),
            (Some(url), Some(repo)) => format!("Live warnings: on (hub {url}, repo {repo})"),
        }
    }
}

struct Cx<'a> {
    ui: &'a Ui,
    args: &'a QuickstartArgs,
    tty: bool,
    home: PathBuf,
    found: Detected,
    manifest: Manifest,
    client: reqwest::Client,
    install_url: Option<String>,
    live: Live,
    /// Repository files `init_stage` wrote (relative to the root); not in the manifest.
    init_written: Vec<PathBuf>,
    failure: Option<String>,
}

/// `--dry-run`: what quickstart would write, from the same plans it follows, without writing.
fn dry_run(args: &QuickstartArgs) -> Result<()> {
    let home = home_dir()?;
    let found = detect::detect(&std::env::current_dir()?);
    println!("Dry run: nothing is written. Quickstart would:");
    match program_plan(&home) {
        Some((binary::Program::Copy, _)) => println!(
            "  copy this program to {}",
            binary::bin_dir(&home).join(binary::LINK).display()
        ),
        Some((binary::Program::InstallDir(link), _)) => {
            println!("  keep using {} (the install directory)", link.display());
        }
        _ => println!("  keep using this program as it is"),
    }
    let hub = if args.local || matches!(args.hub, Some(HubChoice::Local)) {
        "start a managed local hub (service files under ~/.agentscm/hub)".to_owned()
    } else if let Some(url) = &args.hub_url {
        format!("connect to the hub at {url}")
    } else {
        "ask which hub to use, or skip live warnings".to_owned()
    };
    println!("  {hub}");
    #[cfg(unix)]
    {
        let agents: Vec<&str> = if args.no_agents {
            Vec::new()
        } else if !args.agent.is_empty() {
            args.agent.iter().map(|agent| agent.name()).collect()
        } else {
            found.agents.iter().map(|agent| agent.name()).collect()
        };
        for agent in agents {
            println!(
                "  set up {agent}: its hooks and MCP entry, and this checkout's connection (agentscm client setup)"
            );
        }
    }
    if args.init {
        println!("  write the repository's agent instructions (agentscm init-repo)");
    }
    println!(
        "  record every change in {} for `uninstall`",
        manifest::path()?.display()
    );
    if found.repo_root.is_none() {
        println!("Run it inside a Git repository to connect agents.");
    }
    Ok(())
}

/// With `--json`, stdout carries only the final object: the progress text moves to stderr and
/// the original stdout is kept for the object.
#[cfg(unix)]
fn json_stdout() -> Result<std::fs::File> {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    std::io::stdout().flush()?;
    // SAFETY: dup and dup2 take only descriptors this process owns; the new descriptor is
    // wrapped once, in the File this returns.
    unsafe {
        let saved = libc::dup(1);
        if saved < 0 || libc::dup2(2, 1) < 0 {
            return Err(invalid("--json could not redirect progress text to stderr"));
        }
        Ok(std::fs::File::from_raw_fd(saved))
    }
}

fn requested_agents(args: &QuickstartArgs) -> Vec<String> {
    #[cfg(unix)]
    return args
        .agent
        .iter()
        .map(|agent| agent.name().to_owned())
        .collect();
    #[cfg(not(unix))]
    return {
        let _ = args;
        Vec::new()
    };
}

fn json_report(cx: &Cx<'_>) -> serde_json::Value {
    let installed = &cx.live.agents;
    let skipped: Vec<String> = requested_agents(cx.args)
        .into_iter()
        .filter(|agent| !installed.contains(agent))
        .collect();
    let needs_user: Vec<&str> = [
        cx.live.pending_local.as_deref(),
        cx.live.agent_error.as_deref(),
        cx.live.hub_error.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    serde_json::json!({
        "agents": { "installed": installed, "skipped": skipped, "needs_user": needs_user },
        "hub": cx.live.hub,
        "hub_up": cx.live.hub_up,
        "signed_in": cx.live.hub.as_deref().is_some_and(|hub| hub_cli::has_session(Some(hub)).unwrap_or(false)),
    })
}

pub async fn run(mut args: QuickstartArgs) -> Result<u8> {
    args.resolve_hub()?;
    if args.dry_run {
        dry_run(&args)?;
        return Ok(0);
    }
    #[cfg(unix)]
    let json_out = if args.json {
        Some(json_stdout()?)
    } else {
        None
    };
    #[cfg(not(unix))]
    if args.json {
        return Err(invalid("--json is not available on Windows yet"));
    }
    let tty = std::io::stdin().is_terminal();
    if !args.yes && !tty {
        return Err(invalid(NEEDS_TERMINAL));
    }
    if args.tray && !cfg!(target_os = "macos") {
        return Err(invalid("--tray is currently supported only on macOS"));
    }
    // Validate the complete private input before acquiring the lease or performing any install
    // write. Supplying this flag is the only way quickstart enables the optional Slack adapter.
    preflight_slack(&args)?;
    let ui = Ui { yes: args.yes };
    if args.yes
        && (args.local || matches!(args.hub, Some(HubChoice::Local)))
        && args.github_login.is_none()
        && Manifest::load()?.is_none_or(|m| m.local_hub.is_none() && m.native_hub.is_none())
    {
        hub_setup::github_login(None, &ui).map_err(|message| invalid(&message))?;
    }
    let home = home_dir()?;
    let _quickstart_lease = manifest::acquire_lease()?;
    let cwd = std::env::current_dir()?;
    let found = detect::detect(&cwd);
    let mut cx = Cx {
        ui: &ui,
        args: &args,
        tty,
        home,
        found,
        manifest: Manifest::load()?.unwrap_or_else(Manifest::fresh),
        client: client()?,
        install_url: None,
        live: Live::default(),
        init_written: Vec::new(),
        failure: None,
    };
    let record_path = manifest::path()?;
    let original_record = record_fingerprint(&record_path);
    let hub_files = [
        cx.home.join(".agentscm/hub/compose.yaml"),
        cx.home.join(".agentscm/hub/.env"),
    ];
    let original_hub_files = hub_files.each_ref().map(|path| file_hash(path));
    let original_program = program_fingerprint(&cx.home);
    // Signing in during this run rewrites the saved sessions, which the summary lists.
    let sessions_file = hub_cli::sessions_path().ok();
    let original_sessions = sessions_file.as_deref().map(file_hash);
    // Running from the install directory is undoable even if nothing else is written.
    if let Some((binary::Program::InstallDir(_), _)) = program_plan(&cx.home) {
        cx.manifest.bin_dir = Some(binary::bin_dir(&cx.home));
    }
    status(&cx);
    if let Some(url) = choose_hub(&mut cx).await {
        cx.live.hub_up = healthy(&cx.client, &url).await;
        cx.live.hub = Some(url.clone());
        if !cx.live.hub_up {
            println!(
                "The hub at {url} isn't answering, so no agent was connected to it. Start it, then run quickstart again."
            );
        }
    }
    agents_stage(&mut cx).await;
    init_stage(&mut cx);
    let command = cli_command(&cx);
    // Without an agent to set up (--no-agents, or none found) nothing is connected on purpose.
    #[cfg(unix)]
    let nothing_installed = cx.live.installed.is_empty();
    #[cfg(not(unix))]
    let nothing_installed = true;
    let requested_hub_incomplete = (args.local
        || matches!(args.hub, Some(HubChoice::Local | HubChoice::Existing))
        || args.hub_url.is_some())
        && (!cx.live.hub_up
            || (cx.live.repo.is_none() && cx.live.wanted_agents && nothing_installed));
    let ready = cx.failure.is_none()
        && cx.live.pending_local.is_none()
        && cx.live.agent_error.is_none()
        && !requested_hub_incomplete;
    let tray_result = if ready && args.tray {
        launch_tray(&cx)
    } else {
        Ok(())
    };
    if !cx.manifest.is_empty() {
        cx.manifest.save()?;
    }
    let changed = record_fingerprint(&record_path) != original_record
        || hub_files.each_ref().map(|path| file_hash(path)) != original_hub_files
        || program_fingerprint(&cx.home) != original_program
        || sessions_file.as_deref().map(file_hash) != original_sessions
        || !cx.init_written.is_empty();
    summary(&cx, changed, &command);
    #[cfg(target_os = "macos")]
    if !(ready && args.tray) {
        println!("Menu bar app: {command} tray");
    }
    println!("{}", cx.live.line(&command));
    #[cfg(unix)]
    if let Some(mut out) = json_out {
        use std::io::Write;
        writeln!(out, "{}", json_report(&cx))?;
    }
    if let Some(failure) = cx.failure {
        return Err(invalid(&failure));
    }
    if cx.live.pending_local.is_some() {
        return Ok(3);
    }
    if let Some(error) = &cx.live.agent_error {
        return Err(invalid(&format!(
            "setting up the coding agents failed: {error}"
        )));
    }
    if requested_hub_incomplete {
        return Err(invalid(
            "the requested hub setup is incomplete; see the status and recovery instructions above",
        ));
    }
    tray_result?;
    Ok(0)
}

fn record_fingerprint(path: &Path) -> (Option<String>, Option<u32>) {
    #[cfg(unix)]
    let mode = path.metadata().ok().map(|m| m.permissions().mode() & 0o777);
    #[cfg(not(unix))]
    let mode = None;
    (file_hash(path), mode)
}

type ProgramFileFingerprint = (PathBuf, Option<String>, Option<PathBuf>, Option<u32>);

/// Stable managed-program state. The symlink target catches link repair while the digest catches
/// Windows link-copy repair and replacement of a versioned executable with the same filename.
fn program_fingerprint(home: &Path) -> (Option<u32>, Vec<ProgramFileFingerprint>) {
    let directory = binary::bin_dir(home);
    #[cfg(unix)]
    let directory_mode = directory
        .metadata()
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o777);
    #[cfg(not(unix))]
    let directory_mode = None;
    let files = [
        directory.join(binary::LINK),
        directory.join(format!(
            "agentscm-{}{}",
            binary::VERSION,
            std::env::consts::EXE_SUFFIX
        )),
    ]
    .into_iter()
    .map(|path| {
        let metadata = path.symlink_metadata().ok();
        let target = metadata
            .as_ref()
            .filter(|metadata| metadata.file_type().is_symlink())
            .and_then(|_| std::fs::read_link(&path).ok());
        #[cfg(unix)]
        let mode = path
            .metadata()
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777);
        #[cfg(not(unix))]
        let mode = None;
        let digest = path
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|_| file_hash(&path));
        (path, digest, target, mode)
    })
    .collect();
    (directory_mode, files)
}

/// One line on what setup will cover, before anything happens.
fn status(cx: &Cx<'_>) {
    if let Some(url) = &cx.args.hub_url {
        println!("Joining the Hub at {}.", clean(url));
    }
    #[cfg(unix)]
    if !cx.args.no_agents {
        let found = &cx.found.agents;
        if found.is_empty() {
            println!("No coding agent found on this computer.");
        } else {
            println!("Found {}.", labels(found));
        }
    }
}

/// `Claude Code, Codex and Cursor`.
#[cfg(unix)]
fn labels(agents: &[crate::client_setup::Agent]) -> String {
    let names: Vec<&str> = agents.iter().map(|agent| agent.label()).collect();
    match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Which hub to use, or `None` to go without. Starting a local hub that fails falls
/// back to the other two choices.
async fn choose_hub(cx: &mut Cx<'_>) -> Option<String> {
    let current = checkout_connection(&cx.found).map(|connection| connection.hub);
    let mut choice = cx.args.hub;
    if cx.args.local {
        choice = Some(HubChoice::Local);
    } else if cx.args.hosted {
        choice = Some(HubChoice::Hosted);
    } else if choice.is_none() && cx.args.hub_url.is_some() {
        choice = Some(HubChoice::Existing);
    }
    if choice.is_none() && cx.args.slack_config_file.is_some() {
        choice = Some(HubChoice::Local);
    }
    if choice.is_none() {
        // A Hub this checkout or this computer already uses needs no question.
        let signed_in = || match hub_cli::sign_in_summaries().as_slice() {
            [only] => Some(only.hub.clone()),
            _ => None,
        };
        if let Some(url) = current.clone().or_else(signed_in) {
            println!("Using the Hub at {url}; pass --hub-url or --local to choose another.");
            return Some(url);
        }
        choice = Some(if cx.ui.yes {
            HubChoice::Skip
        } else {
            menu(cx, true)
        });
    }
    loop {
        match choice? {
            HubChoice::Skip => {
                println!("Skipping live warnings. Run quickstart again any time to add them.\n");
                return None;
            }
            HubChoice::Existing => return existing_hub(cx, current),
            HubChoice::Hosted => {
                println!("{HOSTED_GUIDE}\n");
                let url = match &cx.args.hub_url {
                    Some(url) => url.clone(),
                    None => cx.ui.ask("Hub URL (leave empty to skip for now)", ""),
                };
                if url.is_empty() {
                    println!(
                        "Skipping live warnings. Run quickstart again with --hub-url URL once the hub runs.\n"
                    );
                    return None;
                }
                return checked_hub_url(&url);
            }
            HubChoice::Local => match local_hub(cx).await {
                Ok(url) => return url,
                Err(message) => {
                    println!("{message}");
                    // Both runtimes' busy-port errors say "already in use".
                    if message.contains("already in use")
                        && let Some(port) = free_port()
                    {
                        println!(
                            "To use a free port, run: {} quickstart --local --hub-port {port}",
                            cli_command(cx)
                        );
                    }
                    cx.live.hub_error = Some(message.clone());
                    if cx.ui.yes {
                        cx.failure = Some(message);
                        return None;
                    }
                    choice = Some(menu(cx, false));
                }
            },
        }
    }
}

fn menu(cx: &Cx<'_>, with_local: bool) -> HubChoice {
    println!("{HUB_EXPLAINED}");
    let github = cx
        .found
        .remote
        .as_ref()
        .is_none_or(|r| r.forge == Forge::Github);
    let mut options = vec![
        "Hosted: a shared hub on a server for a team (unsupported in the alpha; your agent follows a guide)",
        "Skip live warnings for now",
    ];
    let mut choices = vec![HubChoice::Hosted, HubChoice::Skip];
    let mut default = 1;
    if with_local {
        options.insert(
            0,
            "Run it on this machine (recommended for the alpha; GitHub only)",
        );
        choices.insert(0, HubChoice::Local);
        default = if (native_hub_supported() || cx.found.docker) && github {
            0
        } else {
            2
        };
    }
    let skip = choices.iter().position(|c| matches!(c, HubChoice::Skip));
    choices[cx.ui.choose(
        "Where should AgentSCM run?",
        &options,
        default,
        skip.unwrap_or(default),
    )]
}

fn existing_hub(cx: &Cx<'_>, configured: Option<String>) -> Option<String> {
    let url = match (&cx.args.hub_url, configured) {
        (Some(url), _) => url.clone(),
        (None, Some(url)) if cx.ui.yes => url,
        _ if cx.ui.yes => {
            println!("--hub existing needs --hub-url; skipping live warnings.");
            return None;
        }
        (None, configured) => cx.ui.ask("Hub URL", &configured.unwrap_or_default()),
    };
    checked_hub_url(&url)
}

fn checked_hub_url(url: &str) -> Option<String> {
    match check_base_url(url) {
        Ok(url) => Some(url),
        Err(_) => {
            println!(
                "That hub URL isn't usable: it must be https://host, or http://127.0.0.1:PORT for a hub on this machine (not localhost), with no credentials."
            );
            None
        }
    }
}

fn valid_hub_project(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .ok()
}

fn native_hub_supported() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64")) && detect::which("launchctl").is_some()
}

async fn local_hub(cx: &mut Cx<'_>) -> std::result::Result<Option<String>, String> {
    if let Some(remote) = &cx.found.remote
        && remote.forge != Forge::Github
    {
        return Err("The local hub's first-run setup is GitHub-only today; connect to an existing hub for this forge.".into());
    }
    let recorded_runtime = if cx.manifest.native_hub.is_some() {
        Some(HubRuntime::Native)
    } else if cx.manifest.local_hub.is_some() {
        Some(HubRuntime::Docker)
    } else {
        None
    };
    let runtime = match cx.args.hub_runtime.or(recorded_runtime) {
        Some(runtime) => runtime,
        None if native_hub_supported() => HubRuntime::Native,
        None if cx.found.docker => {
            println!("A managed native hub is not supported on this host. Docker is available as an explicit fallback.");
            if cx.ui.yes { return Err("Choose Docker explicitly with --hub-runtime docker, or use --hub existing/skip.".into()); }
            if !cx.ui.confirm("Use the Docker container fallback?", false) { return Ok(None); }
            HubRuntime::Docker
        }
        None => return Err("No supported managed native hub or working Docker Engine was found. Connect to an existing hub, or use --hub skip for local read-only commands.".into()),
    };
    if recorded_runtime.is_some_and(|previous| previous != runtime) {
        return Err("A local hub already uses another runtime. Uninstall that recorded hub before switching runtimes; no second authority was started.".into());
    }
    let (recorded_port, recorded_project) = if let Some(r) = &cx.manifest.native_hub {
        (Some(r.port), Some(r.project.clone()))
    } else if let Some(r) = &cx.manifest.local_hub {
        (Some(r.port), Some(r.project.clone()))
    } else {
        (None, None)
    };
    let port = cx.args.hub_port.or(recorded_port).unwrap_or(7332);
    let project = cx
        .args
        .hub_project
        .clone()
        .or(recorded_project.clone())
        .unwrap_or_else(|| {
            format!(
                "agentscm-hub-{}",
                &sha_hex(cx.home.as_os_str().as_encoded_bytes())[..10]
            )
        });
    if !valid_hub_project(&project) {
        return Err("--hub-project must start with a lowercase letter and contain only lowercase letters, digits and hyphens (at most 63 characters).".into());
    }
    if recorded_port.is_some_and(|old| old != port) {
        return Err(
            "The recorded hub uses another port. Uninstall it before changing --hub-port.".into(),
        );
    }
    if recorded_project.as_ref().is_some_and(|old| old != &project) {
        return Err(
            "The recorded hub uses another project. Uninstall it before changing --hub-project."
                .into(),
        );
    }
    let repo = cx.found.remote.as_ref().map(|r| r.repo.clone());
    let started = match runtime {
        HubRuntime::Docker => {
            if !cx.found.docker {
                return Err("Docker fallback needs a running Linux Docker Engine and Docker Compose. Start Docker, connect to an existing hub, or use --hub skip.".into());
            }
            println!(
                "Local hub runtime: Docker (explicit container fallback). No automatic emulation or runtime switching."
            );
            let slack = slack_bundle(
                cx.args.slack_config_file.as_deref(),
                Path::new("/data/secrets/slack"),
            )?;
            cx.manifest.bin_parent_created |= !cx.home.join(".agentscm").exists();
            hub_local::start(
                cx.ui,
                cx.args.github_login.as_deref(),
                cx.tty,
                &cx.home,
                repo.as_deref(),
                &mut cx.manifest,
                &hub_local::Options {
                    runtime,
                    port,
                    project: project.clone(),
                    slack,
                },
            )
            .await?
        }
        HubRuntime::Native => {
            if !native_hub_supported() {
                return Err("A managed native hub is currently qualified only for macOS ARM64 with launchctl. Choose --hub-runtime docker explicitly for the container fallback.".into());
            }
            if cx.manifest.native_hub.is_none() {
                hub_native::preflight_port(port)?;
            }
            let executable = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .canonicalize()
                .map_err(|e| e.to_string())?;
            let dir = binary::bin_dir(&cx.home);
            let slack = slack_bundle(
                cx.args.slack_config_file.as_deref(),
                &cx.home
                    .join(".agentscm")
                    .join("hub-native")
                    .join(&project)
                    .join("data")
                    .join("secrets")
                    .join("slack"),
            )?;
            println!(
                "Local hub runtime: native macOS ARM64, managed by launchd. Will retain the verified executable in {}.",
                shown(&dir, &cx.home)
            );
            println!(
                "Will write the LaunchAgent in ~/Library/LaunchAgents and private Hub configuration/data in ~/.agentscm/hub-native/{project}, bound to 127.0.0.1:{port}."
            );
            let stable = binary::install(&dir, &executable, binary::VERSION)?;
            cx.manifest.bin_dir = Some(dir);
            cx.manifest.save().map_err(|e| e.to_string())?;
            hub_native::start(
                cx.ui,
                cx.args.github_login.as_deref(),
                cx.tty,
                &cx.home,
                repo.as_deref(),
                &mut cx.manifest,
                &hub_native::Options {
                    executable: stable,
                    port,
                    project: project.clone(),
                    slack,
                },
            )
            .await?
        }
    };
    cx.install_url = started.install_url;
    if started.pending {
        let kind = if runtime == HubRuntime::Native {
            "native"
        } else {
            "docker"
        };
        cx.live.pending_local = Some(format!(
            "{} quickstart --hub local --hub-runtime {kind} --hub-port {port} --hub-project {project}",
            cli_command(cx)
        ));
        return Ok(None);
    }
    Ok(Some(started.url))
}

// ---- coding agents ----

/// Which `agentscm` the hooks would call, and the running program's canonical path.
fn program_plan(home: &Path) -> Option<(binary::Program, PathBuf)> {
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .ok()?;
    let on_path = detect::which("agentscm").and_then(|p| p.canonicalize().ok());
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    Some((binary::plan(home, &exe, on_path.as_deref(), &temp), exe))
}

/// This checkout's `client setup` connection, if it has one.
fn checkout_connection(found: &Detected) -> Option<crate::client_config::Connection> {
    let root = found.repo_root.as_ref()?;
    let root = crate::client_config::canonical_root(root).ok()?;
    crate::client_config::load()
        .ok()?
        .into_iter()
        .find(|connection| connection.root == root)
}

/// The program the agents' hooks and MCP entries run. They must keep working after quickstart
/// exits, so a program in a cache or temp directory is first copied to `~/.agentscm/bin`.
#[cfg(unix)]
fn stable_program(
    cx: &mut Cx<'_>,
    plan: binary::Program,
    exe: PathBuf,
) -> std::result::Result<PathBuf, String> {
    let dir = binary::bin_dir(&cx.home);
    let parent_new = dir.parent().is_some_and(|parent| !parent.exists());
    let recorded = !matches!(plan, binary::Program::Bare);
    let program = place_program(&cx.home, plan, exe)?;
    if recorded {
        cx.manifest.bin_parent_created |= parent_new;
        cx.manifest.bin_dir = Some(dir);
    }
    Ok(program)
}

#[cfg(unix)]
fn place_program(
    home: &Path,
    plan: binary::Program,
    exe: PathBuf,
) -> std::result::Result<PathBuf, String> {
    match plan {
        binary::Program::Bare => Ok(exe),
        binary::Program::InstallDir(link) => Ok(link),
        binary::Program::Copy => {
            let dir = binary::bin_dir(home);
            binary::install(&dir, &exe, binary::VERSION)?;
            Ok(dir.join(binary::LINK))
        }
    }
}

/// The program `client setup` points the agents at: `agentscm` on PATH when that is this
/// program, else its copy in `~/.agentscm/bin`, so a build directory or npm cache can go away.
#[cfg(unix)]
pub(crate) fn setup_program() -> Result<PathBuf> {
    let home = home_dir()?;
    let (plan, exe) =
        program_plan(&home).ok_or_else(|| invalid("can't tell where this program is"))?;
    place_program(&home, plan, exe)
        .map_err(|why| invalid(&format!("couldn't copy the program: {why}")))
}

#[cfg(unix)]
fn chosen_agents(cx: &Cx<'_>) -> Vec<crate::client_setup::Agent> {
    if cx.args.no_agents {
        Vec::new()
    } else if !cx.args.agent.is_empty() {
        cx.args.agent.clone()
    } else {
        cx.found.agents.clone()
    }
}

#[cfg(not(unix))]
async fn agents_stage(_cx: &mut Cx<'_>) {
    println!("Setting up coding agents isn't available on Windows yet.");
}

/// Sets up every chosen agent for all projects, asking nothing: signs in when needed, turns
/// auto-connect on, and inside a checkout connects it with `client setup`, the path that gives
/// the hub's Connect agents page its proof. Outside one, checkouts connect when an agent starts.
#[cfg(unix)]
async fn agents_stage(cx: &mut Cx<'_>) {
    use crate::client_setup::{Agent, Scope};
    let agents = chosen_agents(cx);
    if agents.is_empty() {
        return;
    }
    cx.live.wanted_agents = true;
    let names = labels(&agents);
    let (Some(hub), true) = (cx.live.hub.clone(), cx.live.hub_up) else {
        println!("Not setting up {names}: agents need a hub that answers.");
        return;
    };
    let root = cx
        .found
        .repo_root
        .as_deref()
        .and_then(|root| crate::client_config::canonical_root(root).ok());
    let signed_in = hub_cli::has_session(Some(&hub)).unwrap_or(false);
    if cx.args.agent_token_file.is_none() && !signed_in {
        let provider = cx
            .args
            .provider
            .or(match cx.found.remote.as_ref().map(|r| r.forge) {
                Some(Forge::Github) => Some(Provider::Github),
                Some(Forge::Gitlab) => Some(Provider::Gitlab),
                _ => None,
            });
        if let Err(error) = hub_cli::sign_in_for_setup(&hub, provider).await {
            let why = short(&error);
            println!("Couldn't sign in to {hub}: {why}.");
            if !cx.tty || cx.ui.yes {
                println!(
                    "Run this once in your own terminal: {} quickstart --hub-url {hub}",
                    cli_command(cx)
                );
            }
            cx.live.agent_error = Some(format!("couldn't sign in: {why}"));
            return;
        }
        cx.manifest.session = Some(SessionRecord { hub: hub.clone() });
    }
    // An older sign-in gets this computer's credential, which auto-connect mints through.
    crate::client_cli::enroll(&hub).await;
    // Auto-connect is how checkouts other than this one connect, so it is always on. It needs a
    // sign-in, which a run with only --agent-token-file doesn't have.
    if hub_cli::has_session(Some(&hub)).unwrap_or(false)
        && let Err(error) = hub_cli::set_auto_connect(Some(&hub), true)
    {
        println!("Couldn't turn on auto-connect: {}.", short(&error));
    }
    let Some((plan, exe)) = program_plan(&cx.home) else {
        println!("I can't tell where this program is, so I didn't set up {names}.");
        return;
    };
    let program = match stable_program(cx, plan, exe) {
        Ok(program) => program,
        Err(why) => {
            println!("Couldn't copy the program: {why}. I didn't set up {names}.");
            cx.live.agent_error = Some(format!("couldn't copy the program: {why}"));
            return;
        }
    };
    let _ = cx.manifest.save();
    println!("Setting up {names} for all your projects…");
    if let Some(root) = &root {
        connect_checkout(cx, &agents, &hub, root.clone(), &program).await;
    }
    for agent in &agents {
        if cx.live.agents.iter().any(|name| name == agent.name()) {
            cx.live.installed.push(*agent);
            continue;
        }
        // Inside a checkout its own files are checked too, so no hook runs twice there.
        let here = root.as_deref().unwrap_or(&cx.home);
        // Uninstall takes out only what this run added, never a setup that was already there.
        let existed = crate::client_setup::user_scope_installed(*agent, here);
        let installed = match &root {
            Some(root) => crate::client_setup::install_quiet(*agent, root, &program, Scope::User),
            None => crate::client_setup::install_user(*agent, &program),
        };
        match installed {
            Ok(()) => {
                cx.live.installed.push(*agent);
                if !existed && !cx.manifest.agents.iter().any(|name| name == agent.name()) {
                    cx.manifest.agents.push(agent.name().to_owned());
                }
            }
            Err(error) => cx.live.failed.push((*agent, short(&error))),
        }
    }
    if cx.live.installed.contains(&Agent::Claude) {
        match crate::client_plugin::allow_claude_tools() {
            Ok(added) => cx.manifest.claude_allow |= added,
            Err(error) => println!(
                "Claude Code will ask before each AgentSCM tool: {}.",
                short(&error)
            ),
        }
    }
    let _ = cx.manifest.save();
    // Some agents set up is a success; the summary names each one that failed and why.
    if cx.live.installed.is_empty() && !cx.live.failed.is_empty() {
        cx.live.agent_error = Some("no agent could be set up".into());
    }
    #[cfg(target_os = "macos")]
    if let Some(dir) = &cx.manifest.bin_dir
        && let Err(error) = launcher::ensure(&launcher::app(&cx.home), &dir.join(binary::LINK))
    {
        println!("Didn't add the AgentSCM app launcher: {error}.");
    }
}

/// Connects the checkout quickstart runs in and sets its agents up there. A checkout the Hub
/// doesn't have yet is only reported: the agents are still set up for all projects.
#[cfg(unix)]
async fn connect_checkout(
    cx: &mut Cx<'_>,
    agents: &[crate::client_setup::Agent],
    hub: &str,
    root: PathBuf,
    program: &Path,
) {
    // Quickstart owns this checkout's connection: an unchanged, valid one is kept as it is,
    // and anything else (another hub, another token file) is replaced.
    let existing = checkout_connection(&cx.found);
    let args = crate::client_cli::ConnectArgs {
        hub: Some(hub.to_owned()),
        token_file: cx.args.agent_token_file.clone(),
        provider: cx.args.provider,
        root: Some(root.clone()),
        replace: existing.is_some(),
        // A saved sign-in the Hub no longer accepts is renewed by signing in again.
        no_sign_in: false,
        ..Default::default()
    };
    let wait = crate::client_cli::AppWait {
        ui: cx.ui,
        install_url: cx.install_url.clone(),
    };
    let scoped: Vec<_> = agents
        .iter()
        .map(|agent| (*agent, crate::client_setup::Scope::User))
        .collect();
    let result = crate::client_cli::setup(&scoped, args, root, program, Some(&wait)).await;
    // Uninstall undoes only a connection this run made (also when a later step failed), not one
    // it found and kept, such as a manual `client setup`.
    if let Some(connection) = checkout_connection(&cx.found) {
        let created = existing.as_ref().is_none_or(|before| {
            before.hub != connection.hub || before.token_file != connection.token_file
        });
        if created && !cx.manifest.clients.contains(&connection.root) {
            cx.manifest.clients.push(connection.root.clone());
        }
        cx.live.repo = Some(connection.repo.clone());
        cx.live.agents = connection.agents.clone();
    }
    if let Err(error) = result {
        println!("This checkout isn't connected yet: {}.", short(&error));
        cx.live.agents.clear();
    }
}

/// With `--init` only, writes `agentscm init-repo`'s instruction files: setup itself never writes
/// to the repository. Failure is reported, not fatal.
fn init_stage(cx: &mut Cx<'_>) {
    if !cx.args.init {
        return;
    }
    let Some(root) = cx.found.repo_root.clone() else {
        println!("--init needs a Git repository, so I didn't add the agent instructions.");
        return;
    };
    match crate::init_cli::run_in(&root, None, cx.ui) {
        Ok(outcome) => cx.init_written = outcome.written,
        Err(error) => println!("Couldn't add the agent instructions: {}.", short(&error)),
    }
    if !cx.init_written.is_empty() {
        crate::init_cli::print_next_steps(cx.live.hub.as_deref(), &cx.init_written);
    }
}

fn cli_command(cx: &Cx<'_>) -> String {
    if let Some(dir) = &cx.manifest.bin_dir {
        return binary::quote(&dir.join(binary::LINK));
    }
    // A program in a cache or temp directory is gone by the time anyone runs the hint.
    match program_plan(&cx.home) {
        Some((binary::Program::Bare, _)) => "agentscm".to_owned(),
        Some((binary::Program::InstallDir(link), _)) => binary::quote(&link),
        Some((binary::Program::Copy, exe))
            if !binary::is_cache_path(
                &exe,
                &std::env::temp_dir().canonicalize().unwrap_or_default(),
            ) =>
        {
            binary::quote(&exe)
        }
        _ => "npx agentscm".to_owned(),
    }
}

fn launch_tray(cx: &Cx<'_>) -> Result<()> {
    let program = match &cx.manifest.bin_dir {
        Some(dir) => dir.join(binary::LINK),
        None => std::env::current_exe()?,
    };
    let status = std::process::Command::new(&program)
        .args(["--no-telemetry", "tray"])
        .status()?;
    if status.success() {
        println!("Opened the AgentSCM status-bar companion.");
        Ok(())
    } else {
        Err(invalid(&format!(
            "could not open the AgentSCM status-bar companion (exit status {status})"
        )))
    }
}

/// The closing lines: what is set up, what the person does next, and how to undo it.
fn summary(cx: &Cx<'_>, changed: bool, command: &str) {
    if !changed {
        println!("\nNo changes made.");
    }
    #[cfg(unix)]
    if !cx.live.installed.is_empty() {
        let hub = cx.live.hub.as_deref().unwrap_or_default();
        println!(
            "\nDone: {} set up for all your projects with {hub}.",
            labels(&cx.live.installed)
        );
        println!(
            "Fully quit and reopen your agents, then ask one in a repository: \"Get started with AgentSCM\"."
        );
        if cx
            .live
            .installed
            .contains(&crate::client_setup::Agent::Codex)
        {
            println!("Codex: run /hooks once and trust the AgentSCM hooks.");
        }
    }
    #[cfg(unix)]
    for (agent, why) in &cx.live.failed {
        println!(
            "{} wasn't set up: {why}. Fix that, then run `{command} quickstart` again.",
            agent.label()
        );
    }
    if !cx.init_written.is_empty() {
        println!(
            "Repository files to review and commit (`uninstall` leaves them): {}",
            cx.init_written
                .iter()
                .map(|file| file.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !cx.manifest.is_empty() {
        println!("Undo: {command} uninstall");
    }
}

// ---- uninstall ----

pub async fn uninstall(args: UninstallArgs) -> Result<()> {
    let ui = Ui { yes: args.yes };
    let _quickstart_lease = manifest::acquire_lease()?;
    let Some(mut m) = Manifest::load()? else {
        println!("Nothing to undo: nothing was installed by quickstart.");
        return Ok(());
    };
    if !args.yes && !std::io::stdin().is_terminal() {
        return Err(invalid(
            "agentscm uninstall asks before it removes anything, and there is no terminal here. Pass --yes (and --remove-hub-data to delete the local hub's data).",
        ));
    }
    let home = home_dir()?;
    let programs = m
        .bin_dir
        .as_ref()
        .map(|d| (d.clone(), binary::installed(d)));
    println!("Will undo what quickstart did:");
    for root in &m.clients {
        println!(
            "  - disconnect {} with `client remove`: take out its agents' hooks, MCP entries and skills, and delete and revoke the agent token setup created",
            root.display()
        );
    }
    if !m.agents.is_empty() {
        println!(
            "  - take AgentSCM out of {} for all your projects",
            m.agents.join(", ")
        );
    }
    if m.claude_allow {
        println!("  - take the rule allowing AgentSCM's tools out of Claude Code's settings");
    }
    if m.session.is_some() {
        println!("  - sign out of the hub (end the sign-in quickstart created)");
    }
    if let Some((dir, files)) = &programs {
        let names: Vec<String> = files
            .iter()
            .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        println!(
            "  - remove the program files in {}: {}",
            shown(dir, &home),
            if names.is_empty() {
                "none found".into()
            } else {
                names.join(", ")
            }
        );
        #[cfg(target_os = "macos")]
        println!("  - remove the owned AgentSCM tray login setting when removing its program");
    }
    if let Some(l) = &m.local_hub {
        println!(
            "  - stop the local hub (docker compose project {}); its data stays unless you agree below",
            l.project
        );
    }
    if let Some(native) = &m.native_hub {
        println!(
            "  - stop the native hub LaunchAgent {} and remove its recorded service configuration; data stays unless selected below",
            native.label
        );
    }
    println!("  - delete quickstart's own record, if everything above could be undone");
    let remove_hub_data = (m.local_hub.is_some() || m.native_hub.is_some())
        && (args.remove_hub_data
            || (!args.yes && ui.confirm("Also delete the recorded local hub data (native directory or Docker volume)? This can't be undone.", false)));
    if m.local_hub.is_some() || m.native_hub.is_some() {
        println!(
            "Local hub data: {}.",
            if remove_hub_data {
                "will be deleted"
            } else {
                "will be kept for reinstall"
            }
        );
        println!(
            "The GitHub App and its installations stay on GitHub. Remove them in GitHub Settings if you no longer need them; keeping hub data lets reinstall reuse the app."
        );
    }
    if !ui.confirm("Go ahead?", true) {
        println!("Nothing changed.");
        return Ok(());
    }
    // What could not be undone stays in the record, with the command to finish by hand.
    let mut left = Manifest {
        created_at: m.created_at,
        tray_declined: m.tray_declined,
        ..Manifest::default()
    };
    let mut manual: Vec<String> = Vec::new();
    for root in &m.clients {
        if let Err(error) = crate::client_cli::remove_checkout(root).await {
            println!("Couldn't disconnect {}: {}.", root.display(), short(&error));
            manual.push(format!(
                "agentscm client remove --root {}",
                binary::quote(root)
            ));
            left.clients.push(root.clone());
        }
    }
    #[cfg(unix)]
    for name in &m.agents {
        let agent = <crate::client_setup::Agent as ValueEnum>::from_str(name, false);
        if let Err(error) = agent
            .map_err(|_| invalid("unknown agent"))
            .and_then(crate::client_setup::remove_user)
        {
            println!("Couldn't take AgentSCM out of {name}: {}.", short(&error));
            manual.push(format!(
                "run `agentscm uninstall` again to take AgentSCM out of {name}"
            ));
            left.agents.push(name.clone());
        }
    }
    #[cfg(unix)]
    if m.claude_allow
        && let Err(error) = crate::client_plugin::disallow_claude_tools()
    {
        println!(
            "Couldn't take {} out of Claude Code's settings: {}.",
            crate::client_plugin::CLAUDE_ALLOW,
            short(&error)
        );
        left.claude_allow = true;
    }
    let _client_lifecycle = match crate::client_cli::lifecycle_lock() {
        Ok(lease) => lease,
        Err(error) => {
            println!(
                "Couldn't finish client and program cleanup: {}.",
                short(&error)
            );
            manual.push("run `agentscm uninstall` again after client setup finishes".into());
            left.session = m.session.take();
            left.local_hub = m.local_hub.take();
            left.native_hub = m.native_hub.take();
            left.bin_dir = programs.as_ref().map(|(dir, _)| dir.clone());
            left.bin_parent_created = m.bin_parent_created;
            left.save()?;
            println!(
                "\nSome things were left in place, and quickstart.json still lists them. To finish by hand:"
            );
            for command in &manual {
                println!("  {command}");
            }
            return Err(invalid(
                "uninstall did not finish; see the list above (quickstart.json keeps what is left)",
            ));
        }
    };
    // Ending the sign-in comes after `client remove`, which revokes tokens with it.
    if let Some(session) = &m.session {
        match hub_cli::end_session(&session.hub).await {
            Ok(true) => println!("Signed out of {}.", clean(&session.hub)),
            Ok(false) => println!(
                "The saved sign-in isn't for {} any more; left it alone.",
                clean(&session.hub)
            ),
            Err(e) => {
                println!("Couldn't sign out: {}.", short(&e));
                manual.push("agentscm hub logout".into());
                left.session = Some(session.clone());
            }
        }
    }
    let native_executable_drift = m.native_hub.as_ref().is_some_and(|native| {
        file_hash(&native.executable).as_deref() != Some(native.executable_sha256.as_str())
    });
    if m.native_hub.is_some() {
        match hub_native::uninstall(&mut m, &home, remove_hub_data) {
            Ok(()) => println!(
                "Stopped the native hub and removed its managed LaunchAgent. Hub data {}.",
                if remove_hub_data {
                    "was deleted"
                } else {
                    "was retained for reinstall"
                }
            ),
            Err(message) => {
                println!("Couldn't uninstall the native hub: {message}.");
                manual.push(message);
                left.native_hub = m.native_hub.clone();
            }
        }
    }
    let retain_program = !crate::client_config::load()
        .is_ok_and(|connections| connections.is_empty())
        || left.native_hub.is_some()
        || native_executable_drift;
    #[cfg(target_os = "macos")]
    if programs.is_some()
        && !retain_program
        && let Err(error) = launcher::remove(&launcher::app(&home))
    {
        println!("Couldn't remove the AgentSCM app launcher: {error}.");
    }
    let tray_cleanup_failed = if programs.is_some() && !retain_program {
        match crate::tray::remove_login_setting_locked() {
            Ok(()) => false,
            Err(error) => {
                println!("Couldn't remove the tray login setting: {}.", short(&error));
                manual.push(
                    "review the AgentSCM tray login setting, then run `agentscm uninstall` again"
                        .into(),
                );
                true
            }
        }
    } else {
        false
    };
    if let Some((dir, _)) = programs.as_ref().filter(|_| {
        // Connections quickstart didn't make may run this program too.
        retain_program || tray_cleanup_failed
    }) {
        println!(
            "Left the program files in {}: client connections or a managed hub still need them, or the native executable changed.",
            shown(dir, &home)
        );
        manual.push(
            "after removing the remaining client connections or hub, run `agentscm uninstall` again".into(),
        );
        left.bin_dir = Some(dir.clone());
    } else if let Some((dir, files)) = programs {
        let mut failed = false;
        for file in &files {
            if let Err(e) = std::fs::remove_file(file) {
                println!("Couldn't remove {}: {e}.", file.display());
                failed = true;
            }
        }
        // Only an empty directory goes.
        let _ = std::fs::remove_dir(&dir);
        if !failed && m.bin_parent_created {
            let _ = dir.parent().map(std::fs::remove_dir);
        }
        if failed {
            manual.push(format!(
                "rm {}/agentscm {}/agentscm-*",
                dir.display(),
                dir.display()
            ));
            left.bin_dir = Some(dir);
        } else {
            println!("Removed the program files in {}.", shown(&dir, &home));
            #[cfg(target_os = "macos")]
            println!(
                "Tray login is disabled. If its menu is still open, choose Quit AgentSCM Tray."
            );
        }
    }
    drop(_client_lifecycle);
    if let Some(local) = &m.local_hub
        && let Err(command) = undo_local_hub(remove_hub_data, local, &home)
    {
        manual.push(command);
        left.local_hub = Some(local.clone());
    }
    if m.bin_parent_created && left.bin_dir.is_none() && left.local_hub.is_none() {
        remove_created_root(&home);
    }
    if left.is_empty() {
        let _ = std::fs::remove_file(manifest::path()?);
        println!("Removed quickstart's record.");
        return Ok(());
    }
    left.save()?;
    println!(
        "\nSome things were left in place, and quickstart.json still lists them. To finish by hand:"
    );
    for command in &manual {
        println!("  {command}");
    }
    Err(invalid(
        "uninstall did not finish; see the list above (quickstart.json keeps what is left)",
    ))
}

/// Drops `~/.agentscm` (and an emptied `hubs`) that quickstart created, only if nothing is left.
fn remove_created_root(home: &Path) {
    let root = home.join(".agentscm");
    let _ = std::fs::remove_dir(root.join("hubs"));
    let _ = std::fs::remove_dir(root);
}

/// `Err` carries the command to finish by hand.
fn undo_local_hub(
    wipe: bool,
    local: &manifest::LocalHubRecord,
    home: &Path,
) -> std::result::Result<(), String> {
    let compose = format!(
        "docker compose -p {} -f {}/compose.yaml down{}",
        local.project,
        shown(&local.dir, home),
        if wipe { " -v" } else { "" }
    );
    if !hub_local::down(&local.dir, &local.project, wipe) {
        println!("Docker couldn't stop the hub (is Docker running?).");
        return Err(compose);
    }
    if !wipe {
        println!(
            "Stopped the local hub. Its data and {} were kept.",
            shown(&local.dir, home)
        );
    } else if safe_hub_data_dir(&local.dir, home, &local.project)
        && std::fs::remove_dir_all(&local.dir).is_ok()
    {
        println!("Deleted the local hub and its data.");
    } else {
        println!(
            "Stopped the hub and deleted its volume; couldn't remove {}.",
            shown(&local.dir, home)
        );
        return Err(format!("rm -r {}", shown(&local.dir, home)));
    }
    Ok(())
}

fn safe_hub_data_dir(dir: &Path, home: &Path, project: &str) -> bool {
    dir == home.join(".agentscm/hub") || dir == home.join(".agentscm/hubs").join(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_shows_five_flags_and_hidden_ones_still_parse() {
        use clap::{Args, FromArgMatches};
        let command = QuickstartArgs::augment_args(clap::Command::new("quickstart"));
        let mut shown: Vec<&str> = command
            .get_arguments()
            .filter(|arg| !arg.is_hide_set())
            .filter_map(|arg| arg.get_long())
            .collect();
        shown.sort_unstable();
        #[cfg(unix)]
        assert_eq!(shown, ["dry-run", "hub-url", "json", "only", "yes"]);
        #[cfg(not(unix))]
        assert_eq!(shown, ["dry-run", "hub-url", "json", "yes"]);
        // The agent flags exist only on Unix.
        #[cfg(unix)]
        let argv = [
            "quickstart",
            "--local",
            "--agent",
            "codex",
            "--only",
            "claude",
        ];
        #[cfg(not(unix))]
        let argv = ["quickstart", "--local"];
        let matches = command.try_get_matches_from(argv).unwrap();
        let args = QuickstartArgs::from_arg_matches(&matches).unwrap();
        assert!(args.local);
        #[cfg(unix)]
        assert_eq!(args.agent.len(), 2);
    }

    #[test]
    fn created_root_goes_only_when_empty() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".agentscm");
        std::fs::create_dir_all(root.join("hubs")).unwrap();
        std::fs::write(root.join("keep"), "x").unwrap();
        remove_created_root(home.path());
        assert!(root.join("keep").exists() && !root.join("hubs").exists());
        std::fs::remove_file(root.join("keep")).unwrap();
        std::fs::create_dir_all(root.join("hubs")).unwrap();
        remove_created_root(home.path());
        assert!(!root.exists());
    }

    #[test]
    fn pending_local_setup_names_the_resume_command_instead_of_token_setup() {
        let live = Live {
            pending_local: Some("/opt/agentscm quickstart --hub local".into()),
            ..Live::default()
        };
        let line = live.line("agentscm");
        assert!(line.contains("finish creating and installing the app"));
        assert!(line.contains("`/opt/agentscm quickstart --hub local`"));
        assert!(!line.contains("agent-token-file"));
    }

    #[test]
    fn terminal_refusal_lists_the_flags() {
        for flag in [
            "--yes",
            "--local",
            "--hosted",
            "--hub",
            "--hub-url",
            "--agent-token-file",
            "--agent",
            "--no-agents",
            "--slack-config-file",
            "--init",
            "--tray",
        ] {
            assert!(NEEDS_TERMINAL.contains(flag), "{flag}");
        }
    }

    #[test]
    fn hub_takes_a_choice_or_the_hub_url() {
        #[derive(clap::Parser)]
        struct Quick {
            #[command(flatten)]
            args: QuickstartArgs,
        }
        let parse = |extra: &[&str]| {
            let argv = ["quickstart"].iter().chain(extra);
            let mut args = <Quick as clap::Parser>::try_parse_from(argv)?.args;
            Ok::<_, clap::Error>(args.resolve_hub().map(|()| (args.hub, args.hub_url)))
        };
        let url = "https://hub.example.com";
        assert!(matches!(
            parse(&["--hub", url]).unwrap().unwrap(),
            (Some(HubChoice::Existing), Some(given)) if given == url
        ));
        assert!(matches!(
            parse(&["--hub", "Skip"]).unwrap().unwrap(),
            (Some(HubChoice::Skip), None)
        ));
        assert!(parse(&["--hub", url, "--hub-url", url]).unwrap().is_ok());
        assert!(
            parse(&["--hub", url, "--hub-url", "https://other.example.com"])
                .unwrap()
                .is_err()
        );
        assert!(parse(&["--hub", "hosted"]).is_err());
        assert!(parse(&["--hub", "nonsense"]).is_err());
        assert!(parse(&["--local", "--hub", url]).is_err());
    }

    #[test]
    fn slack_input_rejects_nonlocal_hubs_before_reading_the_file() {
        let args = QuickstartArgs {
            yes: true,
            hub_arg: None,
            hub: Some(HubChoice::Existing),
            hub_url: None,
            hub_runtime: None,
            hub_port: None,
            hub_project: None,
            github_login: None,
            agent_token_file: None,
            slack_config_file: Some(std::env::temp_dir().join("does-not-exist")),
            local: false,
            hosted: false,
            provider: None,
            #[cfg(unix)]
            agent: Vec::new(),
            no_agents: true,
            no_collisions: false,
            init: false,
            tray: false,
            dry_run: false,
            json: false,
        };
        let error = preflight_slack(&args).unwrap_err().to_string();
        assert!(error.contains("requires a managed local Hub"));
    }

    #[test]
    fn the_last_line_names_why_a_requested_local_hub_is_not_running() {
        let live = Live {
            hub_error: Some("Docker is not running.".into()),
            ..Live::default()
        };
        let line = live.line("agentscm");
        assert!(
            line.starts_with("Live warnings: off — local hub not started: Docker is not running."),
            "{line}"
        );
        assert!(!line.contains("no hub chosen"), "{line}");
    }

    #[test]
    fn the_last_line_says_why_warnings_are_off() {
        let mut live = Live::default();
        assert!(
            live.line("agentscm")
                .starts_with("Live warnings: off — no hub chosen (")
        );
        live.hub = Some("http://h".into());
        assert_eq!(
            live.line("agentscm"),
            "Live warnings: off — the hub at http://h isn't answering (start it, then run `agentscm quickstart` again)"
        );
        live.hub_up = true;
        assert!(
            live.line("agentscm")
                .contains("no coding agent was set up (run `agentscm quickstart --agent claude|codex|cursor|copilot|gemini`)")
        );
        live.wanted_agents = true;
        assert!(
            live.line("agentscm")
                .contains("this checkout isn't connected to the hub")
        );
        live.repo = Some("acme/widgets".into());
        assert!(
            live.line("agentscm")
                .contains("no coding agent was set up (run `agentscm quickstart --agent claude`)")
        );
        live.agents = vec!["claude".into()];
        assert_eq!(
            live.line("agentscm"),
            "Live warnings: on (hub http://h, repo acme/widgets)"
        );
    }
}
