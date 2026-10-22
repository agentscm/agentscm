//! Authenticated preparation of an operator-pinned attached Git candidate.
//!
//! This phase writes only the existing owned Mirror journal/ref. It never contacts a remote,
//! pushes a ref, opens a pull request, or posts forge status.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
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

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication",
            post(prepare),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/status",
            post(status),
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
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication/mcp",
            post(mcp),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::CandidateSubject;
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
