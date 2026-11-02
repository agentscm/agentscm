//! Explicit owner-operated continuation through the existing Work API.
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use agentscm::change_capture::{
    ConfiguredRecipeRunRequest, ConfiguredRecipeV1, GitSourceRequest, operator_recipe_identity,
    run_operator_recipe,
};
use agentscm::change_record::{ActionOutcomeV1, CheckResultV1, DriftV1};
use agentscm::continuation::{
    self, Acceptance, Journal, NativeAttempt, NativeClient, TransferPhase, TransferPlan,
    TransferRecord, capture_error, capture_source, selected_file,
};
use agentscm::execution::executable;
use agentscm::hub::work::{
    CheckpointKind, WorkCheckpoint, WorkItem, WorkRepositoryScope, WorkRole, WorkStart, WorkState,
};
use agentscm::skill_workflow::intent::IntentService;
use agentscm::{Error, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::managed_work::ManagedWork;

#[derive(Debug, Subcommand)]
pub enum ContinuationCommand {
    /// Create a task under this adapter before any native execution, establishing its safe boundary.
    Enroll {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        request_key: String,
        #[arg(long)]
        title: String,
        #[arg(long = "path", required = true)]
        paths: Vec<String>,
        #[arg(long)]
        next_action: String,
    },
    /// Observe a stopped/cancelled transfer and preserve its known effects without relaunching.
    Resolve { id: String },
    /// Page retained IDs/outcomes, including earlier artifacts omitted from bounded previews.
    History {
        work: String,
        #[arg(long)]
        before: Option<i64>,
    },
    /// Freeze a bounded source/context disclosure for an existing Work; does not launch.
    Preview {
        work: String,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, value_enum)]
        to: NativeClient,
        #[arg(long)]
        executable: PathBuf,
        /// Approved existing UTF-8 source files. Later transfers inherit these paths.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Earlier same-Work artifact transfer IDs to disclose instead of the latest artifact.
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        /// Optional strict context JSON: criteria, constraints, policy and open_decisions.
        #[arg(long)]
        context: Option<PathBuf>,
        /// Optional operator-owned ConfiguredRecipeV1 JSON, never model-generated argv.
        #[arg(long)]
        check: Option<PathBuf>,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Explicitly authorize the previewed disclosure and managed effect scope.
    Run {
        id: String,
        #[arg(long)]
        confirm: String,
    },
    /// Observe and continue a retained safe phase; never blindly relaunch an uncertain client.
    Recover { id: String },
    /// Inspect retained identities, progress and uncertainty without running anything.
    Show { id: String },
    /// Prevent subsequent managed effects; preserve source, artifacts and history.
    Revoke { id: String },
    /// Read a retained artifact, verifying its exact content identity.
    Artifact { id: String },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Context {
    #[serde(default)]
    criteria: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    policy: String,
    #[serde(default)]
    open_decisions: Vec<String>,
}

pub async fn run(data_dir: &Path, command: ContinuationCommand) -> Result<Value> {
    let journal = Journal::open(data_dir)?;
    match command {
        ContinuationCommand::Enroll {
            root,
            request_key,
            title,
            paths,
            next_action,
        } => {
            continuation::identifier(&request_key)?;
            let root = root.canonicalize()?;
            if data_dir.canonicalize()?.starts_with(&root) {
                return Err(invalid(
                    "continuation data must be outside the source checkout",
                ));
            }
            let (managed, _) = ManagedWork::discover(&root).await?;
            for path in &paths {
                selected_file(&root, path)?;
            }
            let binding = agentscm::store::digest(&serde_json::to_vec(
                &json!({"root":root,"authority":managed.identity_commitment()?,"request_key":request_key,"title":title,"paths":paths,"next_action":next_action}),
            )?);
            let key = format!("continuation-{binding}");
            let _lock = journal.lock_work(&key)?;
            let session = format!("enrolled-{binding}");
            let item = managed
                .start(&WorkStart {
                    work_id: None,
                    source: None,
                    request_key: Some(key),
                    title,
                    session: session.clone(),
                    role: WorkRole::Lead,
                    scope: paths,
                    branch: None,
                    worktree: Some(root.to_string_lossy().into_owned()),
                    next_action: Some(next_action),
                    depends_on: vec![],
                    related_work: vec![],
                    takeover_generation: None,
                    preflight: None,
                })
                .await?;
            if item.generation != 1 || item.lead_session != session {
                return Err(invalid(
                    "enrollment has already executed or changed; inspect the existing Work",
                ));
            }
            journal.enroll(&item, &root, &managed.identity_commitment()?)?;
            Ok(
                json!({"work":item,"boundary":"Created by this adapter before native execution; no outside executor is certified quiescent."}),
            )
        }
        ContinuationCommand::Resolve { id } => resolve(&journal, &id).await,
        ContinuationCommand::History { work, before } => {
            let mut ids = journal.history_ids(&work, before)?;
            let more = ids.len() > 16;
            ids.truncate(16);
            let next = if more {
                ids.last().map(|(cursor, _)| *cursor)
            } else {
                None
            };
            let records = ids
                .iter()
                .map(|(_, id)| journal.get(id).map(|record| effect_summary(&record)))
                .collect::<Result<Vec<_>>>()?;
            Ok(
                json!({"work_id":work,"records":records,"next_cursor":next,"total":journal.history_count(&work)?}),
            )
        }
        ContinuationCommand::Preview {
            work,
            root,
            to,
            executable,
            paths,
            artifacts,
            context,
            check,
            timeout_seconds,
        } => {
            continuation::identifier(&work)?;
            let root = root.canonicalize()?;
            let data = data_dir.canonicalize()?;
            if data.starts_with(&root) {
                return Err(invalid(
                    "continuation data must be outside the source checkout",
                ));
            }
            let (managed, scope) = ManagedWork::discover(&root).await?;
            let item = managed.read(&work).await?;
            if item.state == WorkState::Finished {
                return Err(invalid(
                    "finished Work needs a new explicitly authorized task",
                ));
            }
            let _lock = journal.lock_work(&work)?;
            journal.assert_quiescent(&work, "preview")?;
            journal.check_enrolled(&item, &root, &managed.identity_commitment()?)?;
            let history = journal.work_history(&work)?;
            let latest_completed = journal.latest_record(&work, true)?;
            let previous = latest_completed.as_ref();
            let paths = if paths.is_empty() {
                previous
                    .map(|r| r.plan.files.iter().map(|f| f.path.clone()).collect())
                    .unwrap_or_default()
            } else {
                paths
            };
            if paths.is_empty() || paths.len() > 8 {
                return Err(invalid("select 1..8 source files with --path"));
            }
            let selected = paths
                .iter()
                .map(|path| selected_file(&root, path))
                .collect::<Result<Vec<_>>>()?;
            let supplement = if let Some(path) = context {
                serde_json::to_value(read_json::<Context>(&path)?)?
            } else {
                previous
                    .and_then(|r| r.plan.context.get("owner_context").cloned())
                    .unwrap_or(serde_json::to_value(Context::default())?)
            };
            let mut check = match check {
                Some(path) => Some(read_json::<ConfiguredRecipeV1>(&path)?),
                None => previous.and_then(|r| r.plan.check.clone()),
            };
            let check_executable = if let Some(check) = &mut check {
                check.validate().map_err(capture_error)?;
                let identity = executable::capture(Path::new(&check.argv[0]), &[&root, &data])?;
                check.argv[0] = identity
                    .path
                    .to_str()
                    .ok_or_else(|| invalid("check executable path must be UTF-8"))?
                    .into();
                Some(identity)
            } else {
                None
            };
            let source = capture_source(&root, &scope.repository, &journal.spool()?)?;
            let intent = if data.join("skill-workflows/state.sqlite").exists() {
                match IntentService::new(&data).inspect(&scope.repository, &work) {
                    Ok(record) => Some(serde_json::to_value(record)?),
                    Err(Error::NotFound(_)) => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let record = managed.read_change_record(&item).await?;
            let spool = journal.spool()?;

            let artifact_records = if artifacts.is_empty() {
                journal
                    .latest_artifact(&work)?
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                if artifacts.len() > 4 {
                    return Err(invalid("select at most four earlier artifact transfer IDs"));
                }
                artifacts
                    .iter()
                    .map(|id| journal.get(id))
                    .collect::<Result<Vec<_>>>()?
            };
            let mut artifact_bytes = 0usize;
            let prior_artifacts=artifact_records.iter().map(|record| {
                if record.plan.work.id!=work {return Err(invalid("selected artifact belongs to another Work"));}
                let artifact=record.artifact.as_ref().ok_or_else(||invalid("selected transfer has no retained artifact"))?;
                let bytes=spool.read_artifact(artifact).map_err(capture_error)?;
                artifact_bytes=artifact_bytes.saturating_add(bytes.len());
                if artifact_bytes>continuation::MAX_FILE_BYTES {return Err(invalid("selected artifact bodies exceed 64 KiB; select fewer artifacts"));}
                Ok(json!({"transfer_id":record.plan.id,"artifact":artifact,"text":String::from_utf8(bytes).map_err(|_|invalid("prior artifact is not UTF-8"))?}))
            }).collect::<Result<Vec<_>>>()?;
            let mut prior_effects = Vec::new();
            let mut summary_bytes = 2usize;
            let mut compacted = false;
            for item in &history {
                let mut summary = effect_summary(item);
                if serde_json::to_vec(&summary)?.len() > 32 * 1024 {
                    summary = json!({"transfer_id":item.plan.id,"phase":item.phase,"artifact":item.artifact,"check":item.check.as_ref().map(|check|json!({"outcome":check.outcome,"result":check.result,"assurance":check.assurance,"coverage":check.coverage,"drift":check.drift})),"details_omitted":true,"inspection":"Use show with this exact transfer ID for its complete retained evidence."});
                    compacted = true;
                }
                let bytes = serde_json::to_vec(&summary)?.len();
                if summary_bytes + bytes + 1 > 32 * 1024 {
                    break;
                }
                summary_bytes += bytes + 1;
                prior_effects.push(summary);
            }
            let total = journal.history_count(&work)?;
            let history_disclosure = json!({"total_transfers":total,"included_effect_summaries":prior_effects.len(),"omitted_transfers":total.saturating_sub(prior_effects.len() as u64),"artifact_body_limit_bytes":continuation::MAX_FILE_BYTES,"effect_summary_limit_bytes":32*1024,"effect_summary_bytes":summary_bytes,"compacted_effect_details":compacted,"selection":"Latest artifact body by default; --artifact selects exact earlier transfers. history pages retained IDs; show and artifact inspect exact records. Omitted outcomes are not evidence of absence."});
            let mut plan = TransferPlan {
                id: String::new(),
                work: item,
                root: root.clone(),
                authority: json!({"scope":scope,"identity":managed.identity_commitment()?}),
                source,
                index_digest: continuation::index_digest(&root)?,
                destination: to,
                executable: executable::capture(&executable, &[&root, &data])?,
                files: selected,
                context: json!({"owner_context":supplement,"intent_record":intent,"change_record":record,"prior_effects":prior_effects,"prior_artifacts":prior_artifacts,"history_disclosure":history_disclosure,"limits":"Only selected source bodies and these bounded records are disclosed. Missing criteria or evidence remain unknown. Captured checks do not grant approval."}),
                check,
                check_executable,
                timeout_seconds,
            };
            for file in &plan.files {
                if selected_file(&root, &file.path)? != *file {
                    return Err(invalid("selected source changed while capturing preview"));
                }
            }
            plan.seal()?;
            plan.prompt()?;
            let record = journal.insert(plan)?;
            Ok(
                json!({"transfer":record,"confirmation":record.plan.id,"next":"Inspect the full source/context disclosure, then run continuation run ID --confirm ID. This authorizes only the displayed bounded source and managed operations.","boundary":"Trusted local operator; managed edits/checks only. No unrestricted native-tool, merge or deployment authority."}),
            )
        }
        ContinuationCommand::Show { id } => Ok(serde_json::to_value(journal.get(&id)?)?),
        ContinuationCommand::Revoke { id } => {
            journal.revoke(&id)?;
            Ok(serde_json::to_value(journal.get(&id)?)?)
        }
        ContinuationCommand::Artifact { id } => {
            let record = journal.get(&id)?;
            let artifact = record
                .artifact
                .ok_or_else(|| invalid("this transfer has no retained artifact"))?;
            let bytes = journal
                .spool()?
                .read_artifact(&artifact)
                .map_err(capture_error)?;
            Ok(
                json!({"artifact":artifact,"text":String::from_utf8(bytes).map_err(|_|invalid("artifact is not UTF-8"))?}),
            )
        }
        ContinuationCommand::Run { id, confirm } => {
            if confirm != id {
                return Err(invalid(
                    "--confirm must equal the exact reviewed transfer ID",
                ));
            }
            execute(&journal, &id, false).await
        }
        ContinuationCommand::Recover { id } => execute(&journal, &id, true).await,
    }
}

async fn execute(journal: &Journal, id: &str, recover: bool) -> Result<Value> {
    let mut record = journal.get(id)?;
    let _lock = journal.lock_work(&record.plan.work.id)?;
    record = journal.get(id)?;
    if matches!(
        record.phase,
        TransferPhase::Complete | TransferPhase::Resolved
    ) {
        return Ok(serde_json::to_value(record)?);
    }
    journal.ensure_current(id)?;
    journal.assert_quiescent(&record.plan.work.id, id)?;
    if recover && record.phase == TransferPhase::Preview {
        return Err(invalid(
            "preview has no authorized execution; use run with its exact confirmation",
        ));
    }
    if !recover && record.phase != TransferPhase::Preview {
        return Err(invalid(
            "transfer already reserved; use recover to inspect its retained phase",
        ));
    }
    let scope: WorkRepositoryScope =
        serde_json::from_value(record.plan.authority["scope"].clone())?;
    let managed = ManagedWork::connect(&record.plan.root, scope).await?;
    if record.plan.authority["identity"] != managed.identity_commitment()? {
        return Err(invalid(
            "transfer authority differs from the currently selected connection",
        ));
    }
    let result = advance(journal, &managed, &mut record, recover).await;
    match result {
        Ok(()) => Ok(serde_json::to_value(record)?),
        Err(error) => {
            // Keep the exact interrupted phase: reconciliation must know which effect might exist.
            record.error = Some(error.to_string());
            journal.save(&record)?;
            Err(error)
        }
    }
}

async fn advance(
    journal: &Journal,
    managed: &ManagedWork,
    record: &mut TransferRecord,
    recover: bool,
) -> Result<()> {
    if matches!(
        record.phase,
        TransferPhase::Reserved | TransferPhase::Uncertain
    ) {
        return Err(invalid(
            "native launch outcome is uncertain; no automatic relaunch is safe. Use resolve to observe the retained process/group stopping (or a same-host reboot if launch lost its PID marker); no new Work generation was granted",
        ));
    }
    if matches!(
        record.phase,
        TransferPhase::Preview | TransferPhase::HandingOver
    ) {
        managed.authenticate().await?;
        source_current(journal, record)?;
        let current = managed.read(&record.plan.work.id).await?;
        if record.phase == TransferPhase::Preview
            && (current.generation != record.plan.work.generation
                || current.updated_seq != record.plan.work.updated_seq
                || current.lead_session != record.plan.work.lead_session
                || current.lead_token != managed.identity.token_id())
        {
            return Err(invalid(
                "Work changed after preview or belongs to another current owner",
            ));
        }
        record.phase = TransferPhase::HandingOver;
        journal.save(record)?;
        let request = checkpoint(
            &record.plan.work,
            &format!("{}-handover", record.plan.id),
            CheckpointKind::Handover,
            "Owner confirmed a bounded managed continuation at a local safe boundary.",
            record.plan.work.next_action.clone(),
        );
        let handed = managed.checkpoint(&request).await?;
        if handed.item.generation != record.plan.work.generation
            || handed.item.state != WorkState::Handover
            || handed.item.lead_session != record.plan.work.lead_session
            || handed.item.lead_token != managed.identity.token_id()
            || handed.item.scope != record.plan.work.scope
            || handed.item.branch != record.plan.work.branch
            || handed.item.worktree != record.plan.work.worktree
            || handed.item.updated_seq != handed.receipt.event_seq
        {
            return Err(invalid(
                "handover changed while its checkpoint was recorded",
            ));
        }
        record.handover = Some(handed.item);
        journal.ensure_current(&record.plan.id)?;
        source_current(journal, record)?;
        managed.authenticate().await?;
        executable::verify(&record.plan.executable, &[&record.plan.root])?;
        record.plan.prompt()?;
        record.native = Some(NativeAttempt::reserve());
        record.phase = TransferPhase::Reserved;
        journal.save(record)?;
        journal.ensure_current(&record.plan.id)?;
        let response = continuation::invoke_reserved(journal, record)?;
        record.response = Some(response);
        record.phase = TransferPhase::Received;
        journal.save(record)?;
    }
    if record.phase == TransferPhase::Received {
        journal.ensure_current(&record.plan.id)?;
        managed.authenticate().await?;
        source_current(journal, record)?;
        let response = record
            .response
            .as_ref()
            .ok_or_else(|| invalid("received transfer lost native session evidence"))?;
        let acceptance: Acceptance = serde_json::from_str(&response.text)?;
        acceptance.validate(&record.plan)?;
        record.acceptance = Some(acceptance);
        journal.save(record)?;
        let before = record
            .handover
            .as_ref()
            .ok_or_else(|| invalid("missing handover checkpoint"))?;
        let current = managed.read(&before.id).await?;
        let incoming = if current.generation == before.generation + 1
            && current.lead_token == managed.identity.token_id()
            && current.lead_session == response.session_id
        {
            // The Hub accepted the exact session before the local process lost its response.
            if current.scope != before.scope
                || current.state != WorkState::Active
                || current.next_action != before.next_action
                || current.branch != before.branch
                || current.worktree != before.worktree
                || current.title != before.title
            {
                return Err(invalid("retained takeover changed before recovery"));
            }
            current
        } else {
            if current.generation != before.generation
                || current.updated_seq != before.updated_seq
                || current.state != WorkState::Handover
            {
                return Err(invalid(
                    "handover generation or checkpoint changed before acceptance",
                ));
            }
            managed
                .start(&WorkStart {
                    work_id: Some(before.id.clone()),
                    source: None,
                    request_key: None,
                    title: before.title.clone(),
                    session: response.session_id.clone(),
                    role: WorkRole::Lead,
                    scope: before.scope.clone(),
                    branch: before.branch.clone(),
                    worktree: before.worktree.clone(),
                    next_action: before.next_action.clone(),
                    depends_on: before.depends_on.clone(),
                    related_work: before.related_work.clone(),
                    takeover_generation: Some(before.generation),
                    preflight: None,
                })
                .await?
        };
        record.incoming = Some(incoming);
        record.phase = TransferPhase::Running;
        journal.save(record)?;
    }
    if matches!(
        record.phase,
        TransferPhase::Running | TransferPhase::Applying
    ) {
        managed
            .check_current(
                record
                    .incoming
                    .as_ref()
                    .ok_or_else(|| invalid("missing accepted Work"))?,
            )
            .await?;
        journal.ensure_current(&record.plan.id)?;
        let already_applying = record.phase == TransferPhase::Applying;
        if !already_applying {
            source_current(journal, record)?;
        }
        let accepted = record
            .acceptance
            .as_ref()
            .ok_or_else(|| invalid("missing native acceptance"))?;
        record.phase = TransferPhase::Applying;
        journal.save(record)?;
        if let Some(edit) = &accepted.edit {
            let selected = record
                .plan
                .files
                .iter()
                .find(|file| file.path == edit.path)
                .ok_or_else(|| invalid("missing approved path"))?;
            if selected_file(&record.plan.root, &edit.path)?.mode != selected.mode
                || continuation::index_digest(&record.plan.root)? != record.plan.index_digest
            {
                return Err(invalid(
                    "source mode or index changed before managed replacement",
                ));
            }
            managed
                .check_current(record.incoming.as_ref().unwrap())
                .await?;
            journal.effect(&record.plan.id, || {
                continuation::apply_edit(&record.plan.root, edit, already_applying && recover)
            })?;
        }
        if let Some(text) = &accepted.artifact {
            managed
                .check_current(record.incoming.as_ref().unwrap())
                .await?;
            record.artifact = Some(journal.effect(&record.plan.id, || {
                continuation::retain_result(journal, text)
            })?);
        }
        let after = capture_source(
            &record.plan.root,
            &record.plan.work.repository,
            &journal.spool()?,
        )?;
        if let Some(edit) = &accepted.edit {
            continuation::validate_edit_result(
                &record.plan.root,
                &record.plan.source,
                &after,
                edit,
                &journal.spool()?,
            )?;
        } else if after != record.plan.source {
            return Err(invalid("source changed during artifact continuation"));
        }
        record.after_source = Some(after);
        record.phase = if record.plan.check.is_some() {
            TransferPhase::Checking
        } else {
            TransferPhase::Checkpointing
        };
        journal.save(record)?;
    }
    if record.phase == TransferPhase::Checking {
        let incoming = record
            .incoming
            .as_ref()
            .ok_or_else(|| invalid("missing accepted Work"))?;
        managed.check_current(incoming).await?;
        journal.ensure_current(&record.plan.id)?;
        source_current(journal, record)?;
        if let Some(identity) = &record.plan.check_executable {
            executable::verify(identity, &[&record.plan.root])?;
        }
        let recipe = record
            .plan
            .check
            .clone()
            .ok_or_else(|| invalid("missing pinned check recipe"))?;
        let spool = journal.spool()?;
        let request = check_request(record, recipe)?;
        let retained = if record.check.is_some() {
            record.check.clone()
        } else if recover && record.check_attempt.is_some() {
            run_operator_recipe(
                request.clone(),
                &spool,
                record.check_identity.as_deref().ok_or_else(|| invalid("check reservation lost its exact identity; preserve the unresolved boundary"))?,
                true,
                record
                    .check_attempt
                    .as_ref()
                    .is_some_and(NativeAttempt::quiescent),
                |_, _| Ok(()),
            )
            .map_err(capture_error)?
            .map(|observed| observed.finish)
        } else {
            None
        };
        let finish = if let Some(finish) = retained {
            finish
        } else {
            if record
                .check_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.pid.is_some() || attempt.pgid.is_some())
            {
                return Err(invalid(
                    "spawned check lost its durable start; preserve the unresolved boundary",
                ));
            }
            managed.check_current(incoming).await?;
            source_current(journal, record)?;
            // Dispatch admission is serialized with local revocation. A running admitted check
            // may finish; revocation prevents the next managed effect/checkpoint.
            let transfer_id = record.plan.id.clone();
            let check_identity = operator_recipe_identity(&request).map_err(capture_error)?;
            journal.effect(&transfer_id, || {
                record.check_attempt = Some(NativeAttempt::reserve());
                record.check_identity = Some(check_identity.clone());
                journal.save(record)
            })?;
            run_operator_recipe(
                request,
                &spool,
                &check_identity,
                false,
                false,
                |pid, pgid| {
                    let attempt = record
                        .check_attempt
                        .as_mut()
                        .ok_or_else(|| std::io::Error::other("missing check reservation"))?;
                    attempt.pid = Some(pid);
                    attempt.pgid = Some(pgid);
                    journal
                        .save(record)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                },
            )
            .map_err(capture_error)?
            .ok_or_else(|| invalid("operator check did not execute"))?
            .finish
        };
        record.check = Some(finish);
        journal.save(record)?;
        check_quiescent(record)?;
        // A failed completed check is useful retained progress, not authorization to retry it.
        // The final checkpoint reports the failure and leaves the next correction to a fresh preview.

        record.phase = TransferPhase::Checkpointing;
        journal.save(record)?;
    }
    if record.phase == TransferPhase::Checkpointing {
        journal.ensure_current(&record.plan.id)?;
        managed.authenticate().await?;
        source_current(journal, record)?;
        let incoming = record
            .incoming
            .as_ref()
            .ok_or_else(|| invalid("missing accepted Work"))?;
        let accepted = record
            .acceptance
            .as_ref()
            .ok_or_else(|| invalid("missing native acceptance"))?;
        let failed = record.check.as_ref().is_some_and(|check| {
            check.outcome != ActionOutcomeV1::Succeeded
                || check.result != CheckResultV1::Passed
                || check.drift != DriftV1::NoneObserved
        });
        let summary = if failed {
            format!(
                "{}; configured check did not establish success; inspect retained receipt",
                accepted.summary.chars().take(390).collect::<String>()
            )
        } else {
            accepted.summary.clone()
        };
        let next = if failed {
            "Inspect the retained failed/unknown check and correct only the affected work".into()
        } else {
            accepted.next_action.clone()
        };
        let request = checkpoint(
            incoming,
            &format!("{}-result", record.plan.id),
            CheckpointKind::Handover,
            &summary,
            Some(next),
        );
        let result = managed.checkpoint(&request).await?;
        if result.receipt.generation != incoming.generation
            || result.item.generation != incoming.generation
            || result.item.lead_session != incoming.lead_session
            || result.item.lead_token != managed.identity.token_id()
            || result.item.scope != incoming.scope
            || result.item.branch != incoming.branch
            || result.item.worktree != incoming.worktree
            || result.item.state != WorkState::Handover
            || result.item.updated_seq != result.receipt.event_seq
        {
            return Err(invalid(
                "result checkpoint is no longer the current managed boundary",
            ));
        }
        record.incoming = Some(result.item);
        record.phase = TransferPhase::Complete;
        record.error = None;
        journal.save(record)?;
    }
    Ok(())
}

async fn resolve(journal: &Journal, id: &str) -> Result<Value> {
    let mut record = journal.get(id)?;
    let _lock = journal.lock_work(&record.plan.work.id)?;
    record = journal.get(id)?;
    if matches!(
        record.phase,
        TransferPhase::Complete | TransferPhase::Resolved
    ) {
        return Ok(serde_json::to_value(record)?);
    }
    // Resolution performs observation and an idempotent Hub checkpoint, never new source/check work.
    if record
        .native
        .as_ref()
        .is_some_and(|attempt| !attempt.quiescent())
    {
        return Err(invalid(
            "native process/group is still present or quiescence is unobservable; stop it through its owner and retry. A launch without a PID marker requires a verified same-host reboot",
        ));
    }
    let mut check_never_started = false;
    let scope: WorkRepositoryScope =
        serde_json::from_value(record.plan.authority["scope"].clone())?;
    let managed = ManagedWork::connect(&record.plan.root, scope).await?;
    if record.plan.authority["identity"] != managed.identity_commitment()? {
        return Err(invalid("resolution requires the enrolled authority"));
    }
    let current = managed.read(&record.plan.work.id).await?;
    if record.incoming.is_none()
        && let Some(response) = &record.response
    {
        let before = record.handover.as_ref().unwrap_or(&record.plan.work);
        if current.generation == before.generation + 1
            && current.lead_session == response.session_id
            && current.lead_token == managed.identity.token_id()
        {
            if current.scope != before.scope
                || current.branch != before.branch
                || current.worktree != before.worktree
            {
                return Err(invalid("accepted Work boundary changed before resolution"));
            }
            record.incoming = Some(current.clone());
        }
    }
    if matches!(record.phase, TransferPhase::Applying)
        && let Some(edit) = record.acceptance.as_ref().and_then(|a| a.edit.as_ref())
    {
        let file = selected_file(&record.plan.root, &edit.path)?;
        if file.sha256 == agentscm::store::digest(edit.content.as_bytes()) {
            let after = capture_source(
                &record.plan.root,
                &record.plan.work.repository,
                &journal.spool()?,
            )?;
            continuation::validate_edit_result(
                &record.plan.root,
                &record.plan.source,
                &after,
                edit,
                &journal.spool()?,
            )?;
            record.after_source = Some(after);
        } else if file.sha256 != edit.before_sha256 {
            return Err(invalid(
                "source effect diverged; preserve the file and restore an observable before/after outcome before resolving",
            ));
        }
    }
    if record.phase == TransferPhase::Checking
        && record.check.is_none()
        && record.check_attempt.is_none()
    {
        // Reservation is durable before entering the runner, so this phase has not admitted a check.
        check_never_started = true;
    } else if record.phase == TransferPhase::Checking && record.check.is_none() {
        let recipe = record
            .plan
            .check
            .as_ref()
            .ok_or_else(|| invalid("missing check recipe"))?;
        let request = check_request(&record, recipe.clone())?;
        record.check = run_operator_recipe(
            request,
            &journal.spool()?,
            record.check_identity.as_deref().ok_or_else(|| {
                invalid(
                    "check reservation lost its exact identity; preserve the unresolved boundary",
                )
            })?,
            true,
            record
                .check_attempt
                .as_ref()
                .is_some_and(NativeAttempt::quiescent),
            |_, _| Ok(()),
        )
        .map_err(capture_error)?
        .map(|observed| observed.finish);
        if record.check.is_none() {
            if record
                .check_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.pid.is_some() || attempt.pgid.is_some())
            {
                return Err(invalid(
                    "spawned check lost its durable start; preserve the unresolved boundary",
                ));
            }
            // ActionStart is durable before spawn. Existing compaction preserves unfinished
            // starts and full completed receipts, so this exact empty pair proves no launch.
            check_never_started = true;
        }
    }

    if !check_never_started {
        check_quiescent(&record)?;
    }
    if let Some(incoming) = &record.incoming {
        if !same_responsibility(&current, incoming, managed.identity.token_id()) {
            return Err(invalid(
                "Work responsibility changed; resolution cannot adopt another executor",
            ));
        }
        if current.state == WorkState::Active {
            managed.check_current(&current).await?;
            let request = checkpoint(
                &current,
                &format!("{}-resolve-{}", record.plan.id, current.updated_seq),
                CheckpointKind::Handover,
                "Managed continuation stopped; retained outcomes require inspection before further work.",
                Some("Inspect retained outcomes and prepare a fresh explicit continuation".into()),
            );
            let result = managed.checkpoint(&request).await?;
            if result.receipt.generation != current.generation
                || !same_responsibility(&result.item, &current, managed.identity.token_id())
                || result.item.state != WorkState::Handover
                || result.item.updated_seq != result.receipt.event_seq
            {
                return Err(invalid(
                    "resolution checkpoint is no longer the current managed boundary",
                ));
            }
            record.incoming = Some(result.item);
        } else if current.state != WorkState::Handover {
            return Err(invalid("Work is not at an observable managed boundary"));
        } else {
            record.incoming = Some(current);
        }
    } else if !same_responsibility(&current, &record.plan.work, managed.identity.token_id()) {
        return Err(invalid("original Work responsibility changed"));
    }
    record.phase = TransferPhase::Resolved;
    record.error=Some(if check_never_started {"Resolved: no durable check start or finish exists; start-before-spawn ordering proves the admitted check never launched"}else{"Resolved by observing quiescent managed execution; retained proposal/effects were not replayed"}.into());
    journal.save(&record)?;
    Ok(serde_json::to_value(record)?)
}

fn same_responsibility(current: &WorkItem, expected: &WorkItem, token: &str) -> bool {
    current.generation == expected.generation
        && current.lead_session == expected.lead_session
        && current.lead_token == token
        && current.scope == expected.scope
        && current.branch == expected.branch
        && current.worktree == expected.worktree
}

fn effect_summary(record: &TransferRecord) -> Value {
    json!({"transfer_id":record.plan.id,"phase":record.phase,"destination":record.plan.destination,"session":record.response.as_ref().map(|r|&r.session_id),"native_diagnostic":record.native_diagnostic,"artifact":record.artifact,"edit":record.acceptance.as_ref().and_then(|a|a.edit.as_ref()).map(|e|json!({"path":e.path,"before_sha256":e.before_sha256,"after_sha256":agentscm::store::digest(e.content.as_bytes())})),"check":record.check,"source_after":record.after_source,"error":record.error,"revoked":record.revoked})
}

fn check_quiescent(record: &TransferRecord) -> Result<()> {
    if record
        .check_attempt
        .as_ref()
        .is_some_and(|attempt| !attempt.quiescent())
    {
        return Err(invalid(
            "check process/group remains present or quiescence is unobservable; a retained receipt alone does not release the managed boundary",
        ));
    }
    Ok(())
}

fn check_request(
    record: &TransferRecord,
    recipe: ConfiguredRecipeV1,
) -> Result<ConfiguredRecipeRunRequest> {
    let incoming = record
        .incoming
        .as_ref()
        .ok_or_else(|| invalid("missing accepted Work"))?;
    Ok(ConfiguredRecipeRunRequest {
        work_id: incoming.id.clone(),
        generation: incoming.generation,
        observed_session_id: incoming.lead_session.clone(),
        session_id: incoming.lead_session.clone(),
        source: source_request(&record.plan),
        setup_digest: agentscm::store::digest(record.plan.id.as_bytes()),
        recipe,
        managed_invocation: record.plan.id.clone(),
    })
}

fn checkpoint(
    item: &WorkItem,
    operation: &str,
    kind: CheckpointKind,
    summary: &str,
    next: Option<String>,
) -> WorkCheckpoint {
    WorkCheckpoint {
        work_id: item.id.clone(),
        operation_id: operation.into(),
        generation: item.generation,
        session: item.lead_session.clone(),
        expected_updated_seq: Some(item.updated_seq),
        kind,
        summary: summary.into(),
        next_action: next,
        scope: None,
        branch: None,
        worktree: None,
        resolve_blocker: false,
        source_refs: Vec::new(),
        depends_on: item.depends_on.clone(),
        related_work: item.related_work.clone(),
    }
}
fn source_current(journal: &Journal, record: &TransferRecord) -> Result<()> {
    if continuation::index_digest(&record.plan.root)? != record.plan.index_digest {
        return Err(invalid("Git index changed; create a fresh preview"));
    }
    for file in &record.plan.files {
        if selected_file(&record.plan.root, &file.path)?.mode != file.mode {
            return Err(invalid(
                "approved source permissions changed; create a fresh preview",
            ));
        }
    }
    let expected = record.after_source.as_ref().unwrap_or(&record.plan.source);
    if &capture_source(
        &record.plan.root,
        &record.plan.work.repository,
        &journal.spool()?,
    )? != expected
    {
        return Err(invalid(
            "source changed; affected evidence/transfer is stale, preserve files and create a fresh preview",
        ));
    }
    Ok(())
}
fn source_request(plan: &TransferPlan) -> GitSourceRequest {
    GitSourceRequest {
        repository: plan.work.repository.clone(),
        worktree_id: agentscm::store::digest(plan.root.as_os_str().as_encoded_bytes()),
        repository_path: plan.root.clone(),
        base_ref: "HEAD".into(),
    }
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(invalid("context/check input must be a regular file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(continuation::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > continuation::MAX_INPUT_BYTES {
        return Err(invalid("input exceeds 256 KiB"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
