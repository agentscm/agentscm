//! Workspace-authenticated facade over the shared agent-task journal.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::HubState;
use super::http::{ApiError, blocking};
use super::workspace::{AgentTaskAuthorization, WorkspaceAction, WorkspaceActor};
use super::workspace_execution::{AgentTaskProfile, ExecutionService, RunSubject};
use super::{HubError, Result};
use crate::skill_workflow::*;

const AGENT_TASK_ENVELOPE_HEADROOM_BYTES: usize = 16 * 1024;
pub(crate) const MAX_AGENT_TASK_REQUEST_BYTES: usize =
    crate::skill_workflow::task_contracts::MAX_WIRE_BYTES + AGENT_TASK_ENVELOPE_HEADROOM_BYTES;

struct Clock;

impl TaskClock for Clock {
    fn now_ms(&self) -> crate::Result<u64> {
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| crate::Error::Invalid("system clock precedes epoch".into()))?
                .as_millis(),
        )
        .map_err(|_| crate::Error::Limit("system clock exceeds task timestamp".into()))
    }
}

struct WorkspaceGuard<'a> {
    service: &'a ExecutionService,
    actor: &'a WorkspaceActor,
    repository: &'a str,
    native_repository: String,
    identity: AuthenticatedAgentIdentity,
    audited: Option<(String, Value)>,
}

impl WorkspaceGuard<'_> {
    fn check_actor(
        &self,
        actor: &AuthenticatedAgentIdentity,
        repository: &str,
    ) -> crate::Result<()> {
        if actor != &self.identity || repository != self.native_repository {
            return Err(crate::Error::Conflict(
                "workspace task actor or repository binding changed".into(),
            ));
        }
        Ok(())
    }

    fn guard_error(error: HubError) -> crate::Error {
        match error {
            HubError::Busy | HubError::Unavailable(_) => crate::Error::Io(std::io::Error::other(
                "workspace task authorization unavailable",
            )),
            HubError::Invalid(_) | HubError::Unauthorized => {
                crate::Error::Conflict("workspace task authorization changed".into())
            }
        }
    }

    fn with_preserved_transition_error(
        &self,
        binding: AgentTaskAuthorization<'_>,
        transition: &mut dyn FnMut() -> crate::Result<()>,
    ) -> crate::Result<()> {
        let mut transition_error = None;
        let mut guarded_transition = || match transition() {
            Ok(()) => Ok(()),
            Err(error) => {
                transition_error = Some(error);
                Err(crate::Error::Invalid(
                    "workspace task transition failed".into(),
                ))
            }
        };
        let outcome = self
            .service
            .workspace
            .with_agent_task_authorization(binding, &mut guarded_transition);
        match transition_error {
            Some(error) => Err(error),
            None => outcome.map_err(Self::guard_error),
        }
    }
}

impl TaskAuthorizationGuard for WorkspaceGuard<'_> {
    fn authorize(&self, actor: &AuthenticatedAgentIdentity, repository: &str) -> crate::Result<()> {
        self.check_actor(actor, repository)?;
        self.service
            .workspace
            .authorize(
                self.actor,
                self.repository,
                WorkspaceAction::ExecuteWorkflow,
            )
            .map_err(Self::guard_error)
    }

    fn with_current_authorization(
        &self,
        actor: &AuthenticatedAgentIdentity,
        repository: &str,
        transition: &mut dyn FnMut() -> crate::Result<()>,
    ) -> crate::Result<()> {
        self.check_actor(actor, repository)?;
        self.with_preserved_transition_error(
            AgentTaskAuthorization {
                actor: self.actor,
                repository: self.repository,
                core_actor: actor,
                action: CollectorAction::Submit,
                origins: &[],
                audited: None,
            },
            transition,
        )
    }
}

impl CollectorOriginAuthorizationGuard for WorkspaceGuard<'_> {
    fn with_current_collector_authorization(
        &self,
        actor: &AuthenticatedAgentIdentity,
        action: CollectorAction,
        origins: &[OriginGrantBinding],
        repository: &str,
        transition: &mut dyn FnMut() -> crate::Result<()>,
    ) -> crate::Result<()> {
        self.check_actor(actor, repository)?;
        self.with_preserved_transition_error(
            AgentTaskAuthorization {
                actor: self.actor,
                repository: self.repository,
                core_actor: actor,
                action,
                origins,
                audited: self
                    .audited
                    .as_ref()
                    .map(|(request_id, subject)| (request_id.as_str(), subject)),
            },
            transition,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimInput {
    pub version: u32,
    pub request_id: String,
    pub execution_operation_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub executor: ExecutorBinding,
    pub requested_lease_ms: u64,
}

impl ClaimInput {
    fn bind(self, agent: AuthenticatedAgentIdentity) -> TaskClaimRequest {
        TaskClaimRequest {
            version: self.version,
            request_id: self.request_id,
            execution_operation_id: self.execution_operation_id,
            task_id: self.task_id,
            task_digest: self.task_digest,
            attempt_id: self.attempt_id,
            agent,
            executor: self.executor,
            requested_lease_ms: self.requested_lease_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationInput {
    pub version: u32,
    pub request_id: String,
    pub execution_operation_id: String,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub executor: ExecutorBinding,
    pub action: TaskMutationAction,
}

impl MutationInput {
    fn bind(self, agent: AuthenticatedAgentIdentity) -> TaskMutationRequest {
        TaskMutationRequest {
            version: self.version,
            request_id: self.request_id,
            execution_operation_id: self.execution_operation_id,
            task_id: self.task_id,
            task_digest: self.task_digest,
            attempt_id: self.attempt_id,
            attempt_digest: self.attempt_digest,
            fence: self.fence,
            agent,
            executor: self.executor,
            action: self.action,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusInput {
    pub version: u32,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub fence: u64,
    pub executor: ExecutorBinding,
}

impl StatusInput {
    fn bind(self, agent: AuthenticatedAgentIdentity) -> TaskStatusRequest {
        TaskStatusRequest {
            version: self.version,
            task_id: self.task_id,
            task_digest: self.task_digest,
            attempt_id: self.attempt_id,
            attempt_digest: self.attempt_digest,
            fence: self.fence,
            agent,
            executor: self.executor,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssertionInput {
    pub version: u32,
    pub task_id: String,
    pub task_digest: String,
    pub attempt_id: String,
    pub attempt_digest: String,
    pub request_id: String,
    pub execution_operation_id: String,
    pub executor: ExecutorBinding,
    pub fence: u64,
    pub trust_class: EvidenceTrustClass,
    pub outcome: TaskOutcome,
    pub criteria: Vec<CriterionResult>,
    pub executor_provenance: Option<ExecutorObservationProvenance>,
}

impl AssertionInput {
    fn bind(self, agent: AuthenticatedAgentIdentity) -> TaskResultAssertion {
        TaskResultAssertion {
            version: self.version,
            task_id: self.task_id,
            task_digest: self.task_digest,
            attempt_id: self.attempt_id,
            attempt_digest: self.attempt_digest,
            request_id: self.request_id,
            execution_operation_id: self.execution_operation_id,
            agent,
            executor: self.executor,
            fence: self.fence,
            trust_class: self.trust_class,
            outcome: self.outcome,
            criteria: self.criteria,
            executor_provenance: self.executor_provenance,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentTaskRequest {
    Requirements {
        subject: RunSubject,
    },
    Next {
        subject: RunSubject,
    },
    Instructions {
        subject: RunSubject,
        task_id: String,
        task_digest: String,
        supported_schema_versions: Vec<u32>,
    },
    Reference {
        subject: RunSubject,
        task_id: String,
        task_digest: String,
        reference_name: String,
        expected_sha256: String,
        expected_schema_version: u32,
    },
    Claim {
        subject: RunSubject,
        claim: ClaimInput,
    },
    CollectorClaim {
        subject: RunSubject,
        claim: ClaimInput,
        collector_profile_id: String,
    },
    Heartbeat {
        subject: RunSubject,
        mutation: MutationInput,
    },
    Cancel {
        subject: RunSubject,
        mutation: MutationInput,
    },
    Reconcile {
        subject: RunSubject,
        mutation: MutationInput,
    },
    Result {
        subject: RunSubject,
        assertion: AssertionInput,
    },
    Status {
        subject: RunSubject,
        status: StatusInput,
    },
    Recovery {
        subject: RunSubject,
        mutation: MutationInput,
    },
    CollectorDescriptor {
        subject: RunSubject,
        status: StatusInput,
    },
    CollectorFinalize {
        subject: RunSubject,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorUploadRequest {
    pub subject: RunSubject,
    pub transfer: CollectorTransfer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentTaskEnvelope {
    pub capability: String,
    pub schema_version: u32,
    pub result: Value,
}

pub(crate) struct NativeTaskAction<'a> {
    pub repository: &'a str,
    pub profile_id: &'a str,
    pub request_id: &'a str,
    pub expected_generation: i64,
    pub subject: &'a RunSubject,
}

fn encode(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| HubError::Unavailable("workspace task encoding".into()))
}

fn native_action_error(error: crate::Error) -> HubError {
    match error {
        crate::Error::Conflict(_) => HubError::Invalid("workspace_native_conflict".into()),
        crate::Error::Invalid(_) | crate::Error::NotFound(_) => {
            HubError::Invalid("invalid native action".into())
        }
        _ => HubError::Unavailable("native action unavailable".into()),
    }
}

fn task_error(error: crate::Error) -> HubError {
    match error {
        crate::Error::Conflict(message) if message == "workflow task ID is stale or refused" => {
            HubError::Invalid("invalid agent task request".into())
        }
        crate::Error::Conflict(_) => HubError::Invalid("workspace_native_conflict".into()),
        crate::Error::Invalid(_) | crate::Error::NotFound(_) | crate::Error::Limit(_) => {
            HubError::Invalid("invalid agent task request".into())
        }
        _ => HubError::Unavailable("agent task service unavailable".into()),
    }
}

fn task<T>(result: crate::Result<T>) -> Result<T> {
    result.map_err(task_error)
}

impl ExecutionService {
    fn agent_task_profile(&self, repository: &str, profile: &str) -> Result<AgentTaskProfile> {
        self.agent_task_profiles
            .get(&(repository.into(), profile.into()))
            .cloned()
            .ok_or(HubError::Unauthorized)
    }

    fn task_parts<'a>(
        &'a self,
        actor: &'a WorkspaceActor,
        repository: &'a str,
        profile: &AgentTaskProfile,
    ) -> Result<(AuthenticatedAgentIdentity, WorkspaceGuard<'a>)> {
        let identity = actor.agent_identity()?;
        let guard = WorkspaceGuard {
            service: self,
            actor,
            repository,
            native_repository: profile.native_repository.clone(),
            identity: identity.clone(),
            audited: None,
        };
        Ok((identity, guard))
    }

    fn origin(
        actor: &WorkspaceActor,
        identity: AuthenticatedAgentIdentity,
    ) -> Result<OriginGrantBinding> {
        Ok(OriginGrantBinding {
            grant_id: identity.owner_id.clone(),
            actor: identity,
            grant_generation: u64::try_from(actor.generation())
                .map_err(|_| HubError::Unauthorized)?,
        })
    }

    pub fn agent_task_call(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        request: AgentTaskRequest,
    ) -> Result<AgentTaskEnvelope> {
        let profile = self.agent_task_profile(repository, profile_id)?;
        let subject = match &request {
            AgentTaskRequest::Requirements { subject }
            | AgentTaskRequest::Next { subject }
            | AgentTaskRequest::Instructions { subject, .. }
            | AgentTaskRequest::Reference { subject, .. }
            | AgentTaskRequest::Claim { subject, .. }
            | AgentTaskRequest::CollectorClaim { subject, .. }
            | AgentTaskRequest::Heartbeat { subject, .. }
            | AgentTaskRequest::Cancel { subject, .. }
            | AgentTaskRequest::Reconcile { subject, .. }
            | AgentTaskRequest::Result { subject, .. }
            | AgentTaskRequest::Status { subject, .. }
            | AgentTaskRequest::Recovery { subject, .. }
            | AgentTaskRequest::CollectorDescriptor { subject, .. }
            | AgentTaskRequest::CollectorFinalize { subject } => subject,
        };
        // The shared core decides whether an exact retained request may be read or retried
        // after drift. Resolve the immutable registered subject here without imposing a
        // currentness gate that would hide retained status or recovery.
        let context = profile.action_subject(subject)?;
        let (identity, guard) = self.task_parts(actor, repository, &profile)?;
        let clock = Clock;
        let tasks = task(AgentTaskService::new_with_collector_authorization(
            &profile.data_dir,
            &profile.profile_id,
            &clock,
            &guard,
        ))?;
        let value = match request {
            AgentTaskRequest::Requirements { .. } => {
                encode(task(tasks.requirements(&context, &identity))?)?
            }
            AgentTaskRequest::Next { .. } => encode(task(tasks.next_task(&context, &identity))?)?,
            AgentTaskRequest::Instructions {
                task_id,
                task_digest,
                supported_schema_versions,
                ..
            } => encode(task(tasks.instruction_entrypoint(
                &context,
                &identity,
                &task_id,
                &task_digest,
                &supported_schema_versions,
            ))?)?,
            AgentTaskRequest::Reference {
                task_id,
                task_digest,
                reference_name,
                expected_sha256,
                expected_schema_version,
                ..
            } => encode(task(tasks.reference(
                &context,
                &ReferenceFetchRequest {
                    version: AGENT_TASK_CONTRACT_VERSION,
                    task_id,
                    task_digest,
                    reference_name,
                    expected_sha256,
                    expected_schema_version,
                    actor: identity,
                },
            ))?)?,
            AgentTaskRequest::Claim { claim, .. } => {
                encode(task(tasks.claim(&context, &claim.bind(identity)))?)?
            }
            AgentTaskRequest::CollectorClaim {
                claim,
                collector_profile_id,
                ..
            } => encode(task(tasks.claim_collector(
                &context,
                &CollectorClaimRequest {
                    claim: claim.bind(identity),
                    collector_profile_id,
                },
            ))?)?,
            AgentTaskRequest::Heartbeat { mutation, .. } => {
                encode(task(tasks.heartbeat(&context, &mutation.bind(identity)))?)?
            }
            AgentTaskRequest::Cancel { mutation, .. } => encode(task(
                tasks.cancel_attempt(&context, &mutation.bind(identity)),
            )?)?,
            AgentTaskRequest::Reconcile { mutation, .. } => encode(task(
                tasks.reconcile_attempt(&context, &mutation.bind(identity)),
            )?)?,
            AgentTaskRequest::Result { assertion, .. } => encode(task(
                tasks.submit_result(&context, &assertion.bind(identity)),
            )?)?,
            AgentTaskRequest::Status { status, .. } => {
                encode(task(tasks.status(&context, &status.bind(identity)))?)?
            }
            AgentTaskRequest::Recovery { mutation, .. } => {
                encode(task(tasks.recovery(&context, &mutation.bind(identity)))?)?
            }
            AgentTaskRequest::CollectorDescriptor { status, .. } => {
                let bound = status.bind(identity.clone());
                encode(task(tasks.collector_descriptor(
                    &context,
                    &bound,
                    Self::origin(actor, identity)?,
                ))?)?
            }
            AgentTaskRequest::CollectorFinalize { .. } => json!({
                "receipt": task(tasks.reconcile_collector_finalization(&identity, &context))?
            }),
        };
        Ok(AgentTaskEnvelope {
            capability: AGENT_TASK_CAPABILITY.into(),
            schema_version: AGENT_TASK_CONTRACT_VERSION,
            result: value,
        })
    }

    pub async fn agent_task_upload(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile_id: &str,
        request: CollectorUploadRequest,
    ) -> Result<AgentTaskEnvelope> {
        let profile = self.agent_task_profile(repository, profile_id)?;
        let context = profile.action_subject(&request.subject)?;
        let (_identity, guard) = self.task_parts(actor, repository, &profile)?;
        let clock = Clock;
        let tasks = task(AgentTaskService::new_with_collector_authorization(
            &profile.data_dir,
            &profile.profile_id,
            &clock,
            &guard,
        ))?;
        let result = task(
            tasks
                .submit_collector_transfer(&context, &request.transfer)
                .await,
        )?;
        Ok(AgentTaskEnvelope {
            capability: AGENT_TASK_CAPABILITY.into(),
            schema_version: AGENT_TASK_CONTRACT_VERSION,
            result: encode(result)?,
        })
    }

    pub(crate) fn approve_with_agent_task_origins(
        &self,
        actor: &WorkspaceActor,
        binding: NativeTaskAction<'_>,
        approval_id: &str,
    ) -> Result<crate::model::Approval> {
        self.workspace
            .authorize(actor, binding.repository, WorkspaceAction::ApproveRevision)?;
        let profile = self.agent_task_profile(binding.repository, binding.profile_id)?;
        let context = profile.action_subject(binding.subject)?;
        let approval = crate::model::Approval {
            approval_id: approval_id.into(),
            target: context.target.clone(),
            revision: context.revision.clone(),
            context_digest: context.digest()?,
            change: Some(context.change.clone()),
            change_version: Some(context.change_version),
            policy_epoch: context.policy_epoch,
        };
        let retry = crate::authority::Authority::open_read_only(&profile.data_dir)?
            .approval_retry(&approval)
            .map_err(native_action_error)?;
        if !retry {
            if actor.generation() != binding.expected_generation {
                return Err(HubError::Invalid("workspace_generation_conflict".into()));
            }
            profile.validate_subject(binding.subject)?;
        }
        let identity = actor.authorization_identity();
        let guard = WorkspaceGuard {
            service: self,
            actor,
            repository: binding.repository,
            native_repository: profile.native_repository.clone(),
            identity: identity.clone(),
            audited: Some((binding.request_id.into(), encode(binding.subject)?)),
        };
        let clock = Clock;
        AgentTaskService::new_with_collector_authorization(
            &profile.data_dir,
            &profile.profile_id,
            &clock,
            &guard,
        )?
        .approve_with_live_imported_origins(&identity, &context, &approval)
        .map_err(native_action_error)?;
        Ok(approval)
    }

    pub(crate) fn integrate_with_agent_task_origins(
        &self,
        actor: &WorkspaceActor,
        binding: NativeTaskAction<'_>,
        operation_id: &str,
        approval_id: &str,
    ) -> Result<crate::model::IntegrationEvent> {
        self.workspace.authorize(
            actor,
            binding.repository,
            WorkspaceAction::IntegrateRevision,
        )?;
        let profile = self.agent_task_profile(binding.repository, binding.profile_id)?;
        let context = profile.action_subject(binding.subject)?;
        let request = crate::model::IntegrateRequest {
            operation_id: operation_id.into(),
            target: context.target.clone(),
            expected_target: context.base.as_ref().map(|base| crate::model::TargetState {
                revision: base.revision.clone(),
                version: base.target_version,
            }),
            revision: context.revision.clone(),
            context_digest: context.digest()?,
            approval_id: approval_id.into(),
            change: Some(context.change.clone()),
            change_version: Some(context.change_version),
            policy_epoch: context.policy_epoch,
        };
        let retry = crate::authority::Authority::open_read_only(&profile.data_dir)?
            .integration_retry(&request)
            .map_err(native_action_error)?
            .is_some();
        if !retry {
            if actor.generation() != binding.expected_generation {
                return Err(HubError::Invalid("workspace_generation_conflict".into()));
            }
            profile.validate_subject(binding.subject)?;
        }
        let identity = actor.authorization_identity();
        let guard = WorkspaceGuard {
            service: self,
            actor,
            repository: binding.repository,
            native_repository: profile.native_repository.clone(),
            identity: identity.clone(),
            audited: Some((binding.request_id.into(), encode(binding.subject)?)),
        };
        let clock = Clock;
        AgentTaskService::new_with_collector_authorization(
            &profile.data_dir,
            &profile.profile_id,
            &clock,
            &guard,
        )?
        .integrate_with_live_imported_origins(
            &identity,
            &context,
            &crate::store::Engine::open(&profile.data_dir)?,
            &request,
        )
        .map_err(native_action_error)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpQuery {
    repository: String,
    profile: String,
}

const MCP_TOOLS: [(&str, &str); 14] = [
    ("agentscm_task_requirements", "requirements"),
    ("agentscm_task_next", "next"),
    ("agentscm_task_instructions", "instructions"),
    ("agentscm_task_reference", "reference"),
    ("agentscm_task_claim", "claim"),
    ("agentscm_task_collector_claim", "collector_claim"),
    ("agentscm_task_heartbeat", "heartbeat"),
    ("agentscm_task_result", "result"),
    ("agentscm_task_status", "status"),
    ("agentscm_task_cancel", "cancel"),
    ("agentscm_task_reconcile", "reconcile"),
    ("agentscm_task_recovery", "recovery"),
    ("agentscm_task_collector_descriptor", "collector_descriptor"),
    ("agentscm_task_collector_finalize", "collector_finalize"),
];
const MCP_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

pub fn routes() -> Router<HubState> {
    Router::new().route(
        "/v1/hub/workspace/mcp",
        post(mcp).layer(DefaultBodyLimit::max(MAX_AGENT_TASK_REQUEST_BYTES)),
    )
}

fn mcp_failure(id: &Value, code: i32, message: &str) -> Response {
    axum::Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
        .into_response()
}

fn mcp_tools() -> Value {
    let id = || {
        json!({
            "type":"string",
            "minLength":1,
            "maxLength":crate::skill_workflow::task_contracts::MAX_ID_BYTES,
            "pattern":"^[A-Za-z0-9._:/-]+$"
        })
    };
    let bounded_text = || {
        json!({
            "type":"string",
            "minLength":1,
            "maxLength":crate::skill_workflow::task_contracts::MAX_TEXT_BYTES,
            "pattern":"^[^\\u0000]+$"
        })
    };
    let digest = || json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    let subject = || {
        json!({
            "type":"object",
            "properties":{
                "change_id":id(),"change_version":{"type":"integer","minimum":1},
                "context_digest":digest(),"revision":digest(),"requirement_digest":digest()
            },
            "required":["change_id","change_version","context_digest","revision","requirement_digest"],
            "additionalProperties":false
        })
    };
    let executor = || {
        json!({
            "type":"object",
            "properties":{
                "registration_id":id(),"executor_id":id(),
                "registration_epoch":{"type":"integer","minimum":1},
                "verification_key_id":id()
            },
            "required":["registration_id","executor_id","registration_epoch","verification_key_id"],
            "additionalProperties":false
        })
    };
    let claim = || {
        json!({
            "type":"object",
            "properties":{
                "version":{"const":AGENT_TASK_CONTRACT_VERSION},"request_id":id(),
                "execution_operation_id":id(),"task_id":id(),"task_digest":digest(),
                "attempt_id":id(),"executor":executor(),
                "requested_lease_ms":{"type":"integer","minimum":1,"maximum":crate::skill_workflow::task_contracts::MAX_LEASE_MS}
            },
            "required":["version","request_id","execution_operation_id","task_id","task_digest","attempt_id","executor","requested_lease_ms"],
            "additionalProperties":false
        })
    };
    let status = || {
        json!({
            "type":"object",
            "properties":{
                "version":{"const":AGENT_TASK_CONTRACT_VERSION},"task_id":id(),
                "task_digest":digest(),"attempt_id":id(),"attempt_digest":digest(),
                "fence":{"type":"integer","minimum":1},"executor":executor()
            },
            "required":["version","task_id","task_digest","attempt_id","attempt_digest","fence","executor"],
            "additionalProperties":false
        })
    };
    let mutation = |operation: &str| {
        let action = |kind: &str| {
            if kind == "cancel" {
                json!({"type":"object","properties":{"kind":{"const":kind},"reason":bounded_text()},"required":["kind","reason"],"additionalProperties":false})
            } else {
                json!({"type":"object","properties":{"kind":{"const":kind}},"required":["kind"],"additionalProperties":false})
            }
        };
        let action = if operation == "recovery" {
            json!({"oneOf":[action("heartbeat"),action("cancel"),action("reconcile")]})
        } else {
            action(operation)
        };
        json!({
            "type":"object",
            "properties":{
                "version":{"const":AGENT_TASK_CONTRACT_VERSION},"request_id":id(),
                "execution_operation_id":id(),"task_id":id(),"task_digest":digest(),
                "attempt_id":id(),"attempt_digest":digest(),
                "fence":{"type":"integer","minimum":1},"executor":executor(),
                "action":action
            },
            "required":["version","request_id","execution_operation_id","task_id","task_digest","attempt_id","attempt_digest","fence","executor","action"],
            "additionalProperties":false
        })
    };
    let assertion = || {
        let criterion = json!({
            "type":"object",
            "properties":{
                "criterion":id(),
                "outcome":{"enum":["passed","failed","incomplete","not_claimed"]},
                "findings":{"type":"array","items":bounded_text(),"maxItems":16},
                "evidence_digests":{"type":"array","items":digest(),"maxItems":16}
            },
            "required":["criterion","outcome","findings","evidence_digests"],
            "additionalProperties":false
        });
        let provenance = json!({
            "type":"object",
            "properties":{
                "registration_id":id(),
                "registration_epoch":{"type":"integer","minimum":1},
                "verification_key_id":id(),
                "observation_digest":digest(),
                "signature_algorithm":{"const":"ed25519"},
                "signature_hex":{"type":"string","pattern":"^[0-9a-f]{128}$"}
            },
            "required":["registration_id","registration_epoch","verification_key_id","observation_digest","signature_algorithm","signature_hex"],
            "additionalProperties":false
        });
        let branch = |trust_class: &str, executor_provenance: Value| {
            let mut schema = json!({
                "type":"object",
                "properties":{
                    "version":{"const":AGENT_TASK_CONTRACT_VERSION},
                    "task_id":id(),"task_digest":digest(),"attempt_id":id(),
                    "attempt_digest":digest(),"request_id":id(),"execution_operation_id":id(),
                    "executor":executor(),"fence":{"type":"integer","minimum":1},
                    "trust_class":{"const":trust_class},
                    "outcome":{"enum":["passed","failed","incomplete","cancelled","uncertain"]},
                    "criteria":{"type":"array","items":criterion,"minItems":1,"maxItems":64},
                    "executor_provenance":executor_provenance
                },
                "required":["version","task_id","task_digest","attempt_id","attempt_digest","request_id","execution_operation_id","executor","fence","trust_class","outcome","criteria"],
                "additionalProperties":false
            });
            if trust_class == "registered_executor_observation" {
                schema["required"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("executor_provenance"));
            }
            schema
        };
        json!({"oneOf":[
            branch("agent_assertion", json!({"type":"null"})),
            branch("registered_executor_observation", provenance)
        ]})
    };
    let schema = |operation: &str| {
        let mut properties = serde_json::Map::new();
        properties.insert("subject".into(), subject());
        let required = match operation {
            "requirements" | "next" | "collector_finalize" => vec!["subject"],
            "instructions" => {
                properties.insert("task_id".into(), id());
                properties.insert("task_digest".into(), digest());
                properties.insert("supported_schema_versions".into(), json!({"type":"array","minItems":1,"maxItems":crate::skill_workflow::task_contracts::MAX_SUPPORTED_SCHEMA_VERSIONS,"uniqueItems":true,"items":{"type":"integer","minimum":1,"maximum":u32::MAX}}));
                vec![
                    "subject",
                    "task_id",
                    "task_digest",
                    "supported_schema_versions",
                ]
            }
            "reference" => {
                properties.insert("task_id".into(), id());
                properties.insert("task_digest".into(), digest());
                properties.insert("reference_name".into(), id());
                properties.insert("expected_sha256".into(), digest());
                properties.insert(
                    "expected_schema_version".into(),
                    json!({"const":AGENT_TASK_CONTRACT_VERSION}),
                );
                vec![
                    "subject",
                    "task_id",
                    "task_digest",
                    "reference_name",
                    "expected_sha256",
                    "expected_schema_version",
                ]
            }
            "claim" => {
                properties.insert("claim".into(), claim());
                vec!["subject", "claim"]
            }
            "collector_claim" => {
                properties.insert("claim".into(), claim());
                properties.insert("collector_profile_id".into(), id());
                vec!["subject", "claim", "collector_profile_id"]
            }
            "status" | "collector_descriptor" => {
                properties.insert("status".into(), status());
                vec!["subject", "status"]
            }
            "heartbeat" | "cancel" | "reconcile" | "recovery" => {
                properties.insert("mutation".into(), mutation(operation));
                vec!["subject", "mutation"]
            }
            "result" => {
                properties.insert("assertion".into(), assertion());
                vec!["subject", "assertion"]
            }
            _ => vec!["subject"],
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    };
    Value::Array(
        MCP_TOOLS
            .iter()
            .map(|(name, operation)| {
                json!({
                    "name": name,
                    "description": format!("Agent-owned workflow {operation}; capability {} schema {}. The authenticated workspace credential supplies the actor.", AGENT_TASK_CAPABILITY, AGENT_TASK_CONTRACT_VERSION),
                    "inputSchema": schema(operation)
                })
            })
            .collect(),
    )
}

async fn mcp(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Query(query): Query<McpQuery>,
    body: Bytes,
) -> std::result::Result<Response, ApiError> {
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Ok(mcp_failure(&Value::Null, -32700, "parse error"));
    };
    let Some(message) = message.as_object() else {
        return Ok(mcp_failure(&Value::Null, -32600, "invalid request"));
    };
    let Some(id) = message.get("id") else {
        return Ok(axum::http::StatusCode::ACCEPTED.into_response());
    };
    if !(id.is_string() || id.is_number()) {
        return Ok(mcp_failure(&Value::Null, -32600, "invalid request"));
    }
    let result = match message.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            let version = message
                .get("params")
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str)
                .filter(|version| MCP_VERSIONS.contains(version))
                .unwrap_or(MCP_VERSIONS[1]);
            Ok(json!({
                "protocolVersion":version,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"agentscm-workspace-tasks","version":env!("CARGO_PKG_VERSION")},
                "instructions":format!("Use only server-derived {} schema {} task bindings. Agent assertions are nonpassing; collector upload uses the bounded authenticated HTTP upload endpoint.", AGENT_TASK_CAPABILITY, AGENT_TASK_CONTRACT_VERSION)
            }))
        }
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(json!({"tools":mcp_tools()})),
        Some("tools/call") => {
            let params = message.get("params").and_then(Value::as_object);
            let name = params
                .and_then(|params| params.get("name"))
                .and_then(Value::as_str);
            let operation = name.and_then(|name| {
                MCP_TOOLS
                    .iter()
                    .find_map(|(candidate, operation)| (*candidate == name).then_some(*operation))
            });
            let Some(operation) = operation else {
                return Ok(mcp_failure(id, -32602, "invalid params"));
            };
            let mut arguments = params
                .and_then(|params| params.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let Some(arguments) = arguments.as_object_mut() else {
                return Ok(mcp_failure(id, -32602, "invalid params"));
            };
            arguments.insert("operation".into(), Value::String(operation.into()));
            let Ok(request) =
                serde_json::from_value::<AgentTaskRequest>(Value::Object(arguments.clone()))
            else {
                return Ok(mcp_failure(id, -32602, "invalid params"));
            };
            let execution = state.workspace_execution.clone().ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "execution_not_configured",
                )
            })?;
            let repository = query.repository.clone();
            let profile = query.profile.clone();
            match blocking(&state, move || {
                execution.agent_task_call(&actor, &repository, &profile, request)
            })
            .await
            {
                Ok(envelope) => serde_json::to_string(&envelope)
                    .map(|text| json!({"content":[{"type":"text","text":text}],"isError":false}))
                    .map_err(|_| HubError::Unavailable("workspace task encoding".into())),
                Err(error) => Err(error),
            }
        }
        _ => return Ok(mcp_failure(id, -32601, "method not found")),
    };
    Ok(match result {
        Ok(result) => axum::Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response(),
        Err(HubError::Busy | HubError::Unavailable(_)) => mcp_failure(
            id,
            -32603,
            "temporarily unavailable; inspect task status or recovery before retrying",
        ),
        Err(HubError::Invalid(code)) if code == "workspace_native_conflict" => axum::Json(
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"AgentSCM task request conflicted with current state; inspect task status or recovery"}],"isError":true}}),
        )
        .into_response(),
        Err(HubError::Invalid(_)) => axum::Json(
            json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"AgentSCM task request was invalid or not found"}],"isError":true}}),
        )
        .into_response(),
        Err(HubError::Unauthorized) => {
            mcp_failure(id, -32001, "workspace credential was not accepted")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> Value {
        json!({
            "change_id":"change-1",
            "change_version":1,
            "context_digest":"1".repeat(64),
            "revision":"2".repeat(64),
            "requirement_digest":"3".repeat(64)
        })
    }

    fn executor() -> Value {
        json!({
            "registration_id":"registration-1",
            "executor_id":"executor-1",
            "registration_epoch":1,
            "verification_key_id":"key-1"
        })
    }

    fn claim() -> Value {
        json!({
            "version":1,
            "request_id":"claim-1",
            "execution_operation_id":"execute-1",
            "task_id":"task-1",
            "task_digest":"4".repeat(64),
            "attempt_id":"attempt-1",
            "executor":executor(),
            "requested_lease_ms":1_000
        })
    }

    fn status() -> Value {
        json!({
            "version":1,
            "task_id":"task-1",
            "task_digest":"4".repeat(64),
            "attempt_id":"attempt-1",
            "attempt_digest":"5".repeat(64),
            "fence":1,
            "executor":executor()
        })
    }

    fn mutation(kind: &str) -> Value {
        let action = if kind == "cancel" {
            json!({"kind":kind,"reason":"operator requested cancellation"})
        } else {
            json!({"kind":kind})
        };
        let mut value = status();
        let object = value.as_object_mut().unwrap();
        object.insert("request_id".into(), json!(format!("{kind}-1")));
        object.insert("execution_operation_id".into(), json!("execute-1"));
        object.insert("action".into(), action);
        value
    }

    fn assertion() -> Value {
        let mut value = status();
        let object = value.as_object_mut().unwrap();
        object.insert("request_id".into(), json!("result-1"));
        object.insert("execution_operation_id".into(), json!("execute-1"));
        object.insert("trust_class".into(), json!("agent_assertion"));
        object.insert("outcome".into(), json!("incomplete"));
        object.insert(
            "criteria".into(),
            json!([{"criterion":"source-result","outcome":"incomplete","findings":["not observed"],"evidence_digests":[]}]),
        );
        object.insert("executor_provenance".into(), Value::Null);
        value
    }

    fn arguments(operation: &str) -> Value {
        match operation {
            "requirements" | "next" | "collector_finalize" => json!({"subject":subject()}),
            "instructions" => {
                json!({"subject":subject(),"task_id":"task-1","task_digest":"4".repeat(64),"supported_schema_versions":[1]})
            }
            "reference" => {
                json!({"subject":subject(),"task_id":"task-1","task_digest":"4".repeat(64),"reference_name":"entrypoint","expected_sha256":"6".repeat(64),"expected_schema_version":1})
            }
            "claim" => json!({"subject":subject(),"claim":claim()}),
            "collector_claim" => {
                json!({"subject":subject(),"claim":claim(),"collector_profile_id":"collector-1"})
            }
            "status" | "collector_descriptor" => {
                json!({"subject":subject(),"status":status()})
            }
            "heartbeat" => json!({"subject":subject(),"mutation":mutation("heartbeat")}),
            "cancel" => json!({"subject":subject(),"mutation":mutation("cancel")}),
            "reconcile" => json!({"subject":subject(),"mutation":mutation("reconcile")}),
            "recovery" => json!({"subject":subject(),"mutation":mutation("heartbeat")}),
            "result" => json!({"subject":subject(),"assertion":assertion()}),
            _ => unreachable!(),
        }
    }

    #[test]
    fn every_workspace_mcp_tool_has_a_strict_discoverable_input() {
        let tools = mcp_tools().as_array().unwrap().clone();
        assert_eq!(tools.len(), MCP_TOOLS.len());
        for ((name, operation), tool) in MCP_TOOLS.iter().zip(&tools) {
            assert_eq!(tool["name"], *name);
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            let properties = schema["properties"].as_object().unwrap();
            let required = schema["required"].as_array().unwrap();
            let arguments = arguments(operation);
            for field in required {
                let field = field.as_str().unwrap();
                assert!(
                    properties.contains_key(field),
                    "{name} omits schema for {field}"
                );
                assert!(
                    arguments.get(field).is_some(),
                    "{name} example omits {field}"
                );
            }
            let mut request = arguments;
            request["operation"] = json!(operation);
            serde_json::from_value::<AgentTaskRequest>(request)
                .unwrap_or_else(|error| panic!("{name} example must decode: {error}"));
        }

        for ((_, operation), tool) in MCP_TOOLS.iter().zip(&tools) {
            let properties = &tool["inputSchema"]["properties"];
            match *operation {
                "heartbeat" | "cancel" | "reconcile" | "recovery" => {
                    let action = &properties["mutation"]["properties"]["action"];
                    let variants = if *operation == "recovery" {
                        let variants = action["oneOf"].as_array().unwrap();
                        assert_eq!(variants.len(), 3);
                        variants.iter().collect::<Vec<_>>()
                    } else {
                        assert!(action.get("oneOf").is_none());
                        vec![action]
                    };
                    let expected = if *operation == "recovery" {
                        vec!["heartbeat", "cancel", "reconcile"]
                    } else {
                        vec![*operation]
                    };
                    for (variant, kind) in variants.iter().zip(expected) {
                        assert_eq!(variant["type"], "object");
                        assert_eq!(variant["additionalProperties"], false);
                        assert_eq!(variant["properties"]["kind"], json!({"const":kind}));
                        if kind == "cancel" {
                            assert_eq!(variant["required"], json!(["kind", "reason"]));
                            assert_eq!(
                                variant["properties"]["reason"]["maxLength"],
                                crate::skill_workflow::task_contracts::MAX_TEXT_BYTES
                            );
                        } else {
                            assert_eq!(variant["required"], json!(["kind"]));
                            assert_eq!(variant["properties"].as_object().unwrap().len(), 1);
                        }
                    }
                }
                "result" => {
                    let branches = properties["assertion"]["oneOf"].as_array().unwrap();
                    assert_eq!(branches.len(), 2);
                    for trust in ["agent_assertion", "registered_executor_observation"] {
                        let branch = branches
                            .iter()
                            .find(|branch| branch["properties"]["trust_class"]["const"] == trust)
                            .unwrap();
                        assert_eq!(branch["type"], "object");
                        assert_eq!(branch["additionalProperties"], false);
                        assert!(branch.get("allOf").is_none());
                        let fields = branch["properties"].as_object().unwrap();
                        let required = branch["required"].as_array().unwrap();
                        for field in assertion().as_object().unwrap().keys() {
                            assert!(fields.contains_key(field));
                            assert_eq!(
                                required.contains(&json!(field)),
                                field != "executor_provenance"
                                    || trust == "registered_executor_observation"
                            );
                        }
                        assert_eq!(fields["criteria"]["minItems"], 1);
                        assert_eq!(fields["criteria"]["maxItems"], 64);
                        assert_eq!(fields["criteria"]["items"]["additionalProperties"], false);
                        let provenance = &fields["executor_provenance"];
                        if trust == "agent_assertion" {
                            assert_eq!(provenance, &json!({"type":"null"}));
                        } else {
                            assert_eq!(provenance["type"], "object");
                            assert_eq!(provenance["additionalProperties"], false);
                            assert!(provenance.get("anyOf").is_none());
                            assert_eq!(
                                provenance["required"],
                                json!([
                                    "registration_id",
                                    "registration_epoch",
                                    "verification_key_id",
                                    "observation_digest",
                                    "signature_algorithm",
                                    "signature_hex"
                                ])
                            );
                        }
                    }
                }
                "instructions" => {
                    let versions = &properties["supported_schema_versions"];
                    assert_eq!(versions["minItems"], 1);
                    assert_eq!(
                        versions["maxItems"],
                        crate::skill_workflow::task_contracts::MAX_SUPPORTED_SCHEMA_VERSIONS
                    );
                    assert_eq!(versions["uniqueItems"], true);
                    assert_eq!(
                        versions["items"],
                        json!({"type":"integer","minimum":1,"maximum":u32::MAX})
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn strict_nested_workspace_mcp_inputs_reject_missing_and_unknown_fields() {
        let mut missing_kind = arguments("heartbeat");
        missing_kind["operation"] = json!("heartbeat");
        missing_kind["mutation"]["action"] = json!({});
        assert!(serde_json::from_value::<AgentTaskRequest>(missing_kind).is_err());

        let mut unknown_action = arguments("cancel");
        unknown_action["operation"] = json!("cancel");
        unknown_action["mutation"]["action"]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<AgentTaskRequest>(unknown_action).is_err());

        let mut missing_criterion = arguments("result");
        missing_criterion["operation"] = json!("result");
        missing_criterion["assertion"]["criteria"][0]
            .as_object_mut()
            .unwrap()
            .remove("outcome");
        assert!(serde_json::from_value::<AgentTaskRequest>(missing_criterion).is_err());

        let mut unknown_provenance = arguments("result");
        unknown_provenance["operation"] = json!("result");
        unknown_provenance["assertion"]["executor_provenance"] = json!({
            "registration_id":"registration-1",
            "registration_epoch":1,
            "verification_key_id":"key-1",
            "observation_digest":"6".repeat(64),
            "signature_algorithm":"ed25519",
            "signature_hex":"7".repeat(128),
            "unexpected":true
        });
        assert!(serde_json::from_value::<AgentTaskRequest>(unknown_provenance).is_err());
    }

    #[test]
    fn workspace_mcp_assertion_and_version_examples_reach_contract_validation() {
        let agent = AuthenticatedAgentIdentity {
            owner_id: "owner-1".into(),
            agent_id: "agent-1".into(),
        };
        for omit_provenance in [false, true] {
            let mut value = assertion();
            if omit_provenance {
                value.as_object_mut().unwrap().remove("executor_provenance");
            }
            let input: AssertionInput = serde_json::from_value(value.clone()).unwrap();
            assert!(input.bind(agent.clone()).canonical_bytes().is_ok());
            value["trust_class"] = json!("registered_executor_observation");
            let input: AssertionInput = serde_json::from_value(value).unwrap();
            assert!(
                matches!(input.bind(agent.clone()).canonical_bytes(), Err(crate::Error::Invalid(message)) if message == "assertion trust class and provenance disagree")
            );
        }
        let mut value = assertion();
        value["trust_class"] = json!("registered_executor_observation");
        let input: AssertionInput = serde_json::from_value(value.clone()).unwrap();
        let digest = input.bind(agent.clone()).observation_digest().unwrap();
        value["executor_provenance"] = json!({
            "registration_id":"registration-1", "registration_epoch":1,
            "verification_key_id":"key-1", "observation_digest":digest,
            "signature_algorithm":"ed25519", "signature_hex":"7".repeat(128)
        });
        let input: AssertionInput = serde_json::from_value(value.clone()).unwrap();
        // Contract shape and digest binding, not signature authentication.
        assert!(input.bind(agent.clone()).canonical_bytes().is_ok());
        value["trust_class"] = json!("agent_assertion");
        let input: AssertionInput = serde_json::from_value(value).unwrap();
        assert!(
            matches!(input.bind(agent).canonical_bytes(), Err(crate::Error::Invalid(message)) if message == "assertion trust class and provenance disagree")
        );

        let maximum = crate::skill_workflow::task_contracts::MAX_SUPPORTED_SCHEMA_VERSIONS as u32;
        for (versions, valid) in [
            (vec![2], true),
            ((2..=maximum + 1).collect(), true),
            (vec![], false),
            (vec![0], false),
            (vec![2, 2], false),
            ((2..=maximum + 2).collect(), false),
        ] {
            let response = InstructionEntrypointResponse::SchemaMismatch {
                capability: AGENT_TASK_CAPABILITY.into(),
                required_schema_version: 1,
                supported_schema_versions: versions,
            };
            assert_eq!(
                InstructionEntrypointResponse::from_slice(&serde_json::to_vec(&response).unwrap())
                    .is_ok(),
                valid
            );
        }
    }

    #[test]
    fn workspace_mcp_cross_action_inputs_are_rejected_by_core_handlers() {
        struct RejectAuthorization;
        impl TaskAuthorizationGuard for RejectAuthorization {
            fn authorize(&self, _: &AuthenticatedAgentIdentity, _: &str) -> crate::Result<()> {
                panic!("cross-action requests must be rejected before authorization");
            }
        }
        let root = tempfile::tempdir().unwrap();
        let service =
            AgentTaskService::new(root.path(), "profile-1", &Clock, &RejectAuthorization).unwrap();
        let context = crate::context::ReviewContext {
            version: crate::context::REVIEW_CONTEXT_VERSION,
            change: crate::context::ChangeId {
                repository: "repository-1".into(),
                id: "change-1".into(),
            },
            change_version: 1,
            revision: "2".repeat(64),
            base: None,
            target: "main".into(),
            policy_epoch: 1,
            intent: "Synthetic cross-action rejection".into(),
            acceptance_criteria: vec![],
            scope: vec![],
            exclusions: vec![],
            omissions: vec![],
            dependencies: vec![],
            recipe: crate::context::InputIdentity::Unknown {},
            toolchain: crate::context::InputIdentity::Unknown {},
            environment: crate::context::InputIdentity::Unknown {},
            required_evidence: vec![],
            required_capabilities: vec![],
        };
        for (_, operation) in MCP_TOOLS {
            if !matches!(operation, "heartbeat" | "cancel" | "reconcile") {
                continue;
            }
            for kind in ["heartbeat", "cancel", "reconcile"] {
                if kind == operation {
                    continue;
                }
                let mut value = arguments(operation);
                value["operation"] = json!(operation);
                value["mutation"] = mutation(kind);
                let input = match serde_json::from_value::<AgentTaskRequest>(value).unwrap() {
                    AgentTaskRequest::Heartbeat { mutation, .. }
                    | AgentTaskRequest::Cancel { mutation, .. }
                    | AgentTaskRequest::Reconcile { mutation, .. } => mutation,
                    _ => unreachable!(),
                };
                let request = input.bind(AuthenticatedAgentIdentity {
                    owner_id: "owner-1".into(),
                    agent_id: "agent-1".into(),
                });
                let result = match operation {
                    "heartbeat" => service.heartbeat(&context, &request),
                    "cancel" => service.cancel_attempt(&context, &request),
                    "reconcile" => service.reconcile_attempt(&context, &request),
                    _ => unreachable!(),
                };
                assert!(
                    matches!(result, Err(crate::Error::Invalid(_))),
                    "{operation} accepted {kind}"
                );
            }
        }
    }

    #[test]
    fn workspace_guard_keeps_authorization_outages_uncertain() {
        assert!(matches!(
            WorkspaceGuard::guard_error(HubError::Busy),
            crate::Error::Io(_)
        ));
        assert!(matches!(
            WorkspaceGuard::guard_error(HubError::Unavailable("store".into())),
            crate::Error::Io(_)
        ));
        assert!(matches!(
            WorkspaceGuard::guard_error(HubError::Unauthorized),
            crate::Error::Conflict(_)
        ));
    }
}
