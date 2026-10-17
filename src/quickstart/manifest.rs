//! `~/.config/agentscm/quickstart.json`: everything quickstart wrote, so `uninstall`
//! can take back exactly that and nothing else.
use std::path::{Path, PathBuf};

use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::hub_cli::{config_dir, write_private};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Added {
    pub(crate) event: String,
    pub(crate) entry: Value,
}

/// A Claude Code settings file quickstart created or changed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct HookRecord {
    pub(crate) path: PathBuf,
    /// Quickstart created the file (it did not exist).
    pub(crate) created: bool,
    /// Copies of the file before each change, oldest first.
    pub(crate) backups: Vec<PathBuf>,
    pub(crate) added: Vec<Added>,
    /// Hash of the file as quickstart last wrote it.
    pub(crate) written_hash: String,
    /// SHA-256 of the `env.AGENTSCM_HUB_TOKEN` value quickstart put in the file (never the
    /// token itself); uninstall removes the variable only while it still has this hash.
    #[serde(default)]
    pub(crate) env_token_sha: Option<String>,
}

/// An agent token file for one repository.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TokenRecord {
    pub(crate) repo: String,
    pub(crate) file: PathBuf,
    /// Quickstart created the file (or already owned it); a file that was there before is
    /// never deleted.
    pub(crate) owned: bool,
    pub(crate) id: Option<String>,
    pub(crate) hub: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct HubJsonRecord {
    pub(crate) path: PathBuf,
    pub(crate) hash: String,
    /// The hub.json this one replaced.
    pub(crate) backup: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LocalHubRecord {
    pub(crate) dir: PathBuf,
    pub(crate) project: String,
}

/// Quickstart signed in and created `hub-session.json` (it was not there before).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    pub(crate) hub: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Manifest {
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) hooks: Vec<HookRecord>,
    pub(crate) tokens: Vec<TokenRecord>,
    pub(crate) hub_json: Option<HubJsonRecord>,
    pub(crate) session: Option<SessionRecord>,
    pub(crate) local_hub: Option<LocalHubRecord>,
    /// `~/.agentscm/bin` holds the program the hooks call.
    pub(crate) bin_dir: Option<PathBuf>,
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(config_dir()?.join("agentscm").join("quickstart.json"))
}

pub(crate) fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn file_hash(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| sha_hex(&bytes))
}

impl Manifest {
    /// `None` when quickstart has not been run (no file).
    pub(crate) fn load() -> Result<Option<Self>> {
        match std::fs::read_to_string(path()?) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|_| {
                Error::Invalid("quickstart.json is not valid; remove it by hand".into())
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn fresh() -> Self {
        Self {
            created_at: unix_now(),
            ..Self::default()
        }
    }

    pub(crate) fn save(&mut self) -> Result<()> {
        self.updated_at = unix_now();
        let text = serde_json::to_string_pretty(self)
            .map_err(|_| Error::Invalid("cannot record quickstart.json".into()))?;
        write_private(&path()?, &format!("{text}\n"))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.hooks.is_empty()
            && self.tokens.is_empty()
            && self.hub_json.is_none()
            && self.session.is_none()
            && self.local_hub.is_none()
            && self.bin_dir.is_none()
    }
}
