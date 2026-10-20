//! `agentscm quickstart` and `agentscm uninstall`: guided setup of early collision
//! warnings (and, optionally, live warnings through a hub and Claude Code hooks).
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
use agentscm::hub::store::{unix_now, validate_agent_name};
use agentscm::{Error, Result};
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde_json::{Value, json};

use crate::hub_cli::{
    self, Provider, call, clean, config_dir, load_session, parse_ttl, write_private,
};

mod binary;
pub(crate) mod detect;
mod hook;
mod hub_local;
mod hub_native;
mod hub_setup;
mod manifest;
mod process;

use crate::prompt::Ui;
use detect::{Detected, Forge};
use manifest::{
    Added, HookRecord, HubJsonRecord, Manifest, SessionRecord, TokenRecord, file_hash, sha_hex,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum HubChoice {
    /// Start a managed native hub, or explicitly choose Docker (GitHub only).
    Local,
    /// Connect to a hub that already runs; needs --hub-url.
    Existing,
    /// Only the collision check and hooks; no live warnings yet.
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HubRuntime {
    /// Run the installed native binary as a managed user service (macOS ARM64).
    Native,
    /// Run the matching Linux container image with Docker Compose.
    Docker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HookScope {
    /// Every project: ~/.claude/settings.json.
    User,
    /// This repository only: .claude/settings.local.json.
    Project,
    None,
}

#[derive(Debug, Args)]
pub struct QuickstartArgs {
    /// Answer every question with the recommended choice (needed without a terminal).
    /// The local hub's administrator must then be given with --github-login.
    #[arg(long)]
    yes: bool,
    /// How to get live warnings.
    #[arg(long, value_enum)]
    hub: Option<HubChoice>,
    /// The hub to connect to (with --hub existing).
    #[arg(long)]
    hub_url: Option<String>,
    /// Local hub runtime; native is recommended on qualified macOS ARM64 hosts.
    #[arg(long, value_enum)]
    hub_runtime: Option<HubRuntime>,
    /// Loopback port for the local hub; reuse the recorded port on resume.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    hub_port: Option<u16>,
    /// Instance name for an isolated local service or Docker Compose project.
    #[arg(long)]
    hub_project: Option<String>,
    /// Your GitHub login, to make you the local hub's administrator.
    #[arg(long)]
    github_login: Option<String>,
    /// An agent token you already have; it is checked and used instead of signing in.
    #[arg(long)]
    agent_token_file: Option<PathBuf>,
    /// Name for the agent token (default claude-<hostname>).
    #[arg(long)]
    agent_name: Option<String>,
    /// Lifetime of the agent token, such as 8h or 30d (at most 30d).
    #[arg(long, default_value = "30d")]
    ttl: String,
    /// Which forge to sign in with (default: the one in origin's URL).
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Where to add the Claude Code hooks.
    #[arg(long, value_enum)]
    hook: Option<HookScope>,
    /// Skip the check for colliding branches.
    #[arg(long)]
    no_collisions: bool,
    /// Also write the repository's agent config (agentscm init-repo); with --yes it is
    /// skipped unless this is passed. Loopback hubs never generate repository config.
    #[arg(long)]
    init: bool,
}

impl QuickstartArgs {
    pub(crate) fn noninteractive(&self) -> bool {
        self.yes || !std::io::stdin().is_terminal()
    }

    pub(crate) fn can_start(&self) -> bool {
        self.yes || std::io::stdin().is_terminal()
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
To run it without one, pass --yes and choose with flags: --hub local|existing|skip, --hub-url URL, \
--agent-token-file PATH, --agent-name NAME, --ttl 30d, --provider github|gitlab, --github-login LOGIN, \
--hub-runtime native|docker, --hub-port PORT, --hub-project NAME, \
--hook user|project|none, --no-collisions, --init";

const HUB_EXPLAINED: &str =
    "A hub is a small shared server that lets agents on different machines see each other's work.";

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

// ---- files ----

/// `Ok(None)` only when the file is absent; any other failure to read it (permissions,
/// not UTF-8) is an error, so a file we cannot read is never overwritten.
fn read_existing(path: &Path) -> std::result::Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("I couldn't read {}: {error}", path.display())),
    }
}

/// Where a write really lands: a symlink (a dotfiles setup) is followed, so it stays a link.
fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// Atomic replace of the file `path` resolves to. An existing file keeps its permissions;
/// a new one is 0600.
fn write_keeping_mode(path: &Path, text: &str) -> std::result::Result<(), String> {
    let real = resolve(path);
    #[cfg(unix)]
    let mode = real
        .metadata()
        .ok()
        .map(|m| m.permissions().mode() & 0o7777);
    write_private(&real, text).map_err(|e| short(&e))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let base = format!("{}.agentscm-backup-{}", resolve(path).display(), unix_now());
    let mut candidate = PathBuf::from(&base);
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// A copy of `path` before it is replaced. Failing to make one stops the change.
fn make_backup(path: &Path) -> std::result::Result<PathBuf, String> {
    let backup = backup_path(path);
    std::fs::copy(path, &backup)
        .map_err(|e| format!("I couldn't back up {} first ({e})", path.display()))?;
    Ok(backup)
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

/// `Ok(Some(reply))` for a valid token, `Ok(None)` when the hub rejects it, `Err` when
/// the hub does not answer.
async fn whoami(client: &reqwest::Client, url: &str, token: &str) -> Result<Option<Value>> {
    let url = format!("{url}/v1/hub/whoami");
    match call(client, Method::GET, &url, Some(token), None).await? {
        (200, reply) => Ok(Some(reply)),
        _ => Ok(None),
    }
}

fn hub_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("hub.json"))
}

/// The hub the other commands would use: the environment, else hub.json.
struct HubConfig {
    url: String,
    token_file: PathBuf,
    tokens: Vec<(String, PathBuf)>,
}

impl HubConfig {
    /// The token file the hooks would use in a checkout of `repo`.
    fn file_for(&self, repo: Option<&str>) -> &Path {
        repo.and_then(|repo| {
            self.tokens
                .iter()
                .find(|(r, _)| r.eq_ignore_ascii_case(repo))
        })
        .map_or(&self.token_file, |(_, file)| file)
    }
}

fn current_hub() -> Option<HubConfig> {
    let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (url, file, tokens) = match (var("AGENTSCM_HUB_URL"), var("AGENTSCM_HUB_TOKEN_FILE")) {
        (Some(url), Some(file)) => (url, file, Vec::new()),
        _ => {
            let text = std::fs::read_to_string(hub_json_path().ok()?).ok()?;
            let value: Value = serde_json::from_str(&text).ok()?;
            let tokens = value["tokens"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(repo, file)| Some((repo.clone(), PathBuf::from(file.as_str()?))))
                .collect();
            (
                value["url"].as_str()?.to_owned(),
                value["token_file"].as_str()?.to_owned(),
                tokens,
            )
        }
    };
    Some(HubConfig {
        url: check_base_url(&url).ok()?,
        token_file: PathBuf::from(file),
        tokens,
    })
}

fn read_token(file: &Path) -> Option<String> {
    let token = std::fs::read_to_string(file).ok()?.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

// ---- the flow ----

/// What is true about live warnings when the run ends.
#[derive(Default)]
struct Live {
    hub: Option<String>,
    hub_up: bool,
    /// The repository whose token and hub settings were saved.
    repo: Option<String>,
    hooks: bool,
    pending_local: Option<String>,
}

impl Live {
    /// The last line of every run.
    fn line(&self) -> String {
        let off = |reason: &str, todo: &str| format!("Live warnings: off — {reason} ({todo})");
        if let Some(command) = &self.pending_local {
            return off(
                "the local hub is waiting for GitHub App setup",
                &format!("finish creating and installing the app, then run `{command}`"),
            );
        }
        match (&self.hub, &self.repo) {
            (None, _) => off(
                "no hub chosen",
                "run `agentscm quickstart --hub existing --hub-url URL`, or `--hub local` for a managed local hub",
            ),
            (Some(url), _) if !self.hub_up => off(
                &format!("the hub at {url} isn't answering"),
                "start it, then run `agentscm quickstart` again",
            ),
            (Some(_), None) => off(
                "no agent token or hub settings were saved",
                "run `agentscm quickstart` again, or pass `--agent-token-file`",
            ),
            (Some(_), Some(_)) if !self.hooks => off(
                "no Claude Code hooks were added",
                "run `agentscm quickstart --hook project`",
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

pub async fn run(args: QuickstartArgs) -> Result<u8> {
    let tty = std::io::stdin().is_terminal();
    if !args.yes && !tty {
        return Err(invalid(NEEDS_TERMINAL));
    }
    let ui = Ui { yes: args.yes };
    let home = home_dir()?;
    let _quickstart_lease = manifest::acquire_lease()?;
    let cwd = std::env::current_dir()?;
    let found = detect::detect(&cwd, &home);
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
    // Running from the install directory is undoable even if nothing else is written.
    if let Some((binary::Program::InstallDir(_), _)) = program_plan(&cx.home) {
        cx.manifest.bin_dir = Some(binary::bin_dir(&cx.home));
    }
    status(&cx).await;
    collisions_step(&cx);
    if let Some(url) = choose_hub(&mut cx).await {
        cx.live.hub_up = healthy(&cx.client, &url).await;
        cx.live.hub = Some(url.clone());
        if !cx.live.hub_up {
            println!(
                "The hub at {url} isn't answering; live warnings will start working once it is."
            );
        }
        if let Some(staged) = token_stage(&mut cx, &url).await {
            hub_json_stage(&mut cx, &url, &staged);
        }
    }
    hook_stage(&mut cx);
    init_stage(&mut cx);
    if !cx.manifest.is_empty() {
        cx.manifest.save()?;
    }
    let changed = record_fingerprint(&record_path) != original_record
        || hub_files.each_ref().map(|path| file_hash(path)) != original_hub_files
        || program_fingerprint(&cx.home) != original_program
        || !cx.init_written.is_empty();
    summary(&cx, changed);
    let command = cli_command(&cx);
    println!("Run AgentSCM commands with: {command}");
    if let Some(dir) = &cx.manifest.bin_dir {
        println!(
            r#"To use `agentscm` in this shell: export PATH={}:"$PATH""#,
            binary::quote(dir)
        );
    }
    println!("{}", cx.live.line());
    if let Some(failure) = cx.failure {
        return Err(invalid(&failure));
    }
    if cx.live.pending_local.is_some() {
        return Ok(3);
    }
    if (matches!(args.hub, Some(HubChoice::Local | HubChoice::Existing)) || args.hub_url.is_some())
        && (!cx.live.hub_up || cx.live.repo.is_none())
    {
        return Err(invalid(
            "the requested hub setup is incomplete; see the status and recovery instructions above",
        ));
    }
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
            env!("CARGO_PKG_VERSION"),
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
    match current_hub() {
        Some(config) => {
            let repo = found.remote.as_ref().map(|r| r.repo.as_str());
            let reach = if !healthy(&cx.client, &config.url).await {
                "not answering".to_owned()
            } else {
                match read_token(config.file_for(repo)) {
                    None => "answering; no readable token yet".to_owned(),
                    Some(token) => match whoami(&cx.client, &config.url, &token).await {
                        Ok(Some(reply)) => format!(
                            "answering; token valid for {}",
                            clean(reply["token"]["repository"].as_str().unwrap_or("?"))
                        ),
                        _ => "answering; its token is not accepted".to_owned(),
                    },
                }
            };
            println!("  Hub: {} ({reach})", config.url);
        }
        None => println!("  Hub: not configured"),
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
    if !cx.ui.confirm(
        "Check this repository for branches and worktrees that would collide right now?",
        true,
    ) {
        return;
    }
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
    let current = current_hub();
    let mut choice = cx.args.hub;
    if choice.is_none() && cx.args.hub_url.is_some() {
        choice = Some(HubChoice::Existing);
    }
    if choice.is_none() {
        if let Some(config) = &current
            && (cx.ui.yes
                || cx.ui.confirm(
                    &format!(
                        "A hub is already configured ({}). Keep using it?",
                        config.url
                    ),
                    true,
                ))
        {
            return Some(config.url.clone());
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
            HubChoice::Existing => return existing_hub(cx, current.map(|c| c.url)),
            HubChoice::Local => match local_hub(cx).await {
                Ok(url) => return url,
                Err(message) => {
                    println!("{message}");
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
        "Connect to a hub that already runs (your team's; you need its URL)",
        "Skip live warnings for now",
    ];
    let mut choices = vec![HubChoice::Existing, HubChoice::Skip];
    let mut default = 1;
    if with_local {
        options.insert(
            0,
            "Start a hub on this machine (native when supported; Docker fallback; GitHub only)",
        );
        choices.insert(0, HubChoice::Local);
        default = if (native_hub_supported() || cx.found.docker) && github {
            0
        } else {
            2
        };
    }
    choices[cx
        .ui
        .choose("How should live warnings work?", &options, default)]
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
    match check_base_url(&url) {
        Ok(url) => Some(url),
        Err(_) => {
            println!(
                "That hub URL isn't usable: it must be https://host, or http:// for a loopback address, with no credentials."
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
            let stable = binary::install(&dir, &executable, env!("CARGO_PKG_VERSION"))?;
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

// ---- sign-in and token ----

enum Mint {
    Minted { id: String, token: String },
    NotConnected,
    Forbidden,
    Unauthorized,
    Failed(String),
}

fn classify(status: u16, body: &Value) -> Mint {
    match (status, body["error"].as_str()) {
        (201, _) => match (body["id"].as_str(), body["token"].as_str()) {
            (Some(id), Some(token)) => Mint::Minted {
                id: id.into(),
                token: token.into(),
            },
            _ => Mint::Failed("the hub's reply had no token".into()),
        },
        (403, Some("not_connected")) => Mint::NotConnected,
        (403, _) => Mint::Forbidden,
        (401, _) => Mint::Unauthorized,
        _ => Mint::Failed(format!("the hub replied {status}")),
    }
}

async fn mint(cx: &Cx<'_>, url: &str, session: &str, body: &Value) -> Mint {
    let target = format!("{url}/v1/hub/tokens");
    match call(
        &cx.client,
        Method::POST,
        &target,
        Some(session),
        Some(body.clone()),
    )
    .await
    {
        Ok((status, reply)) => classify(status, &reply),
        Err(error) => Mint::Failed(short(&error)),
    }
}

/// Best effort, as the signed-in user: ask every connection to look at its repositories again.
async fn refresh_connections(cx: &Cx<'_>, url: &str, session: &str) {
    let list = format!("{url}/v1/hub/connections");
    let Ok((200, reply)) = call(&cx.client, Method::GET, &list, Some(session), None).await else {
        return;
    };
    for id in reply["connections"].as_array().into_iter().flatten() {
        if let Some(id) = id["id"].as_str().filter(|id| id.starts_with("con_")) {
            let target = format!("{url}/v1/hub/connections/{id}/refresh");
            let _ = call(&cx.client, Method::POST, &target, Some(session), None).await;
        }
    }
}

fn agent_name(cx: &Cx<'_>) -> String {
    cx.args.agent_name.clone().unwrap_or_else(|| {
        let host = crate::intent_cli::hostname().unwrap_or_default();
        let host: String = host
            .split('.')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c))
            .take(40)
            .collect();
        if host.is_empty() {
            "claude".into()
        } else {
            format!("claude-{host}")
        }
    })
}

/// A token file ready for hub.json.
struct Staged {
    file: PathBuf,
    repo: String,
}

/// Writes the token file. A file that was already there and isn't ours is backed up first
/// (and stays the user's: it is never deleted later). Returns whether the file is ours.
fn write_token_file(
    file: &Path,
    token: &str,
    ours_before: bool,
) -> std::result::Result<bool, String> {
    let existed = match file.symlink_metadata() {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("I couldn't look at {}: {e}", file.display())),
    };
    if existed && !ours_before {
        make_backup(file)?;
    }
    let real = resolve(file);
    real.parent()
        .map_or(Ok(()), private_dir)
        .map_err(|e| e.to_string())?;
    write_private(&real, &format!("{token}\n")).map_err(|e| short(&e))?;
    Ok(!existed || ours_before)
}

/// The token file to hand to hub.json, or `None` when this step could not finish.
async fn token_stage(cx: &mut Cx<'_>, url: &str) -> Option<Staged> {
    let remote_repo = cx.found.remote.as_ref().map(|r| r.repo.clone());
    // A token the user already has.
    if let Some(file) = cx.args.agent_token_file.clone() {
        let Ok(file) = file.canonicalize() else {
            println!("I can't find the token file {}.", file.display());
            return None;
        };
        let token = match read_existing(&file) {
            Ok(text) => text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
            Err(message) => {
                println!("{message}.");
                return None;
            }
        };
        let Some(token) = token else {
            println!("The token file {} is empty.", file.display());
            return None;
        };
        let mut repo = remote_repo;
        match whoami(&cx.client, url, &token).await {
            Ok(Some(reply)) => {
                let named = reply["token"]["repository"].as_str().unwrap_or("?");
                println!("The agent token is valid for {}.", clean(named));
                repo = repo.or_else(|| Some(named.to_owned()));
            }
            Ok(None) => {
                println!("The hub rejected that agent token (expired or revoked?).");
                return None;
            }
            Err(_) => println!(
                "Couldn't check the agent token because the hub isn't answering; using it as given."
            ),
        }
        let Some(repo) = repo else {
            println!(
                "I can't tell which repository that token is for; run quickstart inside the repository."
            );
            return None;
        };
        keep_record(cx, &repo, &file, false, None, url);
        return Some(Staged { file, repo });
    }
    // A working setup from an earlier run.
    if let Some(config) = current_hub()
        && config.url == url
        && let file = config.file_for(remote_repo.as_deref()).to_owned()
        && let Some(token) = read_token(&file)
        && let Ok(Some(reply)) = whoami(&cx.client, url, &token).await
        && let Some(named) = reply["token"]["repository"].as_str()
        && remote_repo
            .as_deref()
            .is_none_or(|r| named.eq_ignore_ascii_case(r))
        && cx.ui.confirm(
            "You already have a working agent token for this hub and repository. Keep it?",
            true,
        )
    {
        return Some(Staged {
            file,
            repo: named.to_owned(),
        });
    }
    let Some(remote) = cx.found.remote.clone() else {
        println!(
            "To get an agent token I need to run inside a Git repository with an origin on GitHub or GitLab, or you can pass --agent-token-file."
        );
        return None;
    };
    let provider = match cx.args.provider {
        Some(p) => p.to_possible_value().map(|v| v.get_name().to_owned()),
        None => remote.provider().map(str::to_owned),
    };
    let Some(provider) = provider else {
        println!(
            "This hub can sign you in with GitHub or GitLab, but origin is on another forge; pass --provider, or --agent-token-file."
        );
        return None;
    };
    if !cx.live.hub_up {
        return None;
    }
    let name = agent_name(cx);
    if validate_agent_name(&name).is_err() {
        println!("The agent name must be 1-64 characters of letters, digits, '.', '_' or '-'.");
        return None;
    }
    let Ok(ttl) = parse_ttl(&cx.args.ttl) else {
        println!("--ttl must look like 8h or 30d.");
        return None;
    };
    let slug = remote.repo.replace('/', "__");
    let file = config_dir()
        .ok()?
        .join("agentscm")
        .join("tokens")
        .join(format!("{slug}.token"));
    println!(
        "Will sign you in to {url} with {provider}, create an agent token (a key that lets agents on this machine use the hub for this one repository) named {name} for {} (valid {}), and write {} (0600).",
        clean(&remote.repo),
        cx.args.ttl,
        shown(&file, &cx.home)
    );
    if !cx.ui.confirm("Go ahead?", true) {
        return None;
    }
    let body = json!({"name": name, "repository": remote.repo, "ttl_seconds": ttl});
    let host = if provider == "github" {
        "github.com".to_owned()
    } else {
        remote.host.clone()
    };
    let Some(Mint::Minted { id, token }) =
        obtain(cx, url, &provider, &host, &body, &remote.repo).await
    else {
        return None;
    };
    let ours_before = cx.manifest.tokens.iter().any(|t| t.file == file && t.owned);
    let owned = match write_token_file(&file, &token, ours_before) {
        Ok(owned) => owned,
        Err(message) => {
            println!("Couldn't write the token file: {message}.");
            return None;
        }
    };
    keep_record(cx, &remote.repo, &file, owned, Some(id), url);
    Some(Staged {
        file,
        repo: remote.repo,
    })
}

/// Remembers a token file in the manifest (replacing any record of the same file).
fn keep_record(
    cx: &mut Cx<'_>,
    repo: &str,
    file: &Path,
    owned: bool,
    id: Option<String>,
    hub: &str,
) {
    cx.manifest.tokens.retain(|t| t.file != file);
    cx.manifest.tokens.push(TokenRecord {
        repo: repo.to_owned(),
        file: file.to_owned(),
        owned,
        id,
        hub: hub.to_owned(),
    });
    let _ = cx.manifest.save();
}

/// Signs in when needed and mints, repairing "app not installed on this repository".
async fn obtain(
    cx: &mut Cx<'_>,
    url: &str,
    provider: &str,
    forge_host: &str,
    body: &Value,
    repo: &str,
) -> Option<Mint> {
    let mut signed_in = false;
    loop {
        let session = load_session(Some(url)).ok().map(|(_, s)| s);
        let Some(session) = session else {
            if signed_in {
                println!("Sign-in didn't produce a usable session.");
                return None;
            }
            if !cx.tty {
                println!(
                    "I can't sign in without a terminal: pass --agent-token-file for non-interactive setup."
                );
                return None;
            }
            println!("Signing in ...");
            let existed = hub_cli::session_path().is_ok_and(|p| p.exists());
            match hub_cli::sign_in(url, provider, Some(forge_host)).await {
                Ok(who) => println!("Signed in as {who}."),
                Err(error) => {
                    println!("Couldn't sign in: {}.", short(&error));
                    return None;
                }
            }
            if !existed {
                cx.manifest.session = Some(SessionRecord {
                    hub: url.to_owned(),
                });
                let _ = cx.manifest.save();
            }
            signed_in = true;
            continue;
        };
        match mint(cx, url, &session, body).await {
            Mint::Unauthorized if !signed_in => {
                let _ = std::fs::remove_file(hub_cli::session_path().ok()?);
            }
            Mint::Unauthorized => {
                println!("The hub didn't accept the new sign-in.");
                return None;
            }
            Mint::NotConnected => {
                refresh_connections(cx, url, &session).await;
                if let done @ Mint::Minted { .. } = mint(cx, url, &session, body).await {
                    return Some(done);
                }
                println!("{}", install_help(cx.install_url.as_deref(), repo));
                if cx.ui.yes {
                    println!("The agent token wasn't created.");
                    return None;
                }
                while cx
                    .ui
                    .retry("Press Enter after installing, or type skip to stop.")
                {
                    refresh_connections(cx, url, &session).await;
                    match mint(cx, url, &session, body).await {
                        done @ Mint::Minted { .. } => return Some(done),
                        Mint::NotConnected => println!("Still not connected."),
                        _ => {
                            println!("Couldn't create the agent token.");
                            return None;
                        }
                    }
                }
                println!("Skipped. Install the app, then run quickstart again.");
                return None;
            }
            Mint::Forbidden => {
                println!(
                    "You can't read {} on the forge, so the hub won't give you a token for it.",
                    clean(repo)
                );
                return None;
            }
            Mint::Failed(why) => {
                println!("Couldn't create the agent token: {}.", clean(&why));
                return None;
            }
            minted @ Mint::Minted { .. } => return Some(minted),
        }
    }
}

fn install_help(install_url: Option<&str>, repo: &str) -> String {
    match install_url {
        Some(link) => format!(
            "The hub's GitHub App isn't installed on {}. Open {link}, choose {}, and install.",
            clean(repo),
            clean(repo)
        ),
        None => format!(
            "The hub's GitHub App isn't installed on {}. Open GitHub, then Settings, Applications, the hub's app, Configure, and add {}.",
            clean(repo),
            clean(repo)
        ),
    }
}

/// Adds this repository's token to hub.json (keeping other repositories' entries).
fn hub_json_stage(cx: &mut Cx<'_>, url: &str, staged: &Staged) {
    let Ok(path) = hub_json_path() else { return };
    let name = shown(&path, &cx.home);
    let existing = match read_existing(&path) {
        Ok(existing) => existing,
        Err(message) => {
            println!("{message}. I left it alone.");
            return;
        }
    };
    let old = match &existing {
        None => None,
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(value) if value.is_object() => Some(value),
            _ => {
                println!(
                    "{name} isn't valid JSON, so I left it alone. Fix or remove it, then run quickstart again."
                );
                return;
            }
        },
    };
    let Some(file) = staged.file.to_str() else {
        println!("The token file's path isn't plain text, so I can't put it in hub.json.");
        return;
    };
    let old_url = old
        .as_ref()
        .and_then(|o| o["url"].as_str())
        .and_then(|u| check_base_url(u).ok());
    let same_hub = old_url.as_deref() == Some(url);
    if old.is_some() && !same_hub && cx.ui.yes && cx.args.hub.is_none() && cx.args.hub_url.is_none()
    {
        println!(
            "{name} points at another hub ({}); with --yes I don't switch it unless you pass --hub or --hub-url, so I kept it.",
            clean(old_url.as_deref().unwrap_or("?"))
        );
        return;
    }
    let mut new = match (&old, same_hub) {
        (Some(old), true) => old.clone(),
        _ => json!({"url": url, "token_file": file}),
    };
    let tokens = new["tokens"].take();
    let mut tokens = if tokens.is_object() {
        tokens
    } else {
        json!({})
    };
    tokens[&staged.repo] = json!(file);
    new["tokens"] = tokens;
    let text = serde_json::to_string_pretty(&new).unwrap_or_default() + "\n";
    if old.as_ref() == Some(&new) {
        // Unchanged: whoever wrote it keeps it, so the manifest is not touched.
        println!("{name} already has this repository's token for this hub; keeping it.");
    } else {
        if let Some(existing) = &existing {
            println!("{name} already exists:\n{}", existing.trim_end());
            if !same_hub {
                println!(
                    "It points at another hub ({}); switching moves your other repositories to {url} too.",
                    clean(old_url.as_deref().unwrap_or("?"))
                );
            }
        }
        println!("Will write {name} (0600 if new): {}", new);
        let question = if existing.is_some() {
            "Replace it? A backup is kept."
        } else {
            "Write it?"
        };
        if !cx.ui.confirm(question, true) {
            println!("Left {name} as it was.");
            return;
        }
        let ours = cx
            .manifest
            .hub_json
            .as_ref()
            .filter(|r| file_hash(&r.path).as_deref() == Some(r.hash.as_str()));
        let backup = match (&existing, ours) {
            (Some(_), Some(record)) => record.backup.clone(),
            (Some(_), None) => match make_backup(&path) {
                Ok(backup) => Some(backup),
                Err(message) => {
                    println!("{message}. I left {name} alone.");
                    return;
                }
            },
            _ => None,
        };
        if let Err(message) = write_keeping_mode(&path, &text) {
            println!("Couldn't write {name}: {message}.");
            return;
        }
        cx.manifest.hub_json = Some(HubJsonRecord {
            path,
            hash: sha_hex(text.as_bytes()),
            backup,
        });
    }
    // Only now is an older token of the same repository unused.
    let stale: Vec<PathBuf> = cx
        .manifest
        .tokens
        .iter()
        .filter(|t| t.repo == staged.repo && t.file != staged.file && t.owned)
        .map(|t| t.file.clone())
        .collect();
    for old in &stale {
        let _ = std::fs::remove_file(old);
    }
    cx.manifest
        .tokens
        .retain(|t| !(t.repo == staged.repo && stale.contains(&t.file)));
    let _ = cx.manifest.save();
    cx.live.repo = Some(staged.repo.clone());
}

// ---- Claude Code hooks ----

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

fn recommended_hook_scope(found: &Detected) -> HookScope {
    if found.repo_root.is_some() && !found.worktrees {
        HookScope::Project
    } else {
        HookScope::User
    }
}

fn hook_stage(cx: &mut Cx<'_>) {
    let root = cx.found.repo_root.clone();
    let scope = match cx.args.hook {
        Some(scope) => scope,
        None if !cx.found.claude => {
            println!(
                "Claude Code wasn't found, so I'm not adding its hooks (use --hook user or --hook project to add them anyway)."
            );
            return;
        }
        None if cx.ui.yes => {
            if cx.live.repo.is_some() && root.is_some() {
                recommended_hook_scope(&cx.found)
            } else {
                println!(
                    "Not adding the Claude Code hooks: live warnings are off, so they would have nothing to report. Use --hook project to add them anyway."
                );
                return;
            }
        }
        None => {
            let options = [
                "This project only (.claude/settings.local.json)",
                "Every project (~/.claude/settings.json)",
                "Skip the hooks",
            ];
            if cx.found.worktrees {
                println!(
                    "Git worktrees detected. Every project is recommended so sessions in worktrees receive the hooks too."
                );
            }
            let default = if recommended_hook_scope(&cx.found) == HookScope::Project {
                0
            } else {
                1
            };
            [HookScope::Project, HookScope::User, HookScope::None][cx.ui.choose(
                "Where should Claude Code's hooks go?",
                &options,
                default,
            )]
        }
    };
    let path = match (scope, &root) {
        (HookScope::None, _) => return,
        (HookScope::User, _) => cx.home.join(".claude").join("settings.json"),
        (HookScope::Project, Some(root)) => root.join(".claude").join("settings.local.json"),
        (HookScope::Project, None) => {
            println!(
                "--hook project needs to run inside a Git repository, so I didn't add the hooks."
            );
            return;
        }
    };
    let name = shown(&path, &cx.home);
    let existing = match read_existing(&path) {
        Ok(existing) => existing,
        Err(message) => {
            println!("{message}. I left it alone.");
            return;
        }
    };
    let mut settings = match &existing {
        None => json!({}),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(_) => {
                println!(
                    "{name} isn't valid JSON, so I left it alone. Fix or remove it, then run quickstart again."
                );
                return;
            }
        },
    };
    // Project scope with a known hub and token: Claude Code calls the hub directly (HTTP
    // hooks, no program on this machine) and the token goes in this file's `env`.
    let token = (scope == HookScope::Project)
        .then(|| project_token(cx))
        .flatten();
    let http = cx.live.hub.clone().zip(token);
    let (wanted, program) = match &http {
        Some((hub, _)) => (hook::http_entries(hub, guard_repo(cx).as_deref()), None),
        None => {
            let Some((plan, exe)) = program_plan(&cx.home) else {
                println!("I can't tell where this program is, so I didn't add the hooks.");
                return;
            };
            let prefix = match &plan {
                binary::Program::Bare => "agentscm".to_owned(),
                binary::Program::InstallDir(link) => binary::quote(link),
                binary::Program::Copy => {
                    binary::quote(&binary::bin_dir(&cx.home).join(binary::LINK))
                }
            };
            (hook::entries(&prefix), Some((plan, exe)))
        }
    };
    let dir = binary::bin_dir(&cx.home);
    let recorded = cx
        .manifest
        .hooks
        .iter()
        .find(|r| r.path == path)
        .and_then(|r| r.env_token_sha.clone());
    let mut env = None;
    let mut replaced = 0;
    if let Some((_, token)) = &http {
        match hook::set_env(&mut settings, token, recorded.as_deref()) {
            Ok(done) => env = Some(done),
            Err(why) => {
                println!("I left {name} alone: {why}.");
                return;
            }
        }
        replaced = hook::remove_command_form(&mut settings);
    }
    let added = match hook::merge(&mut settings, &wanted) {
        Ok(added) => added,
        Err(why) => {
            println!("I left {name} alone: {why}.");
            return;
        }
    };
    if env == Some(hook::Env::Foreign) {
        println!(
            "{name} already sets {} to a value I didn't write, so I left it alone; the hooks use it, so make sure it is this repository's agent token.",
            crate::init_cli::TOKEN_VAR
        );
    }
    let sets_token = matches!(env, Some(hook::Env::Set));
    if added.is_empty() && replaced == 0 && !sets_token {
        println!("{name} already has the AgentSCM hooks; nothing to change.");
        cx.live.hooks = true;
        return;
    }
    println!(
        "Will add this to {name}{}:\n{}",
        if existing.is_some() {
            " (a backup is kept)"
        } else {
            " (0600, new file)"
        },
        hook::preview(&added, sets_token)
    );
    if replaced > 0 {
        println!(
            "Will also take the {replaced} older command-form AgentSCM hook entries out of {name}, so edits aren't announced twice."
        );
    }
    if sets_token {
        println!(
            "The real agent token is written in place of <your agent token>, so {name} will hold a secret."
        );
    }
    if let Some((binary::Program::Copy, _)) = &program {
        println!(
            "Will also copy this program to {0}/agentscm-{1} and point {0}/agentscm at it (0755), so the hooks keep working after this copy goes away.",
            shown(&dir, &cx.home),
            env!("CARGO_PKG_VERSION")
        );
    }
    if !cx.ui.confirm("Go ahead?", true) {
        return;
    }
    if let Some((plan, exe)) = &program {
        if matches!(plan, binary::Program::Copy)
            && let Err(why) = binary::install(&dir, exe, env!("CARGO_PKG_VERSION"))
        {
            println!("Couldn't copy the program: {why}. I didn't add the hooks.");
            return;
        }
        if !matches!(plan, binary::Program::Bare) {
            cx.manifest.bin_dir = Some(dir);
        }
    }
    let env_sha = http
        .as_ref()
        .filter(|_| env.is_some() && env != Some(hook::Env::Foreign))
        .map(|(_, token)| sha_hex(token.as_bytes()));
    let form = Written {
        env_sha,
        http: http.is_some(),
    };
    if let Err(why) = write_settings(cx, &path, existing.is_some(), &settings, added, form) {
        println!("Couldn't update {name}: {why}. The file is unchanged.");
        return;
    }
    cx.live.hooks = true;
    if scope != HookScope::Project {
        return;
    }
    // The command contains this machine's path, or the file holds the agent token: either
    // way it should not be committed.
    if let (Some((git, _)), Some(root)) = (&cx.found.git, &root) {
        let arg = path.display().to_string();
        let args = ["check-ignore", "-q", arg.as_str()];
        let ignored = detect::capture(git, &args, Some(root), Duration::from_secs(10)).is_some();
        match (sets_token, ignored) {
            (true, true) => {
                println!("Note: {name} now holds your agent token; never commit it.");
            }
            (true, false) => println!(
                "Warning: {name} now holds your agent token and isn't ignored by Git; add it to .gitignore before you commit anything (I haven't touched that file)."
            ),
            (false, false) => println!(
                "Note: {name} isn't ignored by Git and now holds a path on this machine; consider adding it to .gitignore (I haven't touched that file)."
            ),
            (false, true) => {}
        }
    }
}

/// The repository from the `origin` remote when it is a plain `owner/name`, for the `?repo=`
/// guard in the hook and MCP URLs.
fn guard_repo(cx: &Cx<'_>) -> Option<String> {
    let repo = &cx.found.remote.as_ref()?.repo;
    crate::intent_cli::valid_repo(repo).then(|| repo.clone())
}

/// The token of the hub quickstart just configured, as the hooks would find it in this
/// checkout.
fn project_token(cx: &Cx<'_>) -> Option<String> {
    let config = current_hub().filter(|c| Some(&c.url) == cx.live.hub.as_ref())?;
    let remote = cx.found.remote.as_ref().map(|r| r.repo.as_str());
    read_token(config.file_for(cx.live.repo.as_deref().or(remote)))
}

/// What a settings write adds to the manifest besides the hook entries.
struct Written {
    /// SHA-256 of the token quickstart put in `env`.
    env_sha: Option<String>,
    /// The HTTP form was written, so recorded command-form entries are gone from the file.
    http: bool,
}

fn write_settings(
    cx: &mut Cx<'_>,
    path: &Path,
    existed: bool,
    settings: &Value,
    added: Vec<Added>,
    form: Written,
) -> std::result::Result<(), String> {
    let backup = if existed {
        Some(make_backup(path)?)
    } else {
        path.parent()
            .map_or(Ok(()), |dir| std::fs::create_dir_all(resolve(dir)))
            .map_err(|e| e.to_string())?;
        None
    };
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())? + "\n";
    write_keeping_mode(path, &text)?;
    #[cfg(unix)]
    let holds_token = form.env_sha.is_some()
        || cx
            .manifest
            .hooks
            .iter()
            .any(|r| r.path == path && r.env_token_sha.is_some());
    // Windows has no mode bits; files under the user's profile are already private to them.
    #[cfg(unix)]
    if holds_token {
        // The file (and any copy of it) holds the agent token: owner only, whatever it was.
        for secret in std::iter::once(resolve(path)).chain(backup.iter().cloned()) {
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
    }
    let hash = sha_hex(text.as_bytes());
    match cx.manifest.hooks.iter_mut().find(|r| r.path == path) {
        Some(record) => {
            record.backups.extend(backup);
            if form.http {
                record
                    .added
                    .retain(|a| a.entry["hooks"][0]["type"] != "command");
            }
            record.added.extend(added);
            record.written_hash = hash;
            record.env_token_sha = form.env_sha.or(record.env_token_sha.take());
        }
        None => cx.manifest.hooks.push(HookRecord {
            path: path.to_owned(),
            created: !existed,
            backups: backup.into_iter().collect(),
            added,
            written_hash: hash,
            env_token_sha: form.env_sha,
        }),
    }
    cx.manifest.save().map_err(|e| e.to_string())
}

/// Whether `url` names this machine, where teammates cannot reach a hub.
fn is_loopback(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// Offers `agentscm init-repo` for the hub just configured. Failure is reported, not fatal.
fn init_stage(cx: &mut Cx<'_>) {
    let (Some(root), Some(hub)) = (cx.found.repo_root.clone(), cx.live.hub.clone()) else {
        if cx.args.init {
            println!("--init needs a Git repository and a hub, so I didn't add the agent config.");
        }
        return;
    };
    if is_loopback(&hub) {
        println!(
            "Skipping repository agent config: this hub runs on this machine. When you have a shared hub, run `agentscm init-repo --hub-url URL`."
        );
        return;
    }
    let go = if cx.ui.yes {
        cx.args.init
    } else {
        cx.ui.confirm(
            "Also add AgentSCM to this repository's agent config (MCP, hooks and instructions for Claude Code, Codex, Cursor and Copilot)? It writes files you can commit.",
            true,
        )
    };
    if !go {
        return;
    }
    match crate::init_cli::run_in(&root, Some(&hub), guard_repo(cx).as_deref(), None, cx.ui) {
        Ok(outcome) => cx.init_written = outcome.written,
        Err(error) => println!("Couldn't add the agent config: {}.", short(&error)),
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
    for token in &m.tokens {
        line(format!(
            "{}: agent token for {}{}",
            shown(&token.file, home),
            clean(&token.repo),
            if token.owned {
                ""
            } else {
                " (yours; used as given)"
            }
        ));
    }
    if let Some(hub) = &m.hub_json {
        line(format!(
            "{}: which hub to use, and which token for which repository{}",
            shown(&hub.path, home),
            hub.backup.as_ref().map_or(String::new(), |b| format!(
                " (previous copy: {})",
                shown(b, home)
            ))
        ));
    }
    if m.session.is_some()
        && let Ok(path) = hub_cli::session_path()
    {
        line(format!("{}: your hub sign-in", shown(&path, home)));
    }
    if let Some(dir) = &m.bin_dir {
        line(format!(
            "{}: the agentscm program the hooks call",
            shown(dir, home)
        ));
    }
    for record in &m.hooks {
        line(format!(
            "{}: {} Claude Code hook entries{}",
            shown(&record.path, home),
            record.added.len(),
            record.backups.first().map_or(String::new(), |b| format!(
                " (original: {})",
                shown(b, home)
            ))
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

/// Puts `backup`'s text back, atomically, through any symlink.
fn restore(path: &Path, backup: &Path) -> std::result::Result<String, String> {
    let text = std::fs::read_to_string(backup).map_err(|e| e.to_string())?;
    write_keeping_mode(path, &text)?;
    Ok(format!("restored {} from its backup", path.display()))
}

/// `(finished, what happened)`; unfinished means the file was left as it is.
fn undo_hooks(record: &HookRecord) -> (bool, String) {
    let path = &record.path;
    let text = match read_existing(path) {
        Ok(Some(text)) => text,
        Ok(None) => return (true, format!("{} is already gone", path.display())),
        Err(message) => return (false, format!("left {} alone: {message}", path.display())),
    };
    // Only a file exactly as quickstart last wrote it, written once, is put back wholesale:
    // after a second write the backups hold edits made in between.
    let unchanged = sha_hex(text.as_bytes()) == record.written_hash;
    let result = match record.backups.as_slice() {
        [] if unchanged && record.created => std::fs::remove_file(path)
            .map(|()| format!("removed {}", path.display()))
            .map_err(|e| e.to_string()),
        [backup] if unchanged && !record.created && backup.exists() => restore(path, backup),
        _ => surgical_undo(record, &text),
    };
    match result {
        Ok(message) => (true, message),
        Err(why) => (false, format!("left {} alone: {why}", path.display())),
    }
}

/// The file changed since quickstart wrote it: remove just our entries.
fn surgical_undo(record: &HookRecord, text: &str) -> std::result::Result<String, String> {
    let mut settings: Value =
        serde_json::from_str(text).map_err(|_| "it is no longer valid JSON".to_owned())?;
    let removed = hook::remove(&mut settings, &record.added);
    if let Some(sha) = &record.env_token_sha {
        hook::remove_env(&mut settings, sha);
    }
    if record.created && settings == json!({}) {
        std::fs::remove_file(&record.path).map_err(|e| e.to_string())?;
        return Ok(format!("removed {}", record.path.display()));
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    write_keeping_mode(&record.path, &text)?;
    Ok(format!(
        "removed {removed} hook entries from {}, leaving the rest",
        record.path.display()
    ))
}

pub async fn uninstall(args: UninstallArgs) -> Result<()> {
    if !args.yes && !std::io::stdin().is_terminal() {
        return Err(invalid(
            "agentscm uninstall asks before it removes anything, and there is no terminal here. Pass --yes (and --remove-hub-data to delete the local hub's data).",
        ));
    }
    let ui = Ui { yes: args.yes };
    let _quickstart_lease = manifest::acquire_lease()?;
    let Some(m) = Manifest::load()? else {
        println!("Nothing to undo: quickstart hasn't installed anything here.");
        return Ok(());
    };
    let home = home_dir()?;
    let programs = m
        .bin_dir
        .as_ref()
        .map(|d| (d.clone(), binary::installed(d)));
    println!("Will undo what quickstart did:");
    for r in &m.hooks {
        println!(
            "  - take the AgentSCM hooks out of {}",
            shown(&r.path, &home)
        );
    }
    for t in m.tokens.iter().filter(|t| t.owned) {
        println!(
            "  - delete the agent token file {} and revoke the token on the hub if it answers",
            shown(&t.file, &home)
        );
    }
    if m.session.is_some() {
        println!("  - sign out of the hub (end the sign-in quickstart created)");
    }
    if let Some(h) = &m.hub_json {
        println!(
            "  - remove {} unless you changed it since",
            shown(&h.path, &home)
        );
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
        ..Manifest::default()
    };
    let mut manual: Vec<String> = Vec::new();
    for token in &m.tokens {
        if token.owned {
            revoke(token).await;
            match std::fs::remove_file(&token.file) {
                Ok(()) => println!("Deleted {}.", shown(&token.file, &home)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("{} was already gone.", shown(&token.file, &home));
                }
                Err(e) => {
                    println!("Couldn't delete {}: {e}.", shown(&token.file, &home));
                    manual.push(format!("rm {}", token.file.display()));
                    left.tokens.push(token.clone());
                }
            }
        }
    }
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
    for record in &m.hooks {
        let (done, message) = undo_hooks(record);
        println!("{message}.");
        if !done {
            manual.push(format!(
                "edit {} and remove the hooks whose commands end in `intent start|check|done --hook claude`",
                record.path.display()
            ));
            left.hooks.push(record.clone());
        }
    }
    if let Some(hub) = &m.hub_json {
        let name = shown(&hub.path, &home);
        match file_hash(&hub.path) {
            None => println!("{name} was already gone."),
            Some(hash) if hash == hub.hash => {
                let result = match hub.backup.as_ref().filter(|b| b.exists()) {
                    Some(backup) => restore(&hub.path, backup),
                    None => std::fs::remove_file(&hub.path)
                        .map(|()| format!("removed {}", hub.path.display()))
                        .map_err(|e| e.to_string()),
                };
                match result {
                    Ok(message) => println!("{message}."),
                    Err(why) => {
                        println!("Left {name} alone: {why}.");
                        manual.push(format!("remove {}", hub.path.display()));
                        left.hub_json = Some(hub.clone());
                    }
                }
            }
            Some(_) => {
                println!("Left {name} alone: you changed it after quickstart wrote it.");
                manual.push(format!(
                    "review {} and remove what you no longer need",
                    hub.path.display()
                ));
                left.hub_json = Some(hub.clone());
            }
        }
    }
    let native_executable_drift = m.native_hub.as_ref().is_some_and(|native| {
        file_hash(&native.executable).as_deref() != Some(native.executable_sha256.as_str())
    });
    if let Some(native) = &m.native_hub {
        match hub_native::uninstall(native, &home, remove_hub_data) {
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
                left.native_hub = Some(native.clone());
            }
        }
    }
    if let Some((dir, _)) = programs
        .as_ref()
        .filter(|_| !left.hooks.is_empty() || left.native_hub.is_some() || native_executable_drift)
    {
        println!(
            "Left the program files in {}: recorded hooks or a managed hub still need them, or the native executable changed.",
            shown(dir, &home)
        );
        manual.push(
            "after resolving the retained hooks or hub, run `agentscm uninstall` again".into(),
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
        if failed {
            manual.push(format!(
                "rm {}/agentscm {}/agentscm-*",
                dir.display(),
                dir.display()
            ));
            left.bin_dir = Some(dir);
        } else {
            println!("Removed the program files in {}.", shown(&dir, &home));
        }
    }
    if let Some(local) = &m.local_hub
        && let Err(command) = undo_local_hub(remove_hub_data, local, &home)
    {
        manual.push(command);
        left.local_hub = Some(local.clone());
    }
    if left.is_empty() {
        let _ = std::fs::remove_file(manifest::path()?);
        println!(
            "Removed quickstart's record. Backups of changed files stay next to them (*.agentscm-backup-*)."
        );
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

async fn revoke(token: &TokenRecord) {
    let Some(id) = token
        .id
        .as_deref()
        .filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    else {
        return;
    };
    let (Ok(client), Ok((hub, session))) = (client(), load_session(Some(&token.hub))) else {
        println!(
            "Couldn't revoke the agent token on the hub (no saved sign-in); it expires on its own."
        );
        return;
    };
    let url = format!("{hub}/v1/hub/tokens/{id}");
    match call(&client, Method::DELETE, &url, Some(&session), None).await {
        Ok((204, _)) => println!("Revoked the agent token on the hub."),
        _ => println!(
            "Couldn't revoke the agent token on the hub (it isn't answering, or your sign-in expired); it expires on its own."
        ),
    }
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
    fn pending_local_setup_names_the_resume_command_instead_of_token_setup() {
        let live = Live {
            pending_local: Some("/opt/agentscm quickstart --hub local".into()),
            ..Live::default()
        };
        let line = live.line();
        assert!(line.contains("finish creating and installing the app"));
        assert!(line.contains("`/opt/agentscm quickstart --hub local`"));
        assert!(!line.contains("agent-token-file"));
    }

    #[test]
    fn not_connected_is_told_apart_from_other_refusals() {
        let mint = |status, body: Value| classify(status, &body);
        assert!(matches!(
            mint(403, json!({"error": "not_connected"})),
            Mint::NotConnected
        ));
        assert!(matches!(
            mint(403, json!({"error": "forbidden"})),
            Mint::Forbidden
        ));
        assert!(matches!(mint(403, Value::Null), Mint::Forbidden));
        assert!(matches!(mint(401, Value::Null), Mint::Unauthorized));
        assert!(matches!(
            mint(201, json!({"id": "tok_1", "token": "s"})),
            Mint::Minted { .. }
        ));
        assert!(matches!(mint(201, json!({})), Mint::Failed(_)));
        assert!(matches!(mint(500, Value::Null), Mint::Failed(_)));
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
            "--hub",
            "--hub-url",
            "--agent-token-file",
            "--hook",
            "--no-collisions",
            "--init",
        ] {
            assert!(NEEDS_TERMINAL.contains(flag), "{flag}");
        }
    }

    #[test]
    fn the_last_line_says_why_warnings_are_off() {
        let mut live = Live::default();
        assert!(
            live.line()
                .starts_with("Live warnings: off — no hub chosen (")
        );
        live.hub = Some("http://h".into());
        assert_eq!(
            live.line(),
            "Live warnings: off — the hub at http://h isn't answering (start it, then run `agentscm quickstart` again)"
        );
        live.hub_up = true;
        assert!(
            live.line()
                .contains("no agent token or hub settings were saved")
        );
        live.repo = Some("acme/widgets".into());
        assert!(live.line().contains("no Claude Code hooks"));
        live.hooks = true;
        assert_eq!(
            live.line(),
            "Live warnings: on (hub http://h, repo acme/widgets)"
        );
    }

    #[test]
    fn a_token_file_that_was_there_is_backed_up_and_never_ours() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tokens").join("a__b.token");
        // New file: ours, private directory and file.
        assert!(write_token_file(&file, "one", false).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        #[cfg(unix)]
        assert_eq!(mode_of(&file), 0o600);
        #[cfg(unix)]
        assert_eq!(mode_of(file.parent().unwrap()), 0o700);
        // Ours already (the manifest says so): replaced without a backup, still ours.
        assert!(write_token_file(&file, "two", true).unwrap());
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
        // Not ours: backed up, replaced, not ours.
        assert!(!write_token_file(&file, "three", false).unwrap());
        let names: Vec<String> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let backup = names
            .iter()
            .find(|n| n.contains("agentscm-backup"))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(file.parent().unwrap().join(backup)).unwrap(),
            "two\n"
        );
    }

    #[test]
    fn only_this_machine_counts_as_a_local_hub() {
        for url in [
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "https://localhost",
        ] {
            assert!(is_loopback(url), "{url}");
        }
        for url in ["https://hub.example.com", "http://192.168.1.5:8080", "nope"] {
            assert!(!is_loopback(url), "{url}");
        }
    }

    #[test]
    fn undo_takes_back_the_http_hooks_and_only_our_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let mut settings = json!({"model": "x"});
        let added = hook::merge(
            &mut settings,
            &hook::http_entries("https://h.example.com", None),
        )
        .unwrap();
        hook::set_env(&mut settings, "tok", None).unwrap();
        let record = HookRecord {
            path: path.clone(),
            created: true,
            backups: Vec::new(),
            added,
            written_hash: String::new(),
            env_token_sha: Some(sha_hex(b"tok")),
        };
        let text = serde_json::to_string(&settings).unwrap();
        std::fs::write(&path, &text).unwrap();
        surgical_undo(&record, &text).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            json!({"model": "x"})
        );
        // A token the user has since replaced stays.
        let mut changed: Value = serde_json::from_str(&text).unwrap();
        changed["env"][hook_env()] = json!("theirs");
        surgical_undo(&record, &changed.to_string()).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["env"][hook_env()], "theirs");
        assert!(after.get("hooks").is_none());
    }

    fn hook_env() -> &'static str {
        crate::init_cli::TOKEN_VAR
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        path.metadata().unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn writes_go_through_symlinks_and_unreadable_files_are_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_keeping_mode(&link, "new").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(mode_of(&real), 0o640);
        let backup = make_backup(&link).unwrap();
        assert_eq!(backup.parent(), real.canonicalize().unwrap().parent());
        assert_eq!(read_existing(&dir.path().join("missing")), Ok(None));
        std::fs::write(dir.path().join("binary"), [0xff, 0xfe]).unwrap();
        assert!(read_existing(&dir.path().join("binary")).is_err());
        assert!(read_existing(dir.path()).is_err());
    }
}
