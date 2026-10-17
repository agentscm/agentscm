//! Provider-neutral connection observations for a trusted connector service.
//! This module contains no token bytes, network client, sync engine or authority grant.
use std::cell::RefCell;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Forge {
    GitHub,
    GitLab,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    GitHubCom,
    GitLabCom,
}

impl Host {
    pub fn api_origin(self) -> &'static str {
        match self {
            Self::GitHubCom => "https://api.github.com",
            Self::GitLabCom => "https://gitlab.com",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub connection: String,
    pub tenant: String,
    pub operator: String,
    pub forge: Forge,
    pub host: Host,
    pub repository_id: u64,
    pub repository_path: String,
    pub accepted_branch: String,
    pub native_repository: String,
    pub native_target: String,
    pub outbound_branch: String,
}

/// Descriptive scope before remote repository selection. Not an authorization
/// capability; the service must retain its current scoped setup-read permit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupBinding {
    pub connection: String,
    pub tenant: String,
    pub operator: String,
    pub forge: Forge,
    pub host: Host,
    pub native_repository: String,
    pub native_target: String,
}

impl SetupBinding {
    pub fn matches_binding(&self, binding: &Binding) -> bool {
        self.connection == binding.connection
            && self.tenant == binding.tenant
            && self.operator == binding.operator
            && self.forge == binding.forge
            && self.host == binding.host
            && self.native_repository == binding.native_repository
            && self.native_target == binding.native_target
    }
}

pub fn validate_setup_binding(binding: &SetupBinding) -> Result<()> {
    for id in [
        &binding.connection,
        &binding.tenant,
        &binding.operator,
        &binding.native_target,
    ] {
        identifier(id)?;
    }
    if binding.native_repository.len() != 32
        || !binding
            .native_repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || !matches!(
            (binding.forge, binding.host),
            (Forge::GitHub, Host::GitHubCom) | (Forge::GitLab, Host::GitLabCom)
        )
    {
        return Err(Error::Invalid("invalid forge setup binding".into()));
    }
    Ok(())
}

pub const REPOSITORIES_PER_PAGE: usize = 20;
pub const MAX_REPOSITORY_PAGE: u32 = 1000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryQuery {
    pub repository_path: String,
    pub accepted_branch: String,
}

/// Metadata only: listing is not eligibility, contents-read or enable proof.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositorySummary {
    pub id: u64,
    pub path: String,
    pub default_branch: Option<String>,
    pub archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryListResult {
    Listed {
        repositories: Vec<RepositorySummary>,
        next_page: Option<u32>,
        /// A full last allowed page is incomplete, never a complete enumeration.
        limit_reached: bool,
        permissions: PermissionPreview,
    },
    Unavailable {
        failure: Failure,
    },
}

/// Trusted service composition must retain current authorization and credential
/// leases throughout dispatch and selection CAS. These serializable results are
/// descriptive and cannot mint a mapping/enable permit. No automatic pagination.
pub trait RepositorySetup {
    fn list_repositories(
        &self,
        transport: &dyn ReadTransport,
        scope: &SetupBinding,
        page: u32,
    ) -> Result<RepositoryListResult>;
    fn resolve_repository(
        &self,
        transport: &dyn ReadTransport,
        scope: &SetupBinding,
        query: &RepositoryQuery,
    ) -> Result<ObservationResult>;
}

pub fn validate_repository_query(query: &RepositoryQuery) -> Result<()> {
    repository_path(&query.repository_path)?;
    branch(&query.accepted_branch)
}

pub fn validate_repository_page(page: u32) -> Result<()> {
    if page == 0 || page > MAX_REPOSITORY_PAGE {
        return Err(Error::Invalid(
            "repository discovery page exceeds bound".into(),
        ));
    }
    Ok(())
}

pub fn repository_page_end(page: u32, count: usize) -> Result<(Option<u32>, bool)> {
    validate_repository_page(page)?;
    if count > REPOSITORIES_PER_PAGE {
        return Err(Error::Limit(
            "repository discovery page exceeds bound".into(),
        ));
    }
    let full = count == REPOSITORIES_PER_PAGE;
    Ok((
        if full && page < MAX_REPOSITORY_PAGE {
            Some(page + 1)
        } else {
            None
        },
        full && page == MAX_REPOSITORY_PAGE,
    ))
}

/// A server-side credential-store key, not a token, private key or capability.
/// The service must independently authorize the scoped caller before resolution.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ServerSecretRef(String);

impl std::fmt::Debug for ServerSecretRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ServerSecretRef([server-held])")
    }
}

impl ServerSecretRef {
    pub fn new(server_key: String) -> Result<Self> {
        identifier(&server_key)?;
        Ok(Self(server_key))
    }
    pub fn key(&self) -> &str {
        &self.0
    }
    pub fn validate(&self) -> Result<()> {
        identifier(&self.0)
    }
}

/// GET only. The production service must validate TLS, refuse cross-host
/// redirects, apply credentials server-side and bound time and streaming bytes.
#[derive(Clone, Debug)]
pub struct ReadRequest {
    pub host: Host,
    pub path: String,
    pub credential: ServerSecretRef,
}

pub struct ReadResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Immutable server-store generation of the exact credential resolved BEFORE
    /// this HTTP request. Never a provider header or a post-response store read.
    pub credential_generation: u64,
}

pub trait ReadTransport {
    fn get(&self, request: &ReadRequest) -> Result<ReadResponse>;
}

/// One observation only. Every replacement/refresh must change the generation.
/// This consistency check is not an authorization grant or a credential resolver.
pub struct PinnedReads<'a> {
    transport: &'a dyn ReadTransport,
    generations: RefCell<BTreeMap<String, u64>>,
}

impl<'a> PinnedReads<'a> {
    pub fn new(transport: &'a dyn ReadTransport) -> Self {
        Self {
            transport,
            generations: RefCell::new(BTreeMap::new()),
        }
    }
}

impl ReadTransport for PinnedReads<'_> {
    fn get(&self, request: &ReadRequest) -> Result<ReadResponse> {
        let response = self.transport.get(request)?;
        if response.credential_generation == 0 {
            return Err(Error::SourceIndeterminate);
        }
        let mut generations = self.generations.borrow_mut();
        if let Some(previous) = generations.get(request.credential.key()) {
            if *previous != response.credential_generation {
                return Err(Error::SourceIndeterminate);
            }
        } else {
            generations.insert(
                request.credential.key().into(),
                response.credential_generation,
            );
        }
        Ok(response)
    }
}

pub trait Provider {
    fn observe(
        &self,
        transport: &dyn ReadTransport,
        binding: &Binding,
    ) -> Result<ObservationResult>;
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Selected,
    All,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermissionPreview {
    pub repository_read: bool,
    pub branch_write: bool,
    pub proposal_write: bool,
    pub repository_selection: Selection,
    pub scopes: Vec<String>,
    pub disclosures: Vec<String>,
}

/// A point-in-time provider read. Persistence does not make this current evidence
/// or prove native convergence, remote publication, policy admission or a merge.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteObservation {
    pub repository_id: u64,
    pub repository_path: String,
    pub accepted_branch: String,
    pub accepted_commit: String,
    pub protected: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Unauthorized,
    Forbidden,
    NotFound,
    BranchMissing,
    RateLimited,
    TransportUnknown,
    Malformed,
    MismatchedIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub kind: FailureKind,
    pub retryable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationResult {
    Observed {
        observation: RemoteObservation,
        permissions: PermissionPreview,
    },
    Unavailable {
        failure: Failure,
    },
}

pub fn validate_binding(binding: &Binding) -> Result<()> {
    for id in [
        &binding.connection,
        &binding.tenant,
        &binding.operator,
        &binding.native_target,
    ] {
        identifier(id)?;
    }
    if binding.native_repository.len() != 32
        || !binding
            .native_repository
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || binding.repository_id == 0
        || !matches!(
            (binding.forge, binding.host),
            (Forge::GitHub, Host::GitHubCom) | (Forge::GitLab, Host::GitLabCom)
        )
    {
        return Err(Error::Invalid("invalid forge/repository binding".into()));
    }
    repository_path(&binding.repository_path)?;
    branch(&binding.accepted_branch)?;
    branch(&binding.outbound_branch)?;
    if binding.accepted_branch == binding.outbound_branch
        || !binding.outbound_branch.starts_with("agentscm/")
    {
        return Err(Error::Invalid(
            "outbound branch must be a distinct agentscm/ branch".into(),
        ));
    }
    Ok(())
}

pub fn repository_path(value: &str) -> Result<()> {
    if value.len() > 512
        || value.split('/').count() < 2
        || value.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err(Error::Invalid("invalid forge repository path".into()));
    }
    Ok(())
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
    {
        return Err(Error::Invalid("invalid scoped forge identifier".into()));
    }
    Ok(())
}

fn branch(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || value.contains("..")
        || value.contains("@{")
        || value.ends_with('.')
        || value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(Error::Invalid("unsupported forge branch name".into()));
    }
    Ok(())
}

pub fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn unavailable(kind: FailureKind) -> ObservationResult {
    // retryable is a scheduling hint only. It never authorizes dispatch or
    // replay of a remote write whose outcome is unknown.
    ObservationResult::Unavailable {
        failure: Failure {
            kind,
            retryable: matches!(
                kind,
                FailureKind::RateLimited | FailureKind::TransportUnknown
            ),
        },
    }
}

/// Response bytes/errors are never copied into status or evidence. There are no
/// retries here: the connection owner supplies bounded polling and durable state.
pub fn read_json(
    transport: &dyn ReadTransport,
    request: &ReadRequest,
    branch_request: bool,
) -> std::result::Result<Value, FailureKind> {
    request
        .credential
        .validate()
        .map_err(|_| FailureKind::Malformed)?;
    if !request.path.starts_with('/')
        || request.path.starts_with("//")
        || request.path.len() > 2048
        || request.path.contains(['\r', '\n', '#'])
    {
        return Err(FailureKind::Malformed);
    }
    let response = transport
        .get(request)
        .map_err(|_| FailureKind::TransportUnknown)?;
    if response.body.len() > MAX_RESPONSE_BYTES {
        return Err(FailureKind::Malformed);
    }
    match response.status {
        200 => serde_json::from_slice(&response.body).map_err(|_| FailureKind::Malformed),
        401 => Err(FailureKind::Unauthorized),
        // GitHub also uses 403 for throttling. Without vetted response-header
        // metadata, do not call an ambiguous refusal revoked permission.
        403 => Err(FailureKind::TransportUnknown),
        404 if branch_request => Err(FailureKind::BranchMissing),
        404 => Err(FailureKind::NotFound),
        429 => Err(FailureKind::RateLimited),
        _ => Err(FailureKind::TransportUnknown),
    }
}

pub fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
