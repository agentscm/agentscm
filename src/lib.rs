//! The library API is internal and unstable; the supported interface is the `agentscm` CLI and HTTP API.
// The native workspace is Unix-only, which leaves some of its supporting code unused on Windows.
#![cfg_attr(not(unix), allow(dead_code))]

/// The commit this program was built from, or empty when the build knew none (see build.rs).
pub const BUILD_COMMIT: &str = env!("AGENTSCM_BUILD_COMMIT");

#[cfg(unix)]
pub mod attached_worktree;
pub mod authority;
pub mod autopsy;
pub mod backend;
pub mod backend_s3;
pub mod candidate;
#[cfg(unix)]
pub mod change_capture;
pub mod change_record;
pub mod checks;
pub mod civil;
pub mod context;
#[cfg(unix)]
pub mod continuation;
#[cfg(unix)]
pub mod continuity;
pub mod error;
pub mod evidence;
pub mod execution;
#[cfg(unix)]
pub mod git_collisions;
#[cfg(unix)]
pub mod git_export;
mod git_path_profile;
#[cfg(unix)]
pub mod git_reconcile;
#[cfg(unix)]
pub mod git_snapshot;
pub mod guided_service;
pub mod history;
pub(crate) mod hmac;
#[doc(hidden)]
pub mod http_listener;
pub mod hub;
#[doc(hidden)]
pub mod lease;
pub mod local_recovery;
pub mod local_validation;
pub mod merge;
pub mod model;
#[cfg(unix)]
pub mod operating_policy;
pub mod policy;
#[cfg(unix)]
pub mod repo_explorer;
pub mod reviewer;
pub mod store;
pub mod sync;
pub mod team;
#[cfg(unix)]
pub mod workspace;
#[cfg(unix)]
pub mod workspace_timeline_service;
pub use error::{Error, Result};

pub mod forge;
#[cfg(unix)]
pub mod intent_review_host;
pub mod issue_workflow;

pub(crate) mod skill_bundle;
pub mod skill_workflow;
pub mod skill_workflow_service;
#[cfg(unix)]
pub mod workflow_library;
