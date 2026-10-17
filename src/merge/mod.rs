//! Durable trusted-local merge sessions. Retention and protected admission are separate.

mod ancestry;
mod journal;
mod plan;
mod text;

use serde::{Deserialize, Serialize};

pub(crate) use journal::validate_recovery;
pub use journal::{MergeCompletion, MergeJournal};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase {
    Planning,
    AwaitingBase,
    Conflicts,
    Ready,
    Publishing,
    Completed,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeState {
    pub session: String,
    pub generation: u64,
    pub phase: MergePhase,
    pub left: String,
    pub right: String,
    pub base: Option<String>,
    pub base_count: usize,
    pub unresolved: usize,
    #[serde(default)]
    pub pending_operation: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    Content,
    Binary,
    TextProfile,
    EditDelete,
    AddAdd,
    Rename,
    FileDirectory,
    Portable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConflictPath {
    pub path: String,
    pub base: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeConflict {
    pub id: String,
    pub kind: ConflictKind,
    pub paths: Vec<ConflictPath>,
    pub resolved: bool,
    /// Exact current selected blob references, distinct from the original sides.
    pub resolution: Option<Vec<ResolvedPath>>,
    /// An inspection preview. It is never an automatic source resolution.
    pub preview: Option<Vec<u8>>,
    pub preview_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPath {
    /// Index in this conflict's immutable, ordered `paths` list.
    pub path_index: usize,
    pub blob: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "choice",
    content = "content",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Resolution {
    Left,
    Right,
    Base,
    Delete,
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "choice",
    content = "revision",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BaseChoice {
    Revision(String),
    /// Explicit file-set baseline for unrelated histories; no ancestor is invented.
    Empty,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeRejectionReason {
    StructuralConflict,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum MergeOutcome {
    Prepared {
        session: String,
        generation: u64,
        phase: MergePhase,
    },
    Resolved {
        session: String,
        generation: u64,
        phase: MergePhase,
    },
    Rejected {
        session: String,
        generation: u64,
        reason: MergeRejectionReason,
    },
    Completed {
        session: String,
        generation: u64,
        revision: String,
    },
    Aborted {
        session: String,
        generation: u64,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BasePage {
    pub entries: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ConflictPage {
    pub entries: Vec<MergeConflict>,
    pub next: Option<String>,
}
