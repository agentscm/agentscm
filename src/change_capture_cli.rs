//! CLI transport for caller-asserted local capture. Domain behavior lives in
//! `agentscm::change_capture`.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentscm::change_capture::claude_code::{self, CaptureRequest};
use agentscm::change_capture::{
    ConfiguredRecipeReconcileRequest, ConfiguredRecipeRunRequest, DeferredReasonV1,
    GitSourceRequest, LocalSpool, ObservedCommandRequest, ObservedStop, RecipeReconcileOutcome,
    export_spool_archive, inspect_spool, reconcile_retained_configured_recipe,
    run_configured_recipe_with_cancellation, run_observed_command,
};
use agentscm::change_record::{ActionOutcomeV1, CaptureModeV1, CoverageV1};
use agentscm::execution::Cancellation;
use agentscm::{Error, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::change_capture_client;

#[derive(Debug, Subcommand)]
pub enum ChangeCaptureCommand {
    /// Configure one checkout and add marked Claude hooks without replacing foreign entries.
    Configure(change_capture_client::ConfigureArgs),
    /// Disable one checkout, retain its spool lineage, and remove only AgentSCM's marked hooks.
    Unconfigure(change_capture_client::UnconfigureArgs),
    /// Inspect, restore, or prune bounded private Claude settings recovery entries.
    SettingsRecovery(change_capture_client::SettingsRecoveryArgs),
    /// Read one concise shared change record without running a model or mutating Hub state.
    Show(ShowArgs),
    /// Report one checkout's capture binding and inspect its spool without recovery or mutation.
    Status(StatusArgs),
    /// Inspect one private spool without recovery, cleanup, delivery, or authority.
    Inspect(InspectArgs),
    /// Copy one private spool into a non-resumable mode-0700 local archive.
    Export(ExportArgs),
    /// Run one exact configured verification recipe for the current Claude session.
    RunRecipe(RunRecipeArgs),
    /// Reconcile one retained configured-recipe attempt without running its command.
    ReconcileRecipe(ReconcileRecipeArgs),
    /// Consume one Claude Code native hook payload from stdin without blocking the host session.
    HookClaude,
    /// Import the visible portion of one explicitly selected Claude Code session log.
    ImportClaude(ImportClaudeArgs),
    /// Run one local command after durably binding source, recipe and setup.
    Observe(ObserveArgs),
    /// Print a bounded page of shareable pending envelopes; local bodies stay in the spool.
    Pending(PendingArgs),
    /// Print bounded sanitized hooks that remain explicitly unassociated with Hub work.
    Deferred(PendingArgs),
    /// List bounded private crash debris for explicit inspection.
    Quarantine(PendingArgs),
    /// Delete only exact quarantine names returned by a prior inspection.
    PruneQuarantine(PruneQuarantineArgs),
    /// Reclaim acknowledged envelope bytes while retaining replay/idempotency facts.
    Compact(CompactArgs),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CaptureModeArg {
    MetadataOnly,
    LocalConversation,
}

impl From<CaptureModeArg> for CaptureModeV1 {
    fn from(value: CaptureModeArg) -> Self {
        match value {
            CaptureModeArg::MetadataOnly => Self::MetadataOnly,
            CaptureModeArg::LocalConversation => Self::LocalConversation,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CoverageArg {
    Complete,
    Partial,
    Unknown,
}

impl From<CoverageArg> for CoverageV1 {
    fn from(value: CoverageArg) -> Self {
        match value {
            CoverageArg::Complete => Self::Complete,
            CoverageArg::Partial => Self::Partial,
            CoverageArg::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Canonical Git checkout with an AgentSCM client connection.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    work_id: String,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Canonical Git checkout with a retained capture binding.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Absolute private spool path. Inspection never runs recovery or mutates the source.
    #[arg(long)]
    spool: PathBuf,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Absolute private spool path. Export never runs recovery or mutates the source and refuses any file larger than 64 MiB.
    #[arg(long)]
    spool: PathBuf,
    /// New private archive directory; it must not already exist.
    #[arg(long)]
    destination: PathBuf,
}

#[derive(Debug, Args)]
pub struct RunRecipeArgs {
    /// Exact configured recipe identity. Commands and result claims cannot be overridden here.
    #[arg(long)]
    id: String,
}

#[derive(Debug, Args)]
pub struct ReconcileRecipeArgs {
    /// Retained configured recipe identity. Current recipe bytes are not required or accepted.
    #[arg(long)]
    id: String,
}

#[derive(Debug, Args)]
pub struct ImportClaudeArgs {
    /// Absolute private spool directory outside repository source.
    #[arg(long)]
    spool: PathBuf,
    /// Explicit Claude Code JSONL file selected by the local operator.
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    work_id: String,
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    generation: i64,
    #[arg(long)]
    session_id: String,
    #[arg(long)]
    parent_session_id: Option<String>,
    #[arg(long)]
    operation_prefix: String,
    #[arg(long, value_enum, default_value = "metadata-only")]
    capture_mode: CaptureModeArg,
}

#[derive(Debug, Args)]
pub struct ObserveArgs {
    /// Absolute private spool directory outside repository source.
    #[arg(long)]
    spool: PathBuf,
    /// Canonical absolute Git worktree root.
    #[arg(long)]
    repo: PathBuf,
    /// Existing AgentSCM/Hub repository identity, not a local path.
    #[arg(long)]
    repository: String,
    /// Opaque configured worktree identity, not a local path.
    #[arg(long)]
    worktree_id: String,
    #[arg(long, default_value = "origin/main")]
    base_ref: String,
    #[arg(long)]
    work_id: String,
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    generation: i64,
    #[arg(long)]
    session_id: String,
    #[arg(long)]
    action_id: String,
    #[arg(long)]
    start_operation_id: String,
    #[arg(long)]
    finish_operation_id: String,
    /// Lowercase SHA-256 of the effective agent/tool setup.
    #[arg(long)]
    setup_digest: String,
    #[arg(long, value_enum, default_value = "partial")]
    coverage: CoverageArg,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=300))]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 1024 * 1024, value_parser = clap::value_parser!(u64).range(256..=16 * 1024 * 1024))]
    max_output_bytes: u64,
    /// Absolute executable and its arguments. Use `--` before this command.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PendingArgs {
    #[arg(long)]
    spool: PathBuf,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=100))]
    limit: u64,
}

#[derive(Debug, Args)]
pub struct CompactArgs {
    #[arg(long)]
    spool: PathBuf,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=100))]
    limit: u64,
}

#[derive(Debug, Args)]
pub struct PruneQuarantineArgs {
    #[arg(long)]
    spool: PathBuf,
    #[arg(required = true, num_args = 1..=100)]
    names: Vec<String>,
}

pub async fn run(command: ChangeCaptureCommand) -> Result<i32> {
    match command {
        ChangeCaptureCommand::Configure(args) => {
            println!("{}", change_capture_client::configure(args)?);
            Ok(0)
        }
        ChangeCaptureCommand::Unconfigure(args) => {
            println!("{}", change_capture_client::unconfigure(args)?);
            Ok(0)
        }
        ChangeCaptureCommand::SettingsRecovery(args) => {
            println!("{}", change_capture_client::settings_recovery(args)?);
            Ok(0)
        }
        ChangeCaptureCommand::Show(args) => {
            let root = crate::client_config::canonical_root(&args.root)?;
            println!(
                "{}",
                serde_json::to_string(
                    &change_capture_client::fetch_change_record(&root, &args.work_id).await?
                )
                .map_err(|_| Error::Invalid("cannot encode change record".into()))?
            );
            Ok(0)
        }
        ChangeCaptureCommand::Status(args) => {
            println!(
                "{}",
                serde_json::to_string(&change_capture_client::capture_status(&args.root)?)?
            );
            Ok(0)
        }
        ChangeCaptureCommand::Inspect(args) => inspect(args),
        ChangeCaptureCommand::Export(args) => export(args),
        ChangeCaptureCommand::RunRecipe(args) => run_recipe(args).await,
        ChangeCaptureCommand::ReconcileRecipe(args) => reconcile_recipe(args).await,
        ChangeCaptureCommand::HookClaude => hook_claude().await,
        ChangeCaptureCommand::ImportClaude(args) => import_claude(args),
        ChangeCaptureCommand::Observe(args) => observe(args),
        ChangeCaptureCommand::Pending(args) => pending(args),
        ChangeCaptureCommand::Deferred(args) => deferred(args),
        ChangeCaptureCommand::Quarantine(args) => quarantine(args),
        ChangeCaptureCommand::PruneQuarantine(args) => prune_quarantine(args),
        ChangeCaptureCommand::Compact(args) => compact(args),
    }
}

async fn hook_claude() -> Result<i32> {
    const MAX_HOOK_BYTES: u64 = 1024 * 1024;
    let mut input = Vec::new();
    if std::io::stdin()
        .take(MAX_HOOK_BYTES + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() as u64 > MAX_HOOK_BYTES
    {
        eprintln!("AgentSCM local capture warning: native hook input was unavailable");
        return Ok(0);
    }
    let payload: serde_json::Value = match serde_json::from_slice(&input) {
        Ok(payload) => payload,
        Err(_) => {
            eprintln!("AgentSCM local capture warning: native hook input was invalid");
            return Ok(0);
        }
    };
    let binding = match change_capture_client::binding_for_payload(&payload) {
        Ok(Some(binding)) => binding,
        Ok(None) => return Ok(0),
        Err(_) => {
            eprintln!("AgentSCM local capture warning: configured checkout binding is unavailable");
            return Ok(0);
        }
    };
    let spool = match change_capture_client::open_bound_spool(&binding) {
        Ok(spool) => spool,
        Err(_) => {
            eprintln!("AgentSCM local capture warning: private spool is unavailable");
            return Ok(0);
        }
    };
    let session_id = match payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        Some(session_id) => session_id.to_owned(),
        None => {
            eprintln!("AgentSCM local capture warning: native session identity is missing");
            return Ok(0);
        }
    };
    let source = GitSourceRequest {
        repository: binding.repository.clone(),
        worktree_id: binding.worktree_id.clone(),
        repository_path: binding.root.clone(),
        base_ref: binding.base_ref.clone(),
    };
    let actions = claude_code::ActionCaptureConfig {
        source,
        setup_digest: binding.setup_digest.clone(),
        rules: binding.action_rules.clone(),
        managed_invocations: match change_capture_client::managed_recipe_invocations(&binding) {
            Ok(invocations) => invocations,
            Err(_) => {
                eprintln!(
                    "AgentSCM local capture warning: configured recipe identities are unavailable"
                );
                return Ok(0);
            }
        },
    };
    let target = match change_capture_client::resolve_target(&binding, &session_id).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            let retained = claude_code::deferred_native_hook(
                &input,
                DeferredReasonV1::NoCurrentAuthorizedWork,
                &actions,
                &spool,
            );
            let _ = change_capture_client::deliver_pending(&binding.root, &spool).await;
            if retained.is_err() {
                eprintln!(
                    "AgentSCM local capture warning: native session has no current authorized work and its sanitized deferred fact could not be retained"
                );
            } else {
                eprintln!(
                    "AgentSCM local capture warning: native session has no current authorized work; a sanitized fact remains unassociated"
                );
            }
            return Ok(0);
        }
        Err(_) => {
            let retained = claude_code::deferred_native_hook(
                &input,
                DeferredReasonV1::TargetLookupUnavailable,
                &actions,
                &spool,
            );
            let _ = change_capture_client::deliver_pending(&binding.root, &spool).await;
            if retained.is_err() {
                eprintln!(
                    "AgentSCM local capture warning: current authorized work and sanitized deferred retention are unavailable"
                );
            } else {
                eprintln!(
                    "AgentSCM local capture warning: current authorized work could not be resolved; a sanitized fact remains unassociated"
                );
            }
            return Ok(0);
        }
    };
    let request = CaptureRequest {
        operation_prefix: "hook".into(),
        work_id: target.work_id,
        generation: target.generation,
        observed_session_id: session_id,
        canonical_session_id: target.session_id,
        parent_session_id: payload
            .get("parent_session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        capture_mode: binding.capture_mode,
    };
    match claude_code::capture_native_hook(&input, &request, &actions, &spool) {
        Ok(report) => {
            for warning in report.warnings {
                eprintln!("AgentSCM local capture warning: {warning}");
            }
        }
        Err(_) => eprintln!("AgentSCM local capture warning: native event was not retained"),
    }
    if let (Some(history_root), Some(transcript_path)) = (
        binding.claude_history_root.as_deref(),
        payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(std::path::Path::new),
    ) {
        let history_from_start = payload.get("hook_event_name").and_then(Value::as_str)
            == Some("SessionStart")
            && payload.get("source").and_then(Value::as_str) == Some("startup");
        if claude_code::capture_provider_usage(
            transcript_path,
            history_root,
            &request,
            history_from_start,
            &spool,
        )
        .is_err()
        {
            eprintln!(
                "AgentSCM local capture warning: provider usage remains unknown because the bound session history could not be read"
            );
        }
    }
    match change_capture_client::deliver_pending(&binding.root, &spool).await {
        Ok(report) if report.blocked > 0 => eprintln!(
            "AgentSCM local capture warning: {} retained record(s) remain blocked by current Hub authorization or work generation",
            report.blocked
        ),
        Ok(_) => {}
        Err(_) => {
            eprintln!("AgentSCM local capture warning: retained records remain pending delivery")
        }
    }
    if payload.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart") {
        match change_capture_client::session_start_context(&binding) {
            Ok(Some(additional_context)) => {
                println!("{}", session_start_output(additional_context)?)
            }
            Ok(None) => {}
            Err(_) => eprintln!(
                "AgentSCM local capture warning: configured recipe guidance is unavailable"
            ),
        }
    }
    Ok(0)
}

fn session_start_output(additional_context: String) -> Result<String> {
    serde_json::to_string(&json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": additional_context,
        }
    }))
    .map_err(Into::into)
}

async fn run_recipe(args: RunRecipeArgs) -> Result<i32> {
    let (binding, spool, request) = configured_recipe_request(&args.id).await?;
    let cancellation = Cancellation::default();
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let signals = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            cancellation.cancel();
        }
    });
    let observed = tokio::task::block_in_place(|| {
        run_configured_recipe_with_cancellation(request, &spool, &cancellation)
    });
    signals.abort();
    let observed = observed.map_err(capture_error)?;
    if let Err(error) = change_capture_client::deliver_pending(&binding.root, &spool).await {
        eprintln!("AgentSCM local capture warning: receipt remains pending delivery ({error})");
    }
    println!(
        "{}",
        serde_json::to_string(&json!({
            "action_id": observed.finish.action_id,
            "outcome": observed.finish.outcome,
            "result": observed.finish.result,
            "assurance": observed.finish.assurance,
            "coverage": observed.finish.coverage,
            "drift": observed.finish.drift,
            "exit_code": observed.exit_code,
            "signal": observed.signal,
            "stop": format!("{:?}", observed.stop).to_ascii_lowercase(),
            "artifact_availability": observed.finish.output_artifacts.iter().map(|artifact| artifact.availability).collect::<Vec<_>>(),
            "storage_warnings": observed.storage_warnings,
        }))?
    );
    Ok(recipe_exit_status(
        observed.finish.outcome,
        observed.stop,
        observed.exit_code,
    ))
}

fn recipe_exit_status(outcome: ActionOutcomeV1, stop: ObservedStop, exit_code: Option<i32>) -> i32 {
    match (stop, exit_code, outcome) {
        (ObservedStop::Exited, Some(code @ 0..=255), _) => code,
        (ObservedStop::Exited, None, ActionOutcomeV1::Succeeded) => 0,
        (ObservedStop::Exited, None, ActionOutcomeV1::Failed) => 1,
        _ => 2,
    }
}

async fn configured_recipe_request(
    recipe_id: &str,
) -> Result<(
    change_capture_client::CaptureBinding,
    LocalSpool,
    ConfiguredRecipeRunRequest,
)> {
    let observed_session_id = validated_native_session_id()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let binding = change_capture_client::binding_for_root(&cwd)?;
    if cwd != binding.root {
        return Err(Error::Invalid(
            "configured recipe must run from the configured repository root".into(),
        ));
    }
    let recipe = change_capture_client::configured_recipe(&binding, recipe_id)?.clone();
    let managed_invocation = change_capture_client::managed_recipe_invocation(&binding, recipe_id)?;
    let target = change_capture_client::resolve_target(&binding, &observed_session_id)
        .await?
        .ok_or_else(|| {
            Error::Invalid("Claude session has no current authorized work target".into())
        })?;
    let spool = change_capture_client::open_bound_spool(&binding)?;
    let request = ConfiguredRecipeRunRequest {
        work_id: target.work_id,
        generation: target.generation,
        observed_session_id,
        session_id: target.session_id,
        source: GitSourceRequest {
            repository: binding.repository.clone(),
            worktree_id: binding.worktree_id.clone(),
            repository_path: binding.root.clone(),
            base_ref: binding.base_ref.clone(),
        },
        setup_digest: binding.setup_digest.clone(),
        recipe,
        managed_invocation,
    };
    Ok((binding, spool, request))
}

fn validated_native_session_id() -> Result<String> {
    if std::env::var_os("CLAUDE_CODE_CHILD_SESSION").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err(Error::Invalid(
            "configured recipes run only inside a Claude Code child session".into(),
        ));
    }
    let observed_session_id = std::env::var("CLAUDE_CODE_SESSION_ID")
        .map_err(|_| Error::Invalid("Claude Code session identity is unavailable".into()))?;
    if observed_session_id.is_empty()
        || observed_session_id.len() > 255
        || !observed_session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(Error::Invalid(
            "Claude Code session identity is invalid".into(),
        ));
    }
    Ok(observed_session_id)
}

async fn reconcile_recipe(args: ReconcileRecipeArgs) -> Result<i32> {
    let observed_session_id = validated_native_session_id()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let binding = change_capture_client::binding_for_reconciliation(&cwd)?;
    if cwd != binding.root {
        return Err(Error::Invalid(
            "configured recipe must reconcile from the configured repository root".into(),
        ));
    }
    let target = change_capture_client::resolve_target(&binding, &observed_session_id)
        .await?
        .ok_or_else(|| {
            Error::Invalid("Claude session has no current authorized work target".into())
        })?;
    let spool = change_capture_client::open_bound_spool(&binding)?;
    let outcome = reconcile_retained_configured_recipe(
        ConfiguredRecipeReconcileRequest {
            work_id: target.work_id,
            source: GitSourceRequest {
                repository: binding.repository.clone(),
                worktree_id: binding.worktree_id.clone(),
                repository_path: binding.root.clone(),
                base_ref: binding.base_ref.clone(),
            },
            recipe_id: args.id,
        },
        &spool,
    )
    .map_err(capture_error)?;
    if matches!(outcome, RecipeReconcileOutcome::Reconciled(_))
        && let Err(error) = change_capture_client::deliver_pending(&binding.root, &spool).await
    {
        eprintln!("AgentSCM local capture warning: receipt remains pending delivery ({error})");
    }
    let unresolved = matches!(
        outcome,
        RecipeReconcileOutcome::Busy { .. } | RecipeReconcileOutcome::Unresolved { .. }
    );
    println!(
        "{}",
        serde_json::to_string(&match outcome {
            RecipeReconcileOutcome::None => json!({"status": "none"}),
            RecipeReconcileOutcome::Busy { fence_id } => {
                json!({
                    "status": "busy",
                    "reason": "checkout_in_use",
                    "scope": "configured_checkout",
                    "fence_id": fence_id,
                })
            }
            RecipeReconcileOutcome::ClearedPreSpawn { action_id } => {
                json!({"status": "cleared_pre_spawn", "action_id": action_id})
            }
            RecipeReconcileOutcome::Unresolved { action_id, phase } => {
                json!({"status": "unresolved", "action_id": action_id, "phase": phase})
            }
            RecipeReconcileOutcome::Reconciled(finish) => json!({
                "status": "reconciled",
                "action_id": finish.action_id,
                "outcome": finish.outcome,
                "result": finish.result,
                "assurance": finish.assurance,
                "coverage": finish.coverage,
                "drift": finish.drift,
            }),
        })?
    );
    Ok(if unresolved { 2 } else { 0 })
}

fn import_claude(args: ImportClaudeArgs) -> Result<i32> {
    let metadata = fs::symlink_metadata(&args.input)?;
    if !args.input.is_absolute()
        || !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
    {
        return Err(Error::Invalid(
            "Claude Code import must be an absolute regular JSONL file".into(),
        ));
    }
    let spool = LocalSpool::open(&args.spool).map_err(capture_error)?;
    let request = CaptureRequest {
        operation_prefix: args.operation_prefix,
        work_id: args.work_id,
        generation: args.generation,
        observed_session_id: args.session_id.clone(),
        canonical_session_id: args.session_id,
        parent_session_id: args.parent_session_id,
        capture_mode: args.capture_mode.into(),
    };
    let report =
        claude_code::import_jsonl(BufReader::new(File::open(args.input)?), &request, &spool)
            .map_err(capture_error)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "batches": report.batches,
            "events": report.events,
            "gaps": report.gaps,
            "private_blocks_ignored": report.private_blocks_ignored,
            "conversation_content": match request.capture_mode {
                CaptureModeV1::MetadataOnly => "not_retained",
                CaptureModeV1::LocalConversation => "local_only",
            },
        }))?
    );
    Ok(0)
}

fn observe(args: ObserveArgs) -> Result<i32> {
    let spool = LocalSpool::open(&args.spool).map_err(capture_error)?;
    let observed = run_observed_command(
        ObservedCommandRequest {
            start_operation_id: args.start_operation_id,
            finish_operation_id: args.finish_operation_id,
            work_id: args.work_id,
            generation: args.generation,
            session_id: args.session_id,
            action_id: args.action_id,
            source: GitSourceRequest {
                repository: args.repository,
                worktree_id: args.worktree_id,
                repository_path: args.repo,
                base_ref: args.base_ref,
            },
            argv: args.command,
            setup_digest: args.setup_digest,
            coverage: args.coverage.into(),
            timeout: Duration::from_secs(args.timeout_seconds),
            max_output_bytes: args.max_output_bytes,
        },
        &spool,
    )
    .map_err(capture_error)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "action_id": observed.finish.action_id,
            "outcome": observed.finish.outcome,
            "result": observed.finish.result,
            "assurance": observed.finish.assurance,
            "coverage": observed.finish.coverage,
            "drift": observed.finish.drift,
            "exit_code": observed.exit_code,
            "signal": observed.signal,
            "stop": format!("{:?}", observed.stop).to_ascii_lowercase(),
            "artifact_availability": observed.finish.output_artifacts.iter().map(|artifact| artifact.availability).collect::<Vec<_>>(),
            "storage_warnings": observed.storage_warnings,
        }))?
    );
    Ok(match (observed.finish.outcome, observed.stop) {
        (ActionOutcomeV1::Succeeded, ObservedStop::Exited) => 0,
        (ActionOutcomeV1::Failed, ObservedStop::Exited) => 1,
        _ => 2,
    })
}

fn pending(args: PendingArgs) -> Result<i32> {
    let inspection = inspect_spool(&args.spool).map_err(capture_error)?;
    let limit = args.limit as usize;
    let returned = inspection.pending.len().min(limit);
    let count = inspection.pending_count;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "mode": "inspect_only",
            "resumable": false,
            "authority": "none",
            "completeness": "unknown",
            "items": inspection.pending.into_iter().take(limit).collect::<Vec<_>>(),
            "count": {
                "present": count.present,
                "returned": returned,
                "omitted": count.present.saturating_sub(returned),
                "truncated": count.truncated || returned < count.present,
            },
        }))?
    );
    Ok(0)
}

fn deferred(args: PendingArgs) -> Result<i32> {
    let inspection = inspect_spool(&args.spool).map_err(capture_error)?;
    let limit = args.limit as usize;
    let returned = inspection.deferred.len().min(limit);
    let count = inspection.deferred_count;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "mode": "inspect_only",
            "resumable": false,
            "authority": "none",
            "completeness": "unknown",
            "items": inspection.deferred.into_iter().take(limit).collect::<Vec<_>>(),
            "count": {
                "present": count.present,
                "returned": returned,
                "omitted": count.present.saturating_sub(returned),
                "truncated": count.truncated || returned < count.present,
            },
        }))?
    );
    Ok(0)
}

fn quarantine(args: PendingArgs) -> Result<i32> {
    let inspection = inspect_spool(&args.spool).map_err(capture_error)?;
    let limit = args.limit as usize;
    let returned = inspection.quarantine.len().min(limit);
    let count = inspection.quarantine_count;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "mode": "inspect_only",
            "resumable": false,
            "authority": "none",
            "completeness": "unknown",
            "items": inspection.quarantine.into_iter().take(limit).collect::<Vec<_>>(),
            "count": {
                "present": count.present,
                "returned": returned,
                "omitted": count.present.saturating_sub(returned),
                "truncated": count.truncated || returned < count.present,
            },
        }))?
    );
    Ok(0)
}

fn inspect(args: InspectArgs) -> Result<i32> {
    let inspection = inspect_spool(&args.spool).map_err(capture_error)?;
    println!("{}", serde_json::to_string(&inspection)?);
    Ok(0)
}

fn export(args: ExportArgs) -> Result<i32> {
    let manifest = export_spool_archive(&args.spool, &args.destination).map_err(capture_error)?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(0)
}

fn prune_quarantine(args: PruneQuarantineArgs) -> Result<i32> {
    let spool = change_capture_client::open_spool_path(&args.spool)?;
    let report = spool.prune_quarantine(&args.names).map_err(capture_error)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "spool": spool.root(),
            "records": report.records,
            "bytes_reclaimed": report.bytes_reclaimed,
        }))?
    );
    Ok(0)
}

fn compact(args: CompactArgs) -> Result<i32> {
    let spool = change_capture_client::open_spool_path(&args.spool)?;
    let report = spool
        .compact_acknowledged(args.limit as usize)
        .map_err(capture_error)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "spool": spool.root(),
            "limit": args.limit,
            "envelopes": report.envelopes,
            "bytes_reclaimed": report.bytes_reclaimed,
        }))?
    );
    Ok(0)
}

fn capture_error(error: agentscm::change_capture::CaptureError) -> Error {
    Error::Invalid(format!("local capture: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{recipe_exit_status, session_start_output};
    use agentscm::change_capture::ObservedStop;
    use agentscm::change_record::ActionOutcomeV1;

    #[test]
    fn session_start_output_uses_the_native_hook_contract() {
        let output = session_start_output(
            "Run exactly: '/opt/agentscm' change-capture run-recipe --id 'unit-test'".into(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            "Run exactly: '/opt/agentscm' change-capture run-recipe --id 'unit-test'"
        );
        assert_eq!(value["hookSpecificOutput"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn completed_recipe_replay_has_a_stable_status_without_inventing_a_child_code() {
        assert_eq!(
            recipe_exit_status(ActionOutcomeV1::Succeeded, ObservedStop::Exited, None),
            0
        );
        assert_eq!(
            recipe_exit_status(ActionOutcomeV1::Failed, ObservedStop::Exited, None),
            1
        );
        assert_eq!(
            recipe_exit_status(ActionOutcomeV1::Incomplete, ObservedStop::Io, None),
            2
        );
        assert_eq!(
            recipe_exit_status(ActionOutcomeV1::Unknown, ObservedStop::Io, None),
            2
        );
    }
}
