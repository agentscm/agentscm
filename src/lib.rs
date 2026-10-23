//! The library API is internal and unstable; the supported interface is the `agentscm` CLI and HTTP API.
// The native workspace is Unix-only, which leaves some of its supporting code unused on Windows.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(unix)]
pub mod attached_worktree;
pub mod authority;
pub mod autopsy;
pub mod backend;
pub mod backend_s3;
pub mod candidate;
pub mod checks;
pub mod context;
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
pub mod hub;
#[doc(hidden)]
pub mod lease;
pub mod local_recovery;
pub mod local_validation;
pub mod merge;
pub mod model;
pub mod policy;
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
pub mod issue_workflow;

pub mod skill_bundle;
pub mod skill_workflow;
pub mod skill_workflow_service;
