use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::{HeaderValue, uri::Authority};
use postgres::Transaction;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::backend::{ImmutableBackend, ObjectKey};
use crate::store::{MAX_OBJECT_BYTES, digest, validate_id};
use crate::{Error, Result as StorageResult};

use super::auth::{self, Credentials, NewSession, PrincipalState, Role};
use super::publication::{
    self, CompleteRequest, Manifest, PublicationAuthority, PublicationOutcome, RepositoryScope,
    ReserveRequest, SourceNamespace,
};
use super::{Result, TeamError};

const MAX_PRINCIPALS: i64 = 128;
const MAX_AUTH_OPERATIONS: i64 = 10_000;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024 + 1024;
pub const MAX_READ_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub socket: PathBuf,
    pub port: u16,
    pub database: String,
}
impl DatabaseConfig {
    pub fn open(&self, scope: RepositoryScope) -> Result<PublicationAuthority> {
        PublicationAuthority::connect_local(&self.socket, self.port, &self.database, scope)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    pub database: DatabaseConfig,
    pub objects: PathBuf,
    pub origin: String,
    pub bind: SocketAddr,
    pub max_concurrency: usize,
}
impl TeamConfig {
    pub fn validate(&self) -> StorageResult<()> {
        if !self.bind.ip().is_loopback()
            || !(1..=8).contains(&self.max_concurrency)
            || !self.objects.is_absolute()
            || !self.database.socket.is_absolute()
            || self.database.port == 0
        {
            return Err(Error::Invalid(
                "team config requires loopback bind, absolute socket/objects and concurrency 1..8"
                    .into(),
            ));
        }
        validate_origin(&self.origin)?;
        publication::identifier(&self.database.database)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRequest {
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub operation: String,
    pub max_bytes: i64,
    pub max_objects: i64,
    pub principal: String,
    pub roles: Vec<Role>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRequest {
    pub authority_generation: i64,
    pub operation: String,
    pub principal: String,
    pub credential: String,
    pub roles: Vec<Role>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesRequest {
    pub authority_generation: i64,
    pub operation: String,
    pub expected_version: i64,
    pub roles: Vec<Role>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    pub authority_generation: i64,
    pub operation: String,
    pub expected_version: i64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityOutcome {
    pub version: u32,
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub operation: String,
    pub action: String,
    pub actor: Option<String>,
    pub principal: String,
    pub principal_version: i64,
    pub roles: Vec<Role>,
    pub enabled: bool,
    pub sequence: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetireAccessRequest {
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub operation: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetirementOutcome {
    pub version: u32,
    pub action: String,
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub operation: String,
    pub actor: Option<String>,
    pub retired: bool,
    pub sequence: i64,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RetirementResult {
    pub outcome: RetirementOutcome,
    pub historical: bool,
    pub current_authority_generation: i64,
    pub currently_retired: bool,
}

#[derive(Serialize)]
pub struct PrincipalView {
    pub state: PrincipalState,
    pub enabled: bool,
}
#[derive(Serialize)]
pub struct SessionView {
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub principal: PrincipalState,
}
#[derive(Serialize)]
pub struct PublicationView {
    pub api_version: u32,
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub publication: String,
    pub state: String,
    pub root: String,
    pub manifest_digest: Option<String>,
}

/// This application handle always carries credential material. Authorization is
/// derived from current database rows inside each transaction, never from a
/// caller-provided role or success flag. The operator C1 handle stays separate.
pub struct TeamService {
    pub(super) authority: PublicationAuthority,
}
impl TeamService {
    pub fn connect(
        config: &DatabaseConfig,
        scope: RepositoryScope,
        credentials: Credentials,
    ) -> Result<Self> {
        let mut authority = config.open(scope)?;
        authority.credentials = Some(credentials);
        // Reject invalid/unknown scope credentials uniformly before an action
        // can disclose metadata. Mutations still recheck under their own locks.
        {
            let mut tx = authority.client.transaction()?;
            lock_read(&mut tx, &authority.scope)?;
            auth::authorize(
                &mut tx,
                &authority.scope,
                authority
                    .credentials
                    .as_ref()
                    .ok_or(TeamError::Unauthorized)?,
                None,
                false,
            )?;
            tx.commit()?;
        }
        Ok(Self { authority })
    }

    pub fn reserve(&mut self, request: &ReserveRequest) -> Result<()> {
        self.authority.reserve(request)
    }
    pub fn reserve_profile(
        &mut self,
        request: &ReserveRequest,
        namespace: SourceNamespace,
    ) -> Result<()> {
        self.authority.reserve_profile(request, namespace)
    }
    pub fn complete(&mut self, request: &CompleteRequest) -> Result<PublicationOutcome> {
        self.authority.complete(request)
    }
    pub fn abandon(&mut self, generation: i64, publication: &str) -> Result<()> {
        self.authority.abandon(generation, publication)
    }

    pub fn session(&mut self) -> Result<SessionView> {
        let mut tx = self.authority.client.transaction()?;
        let generation = lock_read(&mut tx, &self.authority.scope)?;
        let principal = auth::authorize(
            &mut tx,
            &self.authority.scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            None,
            false,
        )?;
        tx.commit()?;
        Ok(SessionView {
            scope: self.authority.scope.clone(),
            authority_generation: generation,
            principal,
        })
    }

    pub fn login(
        config: &DatabaseConfig,
        scope: RepositoryScope,
        credential: &str,
        generation: i64,
        ttl_seconds: i32,
    ) -> Result<NewSession> {
        let mut authority = config.open(scope)?;
        let mut tx = authority.client.transaction()?;
        publication::lock_repository(&mut tx, &authority.scope).map_err(hide_unknown_repository)?;
        let session = auth::login(
            &mut tx,
            &authority.scope,
            credential,
            generation,
            ttl_seconds,
        )?;
        tx.commit()?;
        Ok(session)
    }

    pub fn logout(&mut self) -> Result<()> {
        let mut tx = self.authority.client.transaction()?;
        publication::lock_repository(&mut tx, &self.authority.scope)
            .map_err(hide_unknown_repository)?;
        auth::logout(
            &mut tx,
            &self.authority.scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn principal(&mut self, principal: &str) -> Result<PrincipalView> {
        publication::identifier(principal)?;
        let mut tx = self.authority.client.transaction()?;
        lock_read(&mut tx, &self.authority.scope)?;
        auth::authorize(
            &mut tx,
            &self.authority.scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Administrator),
            false,
        )?;
        let view = principal_view(&mut tx, &self.authority.scope, principal)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn provision(&mut self, request: &ProvisionRequest) -> Result<IdentityOutcome> {
        validate_action(
            request.authority_generation,
            &request.operation,
            &request.principal,
        )?;
        let roles = canonical_roles(&request.roles)?;
        let credential = auth::credential_digest(&request.credential)?;
        let scope = &self.authority.scope;
        let mut tx = self.authority.client.transaction()?;
        let generation =
            publication::lock_repository(&mut tx, scope).map_err(hide_unknown_repository)?;
        let actor = auth::authorize(
            &mut tx,
            scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Administrator),
            true,
        )?;
        let request_digest = request_digest(&(
            scope,
            request.authority_generation,
            &request.operation,
            "provision",
            &actor.principal,
            &request.principal,
            &credential,
            &roles,
        ))?;
        if let Some(outcome) = identity_retry(
            &mut tx,
            scope,
            request.authority_generation,
            &request.operation,
            &request_digest,
        )? {
            return Ok(outcome);
        }
        reject_generation(generation, request.authority_generation)?;
        let count: i64 = tx
            .query_one(
                "SELECT count(*) FROM team_principals WHERE trust_domain=$1 AND repository=$2",
                &[&scope.trust_domain, &scope.repository],
            )?
            .get(0);
        if count >= MAX_PRINCIPALS {
            return Err(Error::Limit("repository principal limit exceeded".into()).into());
        }
        let inserted = tx.execute(
            "INSERT INTO team_principals VALUES($1,$2,$3,$4,1,TRUE) ON CONFLICT DO NOTHING",
            &[
                &scope.trust_domain,
                &scope.repository,
                &request.principal,
                &credential,
            ],
        )?;
        if inserted != 1 {
            return Err(TeamError::Conflict(
                "principal or credential already provisioned".into(),
            ));
        }
        replace_roles(&mut tx, scope, &request.principal, &roles)?;
        let outcome = record_identity(
            &mut tx,
            scope,
            request.authority_generation,
            &request.operation,
            &request_digest,
            "provision",
            Some(&actor.principal),
            &request.principal,
            1,
            roles,
            true,
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn roles(&mut self, principal: &str, request: &RolesRequest) -> Result<IdentityOutcome> {
        validate_action(request.authority_generation, &request.operation, principal)?;
        let roles = canonical_roles(&request.roles)?;
        self.change_principal(
            principal,
            request.authority_generation,
            &request.operation,
            request.expected_version,
            Some(roles),
        )
    }
    pub fn revoke(&mut self, principal: &str, request: &RevokeRequest) -> Result<IdentityOutcome> {
        validate_action(request.authority_generation, &request.operation, principal)?;
        self.change_principal(
            principal,
            request.authority_generation,
            &request.operation,
            request.expected_version,
            None,
        )
    }

    fn change_principal(
        &mut self,
        principal: &str,
        requested_generation: i64,
        operation: &str,
        expected_version: i64,
        roles: Option<Vec<Role>>,
    ) -> Result<IdentityOutcome> {
        if expected_version <= 0 {
            return Err(
                Error::Invalid("expected principal version must be positive".into()).into(),
            );
        }
        let scope = &self.authority.scope;
        let mut tx = self.authority.client.transaction()?;
        let generation =
            publication::lock_repository(&mut tx, scope).map_err(hide_unknown_repository)?;
        let actor = auth::authorize(
            &mut tx,
            scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Administrator),
            true,
        )?;
        let action = if roles.is_some() { "roles" } else { "revoke" };
        let request_digest = request_digest(&(
            scope,
            requested_generation,
            operation,
            action,
            &actor.principal,
            principal,
            expected_version,
            &roles,
        ))?;
        if let Some(outcome) = identity_retry(
            &mut tx,
            scope,
            requested_generation,
            operation,
            &request_digest,
        )? {
            return Ok(outcome);
        }
        reject_generation(generation, requested_generation)?;
        let current = principal_view(&mut tx, scope, principal)?;
        if !current.enabled || current.state.version != expected_version {
            return Err(TeamError::Conflict(
                "principal generation changed or revoked".into(),
            ));
        }
        let retains_admin = roles
            .as_ref()
            .is_some_and(|roles| roles.contains(&Role::Administrator));
        if current.state.roles.contains(&Role::Administrator) && !retains_admin {
            let others: bool = tx.query_one("SELECT EXISTS(SELECT 1 FROM team_principals p JOIN team_roles r USING(trust_domain,repository,principal) WHERE p.trust_domain=$1 AND p.repository=$2 AND p.principal<>$3 AND p.enabled AND r.role='administrator')", &[&scope.trust_domain,&scope.repository,&principal])?.get(0);
            if !others {
                return Err(TeamError::Conflict(
                    "cannot remove the last repository administrator".into(),
                ));
            }
        }
        let version = expected_version
            .checked_add(1)
            .ok_or_else(|| Error::Limit("principal generation exhausted".into()))?;
        let enabled = roles.is_some();
        tx.execute("UPDATE team_principals SET version=$4,enabled=$5 WHERE trust_domain=$1 AND repository=$2 AND principal=$3", &[&scope.trust_domain,&scope.repository,&principal,&version,&enabled])?;
        let roles = roles.unwrap_or_default();
        replace_roles(&mut tx, scope, principal, &roles)?;
        let outcome = record_identity(
            &mut tx,
            scope,
            requested_generation,
            operation,
            &request_digest,
            action,
            Some(&actor.principal),
            principal,
            version,
            roles,
            enabled,
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn inspect(&mut self, generation: i64, publication_id: &str) -> Result<PublicationView> {
        publication::identifier(publication_id)?;
        let scope = &self.authority.scope;
        let mut tx = self.authority.client.transaction()?;
        lock_read(&mut tx, scope)?;
        auth::authorize(
            &mut tx,
            scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Reader),
            false,
        )?;
        let row = tx.query_opt("SELECT request,state,manifest_digest FROM team_publications WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&scope.trust_domain,&scope.repository,&generation,&publication_id])?.ok_or_else(|| Error::NotFound("publication".into()))?;
        let request: ReserveRequest = publication::parse(&row.get::<_, String>(0))?;
        let view = PublicationView {
            api_version: 1,
            scope: scope.clone(),
            authority_generation: generation,
            publication: publication_id.to_owned(),
            state: row.get(1),
            root: request.root,
            manifest_digest: row.get(2),
        };
        tx.commit()?;
        Ok(view)
    }

    /// Authorization and retained-manifest membership precede every backend
    /// access. Recheck before disclosure after potentially slow immutable reads.
    /// No collector exists yet; future leases must join this same authority.
    pub fn read_object<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication_id: &str,
        id: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<ObjectRead> {
        publication::identifier(publication_id)?;
        validate_id(id)?;
        let permit = self.object_permit(generation, publication_id, id)?;
        let length = length.unwrap_or(permit.size);
        if length > MAX_READ_BYTES
            || offset
                .checked_add(length)
                .is_none_or(|end| end > permit.size)
        {
            return Err(Error::Limit(
                "object read requires an in-bounds range of at most 1MiB".into(),
            )
            .into());
        }
        let bytes = backend.get_range_verified(&permit.key, &permit.incarnation, offset, length)?;
        if bytes.len() as u64 != length {
            return Err(TeamError::Unavailable);
        }
        let current = self.object_permit(generation, publication_id, id)?;
        if current.manifest_digest != permit.manifest_digest
            || current.incarnation != permit.incarnation
            || current.size != permit.size
        {
            return Err(TeamError::Indeterminate);
        }
        Ok(ObjectRead {
            bytes,
            total_bytes: permit.size,
            offset,
            manifest_digest: permit.manifest_digest,
        })
    }

    fn object_permit(
        &mut self,
        generation: i64,
        publication_id: &str,
        id: &str,
    ) -> Result<ObjectPermit> {
        let scope = &self.authority.scope;
        let mut tx = self.authority.client.transaction()?;
        lock_read(&mut tx, scope)?;
        auth::authorize(
            &mut tx,
            scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Reader),
            false,
        )?;
        let row = tx.query_opt("SELECT p.manifest,p.manifest_digest,r.root,p.source_profile FROM team_publications p JOIN team_roots r USING(trust_domain,repository,generation,publication) WHERE p.trust_domain=$1 AND p.repository=$2 AND p.generation=$3 AND p.publication=$4 AND p.state='complete'", &[&scope.trust_domain,&scope.repository,&generation,&publication_id])?.ok_or_else(|| Error::NotFound("retained complete publication".into()))?;
        let encoded: Option<String> = row.get(0);
        let encoded = encoded.ok_or(TeamError::Unavailable)?;
        if encoded.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Limit("retained manifest exceeds bound".into()).into());
        }
        let manifest_digest: Option<String> = row.get(1);
        let manifest_digest = manifest_digest.ok_or(TeamError::Unavailable)?;
        if digest(encoded.as_bytes()) != manifest_digest {
            return Err(TeamError::Unavailable);
        }
        let manifest: Manifest = publication::parse(&encoded)?;
        let stored_profile: String = row.get(3);
        if ![SourceNamespace::V1.as_str(), SourceNamespace::V2.as_str()]
            .contains(&manifest.profile.as_str())
            || manifest.profile != stored_profile
            || manifest.root != row.get::<_, String>(2)
        {
            return Err(TeamError::Unavailable);
        }
        let object = manifest
            .objects
            .iter()
            .find(|object| object.id == id)
            .ok_or_else(|| Error::NotFound("object outside retained publication".into()))?;
        if object.encoded_bytes > MAX_OBJECT_BYTES {
            return Err(Error::Limit("retained object exceeds bound".into()).into());
        }
        let permit = ObjectPermit {
            key: ObjectKey::new(
                &scope.trust_domain,
                &scope.repository,
                &manifest.profile,
                id,
            )?,
            incarnation: manifest.incarnation,
            size: object.encoded_bytes,
            manifest_digest,
        };
        tx.commit()?;
        Ok(permit)
    }

    /// Explicit role is necessary but never a substitute for installed policy,
    /// current source/context and independently collected evidence admission.
    pub fn protected_target_admission(&mut self) -> Result<()> {
        let mut tx = self.authority.client.transaction()?;
        lock_read(&mut tx, &self.authority.scope)?;
        auth::authorize(
            &mut tx,
            &self.authority.scope,
            self.authority
                .credentials
                .as_ref()
                .ok_or(TeamError::Unauthorized)?,
            Some(Role::Integrator),
            false,
        )?;
        Err(TeamError::Unavailable)
    }
}

pub struct ObjectRead {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub offset: u64,
    pub manifest_digest: String,
}
struct ObjectPermit {
    key: ObjectKey,
    incarnation: String,
    size: u64,
    manifest_digest: String,
}

/// Trusted local bootstrap is expected-absent and atomic. The caller durably
/// retains its operator-generated credential before committing this operation.
pub fn operator_bootstrap(
    config: &DatabaseConfig,
    request: &BootstrapRequest,
    credential: &str,
) -> Result<IdentityOutcome> {
    request.scope.validate()?;
    validate_action(
        request.authority_generation,
        &request.operation,
        &request.principal,
    )?;
    let roles = canonical_roles(&request.roles)?;
    if request.max_bytes <= 0 || request.max_objects <= 0 || !roles.contains(&Role::Administrator) {
        return Err(Error::Invalid(
            "bootstrap requires positive budgets and explicit administrator role".into(),
        )
        .into());
    }
    let credential = auth::credential_digest(credential)?;
    let request_digest = request_digest(&(
        &request.scope,
        request.authority_generation,
        &request.operation,
        "bootstrap",
        request.max_bytes,
        request.max_objects,
        &request.principal,
        &credential,
        &roles,
    ))?;
    let mut authority = config.open(request.scope.clone())?;
    let mut tx = authority.client.transaction()?;
    let inserted=tx.execute("INSERT INTO team_repositories(trust_domain,repository,generation,max_bytes,max_objects) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING", &[&request.scope.trust_domain,&request.scope.repository,&request.authority_generation,&request.max_bytes,&request.max_objects])?;
    publication::lock_repository(&mut tx, &request.scope)?;
    if let Some(outcome) = identity_retry(
        &mut tx,
        &request.scope,
        request.authority_generation,
        &request.operation,
        &request_digest,
    )? {
        return Ok(outcome);
    }
    if inserted != 1 {
        return Err(TeamError::Conflict(
            "repository already exists; bootstrap cannot replace it".into(),
        ));
    }
    tx.execute(
        "INSERT INTO team_principals VALUES($1,$2,$3,$4,1,TRUE)",
        &[
            &request.scope.trust_domain,
            &request.scope.repository,
            &request.principal,
            &credential,
        ],
    )?;
    replace_roles(&mut tx, &request.scope, &request.principal, &roles)?;
    let outcome = record_identity(
        &mut tx,
        &request.scope,
        request.authority_generation,
        &request.operation,
        &request_digest,
        "bootstrap",
        None,
        &request.principal,
        1,
        roles,
        true,
    )?;
    tx.commit()?;
    Ok(outcome)
}

/// Terminal access shutdown by the trusted local database/OS operator. This
/// never deletes source/history or reopens a namespace. Application credentials
/// cannot invoke it, and local exact retries remain possible after shutdown.
pub fn operator_retire_access(
    config: &DatabaseConfig,
    request: &RetireAccessRequest,
) -> Result<RetirementResult> {
    request.scope.validate()?;
    publication::identifier(&request.operation)?;
    if request.authority_generation <= 0 {
        return Err(Error::Invalid("authority generation must be positive".into()).into());
    }
    let scope = &request.scope;
    let request_digest = request_digest(&(
        "agentscm.team.retire-access/v1",
        scope,
        request.authority_generation,
        &request.operation,
    ))?;
    let mut authority = config.open(scope.clone())?;
    let mut tx = authority.client.transaction()?;
    // Dedicated local retry path may lock a retired row. Every ordinary
    // application/controller lock rejects it before facts or source lookup.
    let row = tx.query_opt("SELECT generation,retired FROM team_repositories WHERE trust_domain=$1 AND repository=$2 FOR UPDATE", &[&scope.trust_domain,&scope.repository])?.ok_or_else(|| Error::NotFound("repository".into()))?;
    let current: i64 = row.get(0);
    let retired: bool = row.get(1);
    if let Some(outcome) = operation_retry::<RetirementOutcome>(
        &mut tx,
        scope,
        request.authority_generation,
        &request.operation,
        &request_digest,
        "retirement",
    )? {
        if outcome.version != 1
            || outcome.action != "retire_access"
            || outcome.scope != *scope
            || outcome.authority_generation != request.authority_generation
            || outcome.operation != request.operation
            || !outcome.retired
            || outcome.actor.is_some()
            || current < request.authority_generation
            || (current == request.authority_generation && !retired)
        {
            return Err(TeamError::Indeterminate);
        }
        return Ok(RetirementResult {
            historical: current != request.authority_generation,
            current_authority_generation: current,
            currently_retired: retired,
            outcome,
        });
    }
    reject_generation(current, request.authority_generation)?;
    if retired {
        return Err(TeamError::Conflict(
            "repository access already retired".into(),
        ));
    }
    // Versions created by this bounded profile cannot reach i64::MAX. The
    // database's checked arithmetic still aborts inconsistent operator state.
    tx.execute("UPDATE team_principals SET enabled=FALSE,version=version+1 WHERE trust_domain=$1 AND repository=$2 AND enabled", &[&scope.trust_domain,&scope.repository])?;
    tx.execute(
        "DELETE FROM team_roles WHERE trust_domain=$1 AND repository=$2",
        &[&scope.trust_domain, &scope.repository],
    )?;
    tx.execute(
        "UPDATE team_sessions SET revoked=TRUE WHERE trust_domain=$1 AND repository=$2",
        &[&scope.trust_domain, &scope.repository],
    )?;
    // Cached source pages and continuations are ephemeral. Retire them in the
    // same transaction as grants and sessions while retaining source/audit.
    tx.execute(
        "DELETE FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2",
        &[&scope.trust_domain, &scope.repository],
    )?;
    tx.execute(
        "UPDATE team_repositories SET retired=TRUE WHERE trust_domain=$1 AND repository=$2",
        &[&scope.trust_domain, &scope.repository],
    )?;
    let sequence = next_access_sequence(&mut tx, scope)?;
    let outcome = RetirementOutcome {
        version: 1,
        action: "retire_access".into(),
        scope: scope.clone(),
        authority_generation: current,
        operation: request.operation.clone(),
        actor: None,
        retired: true,
        sequence,
    };
    record_access_outcome(
        &mut tx,
        scope,
        current,
        &request.operation,
        &request_digest,
        ("retirement", sequence),
        &outcome,
    )?;
    tx.commit()?;
    Ok(RetirementResult {
        outcome,
        historical: false,
        current_authority_generation: current,
        currently_retired: true,
    })
}

pub(super) fn lock_read(tx: &mut Transaction<'_>, scope: &RepositoryScope) -> Result<i64> {
    tx.query_opt("SELECT generation FROM team_repositories WHERE trust_domain=$1 AND repository=$2 AND NOT retired FOR SHARE", &[&scope.trust_domain,&scope.repository])?.map(|row|row.get(0)).ok_or(TeamError::Unauthorized)
}
fn hide_unknown_repository(error: TeamError) -> TeamError {
    match error {
        TeamError::Storage(Error::NotFound(_)) => TeamError::Unauthorized,
        error => error,
    }
}
fn validate_action(generation: i64, operation: &str, principal: &str) -> Result<()> {
    publication::identifier(operation)?;
    publication::identifier(principal)?;
    if generation <= 0 {
        return Err(Error::Invalid("authority generation must be positive".into()).into());
    }
    Ok(())
}
fn canonical_roles(roles: &[Role]) -> Result<Vec<Role>> {
    if roles.len() > 5 {
        return Err(Error::Limit("at most five explicit roles".into()).into());
    }
    let mut ordered = roles.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.len() != roles.len() {
        return Err(Error::Invalid("duplicate role".into()).into());
    }
    Ok(ordered)
}
fn request_digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(digest(&serde_json::to_vec(value).map_err(Error::from)?))
}
fn replace_roles(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
    roles: &[Role],
) -> Result<()> {
    tx.execute(
        "DELETE FROM team_roles WHERE trust_domain=$1 AND repository=$2 AND principal=$3",
        &[&scope.trust_domain, &scope.repository, &principal],
    )?;
    for role in roles {
        tx.execute(
            "INSERT INTO team_roles VALUES($1,$2,$3,$4)",
            &[
                &scope.trust_domain,
                &scope.repository,
                &principal,
                &role.as_str(),
            ],
        )?;
    }
    Ok(())
}
fn principal_view(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
) -> Result<PrincipalView> {
    let row=tx.query_opt("SELECT version,enabled FROM team_principals WHERE trust_domain=$1 AND repository=$2 AND principal=$3 FOR KEY SHARE", &[&scope.trust_domain,&scope.repository,&principal])?.ok_or_else(||Error::NotFound("principal".into()))?;
    let roles = tx.query("SELECT role FROM team_roles WHERE trust_domain=$1 AND repository=$2 AND principal=$3 ORDER BY role", &[&scope.trust_domain,&scope.repository,&principal])?.into_iter().map(|row|Role::from_database(&row.get::<_,String>(0))).collect::<Result<Vec<_>>>()?;
    Ok(PrincipalView {
        state: PrincipalState {
            principal: principal.to_owned(),
            version: row.get(0),
            roles,
        },
        enabled: row.get(1),
    })
}
fn identity_retry(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    operation: &str,
    request_digest: &str,
) -> Result<Option<IdentityOutcome>> {
    operation_retry(tx, scope, generation, operation, request_digest, "identity")
}
fn operation_retry<T: DeserializeOwned>(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    operation: &str,
    request_digest: &str,
    kind: &str,
) -> Result<Option<T>> {
    if let Some(row)=tx.query_opt("SELECT request_digest,outcome,kind FROM team_outcomes WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND operation=$4", &[&scope.trust_domain,&scope.repository,&generation,&operation])? {
        if row.get::<_,String>(0)!=request_digest || row.get::<_,String>(2)!=kind { return Err(TeamError::Conflict("operation ID reused".into())); }
        return Ok(Some(publication::parse(&row.get::<_,String>(1))?));
    }
    Ok(None)
}

fn reject_generation(current: i64, requested: i64) -> Result<()> {
    if current > requested {
        Err(TeamError::Indeterminate)
    } else if current < requested {
        Err(TeamError::Conflict("authority generation rejected".into()))
    } else {
        Ok(())
    }
}
#[allow(clippy::too_many_arguments)]
fn record_identity(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    operation: &str,
    request_digest: &str,
    action: &str,
    actor: Option<&str>,
    principal: &str,
    version: i64,
    roles: Vec<Role>,
    enabled: bool,
) -> Result<IdentityOutcome> {
    let sequence = next_access_sequence(tx, scope)?;
    let outcome = IdentityOutcome {
        version: 1,
        scope: scope.clone(),
        authority_generation: generation,
        operation: operation.to_owned(),
        action: action.to_owned(),
        actor: actor.map(str::to_owned),
        principal: principal.to_owned(),
        principal_version: version,
        roles,
        enabled,
        sequence,
    };
    record_access_outcome(
        tx,
        scope,
        generation,
        operation,
        request_digest,
        ("identity", sequence),
        &outcome,
    )?;
    Ok(outcome)
}
fn next_access_sequence(tx: &mut Transaction<'_>, scope: &RepositoryScope) -> Result<i64> {
    let count:i64=tx.query_one("SELECT count(*) FROM team_outcomes WHERE trust_domain=$1 AND repository=$2 AND kind IN ('identity','retirement')", &[&scope.trust_domain,&scope.repository])?.get(0);
    let enabled_principals: i64 = tx.query_one("SELECT count(*) FROM team_principals WHERE trust_domain=$1 AND repository=$2 AND enabled", &[&scope.trust_domain,&scope.repository])?.get(0);
    // This transaction already applied the proposed identity change. Reserve
    // one durable outcome for terminal revocation of every enabled principal.
    // A revoke consumes an outcome and frees its principal's reserved slot.
    if count + 1 + enabled_principals > MAX_AUTH_OPERATIONS {
        return Err(Error::Limit("retained identity-operation limit exceeded".into()).into());
    }
    let sequence:i64=tx.query_one("UPDATE team_repositories SET sequence=sequence+1 WHERE trust_domain=$1 AND repository=$2 RETURNING sequence", &[&scope.trust_domain,&scope.repository])?.get(0);
    Ok(sequence)
}
fn record_access_outcome(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    operation: &str,
    request_digest: &str,
    record: (&str, i64),
    outcome: &impl Serialize,
) -> Result<()> {
    let (kind, sequence) = record;
    let encoded = publication::json(outcome)?;
    tx.execute("INSERT INTO team_outcomes(trust_domain,repository,generation,operation,request_digest,outcome,kind) VALUES($1,$2,$3,$4,$5,$6,$7)", &[&scope.trust_domain,&scope.repository,&generation,&operation,&request_digest,&encoded,&kind])?;
    tx.execute("INSERT INTO team_events(trust_domain,repository,sequence,outcome,kind) VALUES($1,$2,$3,$4,$5)", &[&scope.trust_domain,&scope.repository,&sequence,&encoded,&kind])?;
    tx.execute("INSERT INTO team_outbox(trust_domain,repository,sequence,payload,kind) VALUES($1,$2,$3,$4,$5)", &[&scope.trust_domain,&scope.repository,&sequence,&encoded,&kind])?;
    Ok(())
}

pub(super) fn validate_origin(origin: &str) -> StorageResult<()> {
    let invalid = || {
        Error::Invalid(
            "team origin requires canonical HTTPS with a supported ASCII DNS host".into(),
        )
    };
    let authority = origin.strip_prefix("https://").ok_or_else(invalid)?;
    let parsed: Authority = authority.parse().map_err(|_| invalid())?;
    let host = parsed.host();
    // A deliberately narrow browser-origin profile: lowercase ASCII DNS only.
    // IP/IDNA/percent forms require URL normalization and are unsupported here.
    if host.len() > 253
        || !host
            .rsplit('.')
            .next()
            .is_some_and(|label| label.as_bytes().first().is_some_and(u8::is_ascii_lowercase))
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with("xn--")
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        || HeaderValue::from_str(origin).is_err()
    {
        return Err(invalid());
    }
    let suffix = &authority[host.len()..];
    if !suffix.is_empty() {
        let raw = suffix.strip_prefix(':').ok_or_else(invalid)?;
        let port: u16 = raw.parse().map_err(|_| invalid())?;
        if port == 0 || port == 443 || port.to_string() != raw {
            return Err(invalid());
        }
    }
    Ok(())
}
