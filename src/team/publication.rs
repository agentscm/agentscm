use std::path::Path;

#[cfg(unix)]
use postgres::NoTls;
use postgres::{Client, Transaction};
use serde::{Deserialize, Serialize};

use crate::backend::{ImmutableBackend, ObjectKey};
use crate::store::{Engine, digest, validate_id};
use crate::sync::{self, ObjectDescriptor, SOURCE_PROFILE_V1, SOURCE_PROFILE_V2};
use crate::{Error, Result as StorageResult};

use super::auth::{self, Credentials, Role};
use super::{Result, TeamError};

const MAX_PUBLICATION_BYTES: i64 = 1024 * 1024 * 1024;
const MAX_PUBLICATION_OBJECTS: i64 = 100_000;
const MAX_LEASE_SECONDS: i32 = 3_600;

const SOURCE_CAPABILITIES_V1: [&str; 3] = [
    "complete-revision-history",
    "exact-object-bytes",
    "root-last-import",
];
const SOURCE_CAPABILITIES_V2: [&str; 4] = [
    "complete-revision-history",
    "exact-object-bytes",
    "root-last-import",
    "multi-parent-revisions",
];

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SourceNamespace {
    #[serde(rename = "agentscm-source-v1")]
    V1,
    #[serde(rename = "agentscm-source-v2")]
    V2,
}

impl SourceNamespace {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => SOURCE_PROFILE_V1,
            Self::V2 => SOURCE_PROFILE_V2,
        }
    }

    pub fn version(self) -> u32 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    pub fn required_capabilities(self) -> &'static [&'static str] {
        match self {
            Self::V1 => &SOURCE_CAPABILITIES_V1,
            Self::V2 => &SOURCE_CAPABILITIES_V2,
        }
    }

    pub fn validate_offer(self, offer: &sync::SourceOffer) -> StorageResult<()> {
        if offer.version != self.version()
            || offer.profile != self.as_str()
            || !offer
                .required_capabilities
                .iter()
                .map(String::as_str)
                .eq(self.required_capabilities().iter().copied())
        {
            let message = match self {
                Self::V1 => {
                    "publication supports only source profile v1 with its exact capabilities"
                        .to_owned()
                }
                Self::V2 => format!(
                    "publication source does not match reserved profile {}",
                    self.as_str()
                ),
            };
            return Err(Error::Invalid(message));
        }
        Ok(())
    }

    fn parse(value: &str) -> StorageResult<Self> {
        match value {
            SOURCE_PROFILE_V1 => Ok(Self::V1),
            SOURCE_PROFILE_V2 => Ok(Self::V2),
            _ => Err(Error::Invalid("unsupported stored source profile".into())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryScope {
    pub trust_domain: String,
    pub repository: String,
}

impl RepositoryScope {
    pub(super) fn validate(&self) -> StorageResult<()> {
        identifier(&self.trust_domain)?;
        identifier(&self.repository)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveRequest {
    pub generation: i64,
    pub publication: String,
    pub root: String,
    pub byte_limit: i64,
    pub object_limit: i64,
    pub lease_seconds: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRequest {
    pub generation: i64,
    pub publication: String,
    pub operation: String,
    pub manifest_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationOutcome {
    pub scope: RepositoryScope,
    pub generation: i64,
    pub publication: String,
    pub operation: String,
    pub root: String,
    pub manifest_digest: String,
    pub sequence: i64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Manifest {
    pub(super) profile: String,
    pub(super) root: String,
    // Every object uses this publication's unique incarnation. There is no
    // cross-publication reuse or collector in this increment.
    pub(super) incarnation: String,
    pub(super) objects: Vec<ObjectDescriptor>,
}

/// Trusted operator/controller API, scoped at construction. Callers must not
/// expose it to clients until repository authorization and policy are wired.
/// Connections are bounded synchronous clients and must run off async workers.
pub struct PublicationAuthority {
    pub(super) client: Client,
    pub(super) scope: RepositoryScope,
    pub(super) credentials: Option<Credentials>,
}

impl PublicationAuthority {
    /// Connect only to a project-owned local Unix socket. Remote TLS/pooling and
    /// deployment credentials belong to a later authenticated service profile.
    #[cfg(unix)]
    pub fn connect_local(
        socket: &Path,
        port: u16,
        database: &str,
        scope: RepositoryScope,
    ) -> Result<Self> {
        scope.validate()?;
        identifier(database)?;
        if !socket.is_absolute() {
            return Err(Error::Invalid("PostgreSQL socket must be absolute".into()).into());
        }
        let mut config = postgres::Config::new();
        config
            .host_path(socket)
            .port(port)
            .dbname(database)
            .connect_timeout(std::time::Duration::from_secs(5));
        let mut client = config.connect(NoTls)?;
        client.batch_execute("SET search_path = public; SET statement_timeout = '5s'; SET lock_timeout = '5s'; SET idle_in_transaction_session_timeout = '5s'; SET synchronous_commit = on;")?;
        let mut tx = client.transaction()?;
        tx.query_one("SELECT pg_advisory_xact_lock(1095975757)", &[])?;
        let exists: bool = tx
            .query_one("SELECT to_regclass('public.team_schema') IS NOT NULL", &[])?
            .get(0);
        if !exists {
            tx.batch_execute(include_str!("schema.sql"))?;
        }
        let mut version: i32 = tx
            .query_one("SELECT version FROM team_schema WHERE singleton", &[])?
            .get(0);
        if version == 1 {
            tx.batch_execute(include_str!("auth-schema.sql"))?;
            version = 2;
        }
        if version == 2 {
            tx.batch_execute(include_str!("source-schema.sql"))?;
            version = 3;
        }
        if version == 3 {
            tx.batch_execute(include_str!("publication-owner-schema.sql"))?;
            version = 4;
        }
        if version != 4 {
            return Err(Error::Invalid("unsupported team schema".into()).into());
        }
        tx.commit()?;
        Ok(Self {
            client,
            scope,
            credentials: None,
        })
    }
    #[cfg(not(unix))]
    pub fn connect_local(
        _socket: &Path,
        _port: u16,
        _database: &str,
        _scope: RepositoryScope,
    ) -> Result<Self> {
        Err(Error::Invalid(
            "the team service needs a PostgreSQL Unix socket and is not available on Windows"
                .into(),
        )
        .into())
    }

    /// Registration is operator-only, expected-absent, and never replaces a
    /// repository or changes its generation. No live promotion is supported.
    pub fn register(&mut self, generation: i64, max_bytes: i64, max_objects: i64) -> Result<()> {
        if generation <= 0 || max_bytes <= 0 || max_objects <= 0 {
            return Err(
                Error::Invalid("repository generation/quotas must be positive".into()).into(),
            );
        }
        let inserted = self.client.execute("INSERT INTO team_repositories(trust_domain,repository,generation,max_bytes,max_objects) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING", &[&self.scope.trust_domain,&self.scope.repository,&generation,&max_bytes,&max_objects])?;
        if inserted != 1 {
            return Err(TeamError::Conflict("repository already registered".into()));
        }
        Ok(())
    }

    pub fn reserve(&mut self, request: &ReserveRequest) -> Result<()> {
        self.reserve_inner(request, SourceNamespace::V1, false)
    }

    pub fn reserve_profile(
        &mut self,
        request: &ReserveRequest,
        namespace: SourceNamespace,
    ) -> Result<()> {
        self.reserve_inner(request, namespace, true)
    }

    fn reserve_inner(
        &mut self,
        request: &ReserveRequest,
        namespace: SourceNamespace,
        explicit_profile: bool,
    ) -> Result<()> {
        validate_reservation(request)?;
        let canonical = json(request)?;
        // Preserve the original request digest for the existing reserve API.
        // Explicit-profile reservations use a separate domain that binds the
        // exact namespace even when V1 was selected.
        let request_digest = if explicit_profile {
            digest(&serde_json::to_vec(&(&self.scope, namespace, request)).map_err(Error::from)?)
        } else {
            digest(&serde_json::to_vec(&(&self.scope, request)).map_err(Error::from)?)
        };
        let mut tx = self.client.transaction()?;
        let principal = fence_authorized(
            &mut tx,
            &self.scope,
            request.generation,
            self.credentials.as_ref(),
        )?;
        if let Some(row) = tx.query_opt("SELECT request_digest,state,expires_at > clock_timestamp(),source_profile,owner FROM team_publications WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&self.scope.trust_domain,&self.scope.repository,&request.generation,&request.publication])? {
            if row.get::<_,String>(0) != request_digest
                || row.get::<_,String>(1) == "abandoned"
                || row.get::<_,String>(3) != namespace.as_str()
            {
                return Err(TeamError::Conflict("publication ID reused".into()));
            }
            if row.get::<_,String>(1) != "complete" {
                require_owner(principal.as_deref(), row.get::<_,Option<String>>(4).as_deref())?;
            }
            if row.get::<_,String>(1) != "complete" && !row.get::<_,bool>(2) {
                return Err(TeamError::Expired);
            }
            return Ok(());
        }
        let updated = tx.execute("UPDATE team_repositories SET allocated_bytes=allocated_bytes+$3,allocated_objects=allocated_objects+$4 WHERE trust_domain=$1 AND repository=$2 AND $3 <= max_bytes-allocated_bytes AND $4 <= max_objects-allocated_objects", &[&self.scope.trust_domain,&self.scope.repository,&request.byte_limit,&request.object_limit])?;
        if updated != 1 {
            return Err(Error::Limit("repository reservation quota exceeded".into()).into());
        }
        tx.execute("INSERT INTO team_publications(trust_domain,repository,generation,publication,request_digest,request,state,expires_at,source_profile,owner) VALUES($1,$2,$3,$4,$5,$6,'reserved',clock_timestamp()+$7::integer * interval '1 second',$8,$9)", &[&self.scope.trust_domain,&self.scope.repository,&request.generation,&request.publication,&request_digest,&canonical,&request.lease_seconds,&namespace.as_str(),&principal])?;
        tx.commit()?;
        Ok(())
    }

    /// Prove complete source/history closure using the existing sync
    /// validator, then upload and read-verify every object outside DB locks.
    /// The output offer must be a new path in operator-owned scratch. Interrupted
    /// work stays reserved/uploading; retry with a fresh offer output path.
    pub fn upload_and_qualify<B: ImmutableBackend>(
        &mut self,
        generation: i64,
        publication: &str,
        source: &Engine,
        backend: &B,
        offer_output: &Path,
    ) -> Result<String> {
        identifier(publication)?;
        let (request, namespace) = {
            let mut tx = self.client.transaction()?;
            let principal =
                fence_authorized(&mut tx, &self.scope, generation, self.credentials.as_ref())?;
            let row = active_publication(
                &mut tx,
                &self.scope,
                generation,
                publication,
                principal.as_deref(),
            )?;
            if row.get::<_, String>(1) == "complete" {
                return Err(TeamError::Conflict("publication already complete".into()));
            }
            if row.get::<_, String>(1) == "qualifying" {
                return row.get::<_, Option<String>>(3).ok_or_else(|| {
                    TeamError::Conflict("qualified publication has no manifest".into())
                });
            }
            let request: ReserveRequest = parse(&row.get::<_, String>(0))?;
            let namespace = SourceNamespace::parse(&row.get::<_, String>(4))?;
            tx.commit()?;
            (request, namespace)
        };
        let offer = sync::write_offer(
            source,
            &self.scope.repository,
            std::slice::from_ref(&request.root),
            offer_output,
        )?;
        namespace.validate_offer(&offer)?;
        let total = offer.objects.iter().try_fold(0u64, |sum, object| {
            sum.checked_add(object.encoded_bytes)
                .ok_or_else(|| Error::Limit("publication bytes overflow".into()))
        })?;
        if total > request.byte_limit as u64 || offer.objects.len() as i64 > request.object_limit {
            return Err(Error::Limit("qualified closure exceeds reservation".into()).into());
        }
        // Validation has no backend side effects. Keep a new reservation
        // refundable until the last fenced admission before preparation/writes.
        // A concurrent abandon can win here; it must prevent all later writes.
        {
            let mut tx = self.client.transaction()?;
            let principal =
                fence_authorized(&mut tx, &self.scope, generation, self.credentials.as_ref())?;
            let row = active_publication(
                &mut tx,
                &self.scope,
                generation,
                publication,
                principal.as_deref(),
            )?;
            if SourceNamespace::parse(&row.get::<_, String>(4))? != namespace {
                return Err(TeamError::Conflict(
                    "publication source profile changed".into(),
                ));
            }
            match row.get::<_, String>(1).as_str() {
                "complete" => {
                    return Err(TeamError::Conflict("publication already complete".into()));
                }
                "qualifying" => {
                    return row.get::<_, Option<String>>(3).ok_or_else(|| {
                        TeamError::Conflict("qualified publication has no manifest".into())
                    });
                }
                _ => {}
            }
            tx.execute("UPDATE team_publications SET state='uploading' WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&self.scope.trust_domain,&self.scope.repository,&generation,&publication])?;
            tx.commit()?;
        }
        let incarnation = format!("{generation}-{publication}");
        backend.prepare_publication(
            &ObjectKey {
                trust_domain: self.scope.trust_domain.clone(),
                repository: self.scope.repository.clone(),
                profile: namespace.as_str().to_owned(),
                id: request.root.clone(),
            },
            &incarnation,
        )?;
        for object in &offer.objects {
            let bytes = source.read_encoded_object(&object.id)?;
            let key = ObjectKey {
                trust_domain: self.scope.trust_domain.clone(),
                repository: self.scope.repository.clone(),
                profile: namespace.as_str().to_owned(),
                id: object.id.clone(),
            };
            backend.put_if_absent(&key, &incarnation, &bytes)?;
            if backend.get_verified(&key, &incarnation)? != bytes {
                return Err(Error::Invalid("uploaded immutable object differs".into()).into());
            }
        }
        let manifest = json(&Manifest {
            profile: namespace.as_str().to_owned(),
            root: request.root,
            incarnation,
            objects: offer.objects,
        })?;
        let manifest_digest = digest(manifest.as_bytes());
        let mut tx = self.client.transaction()?;
        let principal =
            fence_authorized(&mut tx, &self.scope, generation, self.credentials.as_ref())?;
        let row = active_publication(
            &mut tx,
            &self.scope,
            generation,
            publication,
            principal.as_deref(),
        )?;
        let state: String = row.get(1);
        if state == "complete" {
            return Err(TeamError::Conflict("publication already complete".into()));
        }
        if let Some(existing) = row.get::<_, Option<String>>(3)
            && existing != manifest_digest
        {
            return Err(TeamError::Conflict("qualification identity changed".into()));
        }
        tx.execute("UPDATE team_publications SET state='qualifying',manifest=$5,manifest_digest=$6 WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&self.scope.trust_domain,&self.scope.repository,&generation,&publication,&manifest,&manifest_digest])?;
        tx.commit()?;
        Ok(manifest_digest)
    }

    /// Complete a retained draft only. This does not integrate a target, consume
    /// an approval, satisfy installed policy, or authorize any client operation.
    pub fn complete(&mut self, request: &CompleteRequest) -> Result<PublicationOutcome> {
        identifier(&request.publication)?;
        identifier(&request.operation)?;
        validate_id(&request.manifest_digest)?;
        if request.generation <= 0 {
            return Err(Error::Invalid("generation must be positive".into()).into());
        }
        let request_digest =
            digest(&serde_json::to_vec(&(&self.scope, request)).map_err(Error::from)?);
        let mut tx = self.client.transaction()?;
        let current = lock_repository(&mut tx, &self.scope)?;
        let principal = authorize_publication(&mut tx, &self.scope, self.credentials.as_ref())?;
        if let Some(row) = tx.query_opt("SELECT request_digest,outcome,kind FROM team_outcomes WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND operation=$4", &[&self.scope.trust_domain,&self.scope.repository,&request.generation,&request.operation])? {
            if row.get::<_,String>(0) != request_digest || row.get::<_,String>(2) != "publication" {
                return Err(TeamError::Conflict("operation ID reused".into()));
            }
            return parse(&row.get::<_,String>(1));
        }
        if current != request.generation {
            return Err(TeamError::Indeterminate);
        }
        let row = active_publication(
            &mut tx,
            &self.scope,
            request.generation,
            &request.publication,
            principal.as_deref(),
        )?;
        if row.get::<_, String>(1) != "qualifying"
            || row.get::<_, Option<String>>(3).as_deref() != Some(request.manifest_digest.as_str())
        {
            return Err(TeamError::Conflict(
                "publication is not qualified for this manifest".into(),
            ));
        }
        let reservation: ReserveRequest = parse(&row.get::<_, String>(0))?;
        let sequence: i64 = tx.query_one("UPDATE team_repositories SET sequence=sequence+1 WHERE trust_domain=$1 AND repository=$2 RETURNING sequence", &[&self.scope.trust_domain,&self.scope.repository])?.get(0);
        let outcome = PublicationOutcome {
            scope: self.scope.clone(),
            generation: request.generation,
            publication: request.publication.clone(),
            operation: request.operation.clone(),
            root: reservation.root,
            manifest_digest: request.manifest_digest.clone(),
            sequence,
        };
        let encoded = json(&outcome)?;
        tx.execute("UPDATE team_publications SET state='complete' WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&self.scope.trust_domain,&self.scope.repository,&request.generation,&request.publication])?;
        tx.execute(
            "INSERT INTO team_roots VALUES($1,$2,$3,$4,$5)",
            &[
                &self.scope.trust_domain,
                &self.scope.repository,
                &request.generation,
                &request.publication,
                &outcome.root,
            ],
        )?;
        tx.execute(
            "INSERT INTO team_outcomes(trust_domain,repository,generation,operation,request_digest,outcome) VALUES($1,$2,$3,$4,$5,$6)",
            &[
                &self.scope.trust_domain,
                &self.scope.repository,
                &request.generation,
                &request.operation,
                &request_digest,
                &encoded,
            ],
        )?;
        tx.execute(
            "INSERT INTO team_events(trust_domain,repository,sequence,outcome) VALUES($1,$2,$3,$4)",
            &[
                &self.scope.trust_domain,
                &self.scope.repository,
                &sequence,
                &encoded,
            ],
        )?;
        tx.execute(
            "INSERT INTO team_outbox(trust_domain,repository,sequence,payload) VALUES($1,$2,$3,$4)",
            &[
                &self.scope.trust_domain,
                &self.scope.repository,
                &sequence,
                &encoded,
            ],
        )?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn abandon(&mut self, generation: i64, publication: &str) -> Result<()> {
        identifier(publication)?;
        let mut tx = self.client.transaction()?;
        let principal =
            fence_authorized(&mut tx, &self.scope, generation, self.credentials.as_ref())?;
        let row = tx.query_opt("SELECT request,state,owner FROM team_publications WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4 FOR UPDATE", &[&self.scope.trust_domain,&self.scope.repository,&generation,&publication])?.ok_or_else(|| Error::NotFound("publication".into()))?;
        match row.get::<_, String>(1).as_str() {
            "complete" => {
                return Err(TeamError::Conflict(
                    "completed root cannot be abandoned".into(),
                ));
            }
            "abandoned" => return Ok(()),
            _ => {}
        }
        require_owner(
            principal.as_deref(),
            row.get::<_, Option<String>>(2).as_deref(),
        )?;
        let request: ReserveRequest = parse(&row.get::<_, String>(0))?;
        // Once uploading started, bytes may remain durable even if a reply
        // was lost. Retain the conservative charge until authorized deletion
        // is terminal; this increment has no collector.
        if row.get::<_, String>(1) == "reserved" {
            tx.execute("UPDATE team_repositories SET allocated_bytes=allocated_bytes-$3,allocated_objects=allocated_objects-$4 WHERE trust_domain=$1 AND repository=$2", &[&self.scope.trust_domain,&self.scope.repository,&request.byte_limit,&request.object_limit])?;
        }
        tx.execute("UPDATE team_publications SET state='abandoned' WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4", &[&self.scope.trust_domain,&self.scope.repository,&generation,&publication])?;
        tx.commit()?;
        Ok(())
    }
}

fn fence_authorized(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    credentials: Option<&Credentials>,
) -> Result<Option<String>> {
    let current = lock_repository(tx, scope).map_err(|error| match error {
        TeamError::Storage(Error::NotFound(_)) if credentials.is_some() => TeamError::Unauthorized,
        error => error,
    })?;
    let principal = authorize_publication(tx, scope, credentials)?;
    if current != generation {
        return Err(TeamError::Conflict("authority generation rejected".into()));
    }
    Ok(principal)
}

fn authorize_publication(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credentials: Option<&Credentials>,
) -> Result<Option<String>> {
    credentials
        .map(|credentials| {
            auth::authorize(tx, scope, credentials, Some(Role::Contributor), true)
                .map(|state| state.principal)
        })
        .transpose()
}

/// Credentialed callers may mutate only publications they reserved; the
/// trusted operator (no credentials) is not restricted. A row reserved before
/// owners were recorded has none; it keeps the old any-contributor rule until its
/// lease expires, so its contributor can still abandon it and free the quota.
fn require_owner(principal: Option<&str>, owner: Option<&str>) -> Result<()> {
    match (principal, owner) {
        (Some(principal), Some(owner)) if owner != principal => Err(TeamError::Denied),
        _ => Ok(()),
    }
}

pub(super) fn lock_repository(tx: &mut Transaction<'_>, scope: &RepositoryScope) -> Result<i64> {
    let row = tx.query_opt("SELECT generation,retired FROM team_repositories WHERE trust_domain=$1 AND repository=$2 FOR UPDATE", &[&scope.trust_domain,&scope.repository])?.ok_or_else(|| Error::NotFound("repository".into()))?;
    if row.get::<_, bool>(1) {
        return Err(TeamError::Unauthorized);
    }
    Ok(row.get(0))
}

fn active_publication(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    generation: i64,
    publication: &str,
    principal: Option<&str>,
) -> Result<postgres::Row> {
    let row = tx.query_opt("SELECT request,state,expires_at > clock_timestamp(),manifest_digest,source_profile,owner FROM team_publications WHERE trust_domain=$1 AND repository=$2 AND generation=$3 AND publication=$4 FOR UPDATE", &[&scope.trust_domain,&scope.repository,&generation,&publication])?.ok_or_else(|| Error::NotFound("publication".into()))?;
    if row.get::<_, String>(1) != "complete" {
        require_owner(principal, row.get::<_, Option<String>>(5).as_deref())?;
    }
    if row.get::<_, String>(1) == "abandoned" {
        return Err(TeamError::Conflict("publication abandoned".into()));
    }
    if !row.get::<_, bool>(2) {
        return Err(TeamError::Expired);
    }
    Ok(row)
}

fn validate_reservation(request: &ReserveRequest) -> StorageResult<()> {
    identifier(&request.publication)?;
    if request.publication.len() > 96 {
        return Err(Error::Invalid("publication ID exceeds 96 bytes".into()));
    }
    validate_id(&request.root)?;
    if request.generation <= 0
        || !(1..=MAX_PUBLICATION_BYTES).contains(&request.byte_limit)
        || !(1..=MAX_PUBLICATION_OBJECTS).contains(&request.object_limit)
        || !(1..=MAX_LEASE_SECONDS).contains(&request.lease_seconds)
    {
        return Err(Error::Invalid(
            "publication bounds or generation invalid".into(),
        ));
    }
    Ok(())
}

pub(super) fn identifier(value: &str) -> StorageResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Invalid(
            "identifier must be 1..128 portable ASCII bytes".into(),
        ));
    }
    Ok(())
}

pub(super) fn json<T: Serialize>(value: &T) -> StorageResult<String> {
    serde_json::to_string(value).map_err(Error::from)
}
pub(super) fn parse<T: for<'a> Deserialize<'a>>(value: &str) -> Result<T> {
    serde_json::from_str(value)
        .map_err(Error::from)
        .map_err(TeamError::from)
}
