use serde::{Deserialize, Serialize};

use crate::context::{ChangeId, ChangeState, ReviewContext};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub version: u32,
    pub parents: Vec<String>,
    pub tree: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub content: Option<Vec<u8>>,
}
/// What a manifest entry's blob content means. Only the exact file manifest
/// records a kind; `Symlink` content is the link target bytes.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    Regular,
    Executable,
    Symlink,
}

impl EntryKind {
    /// Stable lowercase name, identical to the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Executable => "executable",
            Self::Symlink => "symlink",
        }
    }
}

/// Explicit-kind counterpart of [`FileChange`]. `content: None` deletes the
/// path whatever its kind; `kind` is then ignored. For a `Symlink`, `content`
/// is the target and must pass `store::validate_symlink_target`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EntryChange {
    pub path: String,
    pub content: Option<Vec<u8>>,
    pub kind: EntryKind,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiffEntry {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    /// Kind of the before side. Set only when that side exists and is not
    /// regular, so regular-only diffs serialize as before. An entry is emitted
    /// when the blob or the kind differs; absent kind with a present side
    /// means `Regular`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_kind: Option<EntryKind>,
    /// Kind of the after side; same rule as `before_kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_kind: Option<EntryKind>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResult {
    pub entries: Vec<DiffEntry>,
    pub truncated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetState {
    pub revision: String,
    pub version: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub approval_id: String,
    pub target: String,
    pub revision: String,
    pub context_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_version: Option<u64>,
    pub policy_epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrateRequest {
    pub operation_id: String,
    pub target: String,
    pub expected_target: Option<TargetState>,
    pub revision: String,
    pub context_digest: String,
    pub approval_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_version: Option<u64>,
    pub policy_epoch: u64,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    #[default]
    Integrate,
    Initialize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationEvent {
    #[serde(default)]
    pub kind: EventKind,
    pub semantics_version: u32,
    pub sequence: u64,
    pub operation_id: String,
    pub target: String,
    pub before: Option<TargetState>,
    pub after: TargetState,
    pub approval_id: String,
    pub context_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_version: Option<u64>,
    pub policy_epoch: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareLandingRequest {
    pub operation_id: String,
    pub target: String,
    pub expected_target: TargetState,
    pub source_tip: String,
    pub change_version: u64,
    pub context: ReviewContext,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum LandingPreparation {
    Prepared {
        operation_id: String,
        target: String,
        base: TargetState,
        source_tip: String,
        revision: String,
        change: ChangeState,
    },
    AlreadyCurrent {
        operation_id: String,
        target: String,
        base: TargetState,
        source_tip: String,
    },
}
