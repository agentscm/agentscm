//! `agentscm quickstart` and `agentscm uninstall`: guided setup of early collision
//! warnings (and, optionally, live warnings through a hub and coding agents set up by `client setup`).
//! Every write is announced first and recorded in `quickstart.json`, which `uninstall`
//! reads to take back exactly what was done. Nothing here is telemetry.
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(unix)]
use agentscm::git_collisions::{self, Request};
use agentscm::hub::forge_http::{self, check_base_url};
use agentscm::hub::slack::setup as slack_setup;
use agentscm::{Error, Result};
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde_json::Value;

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
    /// Connect to the hub that already runs at this URL.
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
    /// Coding agent to set up (repeatable; default: Claude Code when it is installed).
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
    /// Skip the check for colliding branches.
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
--slack-config-file ABSOLUTE_PATH, --agent claude|codex|cursor|copilot|gemini (repeatable) or --no-agents, --no-collisions, --init, --tray";

const HUB_EXPLAINED: &str =
    "A hub is a small shared server that lets agents on different machines see each other's work.";

const HOSTED_GUIDE: &str = "\
Hosted (a shared hub on a server) is not supported in the alpha: it will likely break and is not \
for production. Quickstart does not set up a server. Hand this guide to your coding agent and work \
through it together:
  https://github.com/counterbranch/agentscm/blob/main/docs/hosted.md
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
    /// This run tried to set up at least one agent.
    wanted_agents: bool,
    /// The user answered no to "Go ahead?", or no to installing for all projects when every
    /// chosen agent needs that.
    declined: bool,
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
        if self.declined {
            return off(
                "you chose not to set up a coding agent; nothing was changed for it",
                &format!("run `{cli} quickstart` again when you want it"),
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
        } else if found.claude {
            vec!["claude"]
        } else {
            Vec::new()
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
    status(&cx).await;
    // Local or hosted is the first question; the collision check does not depend on it.
    if let Some(url) = choose_hub(&mut cx).await {
        cx.live.hub_up = healthy(&cx.client, &url).await;
        cx.live.hub = Some(url.clone());
        if !cx.live.hub_up {
            println!(
                "The hub at {url} isn't answering, so no agent was connected to it. Start it, then run quickstart again."
            );
        }
    }
    collisions_step(&cx);
    agents_stage(&mut cx).await;
    init_stage(&mut cx);
    let command = cli_command(&cx);
    // Without an agent to set up (--no-agents, or none found) nothing is connected on purpose.
    let requested_hub_incomplete = (args.local
        || matches!(args.hub, Some(HubChoice::Local | HubChoice::Existing))
        || args.hub_url.is_some())
        && !cx.live.declined
        && (!cx.live.hub_up || (cx.live.repo.is_none() && cx.live.wanted_agents));
    let ready = cx.failure.is_none()
        && cx.live.pending_local.is_none()
        && cx.live.agent_error.is_none()
        && !requested_hub_incomplete;
    // Asked before the summary, so a remembered answer counts as a change.
    let offered = ready && !args.tray && offers_tray(&cx);
    let tray_result = if ready && args.tray {
        launch_tray(&cx)
    } else if offered {
        tray_offer(&mut cx, &command)
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
    summary(&cx, changed);
    println!("Run AgentSCM commands with: {command}");
    if let Some(dir) = &cx.manifest.bin_dir {
        println!(
            r#"To use `agentscm` in this shell: export PATH={}:"$PATH""#,
            binary::quote(dir)
        );
    }
    if let (Some(url), Some(repo), true) = (&cx.live.hub, &cx.live.repo, cx.live.hub_up) {
        println!("Hub status page: {url}/ui/");
        println!(
            "Sign in there with a read-only token: {command} hub token create --read-only --name status-page --repo {repo} --ttl 30d"
        );
    }
    #[cfg(target_os = "macos")]
    if !(offered || ready && args.tray) {
        println!(
            "Optional status-bar companion: {command} tray (Start at Login is off until you enable it in the menu)."
        );
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
    if cx.live.agent_error.is_some() {
        return Err(invalid(
            "setting up the coding agent failed; see the status and recovery instructions above",
        ));
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

async fn status(cx: &Cx<'_>) {
    println!("What I found:");
    let found = &cx.found;
    println!(
        "  Executable platform: {}/{}; managed native hub {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if native_hub_supported() {
            "available"
        } else {
            "unavailable (explicit Docker fallback or existing hub)"
        }
    );
    match (&found.repo_root, &found.remote) {
        (Some(root), Some(remote)) => {
            let forge = match remote.forge {
                Forge::Github => "GitHub",
                Forge::Gitlab => "GitLab",
                Forge::Other => "another forge",
            };
            println!(
                "  Git repository: {} (origin: {forge}, {})",
                root.display(),
                clean(&remote.repo)
            );
        }
        (Some(root), None) => {
            println!(
                "  Git repository: {} (no usable origin remote)",
                root.display()
            );
        }
        _ => println!("  Git repository: not inside one"),
    }
    match &found.git {
        Some((_, version)) => println!("  git: {}", clean(version)),
        None => println!("  git: not found on PATH"),
    }
    println!(
        "  Docker: {}",
        if found.docker {
            "available (with compose)"
        } else {
            "not available"
        }
    );
    match checkout_connection(found) {
        Some(connection) => {
            let reach = if healthy(&cx.client, &connection.hub).await {
                "answering"
            } else {
                "not answering"
            };
            println!(
                "  Hub: {} ({reach}); this checkout is connected for {}{}",
                connection.hub,
                clean(&connection.repo),
                if connection.agents.is_empty() {
                    String::new()
                } else {
                    format!(" with {}", connection.agents.join(", "))
                }
            );
        }
        None => println!("  Hub: this checkout isn't connected to one"),
    }
    println!(
        "  Claude Code: {}\n",
        if found.claude { "found" } else { "not found" }
    );
}

/// The friendly summary of a collisions report.
fn summarize(report: &Value) -> String {
    let checked = report["items"].as_array().map_or(0, Vec::len);
    let list = report["collisions"].as_array().cloned().unwrap_or_default();
    if checked < 2 && !report["truncated"].is_string() {
        return "No other branches or worktrees to compare yet.".into();
    }
    let conflicts = list.iter().filter(|c| c["level"] == "conflict").count();
    let noun = if checked == 1 {
        "branch/worktree"
    } else {
        "branches/worktrees"
    };
    let mut text = format!(
        "{checked} {noun} checked: {conflicts} would conflict, {} touch the same files.",
        list.len() - conflicts
    );
    for c in list.iter().take(3) {
        let name = |i: usize| clean(c["items"][i].as_str().unwrap_or("?"));
        let paths: Vec<String> = c["shared_paths"]
            .as_array()
            .into_iter()
            .flatten()
            .take(3)
            .filter_map(|p| p.as_str().map(clean))
            .collect();
        let total = c["shared_paths_total"].as_u64().unwrap_or(0) as usize;
        let more = total.saturating_sub(paths.len());
        text.push_str(&format!(
            "\n  {} and {}: {}{}{}",
            name(0),
            name(1),
            paths.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            },
            if c["level"] == "conflict" {
                " (would conflict)"
            } else {
                ""
            }
        ));
    }
    if report["truncated"].is_string() {
        text.push_str("\n  (Stopped early; this is a partial result.)");
    }
    text
}

#[cfg(not(unix))]
fn collisions_step(cx: &Cx<'_>) {
    if !cx.args.no_collisions {
        println!("Skipping the collision check: it isn't available on Windows yet.\n");
    }
}

#[cfg(unix)]
fn collisions_step(cx: &Cx<'_>) {
    if cx.args.no_collisions {
        return;
    }
    let (Some((git, _)), Some(root)) = (&cx.found.git, &cx.found.repo_root) else {
        println!("Skipping the collision check: this isn't inside a Git repository.\n");
        return;
    };
    // Read-only and the reason AgentSCM was installed, so it runs without asking; --no-collisions skips it.
    println!("Checking for branches and worktrees that change the same files…");
    let request = Request {
        git: git.canonicalize().unwrap_or_else(|_| git.clone()),
        repository: root.clone(),
        base: None,
        refs: Vec::new(),
        worktrees: true,
        active_days: Some(14),
        include_merged: false,
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        max_bytes: None,
    };
    match git_collisions::collisions(&request) {
        Ok(report) => println!("{}\n", summarize(&report)),
        Err(error) => println!("Couldn't check for collisions: {}.\n", short(&error)),
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
        if let Some(url) = &current
            && (cx.ui.yes
                || cx.ui.confirm(
                    &format!("This checkout is already connected to a hub ({url}). Keep using it?"),
                    true,
                ))
        {
            return Some(url.clone());
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
            if !cx.ui.confirm(
                "Install the native hub executable and managed user service?",
                true,
            ) {
                return Ok(None);
            }
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
    match plan {
        binary::Program::Bare => Ok(exe),
        binary::Program::InstallDir(link) => {
            cx.manifest.bin_dir = Some(dir);
            Ok(link)
        }
        binary::Program::Copy => {
            let parent_new = dir.parent().is_some_and(|parent| !parent.exists());
            binary::install(&dir, &exe, binary::VERSION)?;
            cx.manifest.bin_parent_created |= parent_new;
            cx.manifest.bin_dir = Some(dir.clone());
            Ok(dir.join(binary::LINK))
        }
    }
}

#[cfg(unix)]
fn chosen_agents(cx: &Cx<'_>) -> Vec<crate::client_setup::Agent> {
    if cx.args.no_agents {
        Vec::new()
    } else if !cx.args.agent.is_empty() {
        cx.args.agent.clone()
    } else if cx.found.claude {
        vec![crate::client_setup::Agent::Claude]
    } else if cx.ui.yes {
        println!("Claude Code wasn't found, so I'm not setting up a coding agent.");
        Vec::new()
    } else {
        use clap::ValueEnum;
        let agents = crate::client_setup::Agent::value_variants();
        let mut options: Vec<&str> = agents.iter().map(|agent| agent.name()).collect();
        options.push("none for now");
        let none = options.len() - 1;
        let pick = cx.ui.choose(
            "Claude Code wasn't found. Which coding agent should I set up?",
            &options,
            none,
            none,
        );
        agents
            .get(pick)
            .map(|agent| vec![*agent])
            .unwrap_or_default()
    }
}

/// Where each agent is set up. For all projects is the default, so agents connect in any repo,
/// but it reaches beyond this checkout, so say so and ask first, once: agents already set up
/// that way are not asked about again. `--yes` answers yes, after the notice. A No sets up
/// Cursor, Copilot and Gemini in this checkout only, and drops Claude Code and Codex, whose
/// hooks come only from a user-scope plugin.
#[cfg(unix)]
fn agent_scopes(
    cx: &Cx<'_>,
    root: &Path,
    agents: &[crate::client_setup::Agent],
) -> Vec<(crate::client_setup::Agent, crate::client_setup::Scope)> {
    use crate::client_setup::{Agent, Scope, user_scope_installed};
    let (installed, new): (Vec<Agent>, Vec<Agent>) = agents
        .iter()
        .partition(|agent| user_scope_installed(**agent, root));
    let user = |agents: &[Agent]| agents.iter().map(|agent| (*agent, Scope::User)).collect();
    if new.is_empty() {
        return user(&installed);
    }
    let names = new
        .iter()
        .map(|agent| agent.label())
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "AgentSCM for {names} is installed for your user account, so it is in all your projects; `{} uninstall` removes it when no other connected checkout uses it.",
        cli_command(cx)
    );
    if cx.ui.confirm(
        &format!(
            "Install AgentSCM for {names} in all your projects? It runs a quick check whenever an agent starts and only contacts a Hub in repos connected to it."
        ),
        true,
    ) {
        return user(agents);
    }
    let mut scopes: Vec<(Agent, Scope)> = user(&installed);
    scopes.extend(
        new.into_iter()
            .filter(|agent| !matches!(agent, Agent::Claude | Agent::Codex))
            .map(|agent| (agent, Scope::Checkout)),
    );
    scopes
}

/// The `client setup` line for every supported agent, for setting one up later.
#[cfg(unix)]
fn print_agent_setup_hints(cx: &Cx<'_>, hub: &str) {
    use clap::ValueEnum;
    println!("To set up a coding agent later, run one of these in this checkout:");
    for agent in crate::client_setup::Agent::value_variants() {
        println!(
            "  {} client setup --agent {} --hub {hub}",
            cli_command(cx),
            agent.name()
        );
    }
}

#[cfg(not(unix))]
async fn agents_stage(_cx: &mut Cx<'_>) {
    println!("Setting up coding agents isn't available on Windows yet.");
}

/// Connects this checkout and sets up its coding agents with `client setup`, the one path that
/// writes agent hooks and MCP configuration and gives the hub's Connect agents page its proof.
#[cfg(unix)]
async fn agents_stage(cx: &mut Cx<'_>) {
    let agents = chosen_agents(cx);
    if agents.is_empty() {
        if let (false, Some(hub), true) = (cx.args.no_agents, &cx.live.hub, cx.live.hub_up) {
            print_agent_setup_hints(cx, hub);
        }
        return;
    }
    cx.live.wanted_agents = true;
    let names = agents
        .iter()
        .map(|agent| agent.name())
        .collect::<Vec<_>>()
        .join(", ");
    let (Some(hub), true) = (cx.live.hub.clone(), cx.live.hub_up) else {
        println!("Not setting up {names}: agents need a hub that answers.");
        return;
    };
    let Some(root) = cx
        .found
        .repo_root
        .as_deref()
        .and_then(|root| crate::client_config::canonical_root(root).ok())
    else {
        println!(
            "Not setting up {names}: run quickstart inside the Git repository the agents work in."
        );
        return;
    };
    let signed_in = hub_cli::has_session(Some(&hub)).unwrap_or(false);
    // With --yes or no terminal nobody can finish a browser or device sign-in.
    let no_sign_in = cx.ui.yes || !cx.tty;
    if cx.args.agent_token_file.is_none() && no_sign_in && !signed_in {
        println!(
            "Not setting up {names}: this run can't sign in to the hub (no terminal, or --yes). Run quickstart at a terminal, or pass --agent-token-file."
        );
        return;
    }
    let Some((plan, exe)) = program_plan(&cx.home) else {
        println!("I can't tell where this program is, so I didn't set up {names}.");
        return;
    };
    // Asked only once nothing else stops the setup, so a No is a real choice.
    let agents = agent_scopes(cx, &root, &agents);
    if agents.is_empty() {
        cx.live.declined = true;
        return;
    }
    let names = agents
        .iter()
        .map(|(agent, _)| agent.name())
        .collect::<Vec<_>>()
        .join(", ");
    // Quickstart owns this checkout's connection: an unchanged, valid one is kept as it is,
    // and anything else (another hub, another token file) is replaced after this notice.
    let existing = checkout_connection(&cx.found);
    let replace = existing.is_some();
    if let Some(saved) = existing.as_ref().filter(|saved| saved.hub != hub) {
        println!(
            "This checkout is connected to another hub ({}); setting it up for {hub} replaces that connection.",
            saved.hub
        );
    }
    println!(
        "Will connect {} to {hub} with {}, then set up {names}: hooks, MCP and the coordination skill. Undo it with `{} uninstall`.",
        root.display(),
        if cx.args.agent_token_file.is_some() {
            "your agent token"
        } else {
            // This computer's Hub credential mints a short-lived token and renews it; a sign-in without one falls back to 30 days.
            "an agent token for this one repository, renewed automatically"
        },
        cli_command(cx)
    );
    if matches!(plan, binary::Program::Copy) {
        println!(
            "Will also copy this program to {0}/agentscm-{1} and point {0}/agentscm at it (0755), so the hooks keep working after this copy goes away.",
            shown(&binary::bin_dir(&cx.home), &cx.home),
            binary::VERSION
        );
    }
    #[cfg(target_os = "macos")]
    println!(
        "On macOS this also adds {}, which reopens the menu bar app.",
        shown(&launcher::app(&cx.home), &cx.home)
    );
    if !cx.ui.confirm("Go ahead?", true) {
        cx.live.declined = true;
        return;
    }
    let program = match stable_program(cx, plan, exe) {
        Ok(program) => program,
        Err(why) => {
            println!("Couldn't copy the program: {why}. I didn't set up {names}.");
            cx.live.agent_error = Some(format!("couldn't copy the program: {why}"));
            return;
        }
    };
    let args = crate::client_cli::ConnectArgs {
        hub: Some(hub.clone()),
        token_file: cx.args.agent_token_file.clone(),
        provider: cx.args.provider,
        root: Some(root.clone()),
        replace,
        no_sign_in,
        ..Default::default()
    };
    let wait = crate::client_cli::AppWait {
        ui: cx.ui,
        install_url: cx.install_url.clone(),
    };
    let result = crate::client_cli::setup(&agents, args, root.clone(), &program, Some(&wait)).await;
    if !signed_in && hub_cli::has_session(Some(&hub)).unwrap_or(false) {
        cx.manifest.session = Some(SessionRecord { hub: hub.clone() });
    }
    // Uninstall undoes only a connection this run made (also when a later step failed), not one
    // it found and kept, such as a manual `client setup`.
    if let Some(connection) = checkout_connection(&cx.found) {
        let created = existing.as_ref().is_none_or(|before| {
            before.hub != connection.hub || before.token_file != connection.token_file
        });
        if created && !cx.manifest.clients.contains(&connection.root) {
            cx.manifest.clients.push(connection.root.clone());
        } else if !cx.manifest.clients.contains(&connection.root) {
            println!(
                "This checkout's connection existed before quickstart, so `uninstall` leaves it; `{} client remove` takes it out.",
                cli_command(cx)
            );
        }
        cx.live.repo = Some(connection.repo.clone());
        cx.live.agents = connection.agents.clone();
    }
    let _ = cx.manifest.save();
    if let Err(error) = result {
        println!("Couldn't finish setting up {names}: {}.", short(&error));
        cx.live.agent_error = Some(short(&error));
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(dir) = &cx.manifest.bin_dir {
        match launcher::ensure(&launcher::app(&cx.home), &dir.join(binary::LINK)) {
            Ok(true) => println!(
                "Added {}: open it from Spotlight or Launchpad to bring back the menu bar app.",
                shown(&launcher::app(&cx.home), &cx.home)
            ),
            Ok(false) => {}
            Err(error) => println!("Didn't add the AgentSCM app launcher: {error}."),
        }
    }
    if cx.found.worktrees {
        println!(
            "Git worktrees of this checkout use its connection; a separate clone needs its own `{} client setup --agent AGENT --hub {hub}`.",
            cli_command(cx)
        );
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
    std::env::current_exe()
        .map(|path| binary::quote(&path))
        .unwrap_or_else(|_| "npx agentscm".to_owned())
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

/// Asked once, on macOS at a terminal, after this checkout's agents were connected from the
/// installed program; `--yes` and scripted runs keep the tray off unless `--tray` is passed.
fn offers_tray(cx: &Cx<'_>) -> bool {
    cfg!(target_os = "macos")
        && cx.tty
        && !cx.ui.yes
        && cx.live.repo.is_some()
        && cx
            .manifest
            .bin_dir
            .as_ref()
            .is_some_and(|dir| dir.join(binary::LINK).exists())
        && !cx.manifest.tray_declined
        && !tray_login_enabled()
}

/// A Yes is remembered by Start at Login itself.
fn tray_login_enabled() -> bool {
    #[cfg(target_os = "macos")]
    return crate::tray::login_enabled();
    #[cfg(not(target_os = "macos"))]
    false
}

fn tray_offer(cx: &mut Cx<'_>, command: &str) -> Result<()> {
    match cx.ui.answer(
        "Keep AgentSCM in the menu bar, and start it when you log in?",
        true,
    ) {
        Some(true) => {
            #[cfg(target_os = "macos")]
            if let Some(dir) = &cx.manifest.bin_dir
                && let Err(error) = crate::tray::set_login_enabled(true, &dir.join(binary::LINK))
            {
                println!(
                    "Couldn't turn on Start at Login: {}. You can turn it on in the menu.",
                    short(&error)
                );
            }
            launch_tray(cx)
        }
        Some(false) => {
            cx.manifest.tray_declined = true;
            cx.manifest.save()?;
            println!("OK. Open it any time with `{command} tray`.");
            Ok(())
        }
        None => Ok(()),
    }
}

fn summary(cx: &Cx<'_>, changed: bool) {
    let m = &cx.manifest;
    let home = &cx.home;
    if !changed {
        println!("\nNo changes made.");
    }
    println!("\nQuickstart's recorded setup:");
    let mut any = false;
    let mut line = |text: String| {
        any = true;
        println!("  {text}");
    };
    if let Some(local) = &m.local_hub {
        line(format!(
            "{}/compose.yaml and .env: the local hub's Docker files (project {})",
            shown(&local.dir, home),
            local.project
        ));
    }
    if let Some(native) = &m.native_hub {
        line(format!(
            "{}: native hub LaunchAgent {} (port {})",
            shown(&native.plist, home),
            native.label,
            native.port
        ));
    }
    if m.session.is_some()
        && let Ok(path) = hub_cli::sessions_path()
    {
        line(format!("{}: your hub sign-in", shown(&path, home)));
    }
    let connections = crate::client_config::load().unwrap_or_default();
    for root in &m.clients {
        match connections
            .iter()
            .find(|connection| &connection.root == root)
        {
            Some(connection) => line(format!(
                "{}: connected to {} for {}{}; token {}",
                root.display(),
                connection.hub,
                clean(&connection.repo),
                if connection.agents.is_empty() {
                    String::new()
                } else {
                    format!(", agents set up: {}", connection.agents.join(", "))
                },
                shown(&connection.token_file, home)
            )),
            None => line(format!("{}: no longer connected", root.display())),
        }
    }
    if let Some(dir) = &m.bin_dir {
        line(format!(
            "{}: the agentscm program{}",
            shown(dir, home),
            if m.clients.is_empty() {
                ""
            } else {
                " the agents' hooks call"
            }
        ));
    }
    if !any && cx.init_written.is_empty() {
        println!("  nothing recorded.");
    } else if !any {
        println!("  nothing that `agentscm uninstall` undoes.");
    } else {
        if let Ok(path) = manifest::path() {
            println!("  {}: a record of the above", shown(&path, home));
        }
        println!("Undo all of this with `{} uninstall`.", cli_command(cx));
    }
    if !cx.init_written.is_empty() {
        println!(
            "\nRepository files to review and commit (not in quickstart's record; `agentscm uninstall` does not remove them):"
        );
        for file in &cx.init_written {
            println!("  {}", file.display());
        }
    }
    println!();
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
    use serde_json::json;

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
        assert_eq!(shown, ["dry-run", "hub-url", "json", "only", "yes"]);
        let mut argv = vec!["quickstart", "--local"];
        // The agent flags exist only on Unix.
        #[cfg(unix)]
        argv.extend(["--agent", "codex", "--only", "claude"]);
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
    fn collisions_summary_reads_plainly() {
        let report = json!({
            "items": [{}, {}, {}, {}],
            "truncated": null,
            "collisions": [
                {"level": "conflict", "items": ["feat-a", "feat-b"], "shared_paths_total": 5, "shared_paths": ["a", "b", "c", "d", "e"]},
                {"level": "same-file", "items": ["feat-a", "worktree:/w"], "shared_paths_total": 1, "shared_paths": ["x"]},
            ],
        });
        assert_eq!(
            summarize(&report),
            "4 branches/worktrees checked: 1 would conflict, 1 touch the same files.\n  feat-a and feat-b: a, b, c and 2 more (would conflict)\n  feat-a and worktree:/w: x"
        );
        assert_eq!(
            summarize(&json!({"items": [{}], "collisions": [], "truncated": "deadline"})),
            "1 branch/worktree checked: 0 would conflict, 0 touch the same files.\n  (Stopped early; this is a partial result.)"
        );
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
            "--no-collisions",
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
    fn declining_the_agent_setup_says_nothing_was_changed_and_suggests_no_flag() {
        let live = Live {
            hub: Some("http://h".into()),
            hub_up: true,
            wanted_agents: true,
            declined: true,
            ..Live::default()
        };
        let line = live.line("agentscm");
        assert!(line.contains("nothing was changed"), "{line}");
        assert!(!line.contains("--agent-token-file"), "{line}");
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
