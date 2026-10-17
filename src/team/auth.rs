use std::fmt;

use postgres::Transaction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;

use super::publication::RepositoryScope;
use super::{Result, TeamError};

const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_BYTES: usize = TOKEN_BYTES * 2;
const MAX_SESSION_TTL_SECONDS: i32 = 900;
const MAX_ACTIVE_SESSIONS: i64 = 8;

const CREDENTIAL_DIGEST_DOMAIN: &[u8] = b"agentscm.team.credential.v1\0";
const SESSION_DIGEST_DOMAIN: &[u8] = b"agentscm.team.session.v1\0";
const CSRF_DIGEST_DOMAIN: &[u8] = b"agentscm.team.csrf.v1\0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Reader,
    Contributor,
    Reviewer,
    Integrator,
    Administrator,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Contributor => "contributor",
            Self::Reviewer => "reviewer",
            Self::Integrator => "integrator",
            Self::Administrator => "administrator",
        }
    }

    pub(super) fn from_database(value: &str) -> Result<Self> {
        match value {
            "reader" => Ok(Self::Reader),
            "contributor" => Ok(Self::Contributor),
            "reviewer" => Ok(Self::Reviewer),
            "integrator" => Ok(Self::Integrator),
            "administrator" => Ok(Self::Administrator),
            _ => Err(TeamError::Unavailable),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PrincipalState {
    pub principal: String,
    pub version: i64,
    pub roles: Vec<Role>,
}

#[derive(Clone)]
pub struct Credentials {
    kind: CredentialKind,
}

#[derive(Clone)]
enum CredentialKind {
    Bearer {
        digest: String,
    },
    Browser {
        session_digest: String,
        csrf_digest: Option<String>,
    },
}

impl Credentials {
    pub fn bearer(raw: &str) -> Result<Self> {
        Ok(Self {
            kind: CredentialKind::Bearer {
                digest: credential_digest(raw)?,
            },
        })
    }

    pub(super) fn browser(raw: &str, csrf: Option<&str>) -> Result<Self> {
        let session_digest = token_digest(SESSION_DIGEST_DOMAIN, raw)?;
        let csrf_digest = csrf
            .map(|value| token_digest(CSRF_DIGEST_DOMAIN, value))
            .transpose()?;
        Ok(Self {
            kind: CredentialKind::Browser {
                session_digest,
                csrf_digest,
            },
        })
    }

    pub fn is_browser(&self) -> bool {
        matches!(&self.kind, CredentialKind::Browser { .. })
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            CredentialKind::Bearer { .. } => "bearer",
            CredentialKind::Browser { .. } => "browser",
        };
        f.debug_struct("Credentials")
            .field("kind", &kind)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

pub fn generate_secret() -> Result<Secret> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| TeamError::Unavailable)?;
    Ok(Secret(lower_hex(&bytes)))
}

pub(super) fn credential_digest(raw: &str) -> Result<String> {
    token_digest(CREDENTIAL_DIGEST_DOMAIN, raw)
}

/// Authorize against the current rows in the caller's PostgreSQL transaction.
///
/// Roles are explicit capabilities: one role never implies another. The caller
/// must lock the repository row before calling so authorization observes the
/// same serialized authority generation as the protected operation.
pub(super) fn authorize(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credentials: &Credentials,
    required: Option<Role>,
    mutation: bool,
) -> Result<PrincipalState> {
    let generation = current_generation(tx, scope)?;
    let (principal, version) = match &credentials.kind {
        CredentialKind::Bearer { digest } => tx
            .query_opt(
                "SELECT principal,version FROM team_principals \
                 WHERE trust_domain=$1 AND repository=$2 AND credential_digest=$3 AND enabled \
                 FOR KEY SHARE",
                &[&scope.trust_domain, &scope.repository, digest],
            )?
            .map(|row| (row.get(0), row.get(1)))
            .ok_or(TeamError::Unauthorized)?,
        CredentialKind::Browser {
            session_digest,
            csrf_digest,
        } => tx
            .query_opt(
                "SELECT p.principal,p.version \
                 FROM team_sessions s \
                 JOIN team_principals p ON p.trust_domain=s.trust_domain \
                   AND p.repository=s.repository AND p.principal=s.principal \
                 WHERE s.trust_domain=$1 AND s.repository=$2 AND s.session_digest=$3 \
                   AND NOT s.revoked AND s.expires_at > clock_timestamp() \
                   AND s.authority_generation=$4 AND s.principal_version=p.version AND p.enabled \
                   AND (NOT $5::boolean OR ($6::text IS NOT NULL AND s.csrf_digest=$6)) \
                 FOR KEY SHARE OF s,p",
                &[
                    &scope.trust_domain,
                    &scope.repository,
                    session_digest,
                    &generation,
                    &mutation,
                    csrf_digest,
                ],
            )?
            .map(|row| (row.get(0), row.get(1)))
            .ok_or(TeamError::Unauthorized)?,
    };
    let mut roles = tx
        .query(
            "SELECT role FROM team_roles \
             WHERE trust_domain=$1 AND repository=$2 AND principal=$3 \
             FOR KEY SHARE",
            &[&scope.trust_domain, &scope.repository, &principal],
        )?
        .into_iter()
        .map(|row| Role::from_database(&row.get::<_, String>(0)))
        .collect::<Result<Vec<_>>>()?;
    roles.sort_unstable();
    if required.is_some_and(|role| !roles.contains(&role)) {
        return Err(TeamError::Denied);
    }
    Ok(PrincipalState {
        principal,
        version,
        roles,
    })
}

pub struct NewSession {
    pub session_token: String,
    pub csrf_token: String,
    pub expires_epoch: i64,
    pub principal: PrincipalState,
}

/// Create a short-lived browser session after bearer authentication.
///
/// Session TTL is limited to 1..=900 seconds and each principal may retain at
/// most eight active sessions in a repository. These are admission limits, not
/// guarantees about physical database or disk consumption. The caller must
/// lock the repository row before calling.
pub(super) fn login(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credential: &str,
    generation: i64,
    ttl_seconds: i32,
) -> Result<NewSession> {
    if !(1..=MAX_SESSION_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(Error::Invalid("session TTL must be 1..=900 seconds".into()).into());
    }
    let current = current_generation(tx, scope)?;
    let principal = authorize(tx, scope, &Credentials::bearer(credential)?, None, false)?;
    if current != generation {
        return Err(TeamError::Conflict("authority generation rejected".into()));
    }

    tx.execute(
        "DELETE FROM team_sessions s \
         WHERE s.trust_domain=$1 AND s.repository=$2 \
           AND (s.revoked OR s.expires_at <= clock_timestamp() \
             OR s.authority_generation <> $3 \
             OR NOT EXISTS (SELECT 1 FROM team_principals p \
               WHERE p.trust_domain=s.trust_domain AND p.repository=s.repository \
                 AND p.principal=s.principal AND p.version=s.principal_version))",
        &[&scope.trust_domain, &scope.repository, &current],
    )?;
    let active: i64 = tx
        .query_one(
            "SELECT count(*) FROM team_sessions \
             WHERE trust_domain=$1 AND repository=$2 AND principal=$3",
            &[&scope.trust_domain, &scope.repository, &principal.principal],
        )?
        .get(0);
    if active >= MAX_ACTIVE_SESSIONS {
        return Err(Error::Limit("active session limit exceeded".into()).into());
    }

    for _ in 0..3 {
        let session_token = generate_secret()?;
        let csrf_token = generate_secret()?;
        let session_digest = token_digest(SESSION_DIGEST_DOMAIN, session_token.expose())?;
        let csrf_digest = token_digest(CSRF_DIGEST_DOMAIN, csrf_token.expose())?;
        let row = tx.query_opt(
            "INSERT INTO team_sessions( \
               trust_domain,repository,session_digest,csrf_digest,principal, \
               principal_version,authority_generation,expires_at,revoked) \
             VALUES($1,$2,$3,$4,$5,$6,$7,clock_timestamp()+$8::integer * interval '1 second',FALSE) \
             ON CONFLICT (trust_domain,repository,session_digest) DO NOTHING \
             RETURNING floor(extract(epoch FROM expires_at))::bigint",
            &[
                &scope.trust_domain,
                &scope.repository,
                &session_digest,
                &csrf_digest,
                &principal.principal,
                &principal.version,
                &current,
                &ttl_seconds,
            ],
        )?;
        if let Some(row) = row {
            return Ok(NewSession {
                session_token: session_token.0,
                csrf_token: csrf_token.0,
                expires_epoch: row.get(0),
                principal,
            });
        }
    }
    Err(TeamError::Unavailable)
}

pub(super) fn logout(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credentials: &Credentials,
) -> Result<()> {
    authorize(tx, scope, credentials, None, true)?;
    if !credentials.is_browser() {
        return Err(TeamError::Denied);
    }
    let CredentialKind::Browser { session_digest, .. } = &credentials.kind else {
        return Err(TeamError::Denied);
    };
    let updated = tx.execute(
        "UPDATE team_sessions SET revoked=TRUE \
         WHERE trust_domain=$1 AND repository=$2 AND session_digest=$3",
        &[&scope.trust_domain, &scope.repository, session_digest],
    )?;
    if updated != 1 {
        return Err(TeamError::Unavailable);
    }
    Ok(())
}

fn current_generation(tx: &mut Transaction<'_>, scope: &RepositoryScope) -> Result<i64> {
    tx.query_opt(
        "SELECT generation FROM team_repositories WHERE trust_domain=$1 AND repository=$2 AND NOT retired",
        &[&scope.trust_domain, &scope.repository],
    )?
    .map(|row| row.get(0))
    .ok_or(TeamError::Unauthorized)
}

pub(crate) fn token_digest(domain: &[u8], raw: &str) -> Result<String> {
    validate_token(raw)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(raw.as_bytes());
    Ok(lower_hex(&hasher.finalize()))
}

fn validate_token(raw: &str) -> Result<()> {
    if raw.len() != TOKEN_HEX_BYTES
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TeamError::Unauthorized);
    }
    Ok(())
}

pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
