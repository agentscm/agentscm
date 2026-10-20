//! Public operator and authenticated client commands for agent-owned workflow tasks.

use std::ffi::{CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::authority::Authority;
use agentscm::hub::workspace_agent_tasks::{
    AgentTaskEnvelope, AgentTaskRequest, CollectorUploadRequest, StatusInput,
};
use agentscm::hub::workspace_execution::RunSubject;
use agentscm::reviewer::{ClaudeCodeTransport, ModelTransport, ReviewCancellation};
use agentscm::skill_workflow::{
    AgentTaskService, CollectorProfile, CollectorProfileAction, CollectorTransfer,
    CollectorWorkDescriptor, ExecutorEnrollment, ExecutorRevocation, TrustedLocalCollector,
};
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use ed25519_dalek::SigningKey;
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_TASK_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN_BYTES: u64 = 16 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024 + 64 * 1024;
const MAX_UPLOAD_ENVELOPE_BYTES: usize =
    agentscm::skill_workflow::MAX_COLLECTOR_TRANSFER_BYTES + 16 * 1024;

#[derive(Debug, Subcommand)]
pub enum AgentTaskCommand {
    /// Generate a new raw Ed25519 private key file without overwriting an existing path.
    KeyGenerate { output: PathBuf },
    /// Print the public enrollment identifiers for a private key file.
    KeyInfo { private_key: PathBuf },
    /// Probe an explicitly selected Docker backend and print its profile binding.
    CommandRuntimeInfo {
        #[arg(long)]
        docker: PathBuf,
        #[arg(long)]
        docker_socket: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
    },
    /// Probe an explicitly selected Claude Code transport and print its profile binding.
    ReviewTransportInfo {
        #[arg(long)]
        claude: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
    },
    /// Enroll an executor using strict JSON and the matching private key file.
    Enroll {
        input: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
    },
    /// Revoke one exact executor registration epoch from strict JSON.
    Revoke { input: PathBuf },
    /// Install an immutable collector profile from strict JSON.
    ProfileInstall { input: PathBuf },
    /// Inspect an executor registration and optional collector profile.
    Inspect {
        registration_id: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Make one typed authenticated task operation through the shared HTTP facade.
    Call {
        #[command(flatten)]
        remote: RemoteArgs,
        /// Strict AgentTaskRequest JSON, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Upload one bounded signed CollectorUploadRequest JSON through the dedicated route.
    Upload {
        #[command(flatten)]
        remote: RemoteArgs,
        /// Strict CollectorUploadRequest JSON, or - for standard input.
        #[arg(default_value = "-")]
        input: PathBuf,
    },
    /// Run one exact server-derived collector descriptor and upload its retained transfer.
    Collect {
        #[command(flatten)]
        remote: RemoteArgs,
        /// Exact CollectorWorkDescriptor returned by collector_descriptor.
        #[arg(long)]
        descriptor: PathBuf,
        /// Owner-private durable collector state outside candidate source.
        #[arg(long)]
        state_dir: PathBuf,
        /// Matching owner-private raw 32-byte Ed25519 private key.
        #[arg(long)]
        private_key: PathBuf,
        /// Fresh stable upload identity; retries must reuse this value.
        #[arg(long)]
        transfer_request_id: String,
        /// Trusted absolute Docker CLI path for command profiles.
        #[arg(long, requires = "docker_socket")]
        docker: Option<PathBuf>,
        /// Trusted absolute local Docker socket for command profiles.
        #[arg(long, requires = "docker")]
        docker_socket: Option<PathBuf>,
        /// Trusted absolute Claude Code executable for review profiles.
        #[arg(long)]
        claude: Option<PathBuf>,
    },
    /// Upload the exact signed bytes retained by a prior collect; never reruns work.
    RetryUpload {
        #[command(flatten)]
        remote: RemoteArgs,
        #[arg(long)]
        descriptor: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        transfer_request_id: String,
    },
}

#[derive(Clone, Debug, Args)]
pub struct RemoteArgs {
    /// Workspace base URL. HTTPS is required except for IP-literal loopback HTTP.
    #[arg(long)]
    endpoint: String,
    /// Workspace repository (owner/name).
    #[arg(long)]
    repository: String,
    /// Operator-installed task profile.
    #[arg(long)]
    profile: String,
    /// Absolute owner-private bearer-token file.
    #[arg(long)]
    token_file: PathBuf,
}

pub async fn run(data_dir: &Path, command: AgentTaskCommand) -> Result<Value> {
    match command {
        AgentTaskCommand::KeyGenerate { output } => generate_key(&output),
        AgentTaskCommand::KeyInfo { private_key } => key_info(&private_key),
        AgentTaskCommand::CommandRuntimeInfo {
            docker,
            docker_socket,
            state_dir,
        } => {
            let state_dir = ensure_private_directory(&state_dir)?;
            let config = TemporaryPrivateDirectory::new(&state_dir, "docker-config")?;
            let backend =
                agentscm::execution::OciBackend::new(&docker, &docker_socket, config.path())?;
            let probe = backend.probe();
            let identity = probe.identity.filter(|_| probe.available).ok_or_else(|| {
                Error::Conflict(format!("OCI runtime unavailable: {}", probe.detail))
            })?;
            let scope = agentscm::skill_workflow::runtime_scope_digest(&identity);
            Ok(json!({"ok":true,"runtime_identity":identity,"runtime_scope_sha256":scope}))
        }
        AgentTaskCommand::ReviewTransportInfo { claude, state_dir } => {
            let state_dir = ensure_private_directory(&state_dir)?;
            let work = TemporaryPrivateDirectory::new(&state_dir, "claude-work")?;
            let transport = ClaudeCodeTransport::new(&claude, work.path())?;
            Ok(
                json!({"ok":true,"executable_sha256":transport.verified_executable_sha256()?,"provider_scope_sha256":transport.scope_sha256(),"transport_basis":transport.basis()}),
            )
        }
        AgentTaskCommand::Enroll { input, private_key } => {
            let enrollment: ExecutorEnrollment = read_json(&input)?;
            let key = read_signing_key(&private_key)?;
            let public = public_hex(&key);
            let mut authority = Authority::open(data_dir)?;
            let digest = authority.enroll_workflow_executor(&enrollment, &public)?;
            Ok(
                json!({"ok":true,"enrollment_digest":digest,"policy_epoch":authority.policy_epoch()?,"registration":enrollment}),
            )
        }
        AgentTaskCommand::Revoke { input } => {
            let revocation: ExecutorRevocation = read_json(&input)?;
            let mut authority = Authority::open(data_dir)?;
            let digest = authority.revoke_workflow_executor(&revocation)?;
            Ok(
                json!({"ok":true,"revocation_digest":digest,"policy_epoch":authority.policy_epoch()?,"revocation":revocation}),
            )
        }
        AgentTaskCommand::ProfileInstall { input } => {
            let profile: CollectorProfile = read_json(&input)?;
            let mut authority = Authority::open(data_dir)?;
            let digest = authority.install_workflow_collector_profile(&profile)?;
            Ok(json!({"ok":true,"profile_digest":digest,"profile":profile}))
        }
        AgentTaskCommand::Inspect {
            registration_id,
            profile,
        } => {
            let authority = Authority::open_read_only(data_dir)?;
            let registration = authority.workflow_executor_registration(&registration_id)?;
            let collector_profile = profile
                .as_deref()
                .map(|id| authority.workflow_collector_profile(id))
                .transpose()?
                .flatten();
            Ok(json!({"ok":true,"registration":registration,"collector_profile":collector_profile}))
        }
        AgentTaskCommand::Call { remote, input } => {
            let request: AgentTaskRequest = read_json(&input)?;
            Ok(serde_json::to_value(
                Client::new(&remote)?
                    .call(&request)
                    .await
                    .map_err(RemoteFailure::into_error)?,
            )?)
        }
        AgentTaskCommand::Upload { remote, input } => {
            let request: CollectorUploadRequest = read_json(&input)?;
            Ok(serde_json::to_value(
                Client::new(&remote)?
                    .upload(&request)
                    .await
                    .map_err(RemoteFailure::into_error)?,
            )?)
        }
        AgentTaskCommand::Collect {
            remote,
            descriptor,
            state_dir,
            private_key,
            transfer_request_id,
            docker,
            docker_socket,
            claude,
        } => {
            let descriptor: CollectorWorkDescriptor = read_json(&descriptor)?;
            let client = Client::new(&remote)?;
            let transfer = collect(LocalCollect {
                data_dir,
                client: &client,
                descriptor: &descriptor,
                state_dir: &state_dir,
                private_key: &private_key,
                transfer_request_id: &transfer_request_id,
                docker: docker.as_deref(),
                docker_socket: docker_socket.as_deref(),
                claude: claude.as_deref(),
            })
            .await?;
            upload_transfer(&client, &descriptor, transfer).await
        }
        AgentTaskCommand::RetryUpload {
            remote,
            descriptor,
            state_dir,
            private_key,
            transfer_request_id,
        } => {
            let descriptor: CollectorWorkDescriptor = read_json(&descriptor)?;
            descriptor.validate()?;
            let state_dir = ensure_private_directory(&state_dir)?;
            let signing_key = read_signing_key(&private_key)?;
            let collector = TrustedLocalCollector::new(
                &state_dir,
                descriptor.profile.clone(),
                signing_key.clone(),
            )?;
            let transfer = collector.retained_transfer(&transfer_request_id)?;
            validate_retained_retry(&descriptor, &transfer, &signing_key, &transfer_request_id)?;
            let client = Client::new(&remote)?;
            upload_transfer(&client, &descriptor, transfer).await
        }
    }
}

struct LocalCollect<'a> {
    data_dir: &'a Path,
    client: &'a Client,
    descriptor: &'a CollectorWorkDescriptor,
    state_dir: &'a Path,
    private_key: &'a Path,
    transfer_request_id: &'a str,
    docker: Option<&'a Path>,
    docker_socket: Option<&'a Path>,
    claude: Option<&'a Path>,
}

async fn collect(options: LocalCollect<'_>) -> Result<CollectorTransfer> {
    let LocalCollect {
        data_dir,
        client,
        descriptor,
        state_dir,
        private_key,
        transfer_request_id,
        docker,
        docker_socket,
        claude,
    } = options;
    let plan = AgentTaskService::compile_collector_descriptor(data_dir, descriptor)?;
    client
        .recheck(descriptor, true)
        .await
        .map_err(RemoteFailure::into_error)?;
    let state_dir = ensure_private_directory(state_dir)?;
    let collector = TrustedLocalCollector::new(
        &state_dir,
        descriptor.profile.clone(),
        read_signing_key(private_key)?,
    )?;
    let guard = || authenticated_recheck(client, descriptor);
    match &descriptor.profile.action {
        CollectorProfileAction::Command { .. } => {
            let docker = docker.ok_or_else(|| {
                Error::Invalid("command collection requires --docker and --docker-socket".into())
            })?;
            let socket = docker_socket.ok_or_else(|| {
                Error::Invalid("command collection requires --docker and --docker-socket".into())
            })?;
            if claude.is_some() {
                return Err(Error::Invalid(
                    "command collection does not accept --claude".into(),
                ));
            }
            let runtime = TemporaryPrivateDirectory::new(&state_dir, "docker-config")?;
            let backend = agentscm::execution::OciBackend::new(docker, socket, runtime.path())?;
            let cancellation = agentscm::execution::Cancellation::default();
            guard()?;
            collector.collect_command(
                data_dir,
                &plan,
                &descriptor.task,
                &descriptor.attempt,
                descriptor.origin.clone(),
                transfer_request_id,
                &backend,
                &cancellation,
                &guard,
            )
        }
        CollectorProfileAction::FiniteReview { .. } => {
            if docker.is_some() || docker_socket.is_some() {
                return Err(Error::Invalid(
                    "review collection does not accept Docker options".into(),
                ));
            }
            let executable = claude
                .ok_or_else(|| Error::Invalid("review collection requires --claude".into()))?;
            let work = TemporaryPrivateDirectory::new(&state_dir, "claude-work")?;
            guard()?;
            let transport = ClaudeCodeTransport::new(executable, work.path())?;
            collector
                .collect_review(
                    data_dir,
                    &plan,
                    &descriptor.task,
                    &descriptor.attempt,
                    descriptor.origin.clone(),
                    transfer_request_id,
                    &transport,
                    &ReviewCancellation::default(),
                    &guard,
                )
                .await
        }
    }
}

fn validate_retained_retry(
    descriptor: &CollectorWorkDescriptor,
    transfer: &CollectorTransfer,
    signing_key: &SigningKey,
    transfer_request_id: &str,
) -> Result<()> {
    transfer.verify_signature(&public_hex(signing_key))?;
    transfer.validate_for(
        &descriptor.task,
        &descriptor.attempt,
        &descriptor.profile,
        &descriptor.profile.authority_audience,
        &descriptor.origin,
    )?;
    if transfer.header.version != descriptor.version
        || transfer.header.capability != descriptor.capability
        || transfer.header.authority_audience != descriptor.profile.authority_audience
        || transfer.header.repository != descriptor.task.subject.repository
        || transfer.header.workflow_profile != descriptor.task.subject.profile
        || transfer.header.transfer_request_id != transfer_request_id
    {
        return Err(Error::Conflict(
            "retained collector transfer differs from the supplied descriptor or request identity"
                .into(),
        ));
    }
    Ok(())
}

fn authenticated_recheck(client: &Client, descriptor: &CollectorWorkDescriptor) -> Result<()> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(client.recheck(descriptor, true))
            .map_err(RemoteFailure::into_error)
    })
}

async fn upload_transfer(
    client: &Client,
    descriptor: &CollectorWorkDescriptor,
    transfer: CollectorTransfer,
) -> Result<Value> {
    let transfer_request_id = transfer.header.transfer_request_id.clone();
    let envelope = async {
        client.recheck(descriptor, false).await?;
        client
            .upload(&CollectorUploadRequest {
                subject: run_subject(descriptor),
                transfer,
            })
            .await
    }
    .await
    .map_err(|failure| retained_upload_error(&transfer_request_id, failure))?;
    Ok(serde_json::to_value(envelope)?)
}

#[derive(Debug)]
enum RemoteFailure {
    Authorization(Error),
    State(Error),
    Conclusive(Error),
    Uncertain(Error),
}

impl RemoteFailure {
    fn into_error(self) -> Error {
        match self {
            Self::Authorization(error)
            | Self::State(error)
            | Self::Conclusive(error)
            | Self::Uncertain(error) => error,
        }
    }
}

fn retained_upload_error(transfer_request_id: &str, failure: RemoteFailure) -> Error {
    let (status, next_action, error) = match failure {
        RemoteFailure::Authorization(error) => (
            "conclusive_authorization_rejection",
            "inspect_authenticated_status_then_obtain_fresh_authorization",
            error,
        ),
        RemoteFailure::State(error) => (
            "conclusive_state_conflict",
            "inspect_authenticated_status_then_reconcile_state",
            error,
        ),
        RemoteFailure::Conclusive(error) => (
            "conclusive_request_rejection",
            "stop_and_correct_local_or_remote_validation",
            error,
        ),
        RemoteFailure::Uncertain(error) => (
            "uncertain_remote_outcome",
            "inspect_authenticated_status_then_retry_identical_retained_transfer_if_safe",
            error,
        ),
    };
    let message = format!(
        "signed transfer {transfer_request_id} remains retained; recovery_status={status}; next_action={next_action}; cause={error}"
    );
    match error {
        Error::Limit(_) => Error::Limit(message),
        Error::Conflict(_) | Error::Io(_) if status == "conclusive_state_conflict" => {
            Error::Conflict(message)
        }
        _ => Error::Invalid(message),
    }
}

fn status_request(descriptor: &CollectorWorkDescriptor) -> Result<AgentTaskRequest> {
    Ok(AgentTaskRequest::Status {
        subject: run_subject(descriptor),
        status: StatusInput {
            version: descriptor.version,
            task_id: descriptor.task.task_id.clone(),
            task_digest: descriptor.task.digest()?,
            attempt_id: descriptor.attempt.attempt_id.clone(),
            attempt_digest: descriptor.attempt.digest()?,
            fence: descriptor.attempt.lease.fence,
            executor: descriptor.attempt.executor.clone(),
        },
    })
}

fn run_subject(descriptor: &CollectorWorkDescriptor) -> RunSubject {
    RunSubject {
        change_id: descriptor.task.subject.change.id.clone(),
        change_version: descriptor.task.subject.change_version,
        context_digest: descriptor.task.subject.context_digest.clone(),
        revision: descriptor.task.subject.revision.clone(),
        requirement_digest: descriptor.task.subject.requirement_digest.clone(),
    }
}

struct Client {
    http: reqwest::Client,
    endpoint: Url,
    token: String,
    repository: String,
    profile: String,
}

impl Client {
    fn new(args: &RemoteArgs) -> Result<Self> {
        let endpoint = vetted_url(&args.endpoint)?;
        validate_repository(&args.repository)?;
        validate_id(&args.profile, "profile")?;
        let token = read_private_file(&args.token_file, MAX_TOKEN_BYTES)?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| Error::Invalid("agent-task token is not UTF-8".into()))?
            .trim()
            .to_owned();
        if token.is_empty() || token.chars().any(char::is_whitespace) {
            return Err(Error::Invalid("agent-task token is invalid".into()));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Invalid("could not build agent-task HTTP client".into()))?;
        Ok(Self {
            http,
            endpoint,
            token,
            repository: args.repository.clone(),
            profile: args.profile.clone(),
        })
    }

    async fn recheck(
        &self,
        descriptor: &CollectorWorkDescriptor,
        require_leased: bool,
    ) -> std::result::Result<(), RemoteFailure> {
        let request = status_request(descriptor).map_err(RemoteFailure::Conclusive)?;
        let envelope = self.call(&request).await?;
        if envelope.capability != agentscm::skill_workflow::AGENT_TASK_CAPABILITY
            || envelope.schema_version != descriptor.version
        {
            return Err(RemoteFailure::State(Error::Conflict(
                "authenticated task status returned a different contract".into(),
            )));
        }
        let view = decode_status_result(envelope.result)?;
        let view = view.ok_or_else(|| {
            RemoteFailure::State(Error::Conflict(
                "authenticated task attempt is missing".into(),
            ))
        })?;
        if view.attempt != descriptor.attempt {
            return Err(RemoteFailure::State(Error::Conflict(
                "authenticated task status differs from the collector descriptor".into(),
            )));
        }
        if require_leased
            && (!view.currently_applicable
                || view.state != agentscm::skill_workflow::AgentAttemptState::Leased)
        {
            return Err(RemoteFailure::State(Error::Conflict(
                "collector attempt is no longer current and leased".into(),
            )));
        }
        Ok(())
    }

    async fn call(
        &self,
        request: &AgentTaskRequest,
    ) -> std::result::Result<AgentTaskEnvelope, RemoteFailure> {
        self.post(false, request).await
    }

    async fn upload(
        &self,
        request: &CollectorUploadRequest,
    ) -> std::result::Result<AgentTaskEnvelope, RemoteFailure> {
        let bytes = serde_json::to_vec(request)
            .map_err(Error::from)
            .map_err(RemoteFailure::Conclusive)?;
        if bytes.len() > MAX_UPLOAD_ENVELOPE_BYTES {
            return Err(RemoteFailure::Conclusive(Error::Limit(
                "collector upload request exceeds bounded envelope".into(),
            )));
        }
        self.post_bytes(true, bytes).await
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        upload: bool,
        request: &T,
    ) -> std::result::Result<AgentTaskEnvelope, RemoteFailure> {
        let bytes = serde_json::to_vec(request)
            .map_err(Error::from)
            .map_err(RemoteFailure::Conclusive)?;
        if bytes.len() > MAX_TASK_JSON_BYTES as usize {
            return Err(RemoteFailure::Conclusive(Error::Limit(
                "agent-task request exceeds 16 MiB".into(),
            )));
        }
        self.post_bytes(upload, bytes).await
    }

    async fn post_bytes(
        &self,
        upload: bool,
        bytes: Vec<u8>,
    ) -> std::result::Result<AgentTaskEnvelope, RemoteFailure> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| {
                RemoteFailure::Conclusive(Error::Invalid(
                    "agent-task endpoint cannot be a base URL".into(),
                ))
            })?
            .pop_if_empty()
            .extend([
                "v1",
                "hub",
                "workspace",
                "repositories",
                &self.repository,
                "profiles",
                &self.profile,
                "agent-tasks",
            ]);
        if upload {
            url.path_segments_mut()
                .expect("vetted URL")
                .push("collector-upload");
        }
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .send()
            .await
            .map_err(|_| {
                RemoteFailure::Uncertain(Error::Invalid("agent-task request failed".into()))
            })?;
        response_json(response).await
    }
}

fn decode_status_result(
    result: Value,
) -> std::result::Result<Option<agentscm::skill_workflow::TaskAttemptView>, RemoteFailure> {
    serde_json::from_value(result).map_err(|_| {
        RemoteFailure::Uncertain(Error::Invalid(
            "authenticated task status has invalid shape".into(),
        ))
    })
}

async fn response_json(
    mut response: reqwest::Response,
) -> std::result::Result<AgentTaskEnvelope, RemoteFailure> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(RemoteFailure::Authorization(Error::Invalid(
            "agent-task token was rejected or lacks permission".into(),
        )));
    }
    if status == StatusCode::CONFLICT {
        return Err(RemoteFailure::State(Error::Conflict(
            "agent-task state changed".into(),
        )));
    }
    if status.is_client_error() {
        return Err(RemoteFailure::Conclusive(Error::Invalid(format!(
            "agent-task request failed with status {}",
            status.as_u16()
        ))));
    }
    if status.is_server_error() || !status.is_success() {
        return Err(RemoteFailure::Uncertain(Error::Invalid(format!(
            "agent-task request failed with status {}",
            status.as_u16()
        ))));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        || response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some()
    {
        return Err(RemoteFailure::Uncertain(Error::Limit(
            "agent-task response is encoded or too large".into(),
        )));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .map(str::trim);
    if content_type != Some("application/json") {
        return Err(RemoteFailure::Uncertain(Error::Invalid(
            "agent-task response has unexpected content type".into(),
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        RemoteFailure::Uncertain(Error::Invalid("agent-task response was interrupted".into()))
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(RemoteFailure::Uncertain(Error::Limit(
                "agent-task response is too large".into(),
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        RemoteFailure::Uncertain(Error::Invalid("agent-task response is invalid JSON".into()))
    })
}

fn generate_key(path: &Path) -> Result<Value> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "private key output path must be absolute".into(),
        ));
    }
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Invalid("random source unavailable".into()))?;
    let parent = VerifiedPrivateParent::for_leaf(path, "private key")?;
    let mut file = parent.open_leaf(path, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
    validate_private_file(&file.metadata()?, 32)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    validate_private_file(&file.metadata()?, 32)?;
    parent.directory.sync_all()?;
    key_value(&SigningKey::from_bytes(&bytes))
}

fn key_info(path: &Path) -> Result<Value> {
    let key = read_signing_key(path)?;
    key_value(&key)
}

fn key_value(key: &SigningKey) -> Result<Value> {
    let public_key_hex = public_hex(key);
    Ok(json!({
        "ok":true,
        "public_key_hex":public_key_hex,
        "verification_key_id":agentscm::policy::issuer_key_id(&public_key_hex)?,
        "verification_key_sha256":format!("{:x}",Sha256::digest(key.verifying_key().to_bytes()))
    }))
}

fn read_signing_key(path: &Path) -> Result<SigningKey> {
    let bytes = read_private_file(path, 32)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        Error::Invalid("collector private key must contain exactly 32 raw bytes".into())
    })?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn public_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_private_file(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "credential file path must be absolute".into(),
        ));
    }
    let parent = VerifiedPrivateParent::for_leaf(path, "credential")?;
    let mut file = parent.open_leaf(path, libc::O_RDONLY, 0)?;
    validate_private_file(&file.metadata()?, maximum)?;
    let capacity = usize::try_from(maximum).unwrap_or(0).saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(Error::Limit("credential file is too large".into()));
    }
    validate_private_file(&file.metadata()?, maximum)?;
    Ok(bytes)
}

fn validate_private_file(metadata: &fs::Metadata, maximum: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid only reads the effective process identity and has no preconditions.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != owner
        || metadata.nlink() != 1
    {
        return Err(Error::Invalid(
            "credential file must be an owner-private regular file with one link".into(),
        ));
    }
    if metadata.len() > maximum {
        return Err(Error::Limit("credential file is too large".into()));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = if path == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(MAX_TASK_JSON_BYTES + 1)
            .read_to_end(&mut bytes)?;
        bytes
    } else {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_TASK_JSON_BYTES + 1)
            .read_to_end(&mut bytes)?;
        bytes
    };
    if bytes.len() as u64 > MAX_TASK_JSON_BYTES {
        return Err(Error::Limit("agent-task JSON input exceeds 16 MiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn vetted_url(input: &str) -> Result<Url> {
    let normalized = agentscm::hub::forge_http::check_base_url(input)
        .map_err(|_| Error::Invalid("invalid agent-task endpoint".into()))?;
    let parsed = Url::parse(&normalized)
        .map_err(|_| Error::Invalid("invalid agent-task endpoint".into()))?;
    if parsed.path() != "/" {
        return Err(Error::Invalid(
            "agent-task endpoint must not contain a path".into(),
        ));
    }
    Ok(parsed)
}

fn validate_repository(value: &str) -> Result<()> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err(Error::Invalid(
            "workspace repository must be owner/name".into(),
        ));
    };
    validate_id(owner, "repository owner")?;
    validate_id(name, "repository name")
}

fn validate_id(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid(format!("invalid {kind}")));
    }
    Ok(())
}

struct TemporaryPrivateDirectory(PathBuf);

struct VerifiedPrivateParent {
    directory: fs::File,
    canonical: PathBuf,
}

impl VerifiedPrivateParent {
    fn for_leaf(path: &Path, kind: &str) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid(format!("{kind} path must be absolute")));
        }
        let name = path
            .file_name()
            .filter(|name| {
                !name.is_empty() && *name != OsStr::new(".") && *name != OsStr::new("..")
            })
            .ok_or_else(|| Error::Invalid(format!("{kind} path must name one leaf")))?;
        if name.as_bytes().contains(&b'/') {
            return Err(Error::Invalid(format!("{kind} path has an invalid leaf")));
        }
        let canonical = fs::canonicalize(
            path.parent()
                .ok_or_else(|| Error::Invalid(format!("{kind} path has no parent")))?,
        )?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&canonical)?;
        validate_private_directory_metadata(&directory.metadata()?)?;
        Ok(Self {
            directory,
            canonical,
        })
    }

    fn leaf_name<'a>(&self, path: &'a Path) -> Result<&'a OsStr> {
        path.file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Invalid("private path must name one leaf".into()))
    }

    fn open_leaf(&self, path: &Path, flags: i32, mode: libc::c_uint) -> Result<fs::File> {
        let name = CString::new(self.leaf_name(path)?.as_bytes())
            .map_err(|_| Error::Invalid("private path contains a NUL byte".into()))?;
        // SAFETY: the directory descriptor and NUL-terminated leaf remain live for this call;
        // flags reject a final symlink, and the returned descriptor is uniquely owned.
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a fresh owned descriptor on success.
        Ok(unsafe { fs::File::from_raw_fd(descriptor) })
    }

    fn create_directory(&self, path: &Path) -> Result<()> {
        let name = CString::new(self.leaf_name(path)?.as_bytes())
            .map_err(|_| Error::Invalid("private path contains a NUL byte".into()))?;
        // SAFETY: the verified directory descriptor and NUL-terminated leaf are valid.
        let result = unsafe { libc::mkdirat(self.directory.as_raw_fd(), name.as_ptr(), 0o700) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        Ok(())
    }
}

fn validate_private_directory_metadata(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid only reads the effective process identity and has no preconditions.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != owner {
        return Err(Error::Invalid(
            "private path parent must be an owner-private directory".into(),
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "collector state directory must be absolute".into(),
        ));
    }
    let parent = VerifiedPrivateParent::for_leaf(path, "collector state directory")?;
    parent.create_directory(path)?;
    let directory = parent.open_leaf(path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    validate_private_directory_metadata(&directory.metadata()?)?;
    parent.directory.sync_all()?;
    Ok(parent.canonical.join(parent.leaf_name(path)?))
}

impl TemporaryPrivateDirectory {
    fn new(parent: &Path, prefix: &str) -> Result<Self> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| Error::Invalid("random source unavailable".into()))?;
        let path = parent.join(format!(
            "{prefix}-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let verified = VerifiedPrivateParent::for_leaf(&path, "temporary directory")?;
        verified.create_directory(&path)?;
        let directory = verified.open_leaf(&path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        validate_private_directory_metadata(&directory.metadata()?)?;
        verified.directory.sync_all()?;
        Ok(Self(verified.canonical.join(verified.leaf_name(&path)?)))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryPrivateDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_private_parent_resists_path_substitution() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let parent = root.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let leaf = parent.join("credential");
        let verified = VerifiedPrivateParent::for_leaf(&leaf, "test credential").unwrap();

        let moved = root.path().join("moved-parent");
        fs::rename(&parent, &moved).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(parent.join("credential"), b"replacement").unwrap();

        let mut opened = verified
            .open_leaf(&leaf, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
            .unwrap();
        opened.write_all(b"original").unwrap();
        opened.sync_all().unwrap();
        assert_eq!(fs::read(moved.join("credential")).unwrap(), b"original");
        assert_eq!(fs::read(parent.join("credential")).unwrap(), b"replacement");
    }

    async fn raw_response(response: &'static [u8]) -> RemoteFailure {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(response).unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let failure = response_json(response).await.unwrap_err();
        server.join().unwrap();
        failure
    }

    #[tokio::test]
    async fn response_outcomes_preserve_recovery_classification() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
            );
            assert!(matches!(
                raw_response(Box::leak(response.into_boxed_str()).as_bytes()).await,
                RemoteFailure::Authorization(_)
            ));
        }
        assert!(matches!(
            raw_response(b"HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await,
            RemoteFailure::State(_)
        ));
        assert!(matches!(
            raw_response(b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await,
            RemoteFailure::Conclusive(_)
        ));
        assert!(matches!(
            raw_response(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}").await,
            RemoteFailure::Uncertain(_)
        ));
        assert!(matches!(
            raw_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await,
            RemoteFailure::Uncertain(_)
        ));
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\nx".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16842753\r\n\r\n".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".as_slice(),
        ] {
            assert!(matches!(
                raw_response(response).await,
                RemoteFailure::Uncertain(_)
            ));
        }
        assert!(matches!(
            decode_status_result(json!({"unexpected":"shape"})),
            Err(RemoteFailure::Uncertain(_))
        ));

        let authorization = retained_upload_error(
            "transfer",
            RemoteFailure::Authorization(Error::Invalid("rejected".into())),
        );
        assert!(
            authorization
                .to_string()
                .contains("conclusive_authorization_rejection")
        );
        let state = retained_upload_error(
            "transfer",
            RemoteFailure::State(Error::Conflict("changed".into())),
        );
        assert!(matches!(state, Error::Conflict(_)));
        let client = retained_upload_error(
            "transfer",
            RemoteFailure::Conclusive(Error::Limit("large".into())),
        );
        assert!(matches!(client, Error::Limit(_)));
        let uncertain = retained_upload_error(
            "transfer",
            RemoteFailure::Uncertain(Error::Invalid("interrupted".into())),
        );
        assert!(
            uncertain
                .to_string()
                .contains("inspect_authenticated_status_then_retry_identical")
        );
        assert!(
            uncertain
                .to_string()
                .contains("retry_identical_retained_transfer_if_safe")
        );
    }
}
