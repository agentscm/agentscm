//! Portable, signed check evidence.
//!
//! Verification proves integrity of bounded claims. It does not prove that an
//! execution occurred, qualify a runtime, or authorize protected admission.
mod contracts;
mod signature;
mod verification;

pub use contracts::*;
pub use signature::sign_evidence;
pub(crate) use verification::is_canonical_ed25519_encoding;
pub use verification::{ArtifactInput, SuppliedArtifact, verify_evidence};
