//! Controller-side isolated command collection. This module does not grant admission.
mod attempts;
mod backend;
mod candidates;
mod contracts;
pub(crate) mod process;
mod snapshot;

pub use attempts::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, CleanupCustody, CommandAttempt, CommandCheckRequest,
    CommandState,
};
pub(crate) use attempts::{
    create_command_attempt, read_command_attempt, read_command_record,
    require_current_command_success, run_command_attempt, stop_command_attempt,
};
pub use backend::{Cancellation, OciBackend, RuntimeProbe};
pub(crate) use candidates::command_candidate;
pub use candidates::{CandidateChange, CandidateOutput, CommandCandidate};
pub use contracts::*;
pub use snapshot::Snapshot;
