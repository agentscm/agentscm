//! A single-server queue over the existing native workflow controller.
//!
//! Configuration is trusted startup input. HTTP requests select only registered
//! identifiers and an exact existing native subject; they never supply programs,
//! directories, provider endpoints or recipes. Source transfer is separately opted
//! in through operator-pinned profiles in the source module.

pub mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::workspace::{Workspace, WorkspaceAction, WorkspaceActor};
use super::{HubError, Result};
use crate::authority::Authority;
use crate::authority::workflows::WorkflowRequirement;
use crate::context::{ChangeId, ReviewContext};
use crate::execution::{Cancellation, OciBackend};
use crate::lease::Lease;
use crate::reviewer::ReviewCancellation;
use crate::skill_workflow::runtime::{ClaudeExecutor, CommandExecutor};
use crate::skill_workflow::{self, RunState};

pub const CONFIG_ENV: &str = "AGENTSCM_WORKSPACE_EXECUTION_FILE";
const MAX_QUEUE: i64 = 32;
const MAX_PROFILES: usize = 32;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<ExecutionProfile>,
    #[serde(default)]
    pub source_profiles: Vec<source::SourceProfile>,
    #[serde(default)]
    pub agent_task_profiles: Vec<AgentTaskProfile>,
    #[serde(default)]
    pub attached_publication_profiles: Vec<AttachedPublicationProfile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTaskProfile {
    pub repository_id: String,
    pub profile_id: String,
    pub native_repository: String,
    pub data_dir: PathBuf,
    pub target: String,
    pub requirement_digest: String,
    /// Off by default: the Hub publishes a forge commit status for this profile only when set.
    #[serde(default)]
    pub publish_forge_status: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachedPublicationProfile {
    pub repository_id: String,
    pub profile_id: String,
    pub attachment_id: String,
    pub owner_id: String,
    pub git: PathBuf,
    pub worktree: PathBuf,
    pub git_common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub index: PathBuf,
    pub mirror: PathBuf,
    pub endpoint: String,
    pub target: String,
    pub output: String,
    pub credential_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub repository_id: String,
    pub profile_id: String,
    pub native_repository: String,
    pub data_dir: PathBuf,
    pub target: String,
    pub requirement_digest: String,
    pub docker: PathBuf,
    pub docker_socket: PathBuf,
    pub claude: PathBuf,
}

impl ExecutionConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err(HubError::Invalid(
                "execution configuration exceeds 64 KiB".into(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| HubError::Invalid("invalid execution configuration".into()))
    }
}

fn validate_attached_publication_profile(
    profile: &AttachedPublicationProfile,
    task: &AgentTaskProfile,
) -> Result<()> {
    use crate::git_export::remote::{Endpoint, HttpsTrust};
    if profile.target != task.target
        || profile.output == profile.target
        || !crate::hub::github::valid_ref_name(&profile.output)
        || [
            &profile.git,
            &profile.worktree,
            &profile.git_common_dir,
            &profile.git_dir,
            &profile.index,
            &profile.mirror,
        ]
        .iter()
        .any(|path| !path.is_absolute())
    {
        return Err(HubError::Invalid(
            "invalid attached publication identity".into(),
        ));
    }
    if profile.endpoint.starts_with("https://") {
        Endpoint::Https {
            url: profile.endpoint.clone(),
            trust: HttpsTrust::System,
        }
        .validate()?;
    } else if !Path::new(&profile.endpoint).is_absolute() {
        return Err(HubError::Invalid(
            "attached publication endpoint must be canonical HTTPS or absolute local".into(),
        ));
    }
    if profile.endpoint.len() > 4096 || profile.endpoint.chars().any(char::is_control) {
        return Err(HubError::Invalid(
            "invalid attached publication endpoint".into(),
        ));
    }
    if let Some(path) = &profile.credential_file {
        if !path.is_absolute() {
            return Err(HubError::Invalid("credential path must be absolute".into()));
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(HubError::Invalid(
                "credential must be an owner-private regular file".into(),
            ));
        }
    }
    if let Some(path) = &profile.ca_file {
        if !path.is_absolute() || !fs::symlink_metadata(path)?.is_file() {
            return Err(HubError::Invalid(
                "CA file must be an absolute regular file".into(),
            ));
        }
        crate::git_export::remote::network::trust_digest(path)?;
    }
    let state = crate::attached_worktree::AttachmentJournal::open_existing(
        &task.data_dir,
        &task.native_repository,
    )?
    .state(&profile.attachment_id)?;
    if state.git != profile.git.to_string_lossy()
        || state.worktree != profile.worktree.to_string_lossy()
        || state.git_common_dir != profile.git_common_dir.to_string_lossy()
        || state.git_dir != profile.git_dir.to_string_lossy()
        || state.index != profile.index.to_string_lossy()
        || state.remote_url != profile.endpoint
        || state.target != profile.target
        || state.output != profile.output
    {
        return Err(HubError::Invalid(
            "attached publication configuration differs from retained attachment".into(),
        ));
    }
    crate::git_export::Mirror::open(&profile.git, &profile.mirror, &task.native_repository)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunSubject {
    pub change_id: String,
    pub change_version: u64,
    pub context_digest: String,
    pub revision: String,
    pub requirement_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub request_id: String,
    pub expected_generation: i64,
    pub profile_id: String,
    pub subject: RunSubject,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub request_id: String,
    pub expected_generation: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    Stale,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRun {
    pub run_id: String,
    pub repository_id: String,
    pub profile_id: String,
    pub actor_id: String,
    pub generation: i64,
    pub state: ExecutionState,
    pub subject: RunSubject,
    pub native_run_id: String,
    pub receipt: Option<String>,
    pub detail: String,
}

#[derive(Serialize)]
pub struct RunPage {
    pub runs: Vec<WorkspaceRun>,
    pub next_cursor: Option<String>,
}
#[derive(Serialize)]
pub struct SubjectPage {
    pub subjects: Vec<RunSubject>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
pub struct RepositoryPage {
    pub repositories: Vec<RepositoryView>,
}
#[derive(Serialize)]
pub struct RepositoryView {
    pub repository_id: String,
    pub profiles: Vec<ProfileView>,
}
#[derive(Serialize)]
pub struct ProfileView {
    pub profile_id: String,
    pub target: String,
    pub requirement_digest: String,
    pub readiness: &'static str,
    pub limitations: &'static str,
}

struct Queue {
    db: Connection,
}
pub struct ExecutionService {
    queue: Mutex<Queue>,
    profiles: BTreeMap<(String, String), ExecutionProfile>,
    pub(crate) agent_task_profiles: BTreeMap<(String, String), AgentTaskProfile>,
    pub(crate) attached_publication_profiles:
        BTreeMap<(String, String, String), AttachedPublicationProfile>,
    source_profiles: BTreeMap<(String, String), source::SourceProfile>,
    source_worker: Mutex<()>,
    pub(crate) workspace: Arc<Workspace>,
    // Prevent a second server from recovering or dispatching the same queue.
    _lease: Lease,
    worker: Mutex<()>,
}

impl ExecutionService {
    pub fn server_execution_enabled(&self) -> bool {
        !self.profiles.is_empty()
    }

    pub fn open(path: &Path, config: ExecutionConfig, workspace: Arc<Workspace>) -> Result<Self> {
        if config.version != 1
            || config.profiles.len() > MAX_PROFILES
            || config.source_profiles.len() > MAX_PROFILES
            || config.agent_task_profiles.len() > MAX_PROFILES
            || config.attached_publication_profiles.len() > MAX_PROFILES
        {
            return Err(HubError::Invalid(
                "unsupported execution configuration".into(),
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("lock"))?;
        let lease = Lease::try_exclusive(lock).map_err(|_| HubError::Busy)?;
        let mut profiles = BTreeMap::new();
        for mut profile in config.profiles {
            super::store::validate_repository(&profile.repository_id)?;
            validate_id(&profile.profile_id)?;
            if !profile.data_dir.is_absolute()
                || !profile.docker.is_absolute()
                || !profile.docker_socket.is_absolute()
                || !profile.claude.is_absolute()
            {
                return Err(HubError::Invalid("execution paths must be absolute".into()));
            }
            profile.data_dir = fs::canonicalize(&profile.data_dir)?;
            profile.current_requirement()?;
            if profiles
                .insert(
                    (profile.repository_id.clone(), profile.profile_id.clone()),
                    profile,
                )
                .is_some()
            {
                return Err(HubError::Invalid("duplicate execution profile".into()));
            }
        }
        let mut agent_task_profiles = BTreeMap::new();
        for profile in profiles.values() {
            let task = AgentTaskProfile::from(profile);
            agent_task_profiles.insert((task.repository_id.clone(), task.profile_id.clone()), task);
        }
        for mut profile in config.agent_task_profiles {
            super::store::validate_repository(&profile.repository_id)?;
            validate_id(&profile.profile_id)?;
            if !profile.data_dir.is_absolute() {
                return Err(HubError::Invalid(
                    "agent task data paths must be absolute".into(),
                ));
            }
            profile.data_dir = fs::canonicalize(&profile.data_dir)?;
            profile.current_requirement()?;
            if agent_task_profiles
                .insert(
                    (profile.repository_id.clone(), profile.profile_id.clone()),
                    profile,
                )
                .is_some()
            {
                return Err(HubError::Invalid("duplicate agent task profile".into()));
            }
        }
        let mut publishing = BTreeSet::new();
        for task in agent_task_profiles
            .values()
            .filter(|p| p.publish_forge_status)
        {
            // Two profiles would post the same status context on one repository.
            if !publishing.insert((task.repository_id.to_ascii_lowercase(), task.target.clone())) {
                return Err(HubError::Invalid(
                    "more than one agent task profile publishes forge status for a repository and target"
                        .into(),
                ));
            }
        }
        let mut attached_publication_profiles = BTreeMap::new();
        let mut attached_outputs = BTreeSet::new();
        let mut attached_mirrors = BTreeSet::new();
        for profile in config.attached_publication_profiles {
            super::store::validate_repository(&profile.repository_id)?;
            validate_id(&profile.profile_id)?;
            validate_id(&profile.attachment_id)?;
            validate_id(&profile.owner_id)?;
            let task = agent_task_profiles
                .get(&(profile.repository_id.clone(), profile.profile_id.clone()))
                .ok_or_else(|| {
                    HubError::Invalid("attached publication has no agent task profile".into())
                })?;
            validate_attached_publication_profile(&profile, task)?;
            let output_owner = (
                profile.repository_id.to_ascii_lowercase(),
                profile.endpoint.clone(),
                profile.output.clone(),
            );
            if !attached_outputs.insert(output_owner)
                || !attached_mirrors.insert(profile.mirror.clone())
            {
                return Err(HubError::Invalid(
                    "attached publications must have distinct mirrors and output ownership".into(),
                ));
            }
            let key = (
                profile.repository_id.clone(),
                profile.profile_id.clone(),
                profile.attachment_id.clone(),
            );
            if attached_publication_profiles.insert(key, profile).is_some() {
                return Err(HubError::Invalid(
                    "duplicate attached publication profile".into(),
                ));
            }
        }
        let mut source_profiles = BTreeMap::new();
        for source in config.source_profiles {
            let key = (source.repository_id.clone(), source.profile_id.clone());
            let profile = profiles.get(&key).ok_or_else(|| {
                HubError::Invalid("source profile has no execution profile".into())
            })?;
            let authority = profile.stable_mapping()?;
            let template = authority.review_context(&source.template_context_digest)?;
            if template.change.repository != profile.native_repository
                || template.target != profile.target
                || source_profiles.insert(key, source).is_some()
            {
                return Err(HubError::Invalid(
                    "invalid or duplicate source profile".into(),
                ));
            }
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS workspace_runs(
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE, repository_id TEXT NOT NULL,
                actor_id TEXT NOT NULL, request_id TEXT NOT NULL,
                request_json BLOB NOT NULL, actor_json BLOB NOT NULL, run_json BLOB NOT NULL,
                state TEXT NOT NULL, UNIQUE(actor_id,request_id)
            ) STRICT;
            CREATE TABLE IF NOT EXISTS workspace_run_events(
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL, event TEXT NOT NULL, run_json BLOB NOT NULL
            ) STRICT;
            CREATE TABLE IF NOT EXISTS workspace_run_cancellations(
                actor_id TEXT NOT NULL, request_id TEXT NOT NULL, request_json BLOB NOT NULL,
                run_json BLOB NOT NULL, PRIMARY KEY(actor_id,request_id)
            ) STRICT;",
        )?;
        source::initialize_source_schema(&db)?;
        source::recover_source_transfers(&db)?;
        let mut queue = Queue { db };
        // A server interruption cannot prove whether an external call started or
        // whether its result reached durable native storage. Never replay it.
        let interrupted = queue.all_active()?;
        for mut run in interrupted {
            run.state = ExecutionState::Unknown;
            run.receipt = None;
            run.detail = "Server interrupted this request; inspect retained native evidence. It will not be replayed.".into();
            queue.update(&mut run)?;
        }
        Ok(Self {
            queue: Mutex::new(queue),
            profiles,
            agent_task_profiles,
            attached_publication_profiles,
            source_profiles,
            source_worker: Mutex::new(()),
            workspace,
            _lease: lease,
            worker: Mutex::new(()),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Queue>> {
        self.queue
            .lock()
            .map_err(|_| HubError::Unavailable("execution queue lock failed".into()))
    }

    fn profile(&self, repository: &str, profile: &str) -> Result<&ExecutionProfile> {
        self.profiles
            .get(&(repository.into(), profile.into()))
            .ok_or(HubError::Unauthorized)
    }

    pub fn repositories(&self, actor: &WorkspaceActor) -> Result<RepositoryPage> {
        let mut repositories: BTreeMap<String, Vec<ProfileView>> = BTreeMap::new();
        for profile in self.agent_task_profiles.values() {
            if self
                .workspace
                .authorize(
                    actor,
                    &profile.repository_id,
                    WorkspaceAction::ReadRepository,
                )
                .is_err()
            {
                continue;
            }
            repositories.entry(profile.repository_id.clone()).or_default().push(ProfileView {
                profile_id: profile.profile_id.clone(), target: profile.target.clone(),
                requirement_digest: profile.requirement_digest.clone(),
                readiness: if profile.current_requirement().is_ok() { "configured" } else { "unavailable" },
                limitations: if self.profiles.contains_key(&(profile.repository_id.clone(), profile.profile_id.clone())) {
                    if self.source_profiles.contains_key(&(profile.repository_id.clone(), profile.profile_id.clone())) { "Server execution and opt-in laptop source/context sync are enabled. Agent-owned tasks remain separately claimed and never trigger server model dispatch." } else { "Server execution and agent-owned tasks are enabled. Source and Change context must already exist in the server native store." }
                } else { "Agent-owned tasks only. No server command or model executor is configured; source and Change context must already exist in the native store." },
            });
        }
        Ok(RepositoryPage {
            repositories: repositories
                .into_iter()
                .map(|(repository_id, profiles)| RepositoryView {
                    repository_id,
                    profiles,
                })
                .collect(),
        })
    }

    pub fn subject(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        change: &str,
    ) -> Result<RunSubject> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ReadRepository)?;
        let profile = self
            .agent_task_profiles
            .get(&(repository.into(), profile.into()))
            .ok_or(HubError::Unauthorized)?;
        let (authority, requirement) = profile.current_requirement()?;
        let id = ChangeId {
            repository: profile.native_repository.clone(),
            id: change.into(),
        };
        let state = authority.change(&id)?;
        let context = authority.review_context(&state.context_digest)?;
        if context.target != profile.target || context.policy_epoch != authority.policy_epoch()? {
            return Err(HubError::Invalid(
                "Change context is stale or belongs to another target".into(),
            ));
        }
        let subject = RunSubject {
            change_id: context.change.id,
            change_version: context.change_version,
            context_digest: state.context_digest,
            revision: context.revision,
            requirement_digest: requirement.digest()?,
        };
        profile.validate_subject(&subject)?;
        Ok(subject)
    }

    pub fn subjects(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<SubjectPage> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ReadRepository)?;
        if !(1..=50).contains(&limit) {
            return Err(HubError::Invalid("invalid subject page limit".into()));
        }
        let configured = self
            .agent_task_profiles
            .get(&(repository.into(), profile.into()))
            .ok_or(HubError::Unauthorized)?;
        let (authority, _) = configured.current_requirement()?;
        // The cursor advances over all Changes, including Changes for another
        // target. A page may be empty while retaining an actionable next cursor.
        let mut changes = authority.list_changes(limit + 1, after)?;
        let more = changes.len() > limit;
        changes.truncate(limit);
        let next_cursor = if more {
            changes.last().map(|change| change.id.id.clone())
        } else {
            None
        };
        let mut subjects = Vec::new();
        for change in changes {
            if let Ok(subject) = self.subject(actor, repository, profile, &change.id.id) {
                subjects.push(subject);
            }
        }
        Ok(SubjectPage {
            subjects,
            next_cursor,
        })
    }

    pub fn workflow(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        run_id: &str,
    ) -> Result<super::skill_workflow::SkillWorkflowEnvelope> {
        let run = self.get(actor, repository, run_id)?;
        let profile = self.profile(repository, &run.profile_id)?;
        let authority = profile.stable_mapping()?;
        let context = authority.review_context(&run.subject.context_digest)?;
        if context.change.repository != profile.native_repository
            || context.change.id != run.subject.change_id
            || context.change_version != run.subject.change_version
            || context.revision != run.subject.revision
        {
            return Err(HubError::Unauthorized);
        }
        let engine = crate::store::Engine::open(&profile.data_dir)?;
        let projection =
            crate::skill_workflow_service::read(&profile.data_dir, &engine, &run.native_run_id)?;
        if projection.run.source.repository != profile.native_repository
            || projection.run.source.revision != run.subject.revision
            || projection.run.source.policy_epoch != context.policy_epoch
            || projection.run.run_id != run.native_run_id
        {
            return Err(HubError::Unauthorized);
        }
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ReadRepository)?;
        Ok(super::skill_workflow::SkillWorkflowEnvelope {
            version: 1,
            availability: super::skill_workflow::Availability::Available,
            projection: Some(projection),
            reason: None,
        })
    }

    pub fn start(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        request: StartRequest,
    ) -> Result<WorkspaceRun> {
        validate_id(&request.request_id)?;
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        let bytes = serde_json::to_vec(&(repository, &request)).map_err(native_json)?;
        let mut queue = self.lock()?;
        if let Some((stored, run)) = queue.db.query_row(
            "SELECT request_json,run_json FROM workspace_runs WHERE actor_id=?1 AND request_id=?2",
            params![actor.user_id(), request.request_id], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))).optional()? {
            if stored != bytes { return Err(HubError::Invalid("workspace_idempotency_conflict".into())); }
            return decode(&run);
        }
        if request.expected_generation != actor.generation() {
            return Err(HubError::Invalid("workspace_generation_conflict".into()));
        }
        let profile = self.profile(repository, &request.profile_id)?;
        let context = profile.validate_subject(&request.subject)?;
        // Compile before accepting; no request can weaken the installed profile.
        let (_, required) = profile.current_requirement()?;
        let plan = skill_workflow::compile(&profile.data_dir, &required.instantiate(&context)?)?;
        if WorkflowRequirement::from_plan(&profile.target, &plan)? != required {
            return Err(HubError::Invalid(
                "source does not match the installed execution profile".into(),
            ));
        }
        let count: i64 = queue.db.query_row(
            "SELECT count(*) FROM workspace_runs WHERE state IN ('queued','running')",
            [],
            |row| row.get(0),
        )?;
        if count >= MAX_QUEUE {
            return Err(HubError::Busy);
        }
        let run_id = random_id()?;
        let run = WorkspaceRun {
            native_run_id: format!("workspace-{run_id}"),
            run_id,
            repository_id: repository.into(),
            profile_id: request.profile_id.clone(),
            actor_id: actor.user_id().into(),
            generation: 1,
            state: ExecutionState::Queued,
            subject: request.subject.clone(),
            receipt: None,
            detail: "Queued for the single server worker".into(),
        };
        self.workspace.with_authorization(actor, repository, WorkspaceAction::ExecuteWorkflow, || {
            let tx = queue.db.transaction()?;
            tx.execute("INSERT INTO workspace_runs(run_id,repository_id,actor_id,request_id,request_json,actor_json,run_json,state)
                VALUES(?1,?2,?3,?4,?5,?6,?7,'queued')", params![run.run_id,repository,actor.user_id(),request.request_id,bytes,serde_json::to_vec(actor)?,serde_json::to_vec(&run)?])?;
            tx.execute("INSERT INTO workspace_run_events(run_id,event,run_json) VALUES(?1,'queued',?2)", params![run.run_id, serde_json::to_vec(&run)?])?;
            tx.commit()?;
            Ok(())
        })?;
        Ok(run)
    }

    pub fn get(&self, actor: &WorkspaceActor, repository: &str, id: &str) -> Result<WorkspaceRun> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ReadRepository)?;
        self.lock()?.get(repository, id)
    }

    pub fn page(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        limit: usize,
        before: Option<i64>,
    ) -> Result<RunPage> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ReadRepository)?;
        self.profile_for_repository(repository)?;
        if !(1..=50).contains(&limit) || before.is_some_and(|value| value <= 0) {
            return Err(HubError::Invalid("invalid execution page".into()));
        }
        let queue = self.lock()?;
        let mut stmt = queue.db.prepare("SELECT sequence,run_json FROM workspace_runs WHERE repository_id=?1 AND (?2 IS NULL OR sequence<?2) ORDER BY sequence DESC LIMIT ?3")?;
        let mut rows = stmt.query(params![repository, before, limit as i64 + 1])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push((
                row.get::<_, i64>(0)?,
                decode::<WorkspaceRun>(&row.get::<_, Vec<u8>>(1)?)?,
            ));
        }
        let more = results.len() > limit;
        results.truncate(limit);
        let next_cursor = if more {
            results.last().map(|row| row.0.to_string())
        } else {
            None
        };
        Ok(RunPage {
            runs: results.into_iter().map(|row| row.1).collect(),
            next_cursor,
        })
    }

    fn profile_for_repository(&self, repository: &str) -> Result<()> {
        if self.profiles.keys().any(|(repo, _)| repo == repository) {
            Ok(())
        } else {
            Err(HubError::Unauthorized)
        }
    }

    pub fn cancel(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        id: &str,
        request: CancelRequest,
    ) -> Result<WorkspaceRun> {
        validate_id(&request.request_id)?;
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        let bytes = serde_json::to_vec(&(repository, id, &request)).map_err(native_json)?;
        let mut queue = self.lock()?;
        if let Some((stored,run)) = queue.db.query_row("SELECT request_json,run_json FROM workspace_run_cancellations WHERE actor_id=?1 AND request_id=?2",
            params![actor.user_id(),request.request_id], |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,Vec<u8>>(1)?))).optional()? {
            if stored != bytes { return Err(HubError::Invalid("workspace_idempotency_conflict".into())); }
            return decode(&run);
        }
        let mut run = queue.get(repository, id)?;
        if request.expected_generation != run.generation {
            return Err(HubError::Invalid("workspace_generation_conflict".into()));
        }
        if !matches!(run.state, ExecutionState::Queued | ExecutionState::Running) {
            return Err(HubError::Invalid(
                "only queued or running requests can be cancelled".into(),
            ));
        }
        self.workspace.with_authorization(actor, repository, WorkspaceAction::ExecuteWorkflow, || {
            let tx = queue.db.transaction()?;
            run.state = ExecutionState::Cancelled; run.receipt = None;
            run.generation += 1; run.detail = "Cancellation requested; any external work is being stopped. No passing receipt will be created.".into();
            tx.execute("UPDATE workspace_runs SET run_json=?1,state='cancelled' WHERE run_id=?2",params![serde_json::to_vec(&run)?,run.run_id])?;
            tx.execute("INSERT INTO workspace_run_cancellations(actor_id,request_id,request_json,run_json) VALUES(?1,?2,?3,?4)",params![actor.user_id(),request.request_id,bytes,serde_json::to_vec(&run)?])?;
            tx.execute("INSERT INTO workspace_run_events(run_id,event,run_json) VALUES(?1,'cancelled',?2)", params![run.run_id,serde_json::to_vec(&run)?])?;
            tx.commit()?; Ok(())
        })?;
        // Do not take a native journal lock under the queue lock. The worker's
        // cancellation monitor observes this durable state within 50 ms.
        drop(queue);
        if let Ok(profile) = self.profile(repository, &run.profile_id) {
            let _ = skill_workflow::cancel(&profile.data_dir, &run.native_run_id);
        }
        Ok(run)
    }

    /// Spawn one worker. On shutdown the durable active state is deliberately
    /// left uncertain, and the next open marks it Unknown without replay.
    pub fn spawn_worker(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let service = self.clone();
                match tokio::task::spawn_blocking(move || service.process_next()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        eprintln!("workspace execution worker stopped: {error}");
                        break;
                    }
                    Err(error) => {
                        eprintln!("workspace execution worker interrupted: {error}");
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }

    pub fn process_next(&self) -> Result<bool> {
        self.process_next_using(|actor, run| self.execute_native(actor, run))
    }

    // Injection is private to this controller and its deterministic boundary
    // tests. HTTP and operator configuration always use the native implementation.
    fn process_next_using(
        &self,
        execute: impl FnOnce(&WorkspaceActor, &WorkspaceRun) -> Result<(ExecutionState, String)>,
    ) -> Result<bool> {
        let Ok(_worker) = self.worker.try_lock() else {
            return Ok(false);
        };
        let (mut run, actor) = {
            let mut queue = self.lock()?;
            let row = queue.db.query_row("SELECT run_json,actor_json FROM workspace_runs WHERE state='queued' ORDER BY sequence LIMIT 1",[],
                |row| Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,Vec<u8>>(1)?))).optional()?;
            let Some((run, actor)) = row else {
                return Ok(false);
            };
            let mut run: WorkspaceRun = decode(&run)?;
            let actor: WorkspaceActor = decode(&actor)?;
            run.state = ExecutionState::Running;
            run.detail = "Executing the exact installed native workflow".into();
            queue.update(&mut run)?;
            (run, actor)
        };
        let result = execute(&actor, &run);
        let mut queue = self.lock()?;
        let current = queue.get(&run.repository_id, &run.run_id)?;
        if current.state != ExecutionState::Running {
            return Ok(true);
        }
        run.generation = current.generation;
        match result {
            Ok((state, detail)) => {
                run.state = state;
                run.detail = detail;
            }
            Err(_) => {
                run.state = if self
                    .workspace
                    .authorize(&actor, &run.repository_id, WorkspaceAction::ExecuteWorkflow)
                    .is_err()
                    || self
                        .profile(&run.repository_id, &run.profile_id)
                        .and_then(|profile| profile.validate_subject(&run.subject))
                        .is_err()
                {
                    ExecutionState::Stale
                } else {
                    ExecutionState::Unknown
                };
                run.detail="Execution did not return a conclusive result; inspect native evidence. This request will not be replayed.".into();
            }
        }
        // Current authorization and cancellation are fenced through the exact
        // native authority receipt write. No authorization lock spans a process.
        if run.state == ExecutionState::Succeeded {
            let profile = self.profile(&run.repository_id, &run.profile_id)?;
            let context = profile.validate_subject(&run.subject);
            let finalized = context.and_then(|context| {
                self.workspace.with_authorization(
                    &actor,
                    &run.repository_id,
                    WorkspaceAction::ExecuteWorkflow,
                    || {
                        skill_workflow::runtime::finalize(
                            &profile.data_dir,
                            &run.native_run_id,
                            &context,
                        )
                    },
                )
            });
            match finalized {
                Ok(receipt) => run.receipt = Some(receipt),
                Err(_) => {
                    run.state = ExecutionState::Stale;
                    run.detail="Current authorization or native subject changed before completion; no passing receipt was created.".into();
                }
            }
        }
        queue.update(&mut run)?;
        Ok(true)
    }

    fn check_current(&self, actor: &WorkspaceActor, run: &WorkspaceRun) -> crate::Result<()> {
        let queue = self.lock().map_err(guard_error)?;
        if queue
            .get(&run.repository_id, &run.run_id)
            .map_err(guard_error)?
            .state
            != ExecutionState::Running
        {
            return Err(crate::Error::Conflict(
                "workspace request is cancelled or fenced".into(),
            ));
        }
        self.workspace
            .authorize(actor, &run.repository_id, WorkspaceAction::ExecuteWorkflow)
            .map_err(guard_error)?;
        self.profile(&run.repository_id, &run.profile_id)
            .map_err(guard_error)?
            .validate_subject(&run.subject)
            .map_err(guard_error)?;
        Ok(())
    }

    fn execute_native(
        &self,
        actor: &WorkspaceActor,
        run: &WorkspaceRun,
    ) -> Result<(ExecutionState, String)> {
        let profile = self.profile(&run.repository_id, &run.profile_id)?;
        let check = || self.check_current(actor, run);
        check().map_err(HubError::from)?;
        let context = profile.validate_subject(&run.subject)?;
        let (_, requirement) = profile.current_requirement()?;
        let plan = skill_workflow::compile(&profile.data_dir, &requirement.instantiate(&context)?)?;
        skill_workflow::start(&profile.data_dir, &run.native_run_id, &plan)?;
        let work = private_directory(&profile.data_dir.join("execution"))?;
        let docker_config = PrivateDirectory::create(&work)?;
        let claude_work = PrivateDirectory::create(&work)?;
        let needs_container = plan
            .definition()
            .steps
            .iter()
            .any(|step| matches!(step.action, skill_workflow::StepAction::Command { .. }));
        let backend = if needs_container {
            match OciBackend::new(&profile.docker, &profile.docker_socket, &docker_config.0) {
                Ok(backend) => Some(backend),
                Err(_) => return Ok((ExecutionState::Failed,
                    "The operator-configured container runtime is unavailable; no workflow command or review was dispatched.".into())),
            }
        } else {
            None
        };
        let command_cancel = Cancellation::default();
        let review_cancel = ReviewCancellation::default();
        let command = backend.as_ref().map(|backend| CommandExecutor {
            backend,
            work_dir: &work,
            cancellation: &command_cancel,
        });
        let claude = ClaudeExecutor {
            executable: &profile.claude,
            work_dir: &claude_work.0,
            cancellation: &review_cancel,
        };
        let done = std::sync::atomic::AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    if check().is_err() {
                        command_cancel.cancel();
                        review_cancel.cancel();
                        let _ = skill_workflow::cancel(&profile.data_dir, &run.native_run_id);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let result = skill_workflow::runtime::advance_guarded(
                &profile.data_dir,
                &run.native_run_id,
                32,
                command.as_ref(),
                Some(&claude),
                &check,
            );
            done.store(true, std::sync::atomic::Ordering::Release);
            result
        });
        if check().is_err() {
            return Ok((ExecutionState::Stale,"Authorization, cancellation or native context changed during execution; no passing receipt was created.".into()));
        }
        let view = match result {
            Ok(view) => view,
            Err(error) => match skill_workflow::status(&profile.data_dir, &run.native_run_id) {
                Ok(view)
                    if matches!(
                        view.state,
                        RunState::Failed | RunState::Blocked | RunState::Cancelled
                    ) =>
                {
                    view
                }
                _ => return Err(error.into()),
            },
        };
        Ok(match view.state {
            RunState::Succeeded | RunState::CompletedWithOmissions => (
                ExecutionState::Succeeded,
                "Exact native workflow completed; authority receipt revalidation required".into(),
            ),
            RunState::Failed | RunState::Blocked => (
                ExecutionState::Failed,
                "Native workflow did not pass; inspect the retained workflow detail".into(),
            ),
            RunState::Cancelled => (
                ExecutionState::Cancelled,
                "Native workflow cancelled".into(),
            ),
            _ => (
                ExecutionState::Unknown,
                "Native workflow did not complete; this request will not be replayed".into(),
            ),
        })
    }
}

impl ExecutionProfile {
    fn stable_mapping(&self) -> Result<Authority> {
        let authority = Authority::open_read_only(&self.data_dir)?;
        if authority.repository_id()? != self.native_repository {
            return Err(HubError::Invalid(
                "native repository mapping changed".into(),
            ));
        }
        Ok(authority)
    }

    fn current_requirement(&self) -> Result<(Authority, WorkflowRequirement)> {
        let authority = self.stable_mapping()?;
        let requirement = authority
            .workflow_requirement(&self.target)?
            .ok_or_else(|| {
                HubError::Invalid("profile has no installed native requirement".into())
            })?;
        if requirement.digest()? != self.requirement_digest {
            return Err(HubError::Invalid(
                "native requirement differs from registered profile".into(),
            ));
        }
        Ok((authority, requirement))
    }
    fn validate_subject(&self, subject: &RunSubject) -> Result<ReviewContext> {
        let (authority, requirement) = self.current_requirement()?;
        let id = ChangeId {
            repository: self.native_repository.clone(),
            id: subject.change_id.clone(),
        };
        let change = authority.change(&id)?;
        let context = authority.review_context(&change.context_digest)?;
        let target = authority.target(&self.target)?;
        let base_current = match (&context.base, target) {
            (None, None) => true,
            (Some(base), Some(target)) => {
                base.revision == target.revision && base.target_version == target.version
            }
            _ => false,
        };
        if !base_current
            || subject.requirement_digest != requirement.digest()?
            || change.version != subject.change_version
            || change.context_digest != subject.context_digest
            || context.revision != subject.revision
            || context.target != self.target
            || context.policy_epoch != authority.policy_epoch()?
        {
            return Err(HubError::Invalid(
                "exact native workflow subject changed; refresh the subject".into(),
            ));
        }
        Ok(context)
    }
}

impl From<&ExecutionProfile> for AgentTaskProfile {
    fn from(profile: &ExecutionProfile) -> Self {
        Self {
            repository_id: profile.repository_id.clone(),
            profile_id: profile.profile_id.clone(),
            native_repository: profile.native_repository.clone(),
            data_dir: profile.data_dir.clone(),
            target: profile.target.clone(),
            requirement_digest: profile.requirement_digest.clone(),
            publish_forge_status: false,
        }
    }
}

impl AgentTaskProfile {
    pub(crate) fn action_subject(&self, subject: &RunSubject) -> Result<ReviewContext> {
        let authority = self.stable_mapping()?;
        let context = authority.review_context(&subject.context_digest)?;
        if subject.requirement_digest != self.requirement_digest
            || context.change.repository != self.native_repository
            || context.change.id != subject.change_id
            || context.change_version != subject.change_version
            || context.revision != subject.revision
            || context.target != self.target
        {
            return Err(HubError::Invalid("workspace_native_conflict".into()));
        }
        Ok(context)
    }

    pub(crate) fn stable_mapping(&self) -> Result<Authority> {
        let authority = Authority::open_read_only(&self.data_dir)?;
        if authority.repository_id()? != self.native_repository {
            return Err(HubError::Invalid(
                "native repository mapping changed".into(),
            ));
        }
        Ok(authority)
    }

    pub(crate) fn current_requirement(&self) -> Result<(Authority, WorkflowRequirement)> {
        let authority = self.stable_mapping()?;
        let requirement = authority
            .workflow_requirement(&self.target)?
            .ok_or_else(|| {
                HubError::Invalid("profile has no installed native requirement".into())
            })?;
        if requirement.digest()? != self.requirement_digest {
            return Err(HubError::Invalid(
                "native requirement differs from registered profile".into(),
            ));
        }
        Ok((authority, requirement))
    }

    pub(crate) fn validate_subject(&self, subject: &RunSubject) -> Result<ReviewContext> {
        let (authority, requirement) = self.current_requirement()?;
        let id = ChangeId {
            repository: self.native_repository.clone(),
            id: subject.change_id.clone(),
        };
        let change = authority.change(&id)?;
        let context = authority.review_context(&change.context_digest)?;
        let target = authority.target(&self.target)?;
        let base_current = match (&context.base, target) {
            (None, None) => true,
            (Some(base), Some(target)) => {
                base.revision == target.revision && base.target_version == target.version
            }
            _ => false,
        };
        if !base_current
            || subject.requirement_digest != requirement.digest()?
            || change.version != subject.change_version
            || change.context_digest != subject.context_digest
            || context.revision != subject.revision
            || context.target != self.target
            || context.policy_epoch != authority.policy_epoch()?
        {
            return Err(HubError::Invalid("workspace_native_conflict".into()));
        }
        Ok(context)
    }
}

impl Queue {
    fn get(&self, repository: &str, id: &str) -> Result<WorkspaceRun> {
        let bytes = self
            .db
            .query_row(
                "SELECT run_json FROM workspace_runs WHERE repository_id=?1 AND run_id=?2",
                params![repository, id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or(HubError::Unauthorized)?;
        decode(&bytes)
    }
    fn update(&mut self, run: &mut WorkspaceRun) -> Result<()> {
        run.generation = run
            .generation
            .checked_add(1)
            .ok_or_else(|| HubError::Unavailable("run generation exhausted".into()))?;
        let tx = self.db.transaction()?;
        let bytes = serde_json::to_vec(run).map_err(native_json)?;
        tx.execute(
            "UPDATE workspace_runs SET run_json=?1,state=?2 WHERE run_id=?3",
            params![bytes, state_name(run.state), run.run_id],
        )?;
        tx.execute(
            "INSERT INTO workspace_run_events(run_id,event,run_json) VALUES(?1,?2,?3)",
            params![run.run_id, state_name(run.state), bytes],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn all_active(&self) -> Result<Vec<WorkspaceRun>> {
        let mut stmt = self
            .db
            .prepare("SELECT run_json FROM workspace_runs WHERE state IN ('queued','running')")?;
        let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| decode(&row?)).collect()
    }
}

fn state_name(state: ExecutionState) -> &'static str {
    match state {
        ExecutionState::Queued => "queued",
        ExecutionState::Running => "running",
        ExecutionState::Succeeded => "succeeded",
        ExecutionState::Failed => "failed",
        ExecutionState::Cancelled => "cancelled",
        ExecutionState::Unknown => "unknown",
        ExecutionState::Stale => "stale",
    }
}
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(HubError::Invalid(
            "identifier must contain 1..64 ASCII letters, digits, hyphens or underscores".into(),
        ));
    }
    Ok(())
}
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| HubError::Unavailable("randomness unavailable".into()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(native_json)
}
fn native_json(error: serde_json::Error) -> HubError {
    HubError::Unavailable(error.to_string())
}
fn guard_error(_: HubError) -> crate::Error {
    crate::Error::Conflict(
        "workspace execution authorization or exact subject is no longer current".into(),
    )
}

fn private_directory(path: &Path) -> Result<PathBuf> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err(HubError::Invalid(
            "execution directory must be private".into(),
        ));
    }
    Ok(path.to_owned())
}
struct PrivateDirectory(PathBuf);
impl PrivateDirectory {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!("controller-{}", random_id()?));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests;
