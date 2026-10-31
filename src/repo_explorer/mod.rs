//! Read-only projections of an operator-selected Git checkout and private capture.
//!
//! Source identity, recorded actions and visible conversation remain separate
//! facts. This module does not fetch, check out, execute or publish anything.

pub mod evidence;
pub mod git;
pub mod walkthrough;

/// The local service pins both capabilities at startup. Requests never choose
/// a repository or spool directory.
pub struct Explorer {
    pub git: git::GitExplorer,
    pub evidence: Option<evidence::EvidenceExplorer>,
}
