//! Controller-side isolated command collection. This module does not grant admission.
mod attempts;
#[cfg(unix)]
mod backend;
#[cfg(unix)]
mod candidates;
mod contracts;
#[cfg(unix)]
pub(crate) mod process;
#[cfg(unix)]
mod snapshot;

pub use attempts::{
    COMMAND_CAPABILITY, COMMAND_EVIDENCE, CleanupCustody, CommandAttempt, CommandCheckRequest,
    CommandState,
};
#[cfg(unix)]
pub(crate) use attempts::{
    create_command_attempt, read_command_attempt, read_command_record, run_command_attempt,
    stop_command_attempt,
};
pub(crate) use attempts::{expected_subject, require_current_command_success};
#[cfg(unix)]
pub use backend::{Cancellation, OciBackend, RuntimeProbe};
#[cfg(unix)]
pub(crate) use candidates::command_candidate;
#[cfg(unix)]
pub use candidates::{CandidateChange, CandidateOutput, CommandCandidate};
pub use contracts::*;
#[cfg(unix)]
pub use snapshot::Snapshot;

#[cfg(unix)]
pub(crate) use snapshot::{MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES};
