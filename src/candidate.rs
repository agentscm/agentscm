//! Platform-neutral candidate identity shared by export and Hub protocols.
use crate::Result;
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
