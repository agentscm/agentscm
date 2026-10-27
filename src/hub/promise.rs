//! Durable, owner-authorized promises and their shared version reference.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub use crate::change_record::{
    ArtifactRefV1, AssuranceBasisV1, CheckResultV1, CoverageV1, SourceBindingV1,
};

use super::{HubError, Result};
use crate::team::auth::lower_hex;

const VERSION_DOMAIN: &[u8] = b"agentscm.hub.promise-version.v1\0";
const OPERATION_DOMAIN: &[u8] = b"agentscm.hub.promise-operation.v1\0";
const ADMISSIONS_DOMAIN: &[u8] = b"agentscm.hub.promise-admissions.v1\0";
const MAX_DEFINITION_BYTES: usize = 64 * 1024;
pub const MAX_PROMISE_LIFETIME_REPOSITORIES: usize = 64;
pub const MAX_PROMISE_LIFETIME_SCOPE_BYTES: usize = 512 * 1024;
pub const DEFAULT_PROMISE_PAGE: usize = 20;
pub const MAX_PROMISE_PAGE: usize = 100;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseRefV1 {
    pub workspace_id: String,
    pub promise_id: String,
    pub version: u64,
    pub digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStageV1 {
    Design,
    Code,
    Review,
    Release,
    PostRelease,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBasisV1 {
    Rehearsal,
    Observed,
    CallerReported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRequirementV1 {
    pub id: String,
    pub criterion_id: String,
    pub stage: EvidenceStageV1,
    pub basis: EvidenceBasisV1,
    pub producer: String,
    pub check_key: String,
    pub check_digest: String,
    pub scenario_digest: Option<String>,
    pub workflow_digest: Option<String>,
    pub environment_digest: Option<String>,
    pub accepted_assurance: Vec<AssuranceBasisV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseCriterionV1 {
    pub id: String,
    pub text: String,
    pub evidence_requirements: Vec<EvidenceRequirementV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseConstraintV1 {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseRepositoryV1 {
    /// Configured Workspace grant key; source identity is admitted by the server per version.
    pub repository_id: String,
    pub relevance: String,
}

/// Server-captured source scope for one configured Workspace grant key. An empty source is
/// explicitly unbound; it cannot inherit a later forge or native mapping.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseRepositoryAdmissionV1 {
    pub repository_id: String,
    pub forge: Option<PromiseForgeAdmissionV1>,
    pub native_repository_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseForgeAdmissionV1 {
    pub provider: String,
    pub host: String,
    pub forge_repository_id: i64,
}

impl PromiseRepositoryAdmissionV1 {
    pub fn unbound(repository_id: &str) -> Self {
        Self {
            repository_id: repository_id.to_owned(),
            forge: None,
            native_repository_ids: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        identifier("repository_id", &self.repository_id)?;
        if let Some(forge) = &self.forge {
            text("forge provider", &forge.provider, 64)?;
            text("forge host", &forge.host, 255)?;
            if forge.forge_repository_id <= 0 {
                return Err(invalid("forge repository id must be positive"));
            }
        }
        if self.native_repository_ids.len() > 32 {
            return Err(invalid("too many native repository identities"));
        }
        for native in &self.native_repository_ids {
            identifier("native repository id", native)?;
        }
        if self
            .native_repository_ids
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid(
                "native repository identities must be sorted and unique",
            ));
        }
        Ok(())
    }
}

pub type PromiseCurrentAdmissionsV1 = BTreeMap<String, PromiseRepositoryAdmissionV1>;

pub fn admissions_digest(
    workspace_id: &str,
    admissions: &[PromiseRepositoryAdmissionV1],
) -> Result<String> {
    identifier("workspace_id", workspace_id)?;
    validate_admission_set(admissions, 8)?;
    let encoded = serde_json::to_vec(admissions)
        .map_err(|_| unavailable("promise admissions encoding failed"))?;
    let mut hash = Sha256::new();
    hash.update(ADMISSIONS_DOMAIN);
    hash.update(workspace_id.as_bytes());
    hash.update([0]);
    hash.update(encoded);
    Ok(lower_hex(&hash.finalize()))
}

pub(crate) fn ensure_admissions_digest(
    workspace_id: &str,
    admissions: &[PromiseRepositoryAdmissionV1],
    expected: &str,
) -> Result<()> {
    if admissions_digest(workspace_id, admissions)? != expected {
        return Err(conflict(
            "promise admissions changed; review source scope again",
        ));
    }
    Ok(())
}

pub(crate) fn validate_admission_set(
    admissions: &[PromiseRepositoryAdmissionV1],
    max: usize,
) -> Result<()> {
    if admissions.is_empty() || admissions.len() > max {
        return Err(invalid("invalid promise admission scope"));
    }
    for admission in admissions {
        admission.validate()?;
    }
    if admissions
        .windows(2)
        .any(|pair| pair[0].repository_id >= pair[1].repository_id)
    {
        return Err(invalid("promise admissions must be sorted and unique"));
    }
    Ok(())
}

pub(crate) fn decode_lifetime_scope(encoded: &str) -> Option<Vec<PromiseRepositoryAdmissionV1>> {
    if encoded.len() > MAX_PROMISE_LIFETIME_SCOPE_BYTES {
        return None;
    }
    let admissions: Vec<PromiseRepositoryAdmissionV1> = serde_json::from_str(encoded).ok()?;
    validate_admission_set(&admissions, MAX_PROMISE_LIFETIME_REPOSITORIES)
        .ok()
        .map(|()| admissions)
}

pub(crate) fn decode_current_scope(encoded: &str) -> Option<Vec<PromiseRepositoryAdmissionV1>> {
    if encoded.len() > 65536 {
        return None;
    }
    let admissions: Vec<PromiseRepositoryAdmissionV1> = serde_json::from_str(encoded).ok()?;
    validate_admission_set(&admissions, 8)
        .ok()
        .map(|()| admissions)
}

pub(crate) fn extend_lifetime(
    retained: &[PromiseRepositoryAdmissionV1],
    current: &[PromiseRepositoryAdmissionV1],
) -> Result<Vec<PromiseRepositoryAdmissionV1>> {
    if !retained.is_empty() {
        validate_admission_set(retained, MAX_PROMISE_LIFETIME_REPOSITORIES)?;
    }
    validate_admission_set(current, 8)?;
    let mut by_key = retained
        .iter()
        .map(|item| (item.repository_id.clone(), item.clone()))
        .collect::<BTreeMap<_, _>>();
    for admission in current {
        match by_key.get(&admission.repository_id) {
            Some(previous) if previous != admission => {
                return Err(conflict(
                    "promise repository key changed source identity; use a new key",
                ));
            }
            Some(_) => {}
            None => {
                by_key.insert(admission.repository_id.clone(), admission.clone());
            }
        }
    }
    if by_key.len() > MAX_PROMISE_LIFETIME_REPOSITORIES {
        return Err(conflict("promise lifetime repository scope is full"));
    }
    let extended = by_key.into_values().collect::<Vec<_>>();
    let size = serde_json::to_vec(&extended)
        .map_err(|_| unavailable("promise lifetime encoding failed"))?
        .len();
    if size > MAX_PROMISE_LIFETIME_SCOPE_BYTES {
        return Err(conflict("promise lifetime repository scope is full"));
    }
    Ok(extended)
}

pub fn current_admission(
    current: &PromiseCurrentAdmissionsV1,
    repository_id: &str,
) -> PromiseRepositoryAdmissionV1 {
    current
        .get(repository_id)
        .cloned()
        .unwrap_or_else(|| PromiseRepositoryAdmissionV1::unbound(repository_id))
}

pub fn admissions_current(
    recorded: &[PromiseRepositoryAdmissionV1],
    current: &PromiseCurrentAdmissionsV1,
) -> bool {
    recorded
        .iter()
        .all(|admission| *admission == current_admission(current, &admission.repository_id))
}

/// Compare each retained admission before validating the aggregate scope. A malformed
/// admission cannot establish current source authority, while duplicate current entries
/// are left to the aggregate decoder to report as damaged storage.
pub(crate) fn encoded_admissions_authorized(
    encoded: &str,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> bool {
    if encoded.len() > 65536 {
        return false;
    }
    serde_json::from_str::<Vec<PromiseRepositoryAdmissionV1>>(encoded).is_ok_and(|admissions| {
        !admissions.is_empty()
            && admissions.len() <= 8
            && admissions
                .iter()
                .all(|admission| admission.validate().is_ok())
            && scope_authorized(&admissions, permitted, current)
    })
}

pub(crate) fn scope_authorized(
    recorded: &[PromiseRepositoryAdmissionV1],
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> bool {
    admissions_current(recorded, current)
        && permitted.is_none_or(|grants| {
            recorded
                .iter()
                .all(|admission| grants.contains(&admission.repository_id))
        })
}

pub fn validate_admissions(
    repositories: &[PromiseRepositoryV1],
    admissions: &[PromiseRepositoryAdmissionV1],
) -> Result<()> {
    if repositories.len() != admissions.len() {
        return Err(invalid("promise repository admissions do not match scope"));
    }
    let mut expected = repositories
        .iter()
        .map(|item| item.repository_id.as_str())
        .collect::<Vec<_>>();
    expected.sort_unstable();
    for (index, admission) in admissions.iter().enumerate() {
        admission.validate()?;
        if expected[index] != admission.repository_id {
            return Err(invalid("promise repository admissions do not match scope"));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseDefinitionV1 {
    /// Exact owner-confirmed request retained in every version.
    pub original_request: String,
    pub commitment: String,
    pub criteria: Vec<PromiseCriterionV1>,
    pub constraints: Vec<PromiseConstraintV1>,
    pub repositories: Vec<PromiseRepositoryV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseMutableContentV1 {
    pub commitment: String,
    pub criteria: Vec<PromiseCriterionV1>,
    pub constraints: Vec<PromiseConstraintV1>,
    pub repositories: Vec<PromiseRepositoryV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseCreateV1 {
    pub operation_id: String,
    pub expected_admissions_digest: String,
    pub definition: PromiseDefinitionV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseAdoptV1 {
    pub operation_id: String,
    pub expected_admissions_digest: String,
    pub proposal_id: String,
    pub definition: PromiseDefinitionV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromiseInputV1 {
    Proposal {
        operation_id: String,
        repository_id: String,
        original_request: String,
        commitment: String,
        criteria: Vec<PromiseCriterionV1>,
        constraints: Vec<PromiseConstraintV1>,
    },
    Report {
        operation_id: String,
        promise_id: String,
        promise_version: u64,
        promise_digest: String,
        repository_id: String,
        summary: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromiseChangeActionV1 {
    Amend { content: PromiseMutableContentV1 },
    Correct { content: PromiseMutableContentV1 },
    Retire,
    Transfer { to_user: String },
    RecoveryTransfer { to_user: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseChangeV1 {
    pub operation_id: String,
    pub expected_admissions_digest: String,
    pub expected_version: u64,
    pub expected_digest: String,
    pub reason: String,
    pub action: PromiseChangeActionV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromiseStateV1 {
    Active,
    Retired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromiseVersionKindV1 {
    Create,
    Adopt,
    Amend,
    Correct,
    Retire,
    Transfer,
    RecoveryTransfer,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseVersionV1 {
    #[serde(rename = "ref")]
    pub reference: PromiseRefV1,
    pub prior_digest: Option<String>,
    pub steward_user: String,
    pub state: PromiseStateV1,
    pub kind: PromiseVersionKindV1,
    pub reason: Option<String>,
    pub definition: PromiseDefinitionV1,
    /// Immutable server-captured source identities included in the version digest.
    pub repository_admissions: Vec<PromiseRepositoryAdmissionV1>,
    pub created_by: String,
    pub proposal_id: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseViewV1 {
    pub current: PromiseVersionV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseReceiptV1 {
    #[serde(rename = "ref")]
    pub reference: PromiseRefV1,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseInputReceiptV1 {
    pub input_id: String,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseInputViewV1 {
    pub input_id: String,
    pub repository_id: String,
    pub actor_user: String,
    pub submitted_by_agent: bool,
    pub payload: PromiseInputV1,
    pub adopted_promise: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseInputPageV1 {
    pub inputs: Vec<PromiseInputViewV1>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseAdmissionPreflightRequestV1 {
    pub repository_ids: Vec<String>,
}

impl PromiseAdmissionPreflightRequestV1 {
    pub fn validate(&self) -> Result<()> {
        if self.repository_ids.is_empty() || self.repository_ids.len() > 8 {
            return Err(invalid("invalid promise preflight scope"));
        }
        for key in &self.repository_ids {
            identifier("repository_id", key)?;
        }
        if self
            .repository_ids
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid(
                "promise preflight repositories must be sorted and unique",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseAdmissionPreflightV1 {
    pub admissions: Vec<PromiseRepositoryAdmissionV1>,
    pub digest: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseChangeCapabilitiesV1 {
    pub amend: bool,
    pub correct: bool,
    pub retire: bool,
    pub transfer: bool,
    pub recovery_transfer: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromisePageV1 {
    pub promises: Vec<PromiseViewV1>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromiseHistoryPageV1 {
    pub versions: Vec<PromiseVersionV1>,
    pub next: Option<u64>,
}

/// Trusted service decision. This is deliberately not deserializable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromiseChangeAuthority {
    Steward,
    RecoveryAdministrator,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct PromiseVersionContentV1<'a> {
    schema: &'static str,
    workspace_id: &'a str,
    promise_id: &'a str,
    version: u64,
    prior_digest: &'a Option<String>,
    steward_user: &'a str,
    state: PromiseStateV1,
    kind: PromiseVersionKindV1,
    reason: &'a Option<String>,
    definition: &'a PromiseDefinitionV1,
    repository_admissions: &'a [PromiseRepositoryAdmissionV1],
    created_by: &'a str,
    proposal_id: &'a Option<String>,
    created_at: i64,
}

impl PromiseCreateV1 {
    pub fn validate(&self) -> Result<()> {
        identifier("operation_id", &self.operation_id)?;
        digest(
            "expected_admissions_digest",
            &self.expected_admissions_digest,
        )?;
        self.definition.validate()
    }
}

impl PromiseAdoptV1 {
    pub fn validate(&self) -> Result<()> {
        identifier("operation_id", &self.operation_id)?;
        digest(
            "expected_admissions_digest",
            &self.expected_admissions_digest,
        )?;
        identifier("proposal_id", &self.proposal_id)?;
        self.definition.validate()
    }
}

impl PromiseInputV1 {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::Proposal { operation_id, .. } | Self::Report { operation_id, .. } => operation_id,
        }
    }

    pub fn repository_id(&self) -> &str {
        match self {
            Self::Proposal { repository_id, .. } | Self::Report { repository_id, .. } => {
                repository_id
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Proposal { .. } => "proposal",
            Self::Report { .. } => "report",
        }
    }

    pub fn validate(&self) -> Result<()> {
        identifier("operation_id", self.operation_id())?;
        identifier("repository_id", self.repository_id())?;
        match self {
            Self::Proposal {
                original_request,
                commitment,
                criteria,
                constraints,
                ..
            } => {
                text("original_request", original_request, 16 * 1024)?;
                let repositories = [PromiseRepositoryV1 {
                    repository_id: self.repository_id().to_owned(),
                    relevance: "proposal scope".into(),
                }];
                validate_content(commitment, criteria, constraints, &repositories)?;
                bounded_json(self)
            }
            Self::Report {
                promise_id,
                promise_version,
                promise_digest,
                summary,
                ..
            } => {
                identifier("promise_id", promise_id)?;
                if *promise_version == 0 {
                    return Err(invalid("promise version must be positive"));
                }
                digest("promise_digest", promise_digest)?;
                text("summary", summary, 8192)
            }
        }
    }
}

impl PromiseChangeV1 {
    pub fn validate(&self) -> Result<()> {
        identifier("operation_id", &self.operation_id)?;
        digest(
            "expected_admissions_digest",
            &self.expected_admissions_digest,
        )?;
        if self.expected_version == 0 {
            return Err(invalid("expected_version must be positive"));
        }
        digest("expected_digest", &self.expected_digest)?;
        text("reason", &self.reason, 2048)?;
        match &self.action {
            PromiseChangeActionV1::Amend { content }
            | PromiseChangeActionV1::Correct { content } => content.validate(),
            PromiseChangeActionV1::Transfer { to_user }
            | PromiseChangeActionV1::RecoveryTransfer { to_user } => identifier("to_user", to_user),
            PromiseChangeActionV1::Retire => Ok(()),
        }
    }
}

impl PromiseDefinitionV1 {
    pub fn validate(&self) -> Result<()> {
        text("original_request", &self.original_request, 16 * 1024)?;
        validate_content(
            &self.commitment,
            &self.criteria,
            &self.constraints,
            &self.repositories,
        )?;
        bounded_json(self)
    }
}

impl PromiseMutableContentV1 {
    pub fn validate(&self) -> Result<()> {
        validate_content(
            &self.commitment,
            &self.criteria,
            &self.constraints,
            &self.repositories,
        )?;
        bounded_json(self)
    }
}

impl PromiseVersionV1 {
    pub(crate) fn initial(
        workspace_id: &str,
        promise_id: &str,
        steward_user: &str,
        proposal_id: Option<String>,
        definition: PromiseDefinitionV1,
        repository_admissions: Vec<PromiseRepositoryAdmissionV1>,
        created_at: i64,
    ) -> Result<Self> {
        let kind = if proposal_id.is_some() {
            PromiseVersionKindV1::Adopt
        } else {
            PromiseVersionKindV1::Create
        };
        definition.validate()?;
        validate_admissions(&definition.repositories, &repository_admissions)?;
        let mut version = Self {
            reference: PromiseRefV1 {
                workspace_id: workspace_id.to_owned(),
                promise_id: promise_id.to_owned(),
                version: 1,
                digest: String::new(),
            },
            prior_digest: None,
            steward_user: steward_user.to_owned(),
            state: PromiseStateV1::Active,
            kind,
            reason: None,
            definition,
            repository_admissions,
            created_by: steward_user.to_owned(),
            proposal_id,
            created_at,
        };
        version.reference.digest = version.digest()?;
        Ok(version)
    }

    pub(crate) fn next(
        &self,
        actor: &str,
        authority: PromiseChangeAuthority,
        change: &PromiseChangeV1,
        repository_admissions: Vec<PromiseRepositoryAdmissionV1>,
        created_at: i64,
    ) -> Result<Self> {
        change.validate()?;
        if self.state == PromiseStateV1::Retired {
            return Err(conflict("retired promise is terminal"));
        }
        let (kind, state, steward_user, definition) = match &change.action {
            PromiseChangeActionV1::Amend { content } => (
                PromiseVersionKindV1::Amend,
                PromiseStateV1::Active,
                self.steward_user.clone(),
                content.with_original(&self.definition.original_request),
            ),
            PromiseChangeActionV1::Correct { content } => (
                PromiseVersionKindV1::Correct,
                PromiseStateV1::Active,
                self.steward_user.clone(),
                content.with_original(&self.definition.original_request),
            ),
            PromiseChangeActionV1::Retire => (
                PromiseVersionKindV1::Retire,
                PromiseStateV1::Retired,
                self.steward_user.clone(),
                self.definition.clone(),
            ),
            PromiseChangeActionV1::Transfer { to_user } => (
                PromiseVersionKindV1::Transfer,
                PromiseStateV1::Active,
                to_user.clone(),
                self.definition.clone(),
            ),
            PromiseChangeActionV1::RecoveryTransfer { to_user } => (
                PromiseVersionKindV1::RecoveryTransfer,
                PromiseStateV1::Active,
                to_user.clone(),
                self.definition.clone(),
            ),
        };
        let recovery = matches!(
            &change.action,
            PromiseChangeActionV1::RecoveryTransfer { .. }
        );
        if recovery != (authority == PromiseChangeAuthority::RecoveryAdministrator) {
            return Err(conflict("promise change authority does not match action"));
        }
        definition.validate()?;
        validate_admissions(&definition.repositories, &repository_admissions)?;
        ensure_admissions_digest(
            &self.reference.workspace_id,
            &repository_admissions,
            &change.expected_admissions_digest,
        )?;
        let mut version = Self {
            reference: PromiseRefV1 {
                workspace_id: self.reference.workspace_id.clone(),
                promise_id: self.reference.promise_id.clone(),
                version: self
                    .reference
                    .version
                    .checked_add(1)
                    .ok_or_else(|| unavailable("promise version exhausted"))?,
                digest: String::new(),
            },
            prior_digest: Some(self.reference.digest.clone()),
            steward_user,
            state,
            kind,
            reason: Some(change.reason.clone()),
            definition,
            repository_admissions,
            created_by: actor.to_owned(),
            proposal_id: self.proposal_id.clone(),
            created_at,
        };
        version.reference.digest = version.digest()?;
        Ok(version)
    }

    pub(crate) fn validate_stored(&self) -> Result<()> {
        identifier("workspace_id", &self.reference.workspace_id)?;
        identifier("promise_id", &self.reference.promise_id)?;
        identifier("steward_user", &self.steward_user)?;
        identifier("created_by", &self.created_by)?;
        if self.reference.version == 0 || self.created_at < 0 {
            return Err(unavailable("invalid stored promise version"));
        }
        if let Some(prior) = &self.prior_digest {
            digest("prior_digest", prior)?;
        }
        if let Some(reason) = &self.reason {
            text("reason", reason, 2048)?;
        }
        if let Some(proposal) = &self.proposal_id {
            identifier("proposal_id", proposal)?;
        }
        self.definition.validate()?;
        validate_admissions(&self.definition.repositories, &self.repository_admissions)?;
        digest("promise digest", &self.reference.digest)?;
        if self.digest()? != self.reference.digest {
            return Err(unavailable("stored promise digest mismatch"));
        }
        Ok(())
    }

    fn digest(&self) -> Result<String> {
        let content = PromiseVersionContentV1 {
            schema: "agentscm.hub.promise-version-content.v1",
            workspace_id: &self.reference.workspace_id,
            promise_id: &self.reference.promise_id,
            version: self.reference.version,
            prior_digest: &self.prior_digest,
            steward_user: &self.steward_user,
            state: self.state,
            kind: self.kind,
            reason: &self.reason,
            definition: &self.definition,
            repository_admissions: &self.repository_admissions,
            created_by: &self.created_by,
            proposal_id: &self.proposal_id,
            created_at: self.created_at,
        };
        let encoded = serde_json::to_vec(&content)
            .map_err(|_| unavailable("promise version encoding failed"))?;
        let mut hash = Sha256::new();
        hash.update(VERSION_DOMAIN);
        hash.update(encoded);
        Ok(lower_hex(&hash.finalize()))
    }
}

impl PromiseMutableContentV1 {
    fn with_original(&self, original_request: &str) -> PromiseDefinitionV1 {
        PromiseDefinitionV1 {
            original_request: original_request.to_owned(),
            commitment: self.commitment.clone(),
            criteria: self.criteria.clone(),
            constraints: self.constraints.clone(),
            repositories: self.repositories.clone(),
        }
    }
}

pub(crate) fn operation_fingerprint<T: Serialize>(
    kind: &str,
    workspace_id: &str,
    actor: &str,
    request: &T,
) -> Result<String> {
    let encoded = serde_json::to_vec(request)
        .map_err(|_| unavailable("promise operation encoding failed"))?;
    let mut hash = Sha256::new();
    hash.update(OPERATION_DOMAIN);
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(workspace_id.as_bytes());
    hash.update([0]);
    hash.update(actor.as_bytes());
    hash.update([0]);
    hash.update(encoded);
    Ok(lower_hex(&hash.finalize()))
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| unavailable("promise encoding failed"))
}

pub(crate) fn decode_version(value: &str) -> Result<PromiseVersionV1> {
    let version: PromiseVersionV1 =
        serde_json::from_str(value).map_err(|_| unavailable("invalid stored promise version"))?;
    version
        .validate_stored()
        .map_err(|_| unavailable("invalid stored promise version"))?;
    Ok(version)
}

/// Only a self-valid retained version can establish its complete source authority.
pub(crate) fn payload_admissions_authorized(
    value: &str,
    workspace_id: &str,
    promise_id: &str,
    version: i64,
    permitted: Option<&[String]>,
    current: &PromiseCurrentAdmissionsV1,
) -> bool {
    decode_version(value).is_ok_and(|decoded| {
        decoded.reference.workspace_id == workspace_id
            && decoded.reference.promise_id == promise_id
            && i64::try_from(decoded.reference.version).ok() == Some(version)
            && scope_authorized(&decoded.repository_admissions, permitted, current)
    })
}

fn validate_content(
    commitment: &str,
    criteria: &[PromiseCriterionV1],
    constraints: &[PromiseConstraintV1],
    repositories: &[PromiseRepositoryV1],
) -> Result<()> {
    text("commitment", commitment, 16 * 1024)?;
    if criteria.is_empty() || criteria.len() > 64 || constraints.len() > 64 {
        return Err(invalid("promise criteria or constraints are out of bounds"));
    }
    let mut criterion_ids = Vec::with_capacity(criteria.len());
    let mut requirement_ids = Vec::new();
    for criterion in criteria {
        identifier("criterion id", &criterion.id)?;
        text("criterion text", &criterion.text, 2048)?;
        criterion_ids.push(&criterion.id);
        for requirement in &criterion.evidence_requirements {
            requirement.validate(&criterion.id)?;
            requirement_ids.push(&requirement.id);
        }
    }
    if duplicate(&criterion_ids) || requirement_ids.len() > 64 || duplicate(&requirement_ids) {
        return Err(invalid(
            "promise criterion or evidence requirement ids must be unique",
        ));
    }
    let mut constraint_ids = Vec::with_capacity(constraints.len());
    for constraint in constraints {
        identifier("constraint id", &constraint.id)?;
        text("constraint text", &constraint.text, 2048)?;
        constraint_ids.push(&constraint.id);
    }
    if duplicate(&constraint_ids) {
        return Err(invalid("promise constraint ids must be unique"));
    }
    if repositories.is_empty() || repositories.len() > 8 {
        return Err(invalid(
            "promise repository scope must contain 1 to 8 repositories",
        ));
    }
    let mut repository_ids = Vec::with_capacity(repositories.len());
    for repository in repositories {
        identifier("repository_id", &repository.repository_id)?;
        text("repository relevance", &repository.relevance, 2048)?;
        repository_ids.push(&repository.repository_id);
    }
    if duplicate(&repository_ids) {
        return Err(invalid("promise repository ids must be unique"));
    }
    Ok(())
}

impl EvidenceRequirementV1 {
    fn validate(&self, criterion: &str) -> Result<()> {
        identifier("evidence requirement id", &self.id)?;
        if self.criterion_id != criterion {
            return Err(invalid(
                "evidence requirement criterion does not match its owner",
            ));
        }
        identifier("evidence producer", &self.producer)?;
        identifier("evidence check_key", &self.check_key)?;
        digest("evidence check_digest", &self.check_digest)?;
        for (name, value) in [
            ("scenario_digest", &self.scenario_digest),
            ("workflow_digest", &self.workflow_digest),
            ("environment_digest", &self.environment_digest),
        ] {
            if let Some(value) = value {
                digest(name, value)?;
            }
        }
        if self.accepted_assurance.is_empty()
            || self.accepted_assurance.len() > 4
            || (0..self.accepted_assurance.len()).any(|index| {
                self.accepted_assurance[index + 1..].contains(&self.accepted_assurance[index])
            })
        {
            return Err(invalid(
                "accepted assurance values must be nonempty and unique",
            ));
        }
        if self.basis == EvidenceBasisV1::Rehearsal
            && (self.scenario_digest.is_none()
                || self.workflow_digest.is_none()
                || self.environment_digest.is_none())
        {
            return Err(invalid(
                "rehearsal evidence requires scenario, workflow and environment digests",
            ));
        }
        Ok(())
    }
}

fn bounded_json(value: &impl Serialize) -> Result<()> {
    let len = serde_json::to_vec(value)
        .map_err(|_| invalid("promise content could not be encoded"))?
        .len();
    if len > MAX_DEFINITION_BYTES {
        return Err(invalid("promise content exceeds 64 KiB"));
    }
    Ok(())
}

fn duplicate<T: Eq>(values: &[T]) -> bool {
    (0..values.len()).any(|index| values[index + 1..].contains(&values[index]))
}

fn identifier(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./:-".contains(&byte))
    {
        return Err(invalid(&format!("invalid {name}")));
    }
    Ok(())
}

pub(crate) fn valid_promise_id(value: &str) -> bool {
    identifier("promise_id", value).is_ok()
}

pub(crate) fn valid_promise_input_id(value: &str) -> bool {
    (value.starts_with("prp_") || value.starts_with("prr_"))
        && identifier("input_id", value).is_ok()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the retained input row has eight independently validated columns"
)]
pub(crate) fn decode_input_view(
    input_id: String,
    kind: String,
    repository_id: String,
    actor_user: String,
    submitted_by_agent: bool,
    payload: String,
    adopted_promise: Option<String>,
    created_at: i64,
) -> Result<PromiseInputViewV1> {
    let payload: PromiseInputV1 = serde_json::from_str(&payload)
        .map_err(|_| HubError::Unavailable("invalid retained promise input".into()))?;
    payload
        .validate()
        .map_err(|_| HubError::Unavailable("invalid retained promise input".into()))?;
    if payload.kind() != kind || payload.repository_id() != repository_id {
        return Err(HubError::Unavailable("promise input scope mismatch".into()));
    }
    Ok(PromiseInputViewV1 {
        input_id,
        repository_id,
        actor_user,
        submitted_by_agent,
        payload,
        adopted_promise,
        created_at,
    })
}

fn text(name: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(invalid(&format!("invalid {name}")));
    }
    Ok(())
}

fn digest(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(&format!("invalid {name}")));
    }
    Ok(())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

fn conflict(message: &str) -> HubError {
    HubError::Invalid(format!("promise_conflict:{message}"))
}

fn unavailable(message: &str) -> HubError {
    HubError::Unavailable(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> PromiseDefinitionV1 {
        PromiseDefinitionV1 {
            original_request: "Keep existing account settings unchanged.".into(),
            commitment: "Existing accounts retain their settings.".into(),
            criteria: vec![PromiseCriterionV1 {
                id: "settings_preserved".into(),
                text: "Existing settings remain readable and unchanged.".into(),
                evidence_requirements: vec![],
            }],
            constraints: vec![],
            repositories: vec![PromiseRepositoryV1 {
                repository_id: "repo_01".into(),
                relevance: "Owns account settings.".into(),
            }],
        }
    }

    #[test]
    fn version_digest_is_stable_and_binds_authority() {
        let first = PromiseVersionV1::initial(
            "workspace",
            "prm_01",
            "usr_01",
            None,
            definition(),
            vec![PromiseRepositoryAdmissionV1::unbound("repo_01")],
            1,
        )
        .unwrap();
        let same = PromiseVersionV1::initial(
            "workspace",
            "prm_01",
            "usr_01",
            None,
            definition(),
            vec![PromiseRepositoryAdmissionV1::unbound("repo_01")],
            1,
        )
        .unwrap();
        let other = PromiseVersionV1::initial(
            "workspace",
            "prm_01",
            "usr_02",
            None,
            definition(),
            vec![PromiseRepositoryAdmissionV1::unbound("repo_01")],
            1,
        )
        .unwrap();
        assert_eq!(first.reference.digest, same.reference.digest);
        assert_ne!(first.reference.digest, other.reference.digest);
        first.validate_stored().unwrap();
    }

    #[test]
    fn duplicate_admission_property_cannot_narrow_a_retained_version() {
        let mut definition = definition();
        definition.repositories.push(PromiseRepositoryV1 {
            repository_id: "repo_02".into(),
            relevance: "Owns settings history.".into(),
        });
        let admissions = vec![
            PromiseRepositoryAdmissionV1::unbound("repo_01"),
            PromiseRepositoryAdmissionV1::unbound("repo_02"),
        ];
        let version = PromiseVersionV1::initial(
            "workspace",
            "prm_02",
            "usr_01",
            None,
            definition,
            admissions.clone(),
            1,
        )
        .unwrap();
        let encoded = encode(&version).unwrap();
        let first = format!("\"repository_admissions\":{}", encode(&admissions).unwrap());
        let duplicate = format!(
            "{first},\"repository_admissions\":{}",
            serde_json::to_string(&admissions[..1]).unwrap()
        );
        assert_eq!(encoded.matches(&first).count(), 1);
        let damaged = encoded.replacen(&first, &duplicate, 1);
        assert!(
            serde_json::from_str::<PromiseVersionV1>(&damaged)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );
        let current = PromiseCurrentAdmissionsV1::from([
            ("repo_01".into(), admissions[0].clone()),
            ("repo_02".into(), admissions[1].clone()),
        ]);
        assert!(!payload_admissions_authorized(
            &damaged,
            "workspace",
            "prm_02",
            1,
            None,
            &current,
        ));
    }

    #[test]
    fn self_digest_does_not_authorize_invalid_admission_order_or_another_row() {
        let mut definition = definition();
        definition.repositories.push(PromiseRepositoryV1 {
            repository_id: "repo_02".into(),
            relevance: "Owns settings history.".into(),
        });
        let admissions = vec![
            PromiseRepositoryAdmissionV1::unbound("repo_01"),
            PromiseRepositoryAdmissionV1::unbound("repo_02"),
        ];
        let mut version = PromiseVersionV1::initial(
            "workspace",
            "prm_02",
            "usr_01",
            None,
            definition,
            admissions.clone(),
            1,
        )
        .unwrap();
        let current = PromiseCurrentAdmissionsV1::from([
            ("repo_01".into(), admissions[0].clone()),
            ("repo_02".into(), admissions[1].clone()),
        ]);
        let valid = encode(&version).unwrap();
        assert!(payload_admissions_authorized(
            &valid,
            "workspace",
            "prm_02",
            1,
            None,
            &current,
        ));
        assert!(!payload_admissions_authorized(
            &valid,
            "workspace",
            "prm_other",
            1,
            None,
            &current,
        ));
        assert!(!payload_admissions_authorized(
            &valid,
            "other_workspace",
            "prm_02",
            1,
            None,
            &current,
        ));
        assert!(!payload_admissions_authorized(
            &valid,
            "workspace",
            "prm_02",
            2,
            None,
            &current,
        ));
        version.repository_admissions.reverse();
        version.reference.digest = version.digest().unwrap();
        let damaged = encode(&version).unwrap();
        assert!(!payload_admissions_authorized(
            &damaged,
            "workspace",
            "prm_02",
            1,
            None,
            &current,
        ));
    }

    #[test]
    fn malformed_and_unmapped_rehearsal_content_is_rejected() {
        let mut value = definition();
        value.criteria[0]
            .evidence_requirements
            .push(EvidenceRequirementV1 {
                id: "old_accounts".into(),
                criterion_id: "settings_preserved".into(),
                stage: EvidenceStageV1::Code,
                basis: EvidenceBasisV1::Rehearsal,
                producer: "multiverse".into(),
                check_key: "old_account_preservation".into(),
                check_digest: "a".repeat(64),
                scenario_digest: None,
                workflow_digest: Some("b".repeat(64)),
                environment_digest: Some("c".repeat(64)),
                accepted_assurance: vec![AssuranceBasisV1::ObservedTrustedLocal],
            });
        assert!(value.validate().is_err());
        value.criteria[0].evidence_requirements[0].scenario_digest = Some("d".repeat(64));
        value.validate().unwrap();
        value.repositories.push(value.repositories[0].clone());
        assert!(value.validate().is_err());
    }
}
