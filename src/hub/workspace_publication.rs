//! Authenticated preparation of an operator-pinned attached Git candidate.
//!
//! Operator-pinned preparation, branch transport and bound forge publication.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use http_body::Frame;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::http::{ApiError, JsonBody, blocking};
use super::workspace::WorkspaceActor;
use super::workspace_agent_tasks::Clock;
use super::workspace_execution::{AttachedPublicationProfile, ExecutionService, RunSubject};
use super::{HubError, HubState, Result};
use crate::git_export::remote::{
    Access, Record as RemoteRecord, Request as RemoteRequest, State as RemoteState,
};
use crate::git_export::{
    CandidateExportRecord, CandidateExportRequest, CommitMetadata, ExportState, Mirror,
};
use crate::skill_workflow::{AgentTaskService, CollectorAction};

fn publication<T>(result: crate::Result<T>) -> Result<T> {
    result.map_err(|error| match error {
        crate::Error::Conflict(_) => HubError::Invalid("workspace_native_conflict".into()),
        crate::Error::Invalid(_) | crate::Error::NotFound(_) | crate::Error::Limit(_) => {
            HubError::Invalid("invalid attached publication request".into())
        }
        other => HubError::from(other),
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub request_id: String,
    pub expected_generation: u64,
    pub subject: RunSubject,
    pub metadata: CommitMetadata,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublishBranchRequest {
    pub request_id: String,
    pub prepare_request_id: String,
    pub candidate_id: String,
    pub expected_generation: u64,
    #[serde(default)]
    pub previous_branch_request_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverBranchRequest {
    pub request_id: String,
    pub prepare_request_id: String,
    pub candidate_id: String,
}

#[derive(Serialize)]
pub struct BranchStatus {
    pub request_id: String,
    pub candidate_id: String,
    pub state: RemoteState,
    pub commit: String,
    pub observed_oid: Option<String>,
    pub currentness: &'static str,
}

#[derive(Serialize)]
pub struct PublicationStatus {
    pub request_id: String,
    pub candidate: PublicationCandidate,
    pub currentness: &'static str,
}

#[derive(Serialize)]
pub struct PublicationCandidate {
    pub candidate_id: String,
    pub state: ExportState,
    pub commit: Option<String>,
    pub revision: String,
    pub change_id: String,
}

impl From<CandidateExportRecord> for PublicationCandidate {
    fn from(record: CandidateExportRecord) -> Self {
        Self {
            candidate_id: record.request.subject.id,
            state: record.state,
            commit: record.commit,
            revision: record.request.subject.revision,
            change_id: record.request.subject.change.id,
        }
    }
}

/// Several Git transport commands, each bounded at 60 seconds, can outlast the Hub's
/// 30-second request cap on a loaded host, so processing on these routes gets longer.
pub(super) const PROCESSING_TIMEOUT: Duration = Duration::from_secs(120);

/// Reading a retained outcome runs no Git transport and keeps the Hub-wide bound.
pub fn status_routes() -> Router<HubState> {
    Router::new().route(
        "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/status",
        post(status),
    )
}

/// Routes that run Git transport or forge calls; mount them behind [`processing_timeout`].
pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication",
            post(prepare),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/branch",
            post(publish_branch),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/branch/recover",
            post(recover_branch),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/forge",
            post(forge_publish),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/forge/recover",
            post(forge_recover),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/mcp",
            post(mcp),
        )
}

/// Bounds these routes like the Hub-wide request timeout, except that processing may take
/// `processing`. The body must still arrive within the Hub's request bound; it is read by
/// the handler's extractors after authentication, with their usual rejections.
pub(super) async fn processing_timeout(
    State(state): State<HubState>,
    request: Request,
    next: Next,
) -> Response {
    bounded(
        super::REQUEST_TIMEOUT,
        state.publication_processing_timeout,
        request,
        next,
    )
    .await
}

async fn bounded(read: Duration, processing: Duration, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let body = Arc::new(BodyRead::default());
    let deadline = Box::pin(tokio::time::sleep(read));
    let request = request.map(|inner| {
        Body::new(TimedBody {
            inner,
            deadline,
            read: body.clone(),
        })
    });
    match tokio::time::timeout(processing, next.run(request)).await {
        Ok(_) if body.expired.load(Ordering::Acquire) => {
            StatusCode::REQUEST_TIMEOUT.into_response()
        }
        Ok(response) => response,
        Err(_) => still_running(&path, &body.bytes.lock().unwrap_or_else(|e| e.into_inner())),
    }
}

/// What the handler read of the body: whether its deadline passed, and a bounded copy of
/// the bytes so a timed-out MCP call can still be answered with its JSON-RPC id.
#[derive(Default)]
struct BodyRead {
    expired: AtomicBool,
    bytes: Mutex<Vec<u8>>,
}

struct TimedBody {
    inner: Body,
    deadline: Pin<Box<tokio::time::Sleep>>,
    read: Arc<BodyRead>,
}

impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, axum::Error>>> {
        if let Poll::Ready(frame) = Pin::new(&mut self.inner).poll_frame(cx) {
            if let Some(data) = frame
                .as_ref()
                .and_then(|frame| frame.as_ref().ok())
                .and_then(Frame::data_ref)
            {
                let mut bytes = self.read.bytes.lock().unwrap_or_else(|e| e.into_inner());
                let room = super::MAX_BODY_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&data[..room.min(data.len())]);
            }
            return Poll::Ready(frame);
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.read.expired.store(true, Ordering::Release);
            return Poll::Ready(Some(Err(axum::Error::new("request body timed out"))));
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Runs a handler's whole body as its own task, so a request dropped at the processing
/// bound does not cancel work that `still_running` reports as continuing.
async fn detached<T: Send + 'static>(
    work: impl Future<Output = std::result::Result<T, ApiError>> + Send + 'static,
) -> std::result::Result<T, ApiError> {
    tokio::spawn(work)
        .await
        .map_err(|_| ApiError::from(HubError::Unavailable("publication task failed".into())))?
}

/// The blocking work keeps running past the bound, so instead of a failure that may
/// later succeed, name the idempotent call that reads its retained outcome.
fn still_running(path: &str, body: &[u8]) -> Response {
    if path.ends_with("/publication/mcp") {
        let message = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
        let tool = match message["params"]["name"].as_str() {
            Some("agentscm_prepare_attached_publication") => "agentscm_attached_publication_status",
            Some("agentscm_publish_attached_branch") => "agentscm_recover_attached_branch",
            Some(tool) => tool,
            None => "the same tool",
        };
        return mcp_error(
            &message["id"],
            -32000,
            &format!("publication is still running; call {tool} with the same request ids"),
        );
    }
    let follow_up = match path
        .rsplit_once("/publication")
        .map_or("", |(_, suffix)| suffix)
    {
        "" => "status",
        "/branch" => "branch/recover",
        suffix => suffix.trim_start_matches('/'),
    };
    (
        StatusCode::ACCEPTED,
        Json(json!({"state":"still_running","follow_up":follow_up})),
    )
        .into_response()
}

fn access(profile: &AttachedPublicationProfile) -> Result<Access> {
    Ok(Access {
        credential_file: profile.credential_file.clone(),
        credential_helper: profile
            .credential_file
            .as_ref()
            .map(|_| std::env::current_exe())
            .transpose()?,
        ca_file: profile.ca_file.clone(),
        ssh: None,
    })
}

fn branch_status(
    request_id: String,
    candidate_id: String,
    record: RemoteRecord,
    currentness: &'static str,
) -> BranchStatus {
    BranchStatus {
        request_id,
        candidate_id,
        state: record.state,
        commit: record.commit,
        observed_oid: record.observed_oid,
        currentness,
    }
}

fn operation_id(owner: &str, attachment: &str, request: &str) -> Result<String> {
    if request.is_empty()
        || request.len() > 48
        || !request
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(HubError::Invalid("invalid publication request id".into()));
    }
    let digest = crate::store::digest(
        &serde_json::to_vec(&(owner, attachment, request)).map_err(crate::Error::from)?,
    );
    Ok(format!("wsp-{}", &digest[..60]))
}

fn prepare_input(
    config_digest: &str,
    actor: &crate::skill_workflow::AuthenticatedAgentIdentity,
    repository: &str,
    profile: &str,
    attachment: &str,
    request: &PrepareRequest,
) -> serde_json::Value {
    json!({
        "config_digest": config_digest,
        "owner_id": actor.owner_id,
        "actor": actor,
        "repository_id": repository,
        "profile_id": profile,
        "attachment_id": attachment,
        "request_id": request.request_id,
        "expected_generation": request.expected_generation,
        "subject": request.subject,
        "metadata": request.metadata,
    })
}

fn resume_candidate(
    current: &crate::candidate::CandidateSubject,
    metadata: &CommitMetadata,
    retained: CandidateExportRequest,
) -> Result<CandidateExportRequest> {
    let mut expected = current.clone();
    expected.id = retained.subject.id.clone();
    expected.parent_commit = retained.subject.parent_commit.clone();
    if retained.subject != expected || retained.metadata != *metadata {
        return Err(HubError::Invalid(
            "retained publication prepare differs from current qualification".into(),
        ));
    }
    Ok(retained)
}

impl ExecutionService {
    fn publication_profile(
        &self,
        repository: &str,
        profile: &str,
        attachment: &str,
    ) -> Result<(
        AttachedPublicationProfile,
        super::workspace_execution::AgentTaskProfile,
    )> {
        let publication = self
            .attached_publication_profiles
            .get(&(repository.into(), profile.into(), attachment.into()))
            .cloned()
            .ok_or(HubError::Unauthorized)?;
        let task = self.agent_task_profile(repository, profile)?;
        Ok((publication, task))
    }

    pub fn prepare_attached_publication(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        attachment: &str,
        request: PrepareRequest,
    ) -> Result<PublicationStatus> {
        let (profile, task) = self.publication_profile(repository, profile_id, attachment)?;
        if actor.user_id() != profile.owner_id {
            return Err(HubError::Unauthorized);
        }
        let context = task.action_subject(&request.subject)?;
        let engine = publication(crate::store::Engine::open(&task.data_dir))?;
        // Git/source comparison can run subprocesses, so it precedes the workspace mutex.
        let compared = publication(crate::candidate::compare_attached_candidate(
            &task.data_dir,
            &engine,
            &task.native_repository,
            attachment,
            request.expected_generation,
        ))?;
        let identity = actor.authorization_identity();
        let (_, guard) = self.task_parts(actor, repository, &task)?;
        let clock = Clock;
        let service = publication(AgentTaskService::new_with_collector_authorization(
            &task.data_dir,
            &task.profile_id,
            &clock,
            &guard,
        ))?;
        let operation = operation_id(actor.user_id(), attachment, &request.request_id)?;
        let config_digest =
            crate::store::digest(&serde_json::to_vec(&profile).map_err(crate::Error::from)?);
        let input = prepare_input(
            &config_digest,
            &identity,
            repository,
            profile_id,
            attachment,
            &request,
        );
        let mut mirror = publication(Mirror::open(
            &profile.git,
            &profile.mirror,
            &task.native_repository,
        ))?;
        let mut subject = publication(service.with_live_imported_origins(
            &actor.authorization_identity(),
            &context,
            CollectorAction::PublishCandidate,
            |origins| {
                crate::candidate::qualify_attached_candidate_from_comparison(
                    &task.data_dir,
                    &task.native_repository,
                    &compared,
                    Some(origins),
                )
            },
        ))?;
        if subject.change.id != request.subject.change_id
            || subject.change_version != request.subject.change_version
            || subject.context_digest != request.subject.context_digest
            || subject.revision != request.subject.revision
            || request.subject.requirement_digest != task.requirement_digest
            || subject.endpoint != profile.endpoint
            || subject.target != profile.target
            || subject.output != profile.output
        {
            return Err(HubError::Invalid(
                "publication subject or configured identity is stale".into(),
            ));
        }
        let candidate =
            if let Some(retained) = publication(mirror.candidate_operation(&operation, &input))? {
                resume_candidate(&subject, &request.metadata, retained)?
            } else {
                subject.parent_commit = publication(mirror.owned_oid())?
                    .ok_or_else(|| HubError::Invalid("owned mirror has no parent".into()))?;
                subject.id = publication(subject.canonical_id())?;
                let candidate = CandidateExportRequest {
                    expected_oid: subject.parent_commit.clone(),
                    subject,
                    metadata: request.metadata,
                };
                publication(mirror.reserve_candidate_operation(&operation, &input, &candidate))?;
                candidate
            };
        publication(mirror.import_candidate_baseline(
            std::path::Path::new(&compared_state_worktree(
                &task.data_dir,
                &task.native_repository,
                attachment,
            )?),
            &candidate.subject.baseline_commit,
        ))?;
        let record = publication(mirror.export_candidate_guarded(&engine, &candidate, || {
            service.with_live_imported_origins(
                &actor.authorization_identity(),
                &context,
                CollectorAction::PublishCandidate,
                |origins| {
                    let mut current = crate::candidate::qualify_attached_candidate_from_comparison(
                        &task.data_dir,
                        &task.native_repository,
                        &compared,
                        Some(origins),
                    )?;
                    current.id = candidate.subject.id.clone();
                    current.parent_commit = candidate.subject.parent_commit.clone();
                    if current == candidate.subject {
                        Ok(())
                    } else {
                        Err(crate::Error::Conflict("prepared subject is stale".into()))
                    }
                },
            )
        }))?;
        Ok(PublicationStatus {
            request_id: request.request_id,
            candidate: record.into(),
            currentness: "current_at_prepare",
        })
    }

    pub fn attached_publication_status(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        attachment: &str,
        request: StatusRequest,
    ) -> Result<PublicationStatus> {
        let (profile, task) = self.publication_profile(repository, profile_id, attachment)?;
        if actor.user_id() != profile.owner_id {
            return Err(HubError::Unauthorized);
        }
        self.workspace.authorize(
            actor,
            repository,
            super::workspace::WorkspaceAction::ReadRepository,
        )?;
        let operation = operation_id(actor.user_id(), attachment, &request.request_id)?;
        let mirror = Mirror::open(&profile.git, &profile.mirror, &task.native_repository)?;
        let retained = mirror
            .retained_candidate_operation(&operation)?
            .ok_or_else(|| HubError::Invalid("publication prepare was not found".into()))?;
        let record = mirror
            .candidate_record(&retained.subject.id)?
            .ok_or_else(|| HubError::Invalid("publication prepare is incomplete".into()))?;
        Ok(PublicationStatus {
            request_id: request.request_id,
            candidate: record.into(),
            currentness: "not_rechecked",
        })
    }

    fn current_candidate(
        &self,
        actor: &WorkspaceActor,
        task: &super::workspace_execution::AgentTaskProfile,
        profile: &AttachedPublicationProfile,
        attachment: &str,
        expected_generation: u64,
        expected: &crate::candidate::CandidateSubject,
    ) -> Result<()> {
        let context = task.action_subject(&RunSubject {
            change_id: expected.change.id.clone(),
            change_version: expected.change_version,
            context_digest: expected.context_digest.clone(),
            revision: expected.revision.clone(),
            requirement_digest: task.requirement_digest.clone(),
        })?;
        // Source comparison may run Git and is deliberately outside the workspace mutex.
        let compared = publication(crate::candidate::compare_attached_candidate(
            &task.data_dir,
            &crate::store::Engine::open(&task.data_dir)?,
            &task.native_repository,
            attachment,
            expected_generation,
        ))?;
        let (_, guard) = self.task_parts(actor, &profile.repository_id, task)?;
        let clock = Clock;
        let service = publication(AgentTaskService::new_with_collector_authorization(
            &task.data_dir,
            &task.profile_id,
            &clock,
            &guard,
        ))?;
        let mut current = publication(service.with_live_imported_origins(
            &actor.authorization_identity(),
            &context,
            CollectorAction::PublishCandidate,
            |origins| {
                crate::candidate::qualify_attached_candidate_from_comparison(
                    &task.data_dir,
                    &task.native_repository,
                    &compared,
                    Some(origins),
                )
            },
        ))?;
        current.id = expected.id.clone();
        current.parent_commit = expected.parent_commit.clone();
        if &current != expected {
            return Err(HubError::Invalid(
                "publication subject is no longer current".into(),
            ));
        }
        if current.endpoint != profile.endpoint
            || current.target != profile.target
            || current.output != profile.output
        {
            return Err(HubError::Invalid(
                "publication subject is no longer current".into(),
            ));
        }
        Ok(())
    }

    pub fn publish_attached_branch(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        attachment: &str,
        request: PublishBranchRequest,
    ) -> Result<BranchStatus> {
        let (profile, task) = self.publication_profile(repository, profile_id, attachment)?;
        if actor.user_id() != profile.owner_id {
            return Err(HubError::Unauthorized);
        }
        let prepare_operation =
            operation_id(actor.user_id(), attachment, &request.prepare_request_id)?;
        let operation = operation_id(actor.user_id(), attachment, &request.request_id)?;
        let mirror = publication(Mirror::open(
            &profile.git,
            &profile.mirror,
            &task.native_repository,
        ))?;
        let candidate = publication(mirror.retained_candidate_operation(&prepare_operation))?
            .ok_or_else(|| HubError::Invalid("publication prepare was not found".into()))?;
        if candidate.subject.id != request.candidate_id {
            return Err(HubError::Invalid(
                "candidate differs from retained prepare".into(),
            ));
        }
        self.current_candidate(
            actor,
            &task,
            &profile,
            attachment,
            request.expected_generation,
            &candidate.subject,
        )?;
        let config_digest =
            crate::store::digest(&serde_json::to_vec(&profile).map_err(crate::Error::from)?);
        let input = json!({
            "config_digest": config_digest,
            "actor": actor.authorization_identity(),
            "repository_id": repository,
            "profile_id": profile_id,
            "attachment_id": attachment,
            "request": request,
            "subject": candidate.subject,
        });
        publication(mirror.bind_mirrored_candidate_operation(&operation, &input, &candidate))?;
        let expected_oid = if let Some(previous) = request.previous_branch_request_id.as_deref() {
            let previous = operation_id(actor.user_id(), attachment, previous)?;
            let receipt = publication(mirror.remote_lookup(&previous))?
                .ok_or_else(|| HubError::Invalid("branch predecessor was not found".into()))?;
            if !matches!(
                receipt.state,
                RemoteState::Published | RemoteState::ObservedDesired
            ) || receipt.commit != candidate.subject.parent_commit
                || !matches!(receipt.request, RemoteRequest::PublishCandidate { .. })
            {
                return Err(HubError::Invalid(
                    "branch predecessor does not prove the expected preimage".into(),
                ));
            }
            Some(candidate.subject.parent_commit.clone())
        } else {
            // An unpublished merge candidate can have a parent other than its captured baseline.
            // Only an explicit retained predecessor proves a remote preimage; without one this
            // stays create-only and never adopts an observed branch.
            None
        };
        let remote = RemoteRequest::PublishCandidate {
            operation_id: operation,
            candidate_id: candidate.subject.id.clone(),
            expected_oid,
        };
        let transport = access(&profile)?;
        let record = publication(mirror.remote_run_guarded(&remote, &transport, || {
            self.current_candidate(
                actor,
                &task,
                &profile,
                attachment,
                request.expected_generation,
                &candidate.subject,
            )
            .map_err(|error| crate::Error::Conflict(error.to_string()))
        }))?;
        let current = self.current_candidate(
            actor,
            &task,
            &profile,
            attachment,
            request.expected_generation,
            &candidate.subject,
        );
        let currentness = match current {
            Ok(()) => "current",
            Err(HubError::Invalid(_) | HubError::Unauthorized) => "revoked_or_stale",
            Err(_) => "unavailable_or_unknown",
        };
        Ok(branch_status(
            request.request_id,
            candidate.subject.id,
            record,
            currentness,
        ))
    }

    pub fn recover_attached_branch(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        attachment: &str,
        request: RecoverBranchRequest,
    ) -> Result<BranchStatus> {
        let (profile, task) = self.publication_profile(repository, profile_id, attachment)?;
        if actor.user_id() != profile.owner_id {
            return Err(HubError::Unauthorized);
        }
        self.workspace.authorize(
            actor,
            repository,
            super::workspace::WorkspaceAction::ReadRepository,
        )?;
        let prepare_operation =
            operation_id(actor.user_id(), attachment, &request.prepare_request_id)?;
        let operation = operation_id(actor.user_id(), attachment, &request.request_id)?;
        let mirror = publication(Mirror::open(
            &profile.git,
            &profile.mirror,
            &task.native_repository,
        ))?;
        let candidate = publication(mirror.retained_candidate_operation(&prepare_operation))?
            .ok_or_else(|| HubError::Invalid("publication prepare was not found".into()))?;
        if candidate.subject.id != request.candidate_id {
            return Err(HubError::Invalid(
                "candidate differs from retained prepare".into(),
            ));
        }
        let retained = publication(mirror.remote_lookup(&operation))?
            .ok_or_else(|| HubError::Invalid("branch attempt was not found".into()))?;
        if !matches!(&retained.request, RemoteRequest::PublishCandidate { candidate_id, .. } if candidate_id == &candidate.subject.id)
        {
            return Err(HubError::Invalid(
                "branch attempt belongs to another candidate".into(),
            ));
        }
        let record = publication(mirror.remote_recover_with(&operation, &access(&profile)?))?;
        Ok(branch_status(
            request.request_id,
            candidate.subject.id,
            record,
            "not_rechecked",
        ))
    }
}

fn compared_state_worktree(
    data: &std::path::Path,
    repository: &str,
    attachment: &str,
) -> crate::Result<String> {
    Ok(
        crate::attached_worktree::AttachmentJournal::open_existing(data, repository)?
            .state(attachment)?
            .worktree,
    )
}

async fn prepare(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<PrepareRequest>,
) -> std::result::Result<Json<PublicationStatus>, ApiError> {
    call(&state, move |service| {
        service.prepare_attached_publication(&actor, &repository, &profile, &attachment, request)
    })
    .await
}

async fn status(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<StatusRequest>,
) -> std::result::Result<Json<PublicationStatus>, ApiError> {
    call(&state, move |service| {
        service.attached_publication_status(&actor, &repository, &profile, &attachment, request)
    })
    .await
}

async fn publish_branch(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<PublishBranchRequest>,
) -> std::result::Result<Json<BranchStatus>, ApiError> {
    call(&state, move |service| {
        service.publish_attached_branch(&actor, &repository, &profile, &attachment, request)
    })
    .await
}

async fn recover_branch(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<RecoverBranchRequest>,
) -> std::result::Result<Json<BranchStatus>, ApiError> {
    call(&state, move |service| {
        service.recover_attached_branch(&actor, &repository, &profile, &attachment, request)
    })
    .await
}

async fn call<T: Serialize + Send + 'static>(
    state: &HubState,
    work: impl FnOnce(Arc<ExecutionService>) -> Result<T> + Send + 'static,
) -> std::result::Result<Json<T>, ApiError> {
    let execution = state.workspace_execution.clone().ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "execution_not_configured",
        )
    })?;
    blocking(state, move || work(execution))
        .await
        .map(Json)
        .map_err(super::workspace::api_error)
}

async fn mcp(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    body: Bytes,
) -> std::result::Result<Response, ApiError> {
    let Ok(message) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok(mcp_error(&serde_json::Value::Null, -32700, "parse error"));
    };
    let Some(object) = message.as_object() else {
        return Ok(mcp_error(
            &serde_json::Value::Null,
            -32600,
            "invalid request",
        ));
    };
    let Some(id) = object.get("id") else {
        return Ok(axum::http::StatusCode::ACCEPTED.into_response());
    };
    let result = match object.get("method").and_then(serde_json::Value::as_str) {
        Some("initialize") => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"agentscm-attached-publication","version":env!("CARGO_PKG_VERSION")}})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":[
            {"name":"agentscm_prepare_attached_publication","description":"Prepare one exact operator-pinned unlanded Git candidate; does not push or open a pull request.","inputSchema":prepare_schema()},
            {"name":"agentscm_attached_publication_status","description":"Read a retained prepare outcome without re-running it.","inputSchema":status_schema()},
            {"name":"agentscm_publish_attached_branch","description":"Publish the exact retained candidate branch under fresh workspace authorization.","inputSchema":publish_branch_schema()},
            {"name":"agentscm_publish_attached_forge","description":"Open or adopt the exact bound candidate PR/MR under live authorization.","inputSchema":forge_schema()},
            {"name":"agentscm_recover_attached_forge","description":"Observe a retained PR/MR attempt without creating another.","inputSchema":forge_recover_schema()},
            {"name":"agentscm_recover_attached_branch","description":"Observe an uncertain retained branch publication without repeating a push.","inputSchema":recover_branch_schema()}
        ]}),
        Some("tools/call") => {
            let params = object.get("params").and_then(serde_json::Value::as_object);
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(serde_json::Value::as_str);
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let execution = state.workspace_execution.clone().ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "execution_not_configured",
                )
            })?;
            let value = match name {
                Some("agentscm_publish_attached_forge") => {
                    let request =
                        serde_json::from_value(arguments).map_err(|_| ApiError::invalid())?;
                    detached(forge_call(
                        state.clone(),
                        actor,
                        repository,
                        profile,
                        attachment,
                        request,
                    ))
                    .await?
                }
                Some("agentscm_recover_attached_forge") => {
                    let request =
                        serde_json::from_value(arguments).map_err(|_| ApiError::invalid())?;
                    detached(forge_recover_call(
                        state.clone(),
                        actor,
                        repository,
                        profile,
                        attachment,
                        request,
                    ))
                    .await?
                }
                Some("agentscm_prepare_attached_publication") => {
                    let request: PrepareRequest =
                        serde_json::from_value(arguments).map_err(|_| {
                            ApiError::new(axum::http::StatusCode::BAD_REQUEST, "invalid_params")
                        })?;
                    serde_json::to_value(
                        blocking(&state, move || {
                            execution.prepare_attached_publication(
                                &actor,
                                &repository,
                                &profile,
                                &attachment,
                                request,
                            )
                        })
                        .await
                        .map_err(super::workspace::api_error)?,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "encoding_failed",
                        )
                    })?
                }
                Some("agentscm_attached_publication_status") => {
                    let request: StatusRequest =
                        serde_json::from_value(arguments).map_err(|_| {
                            ApiError::new(axum::http::StatusCode::BAD_REQUEST, "invalid_params")
                        })?;
                    serde_json::to_value(
                        blocking(&state, move || {
                            execution.attached_publication_status(
                                &actor,
                                &repository,
                                &profile,
                                &attachment,
                                request,
                            )
                        })
                        .await
                        .map_err(super::workspace::api_error)?,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "encoding_failed",
                        )
                    })?
                }
                Some("agentscm_publish_attached_branch") => {
                    let request: PublishBranchRequest =
                        serde_json::from_value(arguments).map_err(|_| {
                            ApiError::new(axum::http::StatusCode::BAD_REQUEST, "invalid_params")
                        })?;
                    serde_json::to_value(
                        blocking(&state, move || {
                            execution.publish_attached_branch(
                                &actor,
                                &repository,
                                &profile,
                                &attachment,
                                request,
                            )
                        })
                        .await
                        .map_err(super::workspace::api_error)?,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "encoding_failed",
                        )
                    })?
                }
                Some("agentscm_recover_attached_branch") => {
                    let request: RecoverBranchRequest =
                        serde_json::from_value(arguments).map_err(|_| {
                            ApiError::new(axum::http::StatusCode::BAD_REQUEST, "invalid_params")
                        })?;
                    serde_json::to_value(
                        blocking(&state, move || {
                            execution.recover_attached_branch(
                                &actor,
                                &repository,
                                &profile,
                                &attachment,
                                request,
                            )
                        })
                        .await
                        .map_err(super::workspace::api_error)?,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "encoding_failed",
                        )
                    })?
                }
                _ => return Ok(mcp_error(id, -32602, "invalid params")),
            };
            let text = serde_json::to_string(&value).map_err(|_| {
                ApiError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "encoding_failed",
                )
            })?;
            json!({"content":[{"type":"text","text":text}],"isError":false})
        }
        _ => return Ok(mcp_error(id, -32601, "method not found")),
    };
    Ok(Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response())
}

fn id_schema() -> serde_json::Value {
    json!({"type":"string","minLength":1,"maxLength":48,"pattern":"^[A-Za-z0-9_-]+$"})
}

fn subject_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
    "required":["change_id","change_version","context_digest","revision","requirement_digest"],
    "properties":{
        "change_id":{"type":"string","minLength":1},
        "change_version":{"type":"integer","minimum":0},
        "context_digest":{"type":"string","minLength":64,"maxLength":64},
        "revision":{"type":"string","minLength":64,"maxLength":64},
        "requirement_digest":{"type":"string","minLength":64,"maxLength":64}
    }})
}

fn metadata_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
    "required":["name","email","timestamp","message"],
    "properties":{
        "name":{"type":"string","minLength":1},
        "email":{"type":"string","minLength":1},
        "timestamp":{"type":"integer","minimum":0},
        "message":{"type":"string","minLength":1}
    }})
}

fn prepare_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "required":["request_id","expected_generation","subject","metadata"],
        "properties":{"request_id":id_schema(),"expected_generation":{"type":"integer","minimum":0},
            "subject":subject_schema(),"metadata":metadata_schema()}})
}

fn status_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,"required":["request_id"],
        "properties":{"request_id":id_schema()}})
}

fn publish_branch_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "required":["request_id","prepare_request_id","candidate_id","expected_generation"],
        "properties":{"request_id":id_schema(),"prepare_request_id":id_schema(),
            "candidate_id":{"type":"string","minLength":1,"maxLength":128},
            "expected_generation":{"type":"integer","minimum":0},
            "previous_branch_request_id":id_schema()}})
}

fn recover_branch_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "required":["request_id","prepare_request_id","candidate_id"],
        "properties":{"request_id":id_schema(),"prepare_request_id":id_schema(),
            "candidate_id":{"type":"string","minLength":1,"maxLength":128}}})
}

fn mcp_error(id: &serde_json::Value, code: i32, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}

/// Only the server constructs this value. It is stored separately from the manifest.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ForgeBinding {
    actor: WorkspaceActor,
    repository: String,
    profile: String,
    attachment: String,
    config_digest: String,
    request: ForgeRequest,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeRequest {
    pub request_id: String,
    pub prepare_request_id: String,
    pub branch_request_id: String,
    pub candidate_id: String,
    pub expected_generation: u64,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub previous_publication: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverForgeRequest {
    pub publication_id: String,
}

fn forge_config(
    profile: &AttachedPublicationProfile,
    task: &super::workspace_execution::AgentTaskProfile,
) -> Result<String> {
    Ok(crate::store::digest(
        &serde_json::to_vec(&(profile, task)).map_err(crate::Error::from)?,
    ))
}

impl ExecutionService {
    fn forge_parts(
        &self,
        binding: &ForgeBinding,
    ) -> Result<(
        AttachedPublicationProfile,
        super::workspace_execution::AgentTaskProfile,
    )> {
        let (profile, task) =
            self.publication_profile(&binding.repository, &binding.profile, &binding.attachment)?;
        if profile.owner_id != binding.actor.user_id()
            || forge_config(&profile, &task)? != binding.config_digest
        {
            return Err(HubError::Unauthorized);
        }
        Ok((profile, task))
    }

    pub(super) fn check_forge_binding(
        &self,
        binding: &ForgeBinding,
        retained: &super::publish::Publication,
    ) -> Result<()> {
        let (profile, task) = self.forge_parts(binding)?;
        let candidate = retained
            .manifest()?
            .candidate
            .ok_or_else(|| HubError::Invalid("bound candidate missing".into()))?;
        if retained.owner != binding.actor.user_id()
            || retained.repository != binding.repository
            || candidate.subject.id != binding.request.candidate_id
            || candidate.subject.attachment != binding.attachment
        {
            return Err(HubError::Unauthorized);
        }
        self.current_candidate(
            &binding.actor,
            &task,
            &profile,
            &binding.attachment,
            binding.request.expected_generation,
            &candidate.subject,
        )
    }

    pub(super) fn bound_native_state(
        &self,
        binding: &ForgeBinding,
        retained: &super::publish::Publication,
        requested_profile: &super::workspace_execution::AgentTaskProfile,
    ) -> super::forge_check::NativeState {
        use super::forge_check::NativeState;
        let result = (|| -> Result<NativeState> {
            let (profile, task) = self.forge_parts(binding)?;
            if task.profile_id != requested_profile.profile_id
                || task.repository_id != requested_profile.repository_id
            {
                return Err(HubError::Unauthorized);
            }
            self.check_forge_binding(binding, retained)?;
            let subject = retained
                .manifest()?
                .candidate
                .ok_or(HubError::Unauthorized)?
                .subject;
            let context = task.action_subject(&RunSubject {
                change_id: subject.change.id.clone(),
                change_version: subject.change_version,
                context_digest: subject.context_digest.clone(),
                revision: subject.revision.clone(),
                requirement_digest: task.requirement_digest.clone(),
            })?;
            let (_, guard) = self.task_parts(&binding.actor, &profile.repository_id, &task)?;
            let clock = Clock;
            let service = publication(AgentTaskService::new_with_collector_authorization(
                &task.data_dir,
                &task.profile_id,
                &clock,
                &guard,
            ))?;
            publication(service.with_live_imported_origins(
                &binding.actor.authorization_identity(),
                &context,
                CollectorAction::PublishCandidate,
                |origins| {
                    Ok(super::forge_check::native_state_with_origins(
                        &task, &subject, origins,
                    ))
                },
            ))
        })();
        match result {
            Ok(state) => state,
            Err(HubError::Unauthorized | HubError::Invalid(_)) => {
                NativeState::Stale("publication authorization revoked or subject stale")
            }
            Err(_) => NativeState::Unavailable("publication authorization unavailable"),
        }
    }

    fn forge_publication(&self, binding: &ForgeBinding) -> Result<super::publish::NewPublication> {
        use super::publish::{
            Candidate, Change, Manifest, NewPublication, Person, derive_candidate_id,
        };
        let (profile, task) = self.forge_parts(binding)?;
        let request = &binding.request;
        operation_id(
            binding.actor.user_id(),
            &binding.attachment,
            &request.request_id,
        )?;
        let prepare = operation_id(
            binding.actor.user_id(),
            &binding.attachment,
            &request.prepare_request_id,
        )?;
        let branch = operation_id(
            binding.actor.user_id(),
            &binding.attachment,
            &request.branch_request_id,
        )?;
        let mirror = publication(Mirror::open(
            &profile.git,
            &profile.mirror,
            &task.native_repository,
        ))?;
        let candidate = publication(mirror.retained_candidate_operation(&prepare))?
            .ok_or(HubError::Unauthorized)?;
        let input =
            publication(mirror.candidate_binding_input(&branch))?.ok_or(HubError::Unauthorized)?;
        let config =
            crate::store::digest(&serde_json::to_vec(&profile).map_err(crate::Error::from)?);
        if candidate.subject.id != request.candidate_id
            || input["config_digest"] != config
            || input["actor"]
                != serde_json::to_value(binding.actor.authorization_identity())
                    .map_err(crate::Error::from)?
            || input["repository_id"] != binding.repository
            || input["profile_id"] != binding.profile
            || input["attachment_id"] != binding.attachment
            || input["request"]["prepare_request_id"] != request.prepare_request_id
            || input["request"]["candidate_id"] != request.candidate_id
            || input["request"]["expected_generation"] != request.expected_generation
        {
            return Err(HubError::Unauthorized);
        }
        let receipt = publication(mirror.remote_lookup(&branch))?.ok_or(HubError::Unauthorized)?;
        if !matches!(
            receipt.state,
            RemoteState::Published | RemoteState::ObservedDesired
        ) || !matches!(&receipt.request, RemoteRequest::PublishCandidate { candidate_id, .. } if candidate_id == &request.candidate_id)
        {
            return Err(HubError::Invalid(
                "branch publication is not confirmed".into(),
            ));
        }
        self.current_candidate(
            &binding.actor,
            &task,
            &profile,
            &binding.attachment,
            request.expected_generation,
            &candidate.subject,
        )?;
        let export = publication(mirror.candidate_publication(&request.candidate_id))?;
        if export.commit != receipt.commit {
            return Err(HubError::Invalid("branch receipt commit differs".into()));
        }
        let mut changes = Vec::new();
        for change in export.changes {
            let sha256 = change
                .blob
                .as_ref()
                .map(|oid| {
                    mirror
                        .blob(oid, change.size)
                        .map(|bytes| crate::store::digest(&bytes))
                })
                .transpose()?;
            changes.push(Change {
                path: change.path,
                mode: change.mode,
                blob: change.blob,
                sha256,
                size: change.size,
            });
        }
        let person = |p: crate::git_export::publish::Person| Person {
            name: p.name,
            email: p.email,
            date: p.date,
        };
        let new = NewPublication {
            id: derive_candidate_id(
                &binding.repository,
                &request.candidate_id,
                &export.commit,
                &profile.output,
            ),
            manifest: Manifest {
                revision: export.revision,
                event_seq: None,
                candidate: Some(Candidate {
                    subject: candidate.subject,
                    endpoint: profile.endpoint,
                    previous_publication: request.previous_publication.clone(),
                }),
                commit: export.commit,
                tree: export.tree,
                parent: export.parent,
                branch: profile.output,
                base: profile.target,
                title: request.title.clone(),
                body: request.body.clone(),
                author: person(export.author),
                committer: person(export.committer),
                message: export.message,
                changes,
            },
        };
        new.validate(&binding.repository)?;
        Ok(new)
    }
}

async fn forge_publish(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<ForgeRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    detached(forge_call(
        state, actor, repository, profile, attachment, request,
    ))
    .await
    .map(Json)
}

async fn forge_call(
    state: HubState,
    actor: WorkspaceActor,
    repository: String,
    profile: String,
    attachment: String,
    request: ForgeRequest,
) -> std::result::Result<serde_json::Value, ApiError> {
    let service = state
        .workspace_execution
        .clone()
        .ok_or(HubError::Unauthorized)?;
    let (publication_profile, task) =
        service.publication_profile(&repository, &profile, &attachment)?;
    let binding = ForgeBinding {
        actor: actor.clone(),
        repository: repository.clone(),
        profile,
        attachment,
        config_digest: forge_config(&publication_profile, &task)?,
        request,
    };
    let principal = forge_principal(&state, &actor, &repository).await?;
    let (_, connected) = super::publish::target(&state, &principal, true, true).await?;
    // Admission and execution use the same lock as provider sync; never retain a workspace guard.
    let lock = state.sync.lock_repo(&connected.id).await;
    let b = binding.clone();
    let new = blocking(&state, move || service.forge_publication(&b))
        .await
        .map_err(super::workspace::api_error)?;
    let (host, full) = super::publish::candidate_repository(
        &new.manifest
            .candidate
            .as_ref()
            .ok_or(HubError::Unauthorized)?
            .endpoint,
    )?;
    if host != connected.host || full != connected.full_name {
        return Err(HubError::Unauthorized.into());
    }
    let store = state.store.clone();
    let encoded = serde_json::to_string(&binding)
        .map_err(|_| HubError::Invalid("binding encoding failed".into()))?;
    let retained = blocking(&state, move || {
        store.create_bound_publication(
            &principal.token_id,
            &new,
            Some(&encoded),
            super::store::unix_now(),
        )
    })
    .await?;
    let super::publish::Create::Done(retained) = retained else {
        return Err(HubError::Invalid("publication binding conflicts".into()).into());
    };
    drop(lock);
    let job = tokio::spawn(super::publish::execute(state, connected, retained, false));
    let retained = job
        .await
        .map_err(|_| HubError::Unavailable("publication task failed".into()))??;
    Ok(retained.to_json())
}

async fn forge_principal(
    state: &HubState,
    actor: &WorkspaceActor,
    repository: &str,
) -> Result<super::store::Principal> {
    let identity = actor.agent_identity()?;
    let digest = super::store::TokenDigest::from_persisted(&identity.agent_id)?;
    let store = state.store.clone();
    let principal = blocking(state, move || {
        store.authenticate(&digest, super::store::unix_now())
    })
    .await?
    .ok_or(HubError::Unauthorized)?;
    if principal.owner.id != actor.user_id() || principal.repository != repository {
        return Err(HubError::Unauthorized);
    }
    Ok(principal)
}

async fn forge_recover(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, attachment)): Path<(String, String, String)>,
    JsonBody(request): JsonBody<RecoverForgeRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    detached(forge_recover_call(
        state, actor, repository, profile, attachment, request,
    ))
    .await
    .map(Json)
}

async fn forge_recover_call(
    state: HubState,
    actor: WorkspaceActor,
    repository: String,
    profile: String,
    attachment: String,
    request: RecoverForgeRequest,
) -> std::result::Result<serde_json::Value, ApiError> {
    let service = state
        .workspace_execution
        .clone()
        .ok_or(HubError::Unauthorized)?;
    let (p, _) = service.publication_profile(&repository, &profile, &attachment)?;
    if p.owner_id != actor.user_id() {
        return Err(HubError::Unauthorized.into());
    }
    service.workspace.authorize(
        &actor,
        &repository,
        super::workspace::WorkspaceAction::ReadRepository,
    )?;
    let principal = forge_principal(&state, &actor, &repository).await?;
    let store = state.store.clone();
    let repo = repository.clone();
    let retained = blocking(&state, move || {
        store.publication(&repo, &request.publication_id)
    })
    .await?
    .ok_or(HubError::Unauthorized)?;
    let binding: ForgeBinding = serde_json::from_str(
        retained
            .authorization_binding
            .as_deref()
            .ok_or(HubError::Unauthorized)?,
    )
    .map_err(|_| HubError::Unauthorized)?;
    if binding.repository != repository
        || binding.profile != profile
        || binding.attachment != attachment
        || retained.owner != actor.user_id()
    {
        return Err(HubError::Unauthorized.into());
    }
    let (_, connected) = super::publish::target(&state, &principal, false, true).await?;
    let retained = super::publish::execute(state, connected, retained, true).await?;
    Ok(retained.to_json())
}

fn forge_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,
        "required":["request_id","prepare_request_id","branch_request_id","candidate_id","expected_generation","title","body"],
        "properties":{"request_id":{"type":"string"},"prepare_request_id":{"type":"string"},"branch_request_id":{"type":"string"},"candidate_id":{"type":"string"},"expected_generation":{"type":"integer","minimum":0},"title":{"type":"string","minLength":1,"maxLength":256},"body":{"type":"string","maxLength":65536},"previous_publication":{"type":["string","null"]}}})
}
fn forge_recover_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,"required":["publication_id"],"properties":{"publication_id":{"type":"string"}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::CandidateSubject;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn hub_state(dir: &std::path::Path) -> HubState {
        let store =
            crate::hub::store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display()))
                .unwrap();
        HubState::new(
            store,
            Arc::new(crate::backend::LocalBackend::open(dir.join("objects")).unwrap()),
        )
    }

    /// Serves `crate::hub::bounded`, the wiring `hub::router` uses, around injected routes.
    /// Tests run with paused time, so the production 30- and 120-second bounds apply.
    async fn serve(
        routes: Router<HubState>,
        publication: Router<HubState>,
        dir: &std::path::Path,
    ) -> std::net::SocketAddr {
        let state = hub_state(dir);
        let app = crate::hub::bounded(routes, publication, &state).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        address
    }

    async fn send(address: std::net::SocketAddr, path: &str, body: Vec<u8>) -> (u16, Vec<u8>) {
        let response = reqwest::Client::new()
            .post(format!("http://{address}{path}"))
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.bytes().await.unwrap().to_vec())
    }

    /// Sends a request head promising a body that never arrives; returns the reply and when.
    async fn stalled_body(address: std::net::SocketAddr, path: &str) -> (Vec<u8>, Duration) {
        let started = tokio::time::Instant::now();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!("POST {path} HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let mut response = vec![0; 4096];
        let read = stream.read(&mut response).await.unwrap();
        response.truncate(read);
        (response, started.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn publication_processing_outlasts_the_hub_cap_while_other_routes_keep_it() {
        let dir = tempfile::tempdir().unwrap();
        let slow = || async { tokio::time::sleep(Duration::from_secs(45)).await };
        let address = serve(
            Router::new().route("/x/publication/status", post(slow)),
            Router::new().route("/x/publication/branch", post(slow)),
            dir.path(),
        )
        .await;
        assert_eq!(send(address, "/x/publication/branch", vec![]).await.0, 200);
        assert_eq!(send(address, "/x/publication/status", vec![]).await.0, 408);
    }

    #[tokio::test(start_paused = true)]
    async fn publication_past_its_bound_names_the_follow_up_for_work_that_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        let slow = move || {
            let done = done.clone();
            detached(async move {
                tokio::time::sleep(Duration::from_secs(150)).await;
                done.store(true, Ordering::Release);
                Ok(())
            })
        };
        let address = serve(
            Router::new(),
            Router::new()
                .route("/x/publication", post(slow.clone()))
                .route("/x/publication/branch", post(slow)),
            dir.path(),
        )
        .await;
        for (path, follow_up) in [
            ("/x/publication/branch", "branch/recover"),
            ("/x/publication", "status"),
        ] {
            let (status, body) = send(address, path, vec![]).await;
            assert_eq!(status, 202);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body, json!({"state":"still_running","follow_up":follow_up}));
            // The first request's work outlived its 202 and finished during the second.
            assert_eq!(finished.load(Ordering::Acquire), path == "/x/publication");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn publication_bodies_keep_the_hub_read_bound_and_size_rejections() {
        let dir = tempfile::tempdir().unwrap();
        let address = serve(
            Router::new(),
            Router::new()
                .route("/x/publication/mcp", post(|_: Bytes| async {}))
                .route(
                    "/x/publication/branch",
                    post(|JsonBody(_): JsonBody<serde_json::Value>| async {}),
                ),
            dir.path(),
        )
        .await;
        let (response, elapsed) = stalled_body(address, "/x/publication/branch").await;
        assert!(response.starts_with(b"HTTP/1.1 408"), "{response:?}");
        assert!(elapsed >= Duration::from_secs(30) && elapsed < Duration::from_secs(31));
        let oversized = vec![b' '; super::super::MAX_BODY_BYTES + 1];
        assert_eq!(
            send(address, "/x/publication/mcp", oversized.clone())
                .await
                .0,
            413
        );
        // `JsonBody` answers every failure, including size, with its fixed 400.
        assert_eq!(
            send(address, "/x/publication/branch", oversized).await.0,
            400
        );
    }

    #[tokio::test(start_paused = true)]
    async fn publication_routes_authenticate_before_reading_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::hub::router(hub_state(dir.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let (response, elapsed) = stalled_body(
            address,
            "/v1/hub/workspace/repositories/r/profiles/p/attachments/a/publication",
        )
        .await;
        assert!(!response.starts_with(b"HTTP/1.1 408"), "{response:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn mcp_past_its_bound_is_a_json_rpc_error_naming_the_recovery_tool() {
        let body = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"agentscm_publish_attached_branch","arguments":{}}});
        let response = still_running("/x/publication/mcp", &serde_json::to_vec(&body).unwrap());
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response);
        assert_eq!(body["id"], 7);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("agentscm_recover_attached_branch")
        );
    }

    fn response_json(response: Response) -> serde_json::Value {
        let bytes = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    use crate::context::ChangeId;
    use crate::git_export::ExportState;
    use crate::skill_workflow::AuthenticatedAgentIdentity;

    fn request(generation: u64) -> PrepareRequest {
        PrepareRequest {
            request_id: "same-request".into(),
            expected_generation: generation,
            subject: RunSubject {
                change_id: "change".into(),
                change_version: 1,
                context_digest: "a".repeat(64),
                revision: "b".repeat(64),
                requirement_digest: "c".repeat(64),
            },
            metadata: CommitMetadata {
                name: "Synthetic".into(),
                email: "synthetic@example.invalid".into(),
                timestamp: 1,
                message: "prepare".into(),
            },
        }
    }

    #[test]
    fn fixed_prepare_input_binds_generation_and_contains_no_operator_paths_or_secrets() {
        let actor = AuthenticatedAgentIdentity {
            owner_id: "usr_owner".into(),
            agent_id: "token-digest".into(),
        };
        let first = prepare_input(
            "config",
            &actor,
            "repo",
            "profile",
            "attachment",
            &request(4),
        );
        let changed = prepare_input(
            "config",
            &actor,
            "repo",
            "profile",
            "attachment",
            &request(5),
        );
        assert_ne!(first, changed);
        assert_eq!(first["expected_generation"], 4);
        let encoded = first.to_string();
        for forbidden in [
            "credential_file",
            "ca_file",
            "worktree",
            "mirror",
            "endpoint",
            "target",
            "output",
        ] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
    }

    #[test]
    fn retained_status_projection_does_not_expose_endpoint_paths_or_credentials() {
        let record = CandidateExportRecord {
            profile: "agentscm-git-export-v3".into(),
            repository: "d".repeat(32),
            request: CandidateExportRequest {
                subject: CandidateSubject {
                    id: "candidate".into(),
                    attachment: "attachment".into(),
                    endpoint: "/private/remote.git".into(),
                    output: "agents/work".into(),
                    change: ChangeId {
                        repository: "d".repeat(32),
                        id: "change".into(),
                    },
                    change_version: 1,
                    context_digest: "a".repeat(64),
                    policy_epoch: 0,
                    revision: "b".repeat(64),
                    target: "main".into(),
                    baseline_commit: "1".repeat(40),
                    parent_commit: "2".repeat(40),
                    source_commit: "3".repeat(40),
                    source_fingerprint: "c".repeat(64),
                    comparison_fingerprint: "d".repeat(64),
                },
                expected_oid: "2".repeat(40),
                metadata: request(1).metadata,
            },
            tree: Some("4".repeat(40)),
            commit: Some("5".repeat(40)),
            state: ExportState::Mirrored,
            error: None,
        };
        let encoded = serde_json::to_string(&PublicationCandidate::from(record)).unwrap();
        assert!(!encoded.contains("/private/remote.git"));
        for forbidden in [
            "endpoint",
            "output",
            "target",
            "metadata",
            "profile",
            "repository",
        ] {
            assert!(!encoded.contains(forbidden), "{encoded}");
        }
    }

    #[test]
    fn reservation_only_prepare_resumes_retained_parent_and_refuses_drift() {
        let current = CandidateSubject {
            id: String::new(),
            attachment: "attachment".into(),
            endpoint: "/remote.git".into(),
            output: "agents/work".into(),
            change: ChangeId {
                repository: "d".repeat(32),
                id: "change".into(),
            },
            change_version: 1,
            context_digest: "a".repeat(64),
            policy_epoch: 0,
            revision: "b".repeat(64),
            target: "main".into(),
            baseline_commit: "1".repeat(40),
            parent_commit: String::new(),
            source_commit: "3".repeat(40),
            source_fingerprint: "c".repeat(64),
            comparison_fingerprint: "d".repeat(64),
        };
        let metadata = request(1).metadata;
        let mut retained_subject = current.clone();
        retained_subject.parent_commit = "2".repeat(40);
        retained_subject.id = retained_subject.canonical_id().unwrap();
        let retained = CandidateExportRequest {
            expected_oid: retained_subject.parent_commit.clone(),
            subject: retained_subject,
            metadata: metadata.clone(),
        };
        assert_eq!(
            resume_candidate(&current, &metadata, retained.clone()).unwrap(),
            retained
        );
        let mut drifted = current;
        drifted.revision = "e".repeat(64);
        assert!(resume_candidate(&drifted, &metadata, retained).is_err());
    }

    #[test]
    fn mcp_schemas_describe_nested_inputs_and_requests_reject_injected_fields() {
        assert_eq!(
            prepare_schema()["properties"]["subject"]["additionalProperties"],
            false
        );
        assert_eq!(
            prepare_schema()["properties"]["metadata"]["additionalProperties"],
            false
        );
        assert_eq!(publish_branch_schema()["additionalProperties"], false);
        assert_eq!(forge_schema()["additionalProperties"], false);
        assert_eq!(forge_recover_schema()["additionalProperties"], false);
        let request = json!({"request_id":"forge", "prepare_request_id":"prepare", "branch_request_id":"branch", "candidate_id":"candidate", "expected_generation":1, "title":"title", "body":""});
        assert!(serde_json::from_value::<ForgeRequest>(request.clone()).is_ok());
        for field in [
            "endpoint",
            "worktree",
            "credential_file",
            "branch",
            "authorization_binding",
        ] {
            let mut injected = request.clone();
            injected[field] = json!("untrusted");
            assert!(serde_json::from_value::<ForgeRequest>(injected).is_err());
        }
        assert!(
            serde_json::from_value::<RecoverForgeRequest>(
                json!({"publication_id":"pub", "create":true})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<PrepareRequest>(json!({
            "request_id":"request","expected_generation":1,
            "subject":{"change_id":"change","change_version":1,"context_digest":"a".repeat(64),
                "revision":"b".repeat(64),"requirement_digest":"c".repeat(64),"injected":true},
            "metadata":{"name":"Synthetic","email":"synthetic@example.invalid","timestamp":1,"message":"test"}
        })).is_err());
        assert!(
            serde_json::from_value::<PublishBranchRequest>(json!({
                "request_id":"branch","prepare_request_id":"prepare","candidate_id":"candidate",
                "expected_generation":1,"injected":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RecoverBranchRequest>(json!({
                "request_id":"branch","prepare_request_id":"prepare","candidate_id":"candidate",
                "injected":true
            }))
            .is_err()
        );
    }
}
