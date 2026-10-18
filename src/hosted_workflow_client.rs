//! Bounded laptop client for the operator-pinned hosted workflow source lane.
//!
//! The laptop owns only immutable source and its native workspace. The server
//! derives repository authority, review context, workflow, approval and landing
//! state from the authenticated actor and operator profile.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::context::ChangeId;
use agentscm::hub::workspace_execution::source::{
    ContextDeclaration, MAX_SOURCE_BUNDLE_BYTES, MAX_SOURCE_OBJECTS, MAX_SOURCE_OFFER_BYTES,
    SOURCE_TRANSFER_VERSION, SourceBootstrap, SourceDownloadRequest, SourceTransfer,
    SourceTransferRequest, SourceTransferState,
};
use agentscm::hub::workspace_execution::{ExecutionState, RunSubject, StartRequest, WorkspaceRun};
use agentscm::merge::{MergeJournal, MergeOutcome, MergePhase};
use agentscm::model::{EntryChange, EntryKind};
use agentscm::skill_workflow::{RunState, StepState};
use agentscm::skill_workflow_service::SkillWorkflowProjection;
use agentscm::store::Engine;
use agentscm::sync::{self, SourceOffer};
use agentscm::workspace::{OperationKind, OperationOutcome, Workspace};
use agentscm::{Error, Result};
use clap::Args;
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CLIENT_VERSION: u32 = 1;
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_REQUEST_JSON_BYTES: usize = 3 * 1024 * 1024;
const MAX_TOKEN_BYTES: u64 = 16 * 1024;
const MAX_WORKSPACE_CHANGES: usize = 4_096;
const MAX_ANCESTRY_REVISIONS: usize = 100_000;
const MAX_FAILURE_BYTES: usize = 16 * 1024;
const MAX_DETAIL_BYTES: usize = 4 * 1024;
const MAX_JOURNAL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Args)]
pub struct HostedWorkflowArgs {
    /// Hosted workspace base URL. HTTPS is required except for an IP-literal loopback.
    #[arg(long)]
    pub endpoint: String,
    /// Hosted repository selected by the operator profile (owner/name).
    #[arg(long)]
    pub repository: String,
    /// Operator-installed hosted execution profile.
    #[arg(long)]
    pub profile: String,
    /// Absolute path to the private agent-token file.
    #[arg(long)]
    pub token_file: PathBuf,
    /// Native laptop workspace containing the candidate bytes.
    #[arg(long)]
    pub workspace: PathBuf,
    /// Stable laptop-side Change key.
    #[arg(long)]
    pub change: String,
    /// Maximum run-status observations in one invocation.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=60))]
    pub poll_attempts: u16,
    /// Delay between run-status observations.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(10..=5000))]
    pub poll_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ClientJournal {
    version: u32,
    scope_sha256: String,
    repository_id: String,
    profile_id: String,
    actor_id: String,
    native_repository: String,
    source_repository: String,
    source_change: String,
    expected_change_version: u64,
    pending_source_request: Option<SourceTransferRequest>,
    transfer: Option<SourceTransfer>,
    completed_binding: Option<CompletedBinding>,
    pending_run_request: Option<StartRequest>,
    run: Option<WorkspaceRun>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CompletedBinding {
    template_context_digest: String,
    requirement_digest: String,
    target: Option<agentscm::model::TargetState>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowEnvelope {
    version: u32,
    availability: Availability,
    projection: Option<SkillWorkflowProjection>,
    reason: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Availability {
    Available,
    Unavailable,
}

struct Remote {
    client: reqwest::Client,
    base: Url,
    token: String,
    repository: String,
    profile: String,
}

pub async fn initialize(data_dir: &Path, args: &HostedWorkflowArgs) -> Result<Value> {
    require_separate_workspace(data_dir, &args.workspace)?;
    let token_file = checked_external_file_path(data_dir, args)?;
    let remote = Remote::new(args, &token_file)?;
    let bootstrap = remote.bootstrap().await?;
    validate_bootstrap(args, &bootstrap)?;
    let scope = scope_sha256(args)?;
    validate_retained_journal_binding(data_dir, args, &bootstrap, &scope)?;
    if data_dir.try_exists()? && journal_path(data_dir, &scope)?.exists() {
        return Err(Error::Conflict(
            "hosted workflow is already initialized for this workspace; use remote-check to recover retained operations"
                .into(),
        ));
    }
    let engine = Engine::open(data_dir)?;
    import_bootstrap(data_dir, &engine, &remote, &bootstrap).await?;
    let seed = bootstrap
        .target
        .as_ref()
        .map(|target| target.revision.clone())
        .unwrap_or_else(|| bootstrap.template_context.revision.clone());
    require_offer_root(&bootstrap.offer, &seed)?;
    let change = ChangeId {
        repository: bootstrap.native_repository.clone(),
        id: args.change.clone(),
    };
    let workspace = Workspace::create(&engine, &args.workspace, change, &seed)?;
    let state = workspace.state()?;
    let journal = ClientJournal {
        version: CLIENT_VERSION,
        scope_sha256: scope.clone(),
        repository_id: args.repository.clone(),
        profile_id: args.profile.clone(),
        actor_id: bootstrap.actor_id,
        native_repository: bootstrap.native_repository,
        source_repository: state.change.repository.clone(),
        source_change: state.change.id.clone(),
        expected_change_version: 0,
        pending_source_request: None,
        transfer: None,
        completed_binding: None,
        pending_run_request: None,
        run: None,
    };
    save_journal(data_dir, &scope, &journal)?;
    Ok(
        json!({"ok":true,"workspace":state,"seed_revision":seed,"hosted_repository":args.repository,"profile":args.profile}),
    )
}

pub async fn check(data_dir: &Path, args: &HostedWorkflowArgs) -> Result<Value> {
    require_separate_workspace(data_dir, &args.workspace)?;
    let token_file = checked_external_file_path(data_dir, args)?;
    let remote = Remote::new(args, &token_file)?;
    let bootstrap = remote.bootstrap().await?;
    validate_bootstrap(args, &bootstrap)?;
    let scope = scope_sha256(args)?;
    validate_retained_journal_binding(data_dir, args, &bootstrap, &scope)?;
    let engine = Engine::open(data_dir)?;
    let mut workspace = Workspace::open(&args.workspace)?;
    let initial = workspace.state()?;
    if initial.change.id != args.change {
        return Err(Error::Conflict(
            "hosted workflow workspace belongs to a different Change".into(),
        ));
    }
    let mut journal = load_or_create_journal(data_dir, args, &bootstrap, &initial.change)?;
    validate_journal(args, &bootstrap, &scope, &journal, &initial.change)?;

    if let Some(request) = journal.pending_run_request.clone() {
        let run = run_workflow(
            data_dir,
            &remote,
            &bootstrap,
            &request.subject,
            &mut journal,
            &scope,
            args,
        )
        .await?;
        if matches!(run.state, ExecutionState::Queued | ExecutionState::Running) {
            return Ok(blocked(
                vec![
                    json!({"state":run_state(run.state),"detail":bounded(&run.detail,MAX_DETAIL_BYTES)}),
                ],
                "The retained hosted workflow is still queued or running. Wait, then retry this exact operation.",
            ));
        }
        if run.state == ExecutionState::Unknown {
            return Ok(blocked(
                vec![json!({"state":"unknown","detail":bounded(&run.detail,MAX_DETAIL_BYTES)})],
                "The retained hosted workflow outcome is unknown. Inspect its server evidence before creating fresh work.",
            ));
        }
    }

    if let Some(request) = journal.pending_source_request.clone() {
        let transfer = synchronize_source(
            data_dir,
            &engine,
            &remote,
            &bootstrap,
            &request.source_revision,
            &mut journal,
            &scope,
        )
        .await?;
        if transfer.state != SourceTransferState::Complete {
            return Ok(blocked(
                vec![
                    json!({"state":transfer_state(&transfer.state),"detail":bounded(&transfer.detail,MAX_DETAIL_BYTES)}),
                ],
                "The retained hosted source operation is still in progress. Retry this exact operation before submitting newer workspace bytes.",
            ));
        }
        let Some(subject) = transfer.subject.as_ref() else {
            return Ok(blocked(
                vec![json!({"state":"complete","detail":"retained transfer has no exact subject"})],
                "Inspect the retained hosted source operation before creating fresh work.",
            ));
        };
        journal.expected_change_version = subject.change_version;
        save_journal(data_dir, &scope, &journal)?;
    }

    import_bootstrap(data_dir, &engine, &remote, &bootstrap).await?;

    let local_revision = checkpoint_current(&engine, &mut workspace, &initial.change)?;
    let refreshed = refresh_target(
        data_dir,
        &engine,
        &mut workspace,
        &journal.source_repository,
        &local_revision,
        bootstrap
            .target
            .as_ref()
            .map(|target| target.revision.as_str()),
    )?;
    let workspace_after_refresh = workspace.state()?;
    let upload_revision = normalized_upload_revision(
        &engine,
        bootstrap
            .target
            .as_ref()
            .map(|target| target.revision.as_str()),
        &refreshed,
    )?;
    if engine.load_revision(&upload_revision)?.tree
        != engine
            .load_revision(&workspace_after_refresh.base_revision)?
            .tree
    {
        return Err(Error::Conflict(
            "normalized upload does not match the current workspace bytes".into(),
        ));
    }

    let transfer = synchronize_source(
        data_dir,
        &engine,
        &remote,
        &bootstrap,
        &upload_revision,
        &mut journal,
        &scope,
    )
    .await?;
    let Some(subject) = transfer.subject.clone() else {
        return Ok(blocked(
            vec![
                json!({"state":transfer_state(&transfer.state),"detail":bounded(&transfer.detail,MAX_DETAIL_BYTES)}),
            ],
            "The hosted source transfer has no exact workflow subject. Retry with the same operation after inspecting its retained state.",
        ));
    };
    if transfer.input_revision != upload_revision {
        return Err(Error::Conflict(
            "hosted source result is bound to a different input revision".into(),
        ));
    }
    journal.expected_change_version = subject.change_version;
    let run = run_workflow(
        data_dir,
        &remote,
        &bootstrap,
        &subject,
        &mut journal,
        &scope,
        args,
    )
    .await?;

    let current_subject = remote.subject(&subject.change_id).await?;
    let after = workspace.state()?;
    let status = workspace.status(&engine, MAX_WORKSPACE_CHANGES)?;
    let workspace_changed = after.change != initial.change
        || !after.ready
        || after.pending.is_some()
        || after.generation != workspace_after_refresh.generation
        || after.base_revision != workspace_after_refresh.base_revision
        || !status.complete
        || !status.changes.is_empty();
    let exact_success = run.state == ExecutionState::Succeeded
        && run.receipt.is_some()
        && run.subject == subject
        && current_subject == subject
        && !workspace_changed;
    if exact_success {
        let receipt = run.receipt.clone();
        return Ok(
            json!({"context_subject":subject,"run":run,"receipt":receipt,"workspace_changed_during_run":false,"failures":[],"next_action":"The exact current bytes passed the hosted workflow. A human may now inspect them for approval; no approval or landing was performed."}),
        );
    }
    let failures = workflow_failures(&remote, &run).await;
    let next = if workspace_changed {
        "Workspace bytes changed during hosted verification. Checkpoint the new bytes and run the workflow again."
    } else if current_subject != subject {
        "The hosted Change moved during verification. Refresh it and run the workflow again for the new exact subject."
    } else if matches!(run.state, ExecutionState::Queued | ExecutionState::Running) {
        "The hosted workflow is still queued or running. Wait, then retry the same check to observe its retained result."
    } else if run.state == ExecutionState::Unknown {
        "The hosted workflow outcome is unknown. Inspect retained server evidence before starting fresh work."
    } else {
        "Fix the reported hosted workflow failures and run it again for the current bytes."
    };
    Ok(
        json!({"context_subject":subject,"run":run,"receipt":Value::Null,"workspace_changed_during_run":workspace_changed,"failures":failures,"next_action":next}),
    )
}

pub async fn claude_stop(data_dir: &Path, args: HostedWorkflowArgs) -> Result<()> {
    let input = read_hook_payload()?;
    if input.get("hook_event_name").and_then(Value::as_str) != Some("Stop") {
        return Err(Error::Invalid(
            "hosted Claude workflow hook expects hook_event_name Stop".into(),
        ));
    }
    let result = match check(data_dir, &args).await {
        Ok(value) => value,
        Err(error) => blocked(
            vec![json!({"detail":bounded(&error.to_string(),MAX_DETAIL_BYTES)})],
            "Resolve the hosted workflow setup or execution error, then try to finish again.",
        ),
    };
    if result.get("receipt").is_some_and(|value| !value.is_null())
        && result
            .get("workspace_changed_during_run")
            .and_then(Value::as_bool)
            == Some(false)
    {
        println!("{{}}");
    } else {
        let feedback = json!({
            "failures":result.get("failures").cloned().unwrap_or_else(||json!([])),
            "next_action":result.get("next_action").cloned().unwrap_or_else(||json!("Retry the hosted workflow for the current bytes.")),
        });
        let reason = bounded(&serde_json::to_string(&feedback)?, MAX_FAILURE_BYTES);
        println!("{}", json!({"decision":"block","reason":reason}));
    }
    Ok(())
}

impl Remote {
    fn new(args: &HostedWorkflowArgs, token_file: &Path) -> Result<Self> {
        validate_repository(&args.repository)?;
        validate_hosted_id("profile", &args.profile)?;
        validate_segment("change", &args.change)?;
        let base = vetted_url(&args.endpoint)?;
        let token = read_token(token_file)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Invalid("could not build hosted workflow HTTP client".into()))?;
        Ok(Self {
            client,
            base,
            token,
            repository: args.repository.clone(),
            profile: args.profile.clone(),
        })
    }

    fn source_url(&self) -> Result<Url> {
        self.url(&[
            "v1",
            "hub",
            "workspace",
            "repositories",
            &self.repository,
            "profiles",
            &self.profile,
            "source",
        ])
    }

    fn repository_url(&self, suffix: &[&str]) -> Result<Url> {
        let mut parts = vec!["v1", "hub", "workspace", "repositories", &self.repository];
        parts.extend_from_slice(suffix);
        self.url(&parts)
    }

    fn url(&self, parts: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                Error::Invalid("hosted workflow endpoint cannot be a base URL".into())
            })?;
            path.pop_if_empty();
            for part in parts {
                path.push(part);
            }
        }
        Ok(url)
    }

    async fn bootstrap(&self) -> Result<SourceBootstrap> {
        let mut url = self.source_url()?;
        url.path_segments_mut()
            .expect("vetted URL")
            .push("bootstrap");
        self.json(Method::GET, url, None::<&Value>, MAX_JSON_BYTES)
            .await
    }

    async fn download(&self, request: &SourceDownloadRequest) -> Result<Vec<u8>> {
        let mut url = self.source_url()?;
        url.path_segments_mut()
            .expect("vetted URL")
            .push("download");
        self.bytes(Method::POST, url, Some(request), MAX_SOURCE_BUNDLE_BYTES)
            .await
    }

    async fn offer(&self, request: &SourceTransferRequest) -> Result<SourceTransfer> {
        let mut url = self.source_url()?;
        url.path_segments_mut().expect("vetted URL").push("offers");
        self.json(Method::POST, url, Some(request), MAX_JSON_BYTES)
            .await
    }

    async fn upload(&self, transfer: &str, bundle: Vec<u8>) -> Result<SourceTransfer> {
        let mut url = self.source_url()?;
        url.path_segments_mut()
            .expect("vetted URL")
            .extend(["transfers", transfer]);
        self.raw(Method::PUT, url, Some(bundle), MAX_JSON_BYTES)
            .await
    }

    async fn transfer(&self, transfer: &str) -> Result<SourceTransfer> {
        let mut url = self.source_url()?;
        url.path_segments_mut()
            .expect("vetted URL")
            .extend(["transfers", transfer]);
        self.json(Method::GET, url, None::<&Value>, MAX_JSON_BYTES)
            .await
    }

    async fn start(&self, request: &StartRequest) -> Result<WorkspaceRun> {
        self.json(
            Method::POST,
            self.repository_url(&["runs"])?,
            Some(request),
            MAX_JSON_BYTES,
        )
        .await
    }

    async fn run(&self, run: &str) -> Result<WorkspaceRun> {
        self.json(
            Method::GET,
            self.repository_url(&["runs", run])?,
            None::<&Value>,
            MAX_JSON_BYTES,
        )
        .await
    }

    async fn workflow(&self, run: &str) -> Result<WorkflowEnvelope> {
        self.json(
            Method::GET,
            self.repository_url(&["runs", run, "workflow"])?,
            None::<&Value>,
            MAX_JSON_BYTES,
        )
        .await
    }

    async fn subject(&self, change: &str) -> Result<RunSubject> {
        let mut url = self.repository_url(&["profiles", &self.profile, "subject"])?;
        url.query_pairs_mut().append_pair("change_id", change);
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            subject: RunSubject,
        }
        Ok(self
            .json::<_, Reply>(Method::GET, url, None::<&Value>, MAX_JSON_BYTES)
            .await?
            .subject)
    }

    async fn json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<&B>,
        maximum: usize,
    ) -> Result<T> {
        let bytes = self
            .exchange(method, url, body, maximum, "application/json")
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::Invalid("hosted workflow returned invalid JSON".into()))
    }

    async fn bytes<B: Serialize + ?Sized>(
        &self,
        method: Method,
        url: Url,
        body: Option<&B>,
        maximum: usize,
    ) -> Result<Vec<u8>> {
        self.exchange(method, url, body, maximum, "application/octet-stream")
            .await
    }

    async fn raw<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        maximum: usize,
    ) -> Result<T> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body);
        }
        let bytes = response_bytes(request, maximum, "application/json").await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::Invalid("hosted workflow returned invalid JSON".into()))
    }

    async fn exchange<B: Serialize + ?Sized>(
        &self,
        method: Method,
        url: Url,
        body: Option<&B>,
        maximum: usize,
        accept: &'static str,
    ) -> Result<Vec<u8>> {
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, accept);
        if let Some(body) = body {
            let body = serde_json::to_vec(body)?;
            if body.len() > MAX_REQUEST_JSON_BYTES {
                return Err(Error::Limit(
                    "hosted workflow JSON request is too large".into(),
                ));
            }
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        response_bytes(request, maximum, accept).await
    }
}

async fn response_bytes(
    request: reqwest::RequestBuilder,
    maximum: usize,
    expected_type: &str,
) -> Result<Vec<u8>> {
    let mut response = request
        .send()
        .await
        .map_err(|_| Error::Invalid("hosted workflow request failed".into()))?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(Error::Invalid(
            "hosted workflow token was rejected or lacks permission".into(),
        ));
    }
    if status == StatusCode::CONFLICT {
        return Err(Error::Conflict("hosted workflow state changed".into()));
    }
    if !status.is_success() {
        return Err(Error::Invalid(format!(
            "hosted workflow request failed with status {}",
            status.as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(Error::Limit("hosted workflow response is too large".into()));
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some()
    {
        return Err(Error::Invalid(
            "hosted workflow response encoding is unsupported".into(),
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .map(str::trim);
    if content_type != Some(expected_type) {
        return Err(Error::Invalid(
            "hosted workflow returned an unexpected content type".into(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::Invalid("hosted workflow response was interrupted".into()))?
    {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(Error::Limit("hosted workflow response is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_bootstrap(args: &HostedWorkflowArgs, bootstrap: &SourceBootstrap) -> Result<()> {
    if bootstrap.version != SOURCE_TRANSFER_VERSION
        || bootstrap.repository_id != args.repository
        || bootstrap.profile_id != args.profile
        || bootstrap.offer.origin_repository != bootstrap.native_repository
        || bootstrap.offer.roots.len() != 1
        || bootstrap.offer.objects.len() > MAX_SOURCE_OBJECTS
        || serde_json::to_vec(&bootstrap.offer)?.len() > MAX_SOURCE_OFFER_BYTES
    {
        return Err(Error::Conflict(
            "hosted source bootstrap does not match the selected profile".into(),
        ));
    }
    let seed = bootstrap
        .target
        .as_ref()
        .map(|target| target.revision.as_str())
        .unwrap_or(&bootstrap.template_context.revision);
    require_offer_root(&bootstrap.offer, seed)
}

fn require_offer_root(offer: &SourceOffer, revision: &str) -> Result<()> {
    if offer.roots.as_slice() != [revision] {
        return Err(Error::Conflict(
            "hosted source offer does not contain the authoritative seed revision".into(),
        ));
    }
    Ok(())
}

async fn import_bootstrap(
    data_dir: &Path,
    engine: &Engine,
    remote: &Remote,
    bootstrap: &SourceBootstrap,
) -> Result<()> {
    let digest = canonical_digest(&bootstrap.offer)?;
    let directory = private_directory(data_dir, &format!("bootstrap-{digest}"))?;
    let offer_path = directory.join("offer.json");
    write_or_match(&offer_path, &serde_json::to_vec(&bootstrap.offer)?)?;
    let request_path = directory.join("request.json");
    let request = if request_path.exists() {
        read_json_file(&request_path, MAX_SOURCE_OFFER_BYTES)?
    } else {
        sync::write_request(engine, &offer_path, &request_path)?
    };
    let bundle_path = directory.join("source.bundle");
    if !bundle_path.exists() {
        let bytes = remote
            .download(&SourceDownloadRequest {
                offer: bootstrap.offer.clone(),
                request,
            })
            .await?;
        write_new(&bundle_path, &bytes)?;
    }
    let transfer = directory.join("import");
    sync::import_bundle(engine, &bundle_path, &transfer)?;
    Ok(())
}

fn checkpoint_current(
    engine: &Engine,
    workspace: &mut Workspace,
    change: &ChangeId,
) -> Result<String> {
    let before = workspace.state()?;
    if &before.change != change || !before.ready {
        return Err(Error::Conflict("native workspace is not ready".into()));
    }
    if let Some(pending) = &before.pending {
        if pending.kind != OperationKind::Checkpoint {
            return Err(Error::Conflict(
                "native workspace has a non-checkpoint operation pending recovery".into(),
            ));
        }
        return match workspace.checkpoint(engine, &pending.operation_id, before.generation)? {
            OperationOutcome::Checkpointed { revision, .. } => Ok(revision),
            _ => Err(Error::Conflict(
                "native checkpoint recovery did not complete".into(),
            )),
        };
    }
    let status = workspace.status(engine, MAX_WORKSPACE_CHANGES)?;
    if !status.complete {
        return Err(Error::Limit(
            "native workspace scan exceeds the supported change set".into(),
        ));
    }
    if status.changes.is_empty() {
        return Ok(before.base_revision);
    }
    let operation = stable_id(
        "hosted-checkpoint-",
        &[
            change.repository.as_bytes(),
            change.id.as_bytes(),
            &before.generation.to_be_bytes(),
            before.base_revision.as_bytes(),
        ],
    );
    match workspace.checkpoint(engine, &operation, before.generation)? {
        OperationOutcome::Checkpointed { revision, .. } => Ok(revision),
        _ => Err(Error::Conflict("native checkpoint did not complete".into())),
    }
}

fn refresh_target(
    data_dir: &Path,
    engine: &Engine,
    workspace: &mut Workspace,
    source_repository: &str,
    local: &str,
    remote: Option<&str>,
) -> Result<String> {
    let Some(remote) = remote else {
        return Ok(local.to_owned());
    };
    if remote == local {
        return Ok(local.to_owned());
    }
    if revision_is_ancestor(engine, remote, local)? {
        return Ok(local.to_owned());
    }
    let operation = stable_id(
        "hosted-merge-",
        &[
            source_repository.as_bytes(),
            local.as_bytes(),
            remote.as_bytes(),
        ],
    );
    let mut journal = MergeJournal::open(data_dir, source_repository)?;
    let outcome = journal.begin(engine, &operation, local, remote)?;
    let state = match outcome {
        MergeOutcome::Prepared { session, .. } => journal.inspect(&session)?,
        MergeOutcome::Completed {
            session,
            generation,
            ..
        } => {
            let completion = journal.completion(&session, generation)?;
            return apply_merge(engine, workspace, &completion);
        }
        _ => {
            return Err(Error::Conflict(
                "hosted target merge did not prepare".into(),
            ));
        }
    };
    if state.phase == MergePhase::Ready {
        let complete = stable_id("hosted-merge-complete-", &[state.session.as_bytes()]);
        let completed = journal.complete(engine, &state.session, &complete, state.generation)?;
        let MergeOutcome::Completed {
            session,
            generation,
            ..
        } = completed
        else {
            return Err(Error::Conflict(
                "hosted target merge did not complete".into(),
            ));
        };
        let completion = journal.completion(&session, generation)?;
        return apply_merge(engine, workspace, &completion);
    }
    if state.phase == MergePhase::Conflicts || state.phase == MergePhase::AwaitingBase {
        let paths: Vec<String> = journal
            .conflicts(&state.session, None, 100)?
            .entries
            .into_iter()
            .flat_map(|conflict| conflict.paths.into_iter().map(|path| path.path))
            .take(100)
            .collect();
        return Err(Error::Conflict(format!(
            "hosted target refresh needs merge session {} resolution{}",
            state.session,
            if paths.is_empty() {
                String::new()
            } else {
                format!(": {}", paths.join(", "))
            }
        )));
    }
    Err(Error::Conflict(
        "hosted target merge is not ready for workspace application".into(),
    ))
}

fn revision_is_ancestor(engine: &Engine, ancestor: &str, descendant: &str) -> Result<bool> {
    let mut pending = vec![descendant.to_owned()];
    let mut visited = BTreeSet::new();
    while let Some(revision) = pending.pop() {
        if revision == ancestor {
            return Ok(true);
        }
        if !visited.insert(revision.clone()) {
            continue;
        }
        if visited.len() > MAX_ANCESTRY_REVISIONS {
            return Err(Error::Limit(format!(
                "hosted target ancestry exceeds {MAX_ANCESTRY_REVISIONS} revisions"
            )));
        }
        pending.extend(engine.load_revision(&revision)?.parents);
    }
    Ok(false)
}

fn apply_merge(
    engine: &Engine,
    workspace: &mut Workspace,
    completion: &agentscm::merge::MergeCompletion,
) -> Result<String> {
    let before = workspace.state()?;
    let operation = stable_id(
        "hosted-merge-apply-",
        &[
            completion.session().as_bytes(),
            &completion.generation().to_be_bytes(),
            &before.generation.to_be_bytes(),
        ],
    );
    match workspace.apply(engine, &operation, before.generation, completion)? {
        OperationOutcome::Applied { revision, .. } => Ok(revision),
        OperationOutcome::Blocked { paths } => Err(Error::Conflict(format!(
            "workspace changed while applying hosted target refresh: {}",
            paths.join(", ")
        ))),
        _ => Err(Error::Conflict(
            "hosted target refresh did not apply to the workspace".into(),
        )),
    }
}

fn normalized_upload_revision(
    engine: &Engine,
    remote_target: Option<&str>,
    merged: &str,
) -> Result<String> {
    let Some(remote_target) = remote_target else {
        return Ok(merged.to_owned());
    };
    if engine.load_revision(remote_target)?.tree == engine.load_revision(merged)?.tree {
        return Ok(remote_target.to_owned());
    }
    let diff = engine.diff(remote_target, merged, MAX_WORKSPACE_CHANGES)?;
    if diff.truncated {
        return Err(Error::Limit(
            "target-normalized upload exceeds the supported change set".into(),
        ));
    }
    let mut changes = Vec::with_capacity(diff.entries.len());
    for entry in diff.entries {
        changes.push(EntryChange {
            path: entry.path.clone(),
            content: entry
                .after
                .as_ref()
                .map(|_| engine.read_file(merged, &entry.path))
                .transpose()?,
            kind: entry.after_kind.unwrap_or(EntryKind::Regular),
        });
    }
    let revision = engine.checkpoint_entries_with_parents(
        &[remote_target.to_owned(), merged.to_owned()],
        &changes,
    )?;
    if engine.load_revision(&revision)?.tree != engine.load_revision(merged)?.tree {
        return Err(Error::Conflict(
            "target-normalized upload changed the candidate source tree".into(),
        ));
    }
    Ok(revision)
}

async fn synchronize_source(
    data_dir: &Path,
    engine: &Engine,
    remote: &Remote,
    bootstrap: &SourceBootstrap,
    revision: &str,
    journal: &mut ClientJournal,
    scope: &str,
) -> Result<SourceTransfer> {
    let recovering_pending = journal.pending_source_request.is_some();
    let binding = completed_binding(bootstrap)?;
    if journal.pending_source_request.is_none()
        && journal.completed_binding.as_ref() == Some(&binding)
        && let Some(retained) = journal.transfer.as_ref()
        && retained.state == SourceTransferState::Complete
        && retained.input_revision == revision
        && retained.repository_id == remote.repository
        && retained.profile_id == remote.profile
        && retained.native_repository == journal.native_repository
        && retained.source_repository == journal.source_repository
        && retained.source_change == journal.source_change
        && let Some(subject) = retained.subject.as_ref()
        && subject.requirement_digest == bootstrap.requirement_digest
    {
        let current = remote.subject(&subject.change_id).await?;
        if current == *subject {
            return Ok(retained.clone());
        }
    }
    let directory = private_directory(data_dir, &format!("upload-{revision}"))?;
    let offer_path = directory.join("offer.json");
    let offer = if offer_path.exists() {
        read_json_file(&offer_path, MAX_SOURCE_OFFER_BYTES)?
    } else {
        sync::write_offer(
            engine,
            &journal.source_repository,
            &[revision.to_owned()],
            &offer_path,
        )?
    };
    if offer.roots.as_slice() != [revision] {
        return Err(Error::Conflict(
            "retained upload offer has another root".into(),
        ));
    }
    let declaration = declaration(&bootstrap.template_context);
    let declaration_digest = canonical_digest(&declaration)?;
    let request_id = stable_id(
        "hosted-source-",
        &[
            scope.as_bytes(),
            revision.as_bytes(),
            &journal.expected_change_version.to_be_bytes(),
            declaration_digest.as_bytes(),
        ],
    );
    if journal.pending_source_request.is_none()
        && journal
            .transfer
            .as_ref()
            .is_some_and(|retained| retained.request_id != request_id)
    {
        journal.transfer = None;
        journal.completed_binding = None;
    }
    let candidate = SourceTransferRequest {
        version: SOURCE_TRANSFER_VERSION,
        request_id,
        expected_generation: bootstrap.actor_generation,
        source_repository: journal.source_repository.clone(),
        source_change: journal.source_change.clone(),
        source_revision: revision.into(),
        expected_change_version: journal.expected_change_version,
        context: declaration,
        offer: offer.clone(),
    };
    let request = retained_source_request(journal, candidate, revision, &offer)?;
    journal.pending_source_request = Some(request.clone());
    save_journal(data_dir, scope, journal)?;
    let mut transfer = if let Some(retained) = journal.transfer.clone() {
        if retained.request_id == request.request_id {
            remote.transfer(&retained.transfer_id).await?
        } else {
            return Err(Error::Conflict(
                "retained hosted transfer belongs to another request".into(),
            ));
        }
    } else {
        remote.offer(&request).await?
    };
    validate_transfer(remote, journal, revision, &request, &transfer)?;
    journal.transfer = Some(transfer.clone());
    save_journal(data_dir, scope, journal)?;
    match transfer.state {
        SourceTransferState::AwaitingUpload => {
            let request_path = directory.join("request.json");
            write_or_match(&request_path, &serde_json::to_vec(&transfer.request)?)?;
            let bundle_path = directory.join("source.bundle");
            if !bundle_path.exists() {
                sync::write_bundle(engine, &offer_path, &request_path, &bundle_path)?;
            }
            let bytes = read_bounded(&bundle_path, MAX_SOURCE_BUNDLE_BYTES)?;
            transfer = remote.upload(&transfer.transfer_id, bytes).await?;
            validate_transfer(remote, journal, revision, &request, &transfer)?;
            journal.transfer = Some(transfer.clone());
            save_journal(data_dir, scope, journal)?;
        }
        SourceTransferState::Importing => {
            transfer = remote.transfer(&transfer.transfer_id).await?;
            validate_transfer(remote, journal, revision, &request, &transfer)?;
            journal.transfer = Some(transfer.clone());
            save_journal(data_dir, scope, journal)?;
        }
        SourceTransferState::Complete => {}
        SourceTransferState::Rejected => {
            journal.pending_source_request = None;
            journal.transfer = None;
            journal.completed_binding = None;
            save_journal(data_dir, scope, journal)?;
            return Err(Error::Conflict(
                "hosted source transfer was conclusively rejected before authority mutation; refresh bootstrap state and retry in a new invocation"
                    .into(),
            ));
        }
        SourceTransferState::Unknown => {
            return Err(Error::Conflict(
                "hosted source transfer is unknown; inspect the retained server operation before retrying, or recover with a fresh local store, workspace, and Change key after preserving candidate edits"
                    .into(),
            ));
        }
    }
    if transfer.state == SourceTransferState::Complete {
        if let Some(subject) = &transfer.subject {
            journal.expected_change_version = subject.change_version;
        }
        journal.pending_source_request = None;
        journal.completed_binding = (!recovering_pending).then_some(binding);
        save_journal(data_dir, scope, journal)?;
    }
    Ok(transfer)
}

fn validate_transfer(
    remote: &Remote,
    journal: &ClientJournal,
    revision: &str,
    request: &SourceTransferRequest,
    transfer: &SourceTransfer,
) -> Result<()> {
    if transfer.version != SOURCE_TRANSFER_VERSION
        || transfer.request_id != request.request_id
        || transfer.repository_id != remote.repository
        || transfer.profile_id != remote.profile
        || transfer.native_repository != journal.native_repository
        || transfer.source_repository != journal.source_repository
        || transfer.source_change != journal.source_change
        || transfer.input_revision != revision
    {
        return Err(Error::Conflict(
            "hosted source transfer response does not match its request".into(),
        ));
    }
    Ok(())
}

fn retained_source_request(
    journal: &ClientJournal,
    candidate: SourceTransferRequest,
    revision: &str,
    offer: &SourceOffer,
) -> Result<SourceTransferRequest> {
    let Some(retained) = journal.pending_source_request.as_ref() else {
        return Ok(candidate);
    };
    if retained.version != SOURCE_TRANSFER_VERSION
        || retained.source_repository != journal.source_repository
        || retained.source_change != journal.source_change
        || retained.source_revision != revision
        || &retained.offer != offer
    {
        return Err(Error::Conflict(
            "a different hosted source operation is pending recovery".into(),
        ));
    }
    Ok(retained.clone())
}

fn retained_run_request(
    journal: &ClientJournal,
    candidate: StartRequest,
    subject: &RunSubject,
    profile: &str,
) -> Result<StartRequest> {
    let Some(retained) = journal.pending_run_request.as_ref() else {
        return Ok(candidate);
    };
    if retained.profile_id != profile || retained.subject != *subject {
        return Err(Error::Conflict(
            "a different hosted workflow run is pending recovery".into(),
        ));
    }
    Ok(retained.clone())
}

async fn run_workflow(
    data_dir: &Path,
    remote: &Remote,
    bootstrap: &SourceBootstrap,
    subject: &RunSubject,
    journal: &mut ClientJournal,
    scope: &str,
    args: &HostedWorkflowArgs,
) -> Result<WorkspaceRun> {
    let request_id = stable_id(
        "hosted-run-",
        &[
            scope.as_bytes(),
            subject.context_digest.as_bytes(),
            subject.revision.as_bytes(),
            subject.requirement_digest.as_bytes(),
        ],
    );
    if journal.pending_run_request.is_none()
        && journal
            .run
            .as_ref()
            .is_some_and(|retained| retained.subject != *subject)
    {
        journal.run = None;
    }
    let candidate = StartRequest {
        request_id,
        expected_generation: bootstrap.actor_generation,
        profile_id: remote.profile.clone(),
        subject: subject.clone(),
    };
    let request = retained_run_request(journal, candidate, subject, &remote.profile)?;
    journal.pending_run_request = Some(request.clone());
    save_journal(data_dir, scope, journal)?;
    let mut run = if let Some(run) = journal.run.clone() {
        if run.subject != *subject {
            return Err(Error::Conflict(
                "retained run has another exact subject".into(),
            ));
        }
        remote.run(&run.run_id).await?
    } else {
        remote.start(&request).await?
    };
    if run.repository_id != remote.repository
        || run.profile_id != remote.profile
        || run.subject != *subject
    {
        return Err(Error::Conflict(
            "hosted workflow run does not match its exact request".into(),
        ));
    }
    journal.run = Some(run.clone());
    save_journal(data_dir, scope, journal)?;
    for attempt in 0..args.poll_attempts {
        if !matches!(run.state, ExecutionState::Queued | ExecutionState::Running) {
            break;
        }
        if attempt + 1 < args.poll_attempts {
            tokio::time::sleep(Duration::from_millis(args.poll_interval_ms)).await;
        }
        run = remote.run(&run.run_id).await?;
        if run.subject != *subject {
            return Err(Error::Conflict(
                "hosted workflow run changed subject".into(),
            ));
        }
        journal.run = Some(run.clone());
        save_journal(data_dir, scope, journal)?;
    }
    if run_is_conclusive(run.state) {
        journal.pending_run_request = None;
        save_journal(data_dir, scope, journal)?;
    }
    Ok(run)
}

async fn workflow_failures(remote: &Remote, run: &WorkspaceRun) -> Vec<Value> {
    let mut failures =
        vec![json!({"state":run_state(run.state),"detail":bounded(&run.detail,MAX_DETAIL_BYTES)})];
    if let Ok(envelope) = remote.workflow(&run.run_id).await
        && envelope.version == 1
        && envelope.availability == Availability::Available
        && let Some(projection) = envelope.projection
    {
        failures = projection
            .run
            .steps
            .iter()
            .filter(|step| step.state != StepState::Succeeded)
            .map(|step| {
                json!({"step_id":step.id,"required":step.required,"state":step.state,"detail":step.detail.as_deref().map(|detail|bounded(detail,MAX_DETAIL_BYTES)),"review":step.review})
            })
            .collect();
        if failures.is_empty()
            && !matches!(
                projection.run.state,
                RunState::Succeeded | RunState::CompletedWithOmissions
            )
        {
            failures.push(json!({"state":projection.run.state,"detail":envelope.reason}));
        }
    }
    failures
}

fn load_or_create_journal(
    data_dir: &Path,
    args: &HostedWorkflowArgs,
    bootstrap: &SourceBootstrap,
    change: &ChangeId,
) -> Result<ClientJournal> {
    let scope = scope_sha256(args)?;
    let path = journal_path(data_dir, &scope)?;
    if path.exists() {
        return read_json_file(&path, MAX_JOURNAL_BYTES);
    }
    let journal = ClientJournal {
        version: CLIENT_VERSION,
        scope_sha256: scope.clone(),
        repository_id: args.repository.clone(),
        profile_id: args.profile.clone(),
        actor_id: bootstrap.actor_id.clone(),
        native_repository: bootstrap.native_repository.clone(),
        source_repository: change.repository.clone(),
        source_change: change.id.clone(),
        expected_change_version: 0,
        pending_source_request: None,
        transfer: None,
        completed_binding: None,
        pending_run_request: None,
        run: None,
    };
    save_journal(data_dir, &scope, &journal)?;
    Ok(journal)
}

fn validate_retained_journal_binding(
    data_dir: &Path,
    args: &HostedWorkflowArgs,
    bootstrap: &SourceBootstrap,
    scope: &str,
) -> Result<()> {
    // A fresh source store has no retained identity to validate. Do not create
    // journal directories here: Engine::open initializes the parent afterward.
    if !data_dir.try_exists()? {
        return Ok(());
    }
    let path = journal_path(data_dir, scope)?;
    if !path.exists() {
        return Ok(());
    }
    let journal: ClientJournal = read_json_file(&path, MAX_JOURNAL_BYTES)?;
    validate_journal_owner(
        args,
        &bootstrap.actor_id,
        &bootstrap.native_repository,
        scope,
        &journal,
    )
}

fn validate_journal_owner(
    args: &HostedWorkflowArgs,
    actor_id: &str,
    native_repository: &str,
    scope: &str,
    journal: &ClientJournal,
) -> Result<()> {
    if journal.version != CLIENT_VERSION
        || journal.scope_sha256 != scope
        || journal.repository_id != args.repository
        || journal.profile_id != args.profile
        || journal.actor_id != actor_id
        || journal.native_repository != native_repository
    {
        return Err(Error::Conflict(
            "hosted workflow journal is bound to another authenticated actor, workspace, or profile; inspect the retained server operation, then recover with a fresh local store, workspace, and Change key"
                .into(),
        ));
    }
    Ok(())
}

fn validate_journal(
    args: &HostedWorkflowArgs,
    bootstrap: &SourceBootstrap,
    scope: &str,
    journal: &ClientJournal,
    change: &ChangeId,
) -> Result<()> {
    if journal.version != CLIENT_VERSION
        || journal.scope_sha256 != scope
        || journal.repository_id != args.repository
        || journal.profile_id != args.profile
        || journal.actor_id != bootstrap.actor_id
        || journal.native_repository != bootstrap.native_repository
        || journal.source_repository != change.repository
        || journal.source_change != change.id
    {
        return Err(Error::Conflict(
            "hosted workflow journal belongs to another authenticated actor, workspace, or profile"
                .into(),
        ));
    }
    Ok(())
}

fn declaration(context: &agentscm::context::ReviewContext) -> ContextDeclaration {
    ContextDeclaration {
        intent: context.intent.clone(),
        acceptance_criteria: context.acceptance_criteria.clone(),
        scope: context.scope.clone(),
        exclusions: context.exclusions.clone(),
        omissions: context.omissions.clone(),
    }
}

fn completed_binding(bootstrap: &SourceBootstrap) -> Result<CompletedBinding> {
    Ok(CompletedBinding {
        template_context_digest: bootstrap.template_context.digest()?,
        requirement_digest: bootstrap.requirement_digest.clone(),
        target: bootstrap.target.clone(),
    })
}

fn transfer_state(state: &SourceTransferState) -> &'static str {
    match state {
        SourceTransferState::AwaitingUpload => "awaiting_upload",
        SourceTransferState::Importing => "importing",
        SourceTransferState::Complete => "complete",
        SourceTransferState::Rejected => "rejected",
        SourceTransferState::Unknown => "unknown",
    }
}

fn run_state(state: ExecutionState) -> &'static str {
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

fn run_is_conclusive(state: ExecutionState) -> bool {
    matches!(
        state,
        ExecutionState::Succeeded
            | ExecutionState::Failed
            | ExecutionState::Cancelled
            | ExecutionState::Stale
    )
}

fn blocked(failures: Vec<Value>, next_action: &str) -> Value {
    json!({"receipt":Value::Null,"workspace_changed_during_run":false,"failures":failures,"next_action":next_action})
}

fn vetted_url(input: &str) -> Result<Url> {
    let normalized = agentscm::hub::forge_http::check_base_url(input)
        .map_err(|_| Error::Invalid("invalid hosted workflow endpoint".into()))?;
    let parsed = Url::parse(&normalized)
        .map_err(|_| Error::Invalid("invalid hosted workflow endpoint".into()))?;
    if parsed.path() != "/" {
        return Err(Error::Invalid(
            "hosted workflow endpoint must not contain a path".into(),
        ));
    }
    Ok(parsed)
}

/// Resolve the token before any network call or source scan and keep it outside
/// both trees whose bytes can be imported, checkpointed or uploaded.
// Returns validated path metadata only; this function never reads credential bytes.
fn checked_external_file_path(data_dir: &Path, args: &HostedWorkflowArgs) -> Result<PathBuf> {
    if !args.token_file.is_absolute() {
        return Err(Error::Invalid(
            "hosted workflow token file must be absolute".into(),
        ));
    }
    let token = args.token_file.canonicalize()?;
    let metadata = fs::symlink_metadata(&token)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "hosted workflow token file must be a private regular file".into(),
        ));
    }
    let data = prospective_path(data_dir, "laptop source store")?;
    let workspace = prospective_path(&args.workspace, "native workspace")?;
    if token.starts_with(&data) || token.starts_with(&workspace) {
        return Err(Error::Invalid(
            "hosted workflow token file must be outside the source store and native workspace"
                .into(),
        ));
    }
    Ok(token)
}

fn prospective_path(path: &Path, kind: &str) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    Ok(parent.join(
        path.file_name()
            .ok_or_else(|| Error::Invalid(format!("{kind} needs a directory name")))?,
    ))
}

fn read_token(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "hosted workflow token file must be absolute".into(),
        ));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "hosted workflow token file must be a private regular file".into(),
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TOKEN_BYTES {
        return Err(Error::Limit(
            "hosted workflow token file is too large".into(),
        ));
    }
    let token = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Invalid("hosted workflow token is not UTF-8".into()))?
        .trim()
        .to_owned();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err(Error::Invalid("hosted workflow token is invalid".into()));
    }
    Ok(token)
}

fn validate_repository(value: &str) -> Result<()> {
    let Some((owner, name)) = value.split_once('/') else {
        return Err(Error::Invalid(
            "hosted repository must be owner/name".into(),
        ));
    };
    if name.contains('/') {
        return Err(Error::Invalid(
            "hosted repository must be owner/name".into(),
        ));
    }
    validate_segment("repository owner", owner)?;
    validate_segment("repository name", name)
}

fn validate_segment(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(Error::Invalid(format!("invalid hosted {kind}")));
    }
    Ok(())
}

fn validate_hosted_id(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Invalid(format!("invalid hosted {kind}")));
    }
    Ok(())
}

fn scope_sha256(args: &HostedWorkflowArgs) -> Result<String> {
    let endpoint = vetted_url(&args.endpoint)?;
    let workspace = prospective_path(&args.workspace, "native workspace")?;
    Ok(stable_hash(&[
        endpoint.as_str().as_bytes(),
        args.repository.as_bytes(),
        args.profile.as_bytes(),
        args.change.as_bytes(),
        workspace.as_os_str().as_encoded_bytes(),
    ]))
}

fn stable_id(prefix: &str, fields: &[&[u8]]) -> String {
    stable_digest(
        b"agentscm.hosted-workflow-client.operation.v1\0",
        Some(prefix.as_bytes()),
        fields,
    )
}

fn stable_hash(fields: &[&[u8]]) -> String {
    stable_digest(b"agentscm.hosted-workflow-client.v1\0", None, fields)
}

fn stable_digest(domain: &[u8], prefix: Option<&[u8]>, fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    if let Some(prefix) = prefix {
        hasher.update((prefix.len() as u64).to_be_bytes());
        hasher.update(prefix);
    }
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    format!("{:x}", hasher.finalize())
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn require_separate_workspace(data_dir: &Path, workspace: &Path) -> Result<()> {
    let data = prospective_path(data_dir, "laptop source store")?;
    let workspace = prospective_path(workspace, "native workspace")?;
    if data.starts_with(&workspace) || workspace.starts_with(&data) {
        return Err(Error::Invalid(
            "laptop source store and native workspace must be separate, non-nested directories"
                .into(),
        ));
    }
    Ok(())
}

fn state_root(data_dir: &Path) -> Result<PathBuf> {
    let root = data_dir.join("hosted-workflow");
    if !root.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&root)?;
        File::open(data_dir)?.sync_all()?;
    }
    let metadata = fs::symlink_metadata(&root)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "hosted workflow state directory is not private".into(),
        ));
    }
    Ok(root)
}

fn private_directory(data_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_segment("state directory", name)?;
    let parent = state_root(data_dir)?;
    let path = parent.join(name);
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&path)?;
        File::open(&parent)?.sync_all()?;
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "invalid hosted workflow state directory".into(),
        ));
    }
    Ok(path)
}

fn journal_path(data_dir: &Path, scope: &str) -> Result<PathBuf> {
    Ok(state_root(data_dir)?.join(format!("{scope}.json")))
}

fn save_journal(data_dir: &Path, scope: &str, journal: &ClientJournal) -> Result<()> {
    let path = journal_path(data_dir, scope)?;
    let bytes = serde_json::to_vec(journal)?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(Error::Limit("hosted workflow journal is too large".into()));
    }
    let parent = path.parent().expect("journal parent");
    let (temporary, mut file) = (0..16)
        .find_map(|sequence| {
            let temporary =
                parent.join(format!(".{scope}.{}.{}.tmp", std::process::id(), sequence));
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            match options.open(&temporary) {
                Ok(file) => Some(Ok((temporary, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| Error::Conflict("hosted workflow journal writer is busy".into()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_json_file<T: DeserializeOwned>(path: &Path, maximum: usize) -> Result<T> {
    serde_json::from_slice(&read_bounded(path, maximum)?)
        .map_err(|_| Error::Invalid("invalid hosted workflow retained JSON".into()))
}

fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum as u64 {
        return Err(Error::Invalid(
            "invalid hosted workflow retained file".into(),
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(Error::Limit(
            "hosted workflow retained file is too large".into(),
        ));
    }
    Ok(bytes)
}

fn write_or_match(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        if read_bounded(path, bytes.len().saturating_add(1))? == bytes {
            return Ok(());
        }
        return Err(Error::Conflict(
            "hosted workflow retained file has different bytes".into(),
        ));
    }
    write_new(path, bytes)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return if read_bounded(path, bytes.len().saturating_add(1))? == bytes {
            Ok(())
        } else {
            Err(Error::Conflict(
                "hosted workflow retained file has different bytes".into(),
            ))
        };
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("retained file has no parent".into()))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Invalid("retained file has no name".into()))?
        .to_string_lossy();
    let (temporary, mut file) = (0..16)
        .find_map(|sequence| {
            let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            match options.open(&temporary) {
                Ok(file) => Some(Ok((temporary, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| Error::Conflict("retained file writer is busy".into()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let published = match fs::hard_link(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_bounded(path, bytes.len().saturating_add(1))? == bytes {
                Ok(())
            } else {
                Err(Error::Conflict(
                    "hosted workflow retained file has different bytes".into(),
                ))
            }
        }
        Err(error) => Err(error.into()),
    };
    let _ = fs::remove_file(&temporary);
    published?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_hook_payload() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Limit("hosted Stop hook input exceeds 64 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.into();
    }
    let mut end = maximum.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: &Path) -> HostedWorkflowArgs {
        HostedWorkflowArgs {
            endpoint: "https://hub.example".into(),
            repository: "counterbranch/example".into(),
            profile: "checks".into(),
            token_file: root.join("token"),
            workspace: root.join("workspace"),
            change: "laptop-change".into(),
            poll_attempts: 2,
            poll_interval_ms: 10,
        }
    }

    #[test]
    fn scope_binds_endpoint_profile_change_and_workspace_without_the_token() {
        let root = tempfile::tempdir().unwrap();
        let first = args(root.path());
        let digest = scope_sha256(&first).unwrap();
        let mut changed = args(root.path());
        changed.profile = "other".into();
        assert_ne!(scope_sha256(&changed).unwrap(), digest);
        changed = args(root.path());
        changed.token_file = root.path().join("another-token");
        assert_eq!(scope_sha256(&changed).unwrap(), digest);
    }

    #[test]
    fn endpoint_and_repository_validation_refuse_credential_and_path_tricks() {
        for endpoint in [
            "http://example.com",
            "https://user:secret@hub.example",
            "https://hub.example/path",
            "https://hub.example?secret=x",
        ] {
            assert!(vetted_url(endpoint).is_err(), "{endpoint}");
        }
        for repository in ["missing", "a/b/c", "a/%2f"] {
            assert!(validate_repository(repository).is_err(), "{repository}");
        }
    }

    #[test]
    fn operation_and_profile_ids_meet_hosted_identifier_contract() {
        let fields: &[&[u8]] = &[b"repository", b"profile", b"revision"];
        let source = stable_id("hosted-source-", fields);
        let run = stable_id("hosted-run-", fields);

        for id in [&source, &run] {
            assert_eq!(id.len(), 64);
            assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(validate_hosted_id("request id", id).is_ok());
        }
        assert_ne!(source, run, "operation prefix must separate ID domains");

        assert!(validate_hosted_id("profile", &"a".repeat(64)).is_ok());
        assert!(validate_hosted_id("profile", &"a".repeat(65)).is_err());
        assert!(validate_hosted_id("profile", "profile.with-dot").is_err());
    }

    #[test]
    fn token_reader_requires_absolute_private_regular_utf8_file() {
        let root = tempfile::tempdir().unwrap();
        let token = root.path().join("token");
        fs::write(&token, "secret\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token(&token).unwrap(), "secret");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&token).is_err());
        assert!(read_token(Path::new("relative-token")).is_err());
    }

    #[test]
    fn token_location_rejects_workspace_aliases_before_source_capture() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let workspace = root.path().join("real-workspace");
        let alias = root.path().join("workspace-alias");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&workspace).unwrap();
        symlink(&workspace, &alias).unwrap();
        let token = workspace.join("agent.token");
        fs::write(&token, "secret\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();

        let mut nested = args(root.path());
        nested.workspace = alias.clone();
        nested.token_file = token;
        assert!(checked_external_file_path(&data, &nested).is_err());

        nested.token_file = alias.join("agent.token");
        assert!(checked_external_file_path(&data, &nested).is_err());
    }

    #[test]
    fn journal_write_is_private_and_round_trips_strictly() {
        let root = tempfile::tempdir().unwrap();
        let scope = "a".repeat(64);
        let journal = ClientJournal {
            version: 1,
            scope_sha256: scope.clone(),
            repository_id: "counterbranch/example".into(),
            profile_id: "checks".into(),
            actor_id: "actor-a".into(),
            native_repository: "b".repeat(32),
            source_repository: "b".repeat(32),
            source_change: "change".into(),
            expected_change_version: 0,
            pending_source_request: None,
            transfer: None,
            completed_binding: None,
            pending_run_request: None,
            run: None,
        };
        save_journal(root.path(), &scope, &journal).unwrap();
        let path = journal_path(root.path(), &scope).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            read_json_file::<ClientJournal>(&path, MAX_JOURNAL_BYTES).unwrap(),
            journal
        );
    }

    #[test]
    fn retained_journal_rejects_a_second_authenticated_actor() {
        let root = tempfile::tempdir().unwrap();
        let args = args(root.path());
        let scope = scope_sha256(&args).unwrap();
        let journal = ClientJournal {
            version: CLIENT_VERSION,
            scope_sha256: scope.clone(),
            repository_id: args.repository.clone(),
            profile_id: args.profile.clone(),
            actor_id: "actor-a".into(),
            native_repository: "native-repository".into(),
            source_repository: "source-repository".into(),
            source_change: args.change.clone(),
            expected_change_version: 0,
            pending_source_request: None,
            transfer: None,
            completed_binding: None,
            pending_run_request: None,
            run: None,
        };

        validate_journal_owner(&args, "actor-a", "native-repository", &scope, &journal).unwrap();
        let error = validate_journal_owner(&args, "actor-b", "native-repository", &scope, &journal)
            .unwrap_err();
        assert!(error.to_string().contains("authenticated actor"));
    }

    #[test]
    fn unresolved_requests_keep_exact_generation_cas_and_context() {
        let offer = SourceOffer {
            version: 1,
            profile: "agentscm-source-v1".into(),
            required_capabilities: vec![],
            origin_repository: "source".into(),
            roots: vec!["a".repeat(64)],
            objects: vec![],
        };
        let subject = RunSubject {
            change_id: "hosted-change".into(),
            change_version: 1,
            context_digest: "b".repeat(64),
            revision: "c".repeat(64),
            requirement_digest: "d".repeat(64),
        };
        let original_source = SourceTransferRequest {
            version: SOURCE_TRANSFER_VERSION,
            request_id: "source-request".into(),
            expected_generation: 4,
            source_repository: "source".into(),
            source_change: "change".into(),
            source_revision: "a".repeat(64),
            expected_change_version: 0,
            context: ContextDeclaration {
                intent: "original".into(),
                acceptance_criteria: vec!["passes".into()],
                scope: vec!["src".into()],
                exclusions: vec![],
                omissions: vec![],
            },
            offer: offer.clone(),
        };
        let original_run = StartRequest {
            request_id: "run-request".into(),
            expected_generation: 4,
            profile_id: "checks".into(),
            subject: subject.clone(),
        };
        let mut journal = ClientJournal {
            version: 1,
            scope_sha256: "e".repeat(64),
            repository_id: "counterbranch/example".into(),
            profile_id: "checks".into(),
            actor_id: "actor-a".into(),
            native_repository: "f".repeat(32),
            source_repository: "source".into(),
            source_change: "change".into(),
            expected_change_version: 0,
            pending_source_request: Some(original_source.clone()),
            transfer: None,
            completed_binding: None,
            pending_run_request: Some(original_run.clone()),
            run: None,
        };

        let mut changed_source = original_source.clone();
        changed_source.expected_generation = 9;
        changed_source.expected_change_version = 7;
        changed_source.context.intent = "changed later".into();
        changed_source.request_id = "changed-request".into();
        assert_eq!(
            retained_source_request(
                &journal,
                changed_source,
                &original_source.source_revision,
                &offer,
            )
            .unwrap(),
            original_source
        );

        let mut changed_run = original_run.clone();
        changed_run.expected_generation = 9;
        changed_run.request_id = "changed-run".into();
        assert_eq!(
            retained_run_request(&journal, changed_run, &subject, "checks").unwrap(),
            original_run
        );

        journal.pending_source_request = None;
        journal.pending_run_request = None;
        assert!(
            retained_source_request(
                &journal,
                original_source.clone(),
                &original_source.source_revision,
                &offer,
            )
            .is_ok()
        );
    }

    #[test]
    fn pending_state_branches_keep_indeterminate_operations_retained() {
        assert!(!run_is_conclusive(ExecutionState::Queued));
        assert!(!run_is_conclusive(ExecutionState::Running));
        assert!(!run_is_conclusive(ExecutionState::Unknown));
        assert!(run_is_conclusive(ExecutionState::Succeeded));
        assert!(run_is_conclusive(ExecutionState::Failed));
        assert!(run_is_conclusive(ExecutionState::Cancelled));
        assert!(run_is_conclusive(ExecutionState::Stale));

        assert_eq!(
            transfer_state(&SourceTransferState::AwaitingUpload),
            "awaiting_upload"
        );
        assert_eq!(transfer_state(&SourceTransferState::Importing), "importing");
        assert_eq!(transfer_state(&SourceTransferState::Complete), "complete");
        assert_eq!(transfer_state(&SourceTransferState::Rejected), "rejected");
        assert_eq!(transfer_state(&SourceTransferState::Unknown), "unknown");
    }

    #[test]
    fn bounded_text_preserves_utf8_boundary() {
        assert_eq!(bounded("abcdef", 5), "ab...");
        assert_eq!(bounded("éééé", 6), "é...");
    }

    #[test]
    fn incorporated_target_is_found_through_nonempty_and_merge_parents() {
        use agentscm::model::FileChange;

        let root = tempfile::tempdir().unwrap();
        let engine = Engine::open(root.path()).unwrap();
        let base = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "src/lib.rs".into(),
                    content: Some(b"base\n".to_vec()),
                }],
            )
            .unwrap();
        let edited = engine
            .checkpoint(
                Some(&base),
                &[FileChange {
                    path: "src/lib.rs".into(),
                    content: Some(b"edited\n".to_vec()),
                }],
            )
            .unwrap();
        let other = engine
            .checkpoint(
                None,
                &[FileChange {
                    path: "other.txt".into(),
                    content: Some(b"other\n".to_vec()),
                }],
            )
            .unwrap();
        let merged = engine
            .checkpoint_with_parents(&[edited.clone(), other.clone()], &[])
            .unwrap();

        assert!(revision_is_ancestor(&engine, &base, &edited).unwrap());
        assert!(revision_is_ancestor(&engine, &base, &merged).unwrap());
        assert!(revision_is_ancestor(&engine, &other, &merged).unwrap());
        assert!(!revision_is_ancestor(&engine, &other, &edited).unwrap());
    }
}
