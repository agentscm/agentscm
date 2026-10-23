// The native workspace and the Unix-only commands leave some code unused on Windows.
#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsString;
#[cfg(unix)]
use std::fs::DirBuilder;
use std::fs::File;
use std::io::{self, Read};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::checks::{SourcePathsRecipe, SourcePathsRequest};
use agentscm::context::{
    ChangeId, MAX_REVIEW_CONTEXT_BYTES, ReviewContext, SUPPORTED_CAPABILITIES,
};
#[cfg(unix)]
use agentscm::execution::{
    Cancellation, CommandCheckRequest, CommandRecipe, OciBackend, environment_digest,
};
use agentscm::model::{Approval, FileChange, IntegrateRequest, PrepareLandingRequest};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[cfg(unix)]
mod agent_stop;
#[cfg(unix)]
mod agent_stop_install;
#[cfg(unix)]
mod agent_task_cli;
#[cfg(unix)]
mod agent_task_stop;
#[cfg(unix)]
mod attached_worktree_cli;
mod autopsy_cli;
mod client_cli;
mod client_config;
mod client_mcp;
#[cfg(unix)]
mod client_plugin;
#[cfg(unix)]
mod client_setup;
mod demo_cli;
mod distribution;
#[cfg(unix)]
mod git_collisions_cli;
#[cfg(unix)]
mod git_export_cli;
#[cfg(unix)]
mod git_reconcile_cli;
#[cfg(unix)]
mod git_remote_cli;
#[cfg(unix)]
mod git_snapshot_cli;
#[cfg(unix)]
mod guided_cli;
mod hook_cli;
#[cfg(unix)]
mod hosted_workflow_client;
mod hub_cli;
mod init_cli;
mod intent_cli;
mod local_recovery_cli;
#[cfg(unix)]
mod local_validation_cli;
#[cfg(unix)]
mod merge_cli;
mod prompt;
#[cfg(unix)]
mod publish_cli;
mod quickstart;
mod server;
#[cfg(unix)]
mod setup;
#[cfg(unix)]
mod setup_cli;
mod skill_workflow_cli;
#[cfg(unix)]
mod skill_workflow_loop;
#[cfg(unix)]
mod skill_workflow_mcp;
mod team_cli;
mod telemetry;
mod telemetry_cli;
mod work_cli;
#[cfg(unix)]
mod workflow_cli;
#[cfg(unix)]
mod workspace_client;

const MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENT_LIMIT: usize = 1_000;
const MAX_DIFF_LIMIT: usize = 10_000;

#[derive(Debug, Parser)]
#[command(
    name = "agentscm",
    version,
    about = "Coordination for many coding agents on one Git repository (alpha)"
)]
struct Cli {
    /// Repository data directory.
    #[arg(long, global = true, default_value = ".agentscm")]
    data_dir: PathBuf,
    /// Disable telemetry for this invocation, including any background sender.
    #[arg(long, global = true)]
    no_telemetry: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// [experimental] Bind an existing Git worktree to native source qualification.
    #[cfg(unix)]
    AttachedWorktree {
        #[command(subcommand)]
        command: attached_worktree_cli::AttachedWorktreeCommand,
    },
    /// [experimental] Operate or consume agent-owned workflow tasks.
    #[cfg(unix)]
    AgentTask {
        #[command(subcommand)]
        command: agent_task_cli::AgentTaskCommand,
    },
    /// [experimental] Produce, show and verify Git-bound local validation attestations.
    #[cfg(unix)]
    LocalValidation {
        #[command(subcommand)]
        command: local_validation_cli::LocalValidationCommand,
    },
    /// [experimental] Run bounded repository skill workflows on immutable local source.
    SkillWorkflow {
        #[command(subcommand)]
        command: skill_workflow_cli::SkillWorkflowCommand,
    },
    /// Inspect or change pseudonymous product telemetry.
    Telemetry {
        #[command(subcommand)]
        command: telemetry_cli::TelemetryCommand,
    },
    /// [experimental] Initialize or inspect an explicitly owned local native installation.
    #[cfg(unix)]
    Setup {
        #[command(subcommand)]
        command: setup_cli::SetupCommand,
    },
    /// Guide one trusted-local Change through explicit review and landing.
    #[command(hide = true)]
    #[cfg(unix)]
    Workflow {
        #[command(subcommand)]
        command: guided_cli::WorkflowCommand,
    },
    /// [experimental] Export an integrated snapshot to a separately owned local Git mirror.
    #[cfg(unix)]
    GitExport {
        #[command(subcommand)]
        command: git_export_cli::GitExportCommand,
    },
    /// [experimental] Observe an explicit local Git/native pair without changing branch authority.
    #[cfg(unix)]
    GitReconcile {
        #[command(subcommand)]
        command: git_reconcile_cli::GitReconcileCommand,
    },
    /// Warn when in-flight Git branches or worktrees touch the same files.
    #[cfg(unix)]
    Collisions(git_collisions_cli::CollisionsArgs),
    /// Replay your own recent Claude Code sessions through the hub's contested-file rule: a
    /// local, read-only report of how often two of them touched the same file.
    Autopsy(autopsy_cli::AutopsyArgs),
    /// Back up and restore physically isolated trusted-local state.
    #[command(hide = true)]
    Recovery {
        #[command(subcommand)]
        command: local_recovery_cli::RecoveryCommand,
    },
    /// Run explicitly configured authenticated team operations.
    #[command(hide = true)]
    Team {
        #[command(subcommand)]
        command: team_cli::TeamCommand,
    },
    /// Run the multi-user coordination hub.
    Hub {
        #[command(subcommand)]
        command: hub_cli::HubCommand,
    },
    /// Try AgentSCM with no setup: simulate three agents against a throwaway local hub, using
    /// file names from this Git repository. Never writes to the repository.
    Demo(demo_cli::DemoArgs),
    /// Guided setup: check for colliding branches, then optionally connect a hub and
    /// add Claude Code hooks. Asks before every change; `agentscm uninstall` undoes it.
    Quickstart(quickstart::QuickstartArgs),
    /// Undo what `agentscm quickstart` set up (hooks, tokens, hub settings, local hub).
    Uninstall(quickstart::UninstallArgs),
    /// Agent hook client: reads the hook JSON on stdin, asks the hub, prints the agent's reply.
    /// Always exits 0 and fails open. Usage: `agentscm hook <claude|codex|cursor|copilot|generic>
    /// [--hub-url URL]`; the token comes from AGENTSCM_HUB_TOKEN(_FILE) or hub.json.
    Hook(hook_cli::HookArgs),
    /// Wire this Git repository's agents to a hub: write hooks, MCP config and instructions.
    InitRepo(init_cli::InitArgs),
    /// Bind or diagnose a checkout's connection to an existing coordination Hub.
    Client {
        #[command(subcommand)]
        command: client_cli::ClientCommand,
    },
    /// Create a local branded Codex plugin marketplace for this installed executable.
    #[cfg(unix)]
    ClientPlugin(client_plugin::PluginArgs),
    /// Announce and check work intents on a hub (advisory).
    Intent {
        #[command(subcommand)]
        command: intent_cli::IntentCommand,
    },
    /// Coordinate durable engineering work, claims and resumable check-ins.
    Work {
        #[command(subcommand)]
        command: work_cli::WorkCommand,
    },
    /// [experimental] Capture a clean committed Git tree as a native revision.
    #[cfg(unix)]
    GitSnapshot {
        #[command(subcommand)]
        command: git_snapshot_cli::GitSnapshotCommand,
    },
    /// [experimental] Create the native source store in --data-dir (not the hub or agent wiring).
    Init,
    /// Package this executable and its compiled notices into a new local bundle.
    #[command(hide = true)]
    PackageNative {
        output: PathBuf,
        /// Copy a separately built portable verifier without executing it.
        #[arg(long, value_name = "EXECUTABLE")]
        portable_verifier: Option<PathBuf>,
    },
    /// [experimental] Manage durable trusted-local divergent source merge sessions.
    #[cfg(unix)]
    Merge {
        #[command(subcommand)]
        command: merge_cli::MergeCommand,
    },
    /// [experimental] Start a stable change from an immutable review context.
    Start {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Explicit Git fetch and recoverable ref publication.
    #[cfg(unix)]
    GitRemote(git_remote_cli::GitRemoteArgs),
    /// Internal scoped credential helper; never used for interactive sign-in.
    #[command(hide = true)]
    #[cfg(unix)]
    GitRemoteCredential { operation: String },
    /// [experimental] Show a change and its exact current review context.
    Show { repository: String, change: String },
    /// [experimental] Replace a change's context only at the expected generation.
    Context {
        expected_version: u64,
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Inspect an immutable historical review context by digest.
    ReviewContext { digest: String },
    /// [experimental] Manage a native local working directory.
    #[cfg(unix)]
    Workspace {
        #[command(subcommand)]
        command: workflow_cli::WorkspaceCommand,
    },
    /// [experimental] Connect local agents to a pinned workspace timeline.
    #[cfg(unix)]
    WorkspaceClient {
        #[command(subcommand)]
        command: workspace_client::WorkspaceClientCommand,
    },
    /// [experimental] Exchange source objects through offline files.
    #[cfg(unix)]
    Sync {
        #[command(subcommand)]
        command: workflow_cli::SyncCommand,
    },
    /// [experimental] Compute the canonical digest of a source-path check recipe.
    CheckRecipe {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Register an exact-context controller check attempt.
    CheckSourcePaths {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Execute a pending built-in source-path check.
    CheckRun {
        attempt_id: String,
        expected_generation: u64,
    },
    /// [experimental] Cancel an attempt at its expected run generation.
    CheckCancel {
        attempt_id: String,
        expected_generation: u64,
        reason: String,
    },
    /// [experimental] Mark an interrupted attempt incomplete at its expected generation.
    CheckRecover {
        attempt_id: String,
        expected_generation: u64,
        reason: String,
    },
    /// [experimental] Compute the environment and canonical digests of a command check recipe.
    #[cfg(unix)]
    #[command(name = "check-command-recipe")]
    IsolatedCheckRecipe {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Register an exact-context isolated command check attempt.
    #[cfg(unix)]
    #[command(name = "check-command")]
    IsolatedCheckCreate {
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Run a pending command check in an isolated container.
    #[cfg(unix)]
    #[command(name = "check-run-command")]
    IsolatedCheckRun {
        attempt_id: String,
        expected_generation: u64,
        /// Trusted absolute path of the Docker CLI executable.
        #[arg(long)]
        docker: PathBuf,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long)]
        docker_socket: PathBuf,
    },
    /// [experimental] Cancel a pending or running command check, reconciling any container it owns.
    #[cfg(unix)]
    #[command(name = "check-cancel-command")]
    IsolatedCheckCancel {
        attempt_id: String,
        expected_generation: u64,
        reason: String,
        /// Trusted absolute path of the Docker CLI executable; required to reconcile a container.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
    },
    /// [experimental] Mark an interrupted running command check incomplete once its container is confirmed gone.
    #[cfg(unix)]
    #[command(name = "check-recover-command")]
    IsolatedCheckRecover {
        attempt_id: String,
        expected_generation: u64,
        reason: String,
        /// Trusted absolute path of the Docker CLI executable; required to reconcile a container.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Existing local Unix socket of the trusted Docker daemon.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
        /// Finish as indeterminate, keeping the handle, when container removal cannot be confirmed.
        #[arg(long)]
        abandon_unconfirmed: bool,
    },
    /// [experimental] Inspect a retained command check attempt and its record.
    #[cfg(unix)]
    #[command(name = "check-command-show")]
    IsolatedCheckShow { attempt_id: String },
    /// [experimental] Derive the candidate revision from a successful agent run's stdout.
    #[cfg(unix)]
    #[command(name = "check-command-candidate")]
    IsolatedCheckCandidate { attempt_id: String },
    /// [experimental] Inspect a retained controller check attempt.
    CheckShow { attempt_id: String },
    /// [experimental] Inspect immutable controller evidence.
    CheckEvidence { digest: String },
    /// [experimental] Create an immutable revision from a JSON request.
    Checkpoint {
        /// JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] List or find paths in one bounded manifest page, without file contents.
    Files {
        revision: String,
        /// JSON cursor string from a previous page, saved in a file.
        #[arg(long)]
        cursor: Option<PathBuf>,
        /// Maximum manifest entries scanned in this page, before filtering.
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Include this exact path and its descendants.
        #[arg(long)]
        prefix: Option<String>,
        /// Include paths containing this literal, case-sensitive text.
        #[arg(long)]
        contains: Option<String>,
    },
    /// [experimental] Resolve one path's manifest membership without reading its blob.
    Stat { revision: String, path: String },
    /// [experimental] Read one file from an immutable revision.
    Read { revision: String, path: String },
    /// [experimental] Compare two immutable revisions.
    Diff {
        left: String,
        right: String,
        #[arg(long, default_value_t = 1_000)]
        limit: usize,
    },
    /// [experimental] Resume a bounded source diff. Cursor files contain a JSON string.
    DiffPage {
        left: String,
        right: String,
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 1_000)]
        limit: usize,
    },
    /// [experimental] Inspect a bounded history page. Cursor files contain a JSON string.
    History {
        revision: String,
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// [experimental] Inspect head relation with bounded resumable graph traversal.
    Relation {
        left: String,
        right: String,
        #[arg(long)]
        cursor: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// [experimental] Record a local human approval from JSON.
    Approve {
        /// JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Revoke a local approval.
    Revoke { approval_id: String },
    /// [experimental] Show or conditionally advance the policy epoch.
    Policy {
        /// Expected current epoch. When present, advances it by one.
        #[arg(long)]
        advance: Option<u64>,
    },
    /// [experimental] Show a protected target, or null when it does not exist.
    Target { name: String },
    /// [experimental] Conditionally integrate a revision from a JSON request.
    Integrate {
        /// JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Initialize an absent protected target from a JSON request.
    InitializeTarget {
        /// JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] Publish an exact landing revision and rebind a change to it, from a JSON request.
    PrepareLanding {
        /// JSON file, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// [experimental] List integration events after a sequence number.
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Serve collision warnings over HTTP (with --collisions-repo) and the experimental read-only
    /// native API.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7331")]
        bind: SocketAddr,
        #[arg(long, default_value_t = 1)]
        max_concurrency: usize,
        /// Serve collision warnings for this trusted Git checkout. Off when omitted.
        #[arg(long)]
        collisions_repo: Option<PathBuf>,
        /// Serve this native workspace's snapshot timeline read-only under /v1/local. Off when
        /// omitted.
        #[arg(long)]
        timeline_workspace: Option<PathBuf>,
        /// Allow this exact browser origin to read /v1/local cross-origin (repeatable). Requires
        /// AGENTSCM_SERVE_READ_TOKEN.
        #[arg(long)]
        browser_origin: Vec<String>,
        /// Allow a non-loopback bind address.
        #[arg(long)]
        public: bool,
    },
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointInput {
    parent: Option<String>,
    #[serde(default)]
    parents: Option<Vec<String>>,
    changes: Vec<FileChange>,
}

/// Whether argv structurally parses to a Stop-hook subcommand of `skill-workflow`.
fn is_stop_hook_invocation(argv: &[OsString]) -> bool {
    let Ok(root) = <Cli as clap::CommandFactory>::command()
        .ignore_errors(true)
        .try_get_matches_from(argv)
    else {
        return false;
    };
    let Some(("skill-workflow", sub)) = root.subcommand() else {
        return false;
    };
    matches!(
        sub.subcommand_name(),
        Some(
            "claude-stop"
                | "remote-stop"
                | "codex-stop"
                | "remote-codex-stop"
                | "task-stop"
                | "task-codex-stop"
        )
    )
}

/// Recognize capture hooks structurally even when one of their options is invalid.
fn is_capture_hook_invocation(argv: &[OsString]) -> bool {
    let Ok(root) = <Cli as clap::CommandFactory>::command()
        .ignore_errors(true)
        .try_get_matches_from(argv)
    else {
        return false;
    };
    root.subcommand().is_some_and(|(name, sub)| {
        name == "workspace-client" && sub.subcommand_name() == Some("hook")
    })
}

enum ParsedCli {
    Run(Box<Cli>),
    Return,
    Exit(i32),
}

fn parse_cli(argv: Vec<OsString>) -> ParsedCli {
    match Cli::try_parse_from(&argv) {
        Ok(cli) => ParsedCli::Run(Box::new(cli)),
        Err(error) if error.use_stderr() => {
            if is_capture_hook_invocation(&argv) {
                eprintln!("AgentSCM capture skipped: invalid hook command options");
                println!("{{}}");
                return ParsedCli::Return;
            }
            print_error(&Error::Invalid(error.to_string()));
            // Exit 2 blocks a Stop hook, so a parse error there is a non-blocking error.
            ParsedCli::Exit(if is_stop_hook_invocation(&argv) { 1 } else { 2 })
        }
        Err(error) => {
            let _ = error.print();
            ParsedCli::Return
        }
    }
}

fn parse_cli_on_bounded_stack() -> Result<ParsedCli> {
    // Debug builds of the derived command graph exceed a 1 MiB main-thread stack.
    // Keep both the primary parse and hook-error recognition on this bounded stack.
    const PARSER_STACK_BYTES: usize = 4 * 1024 * 1024;
    let argv = std::env::args_os().collect();
    let thread = std::thread::Builder::new()
        .name("agentscm-cli-parser".into())
        .stack_size(PARSER_STACK_BYTES)
        .spawn(move || parse_cli(argv))?;
    match thread.join() {
        Ok(parsed) => Ok(parsed),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::main]
async fn main() {
    // Before clap: a bad flag must not exit non-zero (agents read that as "block").
    if std::env::args_os().nth(1).is_some_and(|a| a == "hook") {
        let args = std::env::args_os().skip(2);
        hook_cli::run_unparsed(args.map(|a| a.to_string_lossy().into_owned()).collect()).await;
    }
    let cli = match parse_cli_on_bounded_stack() {
        Ok(ParsedCli::Run(cli)) => *cli,
        Ok(ParsedCli::Return) => return,
        Ok(ParsedCli::Exit(code)) => std::process::exit(code),
        Err(error) => {
            print_error(&error);
            std::process::exit(1);
        }
    };

    let invocation_off = cli.no_telemetry;
    let verified_install = matches!(
        std::env::var("AGENTSCM_VERIFIED_INSTALL").as_deref(),
        Ok("1")
    ) && verified_install_command(&cli.command);
    let install_after_command = matches!(
        &cli.command,
        Command::Telemetry {
            command: telemetry_cli::TelemetryCommand::On
        }
    );
    if let Command::Quickstart(args) = &cli.command
        && args.can_start()
    {
        let _ = telemetry::onboard(args.noninteractive(), invocation_off);
    }
    if verified_install && !install_after_command {
        telemetry::record_installation(invocation_off);
    }
    let feature = telemetry_feature(&cli.command);
    let result = run(cli).await;
    if verified_install && install_after_command && matches!(result, Ok(RunExit::Completed)) {
        telemetry::record_installation(invocation_off);
    }
    if let Some(feature) = feature {
        telemetry::record(
            feature,
            if matches!(result, Ok(RunExit::Completed)) {
                telemetry::Outcome::Succeeded
            } else {
                telemetry::Outcome::Failed
            },
            invocation_off,
        );
    }
    match result {
        Ok(RunExit::Completed) => {}
        Ok(RunExit::SpecializedFailure(code)) => std::process::exit(code),
        Err(error) => {
            print_error(&error);
            std::process::exit(1);
        }
    }
}

enum RunExit {
    Completed,
    /// The command already rendered its specialized failure; preserve only its exit status.
    SpecializedFailure(i32),
}

async fn run(cli: Cli) -> Result<RunExit> {
    match cli.command {
        #[cfg(unix)]
        Command::AgentTask { command } => {
            print_json(&agent_task_cli::run(&cli.data_dir, command).await?);
        }
        #[cfg(unix)]
        Command::LocalValidation { command } => {
            let code = local_validation_cli::run(command).await?;
            if code != 0 {
                return Ok(RunExit::SpecializedFailure(code));
            }
        }
        Command::SkillWorkflow { command } => {
            skill_workflow_cli::run(&cli.data_dir, command).await?;
        }
        Command::Telemetry { command } => {
            match telemetry_cli::run(command, cli.no_telemetry).await? {
                telemetry_cli::Output::Value(value) => print_json(&value),
                telemetry_cli::Output::Quiet => {}
            }
        }
        #[cfg(unix)]
        Command::Setup { command } => match setup_cli::run(command) {
            Ok(value) => print_json(&value),
            Err(error) => {
                eprintln!("{}", json!({"ok":false,"error":error}));
                return Ok(RunExit::SpecializedFailure(1));
            }
        },
        #[cfg(unix)]
        Command::Workflow { command } => {
            print_json(&guided_cli::run(&cli.data_dir, command)?);
        }
        #[cfg(unix)]
        Command::GitExport { command } => {
            print_json(&git_export_cli::run(command, &cli.data_dir)?);
        }
        #[cfg(unix)]
        Command::GitRemoteCredential { operation } => {
            git_remote_cli::credential(&operation)?;
        }
        #[cfg(unix)]
        Command::GitRemote(args) => {
            print_json(&git_remote_cli::run(args, &cli.data_dir)?);
        }
        #[cfg(unix)]
        Command::AttachedWorktree { command } => {
            print_json(&attached_worktree_cli::run(&cli.data_dir, command).await?);
        }
        #[cfg(unix)]
        Command::GitReconcile { command } => {
            print_json(&git_reconcile_cli::run(command, &cli.data_dir)?);
        }
        #[cfg(unix)]
        Command::Collisions(args) => git_collisions_cli::run(args)?,
        Command::Autopsy(args) => autopsy_cli::run(args)?,
        Command::Recovery { command } => {
            print_json(&local_recovery_cli::run(&cli.data_dir, command)?);
        }
        Command::Team { command } => team_cli::run(command).await?,
        Command::Hub { command } => hub_cli::run(command, &cli.data_dir).await?,
        Command::Hook(args) => hook_cli::run(args).await,
        Command::InitRepo(args) => init_cli::run(args)?,
        Command::Client { command } => client_cli::run(command).await?,
        #[cfg(unix)]
        Command::ClientPlugin(args) => client_plugin::run(args)?,
        Command::Intent { command } => intent_cli::run(command).await?,
        Command::Work { command } => work_cli::run(command).await?,
        Command::Demo(args) => demo_cli::run(args).await?,
        Command::Quickstart(args) => {
            let code = quickstart::run(args).await?;
            if code != 0 {
                return Ok(RunExit::SpecializedFailure(i32::from(code)));
            }
        }
        Command::Uninstall(args) => quickstart::uninstall(args).await?,
        #[cfg(unix)]
        Command::GitSnapshot { command } => {
            print_json(&git_snapshot_cli::run(&cli.data_dir, command)?);
        }
        Command::Init => {
            Engine::open(&cli.data_dir)?;
            let authority = Authority::open(&cli.data_dir)?;
            print_json(&json!({"ok": true, "data_dir": cli.data_dir,
                "repository": authority.repository_id()?}));
        }
        Command::PackageNative {
            output,
            portable_verifier,
        } => {
            let manifest = match portable_verifier {
                Some(verifier) => distribution::package_native_with_verifier(&output, &verifier)?,
                None => distribution::package_native(&output)?,
            };
            print_json(&json!({"ok": true, "manifest": manifest}));
        }
        #[cfg(unix)]
        Command::Merge { command } => {
            print_json(&merge_cli::merge(&cli.data_dir, command)?);
        }
        Command::Start { input } => {
            let context = read_context(&input)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let change = authority.start_change(&context)?;
            print_json(&json!({"ok": true, "change": change,
                "readiness": context.readiness(SUPPORTED_CAPABILITIES)}));
        }
        Command::Show { repository, change } => {
            let authority = Authority::open(&cli.data_dir)?;
            let change = authority.change(&ChangeId {
                repository,
                id: change,
            })?;
            let context = authority.review_context(&change.context_digest)?;
            print_json(&json!({"ok": true, "change": change, "context": context,
                "readiness": context.readiness(SUPPORTED_CAPABILITIES)}));
        }
        Command::Context {
            expected_version,
            input,
        } => {
            let context = read_context(&input)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let change = authority.update_change_context(expected_version, &context)?;
            print_json(&json!({"ok": true, "change": change,
                "readiness": context.readiness(SUPPORTED_CAPABILITIES)}));
        }
        Command::ReviewContext { digest } => {
            let authority = Authority::open(&cli.data_dir)?;
            let context = authority.review_context(&digest)?;
            print_json(&json!({"ok": true, "digest": digest, "context": context,
                "readiness": context.readiness(SUPPORTED_CAPABILITIES)}));
        }
        #[cfg(unix)]
        Command::WorkspaceClient { command } => {
            workspace_client::run(&cli.data_dir, command)?;
        }
        #[cfg(unix)]
        Command::Workspace { command } => {
            print_json(&workflow_cli::workspace(&cli.data_dir, command)?);
        }
        #[cfg(unix)]
        Command::Sync { command } => {
            print_json(&workflow_cli::sync(&cli.data_dir, command)?);
        }
        Command::CheckRecipe { input } => {
            let recipe: SourcePathsRecipe = read_json(&input)?;
            print_json(&json!({"ok":true,"recipe_sha256":recipe.digest()?}));
        }
        Command::CheckSourcePaths { input } => {
            let request: SourcePathsRequest = read_json(&input)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            print_json(
                &json!({"ok":true,"attempt":authority.create_source_paths_attempt(&request)?}),
            );
        }
        Command::CheckRun {
            attempt_id,
            expected_generation,
        } => {
            let engine = Engine::open(&cli.data_dir)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            print_json(
                &json!({"ok":true,"attempt":authority.run_source_paths_attempt(&engine, &attempt_id, expected_generation)?}),
            );
        }
        Command::CheckCancel {
            attempt_id,
            expected_generation,
            reason,
        } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            print_json(
                &json!({"ok":true,"attempt":authority.cancel_check_attempt(&attempt_id, expected_generation, &reason)?}),
            );
        }
        Command::CheckRecover {
            attempt_id,
            expected_generation,
            reason,
        } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            print_json(
                &json!({"ok":true,"attempt":authority.recover_check_attempt(&attempt_id, expected_generation, &reason)?}),
            );
        }
        #[cfg(unix)]
        Command::IsolatedCheckRecipe { input } => {
            // Fill in the environment digest so authors need not compute it by hand.
            let mut recipe: CommandRecipe = read_json(&input)?;
            recipe.environment_sha256 = environment_digest(&recipe.environment)?;
            print_json(&json!({"ok":true,"recipe_sha256":recipe.digest()?,"recipe":recipe}));
        }
        #[cfg(unix)]
        Command::IsolatedCheckCreate { input } => {
            let request: CommandCheckRequest = read_json(&input)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            print_json(&json!({"ok":true,"attempt":authority.create_command_attempt(&request)?}));
        }
        #[cfg(unix)]
        Command::IsolatedCheckRun {
            attempt_id,
            expected_generation,
            docker,
            docker_socket,
        } => {
            let engine = Engine::open(&cli.data_dir)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let work_dir = private_execution_dir(&cli.data_dir)?;
            let config = PrivateExecutionDirectory::create(&work_dir)?;
            let backend = OciBackend::new(&docker, &docker_socket, &config.0)?;
            // The collector then kills and removes the container and the run finishes cancelled.
            let cancel = Cancellation::default();
            // Register both before any container can exist, so neither signal kills the process.
            use tokio::signal::unix::{SignalKind, signal};
            let mut interrupt = signal(SignalKind::interrupt())?;
            let mut terminate = signal(SignalKind::terminate())?;
            let signals = tokio::spawn({
                let cancel = cancel.clone();
                async move {
                    tokio::select! {
                        _ = interrupt.recv() => {}
                        _ = terminate.recv() => {}
                    }
                    cancel.cancel();
                }
            });
            // The run blocks; let the runtime move the signal task to another worker.
            let result = tokio::task::block_in_place(|| {
                authority.run_command_attempt(
                    &engine,
                    &backend,
                    &work_dir,
                    &attempt_id,
                    expected_generation,
                    &cancel,
                )
            });
            signals.abort();
            print_command_attempt(&authority, &result?)?;
        }
        #[cfg(unix)]
        Command::IsolatedCheckCancel {
            attempt_id,
            expected_generation,
            reason,
            docker,
            docker_socket,
        } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            let work_dir = private_execution_dir(&cli.data_dir)?;
            let runtime = optional_backend(docker, docker_socket, &work_dir)?;
            let attempt = authority.cancel_command_attempt(
                runtime.as_ref().map(|(_, backend)| backend),
                &work_dir,
                &attempt_id,
                expected_generation,
                &reason,
            )?;
            print_command_attempt(&authority, &attempt)?;
        }
        #[cfg(unix)]
        Command::IsolatedCheckRecover {
            attempt_id,
            expected_generation,
            reason,
            docker,
            docker_socket,
            abandon_unconfirmed,
        } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            let work_dir = private_execution_dir(&cli.data_dir)?;
            let runtime = optional_backend(docker, docker_socket, &work_dir)?;
            let attempt = authority.recover_command_attempt(
                runtime.as_ref().map(|(_, backend)| backend),
                &work_dir,
                &attempt_id,
                expected_generation,
                abandon_unconfirmed,
                &reason,
            )?;
            print_command_attempt(&authority, &attempt)?;
        }
        #[cfg(unix)]
        Command::IsolatedCheckShow { attempt_id } => {
            let authority = Authority::open(&cli.data_dir)?;
            print_command_attempt(&authority, &authority.command_attempt(&attempt_id)?)?;
        }
        #[cfg(unix)]
        Command::IsolatedCheckCandidate { attempt_id } => {
            let engine = Engine::open(&cli.data_dir)?;
            let authority = Authority::open(&cli.data_dir)?;
            print_json(
                &json!({"ok":true,"candidate":authority.command_candidate(&engine, &attempt_id)?}),
            );
        }
        Command::CheckShow { attempt_id } => {
            let authority = Authority::open(&cli.data_dir)?;
            print_json(&json!({"ok":true,"attempt":authority.check_attempt(&attempt_id)?}));
        }
        Command::CheckEvidence { digest } => {
            let authority = Authority::open(&cli.data_dir)?;
            print_json(&json!({"ok":true,"evidence":authority.check_evidence(&digest)?}));
        }
        Command::Checkpoint { input } => {
            let request: CheckpointInput = read_json(&input)?;
            let engine = Engine::open(&cli.data_dir)?;
            let revision = match request.parents {
                Some(parents) if request.parent.is_none() => {
                    engine.checkpoint_with_parents(&parents, &request.changes)?
                }
                Some(_) => {
                    return Err(Error::Invalid(
                        "checkpoint cannot specify both parent and parents".into(),
                    ));
                }
                None => engine.checkpoint(request.parent.as_deref(), &request.changes)?,
            };
            print_json(&json!({"ok": true, "revision": revision}));
        }
        Command::Files {
            revision,
            cursor,
            limit,
            prefix,
            contains,
        } => {
            require_limit(limit, agentscm::store::MAX_FILE_PAGE, "files")?;
            if let Some(prefix) = &prefix {
                agentscm::store::validate_path(prefix)?;
            }
            if contains
                .as_ref()
                .is_some_and(|text| text.is_empty() || text.len() > 1024)
            {
                return Err(Error::Invalid(
                    "path search text must contain 1 to 1024 bytes".into(),
                ));
            }
            let engine = Engine::open(&cli.data_dir)?;
            let reader = agentscm::store::SourceReader::new(&engine);
            let cursor: Option<String> = cursor.as_deref().map(read_json).transpose()?;
            let page = reader.list_files(&revision, cursor.as_deref(), limit)?;
            let scanned = page.entries.len();
            let entries: Vec<_> = page
                .entries
                .into_iter()
                .filter(|entry| {
                    prefix.as_ref().is_none_or(|prefix| {
                        entry.path == *prefix
                            || entry
                                .path
                                .strip_prefix(prefix)
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    }) && contains
                        .as_ref()
                        .is_none_or(|text| entry.path.contains(text))
                })
                .collect();
            print_json(
                &json!({"ok": true, "revision": revision, "entries": entries,
                "scanned": scanned, "next": page.next, "complete": page.next.is_none()}),
            );
        }
        Command::Stat { revision, path } => {
            let engine = Engine::open(&cli.data_dir)?;
            let entry = agentscm::store::SourceReader::new(&engine).stat_file(&revision, &path)?;
            print_json(&json!({"ok": true, "revision": revision, "path": path,
                "state": if entry.is_some() { "present" } else { "nonexistent" }, "entry": entry}));
        }
        Command::Read { revision, path } => {
            let engine = Engine::open(&cli.data_dir)?;
            let (kind, content) =
                agentscm::store::SourceReader::new(&engine).read_entry(&revision, &path)?;
            if content.len() > MAX_OUTPUT_BYTES {
                return Err(Error::Limit(format!(
                    "CLI source response exceeds {MAX_OUTPUT_BYTES} bytes"
                )));
            }
            print_json(
                &json!({"ok": true, "revision": revision, "path": path, "kind": kind, "content": content}),
            );
        }
        Command::Diff { left, right, limit } => {
            require_limit(limit, MAX_DIFF_LIMIT, "diff")?;
            let engine = Engine::open(&cli.data_dir)?;
            let diff = engine.diff(&left, &right, limit)?;
            print_json(&json!({"ok": true, "diff": diff}));
        }
        Command::DiffPage {
            left,
            right,
            cursor,
            limit,
        } => {
            let engine = Engine::open(&cli.data_dir)?;
            let cursor: Option<String> = cursor.as_deref().map(read_json).transpose()?;
            let page = engine.diff_page(&left, &right, cursor.as_deref(), limit)?;
            print_page(&json!({"ok":true,"page":page}))?;
        }
        Command::History {
            revision,
            cursor,
            limit,
        } => {
            let engine = Engine::open(&cli.data_dir)?;
            let cursor: Option<String> = cursor.as_deref().map(read_json).transpose()?;
            let page =
                agentscm::history::history_page(&engine, &revision, cursor.as_deref(), limit)?;
            print_page(&json!({"ok":true,"page":page}))?;
        }
        Command::Relation {
            left,
            right,
            cursor,
            limit,
        } => {
            let engine = Engine::open(&cli.data_dir)?;
            let cursor: Option<String> = cursor.as_deref().map(read_json).transpose()?;
            let page =
                agentscm::history::relation_page(&engine, &left, &right, cursor.as_deref(), limit)?;
            print_page(&json!({"ok":true,"page":page}))?;
        }
        Command::Approve { input } => {
            let approval: Approval = read_json(&input)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            authority.approve(&approval)?;
            print_json(&json!({"ok": true, "approval_id": approval.approval_id}));
        }
        Command::Revoke { approval_id } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            authority.revoke_approval(&approval_id)?;
            print_json(&json!({"ok": true, "approval_id": approval_id, "revoked": true}));
        }
        Command::Policy { advance } => {
            let mut authority = Authority::open(&cli.data_dir)?;
            let epoch = match advance {
                Some(expected) => authority.advance_policy(expected)?,
                None => authority.policy_epoch()?,
            };
            print_json(&json!({"ok": true, "policy_epoch": epoch}));
        }
        Command::Target { name } => {
            let authority = Authority::open(&cli.data_dir)?;
            let target = authority.target(&name)?;
            print_json(&json!({"ok": true, "name": name, "target": target}));
        }
        Command::Integrate { input } => {
            let request: IntegrateRequest = read_json(&input)?;
            let engine = Engine::open(&cli.data_dir)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let event = authority.integrate(&engine, &request)?;
            print_json(&json!({"ok": true, "event": event}));
        }
        Command::InitializeTarget { input } => {
            let request: IntegrateRequest = read_json(&input)?;
            let engine = Engine::open(&cli.data_dir)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let event = authority.initialize_target(&engine, &request)?;
            print_json(&json!({"ok": true, "event": event}));
        }
        Command::PrepareLanding { input } => {
            let request: PrepareLandingRequest = read_json(&input)?;
            let engine = Engine::open(&cli.data_dir)?;
            let mut authority = Authority::open(&cli.data_dir)?;
            let landing = authority.prepare_landing(&engine, &request)?;
            print_json(&json!({"ok": true, "landing": landing}));
        }
        Command::Events { after, limit } => {
            require_limit(limit, MAX_EVENT_LIMIT, "events")?;
            let authority = Authority::open(&cli.data_dir)?;
            let events = authority.events(after, limit)?;
            print_json(&json!({"ok": true, "events": events}));
        }
        Command::Serve {
            bind,
            max_concurrency,
            collisions_repo,
            timeline_workspace,
            browser_origin,
            public,
        } => {
            server::serve(
                cli.data_dir,
                bind,
                public,
                max_concurrency,
                collisions_repo,
                timeline_workspace,
                browser_origin,
            )
            .await?;
        }
    }
    Ok(RunExit::Completed)
}

fn telemetry_feature(command: &Command) -> Option<telemetry::Feature> {
    use telemetry::Feature;
    match command {
        Command::Quickstart(_) => Some(Feature::Quickstart),
        Command::Uninstall(_) => Some(Feature::Uninstall),
        Command::InitRepo(_) => Some(Feature::InitRepo),
        #[cfg(unix)]
        Command::Collisions(_) => Some(Feature::Collisions),
        Command::Autopsy(_) => Some(Feature::Autopsy),
        Command::Intent { command } if intent_cli::telemetry_action(command) => {
            Some(Feature::Intent)
        }
        Command::Work { command } if work_cli::telemetry_action(command) => Some(Feature::Work),
        Command::Hub { command } if hub_cli::telemetry_action(command) => Some(Feature::Hub),
        Command::Team { command } if team_cli::telemetry_action(command) => Some(Feature::Team),
        Command::SkillWorkflow { command } if skill_workflow_cli::telemetry_action(command) => {
            Some(Feature::SkillWorkflow)
        }
        #[cfg(unix)]
        Command::Setup { command } if setup_cli::telemetry_action(command) => Some(Feature::Setup),
        #[cfg(unix)]
        Command::Workflow { command } if guided_cli::telemetry_action(command) => {
            Some(Feature::Workflow)
        }
        #[cfg(unix)]
        Command::GitSnapshot { command } if git_snapshot_cli::telemetry_action(command) => {
            Some(Feature::GitBridge)
        }
        #[cfg(unix)]
        Command::GitRemote(args) if git_remote_cli::telemetry_action(args) => {
            Some(Feature::GitBridge)
        }
        #[cfg(unix)]
        Command::GitExport { command } if git_export_cli::telemetry_action(command) => {
            Some(Feature::GitBridge)
        }
        #[cfg(unix)]
        Command::GitReconcile { command } if git_reconcile_cli::telemetry_action(command) => {
            Some(Feature::GitBridge)
        }
        #[cfg(unix)]
        Command::Workspace { command } if workflow_cli::workspace_telemetry_action(command) => {
            Some(Feature::NativeSource)
        }
        #[cfg(unix)]
        Command::Sync { .. } => Some(Feature::NativeSource),
        #[cfg(unix)]
        Command::Merge { command } if merge_cli::telemetry_action(command) => {
            Some(Feature::NativeSource)
        }
        Command::Policy { advance: Some(_) } => Some(Feature::NativeSource),
        Command::Init
        | Command::Start { .. }
        | Command::Context { .. }
        | Command::CheckRecipe { .. }
        | Command::CheckSourcePaths { .. }
        | Command::CheckRun { .. }
        | Command::CheckCancel { .. }
        | Command::CheckRecover { .. }
        | Command::Checkpoint { .. }
        | Command::Approve { .. }
        | Command::Revoke { .. }
        | Command::Integrate { .. }
        | Command::InitializeTarget { .. }
        | Command::PrepareLanding { .. } => Some(Feature::NativeSource),
        #[cfg(unix)]
        Command::IsolatedCheckRecipe { .. }
        | Command::IsolatedCheckCreate { .. }
        | Command::IsolatedCheckRun { .. }
        | Command::IsolatedCheckCancel { .. }
        | Command::IsolatedCheckRecover { .. } => Some(Feature::NativeSource),
        _ => None,
    }
}

fn verified_install_command(command: &Command) -> bool {
    !matches!(
        command,
        Command::Demo(_)
            | Command::Hook(_)
            | Command::Telemetry {
                command: telemetry_cli::TelemetryCommand::Off
                    | telemetry_cli::TelemetryCommand::Send
            }
    )
}

fn require_limit(value: usize, maximum: usize, operation: &str) -> Result<()> {
    if value == 0 || value > maximum {
        return Err(Error::Limit(format!(
            "{operation} limit must be between 1 and {maximum}"
        )));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_bytes(path, MAX_INPUT_BYTES)?)?)
}

fn read_context(path: &Path) -> Result<ReviewContext> {
    ReviewContext::from_slice(&read_bytes(path, MAX_REVIEW_CONTEXT_BYTES as u64)?)
}

fn read_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let mut reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(path)?)
    };
    let mut bytes = Vec::new();
    reader.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(Error::Limit(format!("JSON input exceeds {maximum} bytes")));
    }
    Ok(bytes)
}

/// The collector requires a private, controller-owned parent for snapshots and Docker config.
#[cfg(unix)]
fn private_execution_dir(data_dir: &Path) -> Result<PathBuf> {
    // The collector requires absolute paths; the default data directory is relative.
    let path = std::fs::canonicalize(data_dir)?.join("execution");
    match DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::Invalid(
                    "execution directory must be an owner-private directory".into(),
                ));
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

/// A fresh empty private controller directory, removed on drop.
#[cfg(unix)]
struct PrivateExecutionDirectory(PathBuf);

#[cfg(unix)]
impl PrivateExecutionDirectory {
    fn create(work_dir: &Path) -> Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| Error::Invalid(format!("randomness unavailable: {error}")))?;
        let path = work_dir.join(format!(
            "controller-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

#[cfg(unix)]
impl Drop for PrivateExecutionDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn optional_backend(
    docker: Option<PathBuf>,
    socket: Option<PathBuf>,
    work_dir: &Path,
) -> Result<Option<(PrivateExecutionDirectory, OciBackend)>> {
    docker
        .zip(socket)
        .map(|(docker, socket)| {
            let config = PrivateExecutionDirectory::create(work_dir)?;
            let backend = OciBackend::new(&docker, &socket, &config.0)?;
            Ok((config, backend))
        })
        .transpose()
}

#[cfg(unix)]
fn print_command_attempt(
    authority: &Authority,
    attempt: &agentscm::execution::CommandAttempt,
) -> Result<()> {
    let record = attempt
        .record_digest
        .as_deref()
        .map(|digest| authority.command_record(digest))
        .transpose()?;
    print_json(&json!({"ok":true,"attempt":attempt,"record":record}));
    Ok(())
}

fn print_json(value: &Value) {
    println!("{value}");
}

fn print_page(value: &Value) -> Result<()> {
    let output = serde_json::to_string(value)?;
    if output.len() > 32 * 1024 * 1024 {
        return Err(Error::Limit("CLI page response exceeds 32 MiB".into()));
    }
    println!("{output}");
    Ok(())
}

fn print_error(error: &Error) {
    let kind = match error {
        Error::MissingObject(_) => "unavailable",
        Error::NotFound(message) if message.starts_with("object: ") => "unavailable",
        Error::NotFound(_) => "nonexistent",
        Error::SourceDenied => "denied",
        Error::SourceIndeterminate => "indeterminate",
        Error::Limit(_) => "limit",
        Error::Conflict(_) => "conflict",
        Error::Invalid(_) | Error::UnsupportedGitVersion { .. } => "invalid",
        Error::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => "denied",
        Error::Io(_) | Error::Json(_) | Error::Sql(_) => "failed",
    };
    eprintln!(
        "{}",
        json!({"ok": false, "error": error.to_string(), "error_kind": kind})
    );
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reconciliation_read_commands_never_initialize_a_missing_data_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let data = temporary.path().join("missing-data");
        for command in ["status", "lookup"] {
            let mut arguments = vec![
                "agentscm",
                "--data-dir",
                data.to_str().unwrap(),
                "git-reconcile",
                command,
            ];
            if command == "lookup" {
                arguments.push("not-retained");
            }
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(run(cli).await.is_err());
            assert!(
                !data.exists(),
                "read dispatch must not eagerly open writable stores"
            );
        }
    }
}
