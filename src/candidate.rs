//! Platform-neutral candidate identity shared by export and Hub protocols.
#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use crate::Error;
use crate::Result;
#[cfg(unix)]
use crate::attached_worktree::{AttachmentJournal, AttachmentState};
#[cfg(unix)]
use crate::authority::Authority;
#[cfg(unix)]
use crate::skill_workflow::LiveImportedOrigins;
#[cfg(unix)]
use crate::store::Engine;
use serde::{Deserialize, Serialize};

/// An unlanded, exact native subject prepared for an ordinary Git branch/PR.
/// It is deliberately not an integration event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateSubject {
    pub id: String,
    pub attachment: String,
    pub endpoint: String,
    pub output: String,
    pub change: crate::context::ChangeId,
    pub change_version: u64,
    pub context_digest: String,
    pub policy_epoch: u64,
    pub revision: String,
    pub target: String,
    pub baseline_commit: String,
    pub parent_commit: String,
    pub source_commit: String,
    pub source_fingerprint: String,
    pub comparison_fingerprint: String,
}

impl CandidateSubject {
    pub(crate) fn same_lineage(&self, other: &Self) -> bool {
        self.attachment == other.attachment
            && self.change == other.change
            && self.endpoint == other.endpoint
            && self.output == other.output
            && self.target == other.target
    }
    pub fn canonical_id(&self) -> Result<String> {
        let mut fixed = self.clone();
        fixed.id.clear();
        Ok(crate::store::digest(&serde_json::to_vec(&fixed)?))
    }
}

/// Compare an attached Git source with its exact retained native candidate and build the
/// unlanded subject. The live proof is required even for native-only workflows so callers
/// cannot accidentally bypass the shared authorization boundary.
#[cfg(unix)]
pub fn qualify_attached_candidate(
    data_dir: &Path,
    engine: &Engine,
    repository: &str,
    attachment: &str,
    generation: u64,
    origins: &LiveImportedOrigins,
) -> Result<CandidateSubject> {
    let compared =
        compare_attached_candidate(data_dir, engine, repository, attachment, generation)?;
    qualify_attached_candidate_from_comparison(data_dir, repository, &compared, Some(origins))
}

/// Legacy trusted-local CLI boundary. Imported receipts remain fail-closed because no live
/// origin proof is supplied.
#[cfg(unix)]
pub fn qualify_attached_candidate_native(
    data_dir: &Path,
    engine: &Engine,
    repository: &str,
    attachment: &str,
    generation: u64,
) -> Result<CandidateSubject> {
    let compared =
        compare_attached_candidate(data_dir, engine, repository, attachment, generation)?;
    qualify_attached_candidate_from_comparison(data_dir, repository, &compared, None)
}

#[cfg(unix)]
#[derive(Clone)]
pub struct AttachedCandidateComparison {
    state: AttachmentState,
    comparison_fingerprint: String,
}

#[cfg(unix)]
pub fn compare_attached_candidate(
    data_dir: &Path,
    engine: &Engine,
    repository: &str,
    attachment: &str,
    generation: u64,
) -> Result<AttachedCandidateComparison> {
    let journal = AttachmentJournal::open_existing(data_dir, repository)?;
    let state = journal.state(attachment)?;
    if state.generation != generation {
        return Err(Error::Conflict("attachment generation changed".into()));
    }
    let comparison_fingerprint = journal.validate_current(engine, attachment, generation)?;
    Ok(AttachedCandidateComparison {
        state,
        comparison_fingerprint,
    })
}

#[cfg(unix)]
pub fn qualify_attached_candidate_from_comparison(
    data_dir: &Path,
    repository: &str,
    compared: &AttachedCandidateComparison,
    origins: Option<&LiveImportedOrigins>,
) -> Result<CandidateSubject> {
    let journal = AttachmentJournal::open_existing(data_dir, repository)?;
    let state = journal.state(&compared.state.id)?;
    if state != compared.state {
        return Err(Error::Conflict(
            "attachment changed during qualification".into(),
        ));
    }
    let authority = Authority::open_read_only(data_dir)?;
    let change = state
        .change_id
        .clone()
        .ok_or_else(|| Error::Conflict("bind an actual Authority Change first".into()))?;
    let retained = state
        .context
        .ok_or_else(|| Error::Conflict("bind current context first".into()))?;
    let qualify = || match origins {
        Some(origins) => authority.qualify_draft_candidate_with_imported_origins(&change, origins),
        None => authority.qualify_draft_candidate(&change),
    };
    let context = qualify()?;
    if context.digest()? != retained.context_digest
        || context.change_version != retained.version
        || context.revision != state.candidate_revision
    {
        return Err(Error::Conflict(
            "Authority context no longer matches attachment".into(),
        ));
    }
    if qualify()? != context {
        return Err(Error::Conflict(
            "Authority changed during source comparison".into(),
        ));
    }
    Ok(CandidateSubject {
        id: String::new(),
        attachment: state.id,
        endpoint: state.remote_url,
        output: state.output,
        change,
        change_version: context.change_version,
        context_digest: context.digest()?,
        policy_epoch: context.policy_epoch,
        revision: context.revision,
        target: context.target,
        baseline_commit: state.target_commit,
        parent_commit: String::new(),
        source_commit: state.head_commit,
        source_fingerprint: state.source_fingerprint,
        comparison_fingerprint: compared.comparison_fingerprint.clone(),
    })
}
