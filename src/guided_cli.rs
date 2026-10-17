//! Trusted-local composition only. Authority remains responsible for admission.
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::checks::{CheckLimits, SourcePathExpectation, SourcePathsRecipe, SourcePathsRequest};
use agentscm::context::{
    BaseState, ChangeId, InputIdentity, RequiredEvidence, ReviewContext, SUPPORTED_CAPABILITIES,
};
use agentscm::model::{
    Approval, EventKind, IntegrateRequest, IntegrationEvent, LandingPreparation,
    PrepareLandingRequest, TargetState,
};
use agentscm::store::{Engine, MAX_DIFF_PAGE, digest};
use agentscm::workspace::Workspace;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_SESSION: usize = 256 * 1024;
const MAX_VIEW: usize = 4 * 1024 * 1024;
const MAX_DIFF: usize = MAX_DIFF_PAGE;

#[derive(Debug, Args)]
pub struct StartArgs {
    #[arg(long)]
    change: String,
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    revision: String,
    #[arg(long)]
    issue: String,
    #[arg(long)]
    intent: String,
    #[arg(long, required = true)]
    criterion: Vec<String>,
    #[arg(long)]
    scope: Vec<String>,
    #[arg(long)]
    exclusion: Vec<String>,
    #[arg(long)]
    omission: Vec<String>,
    /// Declared recipe identity, not a claim that it ran.
    #[arg(long)]
    recipe: String,
    #[arg(long)]
    toolchain: String,
    #[arg(long)]
    environment: String,
    #[arg(long, default_value = "main")]
    target: String,
    /// Require a controller source-integrity check, PATH=SHA256. Not Cargo execution.
    #[arg(long)]
    require_source_path: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    Start(Box<StartArgs>),
    /// Inspect the retained session without retrying a pending operation.
    Status,
    /// Relinquish an interrupted start without deleting its Change or workspace.
    CancelStart {
        reason: String,
    },
    /// Show durable state; retry an interrupted recorded operation exactly.
    Resume {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Checkpoint {
        operation_id: String,
        expected_generation: u64,
    },
    /// Change criteria at the current generation, invalidating prior approvals.
    Criteria {
        #[arg(long, required = true)]
        criterion: Vec<String>,
    },
    /// Explicitly rebind source, target and policy after drift. Does not merge source.
    Refresh,
    Prepare {
        operation_id: String,
    },
    /// Complete bounded diff with file bytes; incomplete output cannot authorize approval.
    Inspect {
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Run the declared built-in source check for the exact current context.
    Check {
        attempt_id: String,
    },
    /// Retain an unverified operator test claim; never satisfies required checks.
    Note {
        #[arg(long)]
        command: String,
        #[arg(long)]
        environment: String,
        #[arg(long)]
        result: String,
    },
    Approve {
        approval_id: String,
        #[arg(long)]
        inspection: String,
    },
    Integrate {
        operation_id: String,
        #[arg(long)]
        inspection: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u32,
    issue: String,
    workspace: PathBuf,
    workspace_binding: Option<Value>,
    #[serde(default)]
    workspace_identity: Option<Value>,
    #[serde(default)]
    restored_workspace_ack: Option<PathBuf>,
    #[serde(default)]
    restored_instance_ack: Option<String>,
    context: ReviewContext,
    recipe: Option<SourcePathsRecipe>,
    pending: Option<Pending>,
    prepared_tip: Option<String>,
    inspection: Option<String>,
    approved: Option<IntegrateRequest>,
    attempt: Option<String>,
    notes: Vec<Value>,
    event: Option<IntegrationEvent>,
    #[serde(default)]
    already_current: bool,
    #[serde(default)]
    cancelled_start: Option<String>,
    empty_baseline: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Pending {
    Start,
    Checkpoint {
        operation_id: String,
        generation: u64,
    },
    Update {
        expected: u64,
        context: Box<ReviewContext>,
    },
    Prepare {
        request: Box<PrepareLandingRequest>,
    },
    Integrate {
        request: Box<IntegrateRequest>,
    },
}
struct Journal {
    db: Connection,
    _lock: agentscm::lease::Lease,
}
impl Journal {
    fn open(data: &Path) -> Result<Self> {
        if !data.join("state.sqlite").is_file() {
            return Err(Error::Invalid(
                "workflow requires an initialized data directory".into(),
            ));
        }
        let lock = agentscm::lease::Lease::try_exclusive(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(data.join("workflow.lock"))?,
        )
        .map_err(|_| {
            Error::Conflict(
                "another guided workflow command is active; retry after it finishes".into(),
            )
        })?;
        let db = Connection::open(data.join("workflow.sqlite"))?;
        let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if (app, version) != (0, 0) && (app, version) != (0x41534746, 1) {
            return Err(Error::Invalid(
                "unsupported workflow journal identity/version".into(),
            ));
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS session (id INTEGER PRIMARY KEY CHECK(id=1), state BLOB NOT NULL); PRAGMA application_id=1095976774; PRAGMA user_version=1;")?;
        Ok(Self { db, _lock: lock })
    }
    fn load(&self) -> Result<Option<Session>> {
        let bytes: Option<Vec<u8>> = self
            .db
            .query_row("SELECT state FROM session WHERE id=1", [], |r| r.get(0))
            .optional()?;
        bytes
            .map(|b| {
                if b.len() > MAX_SESSION {
                    return Err(Error::Limit("workflow journal exceeds limit".into()));
                }
                let s: Session = serde_json::from_slice(&b)?;
                if s.version != 1 {
                    return Err(Error::Invalid(
                        "unsupported workflow journal version".into(),
                    ));
                }
                Ok(s)
            })
            .transpose()
    }
    fn save(&self, s: &Session) -> Result<()> {
        let mut stored = s.clone();
        if stored.cancelled_start.is_none()
            && stored.event.is_none()
            && !stored.already_current
            && stored.workspace.exists()
        {
            let identity = workspace_identity(&stored.workspace)?;
            if stored.workspace_identity.is_some() && stored.workspace_identity != identity {
                return Err(Error::Conflict(
                    "workspace directory was replaced; explicitly reattach it".into(),
                ));
            }
            stored.workspace_identity = identity;
            stored.workspace_binding = Some(serde_json::to_value(
                Workspace::open(&stored.workspace)?.state()?,
            )?);
        }
        let bytes = serde_json::to_vec(&stored)?;
        if bytes.len() > MAX_SESSION {
            return Err(Error::Limit("workflow journal exceeds limit".into()));
        }
        self.db.execute("INSERT INTO session(id,state) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET state=excluded.state", [bytes])?;
        Ok(())
    }
}

pub fn run(data: &Path, command: WorkflowCommand) -> Result<Value> {
    let data = data.canonicalize()?;
    let _access = agentscm::local_recovery::AccessGuard::shared(&data)?;
    let restored_root = restored_root(&data)?;
    let restored_instance = restored_root
        .as_ref()
        .map(|root| agentscm::local_recovery::restore_instance(root))
        .transpose()?;
    let journal = Journal::open(&data)?;
    let mut authority = Authority::open(&data)?;
    let engine = Engine::open(&data)?;
    let mut s = if let WorkflowCommand::Start(args) = command {
        if journal
            .load()?
            .is_some_and(|s| s.event.is_none() && !s.already_current && s.cancelled_start.is_none())
        {
            return Err(Error::Conflict(
                "one active workflow already exists; use workflow resume".into(),
            ));
        }
        let mut s = start_session(&data, &authority, *args)?;
        s.restored_workspace_ack = restored_root.clone();
        s.restored_instance_ack = restored_instance.clone();
        engine.load_revision(&s.context.revision)?;
        if s.context.base.is_none() {
            s.empty_baseline = Some(engine.checkpoint(None, &[])?);
        }
        journal.save(&s)?;
        finish(&data, &journal, &mut authority, &engine, &mut s)?;
        return summary(&authority, &engine, &s);
    } else {
        journal.load()?.ok_or_else(|| {
            Error::NotFound("no active guided workflow; use workflow start".into())
        })?
    };
    // Journal-only status and completed receipts never need a mutable workspace.
    if matches!(&command, WorkflowCommand::Status) {
        return retained_status(&authority, &s, &restored_root, &restored_instance);
    }
    if let WorkflowCommand::CancelStart { reason } = &command {
        if !matches!(s.pending, Some(Pending::Start)) || reason.is_empty() || reason.len() > 1024 {
            return Err(Error::Conflict(
                "cancel-start requires an interrupted start and a 1..1024-byte reason".into(),
            ));
        }
        s.cancelled_start = Some(reason.clone());
        s.pending = None;
        invalidate(&mut s);
        journal.save(&s)?;
    }
    if s.cancelled_start.is_some() {
        return Ok(
            json!({"ok":true,"cancelled_start":s.cancelled_start,"issue":s.issue,"change":s.context.change,"preserved_workspace":s.workspace}),
        );
    }
    if s.event.is_some() || s.already_current {
        if matches!(&command, WorkflowCommand::Resume { .. }) {
            return summary(&authority, &engine, &s);
        }
        return Err(Error::Conflict(
            "workflow is complete; resume shows its receipt, or start another Change".into(),
        ));
    }
    if let Some(Pending::Integrate { request }) = &s.pending
        && find_event(&authority, &request.operation_id)?
            .as_ref()
            .is_some_and(|event| event_matches(event, request))
        && matches!(&command, WorkflowCommand::Resume { .. })
    {
        finish(&data, &journal, &mut authority, &engine, &mut s)?;
        return summary(&authority, &engine, &s);
    }
    let explicit_reattach = matches!(&command, WorkflowCommand::Resume { workspace: Some(_) });
    if restored_root.is_some()
        && (s.restored_workspace_ack != restored_root
            || s.restored_instance_ack != restored_instance)
        && !explicit_reattach
    {
        return Err(Error::Conflict("restored workflow requires explicit workflow resume --workspace PATH before any original workspace access".into()));
    }
    if !explicit_reattach {
        require_workspace_identity(&s)?;
    }
    if let WorkflowCommand::Resume { workspace } = command {
        if let Some(path) = workspace {
            let root = path.canonicalize()?;
            if root.starts_with(&data) || data.starts_with(&root) {
                return Err(Error::Invalid(
                    "reattached workspace must be separate from data".into(),
                ));
            }
            validate_reattachment(&engine, &s, &root)?;
            s.workspace_identity = workspace_identity(&root)?;
            s.workspace = root;
            s.restored_workspace_ack = restored_root.clone();
            s.restored_instance_ack = restored_instance.clone();
            invalidate(&mut s);
            journal.save(&s)?;
        }
        finish(&data, &journal, &mut authority, &engine, &mut s)?;
        return summary(&authority, &engine, &s);
    }
    if s.pending.is_some() && !matches!(command, WorkflowCommand::Refresh) {
        return Err(Error::Conflict(
            "recorded operation needs workflow resume, or explicit refresh after reconciliation"
                .into(),
        ));
    }
    match command {
        WorkflowCommand::Checkpoint {
            operation_id,
            expected_generation,
        } => {
            require_current(&authority, &s)?;
            invalidate(&mut s);
            s.prepared_tip = None;
            s.pending = Some(Pending::Checkpoint {
                operation_id,
                generation: expected_generation,
            });
        }
        WorkflowCommand::Criteria { criterion } => {
            require_current(&authority, &s)?;
            let mut context = s.context.clone();
            context.acceptance_criteria = criterion;
            queue_update(&mut s, context)?;
        }
        WorkflowCommand::Refresh => {
            if matches!(s.pending, Some(Pending::Start)) {
                return Err(Error::Conflict(
                    "resume the recorded operation before refreshing".into(),
                ));
            }
            if let Some(Pending::Integrate { request }) = &s.pending {
                if let Some(event) = find_event(&authority, &request.operation_id)?
                    && event_matches(&event, request)
                {
                    return Err(Error::Conflict("integration already completed; workflow resume retrieves its receipt before refresh".into()));
                }
                // No matching completed operation in the intact bounded journal.
                // Revoke before discarding the uncompleted local request.
                match authority.revoke_approval(&request.approval_id) {
                    Ok(()) | Err(Error::NotFound(_)) => (),
                    Err(error) => return Err(error),
                }
            }
            if Workspace::open(&s.workspace)?.state()?.pending.is_some() {
                return Err(Error::Conflict(
                    "recover the interrupted workspace operation before refresh".into(),
                ));
            }
            let current = authority.change(&s.context.change)?;
            s.context = authority.review_context(&current.context_digest)?;
            let mut context = s.context.clone();
            context.revision = Workspace::open(&s.workspace)?.state()?.base_revision;
            context.base = base(authority.target(&context.target)?);
            context.policy_epoch = authority.policy_epoch()?;
            if context.base.is_none() && s.empty_baseline.is_none() {
                s.empty_baseline = Some(engine.checkpoint(None, &[])?);
            }
            s.pending = None;
            queue_update(&mut s, context)?;
            s.prepared_tip = None;
        }
        WorkflowCommand::Prepare { operation_id } => {
            require_current(&authority, &s)?;
            let workspace = clean_workspace(&engine, &s)?;
            if s.prepared_tip.as_ref() == Some(&workspace) {
                return summary(&authority, &engine, &s);
            }
            let mut context = s.context.clone();
            context.revision = workspace.clone();
            if let Some(expected_target) = target_from_base(&context) {
                context.change_version = next(context.change_version)?;
                context.validate()?;
                s.pending = Some(Pending::Prepare {
                    request: Box::new(PrepareLandingRequest {
                        operation_id,
                        target: context.target.clone(),
                        expected_target,
                        source_tip: workspace,
                        change_version: s.context.change_version,
                        context,
                    }),
                });
                invalidate(&mut s);
            } else {
                queue_update(&mut s, context)?;
                s.prepared_tip = Some(workspace);
            }
        }
        WorkflowCommand::Inspect { limit } => {
            let (view, token) = inspect(&authority, &engine, &s, limit)?;
            s.inspection = token;
            journal.save(&s)?;
            return Ok(view);
        }
        WorkflowCommand::Check { attempt_id } => {
            require_current(&authority, &s)?;
            let recipe = s
                .recipe
                .clone()
                .ok_or_else(|| Error::Invalid("no built-in source check declared".into()))?;
            let request = SourcePathsRequest {
                attempt_id: attempt_id.clone(),
                change: s.context.change.clone(),
                change_version: s.context.change_version,
                context_digest: s.context.digest()?,
                revision: s.context.revision.clone(),
                recipe,
            };
            let attempt = authority.create_source_paths_attempt(&request)?;
            invalidate(&mut s);
            s.attempt = Some(attempt_id.clone());
            journal.save(&s)?;
            if matches!(attempt.state, agentscm::checks::CheckState::Pending) {
                authority.run_source_paths_attempt(&engine, &attempt_id, attempt.run_generation)?;
            }
            return summary(&authority, &engine, &s);
        }
        WorkflowCommand::Note {
            command,
            environment,
            result,
        } => {
            if s.notes.len() >= 32
                || command.len() > 1024
                || environment.len() > 1024
                || result.len() > 4096
            {
                return Err(Error::Limit("operator note limit exceeded".into()));
            }
            let revision = clean_workspace(&engine, &s)?;
            s.notes.push(json!({"basis":"unverified_operator_claim", "revision":revision,"command":command,"environment":environment,"result":result}));
            invalidate(&mut s);
        }
        WorkflowCommand::Approve {
            approval_id,
            inspection,
        } => {
            require_inspection(&authority, &engine, &s, &inspection)?;
            let request = IntegrateRequest {
                operation_id: String::new(),
                target: s.context.target.clone(),
                expected_target: target_from_base(&s.context),
                revision: s.context.revision.clone(),
                context_digest: s.context.digest()?,
                approval_id: approval_id.clone(),
                change: Some(s.context.change.clone()),
                change_version: Some(s.context.change_version),
                policy_epoch: s.context.policy_epoch,
            };
            authority.approve(&Approval {
                approval_id,
                target: request.target.clone(),
                revision: request.revision.clone(),
                context_digest: request.context_digest.clone(),
                change: request.change.clone(),
                change_version: request.change_version,
                policy_epoch: request.policy_epoch,
            })?;
            // A lost reply before this save does not infer approval. Re-approve explicitly
            // with a new ID (and revoke the uncertain ID) after inspecting again.
            s.approved = Some(request);
        }
        WorkflowCommand::Integrate {
            operation_id,
            inspection,
        } => {
            require_inspection(&authority, &engine, &s, &inspection)?;
            let mut request = s
                .approved
                .clone()
                .ok_or_else(|| Error::Conflict("explicit workflow approval is required".into()))?;
            request.operation_id = operation_id;
            s.pending = Some(Pending::Integrate {
                request: Box::new(request),
            });
        }
        WorkflowCommand::Start(_)
        | WorkflowCommand::Resume { .. }
        | WorkflowCommand::Status
        | WorkflowCommand::CancelStart { .. } => unreachable!(),
    }
    journal.save(&s)?;
    finish(&data, &journal, &mut authority, &engine, &mut s)?;
    summary(&authority, &engine, &s)
}

fn start_session(data: &Path, authority: &Authority, a: StartArgs) -> Result<Session> {
    let parent = a
        .workspace
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let workspace = parent.canonicalize()?.join(
        a.workspace
            .file_name()
            .ok_or_else(|| Error::Invalid("workspace needs a directory name".into()))?,
    );
    if workspace.exists() || workspace.starts_with(data) || data.starts_with(&workspace) {
        return Err(Error::Invalid(
            "workspace must be a new separate non-nested directory".into(),
        ));
    }
    if a.issue.is_empty() || a.issue.len() > 2048 {
        return Err(Error::Invalid(
            "external issue reference must be 1..2048 bytes".into(),
        ));
    }
    let expectations = a
        .require_source_path
        .iter()
        .map(|s| {
            let (path, sha256) = s.rsplit_once('=').ok_or_else(|| {
                Error::Invalid("source check expectation requires PATH=SHA256".into())
            })?;
            Ok(SourcePathExpectation {
                path: path.into(),
                sha256: sha256.into(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let recipe = (!expectations.is_empty()).then_some(SourcePathsRecipe {
        version: 1,
        expectations,
        limits: CheckLimits {
            max_paths: 256,
            max_total_bytes: 64 * 1024 * 1024,
            max_output_bytes: 4096,
            timeout_ms: 30_000,
        },
    });
    let mut capabilities = vec!["agentscm.review-context/v1".into()];
    let mut evidence = vec![];
    let recipe_digest = if let Some(recipe) = &recipe {
        let sha256 = recipe.digest()?;
        capabilities.push("agentscm.checks.source-paths/v1".into());
        evidence.push(RequiredEvidence {
            identity: "source_paths/v1".into(),
            digest: InputIdentity::Digest {
                sha256: sha256.clone(),
            },
        });
        sha256
    } else {
        a.recipe
    };
    let context = ReviewContext {
        version: 1,
        change: ChangeId {
            repository: authority.repository_id()?,
            id: a.change,
        },
        change_version: 1,
        revision: a.revision,
        base: base(authority.target(&a.target)?),
        target: a.target,
        policy_epoch: authority.policy_epoch()?,
        intent: a.intent,
        acceptance_criteria: a.criterion,
        scope: a.scope,
        exclusions: a.exclusion,
        omissions: a.omission,
        dependencies: vec![],
        recipe: InputIdentity::Digest {
            sha256: recipe_digest,
        },
        toolchain: InputIdentity::Digest {
            sha256: a.toolchain,
        },
        environment: InputIdentity::Digest {
            sha256: a.environment,
        },
        required_evidence: evidence,
        required_capabilities: capabilities,
    };
    context.validate()?;
    match authority.change(&context.change) {
        Err(Error::NotFound(_)) => (),
        Ok(_) => {
            return Err(Error::Conflict(
                "Change ID already exists; choose a new ID".into(),
            ));
        }
        Err(error) => return Err(error),
    }
    Ok(Session {
        version: 1,
        issue: a.issue,
        workspace,
        workspace_binding: None,
        workspace_identity: None,
        restored_workspace_ack: None,
        restored_instance_ack: None,
        context,
        recipe,
        pending: Some(Pending::Start),
        prepared_tip: None,
        inspection: None,
        approved: None,
        attempt: None,
        notes: vec![],
        event: None,
        already_current: false,
        cancelled_start: None,
        empty_baseline: None,
    })
}

fn finish(
    data: &Path,
    journal: &Journal,
    a: &mut Authority,
    e: &Engine,
    s: &mut Session,
) -> Result<()> {
    let Some(pending) = s.pending.clone() else {
        return Ok(());
    };
    match pending {
        Pending::Start => {
            match a.change(&s.context.change) {
                Ok(state) if state.context_digest == s.context.digest()? => (),
                Ok(_) => {
                    return Err(Error::Conflict(
                        "Change already exists with another context".into(),
                    ));
                }
                Err(Error::NotFound(_)) => {
                    a.start_change(&s.context)?;
                }
                Err(error) => return Err(error),
            }
            if s.workspace.exists() {
                let w = Workspace::open(&s.workspace)?;
                let state = w.state()?;
                if state.change != s.context.change || state.base_revision != s.context.revision {
                    return Err(Error::Conflict(
                        "existing workspace differs from recorded start".into(),
                    ));
                }
                if !state.ready {
                    return Err(Error::Conflict("workspace initialization interrupted; use workspace recover --choice complete, then workflow resume".into()));
                }
            } else {
                super::workflow_cli::workspace(
                    data,
                    super::workflow_cli::WorkspaceCommand::Create {
                        root: s.workspace.clone(),
                        change: s.context.change.id.clone(),
                        revision: s.context.revision.clone(),
                    },
                )?;
            }
        }
        Pending::Checkpoint {
            operation_id,
            generation,
        } => {
            Workspace::open(&s.workspace)?.checkpoint(e, &operation_id, generation)?;
        }
        Pending::Update { expected, context } => {
            let current = a.change(&context.change)?;
            if current.context_digest != context.digest()? {
                a.update_change_context(expected, &context)?;
            }
            s.context = *context;
        }
        Pending::Prepare { request } => match a.prepare_landing(e, &request)? {
            LandingPreparation::Prepared { change, .. } => {
                s.context = a.review_context(&change.context_digest)?;
                s.prepared_tip = Some(request.source_tip);
            }
            LandingPreparation::AlreadyCurrent { .. } => {
                s.already_current = true;
            }
        },
        Pending::Integrate { request } => {
            // Exact durable request retry is essential after a lost reply: admission's
            // journal lookup precedes freshness checks for a completed operation.
            if !find_event(a, &request.operation_id)?
                .as_ref()
                .is_some_and(|event| event_matches(event, &request))
            {
                let token = s.inspection.as_deref().ok_or_else(|| {
                    Error::Conflict("pending integration has no inspection binding".into())
                })?;
                require_inspection(a, e, s, token)?;
            }
            let event = if request.expected_target.is_none() {
                a.initialize_target(e, &request)?
            } else {
                a.integrate(e, &request)?
            };
            s.event = Some(event);
        }
    }
    s.pending = None;
    journal.save(s)
}
fn restored_root(data: &Path) -> Result<Option<PathBuf>> {
    let Some(root) = data.parent() else {
        return Ok(None);
    };
    match std::fs::symlink_metadata(root.join(".agentscm-recovery/restore-complete.json")) {
        Ok(metadata) if metadata.is_file() => Ok(Some(root.to_path_buf())),
        Ok(_) => Err(Error::Invalid(
            "restore receipt must be a regular file".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn require_workspace_identity(s: &Session) -> Result<()> {
    // A newly journaled Start has no workspace yet. All other active Unix
    // sessions need an explicitly retained directory identity.
    if matches!(s.pending, Some(Pending::Start)) && s.workspace_identity.is_none() {
        match std::fs::symlink_metadata(&s.workspace) {
            Err(error) if error.kind()==std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(Error::Conflict("interrupted start found an unacknowledged workspace; explicitly workflow resume --workspace PATH".into())),
        }
    }
    if cfg!(unix)
        && (s.workspace_identity.is_none()
            || s.workspace_identity != workspace_identity(&s.workspace).ok().flatten())
    {
        return Err(Error::Conflict("workspace directory is missing or replaced; explicitly workflow resume --workspace PATH".into()));
    }
    Ok(())
}
fn event_matches(event: &IntegrationEvent, request: &IntegrateRequest) -> bool {
    event.kind == expected_kind(request)
        && event.target == request.target
        && event.after.revision == request.revision
        && event.before == request.expected_target
        && event.approval_id == request.approval_id
        && event.context_digest == request.context_digest
        && event.change == request.change
        && event.change_version == request.change_version
        && event.policy_epoch == request.policy_epoch
}
fn validate_reattachment(e: &Engine, s: &Session, root: &Path) -> Result<()> {
    let mut w = Workspace::open(root)?;
    let state = w.state()?;
    if state.change != s.context.change {
        return Err(Error::Conflict(
            "reattached workspace belongs to another Change".into(),
        ));
    }
    let binding = serde_json::to_value(&state)?;
    if s.workspace_binding.as_ref() == Some(&binding) {
        return Ok(());
    }
    match &s.pending {
        Some(Pending::Start) if state.generation==0 && state.base_revision==s.context.revision && state.pending.is_none() => Ok(()),
        Some(Pending::Checkpoint{operation_id,generation}) => {
            let old=s.workspace_binding.as_ref().ok_or_else(||Error::Conflict("checkpoint has no retained workspace binding".into()))?;
            let old_base=old["base_revision"].as_str().ok_or_else(||Error::Invalid("invalid retained checkpoint base".into()))?;
            if old["generation"].as_u64()!=Some(*generation) {return Err(Error::Conflict("checkpoint generation differs from retained workspace".into()));}
            if state.ready && state.generation==*generation && state.base_revision==old_base && state.pending.as_ref().is_some_and(|pending|pending.operation_id==*operation_id && pending.kind==agentscm::workspace::OperationKind::Checkpoint) {return Ok(());}
            if state.ready && state.pending.is_none() && state.generation==next(*generation)? {
                // At the advanced generation this call can only retrieve the exact
                // retained checkpoint; a missing/different request fails before writing.
                if let agentscm::workspace::OperationOutcome::Checkpointed{revision,generation:done}=w.checkpoint(e,operation_id,*generation)?
                    && done==state.generation && revision==state.base_revision && e.load_revision(&revision)?.parents.first().map(String::as_str)==Some(old_base) {return Ok(());}
            }
            Err(Error::Conflict("restored checkpoint does not match the retained request and base".into()))
        }
        _ => Err(Error::Conflict("reattached workspace identity/base/generation/pending state differs from retained binding".into())),
    }
}
fn retained_status(
    a: &Authority,
    s: &Session,
    restored_root: &Option<PathBuf>,
    restored_instance: &Option<String>,
) -> Result<Value> {
    let current = match a.change(&s.context.change) {
        Ok(change) => Some(change),
        Err(Error::NotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let context = current
        .as_ref()
        .map(|change| a.review_context(&change.context_digest))
        .transpose()?;
    Ok(
        json!({"ok":true,"issue":s.issue,"pending":s.pending,"event":s.event,"already_current":s.already_current,"cancelled_start":s.cancelled_start,"change":current,"context":context,"recorded_context":s.context,"retained_workspace_binding":s.workspace_binding,"workspace_path":s.workspace,"workspace_status":{"state":"not_read_by_status"},"restored_reattachment_required":restored_root.is_some() && (s.restored_workspace_ack!=*restored_root || s.restored_instance_ack!=*restored_instance)}),
    )
}
fn displayed_evidence_ready(
    a: &Authority,
    s: &Session,
    displayed: Option<&agentscm::checks::CheckAttempt>,
) -> Result<bool> {
    if !s
        .context
        .required_capabilities
        .iter()
        .any(|capability| capability == agentscm::checks::SOURCE_PATHS_CAPABILITY)
    {
        return Ok(true);
    }
    let Some(attempt) = displayed else {
        return Ok(false);
    };
    if s.attempt.as_deref() != Some(attempt.attempt_id.as_str()) {
        return Ok(false);
    }
    if attempt.state != agentscm::checks::CheckState::Success
        || attempt.subject.change != s.context.change
        || attempt.subject.change_version != s.context.change_version
        || attempt.subject.context_digest != s.context.digest()?
        || attempt.subject.revision != s.context.revision
    {
        return Ok(false);
    }
    let Some(digest) = &attempt.evidence_digest else {
        return Ok(false);
    };
    let evidence = a.check_evidence(digest)?;
    Ok(evidence.state == agentscm::checks::CheckState::Success
        && evidence.subject == attempt.subject
        && evidence.attempt_id == attempt.attempt_id
        && evidence.request_digest == attempt.request_digest
        && evidence.run_generation == attempt.run_generation)
}
#[cfg(unix)]
fn workspace_identity(path: &Path) -> Result<Option<Value>> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok(Some(json!([metadata.dev(), metadata.ino()])))
}
#[cfg(not(unix))]
fn workspace_identity(_path: &Path) -> Result<Option<Value>> {
    Ok(None)
}
fn expected_kind(request: &IntegrateRequest) -> EventKind {
    if request.expected_target.is_none() {
        EventKind::Initialize
    } else {
        EventKind::Integrate
    }
}
fn find_event(a: &Authority, operation: &str) -> Result<Option<IntegrationEvent>> {
    let mut after = 0;
    for _ in 0..100 {
        let events = a.events(after, 1000)?;
        if let Some(event) = events.iter().find(|event| event.operation_id == operation) {
            return Ok(Some(event.clone()));
        }
        if events.len() < 1000 {
            return Ok(None);
        }
        after = events.last().expect("nonempty event page").sequence;
    }
    Err(Error::Limit(
        "integration retry lookup exceeds 100000 authority events".into(),
    ))
}
fn next(v: u64) -> Result<u64> {
    v.checked_add(1)
        .ok_or_else(|| Error::Limit("Change generation exhausted".into()))
}
fn base(t: Option<TargetState>) -> Option<BaseState> {
    t.map(|t| BaseState {
        revision: t.revision,
        target_version: t.version,
    })
}
fn target_from_base(c: &ReviewContext) -> Option<TargetState> {
    c.base.as_ref().map(|b| TargetState {
        revision: b.revision.clone(),
        version: b.target_version,
    })
}
fn invalidate(s: &mut Session) {
    s.inspection = None;
    s.approved = None;
}
fn queue_update(s: &mut Session, mut context: ReviewContext) -> Result<()> {
    context.change_version = next(s.context.change_version)?;
    context.validate()?;
    s.pending = Some(Pending::Update {
        expected: s.context.change_version,
        context: Box::new(context),
    });
    invalidate(s);
    Ok(())
}
fn require_current(a: &Authority, s: &Session) -> Result<()> {
    let current = a.change(&s.context.change)?;
    if current.context_digest != s.context.digest()?
        || a.policy_epoch()? != s.context.policy_epoch
        || a.target(&s.context.target)? != target_from_base(&s.context)
    {
        return Err(Error::Conflict("Change, target or policy drifted; inspect and explicitly workflow refresh; source is preserved".into()));
    }
    Ok(())
}
fn clean_workspace(e: &Engine, s: &Session) -> Result<String> {
    let w = Workspace::open(&s.workspace)?;
    let state = w.state()?;
    if state.change != s.context.change {
        return Err(Error::Conflict(
            "workspace Change differs from workflow".into(),
        ));
    }
    let status = w.status(e, MAX_DIFF)?;
    if !state.ready || status.pending.is_some() || !status.complete || !status.changes.is_empty() {
        return Err(Error::Conflict(
            "workspace is dirty, incomplete or interrupted; checkpoint/recover before review"
                .into(),
        ));
    }
    Ok(state.base_revision)
}
fn summary(a: &Authority, e: &Engine, s: &Session) -> Result<Value> {
    if s.event.is_some() || s.already_current {
        return Ok(
            json!({"ok":true,"issue":s.issue,"event":s.event,"already_current":s.already_current,"context":s.context,"retained_workspace_binding":s.workspace_binding,"workspace_status":{"state":"not_read_for_completed_operation"},"evidence":{"operator_notes":s.notes,"operator_notes_authenticated":false}}),
        );
    }

    let current = a.change(&s.context.change)?;
    let context = a.review_context(&current.context_digest)?;
    let attempt = s
        .attempt
        .as_ref()
        .map(|id| a.check_attempt(id))
        .transpose()?;
    let controller_status = match &attempt {
        Some(attempt)
            if attempt.subject.context_digest == current.context_digest
                && attempt.subject.change_version == current.version =>
        {
            format!("{:?}", attempt.state).to_lowercase()
        }
        Some(_) => "stale".into(),
        None if s.recipe.is_some() => "not_recorded_in_workflow".into(),
        None => "not_requested".into(),
    };
    let view = json!({"ok":true,"issue":s.issue,"workspace":Workspace::open(&s.workspace)?.state()?,"workspace_status":Workspace::open(&s.workspace)?.status(e,1000)?,"context":context,"recorded_context":s.context,"change":current,"expected_target":target_from_base(&s.context),"current_target":a.target(&s.context.target)?,"current_policy_epoch":a.policy_epoch()?,"pending":s.pending,"inspection":s.inspection,"approval_recorded":s.approved.is_some(),"event":s.event,"already_current":s.already_current,"evidence":{"required":s.context.required_evidence,"controller_status":controller_status,"controller_attempt":attempt,"operator_notes":s.notes,"operator_notes_authenticated":false},"export":{"status":"query_git_export_separately"}});
    if serde_json::to_vec(&view)?.len() > MAX_VIEW {
        return Err(Error::Limit("workflow summary exceeds output bound".into()));
    }
    Ok(view)
}
fn inspect(
    a: &Authority,
    e: &Engine,
    s: &Session,
    limit: usize,
) -> Result<(Value, Option<String>)> {
    if limit == 0 || limit > MAX_DIFF {
        return Err(Error::Limit(format!(
            "inspection diff limit must be 1..{MAX_DIFF}"
        )));
    }
    let mut view = summary(a, e, s)?;
    let left = match &s.context.base {
        Some(b) => b.revision.clone(),
        None => s.empty_baseline.clone().ok_or_else(|| {
            Error::Conflict("workflow has no prepared empty inspection baseline".into())
        })?,
    };
    let diff = agentscm::guided_service::source_diff(e, &left, &s.context.revision, limit)?;
    let complete = diff["complete"] == true;
    let current = require_current(a, s).is_ok();
    let workspace = clean_workspace(e, s).ok();
    let prepared = workspace
        .as_ref()
        .is_some_and(|w| s.prepared_tip.as_ref() == Some(w));
    view["diff"] = diff;
    view["context_readiness"] = json!(s.context.readiness(SUPPORTED_CAPABILITIES));
    let displayed: Option<agentscm::checks::CheckAttempt> =
        serde_json::from_value(view["evidence"]["controller_attempt"].clone())?;
    let evidence_ready = displayed_evidence_ready(a, s, displayed.as_ref())?;
    let context_ready = matches!(
        s.context.readiness(SUPPORTED_CAPABILITIES),
        agentscm::context::ContextReadiness::Complete
    );
    view["evidence"]["displayed_required_check_ready"] = json!(evidence_ready);
    view["reviewable"] = json!(complete && current && prepared && evidence_ready && context_ready);
    let token = if complete && current && prepared && evidence_ready && context_ready {
        Some(digest(&serde_json::to_vec(
            &json!({"domain":"agentscm.guided-inspection/v1","context":s.context,"workspace_tip":workspace,"notes":s.notes,"issue":s.issue,"controller_attempt":view["evidence"]["controller_attempt"]}),
        )?))
    } else {
        None
    };
    view["inspection"] = json!(token);
    if serde_json::to_vec(&view)?.len() > MAX_VIEW {
        return Err(Error::Limit("inspection exceeds output bound; use immutable read/diff pages; no approval token issued".into()));
    }
    Ok((view, token))
}
fn require_inspection(a: &Authority, e: &Engine, s: &Session, token: &str) -> Result<()> {
    if s.inspection.as_deref() != Some(token) {
        return Err(Error::Conflict(
            "explicit token from a complete workflow inspection is required".into(),
        ));
    }
    let (_, current) = inspect(a, e, s, MAX_DIFF)?;
    if current.as_deref() != Some(token) {
        return Err(Error::Conflict(
            "inspection is stale or incomplete; prepare and inspect again".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscm::checks::CheckState;
    use agentscm::model::FileChange;

    #[test]
    fn evidence_readiness_uses_the_displayed_attempt_not_a_newer_authority_read() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        let engine = Engine::open(&data).unwrap();
        let mut authority = Authority::open(&data).unwrap();
        let revision = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "a.txt".into(),
                    content: Some(b"source".to_vec()),
                }],
            )
            .unwrap();
        let mut session = start_session(
            &data,
            &authority,
            StartArgs {
                change: "snapshot".into(),
                workspace: temp.path().join("work"),
                revision,
                issue: "https://example.invalid/issues/23".into(),
                intent: "Check exact displayed evidence".into(),
                criterion: vec!["Only displayed current success is ready".into()],
                scope: vec!["a.txt".into()],
                exclusion: vec![],
                omission: vec![],
                recipe: "a".repeat(64),
                toolchain: "b".repeat(64),
                environment: "c".repeat(64),
                target: "main".into(),
                require_source_path: vec![format!("a.txt={}", digest(b"source"))],
            },
        )
        .unwrap();
        authority.start_change(&session.context).unwrap();
        let request = SourcePathsRequest {
            attempt_id: "selected".into(),
            change: session.context.change.clone(),
            change_version: 1,
            context_digest: session.context.digest().unwrap(),
            revision: session.context.revision.clone(),
            recipe: session.recipe.clone().unwrap(),
        };
        let displayed = authority.create_source_paths_attempt(&request).unwrap();
        session.attempt = Some("selected".into());
        let newer = authority
            .run_source_paths_attempt(&engine, "selected", 0)
            .unwrap();
        assert_eq!(newer.state, CheckState::Success);
        assert_eq!(
            authority.check_attempt("selected").unwrap().state,
            CheckState::Success
        );
        assert_eq!(displayed.state, CheckState::Pending);
        // The old implementation reread Success here, contradicting the displayed Pending.
        assert!(!displayed_evidence_ready(&authority, &session, Some(&displayed)).unwrap());
        assert!(displayed_evidence_ready(&authority, &session, Some(&newer)).unwrap());
    }

    #[test]
    fn restore_receipt_errors_and_wrong_types_never_disable_reattachment() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir(&data).unwrap();
        assert!(restored_root(&data).unwrap().is_none());
        let control = temp.path().join(".agentscm-recovery");
        // A non-directory control component gives a metadata error on supported hosts.
        std::fs::write(&control, b"invalid control component").unwrap();
        assert!(restored_root(&data).is_err());
        std::fs::remove_file(&control).unwrap();
        std::fs::create_dir(&control).unwrap();
        let receipt = control.join("restore-complete.json");
        std::fs::create_dir(&receipt).unwrap();
        assert!(restored_root(&data).is_err());
        std::fs::remove_dir(&receipt).unwrap();
        std::fs::write(&receipt, b"{}").unwrap();
        assert_eq!(
            restored_root(&data).unwrap(),
            Some(temp.path().to_path_buf())
        );
    }
}
