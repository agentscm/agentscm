use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::context::ChangeId;
use crate::forge::{Forge, Host};
use crate::{Error, Result};

pub const ISSUE_INTAKE_VERSION: u32 = 1;
pub const MAX_INTAKE_BYTES: usize = 128 * 1024;
pub const MAX_CANDIDATES: usize = 100;
const MAX_PATH_BYTES: usize = 512;
const MAX_LABEL_BYTES: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub forge: Forge,
    pub host: Host,
    pub remote_id: u64,
    /// Display metadata only. It is excluded from the stable key.
    pub path: String,
}

impl RepositoryIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.remote_id == 0
            || !matches!(
                (self.forge, self.host),
                (Forge::GitHub, Host::GitHubCom) | (Forge::GitLab, Host::GitLabCom)
            )
        {
            return Err(Error::Invalid("invalid issue repository identity".into()));
        }
        validate_path(&self.path)
    }

    pub fn stable_key(&self) -> String {
        format!(
            "{}:{}:{}",
            forge_name(self.forge),
            host_name(self.host),
            self.remote_id
        )
    }

    pub fn same_repository(&self, other: &Self) -> bool {
        self.forge == other.forge && self.host == other.host && self.remote_id == other.remote_id
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueIdentity {
    pub repository: RepositoryIdentity,
    pub remote_id: u64,
    /// Provider display metadata only. It is excluded from the stable key.
    pub number: u64,
}

impl IssueIdentity {
    pub fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        if self.remote_id == 0 || self.number == 0 {
            return Err(Error::Invalid("issue IDs must be nonzero".into()));
        }
        Ok(())
    }

    pub fn stable_key(&self) -> String {
        format!("{}:issue:{}", self.repository.stable_key(), self.remote_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProposalIdentity {
    pub repository: RepositoryIdentity,
    pub remote_id: u64,
    pub number: u64,
}

impl ProposalIdentity {
    pub fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        if self.remote_id == 0 || self.number == 0 {
            return Err(Error::Invalid("proposal IDs must be nonzero".into()));
        }
        Ok(())
    }

    pub fn stable_key(&self) -> String {
        format!(
            "{}:proposal:{}",
            self.repository.stable_key(),
            self.remote_id
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchBasis {
    ExactRequest,
    Similarity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueCandidate {
    pub identity: IssueIdentity,
    pub state: IssueState,
    pub basis: MatchBasis,
    /// Exact operator-accepted request digest for `exact_request`; a bounded
    /// opaque similarity digest for `similarity`.
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_proposal: Option<ProposalIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProposalCandidate {
    pub identity: ProposalIdentity,
    pub basis: MatchBasis,
    pub request_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeWorkCandidate {
    /// Canonical remote issue already associated with this native Change.
    pub issue: IssueIdentity,
    pub change: ChangeId,
    pub request_digest: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    MorePages,
    Cancelled,
    AccessUnavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Coverage<T> {
    Complete {
        candidates: Vec<T>,
    },
    Incomplete {
        reason: IncompleteReason,
        candidates: Vec<T>,
    },
}

impl<T> Coverage<T> {
    pub fn candidates(&self) -> &[T] {
        match self {
            Self::Complete { candidates } | Self::Incomplete { candidates, .. } => candidates,
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntakeRequest {
    pub version: u32,
    pub repository: RepositoryIdentity,
    pub accepted_request_sha256: String,
    pub native_repository: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntakeObservation {
    pub version: u32,
    pub repository: RepositoryIdentity,
    pub open_issues: Coverage<IssueCandidate>,
    pub closed_issues: Coverage<IssueCandidate>,
    pub proposals: Coverage<ProposalCandidate>,
    pub native_work: Coverage<NativeWorkCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "choice", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorChoice {
    Select { candidate: CandidateReference },
    SeparateIntent {},
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateReference {
    Issue {
        identity: IssueIdentity,
        state: IssueState,
    },
    Proposal {
        identity: ProposalIdentity,
    },
    NativeWork {
        issue: IssueIdentity,
        change: ChangeId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntakeDecision {
    ReuseIssue {
        identity: IssueIdentity,
    },
    ClosedIssue {
        identity: IssueIdentity,
    },
    ExistingProposal {
        identity: ProposalIdentity,
    },
    ExistingWork {
        change: ChangeId,
    },
    ChoiceRequired {
        candidates: Vec<CandidateReference>,
    },
    SearchIncomplete {},
    /// Descriptive only. It does not grant permission to create remote state.
    CreateEligible {},
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntakeTrace {
    pub repository_key: String,
    pub coverage_complete: bool,
    pub candidate_keys: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntakeOutcome {
    pub decision: IntakeDecision,
    /// The full supplied coverage is retained so incompleteness cannot be lost.
    pub coverage: IntakeObservation,
    pub trace: IntakeTrace,
}

impl IntakeRequest {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_INTAKE_BYTES {
            return Err(Error::Limit("issue intake input exceeds byte limit".into()));
        }
        let value: Self = serde_json::from_slice(input)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != ISSUE_INTAKE_VERSION {
            return Err(Error::Invalid("unsupported issue intake version".into()));
        }
        self.repository.validate()?;
        validate_digest(&self.accepted_request_sha256)?;
        validate_native_repository(&self.native_repository)
    }
}

impl IntakeObservation {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_INTAKE_BYTES {
            return Err(Error::Limit(
                "issue observation input exceeds byte limit".into(),
            ));
        }
        let value: Self = serde_json::from_slice(input)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != ISSUE_INTAKE_VERSION {
            return Err(Error::Invalid(
                "unsupported issue observation version".into(),
            ));
        }
        self.repository.validate()?;
        let total = self.open_issues.candidates().len()
            + self.closed_issues.candidates().len()
            + self.proposals.candidates().len()
            + self.native_work.candidates().len();
        if total > MAX_CANDIDATES {
            return Err(Error::Limit(
                "issue intake candidate count exceeds limit".into(),
            ));
        }
        for candidate in self.open_issues.candidates() {
            validate_issue(candidate, IssueState::Open, &self.repository)?;
        }
        for candidate in self.closed_issues.candidates() {
            validate_issue(candidate, IssueState::Closed, &self.repository)?;
        }
        for candidate in self.proposals.candidates() {
            candidate.identity.validate()?;
            require_repository(&candidate.identity.repository, &self.repository)?;
            validate_digest(&candidate.request_digest)?;
        }
        for candidate in self.native_work.candidates() {
            candidate.issue.validate()?;
            require_repository(&candidate.issue.repository, &self.repository)?;
            candidate.change.validate()?;
            validate_digest(&candidate.request_digest)?;
        }
        Ok(())
    }

    pub(crate) fn complete(&self) -> bool {
        self.open_issues.is_complete()
            && self.closed_issues.is_complete()
            && self.proposals.is_complete()
            && self.native_work.is_complete()
    }
}

fn validate_issue(
    candidate: &IssueCandidate,
    expected: IssueState,
    repository: &RepositoryIdentity,
) -> Result<()> {
    candidate.identity.validate()?;
    require_repository(&candidate.identity.repository, repository)?;
    if candidate.state != expected {
        return Err(Error::Conflict(
            "issue state contradicts coverage class".into(),
        ));
    }
    validate_digest(&candidate.request_digest)?;
    if let Some(proposal) = &candidate.related_proposal {
        proposal.validate()?;
        require_repository(&proposal.repository, repository)?;
    }
    Ok(())
}

pub(crate) fn require_repository(a: &RepositoryIdentity, b: &RepositoryIdentity) -> Result<()> {
    if !a.same_repository(b) {
        return Err(Error::Conflict(
            "issue observation repository mismatch".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_native_repository(value: &str) -> Result<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(Error::Invalid("invalid native repository identity".into()));
    }
    Ok(())
}

pub(crate) fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(Error::Invalid("invalid lowercase SHA-256 digest".into()));
    }
    Ok(())
}

pub fn accepted_request_digest(repository: &RepositoryIdentity, request: &[u8]) -> Result<String> {
    repository.validate()?;
    if request.is_empty() || request.len() > 8 * 1024 {
        return Err(Error::Limit(
            "accepted issue request exceeds byte limit".into(),
        ));
    }
    let mut hash = Sha256::new();
    hash.update(b"agentscm.issue-request.v1\0");
    hash.update(repository.stable_key().as_bytes());
    hash.update([0]);
    hash.update(request);
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.split('/').count() < 2
        || value.split('/').any(|part| {
            part.is_empty()
                || part.len() > MAX_LABEL_BYTES
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err(Error::Invalid("invalid issue repository path".into()));
    }
    Ok(())
}

fn forge_name(value: Forge) -> &'static str {
    match value {
        Forge::GitHub => "github",
        Forge::GitLab => "gitlab",
    }
}

fn host_name(value: Host) -> &'static str {
    match value {
        Host::GitHubCom => "github.com",
        Host::GitLabCom => "gitlab.com",
    }
}
