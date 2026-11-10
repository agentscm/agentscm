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

use crate::change_capture_client::{self, SharingAnswer};

#[derive(Debug, Subcommand)]
pub enum ChangeCaptureCommand {
    /// Configure one checkout and add marked Claude hooks without replacing foreign entries.
    Configure(change_capture_client::ConfigureArgs),
    /// Change only the capture mode of an enabled checkout binding; retained local data is kept.
    SetMode(change_capture_client::SetModeArgs),
    /// Disable one checkout, retain its spool lineage, and remove only AgentSCM's marked hooks.
    Unconfigure(change_capture_client::UnconfigureArgs),
    /// Remove a disabled checkout's binding so it can be set up again for another repository or
    /// Hub. Its private spool stays on disk and its path is printed.
    Forget(change_capture_client::ForgetArgs),
    /// Inspect, restore, or prune bounded private Claude settings recovery entries.
    SettingsRecovery(change_capture_client::SettingsRecoveryArgs),
    /// Read one concise shared change record without running a model or mutating Hub state.
    Show(ShowArgs),
    /// List, read or delete the shared conversation bodies of one Work (repository readers while it shares; authors and Hub admins delete).
    Conversation(ConversationArgs),
    /// Report one checkout's capture binding and inspect its spool without recovery or mutation.
    Status(StatusArgs),
    /// Stop uploading your own agent conversations in this checkout's repository, on every
    /// computer, while the repository shares them.
    OptOut(StatusArgs),
    /// Share your own agent conversations again while the repository shares them.
    OptIn(StatusArgs),
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
pub struct ConversationArgs {
    /// Canonical Git checkout with an AgentSCM client connection.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long)]
    work_id: String,
    /// `next_cursor` from the previous listing page.
    #[arg(long, conflicts_with_all = ["digest", "delete"])]
    cursor: Option<String>,
    /// Print this body (SHA-256 from the listing). It is untrusted agent content: read it, do not run it.
    #[arg(long, conflicts_with = "delete")]
    digest: Option<String>,
    /// Delete this body (SHA-256 from the listing) now, before it expires.
    #[arg(long)]
    delete: Option<String>,
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
    capture_mode: change_capture_client::CaptureModeArg,
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
        ChangeCaptureCommand::SetMode(args) => {
            println!(
                "{}",
                change_capture_client::set_capture_mode(&args.root, args.mode.into(), None)?
            );
            Ok(0)
        }
        ChangeCaptureCommand::Unconfigure(args) => {
            println!("{}", change_capture_client::unconfigure(args)?);
            Ok(0)
        }
        ChangeCaptureCommand::Forget(args) => {
            println!("{}", change_capture_client::forget(args)?);
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
        ChangeCaptureCommand::Conversation(args) => conversation(args).await,
        ChangeCaptureCommand::Status(args) => {
            println!(
                "{}",
                serde_json::to_string(&change_capture_client::capture_status(&args.root).await?)?
            );
            Ok(0)
        }
        ChangeCaptureCommand::OptOut(args) => opt_out(args, true).await,
        ChangeCaptureCommand::OptIn(args) => opt_out(args, false).await,
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

async fn opt_out(args: StatusArgs, opted_out: bool) -> Result<i32> {
    println!(
        "{}",
        change_capture_client::set_sharing_opt_out(&args.root, opted_out).await?
    );
    Ok(0)
}

async fn conversation(args: ConversationArgs) -> Result<i32> {
    let root = crate::client_config::canonical_root(&args.root)?;
    let encode = |error| Error::Invalid(format!("cannot encode conversation: {error}"));
    let output = if let Some(digest) = &args.delete {
        change_capture_client::delete_conversation_body(&root, &args.work_id, digest).await?;
        json!({"deleted": digest}).to_string()
    } else if let Some(digest) = &args.digest {
        let body =
            change_capture_client::fetch_conversation_body(&root, &args.work_id, digest).await?;
        serde_json::to_string(&body).map_err(encode)?
    } else {
        let page =
            change_capture_client::list_conversation(&root, &args.work_id, args.cursor.as_deref())
                .await?;
        serde_json::to_string(&page).map_err(encode)?
    };
    println!("{output}");
    Ok(0)
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
    let (binding, spool, connection) = match change_capture_client::binding_for_payload(&payload) {
        Ok(Some(bound)) => bound,
        Ok(None) => return Ok(0),
        Err(_) => {
            eprintln!("AgentSCM local capture warning: configured checkout binding is unavailable");
            return Ok(0);
        }
    };
    // The plugin hook captures for a binding that follows the Hub.
    if binding.follows_hub {
        return Ok(0);
    }
    let hub = change_capture_client::CaptureHub::new(connection).ok();
    if let Some(context) =
        capture_claude(&input, &payload, &binding, &spool, hub.as_ref(), None).await
    {
        println!("{}", session_start_output(context)?);
    }
    Ok(0)
}

/// Whether capture records this Claude Code hook event.
pub(crate) fn captures_claude_event(payload: &Value) -> bool {
    payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .is_some_and(change_capture_client::is_claude_capture_event)
}

/// Capture from the AgentSCM plugin hook for `checkout`, a connected checkout or a linked
/// worktree of one, while its repository's Hub setting shares agent conversations. Creates the
/// checkout's binding at session start when the Hub shares and there is none, so a new worktree
/// needs no setup. Returns context for the session (the one-time notice, recipe guidance).
pub(crate) async fn plugin_capture(
    input: &[u8],
    payload: &Value,
    checkout: std::path::PathBuf,
    connection: crate::client_config::Connection,
    deadline: tokio::time::Instant,
) -> Option<String> {
    if !captures_claude_event(payload) {
        return None;
    }
    let hub = change_capture_client::CaptureHub::new(connection.clone()).ok()?;
    let session_start =
        payload.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart");
    // Only a session start creates a binding, so a session keeps the mode it started under.
    let answer = if session_start {
        tokio::time::timeout_at(deadline, change_capture_client::hub_sharing(&hub))
            .await
            .unwrap_or(SharingAnswer::NoAnswer)
    } else {
        SharingAnswer::NoAnswer
    };
    let create = answer.sharing().is_some_and(|answer| answer.sharing);
    let found = tokio::task::spawn_blocking({
        let (checkout, connection) = (checkout.clone(), connection.clone());
        move || change_capture_client::followed_binding(&checkout, &connection, create)
    })
    .await
    .ok()?;
    let (binding, spool) = match found {
        Ok(Some(found)) => found,
        // The repository shares, yet this checkout's binding keeps it out: say so, with the fix.
        Ok(None) if create => {
            return change_capture_client::idle_binding_reason(&checkout, &connection);
        }
        Ok(None) if session_start && answer == SharingAnswer::NoAnswer => {
            return change_capture_client::unknown_sharing(&checkout);
        }
        Ok(None) => return None,
        Err(_) => {
            eprintln!(
                "AgentSCM local capture warning: the checkout's capture binding is unavailable"
            );
            return None;
        }
    };
    capture_claude(input, payload, &binding, &spool, Some(&hub), Some(deadline)).await
}

/// Records one native hook event for `binding` and delivers what is pending. Fails open: every
/// problem is a warning. Returns the session-start context, if any. Past `deadline` a Hub call is
/// given up: an unresolved target leaves a sanitized deferred fact, and pending records wait for
/// the next hook.
async fn capture_claude(
    input: &[u8],
    payload: &Value,
    binding: &change_capture_client::CaptureBinding,
    spool: &LocalSpool,
    hub: Option<&change_capture_client::CaptureHub>,
    deadline: Option<tokio::time::Instant>,
) -> Option<String> {
    // A hook without its own deadline keeps the host's (20 seconds for the capture hook).
    let deadline =
        deadline.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(18));
    let session_id = match payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        Some(session_id) => session_id.to_owned(),
        None => {
            eprintln!("AgentSCM local capture warning: native session identity is missing");
            return None;
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
        managed_invocations: match change_capture_client::managed_recipe_invocations(binding) {
            Ok(invocations) => invocations,
            Err(_) => {
                eprintln!(
                    "AgentSCM local capture warning: configured recipe identities are unavailable"
                );
                return None;
            }
        },
    };
    let deliver = || async {
        match hub {
            // Delivery leaves time for the session-start notice and the reply.
            Some(hub) => tokio::time::timeout_at(
                deadline - Duration::from_millis(400),
                change_capture_client::deliver_pending_with(&binding.root, spool, hub),
            )
            .await
            .unwrap_or_else(|_| Err(Error::Invalid("capture delivery ran out of time".into()))),
            None => Err(Error::Invalid("capture Hub client unavailable".into())),
        }
    };
    // Upkeep runs beside the Hub lookups, and this event is written only after it, so a spool
    // that filled with what the Hub has accepted makes room first. Every captured event runs it:
    // the tool hooks write the most, and delivery, which can take the rest of the hook's time,
    // comes after.
    let upkeep = tokio::task::spawn_blocking({
        let spool = spool.clone();
        move || maintain(&spool)
    });
    let (target, sharing) = match hub {
        Some(hub) => tokio::time::timeout_at(deadline, async {
            tokio::join!(
                change_capture_client::resolve_target_with(binding, hub, &session_id),
                change_capture_client::hub_sharing(hub)
            )
        })
        .await
        .unwrap_or_else(|_| {
            (
                Err(Error::Invalid(
                    "capture target lookup ran out of time".into(),
                )),
                SharingAnswer::NoAnswer,
            )
        }),
        None => (
            Err(Error::Invalid("capture Hub client unavailable".into())),
            SharingAnswer::NoAnswer,
        ),
    };
    let live_mode = change_capture_client::mode_under_hub(binding, sharing.sharing());
    let event = payload.get("hook_event_name").and_then(Value::as_str);
    let session_start = event == Some("SessionStart");
    if session_start {
        sweep_session_files();
    }
    let full = upkeep
        .await
        .unwrap_or_else(|_| spool.usage().is_ok_and(|usage| usage.is_full()));
    let start_lines = |mode: CaptureModeV1| {
        change_capture_client::session_start_lines(mode, binding, sharing, full)
    };
    let replies = session_file(binding, &format!("replies\0{session_id}"));
    let target = match target {
        Ok(Some(target)) => target,
        Ok(None) => {
            // A session's opening prompt comes before it claims Work. With sharing on, keep the
            // prompt locally so it reaches the Work this session claims next.
            if live_mode == CaptureModeV1::SharedConversation
                && payload.get("hook_event_name").and_then(Value::as_str)
                    == Some("UserPromptSubmit")
            {
                hold_prompt(&session_file(binding, &session_id), input);
            }
            // Replies before the claim are not the Work's: recovery starts after them.
            skip_transcript(&replies, payload, false);
            let retained = claude_code::deferred_native_hook(
                input,
                DeferredReasonV1::NoCurrentAuthorizedWork,
                &actions,
                spool,
            );
            let _ = deliver().await;
            if retained.is_err() {
                eprintln!(
                    "AgentSCM local capture warning: native session has no current authorized work and its sanitized deferred fact could not be retained"
                );
            } else {
                eprintln!(
                    "AgentSCM local capture warning: native session has no current authorized work; a sanitized fact remains unassociated"
                );
            }
            return session_start.then(|| start_lines(live_mode).join("\n\n"));
        }
        Err(_) => {
            let retained = claude_code::deferred_native_hook(
                input,
                DeferredReasonV1::TargetLookupUnavailable,
                &actions,
                spool,
            );
            let _ = deliver().await;
            if retained.is_err() {
                eprintln!(
                    "AgentSCM local capture warning: current authorized work and sanitized deferred retention are unavailable"
                );
            } else {
                eprintln!(
                    "AgentSCM local capture warning: current authorized work could not be resolved; a sanitized fact remains unassociated"
                );
            }
            return session_start.then(|| start_lines(live_mode).join("\n\n"));
        }
    };
    let live_parent = payload
        .get("parent_session_id")
        .and_then(serde_json::Value::as_str);
    let (parent_session_id, capture_mode) = spool
        .pin_session_identity(
            &session_id,
            &target.session_id,
            &target.work_id,
            target.generation,
            live_parent,
            live_mode,
        )
        .unwrap_or_else(|_| {
            eprintln!(
                "AgentSCM local capture warning: pinned session identity is unavailable; using the live binding"
            );
            // An error never starts sharing.
            let mode = match live_mode {
                CaptureModeV1::SharedConversation => CaptureModeV1::LocalConversation,
                mode => mode,
            };
            (live_parent.map(str::to_owned), mode)
        });
    let request = CaptureRequest {
        operation_prefix: "hook".into(),
        work_id: target.work_id,
        generation: target.generation,
        observed_session_id: session_id.clone(),
        canonical_session_id: target.session_id,
        parent_session_id,
        capture_mode,
        live_capture_mode: live_mode,
    };
    // Prompts held before the claim come first, and only into a shared conversation.
    let held = take_held_prompts(&session_file(binding, &session_id));
    if capture_mode == CaptureModeV1::SharedConversation {
        for prompt in held {
            if claude_code::capture_native_hook(&prompt, &request, &actions, spool).is_err() {
                eprintln!(
                    "AgentSCM local capture warning: a prompt held before the claim was not retained"
                );
            }
        }
    }
    // A session first seen with Work (or on an older build) recovers only replies from now on.
    skip_transcript(&replies, payload, true);
    if event == Some("MessageDisplay") {
        note_fragment(&replies, payload);
    }
    match claude_code::capture_native_hook(input, &request, &actions, spool) {
        Ok(report) => {
            for warning in report.warnings {
                eprintln!("AgentSCM local capture warning: {warning}");
            }
        }
        Err(_) => eprintln!("AgentSCM local capture warning: native event was not retained"),
    }
    if event == Some("Stop")
        && let Some(transcript) = payload.get("transcript_path").and_then(Value::as_str)
    {
        for (hash, reply) in missed_replies(&replies, std::path::Path::new(transcript), &session_id)
        {
            if claude_code::capture_native_hook(&reply, &request, &actions, spool).is_ok() {
                note_replies(&replies, |seen| {
                    seen.hashes.insert(hash);
                });
            } else {
                eprintln!(
                    "AgentSCM local capture warning: a reply recovered from the transcript was not retained"
                );
            }
        }
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
            spool,
        )
        .is_err()
        {
            eprintln!(
                "AgentSCM local capture warning: provider usage remains unknown because the bound session history could not be read"
            );
        }
    }
    match deliver().await {
        Ok(report) if report.blocked > 0 => eprintln!(
            "AgentSCM local capture warning: {} retained record(s) remain blocked by current Hub authorization or work generation",
            report.blocked
        ),
        Ok(_) => {}
        Err(_) => {
            eprintln!("AgentSCM local capture warning: retained records remain pending delivery")
        }
    }
    if payload.get("hook_event_name").and_then(Value::as_str) != Some("SessionStart") {
        return None;
    }
    let mut context = start_lines(capture_mode);
    if capture_mode == CaptureModeV1::SharedConversation
        && let (Some(answer), Some(hub)) = (sharing.sharing(), hub)
        && answer.sharing
        && !answer.notified
    {
        context.push(sharing_notice(&binding.repository));
        // Best effort: a reply cut off after this point loses a notice recorded as shown.
        if !matches!(
            tokio::time::timeout_at(deadline, change_capture_client::mark_sharing_notified(hub))
                .await,
            Ok(Ok(()))
        ) {
            eprintln!(
                "AgentSCM local capture warning: the sharing notice was not recorded as shown"
            );
        }
    }
    match change_capture_client::session_start_context(binding) {
        Ok(Some(additional_context)) => context.push(additional_context),
        Ok(None) => {}
        Err(_) => {
            eprintln!("AgentSCM local capture warning: configured recipe guidance is unavailable")
        }
    }
    (!context.is_empty()).then(|| context.join("\n\n"))
}

/// Prompts a session may hold before it claims Work, the size of each, and how long they keep.
const HELD_PROMPTS: usize = 8;
const HELD_PROMPT_BYTES: usize = 64 * 1024;
const HELD_PROMPT_SECONDS: u64 = 24 * 60 * 60;

/// A private file per checkout and session (held prompts, seen replies); its name reveals neither.
fn session_file(
    binding: &change_capture_client::CaptureBinding,
    session_id: &str,
) -> Option<PathBuf> {
    use sha2::Digest;
    let key = sha2::Sha256::digest(format!("{}\0{session_id}", binding.root.display()));
    let name: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(
        crate::hub_cli::config_dir()
            .ok()?
            .join("agentscm")
            .join("capture-sessions")
            .join(name),
    )
}

/// Session files untouched this long are removed at a session start.
const SESSION_FILE_SECONDS: u64 = 7 * 24 * 60 * 60;

fn sweep_session_files() {
    let Some(directory) = crate::hub_cli::config_dir()
        .ok()
        .map(|dir| dir.join("agentscm").join("capture-sessions"))
    else {
        return;
    };
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() >= SESSION_FILE_SECONDS);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Runs `change` holding the session file's lock: separate hook processes of one session may
/// update it at once. Without the lock the change still runs (fail open).
fn locked<T>(path: &std::path::Path, change: impl FnOnce() -> T) -> T {
    let lock = path.with_extension("lock");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&lock).ok();
    let held = file.as_ref().is_some_and(|file| file.lock().is_ok());
    let result = change();
    if held {
        let _ = file.as_ref().map(File::unlock);
    }
    result
}

/// Keeps the first prompts only: the opening prompt matters most, and the bound keeps a session
/// that never claims from growing the file.
fn hold_prompt(path: &Option<PathBuf>, input: &[u8]) {
    let (Some(path), Ok(text)) = (path, std::str::from_utf8(input)) else {
        return;
    };
    if input.len() > HELD_PROMPT_BYTES {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    locked(path, || {
        let mut held = read_held(path);
        if held.len() >= HELD_PROMPTS {
            return;
        }
        held.push(text.to_owned());
        if let Ok(text) = serde_json::to_string(&held) {
            let _ = crate::hub_cli::write_private(path, &text);
        }
    });
}

fn read_held(path: &std::path::Path) -> Vec<String> {
    let fresh = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age.as_secs() < HELD_PROMPT_SECONDS);
    if !fresh {
        let _ = fs::remove_file(path);
        return Vec::new();
    }
    fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() <= HELD_PROMPTS * HELD_PROMPT_BYTES * 2)
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
}

/// The held prompts, removed from disk whatever happens to them next.
fn take_held_prompts(path: &Option<PathBuf>) -> Vec<Vec<u8>> {
    let Some(path) = path else {
        return Vec::new();
    };
    locked(path, || {
        let held = read_held(path);
        let _ = fs::remove_file(path);
        held.into_iter().map(String::into_bytes).collect()
    })
}

/// Replies already captured in a session, by text, and how far its transcript has been read.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct SeenReplies {
    offset: u64,
    hashes: std::collections::BTreeSet<String>,
    /// Fragments of replies still being displayed, by message id, joined until the final one.
    #[serde(default)]
    partial: std::collections::BTreeMap<String, String>,
}

/// Replies being assembled at once, and the size of each.
const PARTIAL_REPLIES: usize = 64;
const PARTIAL_REPLY_BYTES: usize = 1 << 20;

/// Transcript bytes read per Stop, and replies remembered per session.
const TRANSCRIPT_READ_BYTES: u64 = 8 << 20;
const SEEN_REPLIES: usize = 4096;

/// Ignores whitespace: the transcript joins a reply's text blocks differently from the hooks.
fn text_hash(text: &str) -> String {
    use sha2::Digest;
    let text: String = text.split_whitespace().collect();
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Updates the session's seen replies; `change` gets the saved state, or None for a new session.
fn update_replies(
    path: &Option<PathBuf>,
    change: impl FnOnce(Option<SeenReplies>) -> Option<SeenReplies>,
) {
    let Some(path) = path else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    locked(path, || {
        let saved = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(mut seen) = change(saved) else {
            return;
        };
        while seen.hashes.len() > SEEN_REPLIES {
            let first = seen.hashes.iter().next().cloned();
            first.map(|first| seen.hashes.remove(&first));
        }
        while seen.partial.len() > PARTIAL_REPLIES {
            let first = seen.partial.keys().next().cloned();
            first.map(|first| seen.partial.remove(&first));
        }
        if let Ok(text) = serde_json::to_string(&seen) {
            let _ = crate::hub_cli::write_private(path, &text);
        }
    });
}

fn note_replies(path: &Option<PathBuf>, change: impl FnOnce(&mut SeenReplies)) {
    update_replies(path, |seen| {
        let mut seen = seen.unwrap_or_default();
        change(&mut seen);
        Some(seen)
    });
}

/// Records a displayed reply fragment; the final one marks the whole reply as seen.
fn note_fragment(path: &Option<PathBuf>, payload: &Value) {
    let (Some(id), Some(delta)) = (
        payload.get("message_id").and_then(Value::as_str),
        payload.get("delta").and_then(Value::as_str),
    ) else {
        return;
    };
    let last = payload.get("final") == Some(&Value::Bool(true));
    note_replies(path, |seen| {
        let mut text = seen.partial.remove(id).unwrap_or_default();
        if text.len() + delta.len() <= PARTIAL_REPLY_BYTES {
            text.push_str(delta);
        }
        if last {
            seen.hashes.insert(text_hash(&text));
        } else {
            seen.partial.insert(id.to_owned(), text);
        }
    });
}

/// Moves transcript recovery to the transcript's end: always (`only_new` false), or only for a
/// session with no saved state yet.
fn skip_transcript(path: &Option<PathBuf>, payload: &Value, only_new: bool) {
    let Some(length) = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .and_then(|transcript| fs::metadata(transcript).ok())
        .map(|metadata| metadata.len())
    else {
        return;
    };
    update_replies(path, |saved| match saved {
        Some(_) if only_new => None,
        saved => {
            let mut seen = saved.unwrap_or_default();
            seen.offset = length;
            Some(seen)
        }
    });
}

/// The session's assistant replies in its transcript that no MessageDisplay hook delivered (a
/// hook the host stopped loses its reply), as MessageDisplay inputs keyed by transcript entry, so
/// a repeat is a duplicate. Reads on from where the last Stop stopped.
fn missed_replies(
    seen_path: &Option<PathBuf>,
    transcript: &std::path::Path,
    session_id: &str,
) -> Vec<(String, Vec<u8>)> {
    use std::io::{BufRead, Seek};
    let mut found = Vec::new();
    note_replies(seen_path, |seen| {
        let Ok(mut file) = File::open(transcript) else {
            return;
        };
        let length = file.metadata().map_or(0, |metadata| metadata.len());
        // A shorter transcript is another file: read it from the start.
        if seen.offset > length {
            seen.offset = 0;
        }
        if file.seek(std::io::SeekFrom::Start(seen.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::new(file.take(TRANSCRIPT_READ_BYTES));
        let mut line = Vec::new();
        while let Ok(read) = reader.read_until(b'\n', &mut line) {
            // Only whole lines count; a line still being written is read next time.
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            seen.offset += read as u64;
            if let Ok(entry) = serde_json::from_slice::<Value>(&line) {
                let text = (entry["type"] == "assistant" && entry["isSidechain"] != true)
                    .then(|| entry["message"]["content"].as_array())
                    .flatten()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| block["type"] == "text")
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .filter(|text| !text.trim().is_empty());
                if let (Some(text), Some(uuid)) = (text, entry["uuid"].as_str())
                    && !seen.hashes.contains(&text_hash(&text))
                {
                    let reply = json!({
                        "hook_event_name": "MessageDisplay",
                        "session_id": session_id,
                        "message_id": format!("transcript:{uuid}"),
                        "timestamp": entry["timestamp"],
                        "delta": text,
                        "index": 0,
                        "final": true,
                    });
                    found.extend(serde_json::to_vec(&reply).map(|reply| (text_hash(&text), reply)));
                }
            }
            line.clear();
        }
    });
    found
}

/// Upkeep (`LocalSpool::upkeep`) that fails open. Answers whether the spool is still full, where
/// capture records only gaps; when upkeep fails, whether it is full as it stands.
fn maintain(spool: &LocalSpool) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    spool.upkeep(now).unwrap_or_else(|_| {
        eprintln!("AgentSCM local capture warning: the capture store could not be tidied");
        spool.usage().is_ok_and(|usage| usage.is_full())
    })
}

/// Told once per person and repository, in the agent's session context.
fn sharing_notice(repository: &str) -> String {
    format!(
        "AgentSCM: agent conversations in {repository} are shared with its Hub. This session's prompts, replies, tool input and tool results are uploaded after best-effort secret redaction, and people who can read {repository} on the Hub can read them. Tell the person this once. To stop sharing their own sessions in this repository, they run `agentscm change-capture opt-out` in this checkout."
    )
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
        live_capture_mode: args.capture_mode.into(),
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
                CaptureModeV1::SharedConversation => "shared_after_redaction",
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

    #[test]
    fn stop_recovers_only_replies_no_hook_delivered_and_reads_each_line_once() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Some(dir.path().join("seen"));
        let transcript = dir.path().join("t.jsonl");
        let line = |uuid: &str, texts: &[&str]| {
            let blocks: Vec<_> = texts
                .iter()
                .map(|text| serde_json::json!({"type": "text", "text": text}))
                .collect();
            format!(
                "{}\n",
                serde_json::json!({"type": "assistant", "uuid": uuid, "timestamp": "2026-11-09T10:00:00Z",
                    "message": {"content": blocks}})
            )
        };
        let shown = |delta: &str, last: bool| {
            super::note_fragment(
                &seen,
                &serde_json::json!({"message_id": "m1", "delta": delta, "final": last}),
            )
        };
        // "delivered" arrived through MessageDisplay in two fragments; "lost" did not.
        std::fs::write(&transcript, line("old", &["before this build"])).unwrap();
        let payload = serde_json::json!({"transcript_path": transcript});
        super::skip_transcript(&seen, &payload, true);
        shown("Deliv", false);
        shown("ered.", true);
        let mut text = std::fs::read_to_string(&transcript).unwrap();
        text += &(line("a", &["Deliv", "ered."])
            + &line("b", &["lost"])
            + "{\"type\":\"user\"}\n"
            + "{\"partial");
        std::fs::write(&transcript, text).unwrap();
        // State saved already: skipping only for a new session leaves it alone.
        super::skip_transcript(&seen, &payload, true);
        let replies = super::missed_replies(&seen, &transcript, "s");
        assert_eq!(replies.len(), 1);
        let reply: serde_json::Value = serde_json::from_slice(&replies[0].1).unwrap();
        assert_eq!(reply["hook_event_name"], "MessageDisplay");
        assert_eq!(reply["delta"], "lost");
        assert_eq!(reply["message_id"], "transcript:b");
        assert!(super::missed_replies(&seen, &transcript, "s").is_empty());
    }

    #[test]
    fn held_prompts_keep_the_first_few_and_are_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = Some(dir.path().join("held").join("session"));
        for n in 0..super::HELD_PROMPTS + 2 {
            super::hold_prompt(&path, format!("{{\"prompt\":\"{n}\"}}").as_bytes());
        }
        super::hold_prompt(&path, &vec![b'x'; super::HELD_PROMPT_BYTES + 1]);
        let taken = super::take_held_prompts(&path);
        assert_eq!(taken.len(), super::HELD_PROMPTS);
        assert_eq!(taken[0], br#"{"prompt":"0"}"#);
        assert!(super::take_held_prompts(&path).is_empty());
    }
    use agentscm::change_capture::{
        DeferredHookV1, DeferredReasonV1, LocalSpool, ObservedStop, SpoolLimits,
    };
    use agentscm::change_record::{
        ActionOutcomeV1, AgentHarnessV1, CaptureBatchV1, CaptureEnvelopeV1, CaptureModeV1,
        CapturedEventKindV1, CapturedEventV1, GapReasonV1, SCHEMA_VERSION_V1, SessionIdentityV1,
    };
    use std::path::Path;

    use crate::change_capture_client::{CaptureBinding, CaptureHub};

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

    const SMALL: SpoolLimits = SpoolLimits {
        max_records: 64,
        max_total_bytes: 64 * 1024 * 1024,
        max_artifact_bytes: 1024 * 1024,
    };

    /// A metadata-only binding of `root` that has hooks of its own.
    fn binding(root: &Path) -> CaptureBinding {
        CaptureBinding {
            enabled: true,
            root: root.into(),
            spool: root.join("spool"),
            spool_identity: None,
            repository: "acme/widgets".into(),
            hub: "http://127.0.0.1:9".into(),
            worktree_id: "checkout_test".into(),
            base_ref: "origin/main".into(),
            setup_digest: "a".repeat(64),
            capture_mode: CaptureModeV1::MetadataOnly,
            claude_history_root: None,
            claude_settings: None,
            action_rules: Vec::new(),
            configured_recipes: Vec::new(),
            spool_limits: SMALL,
            follows_hub: false,
        }
    }

    fn batch(n: usize) -> CaptureEnvelopeV1 {
        CaptureEnvelopeV1::EventBatch(CaptureBatchV1 {
            schema_version: SCHEMA_VERSION_V1,
            operation_id: format!("op:{n}"),
            work_id: "wrk_fixture".into(),
            generation: 1,
            session: SessionIdentityV1 {
                harness: AgentHarnessV1::ClaudeCode,
                adapter_version: "1.0.0".into(),
                session_id: "session_fixture".into(),
                parent_session_id: None,
                capture_mode: CaptureModeV1::MetadataOnly,
            },
            events: vec![CapturedEventV1 {
                event_id: format!("event:{n}"),
                sequence: 1,
                observed_at: 1,
                kind: CapturedEventKindV1::SessionEnded,
                tool: None,
                artifact: None,
                setup: None,
                gap: None,
                boundary: None,
                subagent: None,
            }],
        })
    }

    /// A small spool at `root/spool`, filled until it refuses facts; `accepted` acknowledges each.
    fn filled_spool(root: &Path, accepted: bool) -> LocalSpool {
        let spool = LocalSpool::open_with_limits(root.join("spool"), SMALL).unwrap();
        fill(&spool, accepted);
        spool
    }

    fn fill(spool: &LocalSpool, accepted: bool) {
        for n in 0..100 {
            let envelope = batch(n);
            if spool.append(&envelope).is_err() {
                break;
            }
            if accepted {
                spool.acknowledge(&envelope).unwrap();
            }
        }
        assert!(spool.usage().unwrap().is_full());
    }

    /// Runs the capture hook for `event` as Claude Code sends it, and returns its context.
    fn hook(
        binding: &CaptureBinding,
        spool: &LocalSpool,
        event: &str,
        hub: Option<&CaptureHub>,
    ) -> Option<String> {
        let payload = serde_json::json!({
            "session_id": "session_fixture", "hook_event_name": event, "cwd": binding.root
        });
        let input = payload.to_string();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(super::capture_claude(
                input.as_bytes(),
                &payload,
                binding,
                spool,
                hub,
                None,
            ))
    }

    #[test]
    fn a_turn_end_compacts_what_the_hub_accepted_so_the_store_keeps_working() {
        crate::client_config::in_isolated_config_home(|config| {
            let spool = filled_spool(config, true);
            let before = spool.usage().unwrap();
            assert_eq!(hook(&binding(config), &spool, "Stop", None), None);
            let after = spool.usage().unwrap();
            assert!(
                after.records < before.records && !after.is_full(),
                "{before:?} -> {after:?}"
            );
        });
    }

    #[test]
    fn a_full_store_and_an_unknown_hub_answer_are_said_at_session_start() {
        crate::client_config::in_isolated_config_home(|config| {
            let spool = filled_spool(config, false);
            let context = hook(&binding(config), &spool, "SessionStart", None).unwrap();
            assert!(context.contains("local capture store is full"), "{context}");
            assert!(
                context.contains("is unknown, because the Hub did not answer in time"),
                "{context}"
            );
            assert!(!context.contains("does not share"), "{context}");
        });
    }

    /// A loopback Hub that says the repository shares and, with `target`, that the session's
    /// current Work is `TARGET_WORK`; it answers 404 to everything else.
    fn sharing_hub(target: bool) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") && matches!(stream.read(&mut byte), Ok(1)) {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let (status, body) = if head
                    .starts_with("GET /v1/hub/repos/acme%2Fwidgets/conversation-sharing ")
                {
                    (
                        200,
                        r#"{"sharing":true,"notified":true,"enabled":true,"opted_out":false}"#,
                    )
                } else if target
                    && head.starts_with("GET /v1/hub/repos/acme%2Fwidgets/change-records/current?")
                {
                    (
                        200,
                        r#"{"version":1,"work_id":"wrk_11111111111111111111111111111111","generation":1,"session_id":"session_fixture"}"#,
                    )
                } else {
                    (404, "{}")
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        url
    }

    /// A client for the loopback Hub (`sharing_hub`), with a token under `config`.
    fn loopback(config: &Path, target: bool) -> CaptureHub {
        use std::os::unix::fs::PermissionsExt;
        let token = config.join("token");
        std::fs::write(&token, "synthetic-token").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        CaptureHub::new(crate::client_config::Connection {
            root: config.into(),
            origin_host: "github.com".into(),
            hub: sharing_hub(target),
            repo: "acme/widgets".into(),
            token_file: token,
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn a_checkout_set_to_metadata_only_says_it_caps_a_sharing_repository_and_the_fix() {
        crate::client_config::in_isolated_config_home(|config| {
            let hub = loopback(config, false);
            let spool = LocalSpool::open_with_limits(config.join("spool"), SMALL).unwrap();
            let context = hook(&binding(config), &spool, "SessionStart", Some(&hub)).unwrap();
            assert!(
                context.contains("acme/widgets shares agent conversations, but this checkout's local setting is to record metadata only"),
                "{context}"
            );
            assert!(
                context.contains("change-capture set-mode --root")
                    && context.contains("--mode shared-conversation"),
                "{context}"
            );
        });
    }

    #[test]
    fn a_store_full_of_accepted_records_makes_room_and_records_the_next_event() {
        crate::client_config::in_isolated_config_home(|config| {
            let spool = LocalSpool::open_with_limits(config.join("spool"), SMALL).unwrap();
            // Facts from before a session had Work, two days old: nothing reads them again.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            for n in 1..=3_u64 {
                spool
                    .retain_deferred(&DeferredHookV1 {
                        occurrence_id: format!("evt:{n}"),
                        reserved_sequence: Some(n),
                        session_id: "session_before_work".into(),
                        hook_event_name: "SessionStart".into(),
                        observed_at: now - 2 * 24 * 60 * 60,
                        reason: DeferredReasonV1::NoCurrentAuthorizedWork,
                        input_digest: format!("{n:064x}"),
                        event_kind: Some(CapturedEventKindV1::SessionStarted),
                        tool_kind: None,
                        source: None,
                        recipe_digest: None,
                        action_kind: None,
                    })
                    .unwrap();
            }
            fill(&spool, true);
            let hub = loopback(config, true);

            assert_eq!(hook(&binding(config), &spool, "Stop", Some(&hub)), None);
            let events: Vec<CapturedEventV1> = spool
                .pending(100)
                .unwrap()
                .into_iter()
                .flat_map(|envelope| match envelope {
                    CaptureEnvelopeV1::EventBatch(batch) => batch.events,
                    _ => Vec::new(),
                })
                .collect();
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == CapturedEventKindV1::TurnEnded),
                "{events:?}"
            );
            assert!(
                events
                    .iter()
                    .all(|event| event.gap.as_ref().is_none_or(|gap| !matches!(
                        gap.reason,
                        GapReasonV1::LocalRetentionExpired | GapReasonV1::CaptureInterrupted
                    ))),
                "{events:?}"
            );
            assert!(spool.deferred(100).unwrap().is_empty());
        });
    }

    #[test]
    fn a_full_store_never_says_sharing_is_on_at_session_start() {
        crate::client_config::in_isolated_config_home(|config| {
            let spool = filled_spool(config, false);
            let mut shared = binding(config);
            shared.capture_mode = CaptureModeV1::SharedConversation;
            let hub = loopback(config, false);
            let context = hook(&shared, &spool, "SessionStart", Some(&hub)).unwrap();
            assert!(context.contains("local capture store is full"), "{context}");
            assert!(
                context.contains("agentscm change-capture unconfigure --root"),
                "{context}"
            );
            assert!(!context.contains("sharing is on"), "{context}");
        });
    }
}
