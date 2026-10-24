//! Authenticated, repository-scoped reads of operator-mapped native skill workflows.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::validate_repository;
use super::{HubError, Result};
use crate::Error;
use crate::authority::Authority;
use crate::skill_workflow::runtime::RunSummary;
use crate::skill_workflow_service::{self, SkillWorkflowProjection};
use crate::store::Engine;

pub const BINDINGS_ENV: &str = "AGENTSCM_WORKFLOW_BINDINGS_FILE";
const CONFIG_VERSION: u32 = 1;
const ENVELOPE_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_BINDINGS: usize = 32;
const MAX_DATA_DIR_BYTES: usize = 4096;
const MAX_RECENT_RUNS: usize = 16;

#[derive(Clone)]
struct Binding {
    native_repository: String,
    data_dir: PathBuf,
    engine: Engine,
}

/// Native workflow mappings selected by the hub operator at startup.
#[derive(Clone, Default)]
pub struct SkillWorkflowRegistry {
    bindings: BTreeMap<String, Binding>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    bindings: Vec<BindingConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingConfig {
    hub_repository: String,
    native_repository: String,
    data_dir: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Unavailable,
}

/// The common HTTP and MCP response shape.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillWorkflowEnvelope {
    pub version: u32,
    pub availability: Availability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection: Option<SkillWorkflowProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// A bounded discovery result. Summaries contain no step output or timestamps.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillWorkflowListEnvelope {
    pub version: u32,
    pub availability: Availability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<Vec<RunSummary>>,
    pub next_cursor: Option<String>,
    /// Search only: the oldest run sequence examined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub searched_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug)]
pub enum ReadError {
    InvalidRunId,
    NotFound,
    Internal(String),
}

impl SkillWorkflowEnvelope {
    fn available(projection: SkillWorkflowProjection) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            availability: Availability::Available,
            projection: Some(projection),
            reason: None,
        }
    }

    fn unavailable(reason: &'static str) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            availability: Availability::Unavailable,
            projection: None,
            reason: Some(reason),
        }
    }
}

impl SkillWorkflowListEnvelope {
    fn available(page: crate::skill_workflow::runtime::RunPage) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            availability: Availability::Available,
            runs: Some(page.runs),
            next_cursor: page.next_cursor,
            searched_to: page.searched_to,
            reason: None,
        }
    }

    fn unavailable(reason: &'static str) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            availability: Availability::Unavailable,
            runs: None,
            next_cursor: None,
            searched_to: None,
            reason: Some(reason),
        }
    }
}

impl SkillWorkflowRegistry {
    pub fn unconfigured() -> Self {
        Self::default()
    }

    /// Loads the optional operator mapping. An absent variable leaves the hub unconfigured.
    pub fn from_env() -> Result<Self> {
        match std::env::var_os(BINDINGS_ENV) {
            None => Ok(Self::unconfigured()),
            Some(path) if path.is_empty() => Err(HubError::Invalid(format!(
                "{BINDINGS_ENV} must name a configuration file"
            ))),
            Some(path) => Self::from_file(Path::new(&path)),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = read_config(path)?;
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|_| HubError::Invalid("invalid workflow bindings configuration".into()))?;
        if config.version != CONFIG_VERSION {
            return Err(HubError::Invalid(
                "unsupported workflow bindings configuration version".into(),
            ));
        }
        if config.bindings.len() > MAX_BINDINGS {
            return Err(HubError::Invalid(format!(
                "workflow bindings configuration supports at most {MAX_BINDINGS} bindings"
            )));
        }

        let mut bindings = BTreeMap::new();
        for configured in config.bindings {
            validate_repository(&configured.hub_repository)?;
            validate_native_repository(&configured.native_repository)?;
            if configured.data_dir.is_empty() || configured.data_dir.len() > MAX_DATA_DIR_BYTES {
                return Err(HubError::Invalid(
                    "workflow binding data directory is invalid".into(),
                ));
            }
            if bindings.contains_key(&configured.hub_repository) {
                return Err(HubError::Invalid(
                    "workflow bindings contain a duplicate hub repository".into(),
                ));
            }
            let data_dir = existing_native_root(Path::new(&configured.data_dir))?;
            let authority =
                Authority::open_read_only(&data_dir).map_err(|_| invalid_native_store())?;
            let current = authority
                .repository_id()
                .map_err(|_| invalid_native_store())?;
            if current != configured.native_repository {
                return Err(HubError::Invalid(
                    "workflow binding native repository does not match its store".into(),
                ));
            }
            // All paths Engine::open could create were required above. It only opens the
            // already provisioned store here; requests reuse this handle and never initialize one.
            let engine = Engine::open(&data_dir).map_err(|_| invalid_native_store())?;
            drop(authority);
            bindings.insert(
                configured.hub_repository,
                Binding {
                    native_repository: configured.native_repository,
                    data_dir,
                    engine,
                },
            );
        }
        Ok(Self { bindings })
    }

    /// Reads only the binding selected by the authenticated hub repository.
    pub fn read(
        &self,
        hub_repository: &str,
        run_id: &str,
    ) -> std::result::Result<SkillWorkflowEnvelope, ReadError> {
        skill_workflow_service::validate_run_id(run_id).map_err(|_| ReadError::InvalidRunId)?;
        let (binding, _authority) = match self.current_binding(hub_repository) {
            Ok(current) => current,
            Err(reason) => return Ok(SkillWorkflowEnvelope::unavailable(reason)),
        };

        let projection = skill_workflow_service::read(&binding.data_dir, &binding.engine, run_id)
            .map_err(read_error)?;
        if projection.run.source.repository != binding.native_repository {
            return Ok(SkillWorkflowEnvelope::unavailable(
                "native_workflow_binding_replaced",
            ));
        }
        // Reopen by configured path after projection so an observed database replacement cannot
        // turn into an available response. The local operator and filesystem remain trusted.
        if let Err(reason) = binding.current() {
            return Ok(SkillWorkflowEnvelope::unavailable(reason));
        }
        Ok(SkillWorkflowEnvelope::available(projection))
    }

    /// Lists recent runs only from the authenticated repository's operator binding.
    pub fn list(
        &self,
        hub_repository: &str,
    ) -> std::result::Result<SkillWorkflowListEnvelope, ReadError> {
        self.page(hub_repository, None, None)
    }

    pub fn page(
        &self,
        hub_repository: &str,
        before: Option<i64>,
        search: Option<&str>,
    ) -> std::result::Result<SkillWorkflowListEnvelope, ReadError> {
        let (binding, _authority) = match self.current_binding(hub_repository) {
            Ok(current) => current,
            Err(reason) => return Ok(SkillWorkflowListEnvelope::unavailable(reason)),
        };
        let page =
            skill_workflow_service::list_page(&binding.data_dir, MAX_RECENT_RUNS, before, search)
                .map_err(read_error)?;
        for run in &page.runs {
            let definition =
                crate::skill_workflow::runtime::retained_definition(&binding.data_dir, &run.run_id)
                    .map_err(read_error)?;
            if definition.source.repository != binding.native_repository {
                return Ok(SkillWorkflowListEnvelope::unavailable(
                    "native_workflow_binding_replaced",
                ));
            }
        }
        if let Err(reason) = binding.current() {
            return Ok(SkillWorkflowListEnvelope::unavailable(reason));
        }
        Ok(SkillWorkflowListEnvelope::available(page))
    }

    fn current_binding(
        &self,
        hub_repository: &str,
    ) -> std::result::Result<(&Binding, Authority), &'static str> {
        let binding = self
            .bindings
            .get(hub_repository)
            .ok_or("native_workflow_not_configured")?;
        let authority = binding.open_current()?;
        Ok((binding, authority))
    }
}

impl Binding {
    fn open_current(&self) -> std::result::Result<Authority, &'static str> {
        let authority = Authority::open_read_only(&self.data_dir)
            .map_err(|_| "native_workflow_binding_unavailable")?;
        match authority.repository_id() {
            Ok(repository) if repository == self.native_repository => Ok(authority),
            Ok(_) => Err("native_workflow_binding_replaced"),
            Err(_) => Err("native_workflow_binding_unavailable"),
        }
    }

    fn current(&self) -> std::result::Result<(), &'static str> {
        self.open_current().map(drop)
    }
}

fn read_error(error: Error) -> ReadError {
    match error {
        Error::NotFound(_) => ReadError::NotFound,
        Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound => ReadError::NotFound,
        error => ReadError::Internal(error.to_string()),
    }
}

fn read_config(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| HubError::Invalid("workflow bindings file is unavailable".into()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_CONFIG_BYTES
    {
        return Err(HubError::Invalid(
            "workflow bindings file must be a regular file no larger than 64 KiB".into(),
        ));
    }
    let mut file = File::open(path)
        .map_err(|_| HubError::Invalid("workflow bindings file is unavailable".into()))?;
    let opened = file
        .metadata()
        .map_err(|_| HubError::Invalid("workflow bindings file is unavailable".into()))?;
    if !opened.is_file() || opened.len() > MAX_CONFIG_BYTES {
        return Err(HubError::Invalid(
            "workflow bindings file must be a regular file no larger than 64 KiB".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HubError::Invalid("workflow bindings file could not be read".into()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(HubError::Invalid(
            "workflow bindings file must be no larger than 64 KiB".into(),
        ));
    }
    Ok(bytes)
}

fn existing_native_root(path: &Path) -> Result<PathBuf> {
    require_directory(path)?;
    let root = fs::canonicalize(path).map_err(|_| invalid_native_store())?;
    for file in [
        root.join(".agentscm-local-access.lock"),
        root.join("state.sqlite"),
    ] {
        require_regular(&file)?;
    }
    require_directory(&root.join("objects"))?;
    Ok(root)
}

fn require_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| invalid_native_store())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid_native_store());
    }
    Ok(())
}

fn require_regular(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| invalid_native_store())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid_native_store());
    }
    Ok(())
}

fn validate_native_repository(repository: &str) -> Result<()> {
    if repository.len() != 32
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(HubError::Invalid(
            "workflow binding native repository must be 32 lowercase hex characters".into(),
        ));
    }
    Ok(())
}

fn invalid_native_store() -> HubError {
    HubError::Invalid("workflow binding requires an existing native store".into())
}
