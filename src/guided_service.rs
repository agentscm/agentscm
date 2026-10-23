//! Bounded trusted-local projection of the existing guided journal.
//!
//! This module reads facts; it does not prepare, approve, retry or admit work.
//! The caller must supply the Engine opened from the same configured data directory.
//! Neither that directory nor an Engine selection may come from an HTTP request.
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::authority::Authority;
use crate::checks::{CheckAttempt, CheckEvidence, CheckState, SourcePathsRecipe};
use crate::context::{ChangeState, InputIdentity, ReviewContext};
use crate::local_recovery::AccessGuard;
use crate::model::{
    EventKind, IntegrateRequest, IntegrationEvent, PrepareLandingRequest, TargetState,
};
use crate::store::{Engine, MAX_DIFF_PAGE};
use crate::{Error, Result};

pub const MAX_VIEW_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SCAN_PAGES: usize = 128;
const MAX_SESSION_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum ReadRequest {
    Status,
    Inspect { limit: usize },
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    Active,
    Pending,
    Completed,
    AlreadyCurrent,
    CancelledStart,
    Indeterminate,
}

#[derive(Clone, Debug, Serialize)]
pub struct PendingView {
    pub kind: String,
    pub operation_id: Option<String>,
    /// Absence of a retained result is not proof that an operation never ran.
    pub outcome: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvidenceView {
    pub selected_attempt: Option<CheckAttempt>,
    pub retained_evidence: Option<CheckEvidence>,
    pub controller_status: String,
    pub controller_basis: String,
    pub selected_success_matches_subject: bool,
    pub operator_notes: Vec<Value>,
    pub operator_notes_basis: String,
    pub execution_proof: String,
    pub model_review_proof: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkflowView {
    pub version: u32,
    pub issue: String,
    pub state: WorkflowState,
    pub recorded_context: ReviewContext,
    pub current_change: Option<ChangeState>,
    pub current_context: Option<ReviewContext>,
    pub expected_target: Option<TargetState>,
    pub current_target: Option<TargetState>,
    pub current_policy_epoch: u64,
    /// A revalidated read observation, never an admission authorization.
    pub context_current: bool,
    pub pending: Option<PendingView>,
    pub event: Option<IntegrationEvent>,
    pub event_status: String,
    pub local_approval_recorded: bool,
    pub workspace_observation: String,
    pub retained_workspace_binding: Option<Value>,
    pub restored_reattachment_required: bool,
    pub evidence: EvidenceView,
    pub diff: Option<Value>,
    pub admission: String,
}

// The current writer's complete v1 schema, including fields deliberately not
// exposed by a service read (workspace paths, decision tokens and requests).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedSession {
    version: u32,
    issue: String,
    #[serde(rename = "workspace")]
    _workspace: PathBuf,
    workspace_binding: Option<Value>,
    #[serde(default, rename = "workspace_identity")]
    _workspace_identity: Option<Value>,
    #[serde(default)]
    restored_workspace_ack: Option<PathBuf>,
    #[serde(default)]
    restored_instance_ack: Option<String>,
    context: ReviewContext,
    #[serde(rename = "recipe")]
    _recipe: Option<SourcePathsRecipe>,
    pending: Option<RetainedPending>,
    #[serde(rename = "prepared_tip")]
    _prepared_tip: Option<String>,
    #[serde(rename = "inspection")]
    _inspection: Option<String>,
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

#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum RetainedPending {
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

impl RetainedPending {
    fn view(&self) -> PendingView {
        let (kind, operation_id) = match self {
            Self::Start => ("start", None),
            Self::Checkpoint {
                operation_id,
                generation,
            } => {
                let _ = generation;
                ("checkpoint", Some(operation_id.clone()))
            }
            Self::Update { expected, context } => {
                let _ = (expected, context);
                ("update", None)
            }
            Self::Prepare { request } => ("prepare", Some(request.operation_id.clone())),
            Self::Integrate { request } => ("integrate", Some(request.operation_id.clone())),
        };
        PendingView {
            kind: kind.into(),
            operation_id,
            outcome: "not_reconciled".into(),
        }
    }
}

/// Read one existing guided session. No journal/lock creation, schema migration,
/// workspace access, pending-operation retry or authorization occurs here.
///
/// Authority and journal SQLite reads can use WAL/SHM metadata. This is logical
/// read-only access on a trusted local filesystem, not a hostile-host sandbox.
/// The supplied Engine must belong to `data_dir`; the server owns that binding.
pub fn read(data_dir: &Path, engine: &Engine, request: ReadRequest) -> Result<WorkflowView> {
    if let ReadRequest::Inspect { limit } = request {
        validate_limit(limit)?;
    }
    let data = data_dir.canonicalize()?;
    let _access = AccessGuard::shared_existing(&data)?;
    require_regular(&data.join("workflow.sqlite"))?;
    require_regular(&data.join("workflow.lock"))?;
    let _lease = crate::lease::Lease::try_shared(
        OpenOptions::new()
            .read(true)
            .open(data.join("workflow.lock"))?,
    )
    .map_err(|_| {
        Error::Conflict(
            "guided workflow is active; retry its read after the command completes".into(),
        )
    })?;
    let session = read_session(&data)?;
    let authority = Authority::open_read_only(&data)?;
    if authority.repository_id()? != session.context.change.repository {
        return Err(Error::Conflict(
            "guided journal belongs to another repository".into(),
        ));
    }
    session.context.validate()?;
    let current_change = optional(authority.change(&session.context.change))?;
    let current_context = current_change
        .as_ref()
        .map(|change| authority.review_context(&change.context_digest))
        .transpose()?;
    let current_target = authority.target(&session.context.target)?;
    let current_policy_epoch = authority.policy_epoch()?;
    let expected_target = session.context.base.as_ref().map(|base| TargetState {
        revision: base.revision.clone(),
        version: base.target_version,
    });
    let recorded_digest = session.context.digest()?;
    let mut context_current = current_change.as_ref().is_some_and(|change| {
        change.version == session.context.change_version && change.context_digest == recorded_digest
    }) && current_policy_epoch == session.context.policy_epoch
        && current_target == expected_target;
    let mut pending = session.pending.as_ref().map(RetainedPending::view);
    let mut event = None;
    let mut event_status = "not_recorded".to_owned();
    let mut state = if session.cancelled_start.is_some() {
        WorkflowState::CancelledStart
    } else if session.already_current {
        WorkflowState::AlreadyCurrent
    } else if session.pending.is_some() {
        WorkflowState::Pending
    } else {
        WorkflowState::Active
    };
    if let Some(recorded) = &session.event {
        if lookup_event(&authority, &recorded.operation_id)?.as_ref() == Some(recorded) {
            event = Some(recorded.clone());
            event_status = "retained_authority_event".into();
            state = WorkflowState::Completed;
        } else {
            event_status = "journal_receipt_not_retained_by_authority".into();
            state = WorkflowState::Indeterminate;
        }
    } else if let Some(RetainedPending::Integrate { request }) = &session.pending {
        if let Some(recorded) = lookup_event(&authority, &request.operation_id)? {
            if event_matches(&recorded, request) {
                event = Some(recorded);
                event_status = "retained_authority_event".into();
                state = WorkflowState::Completed;
                if let Some(pending) = &mut pending {
                    pending.outcome = "exact_retained_result".into();
                }
            } else {
                event_status = "operation_id_has_different_result".into();
                state = WorkflowState::Indeterminate;
            }
        } else {
            // No retry, and no conclusion that the operation did not execute.
            event_status = "no_retained_result_for_pending_operation".into();
        }
    }
    let selected_attempt = session
        .attempt
        .as_ref()
        .map(|id| optional(authority.check_attempt(id)))
        .transpose()?
        .flatten();
    let retained_evidence = selected_attempt
        .as_ref()
        .and_then(|attempt| attempt.evidence_digest.as_ref())
        .map(|digest| optional(authority.check_evidence(digest)))
        .transpose()?
        .flatten();
    let mut selected_success_matches_subject = evidence_matches(
        &session.context,
        selected_attempt.as_ref(),
        retained_evidence.as_ref(),
    )?;
    let mut controller_status: String = match &selected_attempt {
        Some(attempt) if !attempt_matches(&session.context, attempt)? => "stale".into(),
        Some(attempt) => match attempt.state {
            CheckState::Pending => "pending",
            CheckState::Running => "running",
            CheckState::Success if selected_success_matches_subject => "success",
            CheckState::Success => "success_without_matching_evidence",
            CheckState::Fail => "fail",
            CheckState::Incomplete => "incomplete",
            CheckState::Cancelled => "cancelled",
        }
        .into(),
        None if session.attempt.is_some() => "missing".into(),
        None => "not_selected".into(),
    };
    let controller_basis = match &selected_attempt {
        Some(attempt) if attempt.check_kind == "source_paths/v1" => match &retained_evidence {
            Some(evidence) => format!("source_integrity/{}", evidence.execution_basis),
            None => "source_integrity/not_completed".into(),
        },
        Some(_) => "unsupported_check_kind".into(),
        None => "not_collected".into(),
    };
    let diff = match request {
        ReadRequest::Status => None,
        ReadRequest::Inspect { limit } => {
            let left = session
                .context
                .base
                .as_ref()
                .map(|base| base.revision.as_str())
                .or(session.empty_baseline.as_deref())
                .ok_or_else(|| {
                    Error::Conflict("guided session has no retained inspection baseline".into())
                })?;
            Some(source_diff(engine, left, &session.context.revision, limit)?)
        }
    };
    // These are observations, not an atomic read/admission transaction. Refuse
    // to mark a moving subject or attempt as current in the rendered result.
    context_current &= optional(authority.change(&session.context.change))? == current_change
        && authority.target(&session.context.target)? == current_target
        && authority.policy_epoch()? == current_policy_epoch;
    if let Some(attempt) = &selected_attempt {
        let attempt_unchanged =
            optional(authority.check_attempt(&attempt.attempt_id))?.as_ref() == Some(attempt);
        selected_success_matches_subject &= attempt_unchanged;
        if !attempt_unchanged {
            controller_status = "changed_during_read".into();
        } else if !context_current {
            controller_status = "stale".into();
        }
    }
    selected_success_matches_subject &= context_current;
    let restored_reattachment_required = restored_reattachment(&data, &session)?
        && !matches!(
            state,
            WorkflowState::Completed
                | WorkflowState::AlreadyCurrent
                | WorkflowState::CancelledStart
        );
    let view = WorkflowView {
        version: 1,
        issue: session.issue,
        state,
        recorded_context: session.context,
        current_change,
        current_context,
        expected_target,
        current_target,
        current_policy_epoch,
        context_current,
        pending,
        event,
        event_status,
        local_approval_recorded: session.approved.is_some(),
        workspace_observation: "not_observed".into(),
        retained_workspace_binding: session.workspace_binding,
        restored_reattachment_required,
        evidence: EvidenceView {
            selected_attempt,
            retained_evidence,
            controller_status,
            controller_basis,
            selected_success_matches_subject,
            operator_notes: session.notes,
            operator_notes_basis: "unverified_operator_claim".into(),
            execution_proof: "not_collected_by_this_workflow".into(),
            model_review_proof: "not_collected_by_this_workflow".into(),
        },
        diff,
        admission: "not_assessed".into(),
    };
    if serde_json::to_vec(&view)?.len() > MAX_VIEW_BYTES {
        return Err(Error::Limit(
            "guided service view exceeds output bound".into(),
        ));
    }
    Ok(view)
}

fn optional<T>(value: Result<T>) -> Result<Option<T>> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn require_regular(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(Error::Invalid(
            "guided service requires regular existing journal metadata".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound(
            "no existing guided journal and lock".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

fn read_session(data: &Path) -> Result<RetainedSession> {
    let db = Connection::open_with_flags(
        data.join("workflow.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let application: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != 0x41534746 || version != 1 {
        return Err(Error::Invalid(
            "unsupported guided journal identity/version".into(),
        ));
    }
    let count: i64 = db.query_row("SELECT count(*) FROM session", [], |row| row.get(0))?;
    if count == 0 {
        return Err(Error::NotFound("no singleton guided session".into()));
    }
    if count != 1 {
        return Err(Error::Invalid(
            "guided journal does not contain one singleton session".into(),
        ));
    }
    // CASE prevents an oversized SQLite value being copied into application memory.
    let row: Option<(i64, Option<Vec<u8>>)> = db.query_row(
        "SELECT length(state),CASE WHEN typeof(state)='blob' AND length(state)<=?1 THEN state ELSE NULL END FROM session WHERE id=1",
        [MAX_SESSION_BYTES as i64], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let (length, bytes) =
        row.ok_or_else(|| Error::NotFound("no singleton guided session".into()))?;
    if length > MAX_SESSION_BYTES as i64 {
        return Err(Error::Limit("guided journal exceeds read bound".into()));
    }
    let bytes = bytes
        .ok_or_else(|| Error::Invalid("guided journal state must be a bounded blob".into()))?;
    let session: RetainedSession = serde_json::from_slice(&bytes)?;
    if session.version != 1 {
        return Err(Error::Invalid("unsupported guided session version".into()));
    }
    Ok(session)
}

fn restored_reattachment(data: &Path, session: &RetainedSession) -> Result<bool> {
    let Some(root) = data.parent() else {
        return Ok(false);
    };
    match std::fs::symlink_metadata(root.join(".agentscm-recovery/restore-complete.json")) {
        Ok(metadata) if metadata.is_file() => {
            let instance = crate::local_recovery::restore_instance(root)?;
            Ok(session.restored_workspace_ack.as_deref() != Some(root)
                || session.restored_instance_ack.as_deref() != Some(instance.as_str()))
        }
        Ok(_) => Err(Error::Invalid(
            "restore receipt must be a regular file".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn attempt_matches(context: &ReviewContext, attempt: &CheckAttempt) -> Result<bool> {
    Ok(attempt.subject.repository == context.change.repository
        && attempt.subject.change == context.change
        && attempt.subject.change_version == context.change_version
        && attempt.subject.context_digest == context.digest()?
        && attempt.subject.revision == context.revision
        && input_digest(&context.recipe) == Some(attempt.subject.recipe_sha256.as_str())
        && input_digest(&context.toolchain) == Some(attempt.subject.toolchain_sha256.as_str())
        && input_digest(&context.environment) == Some(attempt.subject.environment_sha256.as_str()))
}

fn input_digest(value: &InputIdentity) -> Option<&str> {
    match value {
        InputIdentity::Digest { sha256 } => Some(sha256.as_str()),
        _ => None,
    }
}

fn evidence_matches(
    context: &ReviewContext,
    attempt: Option<&CheckAttempt>,
    evidence: Option<&CheckEvidence>,
) -> Result<bool> {
    let (Some(attempt), Some(evidence)) = (attempt, evidence) else {
        return Ok(false);
    };
    Ok(attempt_matches(context, attempt)?
        && attempt.state == CheckState::Success
        && attempt.check_kind == "source_paths/v1"
        && evidence.check_kind == attempt.check_kind
        && evidence.execution_basis == "controller_builtin"
        && evidence.state == CheckState::Success
        && evidence.subject == attempt.subject
        && evidence.attempt_id == attempt.attempt_id
        && evidence.request_digest == attempt.request_digest
        && evidence.run_generation == attempt.run_generation
        && attempt.evidence_digest.as_deref() == Some(evidence.digest()?.as_str()))
}

fn lookup_event(authority: &Authority, operation: &str) -> Result<Option<IntegrationEvent>> {
    let mut after = 0;
    for _ in 0..100 {
        let events = authority.events(after, 1000)?;
        if let Some(event) = events.iter().find(|event| event.operation_id == operation) {
            return Ok(Some(event.clone()));
        }
        if events.len() < 1000 {
            return Ok(None);
        }
        after = events.last().expect("nonempty event page").sequence;
    }
    Err(Error::Limit(
        "guided outcome read exceeds 100000 events".into(),
    ))
}

fn event_matches(event: &IntegrationEvent, request: &IntegrateRequest) -> bool {
    let kind = if request.expected_target.is_none() {
        EventKind::Initialize
    } else {
        EventKind::Integrate
    };
    event.kind == kind
        && event.target == request.target
        && event.after.revision == request.revision
        && event.before == request.expected_target
        && event.approval_id == request.approval_id
        && event.context_digest == request.context_digest
        && event.change == request.change
        && event.change_version == request.change_version
        && event.policy_epoch == request.policy_epoch
}

fn validate_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_DIFF_PAGE).contains(&limit) {
        return Err(Error::Limit(format!(
            "inspection diff limit must be 1..{MAX_DIFF_PAGE}"
        )));
    }
    Ok(())
}

/// Exact immutable diff shared by the CLI's approval recheck and service reads.
/// It returns source bytes and completeness facts, never an approval token.
pub fn source_diff(engine: &Engine, left: &str, right: &str, limit: usize) -> Result<Value> {
    validate_limit(limit)?;
    inspection_diff(engine, left, right, limit, MAX_SCAN_PAGES)
}

/// Readable file bytes for JSON: `<prefix>text` when valid UTF-8, otherwise
/// `<prefix>base64` (standard alphabet). Nothing is added for an absent side.
pub fn content_fields(target: &mut Value, prefix: &str, bytes: Option<&[u8]>) {
    use base64::Engine as _;
    let Some(bytes) = bytes else { return };
    match std::str::from_utf8(bytes) {
        Ok(text) => target[format!("{prefix}text")] = json!(text),
        Err(_) => {
            target[format!("{prefix}base64")] =
                json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
}

// The Engine bounds traversal work separately from changed-entry count. Empty
// pages can still advance over unchanged source and must not end inspection.
fn inspection_diff(
    e: &Engine,
    left: &str,
    right: &str,
    limit: usize,
    max_pages: usize,
) -> Result<Value> {
    let mut continuation: Option<String> = None;
    let mut needed_objects = Vec::new();
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut scan_pages = 0;
    let mut complete = false;
    let mut incomplete_reason = Some("scan_page_limit");
    'pages: for _ in 0..max_pages {
        let request_cursor = continuation.clone();
        let page = e.diff_page(
            left,
            right,
            request_cursor.as_deref(),
            (limit - entries.len()).max(1),
        )?;
        scan_pages += 1;
        continuation = page.next;
        needed_objects = page.needed_objects;
        if !needed_objects.is_empty() {
            incomplete_reason = Some("needed_objects");
            break;
        }
        for entry in page.entries {
            if entries.len() == limit {
                continuation = request_cursor;
                incomplete_reason = Some("entry_limit");
                break 'pages;
            }
            let before = match &entry.before {
                Some(id) => match e.blob_by_digest_bounded(id, 256 * 1024)? {
                    Some(bytes) => Some(bytes),
                    None => {
                        continuation = request_cursor;
                        incomplete_reason = Some("file_content_limit");
                        break 'pages;
                    }
                },
                None => None,
            };
            let remaining = 256 * 1024 - before.as_ref().map_or(0, Vec::len);
            let after = match &entry.after {
                Some(id) => match e.blob_by_digest_bounded(id, remaining)? {
                    Some(bytes) => Some(bytes),
                    None => {
                        continuation = request_cursor;
                        incomplete_reason = Some("file_content_limit");
                        break 'pages;
                    }
                },
                None => None,
            };
            let mut item =
                json!({"path":entry.path,"before_sha256":entry.before,"after_sha256":entry.after,});
            content_fields(&mut item, "before_", before.as_deref());
            content_fields(&mut item, "after_", after.as_deref());
            // Kinds appear only for non-regular sides, as in `DiffEntry`.
            for (name, kind) in [
                ("before_kind", entry.before_kind),
                ("after_kind", entry.after_kind),
            ] {
                if let Some(kind) = kind {
                    item[name] = json!(kind);
                }
            }
            let item_bytes = serde_json::to_vec(&item)?.len();
            if bytes + item_bytes > MAX_VIEW_BYTES / 2 {
                continuation = request_cursor;
                incomplete_reason = Some("render_limit");
                break 'pages;
            }
            bytes += item_bytes;
            entries.push(item);
        }
        if continuation.is_none() {
            complete = true;
            incomplete_reason = None;
            break;
        }
    }
    Ok(
        json!({"left":left,"right":right,"entries":entries,"complete":complete,"needed_objects":needed_objects,"continuation":continuation,"scan_pages":scan_pages,"scan_page_limit":max_pages,"incomplete_reason":incomplete_reason}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileChange;

    #[test]
    fn inspection_scan_budget_exhaustion_never_claims_a_complete_diff() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(temp.path()).unwrap();
        let files: Vec<_> = (0..200)
            .map(|i| FileChange {
                path: format!("file-{i:03}.txt"),
                content: Some(b"before".to_vec()),
            })
            .collect();
        let left = engine.checkpoint(None, &files).unwrap();
        let mut cursor = None;
        let mut last = None;
        loop {
            let page = engine.list_files(&left, cursor.as_deref(), 4096).unwrap();
            if let Some(file) = page.entries.last() {
                last = Some(file.path.clone());
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        let right = engine
            .checkpoint(
                Some(&left),
                &[FileChange {
                    path: last.unwrap(),
                    content: Some(b"after".to_vec()),
                }],
            )
            .unwrap();
        let bounded = inspection_diff(&engine, &left, &right, MAX_DIFF_PAGE, 1).unwrap();
        assert_eq!(bounded["complete"], false);
        assert_eq!(bounded["incomplete_reason"], "scan_page_limit");
        assert_eq!(bounded["scan_pages"], 1);
        assert!(bounded["entries"].as_array().unwrap().is_empty());
        assert!(bounded["continuation"].is_string());
        let full = inspection_diff(&engine, &left, &right, 1, MAX_SCAN_PAGES).unwrap();
        assert_eq!(full["complete"], true);
        assert_eq!(full["entries"].as_array().unwrap().len(), 1);
        assert!(full["continuation"].is_null());
    }
    #[test]
    fn content_fields_use_text_for_utf8_and_base64_otherwise() {
        let mut v = json!({});
        content_fields(&mut v, "x_", Some("hi \u{e9}".as_bytes()));
        content_fields(&mut v, "y_", Some(&[0, 255, 16]));
        content_fields(&mut v, "z_", None);
        assert_eq!(v, json!({"x_text":"hi \u{e9}","y_base64":"AP8Q"}));
    }
    #[test]
    fn inspection_caps_each_side_before_allocation_and_the_combined_payload() {
        for (before_len, after_len, complete) in [
            (256 * 1024 + 1, 1, false),
            (1, 256 * 1024 + 1, false),
            (160 * 1024, 160 * 1024, false),
            (128 * 1024, 128 * 1024, true),
            (256 * 1024, 0, true),
        ] {
            let temp = tempfile::TempDir::new().unwrap();
            let engine = Engine::open(temp.path()).unwrap();
            let left = engine
                .checkpoint(
                    None,
                    &[FileChange {
                        path: "a.txt".into(),
                        content: Some(vec![b'b'; before_len]),
                    }],
                )
                .unwrap();
            let right = engine
                .checkpoint(
                    Some(&left),
                    &[FileChange {
                        path: "a.txt".into(),
                        content: Some(vec![b'a'; after_len]),
                    }],
                )
                .unwrap();
            let diff = source_diff(&engine, &left, &right, MAX_DIFF_PAGE).unwrap();
            assert_eq!(diff["complete"], complete);
            if complete {
                assert_eq!(
                    diff["entries"][0]["before_text"].as_str().unwrap().len(),
                    before_len
                );
                assert_eq!(
                    diff["entries"][0]["after_text"].as_str().unwrap().len(),
                    after_len
                );
            } else {
                assert_eq!(diff["incomplete_reason"], "file_content_limit");
                assert!(diff["entries"].as_array().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn inspection_shows_kind_only_and_symlink_retarget_changes() {
        use crate::model::{EntryChange, EntryKind};
        let temp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(temp.path()).unwrap();
        let entry = |path: &str, content: &[u8], kind| EntryChange {
            path: path.into(),
            content: Some(content.to_vec()),
            kind,
        };
        let left = engine
            .checkpoint_entries(
                None,
                &[
                    entry("run.sh", b"x", EntryKind::Regular),
                    entry("link", b"old", EntryKind::Symlink),
                    entry("same", b"x", EntryKind::Regular),
                ],
            )
            .unwrap();
        let right = engine
            .checkpoint_entries(
                Some(&left),
                &[
                    entry("run.sh", b"x", EntryKind::Executable),
                    entry("link", b"new", EntryKind::Symlink),
                ],
            )
            .unwrap();
        let diff = source_diff(&engine, &left, &right, MAX_DIFF_PAGE).unwrap();
        assert_eq!(diff["complete"], true);
        let entries = diff["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let find = |path: &str| entries.iter().find(|e| e["path"] == path).unwrap();
        let run = find("run.sh");
        assert_eq!(run["after_kind"], "executable");
        assert!(run.get("before_kind").is_none());
        let link = find("link");
        assert_eq!(link["before_kind"], "symlink");
        assert_eq!(link["after_kind"], "symlink");
        assert_eq!(link["after_text"], "new");
    }
}
