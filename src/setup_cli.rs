//! Explicit local setup commands over the same native operations as unattended callers.
use std::path::{Component, PathBuf};

use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};

use crate::setup::{self, FreshRequest, OwnedRequest, SetupError};

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Claim a new private installation root and return its resume identity.
    Begin {
        #[arg(value_name = "INSTALLATION_ROOT", value_parser = parse_root)]
        root: PathBuf,
        #[arg(long, value_enum, default_value = "trusted-local-native")]
        profile: Profile,
    },
    /// Resume only the exact owned installation; ambiguous partial output stays blocked.
    Resume {
        #[arg(value_name = "INSTALLATION_ROOT", value_parser = parse_root)]
        root: PathBuf,
        #[arg(long, value_parser = parse_installation_id)]
        installation_id: String,
    },
    /// Inspect this owned installation without contacting a service or logging in.
    Doctor {
        #[arg(value_name = "INSTALLATION_ROOT", value_parser = parse_root)]
        root: PathBuf,
        #[arg(long, value_parser = parse_installation_id)]
        installation_id: String,
    },
    /// Exercise a separate owned synthetic repository/workspace; never installation DATA.
    SelfCheck {
        #[arg(value_name = "INSTALLATION_ROOT", value_parser = parse_root)]
        root: PathBuf,
        #[arg(long, value_parser = parse_installation_id)]
        installation_id: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Profile {
    TrustedLocalNative,
    /// Unsupported by the native foundation; no installation is claimed.
    Team,
    /// Unsupported by the native foundation; no installation is claimed.
    LinuxService,
    /// Unsupported by the native foundation; no model is invoked.
    ModelAssisted,
}

impl From<Profile> for setup::Profile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::TrustedLocalNative => Self::TrustedLocalNative,
            Profile::Team => Self::Team,
            Profile::LinuxService => Self::LinuxService,
            Profile::ModelAssisted => Self::ModelAssisted,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Begin,
    Resume,
    Doctor,
    SelfCheck,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidPath,
    ExistingPath,
    ForeignInstallation,
    UnsupportedProfile,
    UnsupportedPlatform,
    Busy,
    Blocked,
    FilesystemFailure,
    NativeFailure,
    InvalidReceipt,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub operation: Operation,
    pub kind: FailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<setup::Blocker>,
}

impl Failure {
    fn from_native(operation: Operation, error: SetupError) -> Self {
        let kind = match &error {
            SetupError::InvalidPath => FailureKind::InvalidPath,
            SetupError::ExistingPath => FailureKind::ExistingPath,
            SetupError::ForeignInstallation => FailureKind::ForeignInstallation,
            SetupError::UnsupportedProfile(_) => FailureKind::UnsupportedProfile,
            SetupError::UnsupportedPlatform => FailureKind::UnsupportedPlatform,
            SetupError::Busy => FailureKind::Busy,
            SetupError::Blocked(_) => FailureKind::Blocked,
            SetupError::Io(_) => FailureKind::FilesystemFailure,
            SetupError::Native(_) => FailureKind::NativeFailure,
            SetupError::Receipt(_) => FailureKind::InvalidReceipt,
        };
        let blocker = match &error {
            SetupError::Blocked(reason) => Some(*reason),
            _ => None,
        };
        // Only the leaf's fixed public Display text escapes. Never serialize its error source.
        Self {
            operation,
            kind,
            message: error.to_string(),
            blocker,
        }
    }
}

pub fn run(command: SetupCommand) -> std::result::Result<Value, Failure> {
    let operation = match &command {
        SetupCommand::Begin { .. } => Operation::Begin,
        SetupCommand::Resume { .. } => Operation::Resume,
        SetupCommand::Doctor { .. } => Operation::Doctor,
        SetupCommand::SelfCheck { .. } => Operation::SelfCheck,
    };
    execute(command, operation).map_err(|error| Failure::from_native(operation, error))
}

fn execute(command: SetupCommand, operation: Operation) -> setup::Result<Value> {
    match command {
        SetupCommand::Begin { root, profile } => {
            let owned = setup::begin(&FreshRequest {
                root,
                profile: profile.into(),
            })?;
            Ok(json!({"ok":true,"operation":operation,"installation":{
                "issue_recovery_config_path":owned.root.join(setup::ISSUE_RECOVERY_CONFIG_FILE),
                "root":owned.root,"installation_id":owned.installation_id},"next_operation":"resume"}))
        }
        SetupCommand::Resume {
            root,
            installation_id,
        } => {
            let report = setup::resume(&OwnedRequest {
                root,
                installation_id,
            })?;
            Ok(json!({"ok":true,"operation":operation,"report":report}))
        }
        SetupCommand::Doctor {
            root,
            installation_id,
        } => {
            let report = setup::doctor(&OwnedRequest {
                root,
                installation_id,
            })?;
            Ok(json!({"ok":true,"operation":operation,"report":report}))
        }
        SetupCommand::SelfCheck {
            root,
            installation_id,
        } => {
            let report = setup::self_check(&OwnedRequest {
                root,
                installation_id,
            })?;
            Ok(json!({"ok":true,"operation":operation,"report":report}))
        }
    }
}

fn parse_root(value: &str) -> std::result::Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err("installation root must be an absolute path without parent traversal".into());
    }
    Ok(path)
}

fn parse_installation_id(value: &str) -> std::result::Result<String, String> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("installation ID must be exactly 32 lowercase hexadecimal characters".into());
    }
    Ok(value.into())
}
