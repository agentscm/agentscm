//! Read-only projection of retained skill workflow runs for trusted-local surfaces.

use std::fs::symlink_metadata;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::authority::Authority;
use crate::skill_workflow::runtime::RunSummary;
use crate::skill_workflow::{RunView, StepAction};
use crate::store::Engine;
use crate::{Error, Result};

const PROJECTION_VERSION: u32 = 1;
const MAX_RUN_ID_BYTES: usize = 64;
const MAX_PROJECTION_BYTES: usize = 4 * 1024 * 1024;
const MAX_APPLICABILITY_REVISION_READ_BYTES: u64 = 64 * 1024;
const MAX_LIST_RUNS: usize = 16;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplicabilityState {
    Current,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresentApplicability {
    /// The only facts assessed by this projection.
    pub basis: String,
    pub state: ApplicabilityState,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillWorkflowProjection {
    pub version: u32,
    /// The retained historical observation. Its state is not an admission decision.
    pub run: RunView,
    /// Whether the retained run's source binding still matches current local authority.
    pub present_applicability: PresentApplicability,
    /// Current Change, review-context and protected-target binding for command-backed
    /// workflows. `None` means the retained definition has no landing binding.
    pub present_landing_binding: Option<PresentLandingBinding>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PresentLandingBinding {
    pub basis: String,
    pub state: ApplicabilityState,
    pub reason: Option<String>,
    pub change_id: Option<String>,
    pub change_version: Option<u64>,
    pub context_digest: Option<String>,
    pub target: Option<String>,
}

/// Validates the identifier accepted by both the runtime and the read surface.
pub fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > MAX_RUN_ID_BYTES
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid(
            "workflow run ID must be 1..64 canonical ASCII bytes".into(),
        ));
    }
    Ok(())
}

/// Reads existing workflow rows without logical state changes or journal initialization.
/// SQLite may manage WAL/SHM sidecars for existing databases.
///
/// `data_dir` is selected by the operator when the native service starts. The
/// caller supplies the already-open native engine rooted at that same directory.
pub fn read(data_dir: &Path, engine: &Engine, run_id: &str) -> Result<SkillWorkflowProjection> {
    validate_run_id(run_id)?;
    require_existing_journal(data_dir)?;

    let mut run = crate::skill_workflow::status(data_dir, run_id).map_err(normalize_absence)?;
    redact_internal_step_errors(&mut run);
    let present_applicability = present_applicability(data_dir, engine, &run);
    let projection = SkillWorkflowProjection {
        version: PROJECTION_VERSION,
        present_landing_binding: present_landing_binding(data_dir, &run),
        run,
        present_applicability,
    };
    if serde_json::to_vec(&projection)?.len() > MAX_PROJECTION_BYTES {
        return Err(Error::Limit(format!(
            "skill workflow projection exceeds {MAX_PROJECTION_BYTES} bytes"
        )));
    }
    Ok(projection)
}

/// Lists a bounded set of newest-created retained runs without projecting their
/// steps. A configured native store with no workflow journal has no runs.
pub fn list(data_dir: &Path, limit: usize) -> Result<Vec<RunSummary>> {
    Ok(list_page(data_dir, limit, None, None)?.runs)
}

pub fn list_page(
    data_dir: &Path,
    limit: usize,
    before: Option<i64>,
    search: Option<&str>,
) -> Result<crate::skill_workflow::runtime::RunPage> {
    if !(1..=MAX_LIST_RUNS).contains(&limit) || before.is_some_and(|value| value <= 0) {
        return Err(Error::Limit("workflow list supports 1..16 runs".into()));
    }
    match symlink_metadata(data_dir.join("skill-workflows")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::skill_workflow::runtime::RunPage {
                runs: vec![],
                next_cursor: None,
                searched_to: None,
            });
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    require_existing_journal(data_dir)?;
    crate::skill_workflow::runtime::list_run_page(data_dir, limit, before, search)
        .map_err(normalize_absence)
}

fn present_landing_binding(data_dir: &Path, run: &RunView) -> Option<PresentLandingBinding> {
    let definition =
        match crate::skill_workflow::runtime::retained_definition(data_dir, &run.run_id) {
            Ok(definition) => definition,
            Err(_) => {
                return Some(landing_binding(
                    ApplicabilityState::Unavailable,
                    "the retained workflow definition is unavailable",
                    None,
                    None,
                    None,
                    None,
                ));
            }
        };
    let mut commands = definition
        .steps
        .iter()
        .filter_map(|step| match &step.action {
            StepAction::Command {
                change,
                change_version,
                context_digest,
                ..
            } => Some((change, *change_version, context_digest)),
            _ => None,
        });
    let (change, change_version, context_digest) = commands.next()?;
    if commands.any(|candidate| {
        candidate.0 != change || candidate.1 != change_version || candidate.2 != context_digest
    }) {
        return Some(landing_binding(
            ApplicabilityState::Unavailable,
            "the workflow contains more than one landing binding",
            Some(&change.id),
            Some(change_version),
            Some(context_digest),
            None,
        ));
    }

    let authority = match Authority::open_read_only(data_dir) {
        Ok(authority) => authority,
        Err(_) => {
            return Some(landing_binding(
                ApplicabilityState::Unavailable,
                "current native authority is unavailable",
                Some(&change.id),
                Some(change_version),
                Some(context_digest),
                None,
            ));
        }
    };
    let context = match authority.review_context(context_digest) {
        Ok(context) => context,
        Err(Error::NotFound(_)) => {
            return Some(landing_binding(
                ApplicabilityState::Stale,
                "the recorded review context is no longer available",
                Some(&change.id),
                Some(change_version),
                Some(context_digest),
                None,
            ));
        }
        Err(_) => {
            return Some(landing_binding(
                ApplicabilityState::Unavailable,
                "current review context could not be inspected",
                Some(&change.id),
                Some(change_version),
                Some(context_digest),
                None,
            ));
        }
    };
    let result = landing_binding(
        ApplicabilityState::Current,
        "",
        Some(&change.id),
        Some(change_version),
        Some(context_digest),
        Some(&context.target),
    );
    if context.change != *change
        || context.change_version != change_version
        || context.revision != run.source.revision
        || context.policy_epoch != run.source.policy_epoch
        || change.repository != run.source.repository
    {
        return Some(PresentLandingBinding {
            state: ApplicabilityState::Stale,
            reason: Some("the recorded review context does not bind this workflow source".into()),
            ..result
        });
    }
    match authority.change(change) {
        Ok(current)
            if current.version == change_version && current.context_digest == *context_digest => {}
        Ok(_) | Err(Error::NotFound(_)) => {
            return Some(PresentLandingBinding {
                state: ApplicabilityState::Stale,
                reason: Some("the Change or its review context changed after this run".into()),
                ..result
            });
        }
        Err(_) => {
            return Some(PresentLandingBinding {
                state: ApplicabilityState::Unavailable,
                reason: Some("the current Change could not be inspected".into()),
                ..result
            });
        }
    }
    let target = match authority.target(&context.target) {
        Ok(target) => target,
        Err(_) => {
            return Some(PresentLandingBinding {
                state: ApplicabilityState::Unavailable,
                reason: Some("the current protected target could not be inspected".into()),
                ..result
            });
        }
    };
    let target_matches = match (&context.base, target) {
        (None, None) => true,
        (Some(base), Some(target)) => {
            base.revision == target.revision && base.target_version == target.version
        }
        _ => false,
    };
    if !target_matches {
        return Some(PresentLandingBinding {
            state: ApplicabilityState::Stale,
            reason: Some("the protected target changed after this run".into()),
            ..result
        });
    }
    Some(result)
}

fn landing_binding(
    state: ApplicabilityState,
    reason: &str,
    change_id: Option<&str>,
    change_version: Option<u64>,
    context_digest: Option<&str>,
    target: Option<&str>,
) -> PresentLandingBinding {
    PresentLandingBinding {
        basis: "current_change_version_review_context_target_and_policy_epoch".into(),
        state,
        reason: (!reason.is_empty()).then(|| reason.into()),
        change_id: change_id.map(str::to_owned),
        change_version,
        context_digest: context_digest.map(str::to_owned),
        target: target.map(str::to_owned),
    }
}

fn redact_internal_step_errors(run: &mut RunView) {
    for step in &mut run.steps {
        if step
            .detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("could not read "))
        {
            step.detail =
                Some("source path could not be read; internal error detail was redacted".into());
        }
    }
}

fn require_existing_journal(data_dir: &Path) -> Result<()> {
    for path in [
        data_dir.join("skill-workflows"),
        data_dir.join("skill-workflows/journal.lock"),
        data_dir.join("skill-workflows/state.sqlite"),
    ] {
        match symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound("skill workflow run".into()));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn normalize_absence(error: Error) -> Error {
    match error {
        Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Error::NotFound("skill workflow run".into())
        }
        error => error,
    }
}

fn present_applicability(data_dir: &Path, engine: &Engine, run: &RunView) -> PresentApplicability {
    let authority = match Authority::open_read_only(data_dir) {
        Ok(authority) => authority,
        Err(_) => return unavailable("current native authority is unavailable"),
    };
    let repository = match authority.repository_id() {
        Ok(repository) => repository,
        Err(_) => return unavailable("current native authority is unavailable"),
    };
    if repository != run.source.repository {
        return stale("repository binding changed after this run was recorded");
    }
    let policy_epoch = match authority.policy_epoch() {
        Ok(policy_epoch) => policy_epoch,
        Err(_) => return unavailable("current native authority is unavailable"),
    };
    if policy_epoch != run.source.policy_epoch {
        return stale("policy epoch changed after this run was recorded");
    }
    let (source, budget) = engine.with_read_budget(MAX_APPLICABILITY_REVISION_READ_BYTES);
    if budget
        .finish(source.load_revision(&run.source.revision))
        .is_err()
    {
        return unavailable("the run's immutable source revision is unavailable");
    }
    PresentApplicability {
        basis: "current_authority_repository_policy_epoch_and_immutable_revision".into(),
        state: ApplicabilityState::Current,
        reason: None,
    }
}

fn stale(reason: &str) -> PresentApplicability {
    PresentApplicability {
        basis: "current_authority_repository_policy_epoch_and_immutable_revision".into(),
        state: ApplicabilityState::Stale,
        reason: Some(reason.into()),
    }
}

fn unavailable(reason: &str) -> PresentApplicability {
    PresentApplicability {
        basis: "current_authority_repository_policy_epoch_and_immutable_revision".into(),
        state: ApplicabilityState::Unavailable,
        reason: Some(reason.into()),
    }
}
