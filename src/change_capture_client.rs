//! Private checkout binding, Claude hook installation and replay-safe Hub delivery.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::change_capture::claude_code::{ActionRuleV1, NativeResultSourceV1};
use agentscm::change_capture::{
    CaptureError, ConfiguredRecipeV1, LocalSpool, NamespaceBindOutcome, SpoolClassificationV1,
    SpoolIdentityV1, SpoolLimits, SpoolNamespaceV1, inspect_spool,
};
use agentscm::change_record::{
    ActionKindV1, CaptureBatchV1, CaptureEnvelopeV1, CaptureModeV1, CaptureTargetV1,
    ChangeRecordResponseV1, ConversationMissingRequestV1, ConversationMissingV1,
    ConversationPageV1, CoverageV1, MAX_CHANGE_RECORD_RESPONSE_BYTES,
    MAX_CONVERSATION_MISSING_DIGESTS, MAX_SHARED_BODY_BYTES, RetentionClassV1,
};
use agentscm::forge::encode_path_segment;
use agentscm::hub::forge_http::{self, send};
use agentscm::{Error, Result};
use clap::{Args, ValueEnum};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{client_config, hub_cli};

#[path = "change_capture_settings.rs"]
mod settings_transaction;

const CONFIG_VERSION: u8 = 5;
/// Written only while a binding uses something older binaries cannot parse (the shared mode or a
/// recorded Claude settings path), so users who never opt in keep a config those binaries still
/// read. Older binaries refuse a version 6 file instead of silently dropping a field.
const EXTENDED_CONFIG_VERSION: u8 = 6;
const CONFIG_BYTES: u64 = 256 * 1024;
const RECIPE_BYTES: u64 = 64 * 1024;
const MAX_BINDINGS: usize = 128;
const MAX_RULES: usize = 32;
const MAX_RECIPES: usize = 16;
const REPLY_BYTES: usize = MAX_CHANGE_RECORD_RESPONSE_BYTES;
const DELIVERY_PAGE: usize = 10;
const DELIVERY_DEADLINE: Duration = Duration::from_secs(5);
const HOOK_TIMEOUT_SECONDS: u8 = 20;
const MAX_SESSION_CONTEXT_BYTES: usize = 16 * 1024;
const HOOK_MARKER: &str = "AGENTSCM_CAPTURE_HOOK_V1=1 ";
const CLAUDE_EVENTS: [&str; 11] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "MessageDisplay",
    "Stop",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
    "SessionEnd",
];

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CaptureModeArg {
    MetadataOnly,
    LocalConversation,
    SharedConversation,
}

impl From<CaptureModeArg> for CaptureModeV1 {
    fn from(value: CaptureModeArg) -> Self {
        match value {
            CaptureModeArg::MetadataOnly => Self::MetadataOnly,
            CaptureModeArg::LocalConversation => Self::LocalConversation,
            CaptureModeArg::SharedConversation => Self::SharedConversation,
        }
    }
}

#[derive(Debug, Args)]
pub struct ConfigureArgs {
    /// Canonical Git checkout whose native hooks may use this binding.
    #[arg(long)]
    root: PathBuf,
    /// Absolute private spool directory outside public repository source.
    #[arg(long)]
    spool: PathBuf,
    /// Opaque configured worktree identity, not a host path.
    #[arg(long)]
    worktree_id: String,
    #[arg(long, default_value = "origin/main")]
    base_ref: String,
    /// Lowercase SHA-256 of the effective agent/tool setup.
    #[arg(long)]
    setup_digest: String,
    #[arg(long, value_enum, default_value = "metadata-only")]
    capture_mode: CaptureModeArg,
    /// Exact action classification, coverage and result source. Execution example:
    /// `Bash=execution:partial:process_exit_code`. Verification additionally requires the exact
    /// canonical recipe digest: `Bash=verification:complete:process_exit_code:<digest>`.
    #[arg(long = "action-rule", required = true)]
    action_rules: Vec<String>,
    /// Private mode-0600 JSON file containing one exact configured recipe.
    #[arg(long = "recipe-file")]
    recipe_files: Vec<PathBuf>,
    /// Claude project-local settings. Defaults to ROOT/.claude/settings.local.json.
    #[arg(long)]
    claude_settings: Option<PathBuf>,
    /// Explicit Claude-managed history root. Omit to leave provider usage unavailable.
    #[arg(long)]
    claude_history_root: Option<PathBuf>,
    /// Maximum retained metadata records in this private spool.
    #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u32).range(1..=65_536))]
    max_spool_records: u32,
    /// Maximum total private spool bytes, including reserved completion headroom.
    #[arg(long, default_value_t = 64 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(4096..=1024 * 1024 * 1024))]
    max_spool_bytes: u64,
    /// Maximum bytes for one locally retained artifact.
    #[arg(long, default_value_t = 16 * 1024 * 1024, value_parser = clap::value_parser!(u64).range(1..=1024 * 1024 * 1024))]
    max_artifact_bytes: u64,
}

#[derive(Debug, Args)]
pub struct SetModeArgs {
    /// Canonical Git checkout with an enabled capture binding.
    #[arg(long)]
    pub root: PathBuf,
    #[arg(long, value_enum)]
    pub mode: CaptureModeArg,
}

#[derive(Debug, Args)]
pub struct UnconfigureArgs {
    #[arg(long)]
    root: PathBuf,
    /// Claude project-local settings. Defaults to ROOT/.claude/settings.local.json.
    #[arg(long)]
    claude_settings: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// Canonical Git checkout whose disabled capture binding is removed from the config.
    #[arg(long)]
    root: PathBuf,
}

#[derive(Debug, Args)]
pub struct SettingsRecoveryArgs {
    #[arg(long)]
    root: PathBuf,
    /// Claude project-local settings. Defaults to ROOT/.claude/settings.local.json.
    #[arg(long)]
    claude_settings: Option<PathBuf>,
    /// Explicitly restore one retained transaction after inspecting the listing.
    #[arg(long, conflicts_with = "prune_unchanged")]
    restore: Option<String>,
    /// Remove only unchanged recovery entries. Late editor writes are never pruned.
    #[arg(long)]
    prune_unchanged: bool,
    /// Adopt inspected late writes retained by one exact transaction.
    #[arg(long, conflicts_with_all = ["restore", "prune_unchanged"])]
    reconcile_changed: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureBinding {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub root: PathBuf,
    pub spool: PathBuf,
    /// Durable v4 identity of the private spool. An existing checkout binding may update capture
    /// settings or follow an identity-preserving offline move, but it can never adopt a different
    /// spool identity in M1.
    #[serde(default)]
    pub spool_identity: Option<SpoolIdentityV1>,
    pub repository: String,
    pub hub: String,
    pub worktree_id: String,
    pub base_ref: String,
    pub setup_digest: String,
    pub capture_mode: CaptureModeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_history_root: Option<PathBuf>,
    /// The Claude settings file holding this binding's hooks when it is not the default
    /// `ROOT/.claude/settings.local.json`; `None` means the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_settings: Option<PathBuf>,
    pub action_rules: Vec<ActionRuleV1>,
    #[serde(default)]
    pub configured_recipes: Vec<ConfiguredRecipeV1>,
    #[serde(default)]
    pub spool_limits: SpoolLimits,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CaptureConfig {
    version: u8,
    bindings: Vec<CaptureBinding>,
}

const fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DeliveryReport {
    pub delivered: usize,
    pub blocked: usize,
    pub pending_page: usize,
}

struct ConfigLock(File);

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor remains open and owned by `self.0` for the call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn config_path() -> Result<PathBuf> {
    Ok(hub_cli::config_dir()?
        .join("agentscm")
        .join("change-capture.json"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(hub_cli::config_dir()?
        .join("agentscm")
        .join("change-capture.lock"))
}

fn acquire_lock() -> Result<ConfigLock> {
    let path = lock_path()?;
    std::fs::create_dir_all(path.parent().ok_or_else(|| invalid("bad config path"))?)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    let file = options.open(path)?;
    // Configuration is an explicit foreground operation; serialize concurrent writers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(ConfigLock(file))
}

fn read_bounded(path: &Path) -> Result<Option<String>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONFIG_BYTES {
        return Err(invalid("change capture config is too large"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("change capture config is not UTF-8"))
}

fn load() -> Result<Vec<CaptureBinding>> {
    let Some(text) = read_bounded(&config_path()?)? else {
        return Ok(Vec::new());
    };
    let config: CaptureConfig =
        serde_json::from_str(&text).map_err(|_| invalid("change capture config is not valid"))?;
    if !matches!(config.version, 2..=EXTENDED_CONFIG_VERSION)
        || config.bindings.len() > MAX_BINDINGS
    {
        return Err(invalid("change capture config is not valid"));
    }
    for binding in &config.bindings {
        validate_binding(binding)?;
    }
    Ok(config.bindings)
}

fn save(bindings: &[CaptureBinding]) -> Result<()> {
    let extended = bindings.iter().any(|binding| {
        binding.capture_mode == CaptureModeV1::SharedConversation
            || binding.claude_settings.is_some()
    });
    let text = serde_json::to_string(&CaptureConfig {
        version: if extended {
            EXTENDED_CONFIG_VERSION
        } else {
            CONFIG_VERSION
        },
        bindings: bindings.to_vec(),
    })
    .map_err(|_| invalid("cannot encode change capture config"))?;
    if text.len() as u64 > CONFIG_BYTES {
        return Err(invalid("change capture config is too large"));
    }
    hub_cli::write_private(&config_path()?, &text)
}

fn validate_binding(binding: &CaptureBinding) -> Result<()> {
    binding
        .spool_limits
        .validate_action_capture()
        .map_err(|error| invalid(format!("change capture spool limits are invalid: {error}")))?;
    if !binding.root.is_absolute()
        || !binding.spool.is_absolute()
        || binding.root == binding.spool
        || binding.spool.starts_with(&binding.root)
        || binding.action_rules.is_empty()
        || binding.action_rules.len() > MAX_RULES
        || binding.configured_recipes.len() > MAX_RECIPES
        || binding.hub.is_empty()
        || binding.hub.len() > 2048
        || binding.hub.chars().any(char::is_control)
        || !is_identifier(&binding.worktree_id, "")
        || !is_digest(&binding.setup_digest)
        || binding.base_ref.is_empty()
        || binding.base_ref.len() > 1024
        || binding.base_ref.chars().any(char::is_control)
        || binding
            .spool_identity
            .is_some_and(|identity| identity.version != 4)
        || binding
            .claude_history_root
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        || binding.claude_settings.as_ref().is_some_and(|path| {
            absolute_without_parent_components(path).is_err() || !path.starts_with(&binding.root)
        })
    {
        return Err(invalid("change capture binding is invalid"));
    }
    let mut rules = BTreeSet::new();
    for rule in &binding.action_rules {
        if rule.native_tool.is_empty()
            || rule.native_tool.len() > 128
            || rule.native_tool.chars().any(char::is_control)
            || !rules.insert((rule.native_tool.as_str(), rule.recipe_digest.as_deref()))
            || (rule.kind == ActionKindV1::Execution && rule.recipe_digest.is_some())
            || (rule.kind == ActionKindV1::Verification && rule.recipe_digest.is_none())
            || rule
                .recipe_digest
                .as_deref()
                .is_some_and(|digest| !is_digest(digest))
        {
            return Err(invalid("change capture action rules are invalid"));
        }
        if rule.result_source == NativeResultSourceV1::ProcessExitCode
            && !matches!(rule.native_tool.as_str(), "Bash" | "PowerShell")
        {
            return Err(invalid(
                "process exit-code results are supported only for Bash or PowerShell",
            ));
        }
    }
    let mut recipe_ids = BTreeSet::new();
    let mut recipe_digests = BTreeSet::new();
    for recipe in &binding.configured_recipes {
        recipe
            .validate()
            .map_err(|error| invalid(format!("configured recipe is invalid: {error}")))?;
        let digest = recipe
            .digest()
            .map_err(|error| invalid(format!("configured recipe is invalid: {error}")))?;
        if !recipe_ids.insert(recipe.id.as_str()) || !recipe_digests.insert(digest) {
            return Err(invalid(
                "configured recipes contain a duplicate id or digest",
            ));
        }
    }
    let _ = session_start_context(binding)?;
    Ok(())
}

fn validate_binding_update(saved: &CaptureBinding, proposed: &CaptureBinding) -> Result<()> {
    let (Some(saved_identity), Some(proposed_identity)) =
        (saved.spool_identity, proposed.spool_identity)
    else {
        return Err(invalid(
            "existing capture binding has no trusted v4 spool identity; inspect/export it and use a different checkout root",
        ));
    };
    if saved.root != proposed.root
        || saved.repository != proposed.repository
        || saved.hub != proposed.hub
        || saved.worktree_id != proposed.worktree_id
        || saved_identity != proposed_identity
    {
        return Err(invalid(
            "capture binding cannot switch spool safety lineage for an existing checkout",
        ));
    }
    if proposed.spool_limits.max_records < saved.spool_limits.max_records
        || proposed.spool_limits.max_total_bytes < saved.spool_limits.max_total_bytes
        || proposed.spool_limits.max_artifact_bytes < saved.spool_limits.max_artifact_bytes
    {
        return Err(invalid(
            "capture spool limits cannot be lowered for an existing safety lineage",
        ));
    }
    Ok(())
}

fn disable_binding(bindings: &mut [CaptureBinding], root: &Path) -> bool {
    let Some(binding) = bindings.iter_mut().find(|binding| binding.root == root) else {
        return false;
    };
    let changed = binding.enabled;
    binding.enabled = false;
    changed
}

fn load_recipe_file(path: &Path) -> Result<ConfiguredRecipeV1> {
    use std::os::unix::fs::OpenOptionsExt;

    if !path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(
            "configured recipe file must be an absolute normalized path",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    load_recipe_from_file(file)
}

fn load_recipe_from_file(file: File) -> Result<ConfiguredRecipeV1> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > RECIPE_BYTES
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(invalid(
            "configured recipe file must be a private bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(RECIPE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECIPE_BYTES {
        return Err(invalid("configured recipe file is too large"));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| invalid("configured recipe file is not UTF-8"))?;
    let recipe: ConfiguredRecipeV1 =
        serde_json::from_str(&text).map_err(|_| invalid("configured recipe file is not valid"))?;
    recipe
        .validate()
        .map_err(|error| invalid(format!("configured recipe file is invalid: {error}")))?;
    Ok(recipe)
}

fn is_identifier(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_rule(value: &str) -> Result<ActionRuleV1> {
    let (native_tool, facts) = value
        .split_once('=')
        .ok_or_else(|| invalid("action rule must be TOOL=KIND:COVERAGE:RESULT_SOURCE[:DIGEST]"))?;
    let parts = facts.split(':').collect::<Vec<_>>();
    if !(parts.len() == 3 || parts.len() == 4) {
        return Err(invalid(
            "action rule must be TOOL=KIND:COVERAGE:RESULT_SOURCE[:DIGEST]",
        ));
    }
    let kind = match parts[0] {
        "execution" => ActionKindV1::Execution,
        "verification" => ActionKindV1::Verification,
        _ => return Err(invalid("action rule has an unknown kind")),
    };
    let coverage = match parts[1] {
        "complete" => CoverageV1::Complete,
        "partial" => CoverageV1::Partial,
        "unknown" => CoverageV1::Unknown,
        _ => return Err(invalid("action rule has an unknown coverage")),
    };
    let result_source = match parts[2] {
        "process_exit_code" => NativeResultSourceV1::ProcessExitCode,
        "unverified" => NativeResultSourceV1::Unverified,
        _ => return Err(invalid("action rule has an unknown result source")),
    };
    let recipe_digest = parts.get(3).map(|value| (*value).to_owned());
    if (kind == ActionKindV1::Execution && recipe_digest.is_some())
        || (kind == ActionKindV1::Verification && recipe_digest.is_none())
        || recipe_digest
            .as_deref()
            .is_some_and(|digest| !is_digest(digest))
    {
        return Err(invalid(
            "execution rules omit a digest; verification rules require one exact digest",
        ));
    }
    Ok(ActionRuleV1 {
        native_tool: native_tool.to_owned(),
        kind,
        coverage,
        result_source,
        recipe_digest,
    })
}

/// Validates one binding against the saved binding for its checkout, saves it and, when
/// `settings` is given, reinstalls the marked hooks, restoring the previous config on failure.
/// The caller holds the config lock and has loaded `old_bindings` under it.
fn commit_binding(
    old_bindings: &[CaptureBinding],
    binding: CaptureBinding,
    settings: Option<&Path>,
) -> Result<()> {
    validate_binding(&binding)?;
    let saved = old_bindings.iter().find(|saved| saved.root == binding.root);
    if let Some(saved) = saved {
        validate_binding_update(saved, &binding)?;
    }
    ensure_spool_namespace(&binding)?;
    let root = binding.root.clone();
    let mut bindings = old_bindings.to_vec();
    bindings.retain(|saved| saved.root != root);
    if saved.is_none() && bindings.len() == MAX_BINDINGS {
        return Err(invalid("too many change capture bindings"));
    }
    bindings.push(binding);
    bindings.sort_by(|left, right| left.root.cmp(&right.root));
    save(&bindings)?;
    if let Some(settings) = settings
        && let Err(error) = install_hooks(&root, settings, true)
    {
        if let Err(rollback) = save(old_bindings) {
            return Err(invalid(format!(
                "Claude hook installation failed ({error}); capture binding rollback also failed ({rollback})"
            )));
        }
        return Err(error);
    }
    Ok(())
}

/// The repository and Hub a person was shown before agreeing to share. Turning sharing on is
/// refused when the checkout is connected to anything else by the time the change is made.
#[derive(Clone, Copy, Debug)]
pub struct ExpectedTarget<'a> {
    pub repository: &'a str,
    pub hub: &'a str,
}

/// The connections lock and the config lock, held together with the checkout's connection and the
/// bindings read under them, so a decision made from them cannot be overtaken by a reconnect or
/// another writer.
struct Locked {
    _connections: File,
    _lock: ConfigLock,
    connection: client_config::Connection,
    bindings: Vec<CaptureBinding>,
}

/// Takes the connections lock, then the config lock (the one global order), reads the checkout's
/// connection and refuses a connection other than `expected`, so what the person agreed to is what
/// the change applies to. A reconnect waits until the caller drops the result.
fn lock_checkout(root: &Path, expected: Option<ExpectedTarget<'_>>) -> Result<Locked> {
    let connections = client_config::lock_connections()?;
    let lock = acquire_lock()?;
    let connection = connection_for(root)?;
    if let Some(expected) = expected
        && (!connection.repo.eq_ignore_ascii_case(expected.repository)
            || connection.hub != expected.hub)
    {
        return Err(invalid(format!(
            "this checkout is now connected to {} on {}; refresh and confirm again",
            connection.repo, connection.hub
        )));
    }
    Ok(Locked {
        _connections: connections,
        _lock: lock,
        connection,
        bindings: load()?,
    })
}

/// The error for a saved capture whose repository or Hub is not the checkout's current one, with
/// the supported way out.
fn lineage_error(saved: &CaptureBinding, connection: &client_config::Connection) -> Error {
    invalid(format!(
        "this checkout's saved capture belongs to {} on {}, and its private spool keeps that history apart, so it cannot be reused for {} on {}. To start fresh, run `agentscm change-capture unconfigure --root <checkout>` if capture is still on, then `agentscm change-capture forget --root <checkout>` (the old spool stays on disk), then try again",
        saved.repository, saved.hub, connection.repo, connection.hub
    ))
}

/// Changes only the capture mode of an existing enabled binding. Retained local data is never
/// touched, and hooks are left alone because they read the mode from the binding when they run.
///
/// `expected` is what the person was shown; it is checked under the config lock when the mode is
/// shared. Only the command line, where the person typed the command, passes `None`.
pub fn set_capture_mode(
    root: &Path,
    mode: CaptureModeV1,
    expected: Option<ExpectedTarget<'_>>,
) -> Result<Value> {
    let root = client_config::canonical_root(root)?;
    let locked = lock_checkout(
        &root,
        expected.filter(|_| mode == CaptureModeV1::SharedConversation),
    )?;
    set_mode_in(&locked, &root, mode)
}

fn set_mode_in(locked: &Locked, root: &Path, mode: CaptureModeV1) -> Result<Value> {
    let mut binding = locked
        .bindings
        .iter()
        .find(|binding| binding.root == root && binding.enabled)
        .cloned()
        .ok_or_else(|| invalid("no enabled capture binding for this checkout"))?;
    if locked.connection.repo != binding.repository || locked.connection.hub != binding.hub {
        return Err(lineage_error(&binding, &locked.connection));
    }
    binding.capture_mode = mode;
    commit_binding(&locked.bindings, binding, None)?;
    Ok(json!({"root": root, "capture_mode": mode}))
}

/// What a checkout's saved capture binding says, for every surface that shows or offers the
/// sharing switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureSetting {
    /// A binding exists, enabled or not.
    pub configured: bool,
    pub enabled: bool,
    /// The binding's mode while it is enabled; `None` otherwise.
    pub mode: Option<CaptureModeV1>,
}

/// The one read path for the tray, the local Settings page and the client status. `None` means the
/// capture config cannot be read, so callers must not offer to change anything.
pub fn capture_setting(root: &Path) -> Option<CaptureSetting> {
    Some(setting_in(&load().ok()?, root))
}

fn setting_in(bindings: &[CaptureBinding], root: &Path) -> CaptureSetting {
    let binding = bindings.iter().find(|binding| binding.root == root);
    CaptureSetting {
        configured: binding.is_some(),
        enabled: binding.is_some_and(|binding| binding.enabled),
        mode: binding
            .filter(|binding| binding.enabled)
            .map(|binding| binding.capture_mode),
    }
}

/// Capture mode of the enabled binding for `root`, or `None` when it has none.
pub fn enabled_capture_mode(root: &Path) -> Option<CaptureModeV1> {
    capture_setting(root)?.mode
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut random = vec![0_u8; bytes];
    getrandom::fill(&mut random).map_err(|_| invalid("random source unavailable"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The checkout's default branch as `origin/<name>` when `origin/HEAD` names a branch that
/// resolves, else `origin/main`.
fn default_base_ref(root: &Path) -> String {
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        output.status.success().then(|| text.trim().to_owned())
    };
    git(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .filter(|name| {
            name.len() > "origin/".len()
                && name.len() <= 1024
                && name.starts_with("origin/")
                && !name.chars().any(char::is_control)
        })
        .filter(|name| {
            git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{name}^{{commit}}"),
            ])
            .is_some()
        })
        .unwrap_or_else(|| "origin/main".into())
}

/// A settings file that setup is about to create, and the transaction files saved beside it, are
/// hidden from `git status` through the repository's local exclude unless Git already ignores
/// them. The tracked `.gitignore` is never edited, and a file that already exists is left as it
/// is.
fn keep_new_settings_untracked(root: &Path, settings: &Path) -> Result<()> {
    if settings.symlink_metadata().is_ok() {
        return Ok(());
    }
    let (relative, name) = settings
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .zip(settings.file_name().and_then(std::ffi::OsStr::to_str))
        .ok_or_else(|| invalid("Claude settings must be inside the configured checkout"))?;
    let directory = relative.strip_suffix(name).unwrap_or_default();
    for (sample, pattern) in [
        (relative.to_owned(), relative.to_owned()),
        (
            format!("{directory}.{name}.agentscm-capture-sample"),
            format!("{directory}.{name}.agentscm-capture-*"),
        ),
    ] {
        if !git_ignores(root, &sample)? {
            crate::client_setup::ignore_new_file(root, &pattern)?;
        }
    }
    Ok(())
}

fn git_ignores(root: &Path, relative: &str) -> Result<bool> {
    let status = std::process::Command::new("git")
        .args(["check-ignore", "-q", "--", relative])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(invalid(
            "cannot check whether Git ignores the Claude settings file",
        )),
    }
}

/// What [`enable_capture`] builds from the bindings read under the lock: `None` when the binding
/// is already enabled and only its mode changes, else the spec that creates the binding or
/// re-enables the disabled one.
fn plan_enable(
    bindings: &[CaptureBinding],
    connection: &client_config::Connection,
    root: &Path,
    mode: CaptureModeV1,
    spools: &Path,
) -> Result<Option<BindingSpec>> {
    let spec = match bindings.iter().find(|binding| binding.root == root) {
        Some(saved) if saved.enabled => return Ok(None),
        Some(saved) => {
            if saved.repository != connection.repo || saved.hub != connection.hub {
                return Err(lineage_error(saved, connection));
            }
            BindingSpec {
                settings: settings_path(root, saved.claude_settings.clone())?,
                root: root.to_owned(),
                spool: saved.spool.clone(),
                worktree_id: saved.worktree_id.clone(),
                base_ref: saved.base_ref.clone(),
                setup_digest: saved.setup_digest.clone(),
                capture_mode: mode,
                action_rules: saved.action_rules.clone(),
                configured_recipes: saved.configured_recipes.clone(),
                claude_history_root: saved.claude_history_root.clone(),
                spool_limits: saved.spool_limits,
            }
        }
        None => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(spools)?;
            BindingSpec {
                spool: spools.join(random_hex(16)?),
                worktree_id: format!("checkout_{}", random_hex(16)?),
                base_ref: default_base_ref(root),
                setup_digest: random_hex(32)?,
                capture_mode: mode,
                action_rules: vec![parse_rule("Bash=execution:partial:process_exit_code")?],
                configured_recipes: Vec::new(),
                settings: settings_path(root, None)?,
                claude_history_root: None,
                spool_limits: SpoolLimits::default(),
                root: root.to_owned(),
            }
        }
    };
    Ok(Some(spec))
}

/// Makes capture available for a connected checkout in one step and sets its mode.
///
/// Everything is decided under the config lock, with the checkout's connection read there, and
/// `expected` (the repository and Hub the person was shown) must match that connection.
///
/// - Enabled binding: only the mode changes, exactly like [`set_capture_mode`].
/// - Disabled binding: re-enabled through [`install_binding`] with everything it saved (spool,
///   worktree, base ref, setup digest, rules, recipes, limits); hooks are reinstalled in the
///   Claude settings file it recorded, or the default `ROOT/.claude/settings.local.json`. A
///   binding saved for another repository or Hub is refused with the way to forget it.
/// - No binding: created through [`install_binding`], the same path as
///   `change-capture configure`, with these defaults. The private spool is
///   `<config>/agentscm/capture-spools/<random>`, mode 0700. The worktree id is
///   `checkout_<random>` and the setup digest is 32 random bytes, so neither can be recomputed
///   from a path and none repeats on another computer; the saved binding keeps them stable. The
///   setup digest is a caller assertion that identifies this default, not a measurement of the
///   agent setup. The base ref is the checkout's default branch from `origin/HEAD` when it
///   resolves, else `origin/main`. The only action rule is
///   `Bash=execution:partial:process_exit_code`. Hooks go to the default Claude settings file,
///   which Git's local exclude hides when setup creates it and Git does not already ignore it.
///
/// Any mode other than the one asked for is never chosen here, so `shared-conversation` is
/// enabled only by an explicit request.
pub fn enable_capture(
    root: &Path,
    mode: CaptureModeV1,
    expected: ExpectedTarget<'_>,
) -> Result<Value> {
    let root = client_config::canonical_root(root)?;
    let locked = lock_checkout(&root, Some(expected))?;
    let spools = hub_cli::config_dir()?
        .join("agentscm")
        .join("capture-spools");
    match plan_enable(&locked.bindings, &locked.connection, &root, mode, &spools)? {
        None => set_mode_in(&locked, &root, mode),
        Some(spec) => {
            keep_new_settings_untracked(&root, &spec.settings)?;
            let mut result = install_binding(&locked.bindings, &locked.connection, spec)?;
            result["capture_mode"] = json!(mode);
            result["root"] = json!(root);
            Ok(result)
        }
    }
}

/// Everything a binding is built from. `configure` fills it from flags; [`enable_capture`] fills
/// it from documented defaults or from the saved binding; both go through [`install_binding`].
struct BindingSpec {
    root: PathBuf,
    spool: PathBuf,
    worktree_id: String,
    base_ref: String,
    setup_digest: String,
    capture_mode: CaptureModeV1,
    action_rules: Vec<ActionRuleV1>,
    configured_recipes: Vec<ConfiguredRecipeV1>,
    settings: PathBuf,
    claude_history_root: Option<PathBuf>,
    spool_limits: SpoolLimits,
}

pub fn configure(args: ConfigureArgs) -> Result<Value> {
    let root = client_config::canonical_root(&args.root)?;
    let spool_limits = SpoolLimits {
        max_records: args.max_spool_records as usize,
        max_total_bytes: args.max_spool_bytes,
        max_artifact_bytes: args.max_artifact_bytes,
    };
    let action_rules = args
        .action_rules
        .iter()
        .map(|value| parse_rule(value))
        .collect::<Result<Vec<_>>>()?;
    let configured_recipes = args
        .recipe_files
        .iter()
        .map(|path| load_recipe_file(path))
        .collect::<Result<Vec<_>>>()?;
    let claude_history_root = args
        .claude_history_root
        .as_deref()
        .map(canonical_history_root)
        .transpose()?;
    let spec = BindingSpec {
        spool: absolute_without_parent_components(&args.spool)?,
        settings: settings_path(&root, args.claude_settings)?,
        root: root.clone(),
        worktree_id: args.worktree_id,
        base_ref: args.base_ref,
        setup_digest: args.setup_digest,
        capture_mode: args.capture_mode.into(),
        action_rules,
        configured_recipes,
        claude_history_root,
        spool_limits,
    };
    let locked = lock_checkout(&root, None)?;
    install_binding(&locked.bindings, &locked.connection, spec)
}

/// Validates and saves the binding `spec` describes and installs its hooks. The caller holds the
/// config lock and has read `old_bindings` and `connection` under it.
fn install_binding(
    old_bindings: &[CaptureBinding],
    connection: &client_config::Connection,
    spec: BindingSpec,
) -> Result<Value> {
    let BindingSpec {
        root,
        spool: requested_spool,
        worktree_id,
        base_ref,
        setup_digest,
        capture_mode,
        action_rules,
        configured_recipes,
        settings,
        claude_history_root,
        spool_limits,
    } = spec;
    spool_limits
        .validate_action_capture()
        .map_err(|error| invalid(format!("local capture: {error}")))?;
    let saved = old_bindings.iter().find(|binding| binding.root == root);
    if let Some(saved) = saved {
        if saved.repository != connection.repo || saved.hub != connection.hub {
            return Err(lineage_error(saved, connection));
        }
        if saved.worktree_id != worktree_id {
            return Err(invalid(
                "capture binding cannot switch namespace safety lineage for an existing checkout",
            ));
        }
        if spool_limits.max_records < saved.spool_limits.max_records
            || spool_limits.max_total_bytes < saved.spool_limits.max_total_bytes
            || spool_limits.max_artifact_bytes < saved.spool_limits.max_artifact_bytes
        {
            return Err(invalid(
                "capture spool limits cannot be lowered for an existing safety lineage",
            ));
        }
    }
    let inspected_identity = if saved.is_some() {
        let inspection = inspect_spool(&requested_spool)
            .map_err(|error| invalid(format!("local capture inspection: {error}")))?;
        if inspection.classification != SpoolClassificationV1::V4IdentityMatch {
            return Err(invalid(
                "existing capture binding requires the same exact v4 spool identity; inspect/export the requested path",
            ));
        }
        Some(inspection.committed_identity.ok_or_else(|| {
            invalid("existing capture binding requires a committed v4 spool identity")
        })?)
    } else {
        None
    };
    if let Some(saved) = saved
        && (saved.spool_identity.is_none() || saved.spool_identity != inspected_identity)
    {
        return Err(invalid(
            "existing capture binding has different or legacy spool safety lineage; it cannot be replaced",
        ));
    }
    let local_spool = match inspected_identity {
        Some(expected) => {
            LocalSpool::open_with_limits_expected(&requested_spool, spool_limits, expected)
        }
        None => LocalSpool::open_with_limits(&requested_spool, spool_limits),
    }
    .map_err(|error| invalid(format!("local capture: {error}")))?;
    let spool_identity = local_spool.identity();
    if inspected_identity.is_some_and(|identity| spool_identity != identity) {
        return Err(invalid(
            "capture spool identity changed between inspection and open",
        ));
    }
    let spool = local_spool
        .root()
        .canonicalize()
        .map_err(|_| invalid("capture spool cannot be canonicalized"))?;
    let binding = CaptureBinding {
        enabled: true,
        root: root.clone(),
        spool,
        spool_identity: Some(spool_identity),
        repository: connection.repo.clone(),
        hub: connection.hub.clone(),
        worktree_id,
        base_ref,
        setup_digest,
        capture_mode,
        claude_history_root,
        claude_settings: (settings != default_settings(&root)).then(|| settings.clone()),
        action_rules,
        configured_recipes,
        spool_limits,
    };
    commit_binding(old_bindings, binding, Some(&settings))?;
    Ok(json!({"configured": true, "root": root, "settings": settings}))
}

/// The root a binding is stored under. A checkout that no longer exists cannot be canonicalized,
/// so it is matched by the absolute path given, normalized lexically; the flag says the tree is
/// gone.
fn binding_root(path: &Path) -> Result<(PathBuf, bool)> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok((client_config::canonical_root(path)?, false));
    }
    if !path.is_absolute() {
        return Err(invalid(
            "the checkout no longer exists; give its absolute path as it was saved",
        ));
    }
    let mut root = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                root.pop();
            }
            part => root.push(part),
        }
    }
    Ok((root, true))
}

pub fn unconfigure(args: UnconfigureArgs) -> Result<Value> {
    let (root, gone) = binding_root(&args.root)?;
    let _lock = acquire_lock()?;
    let old_bindings = load()?;
    let recorded = old_bindings
        .iter()
        .find(|binding| binding.root == root)
        .and_then(|binding| binding.claude_settings.clone());
    let settings = settings_path(&root, args.claude_settings.or(recorded))?;
    let mut bindings = old_bindings.clone();
    let disabled = disable_binding(&mut bindings, &root);
    save(&bindings)?;
    // Hooks lived inside the deleted tree, so there is nothing to remove.
    if !gone && let Err(error) = install_hooks(&root, &settings, false) {
        if let Err(rollback) = save(&old_bindings) {
            return Err(invalid(format!(
                "Claude hook removal failed ({error}); capture binding rollback also failed ({rollback})"
            )));
        }
        return Err(error);
    }
    let mut result = json!({"configured": false, "disabled": disabled, "lineage_retained": bindings.iter().any(|binding| binding.root == root),
        "root": root, "settings": settings, "hooks_removed": !gone});
    if gone {
        result["note"] =
            json!("the checkout no longer exists, so its Claude hooks were not touched");
    }
    Ok(result)
}

/// Removes a disabled checkout's binding from the config so the checkout can be set up again for
/// another repository or Hub. The binding's spool is never touched; its path is returned so the
/// person can export or delete it. Works for a checkout that has been deleted.
pub fn forget(args: ForgetArgs) -> Result<Value> {
    let (root, _) = binding_root(&args.root)?;
    let _lock = acquire_lock()?;
    let mut bindings = load()?;
    let position = bindings
        .iter()
        .position(|binding| binding.root == root)
        .ok_or_else(|| invalid("no capture binding for this checkout"))?;
    if bindings[position].enabled {
        return Err(invalid(
            "capture is still enabled for this checkout; run `agentscm change-capture unconfigure --root <checkout>` first",
        ));
    }
    let binding = bindings.remove(position);
    save(&bindings)?;
    Ok(json!({"forgotten": true, "root": root, "spool": binding.spool, "spool_retained": true}))
}

pub fn settings_recovery(args: SettingsRecoveryArgs) -> Result<Value> {
    let root = client_config::canonical_root(&args.root)?;
    let settings = settings_path(&root, args.claude_settings)?;
    let _lock = acquire_lock()?;
    let location = settings_transaction::SettingsLocation::open(&root, &settings)?;
    settings_transaction::recover(
        &location,
        args.restore.as_deref(),
        args.prune_unchanged,
        args.reconcile_changed.as_deref(),
    )
}

fn absolute_without_parent_components(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid("capture spool must be an absolute normalized path"));
    }
    Ok(path.to_owned())
}

/// Where Claude hooks go unless a binding records another file.
fn default_settings(root: &Path) -> PathBuf {
    root.join(".claude/settings.local.json")
}

fn settings_path(root: &Path, requested: Option<PathBuf>) -> Result<PathBuf> {
    let path = requested.unwrap_or_else(|| default_settings(root));
    let path = absolute_without_parent_components(&path)?;
    if !path.starts_with(root) {
        return Err(invalid(
            "Claude settings must be inside the configured checkout",
        ));
    }
    Ok(path)
}

fn canonical_history_root(requested: &Path) -> Result<PathBuf> {
    let requested = absolute_without_parent_components(requested)?;
    let metadata = std::fs::symlink_metadata(&requested)
        .map_err(|_| invalid("Claude history root is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid(
            "Claude history root must be an existing non-symlink directory",
        ));
    }
    requested
        .canonicalize()
        .map_err(|_| invalid("Claude history root cannot be canonicalized"))
}

fn connection_for(root: &Path) -> Result<client_config::Connection> {
    let connection = client_config::load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .ok_or_else(|| invalid("no client connection for this checkout"))?;
    if !client_config::is_current(&connection) {
        return Err(invalid("client connection no longer matches this checkout"));
    }
    Ok(connection)
}

pub fn binding_for_payload(payload: &Value) -> Result<Option<CaptureBinding>> {
    let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
        return Ok(None);
    };
    let root = match client_config::canonical_root(Path::new(cwd)) {
        Ok(root) => root,
        Err(_) => return Ok(None),
    };
    let Some(binding) = load()?.into_iter().find(|binding| binding.root == root) else {
        return Ok(None);
    };
    if !binding.enabled {
        return Ok(None);
    }
    let connection = connection_for(&root)?;
    if connection.repo != binding.repository || connection.hub != binding.hub {
        return Err(invalid(
            "capture binding namespace differs from client connection",
        ));
    }
    ensure_spool_namespace(&binding)?;
    Ok(Some(binding))
}

pub fn capture_status(root: &Path) -> Result<Value> {
    let root = client_config::canonical_root(root)?;
    let binding = load()?
        .into_iter()
        .find(|binding| binding.root == root)
        .ok_or_else(|| invalid("configured checkout binding is unavailable"))?;
    let current_connection = connection_for(&root).is_ok();
    let inspection = inspect_spool(&binding.spool);
    let identity_matches = inspection.as_ref().is_ok_and(|inspection| {
        inspection.classification == SpoolClassificationV1::V4IdentityMatch
            && binding.spool_identity.is_some()
            && inspection.committed_identity == binding.spool_identity
    });
    let state = if !binding.enabled {
        "disabled"
    } else if identity_matches && current_connection {
        "active"
    } else {
        "paused"
    };
    let (inspection, inspection_error) = match inspection {
        Ok(inspection) => (Some(inspection), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(json!({
        "version": 1,
        "state": state,
        "enabled": binding.enabled,
        "capture_mode": binding.capture_mode,
        "current_connection": current_connection,
        "root": binding.root,
        "spool": binding.spool,
        "retained_spool_identity": binding.spool_identity,
        "identity_matches": identity_matches,
        "inspection": inspection,
        "inspection_error": inspection_error,
        "resumable_from_copy": false,
    }))
}

/// The enabled capture spool bound to exactly this checkout for `repository` and `hub`, found
/// without opening or binding the spool, for read-only browsing.
pub(crate) fn spool_for_checkout(
    root: &Path,
    repository: &str,
    hub: &str,
) -> Result<Option<PathBuf>> {
    let root = client_config::canonical_root(root)?;
    Ok(load()?
        .into_iter()
        .find(|binding| {
            binding.enabled
                && binding.root == root
                && binding.repository == repository
                && binding.hub == hub
        })
        .map(|binding| binding.spool))
}

pub fn binding_for_root(root: &Path) -> Result<CaptureBinding> {
    binding_for_root_mode(root, false)
}

pub fn binding_for_reconciliation(root: &Path) -> Result<CaptureBinding> {
    binding_for_root_mode(root, true)
}

fn binding_for_root_mode(root: &Path, allow_disabled: bool) -> Result<CaptureBinding> {
    let root = client_config::canonical_root(root)?;
    let binding = load()?
        .into_iter()
        .find(|binding| binding.root == root)
        .ok_or_else(|| invalid("configured checkout binding is unavailable"))?;
    if !binding.enabled && !allow_disabled {
        return Err(invalid("configured checkout capture is disabled"));
    }
    let connection = connection_for(&root)?;
    if connection.repo != binding.repository || connection.hub != binding.hub {
        return Err(invalid(
            "capture binding namespace differs from client connection",
        ));
    }
    ensure_spool_namespace(&binding)?;
    Ok(binding)
}

pub fn configured_recipe<'a>(
    binding: &'a CaptureBinding,
    id: &str,
) -> Result<&'a ConfiguredRecipeV1> {
    binding
        .configured_recipes
        .iter()
        .find(|recipe| recipe.id == id)
        .ok_or_else(|| invalid("configured recipe id is unavailable"))
}

pub fn managed_recipe_invocations(binding: &CaptureBinding) -> Result<Vec<String>> {
    let executable = std::env::current_exe()?;
    if !executable.is_absolute() {
        return Err(invalid("AgentSCM executable path is not absolute"));
    }
    let executable = shell_quote(&executable.to_string_lossy());
    Ok(binding
        .configured_recipes
        .iter()
        .map(|recipe| {
            format!(
                "{executable} change-capture run-recipe --id {}",
                shell_quote(&recipe.id)
            )
        })
        .collect())
}

pub fn managed_recipe_invocation(binding: &CaptureBinding, id: &str) -> Result<String> {
    let index = binding
        .configured_recipes
        .iter()
        .position(|recipe| recipe.id == id)
        .ok_or_else(|| invalid("configured recipe id is unavailable"))?;
    managed_recipe_invocations(binding)?
        .into_iter()
        .nth(index)
        .ok_or_else(|| invalid("configured recipe invocation is unavailable"))
}

pub fn session_start_context(binding: &CaptureBinding) -> Result<Option<String>> {
    let commands = managed_recipe_invocations(binding)?;
    if commands.is_empty() {
        return Ok(None);
    }
    let mut context = String::from(
        "AgentSCM configured verification recipes are available for this checkout. When this task requires one, run exactly the matching command below through the ordinary Bash permission flow. Do not add arguments, combine it with another command, or substitute the underlying command. A direct uninstrumented command remains unverified.\n",
    );
    for (recipe, command) in binding.configured_recipes.iter().zip(commands) {
        context.push_str(&format!("Recipe {}: {command}\n", recipe.id));
    }
    if context.len() > MAX_SESSION_CONTEXT_BYTES {
        return Err(invalid(
            "configured recipe SessionStart guidance is too large",
        ));
    }
    Ok(Some(context))
}

fn ensure_spool_namespace(binding: &CaptureBinding) -> Result<()> {
    let expected = binding.spool_identity.ok_or_else(|| {
        invalid("capture binding lacks trusted v4 spool identity; only inspect/export is available")
    })?;
    let spool =
        LocalSpool::open_with_limits_expected(&binding.spool, binding.spool_limits, expected)
            .map_err(|error| invalid(format!("local capture: {error}")))?;
    let outcome = spool
        .bind_namespace(SpoolNamespaceV1 {
            version: 1,
            hub: binding.hub.clone(),
            repository: binding.repository.clone(),
            worktree_id: binding.worktree_id.clone(),
        })
        .map_err(|error| invalid(format!("local capture: {error}")))?;
    if outcome == NamespaceBindOutcome::Conflict {
        return Err(invalid(
            "capture spool belongs to another Hub, repository or worktree; inspect/export it and use a separately configured checkout root",
        ));
    }
    Ok(())
}

pub async fn resolve_target(
    binding: &CaptureBinding,
    session_id: &str,
) -> Result<Option<CaptureTargetV1>> {
    ensure_spool_namespace(binding)?;
    let connection = connection_for(&binding.root)?;
    if connection.repo != binding.repository || connection.hub != binding.hub {
        return Err(invalid(
            "capture binding namespace differs from client connection",
        ));
    }
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let client = forge_http::client().map_err(|_| invalid("capture Hub client unavailable"))?;
    let repo = encode_path_segment(&connection.repo);
    let request = client
        .get(format!(
            "{}/v1/hub/repos/{repo}/change-records/current",
            connection.hub
        ))
        .query(&[("session_id", session_id)])
        .bearer_auth(&token)
        .timeout(Duration::from_secs(2));
    let (status, body) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| invalid("capture target lookup unavailable"))?;
    match status {
        200 => {
            let target: CaptureTargetV1 = serde_json::from_value(body)
                .map_err(|_| invalid("capture target response is invalid"))?;
            target
                .validate()
                .map_err(|_| invalid("capture target response is invalid"))?;
            Ok(Some(target))
        }
        404 => Ok(None),
        _ => Err(invalid(format!(
            "capture target lookup returned status {status}"
        ))),
    }
}

pub async fn fetch_change_record(root: &Path, work_id: &str) -> Result<ChangeRecordResponseV1> {
    if !is_identifier(work_id, "wrk_") {
        return Err(invalid("work_id must be a durable work identity"));
    }
    let connection = connection_for(root)?;
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let client = forge_http::client().map_err(|_| invalid("change record client unavailable"))?;
    let repo = encode_path_segment(&connection.repo);
    let work = encode_path_segment(work_id);
    let request = client
        .get(format!(
            "{}/v1/hub/repos/{repo}/change-records/{work}",
            connection.hub
        ))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(5));
    let (status, body) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| invalid("change record lookup unavailable"))?;
    if status != 200 {
        return Err(invalid(format!(
            "change record lookup returned status {status}"
        )));
    }
    let response: ChangeRecordResponseV1 =
        serde_json::from_value(body).map_err(|_| invalid("change record response is invalid"))?;
    response
        .validate_encoded_size()
        .map_err(|_| invalid("change record response is too large"))?;
    Ok(response)
}

/// A request to a Work's shared-conversation resource, with the client connection's token.
fn conversation_request(
    root: &Path,
    work_id: &str,
    suffix: &str,
    method: reqwest::Method,
) -> Result<reqwest::RequestBuilder> {
    if !is_identifier(work_id, "wrk_") {
        return Err(invalid("work_id must be a durable work identity"));
    }
    let connection = connection_for(root)?;
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let client = forge_http::client().map_err(|_| invalid("conversation client unavailable"))?;
    Ok(client
        .request(
            method,
            format!(
                "{}/v1/hub/repos/{}/change-records/{}/conversation{suffix}",
                connection.hub,
                encode_path_segment(&connection.repo),
                encode_path_segment(work_id)
            ),
        )
        .bearer_auth(&token)
        .timeout(Duration::from_secs(5)))
}

fn conversation_status_error(status: u16) -> Error {
    match status {
        403 => invalid(
            "only the person who shared it and Hub admins can read or delete this conversation",
        ),
        404 => invalid("no such shared conversation body"),
        410 => invalid("this conversation body has expired or been deleted"),
        424 => invalid("the stored body is missing or failed verification (integrity_failure)"),
        _ => invalid(format!("conversation request returned status {status}")),
    }
}

pub async fn list_conversation(
    root: &Path,
    work_id: &str,
    cursor: Option<&str>,
) -> Result<ConversationPageV1> {
    let suffix = cursor.map_or_else(String::new, |cursor| {
        format!("?cursor={}", encode_path_segment(cursor))
    });
    let request = conversation_request(root, work_id, &suffix, reqwest::Method::GET)?;
    let (status, body) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| invalid("conversation lookup unavailable"))?;
    if status != 200 {
        return Err(conversation_status_error(status));
    }
    let page: ConversationPageV1 =
        serde_json::from_value(body).map_err(|_| invalid("conversation response is invalid"))?;
    page.validate_encoded_size()
        .map_err(|_| invalid("conversation response is too large"))?;
    Ok(page)
}

/// One body as JSON. It is untrusted agent-produced content: the caller must print it, not act on it.
pub async fn fetch_conversation_body(root: &Path, work_id: &str, digest: &str) -> Result<Value> {
    if !is_digest(digest) {
        return Err(invalid("digest must be 64 lowercase hex characters"));
    }
    let request = conversation_request(root, work_id, &format!("/{digest}"), reqwest::Method::GET)?;
    let (status, body) = send(request, MAX_SHARED_BODY_BYTES as usize)
        .await
        .map_err(|_| invalid("conversation body lookup unavailable"))?;
    if status != 200 {
        return Err(conversation_status_error(status));
    }
    if body.is_null() {
        return Err(invalid("conversation body is not valid JSON"));
    }
    Ok(body)
}

pub async fn delete_conversation_body(root: &Path, work_id: &str, digest: &str) -> Result<()> {
    if !is_digest(digest) {
        return Err(invalid("digest must be 64 lowercase hex characters"));
    }
    let request = conversation_request(
        root,
        work_id,
        &format!("/{digest}"),
        reqwest::Method::DELETE,
    )?;
    let (status, _) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| invalid("conversation delete unavailable"))?;
    match status {
        204 => Ok(()),
        _ => Err(conversation_status_error(status)),
    }
}

pub async fn deliver_pending(root: &Path, spool: &LocalSpool) -> Result<DeliveryReport> {
    let connection = connection_for(root)?;
    let binding = load()?
        .into_iter()
        .find(|binding| binding.root == root)
        .ok_or_else(|| invalid("capture binding is unavailable"))?;
    if binding.spool != spool.root()
        || binding.repository != connection.repo
        || binding.hub != connection.hub
    {
        return Err(invalid(
            "capture delivery namespace differs from client connection",
        ));
    }
    ensure_spool_namespace(&binding)?;
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let client = forge_http::client().map_err(|_| invalid("capture Hub client unavailable"))?;
    let pending = spool
        .delivery_page(DELIVERY_PAGE)
        .map_err(|error| invalid(format!("local capture: {error}")))?;
    let deadline = tokio::time::Instant::now() + DELIVERY_DEADLINE;
    let mut delivered = 0;
    let mut blocked = 0;
    for envelope in &pending {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let (endpoint, body) = match envelope {
            CaptureEnvelopeV1::EventBatch(value) => (
                format!(
                    "/v1/hub/repos/{}/change-records/events",
                    encode_path_segment(&connection.repo)
                ),
                serde_json::to_vec(value)
                    .map_err(|_| invalid("cannot encode capture event batch"))?,
            ),
            CaptureEnvelopeV1::ActionStart(value) => (
                format!(
                    "/v1/hub/repos/{}/change-records/actions/start",
                    encode_path_segment(&connection.repo)
                ),
                serde_json::to_vec(value)
                    .map_err(|_| invalid("cannot encode capture action start"))?,
            ),
            CaptureEnvelopeV1::ActionFinish(value) => (
                format!(
                    "/v1/hub/repos/{}/change-records/actions/finish",
                    encode_path_segment(&connection.repo)
                ),
                serde_json::to_vec(value)
                    .map_err(|_| invalid("cannot encode capture action finish"))?,
            ),
            CaptureEnvelopeV1::Usage(value) => (
                "/v1/hub/standup/usage".to_owned(),
                serde_json::to_vec(value)
                    .map_err(|_| invalid("cannot encode capture usage observation"))?,
            ),
        };
        let request = client
            .post(format!("{}{endpoint}", connection.hub))
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(&token)
            .timeout(remaining.min(Duration::from_secs(1)))
            .body(body);
        let (status, _) = match send(request, REPLY_BYTES).await {
            Ok(response) => response,
            Err(_) => {
                spool
                    .advance_delivery_cursor(envelope)
                    .map_err(|error| invalid(format!("local capture: {error}")))?;
                return Err(invalid("capture Hub delivery unavailable"));
            }
        };
        let mut uploaded = BodyUpload::Done;
        if status == 200
            && binding.capture_mode == CaptureModeV1::SharedConversation
            && let CaptureEnvelopeV1::EventBatch(batch) = envelope
        {
            uploaded =
                match upload_shared_bodies(&client, &connection, &token, spool, batch, deadline)
                    .await
                {
                    Ok(uploaded) => uploaded,
                    Err(error) => {
                        spool
                            .advance_delivery_cursor(envelope)
                            .map_err(|error| invalid(format!("local capture: {error}")))?;
                        return Err(error);
                    }
                };
        }
        if status == 200 && uploaded == BodyUpload::OutOfTime {
            // Neither acknowledged nor skipped: the next pass starts at this envelope again.
            break;
        } else if status == 200 && uploaded == BodyUpload::Blocked {
            // Bodies stay local and the envelope stays pending; replaying events is idempotent.
            spool
                .advance_delivery_cursor(envelope)
                .map_err(|error| invalid(format!("local capture: {error}")))?;
            blocked += 1;
        } else if status == 200 {
            spool
                .acknowledge(envelope)
                .map_err(|error| invalid(format!("local capture: {error}")))?;
            spool
                .advance_delivery_cursor(envelope)
                .map_err(|error| invalid(format!("local capture: {error}")))?;
            delivered += 1;
        } else if matches!(status, 400 | 401 | 403 | 404 | 409) {
            // A stale generation, revoked author or conflicting operation remains pending.
            // Continue so an old record cannot prevent unrelated current-work receipts.
            spool
                .advance_delivery_cursor(envelope)
                .map_err(|error| invalid(format!("local capture: {error}")))?;
            blocked += 1;
        } else {
            spool
                .advance_delivery_cursor(envelope)
                .map_err(|error| invalid(format!("local capture: {error}")))?;
            return Err(invalid(format!(
                "capture Hub delivery returned status {status}"
            )));
        }
    }
    let pending_page = spool
        .delivery_page(DELIVERY_PAGE)
        .map_err(|error| invalid(format!("local capture: {error}")))?
        .len();
    Ok(DeliveryReport {
        delivered,
        blocked,
        pending_page,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyUpload {
    /// Every body is on the Hub, gone from it for good, or unavailable locally.
    Done,
    /// The Hub refused a body for authorization or state reasons.
    Blocked,
    /// The pass deadline arrived first.
    OutOfTime,
}

/// Asks the Hub which of the batch's shared-approved digests still need bytes. `None` means no
/// answer (older Hub, or a refusal the uploads will repeat), so the caller offers every body.
async fn missing_digests(
    client: &reqwest::Client,
    connection: &client_config::Connection,
    token: &str,
    batch: &CaptureBatchV1,
    deadline: tokio::time::Instant,
) -> Result<Option<BTreeSet<String>>> {
    let mut digests: Vec<&str> = Vec::new();
    for artifact in batch
        .events
        .iter()
        .filter_map(|event| event.artifact.as_ref())
        .filter(|artifact| artifact.retention == RetentionClassV1::SharedApproved)
    {
        if !digests.contains(&artifact.digest.as_str()) {
            digests.push(&artifact.digest);
        }
    }
    if digests.is_empty() || digests.len() > MAX_CONVERSATION_MISSING_DIGESTS {
        return Ok(None);
    }
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    let body = serde_json::to_vec(&ConversationMissingRequestV1 {
        digests: digests.iter().map(|digest| (*digest).to_owned()).collect(),
    })
    .map_err(|_| invalid("cannot encode body preflight"))?;
    let request = client
        .post(format!(
            "{}/v1/hub/repos/{}/change-records/{}/conversation/missing",
            connection.hub,
            encode_path_segment(&connection.repo),
            encode_path_segment(&batch.work_id),
        ))
        .header(CONTENT_TYPE, "application/json")
        .bearer_auth(token)
        .timeout(remaining)
        .body(body);
    let (status, body) = send(request, REPLY_BYTES)
        .await
        .map_err(|_| invalid("capture Hub body preflight unavailable"))?;
    match status {
        200 => Ok(serde_json::from_value::<ConversationMissingV1>(body)
            .ok()
            .map(|answer| answer.missing.into_iter().collect())),
        s if s >= 500 => Err(invalid(format!(
            "capture Hub body preflight returned status {s}"
        ))),
        _ => Ok(None),
    }
}

/// Uploads the shared-approved bodies an accepted event batch names that the Hub still lacks
/// (all of them when the Hub cannot say). A transient failure is an error so the caller leaves
/// the envelope pending; a body missing from the spool is skipped and stays declared on the Hub.
async fn upload_shared_bodies(
    client: &reqwest::Client,
    connection: &client_config::Connection,
    token: &str,
    spool: &LocalSpool,
    batch: &CaptureBatchV1,
    deadline: tokio::time::Instant,
) -> Result<BodyUpload> {
    let only = missing_digests(client, connection, token, batch, deadline).await?;
    let mut seen = BTreeSet::new();
    for artifact in batch
        .events
        .iter()
        .filter_map(|event| event.artifact.as_ref())
    {
        if artifact.retention != RetentionClassV1::SharedApproved
            || !seen.insert(artifact.digest.as_str())
        {
            continue;
        }
        // Available or gone on the Hub: nothing to send.
        if only
            .as_ref()
            .is_some_and(|missing| !missing.contains(&artifact.digest))
        {
            continue;
        }
        let bytes = match spool.read_artifact(artifact) {
            Ok(bytes) => bytes,
            // Only a transient read failure is worth retrying; an absent, corrupt or unreadable
            // body will not come back, so it stays declared on the Hub.
            Err(CaptureError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(invalid(format!("local capture: {error}")));
            }
            Err(_) => continue,
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(BodyUpload::OutOfTime);
        }
        let request = client
            .put(format!(
                "{}/v1/hub/repos/{}/change-records/{}/conversation/{}",
                connection.hub,
                encode_path_segment(&connection.repo),
                encode_path_segment(&batch.work_id),
                artifact.digest
            ))
            .header(CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .timeout(remaining)
            .body(bytes);
        let (status, _) = send(request, REPLY_BYTES)
            .await
            .map_err(|_| invalid("capture Hub body upload unavailable"))?;
        match status {
            // Gone: its author deleted the body or it expired, so there is nothing to retry.
            204 | 410 => {}
            400 | 401 | 403 | 404 | 409 => return Ok(BodyUpload::Blocked),
            _ => {
                return Err(invalid(format!(
                    "capture Hub body upload returned status {status}"
                )));
            }
        }
    }
    Ok(BodyUpload::Done)
}

pub fn open_bound_spool(binding: &CaptureBinding) -> Result<LocalSpool> {
    let expected = binding.spool_identity.ok_or_else(|| {
        invalid("capture binding lacks trusted v4 spool identity; only inspect/export is available")
    })?;
    LocalSpool::open_with_limits_expected(&binding.spool, binding.spool_limits, expected)
        .map_err(|error| invalid(format!("local capture: {error}")))
}

pub fn open_spool_path(path: &Path) -> Result<LocalSpool> {
    let configured = load()?.into_iter().find(|binding| binding.spool == path);
    if let Some(binding) = configured {
        return open_bound_spool(&binding);
    }
    let inspection = inspect_spool(path)
        .map_err(|error| invalid(format!("local capture inspection: {error}")))?;
    let expected = inspection.committed_identity.ok_or_else(|| {
        invalid("capture mutation requires an exact committed v4 spool; inspect/export only")
    })?;
    if inspection.classification != SpoolClassificationV1::V4IdentityMatch {
        return Err(invalid(
            "capture mutation requires an exact committed v4 spool; inspect/export only",
        ));
    }
    LocalSpool::open_with_limits_expected(path, SpoolLimits::default(), expected)
        .map_err(|error| invalid(format!("local capture: {error}")))
}

fn install_hooks(root_path: &Path, path: &Path, enabled: bool) -> Result<()> {
    let location = settings_transaction::SettingsLocation::open(root_path, path)?;
    let retained = location
        .read_target()?
        .map(|bytes| {
            String::from_utf8(bytes).map_err(|_| invalid("Claude settings are not valid UTF-8"))
        })
        .transpose()?;
    let existed = retained.is_some();
    let existing = match retained {
        Some(existing) => existing,
        None if !enabled => return Ok(()),
        None => "{}".into(),
    };
    let mut root: Value = serde_json::from_str(&existing)
        .map_err(|_| invalid("Claude settings are not valid JSON"))?;
    let root = root
        .as_object_mut()
        .ok_or_else(|| invalid("Claude settings must be a JSON object"))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid("Claude settings hooks must be an object"))?;
    let command = format!(
        "{HOOK_MARKER}exec {} change-capture hook-claude",
        shell_quote(&std::env::current_exe()?.to_string_lossy())
    );
    for event in CLAUDE_EVENTS {
        let groups = hooks.entry(event).or_insert_with(|| json!([]));
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| invalid(format!("Claude settings hooks.{event} must be an array")))?;
        remove_owned_hooks(groups);
        if enabled {
            groups.push(json!({"hooks": [{"type": "command", "command": command,
                "timeout": HOOK_TIMEOUT_SECONDS}]}));
        }
    }
    hooks.retain(|_, value| value.as_array().is_none_or(|groups| !groups.is_empty()));
    if hooks.is_empty() {
        root.remove("hooks");
    }
    let text = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|_| invalid("cannot encode Claude settings"))?;
    let replacement = format!("{text}\n");
    if existed && replacement.as_bytes() == existing.as_bytes() {
        return Ok(());
    }
    settings_transaction::replace(
        &location,
        existed.then_some(existing.as_bytes()),
        replacement.as_bytes(),
        enabled,
    )
}

fn remove_owned_hooks(groups: &mut Vec<Value>) {
    groups.retain_mut(|group| {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        hooks.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.starts_with(HOOK_MARKER))
        });
        !hooks.is_empty()
    });
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureBinding, CaptureSetting, ExpectedTarget, ForgetArgs, HOOK_MARKER, UnconfigureArgs,
        acquire_lock, deliver_pending, disable_binding, enable_capture, forget, install_hooks,
        is_digest, is_identifier, load, load_recipe_file, load_recipe_from_file, lock_checkout,
        managed_recipe_invocations, parse_rule, plan_enable, remove_owned_hooks, save,
        session_start_context, set_capture_mode, setting_in, unconfigure, validate_binding,
        validate_binding_update,
    };
    use agentscm::change_capture::claude_code::{ActionRuleV1, NativeResultSourceV1};
    use agentscm::change_capture::{
        ConfiguredRecipeV1, FileIdentityV1, LocalSpool, MIN_ACTION_CAPTURE_BYTES,
        MIN_ACTION_CAPTURE_RECORDS, SpoolIdentityV1, SpoolLimits,
    };
    use agentscm::change_record::{ActionKindV1, CaptureModeV1, CoverageV1};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    fn recipe_binding(recipes: Vec<ConfiguredRecipeV1>) -> CaptureBinding {
        CaptureBinding {
            enabled: true,
            root: "/tmp/agentscm-recipe-root".into(),
            spool: "/tmp/agentscm-recipe-spool".into(),
            spool_identity: Some(SpoolIdentityV1 {
                version: 4,
                root: FileIdentityV1 {
                    device: 1,
                    inode: 2,
                },
                spool_lock: FileIdentityV1 {
                    device: 1,
                    inode: 3,
                },
                managed_lock: FileIdentityV1 {
                    device: 1,
                    inode: 4,
                },
            }),
            repository: "counterbranch/agentscm".into(),
            hub: "http://127.0.0.1:49321".into(),
            worktree_id: "checkout_test".into(),
            base_ref: "origin/main".into(),
            setup_digest: "a".repeat(64),
            capture_mode: CaptureModeV1::MetadataOnly,
            claude_history_root: None,
            claude_settings: None,
            action_rules: vec![ActionRuleV1 {
                native_tool: "Bash".into(),
                kind: ActionKindV1::Execution,
                coverage: CoverageV1::Partial,
                result_source: NativeResultSourceV1::Unverified,
                recipe_digest: None,
            }],
            configured_recipes: recipes,
            spool_limits: SpoolLimits::default(),
        }
    }

    fn recipe(id: &str) -> ConfiguredRecipeV1 {
        ConfiguredRecipeV1 {
            id: id.into(),
            argv: vec!["/usr/bin/true".into()],
            coverage: CoverageV1::Complete,
            timeout_seconds: 30,
            max_output_bytes: 4096,
        }
    }

    #[test]
    fn action_rules_separate_execution_from_exact_verification() {
        let execution = parse_rule("Bash=execution:partial:process_exit_code").unwrap();
        assert_eq!(execution.kind, ActionKindV1::Execution);
        assert_eq!(execution.recipe_digest, None);
        let digest = "a".repeat(64);
        let verification = parse_rule(&format!(
            "Bash=verification:complete:process_exit_code:{digest}"
        ))
        .unwrap();
        assert_eq!(verification.kind, ActionKindV1::Verification);
        assert_eq!(verification.recipe_digest.as_deref(), Some(digest.as_str()));
        assert!(parse_rule("Bash=verification:complete:process_exit_code").is_err());
        assert!(
            parse_rule(&format!(
                "Bash=execution:partial:process_exit_code:{digest}"
            ))
            .is_err()
        );
    }

    #[test]
    fn session_guidance_names_only_exact_managed_recipe_invocations() {
        let binding = recipe_binding(vec![recipe("unit-test")]);
        validate_binding(&binding).unwrap();
        let commands = managed_recipe_invocations(&binding).unwrap();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains(" change-capture run-recipe --id 'unit-test'"));
        let context = session_start_context(&binding).unwrap().unwrap();
        assert!(context.contains(&commands[0]));
        assert!(context.contains("ordinary Bash permission flow"));
        assert!(context.contains("direct uninstrumented command remains unverified"));
        assert!(!context.contains("/usr/bin/true"));
    }

    #[test]
    fn configured_recipe_ids_are_unique_and_bounded() {
        let duplicate = recipe_binding(vec![recipe("same"), recipe("same")]);
        assert!(validate_binding(&duplicate).is_err());
        let mut too_many = Vec::new();
        for index in 0..17 {
            too_many.push(recipe(&format!("recipe-{index}")));
        }
        assert!(validate_binding(&recipe_binding(too_many)).is_err());
    }

    #[test]
    fn configured_spool_must_admit_one_action_and_terminal_receipt() {
        let mut binding = recipe_binding(vec![recipe("unit-test")]);
        binding.spool_limits = SpoolLimits {
            max_records: MIN_ACTION_CAPTURE_RECORDS - 1,
            max_total_bytes: MIN_ACTION_CAPTURE_BYTES,
            max_artifact_bytes: 4096,
        };
        assert!(validate_binding(&binding).is_err());
        binding.spool_limits.max_records = MIN_ACTION_CAPTURE_RECORDS;
        binding.spool_limits.max_total_bytes = MIN_ACTION_CAPTURE_BYTES - 1;
        assert!(validate_binding(&binding).is_err());
        binding.spool_limits.max_total_bytes = MIN_ACTION_CAPTURE_BYTES;
        assert!(validate_binding(&binding).is_ok());
    }

    #[test]
    fn binding_updates_preserve_spool_lineage_and_completion_headroom() {
        let original = recipe_binding(vec![recipe("unit-test")]);
        let mut current = original.clone();
        current.setup_digest = "b".repeat(64);
        current.base_ref = "refs/heads/next".into();
        current.capture_mode = CaptureModeV1::LocalConversation;
        current.action_rules = vec![ActionRuleV1 {
            native_tool: "Write".into(),
            kind: ActionKindV1::Execution,
            coverage: CoverageV1::Unknown,
            result_source: NativeResultSourceV1::Unverified,
            recipe_digest: None,
        }];
        current.configured_recipes = vec![recipe("replacement-test")];
        current.spool = "/private/moved-same-spool".into();
        current.spool_limits.max_records += 1;
        current.spool_limits.max_total_bytes += 1;
        current.spool_limits.max_artifact_bytes += 1;
        assert!(validate_binding_update(&original, &current).is_ok());

        let mut replaced = current.clone();
        replaced.spool_identity.as_mut().unwrap().root.inode += 1;
        assert!(validate_binding_update(&original, &replaced).is_err());

        let mut other_namespace = current.clone();
        other_namespace.repository = "counterbranch/other".into();
        assert!(validate_binding_update(&original, &other_namespace).is_err());

        let mut lowered = current.clone();
        lowered.spool_limits = original.spool_limits;
        lowered.spool_limits.max_records -= 1;
        assert!(validate_binding_update(&original, &lowered).is_err());

        let mut legacy = original.clone();
        legacy.spool_identity = None;
        assert!(validate_binding_update(&legacy, &current).is_err());
    }

    #[test]
    fn disabling_capture_retains_the_spool_lineage() {
        let binding = recipe_binding(vec![recipe("unit-test")]);
        let expected_identity = binding.spool_identity;
        let expected_spool = binding.spool.clone();
        let mut bindings = vec![binding];
        assert!(disable_binding(
            &mut bindings,
            std::path::Path::new("/tmp/agentscm-recipe-root")
        ));
        assert!(!bindings[0].enabled);
        assert_eq!(bindings[0].spool_identity, expected_identity);
        assert_eq!(bindings[0].spool, expected_spool);
        assert!(!disable_binding(
            &mut bindings,
            std::path::Path::new("/tmp/agentscm-recipe-root")
        ));
        assert_eq!(bindings.len(), 1);
    }

    #[test]
    fn setting_reports_configured_enabled_and_mode_for_that_root_only() {
        let root = std::path::Path::new("/tmp/agentscm-recipe-root");
        let other = std::path::Path::new("/tmp/other");
        let mut bindings = vec![recipe_binding(vec![])];
        let setting = |bindings: &[CaptureBinding], root| setting_in(bindings, root);
        assert_eq!(
            setting(&bindings, root),
            CaptureSetting {
                configured: true,
                enabled: true,
                mode: Some(CaptureModeV1::MetadataOnly)
            }
        );
        bindings[0].capture_mode = CaptureModeV1::SharedConversation;
        assert_eq!(
            setting(&bindings, root).mode,
            Some(CaptureModeV1::SharedConversation)
        );
        assert_eq!(
            setting(&bindings, other),
            CaptureSetting {
                configured: false,
                enabled: false,
                mode: None
            }
        );
        bindings[0].enabled = false;
        assert_eq!(
            setting(&bindings, root),
            CaptureSetting {
                configured: true,
                enabled: false,
                mode: None
            }
        );
    }

    #[test]
    fn recipe_loader_reads_the_single_validated_inode_across_path_replacement() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recipe.json");
        let original = recipe("original");
        std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .unwrap();

        let retained = directory.path().join("retained.json");
        std::fs::rename(&path, &retained).unwrap();
        let replacement = recipe("replacement");
        std::fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(load_recipe_from_file(opened).unwrap(), original);
        assert_eq!(load_recipe_file(&path).unwrap(), replacement);
        let link = directory.path().join("linked.json");
        symlink(&path, &link).unwrap();
        assert!(load_recipe_file(&link).is_err());
    }

    #[test]
    fn hook_removal_preserves_foreign_entries_and_mixed_groups() {
        let mut groups = vec![
            json!({"hooks": [{"type":"command","command":"keep"}]}),
            json!({"hooks": [
                {"type":"command","command":format!("{HOOK_MARKER}exec agentscm")},
                {"type":"command","command":"also-keep"}
            ]}),
            json!({"hooks": [
                {"type":"command","command":format!("{HOOK_MARKER}exec old")}
            ]}),
        ];
        remove_owned_hooks(&mut groups);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["hooks"][0]["command"], "keep");
        assert_eq!(groups[1]["hooks"][0]["command"], "also-keep");
    }

    #[test]
    fn repeated_hook_install_and_removal_are_noop_after_the_first_update() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let settings = root_path.join(".claude/settings.local.json");
        install_hooks(&root_path, &settings, true).unwrap();
        let after_install = std::fs::read_dir(settings.parent().unwrap())
            .unwrap()
            .count();
        install_hooks(&root_path, &settings, true).unwrap();
        assert_eq!(
            std::fs::read_dir(settings.parent().unwrap())
                .unwrap()
                .count(),
            after_install
        );
        install_hooks(&root_path, &settings, false).unwrap();
        let after_removal = std::fs::read_dir(settings.parent().unwrap())
            .unwrap()
            .count();
        install_hooks(&root_path, &settings, false).unwrap();
        assert_eq!(
            std::fs::read_dir(settings.parent().unwrap())
                .unwrap()
                .count(),
            after_removal
        );
    }

    fn connection(root: &str, hub: &str) -> crate::client_config::Connection {
        crate::client_config::Connection {
            root: root.into(),
            origin_host: "github.com".into(),
            hub: hub.into(),
            repo: "counterbranch/agentscm".into(),
            token_file: "/synthetic/token".into(),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        }
    }

    #[test]
    fn a_new_binding_gets_random_ids_and_a_saved_one_keeps_its_own() {
        let spools = tempfile::tempdir().unwrap();
        let hub = "http://127.0.0.1:49321";
        let plan = |bindings: &[CaptureBinding], root: &str| {
            plan_enable(
                bindings,
                &connection(root, hub),
                std::path::Path::new(root),
                CaptureModeV1::SharedConversation,
                spools.path(),
            )
        };
        let first = plan(&[], "/synthetic/one").unwrap().unwrap();
        let second = plan(&[], "/synthetic/two").unwrap().unwrap();
        let repeat = plan(&[], "/synthetic/one").unwrap().unwrap();
        for (left, right) in [(&first, &second), (&first, &repeat)] {
            assert_ne!(left.worktree_id, right.worktree_id);
            assert_ne!(left.setup_digest, right.setup_digest);
            assert_ne!(left.spool, right.spool);
        }
        assert!(first.spool.starts_with(spools.path()));
        assert!(is_identifier(&first.worktree_id, "checkout_") && first.worktree_id.len() == 41);
        assert!(is_digest(&first.setup_digest));
        assert_eq!(first.base_ref, "origin/main");

        let mut saved = recipe_binding(vec![]);
        saved.enabled = false;
        saved.claude_settings = Some("/tmp/agentscm-recipe-root/tools/claude.json".into());
        validate_binding(&saved).unwrap();
        let again = plan(&[saved.clone()], "/tmp/agentscm-recipe-root")
            .unwrap()
            .unwrap();
        assert_eq!(again.worktree_id, saved.worktree_id);
        assert_eq!(again.setup_digest, saved.setup_digest);
        assert_eq!(again.spool, saved.spool);
        assert_eq!(Some(again.settings), saved.claude_settings);

        saved.enabled = true;
        assert!(
            plan(&[saved.clone()], "/tmp/agentscm-recipe-root")
                .unwrap()
                .is_none()
        );

        // A disabled capture saved for another Hub is not reused; the error names the way out.
        saved.enabled = false;
        saved.hub = "http://127.0.0.1:1".into();
        let error = plan(&[saved], "/tmp/agentscm-recipe-root").err().unwrap();
        let message = error.to_string();
        assert!(message.contains("change-capture forget"), "{message}");
        assert!(message.contains("http://127.0.0.1:1"), "{message}");
    }

    #[test]
    fn a_settings_path_outside_the_checkout_is_not_a_valid_binding() {
        let mut binding = recipe_binding(vec![]);
        binding.claude_settings = Some("/tmp/elsewhere/settings.json".into());
        assert!(validate_binding(&binding).is_err());
        binding.claude_settings = Some("/tmp/agentscm-recipe-root/../x/settings.json".into());
        assert!(validate_binding(&binding).is_err());
    }

    /// A real Git checkout connected to `http://127.0.0.1:9`, with the config dir pointed at a temp dir.
    fn connected_checkout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let config = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process, so no other thread reads it.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config.path()) };
        let root = config.path().canonicalize().unwrap().join("widgets");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let agentscm = config.path().join("agentscm");
        std::fs::create_dir_all(&agentscm).unwrap();
        connect(&agentscm, &root, "http://127.0.0.1:9");
        (config, root, agentscm)
    }

    fn connect(agentscm: &std::path::Path, root: &std::path::Path, hub: &str) {
        std::fs::write(
            agentscm.join("connections.json"),
            json!({"version": 1, "connections": [{
                "root": root, "origin_host": "github.com", "hub": hub,
                "repo": "acme/widgets", "token_file": agentscm.join("token")
            }]})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn sharing_is_refused_for_a_connection_the_person_did_not_see_and_bindings_are_read_under_the_lock()
     {
        let (config, root, agentscm) = connected_checkout();
        let shared = CaptureModeV1::SharedConversation;
        let seen = |hub| ExpectedTarget {
            repository: "acme/widgets",
            hub,
        };
        let elsewhere = seen("https://hub.example");

        let error = enable_capture(&root, shared, elsewhere)
            .unwrap_err()
            .to_string();
        assert!(error.contains("now connected to") && error.contains("refresh and confirm again"));
        let error = set_capture_mode(&root, shared, Some(elsewhere))
            .unwrap_err()
            .to_string();
        assert!(error.contains("now connected to"), "{error}");
        let other_repository = ExpectedTarget {
            repository: "acme/other",
            hub: "http://127.0.0.1:9",
        };
        assert!(enable_capture(&root, shared, other_repository).is_err());
        assert!(!agentscm.join("change-capture.json").exists());
        // Turning sharing off needs no confirmation, so the expectation is not consulted.
        let error = set_capture_mode(&root, CaptureModeV1::MetadataOnly, Some(elsewhere))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no enabled capture binding"), "{error}");

        // A writer that holds the lock decides first: what is read afterwards sees its binding.
        let lock = acquire_lock().unwrap();
        let reader = {
            let root = root.clone();
            std::thread::spawn(move || {
                lock_checkout(&root, Some(seen("http://127.0.0.1:9")))
                    .map(|locked| locked.bindings.len())
            })
        };
        let mut binding = recipe_binding(vec![]);
        binding.root = root.clone();
        binding.spool = config.path().join("spool");
        save(&[binding]).unwrap();
        drop(lock);
        assert_eq!(reader.join().unwrap().unwrap(), 1);
    }

    #[test]
    fn a_reconnect_during_enable_is_not_overtaken_and_enable_refuses_the_new_hub() {
        let (_config, root, agentscm) = connected_checkout();
        let held = crate::client_config::lock_connections().unwrap();
        let enable = {
            let root = root.clone();
            std::thread::spawn(move || {
                enable_capture(
                    &root,
                    CaptureModeV1::SharedConversation,
                    ExpectedTarget {
                        repository: "acme/widgets",
                        hub: "http://127.0.0.1:9",
                    },
                )
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !enable.is_finished(),
            "enable must wait for the connections lock"
        );
        connect(&agentscm, &root, "https://hub.example");
        drop(held);
        let error = enable.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("on https://hub.example"), "{error}");
        assert!(!agentscm.join("change-capture.json").exists());
    }

    #[test]
    fn delivery_uploads_nothing_when_the_connection_no_longer_matches_the_binding() {
        let (config, root, agentscm) = connected_checkout();
        let spool_root = config.path().join("spool");
        let spool = LocalSpool::open_with_limits(&spool_root, SpoolLimits::default()).unwrap();
        let mut binding = recipe_binding(vec![]);
        binding.root = root.clone();
        binding.spool = spool.root().to_owned();
        binding.repository = "acme/widgets".into();
        binding.hub = "http://127.0.0.1:9".into();
        save(&[binding]).unwrap();
        connect(&agentscm, &root, "https://hub.example");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(deliver_pending(&root, &spool))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("namespace differs from client connection"),
            "{error}"
        );
    }

    #[test]
    fn a_deleted_checkout_can_be_unconfigured_and_forgotten_and_keeps_its_spool() {
        let (config, root, _agentscm) = connected_checkout();
        let mut binding = recipe_binding(vec![]);
        binding.root = root.clone();
        binding.spool = config.path().join("spool");
        std::fs::create_dir(&binding.spool).unwrap();
        binding.claude_settings = None;
        save(&[binding.clone()]).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        let given = root.join("sub/..");
        let forget_args = || ForgetArgs {
            root: given.clone(),
        };

        let error = forget(forget_args()).unwrap_err().to_string();
        assert!(error.contains("still enabled"), "{error}");
        let result = unconfigure(UnconfigureArgs {
            root: given.clone(),
            claude_settings: None,
        })
        .unwrap();
        assert_eq!(result["disabled"], true);
        assert_eq!(result["hooks_removed"], false);
        let result = forget(forget_args()).unwrap();
        assert_eq!(result["spool"], json!(binding.spool));
        assert!(binding.spool.exists());
        assert!(load().unwrap().is_empty());
    }
}
