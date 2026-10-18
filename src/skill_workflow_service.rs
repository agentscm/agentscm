//! Read-only projection of retained skill workflow runs for trusted-local surfaces.

use std::fs::symlink_metadata;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::authority::Authority;
use crate::skill_workflow::RunView;
use crate::store::Engine;
use crate::{Error, Result};

const PROJECTION_VERSION: u32 = 1;
const MAX_RUN_ID_BYTES: usize = 64;
const MAX_PROJECTION_BYTES: usize = 4 * 1024 * 1024;
const MAX_APPLICABILITY_REVISION_READ_BYTES: u64 = 64 * 1024;

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
