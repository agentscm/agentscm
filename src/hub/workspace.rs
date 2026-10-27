//! Opt-in single-workspace membership, browser sessions and settings.
//!
//! Forge identities remain in the Hub identity store. This store contains only
//! workspace grants and session digests. Roles grant independent capabilities.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::http::{ApiError, JsonBody, bearer, blocking};
use super::login::random_hex;
use super::promise::{
    PromiseChangeActionV1, PromiseChangeV1, PromiseCurrentAdmissionsV1, PromiseForgeAdmissionV1,
    PromiseRepositoryAdmissionV1, PromiseVersionV1,
};
#[cfg(unix)]
pub(crate) use super::workspace_execution::ExecutionService as PromiseExecution;
#[cfg(not(unix))]
pub(crate) struct PromiseExecution;
#[cfg(not(unix))]
impl PromiseExecution {
    fn promise_native_keys(&self) -> Vec<String> {
        Vec::new()
    }
    fn promise_native_repository_ids(&self, _: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}
use super::store::{ForgeIdentity, Store, TokenDigest, User, agent_token_digest, unix_now};
use super::{HubError, HubState, Result};
use crate::team::auth::{Role, token_digest};

const COOKIE: &str = "__Host-agentscm_workspace";
const SESSION_DOMAIN: &[u8] = b"agentscm.workspace.session.v1\0";
const CSRF_DOMAIN: &[u8] = b"agentscm.workspace.csrf.v1\0";
const SESSION_SECONDS: i64 = 900;
const MAX_MEMBERS: i64 = 1000;
const MAX_SESSIONS: i64 = 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub provider: String,
    pub host: String,
    pub user_id: i64,
}

impl Identity {
    pub fn forge(&self) -> ForgeIdentity {
        ForgeIdentity {
            provider: self.provider.clone(),
            host: self.host.clone(),
            user_id: self.user_id,
        }
    }

    fn key(&self) -> Result<String> {
        self.forge().validate()?;
        Ok(format!("{}|{}|{}", self.provider, self.host, self.user_id))
    }
}

impl From<&ForgeIdentity> for Identity {
    fn from(value: &ForgeIdentity) -> Self {
        Self {
            provider: value.provider.clone(),
            host: value.host.clone(),
            user_id: value.user_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub workspace_id: String,
    pub name: String,
    pub database: PathBuf,
    pub initial_owner: Identity,
    /// Optional operator-controlled grant-key to stable forge repository mapping.
    #[serde(default)]
    pub forge_repositories: Vec<WorkspaceForgeRepositoryV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceForgeRepositoryV1 {
    pub repository_id: String,
    pub provider: String,
    pub host: String,
    pub forge_repository_id: i64,
}

impl WorkspaceConfig {
    /// Operator-selected startup file. No request may supply its path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err(invalid("invalid workspace configuration file"));
        }
        serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|_| invalid("invalid workspace configuration"))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub id: String,
    pub name: String,
    pub generation: i64,
    pub max_concurrent_runs: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Member {
    pub identity: Identity,
    pub roles: Vec<Role>,
    pub repository_ids: Vec<String>,
    pub enabled: bool,
    pub generation: i64,
}

/// Opaque authenticated actor. Deserialize only from trusted controller records,
/// never from an HTTP request body. The stored digest cannot act as a bearer.
#[derive(Clone, Deserialize, Serialize)]
pub struct WorkspaceActor {
    workspace_id: String,
    identity: Identity,
    user_id: String,
    generation: i64,
    credential: Credential,
}

#[derive(Clone, Deserialize, Serialize)]
enum Credential {
    Browser {
        digest: String,
    },
    Agent {
        digest: String,
        #[serde(default)]
        token_id: Option<String>,
        repository: String,
        read_only: bool,
    },
}

impl WorkspaceActor {
    pub fn generation(&self) -> i64 {
        self.generation
    }
    pub fn identity(&self) -> ForgeIdentity {
        self.identity.forge()
    }
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn is_browser(&self) -> bool {
        matches!(self.credential, Credential::Browser { .. })
    }

    pub(crate) fn token_id(&self) -> Option<&str> {
        match &self.credential {
            Credential::Browser { .. } => None,
            Credential::Agent { token_id, .. } => token_id.as_deref(),
        }
    }

    pub(crate) fn agent_identity(
        &self,
    ) -> Result<crate::skill_workflow::AuthenticatedAgentIdentity> {
        let Credential::Agent { digest, .. } = &self.credential else {
            return Err(forbidden());
        };
        Ok(crate::skill_workflow::AuthenticatedAgentIdentity {
            owner_id: self.user_id.clone(),
            agent_id: digest.clone(),
        })
    }

    pub(crate) fn authorization_identity(
        &self,
    ) -> crate::skill_workflow::AuthenticatedAgentIdentity {
        let agent_id = match &self.credential {
            Credential::Browser { .. } => "workspace-browser".to_owned(),
            Credential::Agent { digest, .. } => digest.clone(),
        };
        crate::skill_workflow::AuthenticatedAgentIdentity {
            owner_id: self.user_id.clone(),
            agent_id,
        }
    }
}

#[derive(Clone, Copy)]
pub enum WorkspaceAction {
    ReadRepository,
    ExecuteWorkflow,
    ApproveRevision,
    IntegrateRevision,
}

pub(crate) struct AgentTaskAuthorization<'a> {
    pub actor: &'a WorkspaceActor,
    pub repository: &'a str,
    pub core_actor: &'a crate::skill_workflow::AuthenticatedAgentIdentity,
    pub action: crate::skill_workflow::CollectorAction,
    pub origins: &'a [crate::skill_workflow::OriginGrantBinding],
    pub audited: Option<(&'a str, &'a Value)>,
}

pub struct Workspace {
    conn: Mutex<Connection>,
    store: Arc<dyn Store>,
    id: String,
    forge_repositories: BTreeMap<String, PromiseForgeAdmissionV1>,
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}
fn forbidden() -> HubError {
    invalid("workspace_forbidden")
}
fn conflict() -> HubError {
    invalid("workspace_generation_conflict")
}

fn json_text(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| HubError::Unavailable("workspace encoding".into()))
}
fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text)
        .map_err(|_| HubError::Unavailable("workspace record encoding".into()))
}
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 100 && !name.chars().any(char::is_control)
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
}

fn settings(conn: &Connection) -> Result<Settings> {
    Ok(conn.query_row(
        "SELECT id,name,generation,max_concurrent_runs FROM workspace_meta WHERE singleton=1",
        [],
        |row| {
            Ok(Settings {
                id: row.get(0)?,
                name: row.get(1)?,
                generation: row.get(2)?,
                max_concurrent_runs: row.get(3)?,
            })
        },
    )?)
}
fn member(conn: &Connection, key: &str) -> Result<Option<Member>> {
    let raw: Option<(String,String,String,bool,i64)> = conn.query_row(
        "SELECT identity,roles,repository_ids,enabled,generation FROM workspace_members WHERE identity_key=?1", [key],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional()?;
    raw.map(|(identity, roles, repositories, enabled, generation)| {
        Ok(Member {
            identity: decode(&identity)?,
            roles: decode(&roles)?,
            repository_ids: decode(&repositories)?,
            enabled,
            generation,
        })
    })
    .transpose()
}
fn audit(conn: &Connection, actor: &str, kind: &str, data: &impl Serialize) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_audit(at,actor,kind,data) VALUES(?1,?2,?3,?4)",
        params![unix_now(), actor, kind, json_text(data)?],
    )?;
    Ok(())
}

impl Workspace {
    pub fn open(config: WorkspaceConfig, store: Arc<dyn Store>) -> Result<Self> {
        if !valid_id(&config.workspace_id)
            || !valid_name(&config.name)
            || !config.database.is_absolute()
        {
            return Err(invalid(
                "workspace requires an id, name and absolute database path",
            ));
        }
        if config.forge_repositories.len() > 100 {
            return Err(invalid("too many workspace forge repositories"));
        }
        let mut forge_repositories = BTreeMap::new();
        let mut forge_identities = BTreeSet::new();
        for entry in &config.forge_repositories {
            if !valid_id(&entry.repository_id) || entry.forge_repository_id <= 0 {
                return Err(invalid("invalid workspace forge repository binding"));
            }
            let admission = PromiseForgeAdmissionV1 {
                provider: entry.provider.clone(),
                host: entry.host.clone(),
                forge_repository_id: entry.forge_repository_id,
            };
            if !forge_identities.insert((
                admission.provider.clone(),
                admission.host.clone(),
                admission.forge_repository_id,
            )) || forge_repositories
                .insert(entry.repository_id.clone(), admission.clone())
                .is_some()
            {
                return Err(invalid("duplicate workspace forge repository binding"));
            }
            let repository = store
                .repository_by_id(&admission.host, admission.forge_repository_id)?
                .ok_or_else(|| invalid("workspace forge repository is not connected"))?;
            let connection = store
                .connection(&repository.connection)?
                .ok_or_else(|| invalid("workspace forge connection is unavailable"))?;
            if connection.provider != admission.provider || connection.host != admission.host {
                return Err(invalid(
                    "workspace forge repository provider differs from binding",
                ));
            }
        }
        let owner = config.initial_owner.key()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&config.database)?;
        }
        let mut conn = Connection::open(&config.database)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS workspace_meta (
          singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL, name TEXT NOT NULL,
          generation INTEGER NOT NULL, max_concurrent_runs INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS workspace_members (
          identity_key TEXT PRIMARY KEY, identity TEXT NOT NULL, roles TEXT NOT NULL,
          repository_ids TEXT NOT NULL, enabled INTEGER NOT NULL, generation INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS workspace_sessions (
          digest TEXT PRIMARY KEY, user_id TEXT NOT NULL, identity_key TEXT NOT NULL,
          csrf_digest TEXT NOT NULL, expires_at INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS workspace_requests (
          request_id TEXT PRIMARY KEY, actor TEXT NOT NULL, fingerprint TEXT NOT NULL, generation INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS workspace_audit (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL);")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let initialized: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_meta)", [], |row| {
                row.get(0)
            })?;
        if !initialized {
            tx.execute(
                "INSERT INTO workspace_meta VALUES(1,?1,?2,1,1)",
                params![config.workspace_id, config.name],
            )?;
            let roles = vec![Role::Administrator, Role::Reader, Role::Contributor];
            tx.execute(
                "INSERT INTO workspace_members VALUES(?1,?2,?3,'[\"*\"]',1,1)",
                params![owner, json_text(&config.initial_owner)?, json_text(&roles)?],
            )?;
            audit(
                &tx,
                "operator",
                "workspace_created",
                &json!({"owner":config.initial_owner}),
            )?;
        } else if settings(&tx)?.id != config.workspace_id {
            return Err(invalid(
                "workspace database belongs to a different workspace",
            ));
        }
        // Owner/name are seeds only. Restarting never restores a revoked grant.
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
            store,
            id: config.workspace_id,
            forge_repositories,
        })
    }

    /// The current operator config is the sole mapping authority. Revalidate the connected
    /// repository by stable forge identity on every use; names never authorize a promise.
    pub(crate) fn promise_forge_admission(
        &self,
        repository_id: &str,
    ) -> Result<Option<PromiseForgeAdmissionV1>> {
        let Some(admission) = self.forge_repositories.get(repository_id) else {
            return Ok(None);
        };
        let repository = self
            .store
            .repository_by_id(&admission.host, admission.forge_repository_id)?
            .ok_or_else(forbidden)?;
        let connection = self
            .store
            .connection(&repository.connection)?
            .ok_or_else(forbidden)?;
        if connection.provider != admission.provider || connection.host != admission.host {
            return Err(forbidden());
        }
        Ok(Some(admission.clone()))
    }

    pub(crate) fn promise_forge_repository_ids(&self) -> impl Iterator<Item = &String> {
        self.forge_repositories.keys()
    }

    /// A snapshot of current operator-admitted identities. Only configured keys are stored;
    /// absent keys resolve to explicit unbound admissions in the promise domain.
    pub(crate) fn promise_current_admissions(
        &self,
        execution: Option<&PromiseExecution>,
    ) -> Result<PromiseCurrentAdmissionsV1> {
        let mut keys = self
            .promise_forge_repository_ids()
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(execution) = execution {
            keys.extend(execution.promise_native_keys());
        }
        let mut current = PromiseCurrentAdmissionsV1::new();
        for key in keys {
            let admission = PromiseRepositoryAdmissionV1 {
                repository_id: key.clone(),
                forge: self.promise_forge_admission(&key)?,
                native_repository_ids: execution
                    .map(|service| service.promise_native_repository_ids(&key))
                    .transpose()?
                    .unwrap_or_default(),
            };
            admission.validate()?;
            current.insert(key, admission);
        }
        Ok(current)
    }

    pub(crate) fn promise_admissions_for(
        &self,
        repositories: &[String],
        execution: Option<&PromiseExecution>,
    ) -> Result<Vec<PromiseRepositoryAdmissionV1>> {
        let current = self.promise_current_admissions(execution)?;
        let mut admitted = repositories
            .iter()
            .map(|key| super::promise::current_admission(&current, key))
            .collect::<Vec<_>>();
        admitted.sort_by(|a, b| a.repository_id.cmp(&b.repository_id));
        Ok(admitted)
    }

    /// An agent can attest a forge scope only when its live token has the same numeric binding.
    pub(crate) fn promise_agent_forge_scope(
        &self,
        actor: &WorkspaceActor,
        repository_id: &str,
        admission: &Option<PromiseForgeAdmissionV1>,
    ) -> Result<()> {
        let Credential::Agent { digest, .. } = &actor.credential else {
            return Ok(());
        };
        let principal = self
            .store
            .authenticate(&TokenDigest::from_persisted(digest)?, unix_now())?
            .ok_or_else(forbidden)?;
        let source_matches = match admission {
            Some(forge) => {
                principal.owner.identity.provider == forge.provider
                    && principal.owner.identity.host == forge.host
                    && principal.repository_id == Some(forge.forge_repository_id)
            }
            None => principal.repository_id.is_none(),
        };
        if principal.owner.id != actor.user_id
            || principal.repository != repository_id
            || !source_matches
        {
            return Err(forbidden());
        }
        Ok(())
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| HubError::Unavailable("workspace lock poisoned".into()))
    }

    pub fn settings(&self) -> Result<Settings> {
        settings(&*self.conn()?)
    }

    pub fn allows_sign_in(&self, identity: &ForgeIdentity) -> Result<bool> {
        Ok(member(&*self.conn()?, &Identity::from(identity).key()?)?.is_some_and(|m| m.enabled))
    }

    fn current(&self, conn: &Connection, actor: &WorkspaceActor) -> Result<(User, Member)> {
        if actor.workspace_id != self.id {
            return Err(HubError::Unauthorized);
        }
        let user = self
            .store
            .user_by_id(&actor.user_id)?
            .filter(|u| !u.disabled && Identity::from(&u.identity) == actor.identity)
            .ok_or(HubError::Unauthorized)?;
        let member = member(conn, &actor.identity.key()?)?
            .filter(|m| m.enabled && m.generation == actor.generation)
            .ok_or(HubError::Unauthorized)?;
        match &actor.credential {
            Credential::Browser { digest } => {
                let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM workspace_sessions WHERE digest=?1 AND user_id=?2 AND identity_key=?3 AND expires_at>?4)",params![digest,user.id,actor.identity.key()?,unix_now()],|row|row.get(0))?;
                if !valid {
                    return Err(HubError::Unauthorized);
                }
            }
            Credential::Agent {
                digest,
                token_id,
                repository,
                read_only,
            } => {
                let principal = self
                    .store
                    .authenticate(&TokenDigest::from_persisted(digest)?, unix_now())?
                    .ok_or(HubError::Unauthorized)?;
                if principal.owner.id != user.id
                    || token_id
                        .as_deref()
                        .is_some_and(|token_id| principal.token_id != token_id)
                    || principal.repository != *repository
                    || principal.read_only != *read_only
                {
                    return Err(HubError::Unauthorized);
                }
            }
        }
        Ok((user, member))
    }

    pub fn recheck(&self, actor: &WorkspaceActor) -> Result<()> {
        self.current(&*self.conn()?, actor).map(|_| ())
    }

    fn authorized(
        &self,
        conn: &Connection,
        actor: &WorkspaceActor,
        repository: &str,
        action: WorkspaceAction,
    ) -> Result<()> {
        let (_, member) = self.current(conn, actor)?;
        if !valid_id(repository)
            || !member
                .repository_ids
                .iter()
                .any(|id| id == repository || id == "*")
        {
            return Err(forbidden());
        }
        let required = match action {
            WorkspaceAction::ReadRepository => Role::Reader,
            WorkspaceAction::ExecuteWorkflow => Role::Contributor,
            WorkspaceAction::ApproveRevision => Role::Reviewer,
            WorkspaceAction::IntegrateRevision => Role::Integrator,
        };
        if !member.roles.contains(&required) {
            return Err(forbidden());
        }
        if let Credential::Agent {
            repository: scope,
            read_only,
            ..
        } = &actor.credential
            && (scope != repository
                || (!matches!(action, WorkspaceAction::ReadRepository)
                    && (*read_only || !matches!(action, WorkspaceAction::ExecuteWorkflow))))
        {
            return Err(forbidden());
        }
        Ok(())
    }

    pub fn authorize(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        action: WorkspaceAction,
    ) -> Result<()> {
        self.authorized(&*self.conn()?, actor, repository, action)
    }

    /// Membership/session changes and global Hub disables serialize with finalization.
    /// Lock ordering: execution queue -> workspace -> identity store. Management never acquires the execution queue.
    pub fn with_authorization<T>(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        action: WorkspaceAction,
        operation: impl FnOnce() -> crate::Result<T>,
    ) -> Result<T> {
        let conn = self.conn()?;
        self.authorized(&conn, actor, repository, action)?;
        operation().map_err(HubError::from)
    }

    pub(crate) fn workspace_id(&self) -> &str {
        &self.id
    }

    fn promise_authorized(
        &self,
        conn: &Connection,
        actor: &WorkspaceActor,
        repositories: &[String],
        reader: bool,
        contributor: bool,
        browser: bool,
    ) -> Result<(User, Member)> {
        let (user, member) = self.current(conn, actor)?;
        if browser && !actor.is_browser() {
            return Err(forbidden());
        }
        if repositories.is_empty()
            || repositories.iter().any(|repository| {
                !valid_id(repository)
                    || !member
                        .repository_ids
                        .iter()
                        .any(|grant| grant == repository || grant == "*")
            })
            || (reader && !member.roles.contains(&Role::Reader))
            || (contributor && !member.roles.contains(&Role::Contributor))
        {
            return Err(forbidden());
        }
        if let Credential::Agent {
            repository,
            read_only,
            ..
        } = &actor.credential
            && (repositories.iter().any(|scope| scope != repository) || (contributor && *read_only))
        {
            return Err(forbidden());
        }
        if matches!(&actor.credential, Credential::Agent { .. }) {
            for repository in repositories {
                let forge = self.promise_forge_admission(repository)?;
                self.promise_agent_forge_scope(actor, repository, &forge)?;
            }
        }
        Ok((user, member))
    }

    pub(crate) fn with_promise_list<T>(
        &self,
        actor: &WorkspaceActor,
        operation: impl FnOnce(Option<&[String]>) -> Result<T>,
    ) -> Result<T> {
        let conn = self.conn()?;
        let (_, member) = self.current(&conn, actor)?;
        if !member.roles.contains(&Role::Reader) {
            return Err(forbidden());
        }
        match &actor.credential {
            Credential::Agent { repository, .. } => {
                if !member
                    .repository_ids
                    .iter()
                    .any(|grant| grant == repository || grant == "*")
                {
                    return Err(forbidden());
                }
                let forge = self.promise_forge_admission(repository)?;
                self.promise_agent_forge_scope(actor, repository, &forge)?;
                let repositories = [repository.clone()];
                operation(Some(&repositories))
            }
            Credential::Browser { .. } if member.repository_ids.iter().any(|id| id == "*") => {
                operation(None)
            }
            Credential::Browser { .. } => operation(Some(&member.repository_ids)),
        }
    }

    pub(crate) fn with_promise_create<T>(
        &self,
        actor: &WorkspaceActor,
        repositories: &[String],
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.promise_authorized(&tx, actor, repositories, true, true, true)?;
        let result = operation()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn promise_change_capabilities(
        &self,
        actor: &WorkspaceActor,
        version: &crate::hub::promise::PromiseVersionV1,
    ) -> Result<crate::hub::promise::PromiseChangeCapabilitiesV1> {
        let conn = self.conn()?;
        let repositories = version
            .definition
            .repositories
            .iter()
            .map(|repository| repository.repository_id.clone())
            .collect::<Vec<_>>();
        let (user, member) =
            self.promise_authorized(&conn, actor, &repositories, true, false, true)?;
        let active = version.state == crate::hub::promise::PromiseStateV1::Active;
        let contributor = member.roles.contains(&Role::Contributor);
        let owner = active && contributor && user.id == version.steward_user;
        let recovery = active
            && contributor
            && user.id != version.steward_user
            && member.roles.contains(&Role::Administrator)
            && !self.promise_user_eligible(&conn, &version.steward_user, &repositories)?;
        Ok(crate::hub::promise::PromiseChangeCapabilitiesV1 {
            amend: owner,
            correct: owner,
            retire: owner,
            transfer: owner,
            recovery_transfer: recovery,
        })
    }

    pub(crate) fn with_promise_proposal<T>(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.promise_authorized(&tx, actor, &[repository.to_owned()], false, true, false)?;
        let result = operation()?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn with_promise_report<T>(
        &self,
        actor: &WorkspaceActor,
        promise_repositories: &[String],
        repository: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.promise_authorized(&tx, actor, promise_repositories, true, true, false)?;
        if !promise_repositories.iter().any(|scope| scope == repository) {
            return Err(forbidden());
        }
        let result = operation()?;
        tx.commit()?;
        Ok(result)
    }

    fn promise_user_eligible(
        &self,
        conn: &Connection,
        user_id: &str,
        repositories: &[String],
    ) -> Result<bool> {
        let Some(user) = self
            .store
            .user_by_id(user_id)?
            .filter(|user| !user.disabled)
        else {
            return Ok(false);
        };
        let Some(member) =
            member(conn, &Identity::from(&user.identity).key()?)?.filter(|member| member.enabled)
        else {
            return Ok(false);
        };
        Ok(member.roles.contains(&Role::Reader)
            && member.roles.contains(&Role::Contributor)
            && repositories.iter().all(|repository| {
                member
                    .repository_ids
                    .iter()
                    .any(|grant| grant == repository || grant == "*")
            }))
    }

    pub(crate) fn with_promise_change<T>(
        &self,
        actor: &WorkspaceActor,
        current: &PromiseVersionV1,
        request: &PromiseChangeV1,
        replay: impl FnOnce() -> Result<Option<T>>,
        operation: impl FnOnce(crate::hub::promise::PromiseChangeAuthority, &[String]) -> Result<T>,
    ) -> Result<T> {
        let old_repositories = current
            .definition
            .repositories
            .iter()
            .map(|repository| repository.repository_id.clone())
            .collect::<Vec<_>>();
        let new_repositories = match &request.action {
            PromiseChangeActionV1::Amend { content }
            | PromiseChangeActionV1::Correct { content } => content
                .repositories
                .iter()
                .map(|repository| repository.repository_id.clone())
                .collect::<Vec<_>>(),
            _ => old_repositories.clone(),
        };
        let target_user = match &request.action {
            PromiseChangeActionV1::Transfer { to_user }
            | PromiseChangeActionV1::RecoveryTransfer { to_user } => Some(to_user.as_str()),
            _ => None,
        };
        let recovery = matches!(
            &request.action,
            PromiseChangeActionV1::RecoveryTransfer { .. }
        );
        let current_steward = current.steward_user.as_str();
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Receipt recovery is narrower than a new owner action. Current read access and a
        // live browser session are required even if the original steward has transferred.
        self.promise_authorized(&tx, actor, &old_repositories, true, false, true)?;
        if let Some(receipt) = replay()? {
            tx.commit()?;
            return Ok(receipt);
        }
        let mut repositories = old_repositories.to_vec();
        repositories.extend_from_slice(&new_repositories);
        repositories.sort();
        repositories.dedup();
        let (user, member) =
            self.promise_authorized(&tx, actor, &repositories, true, true, true)?;
        let authority = if recovery {
            if user.id == current_steward
                || !member.roles.contains(&Role::Administrator)
                || self.promise_user_eligible(&tx, current_steward, &old_repositories)?
            {
                return Err(forbidden());
            }
            crate::hub::promise::PromiseChangeAuthority::RecoveryAdministrator
        } else {
            if user.id != current_steward {
                return Err(forbidden());
            }
            crate::hub::promise::PromiseChangeAuthority::Steward
        };
        if let Some(target) = target_user
            && !self.promise_user_eligible(&tx, target, &new_repositories)?
        {
            return Err(forbidden());
        }
        let result = operation(authority, &new_repositories)?;
        tx.commit()?;
        Ok(result)
    }

    /// Hold one workspace lock while checking the initiating credential/action and every
    /// originating agent grant. The callback may acquire the workflow journal and native
    /// authority in that order; it must not re-enter workspace authorization.
    pub(crate) fn with_agent_task_authorization<T>(
        &self,
        binding: AgentTaskAuthorization<'_>,
        operation: impl FnOnce() -> crate::Result<T>,
    ) -> Result<T> {
        let workspace_action = match binding.action {
            crate::skill_workflow::CollectorAction::Submit
            | crate::skill_workflow::CollectorAction::Finalize
            | crate::skill_workflow::CollectorAction::PublishCandidate => {
                WorkspaceAction::ExecuteWorkflow
            }
            crate::skill_workflow::CollectorAction::Approve => WorkspaceAction::ApproveRevision,
            crate::skill_workflow::CollectorAction::Integrate => WorkspaceAction::IntegrateRevision,
        };
        let conn = self.conn()?;
        self.authorized(&conn, binding.actor, binding.repository, workspace_action)?;
        if &binding.actor.authorization_identity() != binding.core_actor {
            return Err(HubError::Unauthorized);
        }
        for origin in binding.origins {
            if origin.grant_id != origin.actor.owner_id {
                return Err(forbidden());
            }
            let digest = TokenDigest::from_persisted(&origin.actor.agent_id)?;
            let principal = self
                .store
                .authenticate(&digest, unix_now())?
                .filter(|principal| {
                    principal.owner.id == origin.actor.owner_id
                        && principal.repository == binding.repository
                        && !principal.read_only
                })
                .ok_or(HubError::Unauthorized)?;
            let identity = Identity::from(&principal.owner.identity);
            member(&conn, &identity.key()?)?
                .filter(|member| {
                    member.enabled
                        && u64::try_from(member.generation).ok() == Some(origin.grant_generation)
                        && member.roles.contains(&Role::Contributor)
                        && member
                            .repository_ids
                            .iter()
                            .any(|id| id == binding.repository || id == "*")
                })
                .ok_or_else(forbidden)?;
        }
        if matches!(
            binding.action,
            crate::skill_workflow::CollectorAction::Approve
                | crate::skill_workflow::CollectorAction::Integrate
        ) {
            let (request_id, subject) = binding.audited.ok_or_else(forbidden)?;
            if !valid_id(request_id) {
                return Err(invalid("invalid request id"));
            }
            let action_name = if binding.action == crate::skill_workflow::CollectorAction::Approve {
                "approve_revision"
            } else {
                "integrate_revision"
            };
            let data = json!({"request_id":request_id,"repository":binding.repository,"action":action_name,"subject":subject});
            audit(
                &conn,
                &binding.actor.identity.key()?,
                "native_action_started",
                &data,
            )?;
            let outcome = operation().map_err(HubError::from);
            audit(
                &conn,
                &binding.actor.identity.key()?,
                if outcome.is_ok() {
                    "native_action_succeeded"
                } else {
                    "native_action_failed"
                },
                &data,
            )?;
            outcome
        } else {
            if binding.audited.is_some() {
                return Err(forbidden());
            }
            operation().map_err(HubError::from)
        }
    }

    /// Native authority retries retain their own exact-subject idempotency.
    /// This audit records each attempted action and observed completion without
    /// pretending the two SQLite databases form one transaction.
    pub fn with_audited_authorization<T>(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        action: WorkspaceAction,
        request_id: &str,
        subject: &impl Serialize,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if !valid_id(request_id) {
            return Err(invalid("invalid request id"));
        }
        let action_name = match action {
            WorkspaceAction::ApproveRevision => "approve_revision",
            WorkspaceAction::IntegrateRevision => "integrate_revision",
            _ => return Err(forbidden()),
        };
        let conn = self.conn()?;
        self.authorized(&conn, actor, repository, action)?;
        let data = json!({"request_id":request_id,"repository":repository,"action":action_name,"subject":subject});
        audit(
            &conn,
            &actor.identity.key()?,
            "native_action_started",
            &data,
        )?;
        let outcome = operation();
        audit(
            &conn,
            &actor.identity.key()?,
            if outcome.is_ok() {
                "native_action_succeeded"
            } else {
                "native_action_failed"
            },
            &data,
        )?;
        outcome
    }

    pub(super) fn with_identity_change<T>(
        &self,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _conn = self.conn()?;
        operation()
    }

    pub(super) fn with_user_change<T>(
        &self,
        actor_id: &str,
        user_id: &str,
        disabled: bool,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.conn()?;
        let acting_user = self
            .store
            .user_by_id(actor_id)?
            .filter(|user| !user.disabled)
            .ok_or(HubError::Unauthorized)?;
        let acting_identity = Identity::from(&acting_user.identity).key()?;
        let user = self.store.user_by_id(user_id)?;
        if disabled && let Some(user) = &user {
            let target = member(&conn, &Identity::from(&user.identity).key()?)?;
            if !user.disabled
                && target.is_some_and(|m| m.enabled && m.roles.contains(&Role::Administrator))
                && self.enabled_administrators(&conn)? <= 1
            {
                return Err(invalid("workspace_last_administrator"));
            }
        }
        // Revoke first, while excluding new sessions/finalization. If the
        // identity-store update fails, old browser sessions still stay revoked.
        if disabled {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM workspace_sessions WHERE user_id=?1", [user_id])?;
            if let Some(user) = &user {
                let identity = Identity::from(&user.identity);
                let key = identity.key()?;
                if member(&tx, &key)?.is_some() {
                    // Fence retained agent actors as well: re-enabling the Hub
                    // user must not resurrect a run authorized before disable.
                    let generation = settings(&tx)?.generation + 1;
                    tx.execute(
                        "UPDATE workspace_members SET generation=?1 WHERE identity_key=?2",
                        params![generation, key],
                    )?;
                    tx.execute(
                        "UPDATE workspace_meta SET generation=?1 WHERE singleton=1",
                        [generation],
                    )?;
                    audit(
                        &tx,
                        &acting_identity,
                        "user_sessions_revoked",
                        &json!({"identity":identity,"generation":generation}),
                    )?;
                }
            }
            tx.commit()?;
        }
        operation()
    }

    fn eligible_identity(&self, identity: &Identity) -> Result<bool> {
        // Only a known enabled Hub identity can preserve administrative access.
        // A pending numeric grant may be a typo or an account that cannot sign in.
        Ok(self
            .store
            .user_for_identity(&identity.forge())?
            .is_some_and(|user| !user.disabled))
    }

    fn enabled_administrators(&self, conn: &Connection) -> Result<usize> {
        let mut query=conn.prepare("SELECT identity FROM workspace_members WHERE enabled=1 AND EXISTS(SELECT 1 FROM json_each(roles) WHERE value='administrator') LIMIT 1001")?;
        let identities = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if identities.len() > MAX_MEMBERS as usize {
            return Err(HubError::Unavailable(
                "workspace member bound exceeded".into(),
            ));
        }
        let mut count = 0;
        for text in identities {
            if self.eligible_identity(&decode::<Identity>(&text)?)? {
                count += 1;
            }
        }
        Ok(count)
    }

    pub(super) fn issue_session(&self, user: &User) -> Result<String> {
        let raw = random_hex()?;
        let digest = session_digest(&raw)?;
        let csrf = csrf_token(&raw);
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fresh = self
            .store
            .user_by_id(&user.id)?
            .filter(|u| !u.disabled && u.identity == user.identity)
            .ok_or(HubError::Unauthorized)?;
        let key = Identity::from(&fresh.identity).key()?;
        if !member(&tx, &key)?.is_some_and(|m| m.enabled) {
            return Err(forbidden());
        }
        let now = unix_now();
        tx.execute("DELETE FROM workspace_sessions WHERE expires_at<=?1", [now])?;
        // Bound both global and per-identity live sessions; oldest expires first.
        tx.execute("DELETE FROM workspace_sessions WHERE digest IN (SELECT digest FROM workspace_sessions WHERE identity_key=?1 ORDER BY expires_at DESC,digest DESC LIMIT -1 OFFSET 7)",[&key])?;
        tx.execute("DELETE FROM workspace_sessions WHERE digest IN (SELECT digest FROM workspace_sessions ORDER BY expires_at DESC,digest DESC LIMIT -1 OFFSET ?1)",[MAX_SESSIONS-1])?;
        tx.execute(
            "INSERT INTO workspace_sessions VALUES(?1,?2,?3,?4,?5)",
            params![
                digest,
                user.id,
                key,
                csrf_digest(&csrf)?,
                now + SESSION_SECONDS
            ],
        )?;
        audit(
            &tx,
            &key,
            "session_created",
            &json!({"expires_at":now+SESSION_SECONDS}),
        )?;
        tx.commit()?;
        Ok(cookie(&raw, SESSION_SECONDS))
    }

    fn actor_for_cookie(
        &self,
        raw: &str,
        csrf: Option<&str>,
        mutation: bool,
    ) -> Result<WorkspaceActor> {
        let digest = session_digest(raw)?;
        let conn = self.conn()?;
        let row: Option<(String,String,String)> = conn.query_row("SELECT user_id,identity_key,csrf_digest FROM workspace_sessions WHERE digest=?1 AND expires_at>?2",params![digest,unix_now()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let (user_id, key, expected_csrf) = row.ok_or(HubError::Unauthorized)?;
        if mutation && csrf.map(csrf_digest).transpose()?.as_ref() != Some(&expected_csrf) {
            return Err(invalid("workspace_csrf"));
        }
        let member = member(&conn, &key)?
            .filter(|m| m.enabled)
            .ok_or(HubError::Unauthorized)?;
        let actor = WorkspaceActor {
            workspace_id: self.id.clone(),
            identity: member.identity,
            user_id,
            generation: member.generation,
            credential: Credential::Browser { digest },
        };
        self.current(&conn, &actor)?;
        Ok(actor)
    }

    fn actor_for_agent(&self, raw: &str, mutation: bool) -> Result<WorkspaceActor> {
        let digest = agent_token_digest(raw)?;
        let principal = self
            .store
            .authenticate(&digest, unix_now())?
            .ok_or(HubError::Unauthorized)?;
        // Web-page tokens minted before the separate digest domain used this
        // reserved name. Deny them here until their 30-minute lifetime ends.
        // An agent choosing this name is denied Workspace access, never granted it.
        if principal.name == "hub-web" {
            return Err(HubError::Unauthorized);
        }
        if mutation && principal.read_only {
            return Err(forbidden());
        }
        let conn = self.conn()?;
        let identity = Identity::from(&principal.owner.identity);
        let member = member(&conn, &identity.key()?)?
            .filter(|m| m.enabled)
            .ok_or(HubError::Unauthorized)?;
        let actor = WorkspaceActor {
            workspace_id: self.id.clone(),
            identity,
            user_id: principal.owner.id,
            generation: member.generation,
            credential: Credential::Agent {
                digest: digest.as_str().to_owned(),
                token_id: Some(principal.token_id),
                repository: principal.repository,
                read_only: principal.read_only,
            },
        };
        self.current(&conn, &actor)?;
        Ok(actor)
    }

    /// Resolve the MCP bearer through the same live workspace actor check as HTTP.
    pub(crate) fn actor_for_promise_agent(
        &self,
        raw: &str,
        mutation: bool,
    ) -> Result<WorkspaceActor> {
        self.actor_for_agent(raw, mutation)
    }

    fn require_admin(&self, conn: &Connection, actor: &WorkspaceActor) -> Result<Member> {
        let (_, member) = self.current(conn, actor)?;
        if !actor.is_browser() || !member.roles.contains(&Role::Administrator) {
            return Err(forbidden());
        }
        Ok(member)
    }

    pub fn put_member(&self, actor: &WorkspaceActor, mut change: MemberChange) -> Result<i64> {
        change.identity.key()?;
        if change.roles.len() > 5 || change.repository_ids.len() > 100 {
            return Err(invalid("invalid workspace grants"));
        }
        change.roles.sort();
        change.roles.dedup();
        change.repository_ids.sort();
        change.repository_ids.dedup();
        if change.roles.is_empty()
            || change.repository_ids.len() > 100
            || change
                .repository_ids
                .iter()
                .any(|id| id != "*" && !valid_id(id))
        {
            return Err(invalid("invalid workspace grants"));
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actor_member = self.require_admin(&tx, actor)?;
        let fingerprint = json_text(&change)?;
        if let Some(generation) = replay(&tx, actor, &change.request_id, &fingerprint)? {
            return Ok(generation);
        }
        let previous = settings(&tx)?;
        if previous.generation != change.expected_generation {
            return Err(conflict());
        }
        let key = change.identity.key()?;
        let old = member(&tx, &key)?;
        if old.is_none() {
            let count: i64 = tx.query_row("SELECT count(*) FROM workspace_members", [], |row| {
                row.get(0)
            })?;
            if count >= MAX_MEMBERS {
                return Err(invalid("workspace_member_limit"));
            }
        }
        if change.identity == actor.identity {
            let widened = change.roles.iter().any(|r| !actor_member.roles.contains(r))
                || change.repository_ids.iter().any(|id| {
                    !actor_member.repository_ids.contains(&"*".to_owned())
                        && !actor_member.repository_ids.contains(id)
                });
            if widened {
                return Err(forbidden());
            }
        }
        if old
            .as_ref()
            .is_some_and(|m| m.enabled && m.roles.contains(&Role::Administrator))
            && (!change.enabled || !change.roles.contains(&Role::Administrator))
            && self.eligible_identity(&change.identity)?
        {
            let count = self.enabled_administrators(&tx)?;
            if count <= 1 {
                return Err(invalid("workspace_last_administrator"));
            }
        }
        let generation = previous.generation + 1;
        tx.execute("INSERT INTO workspace_members VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(identity_key) DO UPDATE SET roles=excluded.roles,repository_ids=excluded.repository_ids,enabled=excluded.enabled,generation=excluded.generation",params![key,json_text(&change.identity)?,json_text(&change.roles)?,json_text(&change.repository_ids)?,change.enabled,generation])?;
        // Any grant change revokes existing browser sessions for the target.
        tx.execute(
            "DELETE FROM workspace_sessions WHERE identity_key=?1",
            [&key],
        )?;
        complete(
            &tx,
            actor,
            &change.request_id,
            &fingerprint,
            generation,
            "member_changed",
            &change,
        )?;
        tx.commit()?;
        Ok(generation)
    }

    pub fn put_settings(&self, actor: &WorkspaceActor, change: SettingsChange) -> Result<i64> {
        if !valid_name(&change.name) || change.max_concurrent_runs != 1 {
            return Err(invalid("invalid workspace settings"));
        }
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.require_admin(&tx, actor)?;
        let fingerprint = json_text(&change)?;
        if let Some(generation) = replay(&tx, actor, &change.request_id, &fingerprint)? {
            return Ok(generation);
        }
        let previous = settings(&tx)?;
        if previous.generation != change.expected_generation {
            return Err(conflict());
        }
        let generation = previous.generation + 1;
        tx.execute(
            "UPDATE workspace_meta SET name=?1,max_concurrent_runs=?2 WHERE singleton=1",
            params![change.name, change.max_concurrent_runs],
        )?;
        complete(
            &tx,
            actor,
            &change.request_id,
            &fingerprint,
            generation,
            "settings_changed",
            &change,
        )?;
        tx.commit()?;
        Ok(generation)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemberChange {
    pub request_id: String,
    pub expected_generation: i64,
    pub identity: Identity,
    pub roles: Vec<Role>,
    pub repository_ids: Vec<String>,
    pub enabled: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsChange {
    pub request_id: String,
    pub expected_generation: i64,
    pub name: String,
    pub max_concurrent_runs: i64,
}
fn replay(
    conn: &Connection,
    actor: &WorkspaceActor,
    id: &str,
    fingerprint: &str,
) -> Result<Option<i64>> {
    if !valid_id(id) {
        return Err(invalid("invalid request id"));
    }
    let old: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT actor,fingerprint,generation FROM workspace_requests WHERE request_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match old {
        Some((owner, previous, generation))
            if owner == actor.identity.key()? && previous == fingerprint =>
        {
            Ok(Some(generation))
        }
        Some(_) => Err(invalid("workspace_idempotency_conflict")),
        None => Ok(None),
    }
}
fn complete(
    conn: &Connection,
    actor: &WorkspaceActor,
    id: &str,
    fingerprint: &str,
    generation: i64,
    kind: &str,
    data: &impl Serialize,
) -> Result<()> {
    conn.execute(
        "UPDATE workspace_meta SET generation=?1 WHERE singleton=1",
        [generation],
    )?;
    conn.execute(
        "INSERT INTO workspace_requests VALUES(?1,?2,?3,?4)",
        params![id, actor.identity.key()?, fingerprint, generation],
    )?;
    audit(conn, &actor.identity.key()?, kind, data)
}
fn session_digest(raw: &str) -> Result<String> {
    token_digest(SESSION_DOMAIN, raw).map_err(|_| HubError::Unauthorized)
}
fn csrf_token(raw: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agentscm.workspace.csrf-derivation.v1\0");
    hash.update(raw.as_bytes());
    super::store::lower_hex_of(&hash.finalize())
}
fn csrf_digest(raw: &str) -> Result<String> {
    token_digest(CSRF_DOMAIN, raw).map_err(|_| invalid("workspace_csrf"))
}
fn cookie(raw: &str, seconds: i64) -> String {
    format!("{COOKIE}={raw}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={seconds}")
}
fn cookie_value(headers: &HeaderMap) -> Result<Option<String>> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().strip_prefix(COOKIE)?.strip_prefix('='));
    match (values.next(), values.next()) {
        (None, None) => Ok(None),
        (Some(value), None) => Ok(Some(value.to_owned())),
        _ => Err(HubError::Unauthorized),
    }
}

pub fn api_error(error: HubError) -> ApiError {
    match &error {
        HubError::Invalid(code) => match code.as_str() {
            "workspace_forbidden" => ApiError::new(StatusCode::FORBIDDEN, "forbidden"),
            "workspace_csrf" => ApiError::new(StatusCode::FORBIDDEN, "csrf_required"),
            "workspace_generation_conflict" => {
                ApiError::new(StatusCode::CONFLICT, "generation_conflict")
            }
            "workspace_idempotency_conflict" => {
                ApiError::new(StatusCode::CONFLICT, "idempotency_conflict")
            }
            "workspace_native_conflict" => {
                ApiError::new(StatusCode::CONFLICT, "native_subject_conflict")
            }
            "workspace_last_administrator" => {
                ApiError::new(StatusCode::CONFLICT, "last_administrator")
            }
            "workspace_member_limit" => ApiError::new(StatusCode::CONFLICT, "member_limit"),
            _ => ApiError::from(error),
        },
        _ => ApiError::from(error),
    }
}

impl FromRequestParts<HubState> for WorkspaceActor {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let workspace = state.workspace.clone().ok_or_else(ApiError::not_found)?;
        let mutation = !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
        let raw = cookie_value(&parts.headers).map_err(api_error)?;
        let csrf = parts
            .headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let token = if raw.is_none() {
            Some(bearer(parts)?.to_owned())
        } else {
            None
        };
        blocking(state, move || match raw {
            Some(raw) => workspace.actor_for_cookie(&raw, csrf.as_deref(), mutation),
            None => {
                workspace.actor_for_agent(token.as_deref().ok_or(HubError::Unauthorized)?, mutation)
            }
        })
        .await
        .map_err(api_error)
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/workspace/login", get(login))
        .route("/v1/hub/workspace/session", get(session).delete(logout))
        .route("/v1/hub/workspace/members", get(members).put(put_members))
        .route(
            "/v1/hub/workspace/settings",
            get(get_settings).put(update_settings),
        )
        .route(
            "/v1/hub/workspace/view-settings",
            axum::routing::put(put_view_settings),
        )
        .route(
            "/v1/hub/workspace/view-settings/me",
            axum::routing::put(put_my_view_settings),
        )
}
type ApiResult<T> = std::result::Result<T, ApiError>;
fn configured(state: &HubState) -> ApiResult<Arc<Workspace>> {
    state.workspace.clone().ok_or_else(ApiError::not_found)
}
async fn login(State(state): State<HubState>) -> ApiResult<Json<Value>> {
    configured(&state)?;
    Ok(Json(json!({"providers":state.login.enabled()})))
}
async fn session(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !actor.is_browser() {
        return Err(api_error(forbidden()));
    }
    let workspace = configured(&state)?;
    let raw = cookie_value(&headers)
        .map_err(api_error)?
        .ok_or_else(ApiError::unauthorized)?;
    let providers = state.login.enabled();
    let body=blocking(&state,move || {
        let conn=workspace.conn()?;
        let (user,member)=workspace.current(&conn,&actor)?;
        Ok(json!({"workspace":settings(&conn)?,"actor":{"user":{"id":user.id,"identity":Identity::from(&user.identity),"login":user.login},"roles":member.roles,"repository_ids":member.repository_ids,"generation":member.generation},
          "capabilities":{"manage_workspace":member.roles.contains(&Role::Administrator),"read_repositories":member.roles.contains(&Role::Reader),"execute_workflows":member.roles.contains(&Role::Contributor),"approve_revisions":member.roles.contains(&Role::Reviewer),"integrate_revisions":member.roles.contains(&Role::Integrator)},"csrf_token":csrf_token(&raw),"login_providers":providers}))
    }).await.map_err(api_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response())
}
async fn logout(State(state): State<HubState>, actor: WorkspaceActor) -> ApiResult<Response> {
    if !actor.is_browser() {
        return Err(api_error(forbidden()));
    }
    let workspace = configured(&state)?;
    blocking(&state, move || {
        let mut conn = workspace.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        workspace.current(&tx, &actor)?;
        if let Credential::Browser { digest } = &actor.credential {
            tx.execute("DELETE FROM workspace_sessions WHERE digest=?1", [digest])?;
        }
        audit(&tx, &actor.identity.key()?, "session_revoked", &json!({}))?;
        tx.commit()?;
        Ok(())
    })
    .await
    .map_err(api_error)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie("", 0)).map_err(|_| ApiError::invalid())?,
    );
    Ok(response)
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    limit: Option<usize>,
    after: Option<String>,
}
async fn members(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Query(page): Query<Page>,
) -> ApiResult<Json<Value>> {
    let workspace = configured(&state)?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit) || page.after.as_ref().is_some_and(|v| v.len() > 600) {
        return Err(ApiError::invalid());
    }
    let value=blocking(&state,move || {
        let conn=workspace.conn()?;
        workspace.require_admin(&conn,&actor)?;
        let mut statement=conn.prepare("SELECT identity_key FROM workspace_members WHERE identity_key>?1 ORDER BY identity_key LIMIT ?2")?;
        let keys=statement.query_map(params![page.after.unwrap_or_default(),limit as i64+1],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let next=if keys.len()>limit { keys.get(limit-1).cloned() } else { None };
        let members=keys.iter().take(limit).map(|key|member(&conn,key)?.ok_or_else(||HubError::Unavailable("workspace member disappeared".into()))).collect::<Result<Vec<_>>>()?;
        Ok(json!({"members":members,"next_cursor":next,"generation":settings(&conn)?.generation}))
    }).await.map_err(api_error)?;
    Ok(Json(value))
}
async fn get_settings(
    State(state): State<HubState>,
    actor: WorkspaceActor,
) -> ApiResult<Json<Settings>> {
    let workspace = configured(&state)?;
    let result = blocking(&state, move || {
        let conn = workspace.conn()?;
        workspace.require_admin(&conn, &actor)?;
        settings(&conn)
    })
    .await
    .map_err(api_error)?;
    Ok(Json(result))
}
async fn put_members(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(change): JsonBody<MemberChange>,
) -> ApiResult<Json<Value>> {
    let workspace = configured(&state)?;
    let generation = blocking(&state, move || workspace.put_member(&actor, change))
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"generation":generation})))
}
async fn put_view_settings(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(settings): JsonBody<super::view_settings::ViewSettings>,
) -> ApiResult<Json<super::view_settings::ViewSettings>> {
    let workspace = configured(&state)?;
    let store = state.store.clone();
    blocking(&state, move || {
        let conn = workspace.conn()?;
        let member = workspace.require_admin(&conn, &actor)?;
        super::view_settings::ViewSettings::save(&*store, &settings, &member.identity.key()?)?;
        Ok(settings)
    })
    .await
    .map(Json)
    .map_err(api_error)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MyViewSettings {
    #[serde(flatten)]
    choices: super::view_settings::ViewOverride,
    #[serde(default)]
    reset: bool,
}
/// Any signed-in member patches their own override; `reset` returns to the hub default.
async fn put_my_view_settings(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(body): JsonBody<MyViewSettings>,
) -> ApiResult<Json<Value>> {
    let workspace = configured(&state)?;
    let store = state.store.clone();
    blocking(&state, move || {
        let conn = workspace.conn()?;
        let (user, _) = workspace.current(&conn, &actor)?;
        let owner = user.id;
        let choices = if body.reset {
            Default::default()
        } else {
            // A patch: fields the caller leaves out keep their current override.
            super::view_settings::ViewSettings::load_override(&*store, &owner)?
                .unwrap_or_default()
                .merge(body.choices)
        };
        super::view_settings::ViewSettings::save_override(&*store, &owner, &choices)?;
        super::view_settings::ViewSettings::report(&*store, &owner)
    })
    .await
    .map(Json)
    .map_err(api_error)
}
async fn update_settings(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(change): JsonBody<SettingsChange>,
) -> ApiResult<Json<Value>> {
    let workspace = configured(&state)?;
    let generation = blocking(&state, move || workspace.put_settings(&actor, change))
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"generation":generation})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::promise::{PromiseCriterionV1, PromiseInputV1, PromiseRepositoryAdmissionV1};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn promise_write_holds_cross_connection_grant_until_store_commit() {
        let dir = TempDir::new().unwrap();
        let store = super::super::store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let identity = |user_id| ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id,
        };
        let owner = store.upsert_user(&identity(101), "owner").unwrap();
        let member_user = store.upsert_user(&identity(202), "contributor").unwrap();
        let config = || WorkspaceConfig {
            workspace_id: "synthetic".into(),
            name: "Synthetic workspace".into(),
            database: dir.path().join("workspace.sqlite"),
            initial_owner: Identity::from(&owner.identity),
            forge_repositories: vec![],
        };
        let writer = Arc::new(Workspace::open(config(), store.clone()).unwrap());
        let revoker = Arc::new(Workspace::open(config(), store.clone()).unwrap());
        let actor_for = |workspace: &Workspace, user: &User| {
            let cookie = workspace.issue_session(user).unwrap();
            let raw = cookie.split(';').next().unwrap().split_once('=').unwrap().1;
            workspace.actor_for_cookie(raw, None, false).unwrap()
        };
        let owner_actor = actor_for(&writer, &owner);
        writer
            .put_member(
                &owner_actor,
                MemberChange {
                    request_id: "grant-contributor".into(),
                    expected_generation: 1,
                    identity: Identity::from(&member_user.identity),
                    roles: vec![Role::Reader, Role::Contributor],
                    repository_ids: vec!["synthetic/project".into()],
                    enabled: true,
                },
            )
            .unwrap();
        let member_actor = actor_for(&writer, &member_user);
        let input = |operation_id: &str| PromiseInputV1::Proposal {
            operation_id: operation_id.into(),
            repository_id: "synthetic/project".into(),
            original_request: "Keep settings.".into(),
            commitment: "Settings persist.".into(),
            criteria: vec![PromiseCriterionV1 {
                id: "settings".into(),
                text: "Settings persist.".into(),
                evidence_requirements: vec![],
            }],
            constraints: vec![],
        };
        let admission = PromiseRepositoryAdmissionV1::unbound("synthetic/project");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writing = writer.clone();
        let write_store = store.clone();
        let member_id = member_user.id.clone();
        let first_input = input("before-revocation");
        let first_admission = admission.clone();
        let first_actor = member_actor.clone();
        let write = std::thread::spawn(move || {
            writing.with_promise_proposal(&first_actor, "synthetic/project", || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                write_store.promise_input(
                    "synthetic",
                    &member_id,
                    None,
                    &first_input,
                    &first_admission,
                    unix_now(),
                )
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let contender = rusqlite::Connection::open(dir.path().join("workspace.sqlite")).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        let busy = contender.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(matches!(
            busy,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::DatabaseBusy
        ));
        drop(contender);
        let changing = revoker.clone();
        let revoked_identity = Identity::from(&member_user.identity);
        let (revoking_tx, revoking_rx) = std::sync::mpsc::channel();
        let revoke = std::thread::spawn(move || {
            revoking_tx.send(()).unwrap();
            changing.put_member(
                &owner_actor,
                MemberChange {
                    request_id: "revoke-contributor".into(),
                    expected_generation: 2,
                    identity: revoked_identity,
                    roles: vec![Role::Reader, Role::Contributor],
                    repository_ids: vec!["synthetic/project".into()],
                    enabled: false,
                },
            )
        });
        revoking_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        let receipt = write.join().unwrap().unwrap();
        revoke.join().unwrap().unwrap();
        let hub_db = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        let count = || {
            hub_db
                .query_row("SELECT COUNT(*) FROM hub_promise_inputs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(count(), 1);
        assert!(!receipt.input_id.is_empty());
        let later_input = input("after-revocation");
        assert!(
            writer
                .with_promise_proposal(&member_actor, "synthetic/project", || {
                    store.promise_input(
                        "synthetic",
                        &member_user.id,
                        None,
                        &later_input,
                        &admission,
                        unix_now(),
                    )
                })
                .is_err()
        );
        assert_eq!(count(), 1);
    }

    #[test]
    fn agent_task_origin_grants_and_token_revocation_share_one_workspace_lock() {
        let dir = TempDir::new().unwrap();
        let store = super::super::store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id: 101,
        };
        let user = store.upsert_user(&identity, "synthetic-owner").unwrap();
        let workspace = Arc::new(
            Workspace::open(
                WorkspaceConfig {
                    workspace_id: "synthetic".into(),
                    name: "Synthetic workspace".into(),
                    database: dir.path().join("workspace.sqlite"),
                    initial_owner: Identity::from(&identity),
                    forge_repositories: vec![],
                },
                store.clone(),
            )
            .unwrap(),
        );
        let (token_id, token) = super::super::store::mint_agent_token(
            &*store,
            &user.id,
            "task-agent",
            "synthetic/project",
            3600,
        )
        .unwrap();
        let actor = workspace.actor_for_agent(token.expose(), true).unwrap();
        let task_actor = actor.agent_identity().unwrap();
        let origin = crate::skill_workflow::OriginGrantBinding {
            actor: task_actor.clone(),
            grant_id: task_actor.owner_id.clone(),
            grant_generation: u64::try_from(actor.generation()).unwrap(),
        };
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let held = workspace.clone();
        let guard = std::thread::spawn(move || {
            held.with_agent_task_authorization(
                AgentTaskAuthorization {
                    actor: &actor,
                    repository: "synthetic/project",
                    core_actor: &task_actor,
                    action: crate::skill_workflow::CollectorAction::Submit,
                    origins: &[origin],
                    audited: None,
                },
                || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let changing = workspace.clone();
        let revoke_store = store.clone();
        let (revoked_tx, revoked_rx) = std::sync::mpsc::channel();
        let revoker = std::thread::spawn(move || {
            changing
                .with_identity_change(|| {
                    revoke_store.revoke_owned_agent_token(&user.id, &token_id)?;
                    Ok(())
                })
                .unwrap();
            revoked_tx.send(()).unwrap();
        });
        assert!(revoked_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        guard.join().unwrap();
        revoked_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        revoker.join().unwrap();
        assert!(workspace.actor_for_agent(token.expose(), false).is_err());
    }

    #[test]
    fn administrator_does_not_imply_repository_actions_or_self_escalation() {
        let dir = TempDir::new().unwrap();
        let store = super::super::store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id: 101,
        };
        let user = store.upsert_user(&identity, "synthetic-admin").unwrap();
        let workspace = Workspace::open(
            WorkspaceConfig {
                workspace_id: "synthetic".into(),
                name: "Synthetic workspace".into(),
                database: dir.path().join("workspace.sqlite"),
                initial_owner: Identity::from(&identity),
                forge_repositories: vec![],
            },
            store,
        )
        .unwrap();
        let signin = || {
            let cookie = workspace.issue_session(&user).unwrap();
            let raw = cookie.split(';').next().unwrap().split_once('=').unwrap().1;
            workspace.actor_for_cookie(raw, None, false).unwrap()
        };
        let change = |generation, roles| MemberChange {
            request_id: format!("change-{generation}"),
            expected_generation: generation,
            identity: Identity::from(&identity),
            roles,
            repository_ids: vec!["*".into()],
            enabled: true,
        };
        workspace
            .put_member(&signin(), change(1, vec![Role::Administrator]))
            .unwrap();
        let admin = signin();
        for action in [
            WorkspaceAction::ReadRepository,
            WorkspaceAction::ExecuteWorkflow,
            WorkspaceAction::ApproveRevision,
            WorkspaceAction::IntegrateRevision,
        ] {
            assert!(
                workspace
                    .authorize(&admin, "synthetic/project", action)
                    .is_err()
            );
        }
        assert!(
            matches!(workspace.put_member(&admin,change(2,vec![Role::Administrator,Role::Contributor])),Err(HubError::Invalid(code)) if code=="workspace_forbidden")
        );
    }

    #[test]
    fn disabled_or_unknown_hub_administrator_cannot_satisfy_last_admin_and_sessions_stay_revoked() {
        let dir = TempDir::new().unwrap();
        let store = super::super::store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let identity = |id| ForgeIdentity {
            provider: "github".into(),
            host: "forge.example".into(),
            user_id: id,
        };
        let owner = store
            .upsert_user(&identity(101), "synthetic-owner")
            .unwrap();
        let second = store
            .upsert_user(&identity(202), "synthetic-second")
            .unwrap();
        let workspace = Workspace::open(
            WorkspaceConfig {
                workspace_id: "synthetic".into(),
                name: "Synthetic workspace".into(),
                database: dir.path().join("workspace.sqlite"),
                initial_owner: Identity::from(&owner.identity),
                forge_repositories: vec![],
            },
            store.clone(),
        )
        .unwrap();
        let raw = |cookie: String| {
            cookie
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap()
                .1
                .to_owned()
        };
        let owner_raw = raw(workspace.issue_session(&owner).unwrap());
        let actor = workspace.actor_for_cookie(&owner_raw, None, false).unwrap();
        workspace
            .put_member(
                &actor,
                MemberChange {
                    request_id: "second-admin".into(),
                    expected_generation: 1,
                    identity: Identity::from(&second.identity),
                    roles: vec![Role::Administrator],
                    repository_ids: vec![],
                    enabled: true,
                },
            )
            .unwrap();
        let second_raw = raw(workspace.issue_session(&second).unwrap());
        let (_, token) = super::super::store::mint_agent_token(
            &*store,
            &second.id,
            "synthetic-agent",
            "synthetic/project",
            3600,
        )
        .unwrap();
        let agent_actor = workspace.actor_for_agent(token.expose(), false).unwrap();
        workspace
            .with_user_change(&owner.id, &second.id, true, || {
                store.set_user_disabled(&second.id, true)
            })
            .unwrap();
        assert!(
            workspace
                .actor_for_cookie(&second_raw, None, false)
                .is_err()
        );
        // A numeric typo is pre-provisionable but cannot preserve access for
        // the only administrator who has actually signed in.
        assert!(store.user_for_identity(&identity(999)).unwrap().is_none());
        workspace
            .put_member(
                &actor,
                MemberChange {
                    request_id: "unknown-admin".into(),
                    expected_generation: 3,
                    identity: Identity::from(&identity(999)),
                    roles: vec![Role::Administrator],
                    repository_ids: vec![],
                    enabled: true,
                },
            )
            .unwrap();
        let denied = workspace.put_member(
            &actor,
            MemberChange {
                request_id: "remove-owner".into(),
                expected_generation: 4,
                identity: Identity::from(&owner.identity),
                roles: vec![Role::Reader],
                repository_ids: vec!["*".into()],
                enabled: true,
            },
        );
        assert!(
            matches!(denied,Err(HubError::Invalid(code)) if code=="workspace_last_administrator")
        );
        let denied = workspace.with_user_change(&owner.id, &owner.id, true, || {
            store.set_user_disabled(&owner.id, true)
        });
        assert!(
            matches!(denied,Err(HubError::Invalid(code)) if code=="workspace_last_administrator")
        );
        assert!(!store.user_by_id(&owner.id).unwrap().unwrap().disabled);
        workspace
            .with_user_change(&owner.id, &second.id, false, || {
                store.set_user_disabled(&second.id, false)
            })
            .unwrap();
        assert!(
            workspace.recheck(&agent_actor).is_err(),
            "re-enabling must not revive a retained actor generation"
        );
        assert!(
            workspace
                .actor_for_cookie(&second_raw, None, false)
                .is_err(),
            "re-enabling must not revive a revoked cookie"
        );
    }
}
