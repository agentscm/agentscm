use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::HubError;

const TEXT_LIMIT: usize = 800;
const DIAGNOSTIC_LIMIT: usize = 500;
static ENTROPY_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    Retry,
    Wait,
    SignIn,
    RenewCredential,
    Reconnect,
    ProvideField,
    StartSession,
    InspectWork,
    AskOwner,
    UpdateClient,
    ContactOperator,
    None,
}

impl RecoveryAction {
    pub const ALL: &'static [Self] = &[
        Self::Retry,
        Self::Wait,
        Self::SignIn,
        Self::RenewCredential,
        Self::Reconnect,
        Self::ProvideField,
        Self::StartSession,
        Self::InspectWork,
        Self::AskOwner,
        Self::UpdateClient,
        Self::ContactOperator,
        Self::None,
    ];

    pub fn recovery(self) -> Recovery {
        Recovery {
            action: self,
            command: match self {
                Self::SignIn => Some("agentscm hub login".into()),
                Self::RenewCredential => Some("agentscm client renew".into()),
                Self::Reconnect => Some("agentscm client doctor".into()),
                _ => None,
            },
        }
    }

    pub fn sentence(self) -> &'static str {
        match self {
            Self::Retry | Self::Wait => "Retry later with the same idempotency key or operation id",
            Self::SignIn => "Sign in again with agentscm hub login",
            Self::RenewCredential => "Renew the credential with agentscm client renew",
            Self::Reconnect => "Check the Hub connection with agentscm client doctor",
            Self::ProvideField => "Correct the named request fields and send a new request",
            Self::StartSession => "Pass your own native session ID on every AgentSCM call",
            Self::InspectWork => "Inspect current Work and coordinate before changing the request",
            Self::AskOwner => "Ask the repository owner or Hub administrator for access",
            Self::UpdateClient => "Update the AgentSCM client",
            Self::ContactOperator => "Ask the Hub operator to inspect the error reference",
            Self::None => "No automatic recovery is available",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Recovery {
    pub action: RecoveryAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

macro_rules! error_codes {
    ($($variant:ident => ($code:literal, $status:literal, $retry:literal, $action:ident, $message:literal)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum ErrorCode { $($variant),+ }

        impl ErrorCode {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $code),+ }
            }
            pub fn parse(code: &str) -> Option<Self> {
                match code { $($code => Some(Self::$variant)),+, _ => None }
            }
            pub fn status(self) -> u16 {
                match self { $(Self::$variant => $status),+ }
            }
            pub fn retryable(self) -> bool {
                match self { $(Self::$variant => $retry),+ }
            }
            pub fn action(self) -> RecoveryAction {
                match self { $(Self::$variant => RecoveryAction::$action),+ }
            }
            pub fn message(self) -> &'static str {
                match self { $(Self::$variant => $message),+ }
            }
        }
    };
}

error_codes! {
    InvalidRequest => ("invalid_request", 400, false, ProvideField, "The Hub rejected an invalid request; check the required fields and send a corrected request."),
    NotFound => ("not_found", 404, false, None, "The resource was not found; check the requested identifier."),
    ReadOnly => ("read_only", 403, false, AskOwner, "This credential is read-only; use an authorized agent credential for changes."),
    Forbidden => ("forbidden", 403, false, AskOwner, "This operation is not allowed; ask the owner or Hub administrator for access."),
    Gone => ("gone", 410, false, InspectWork, "The retained resource is no longer available; refresh the current state."),
    Unauthorized => ("unauthorized", 401, false, SignIn, "The credential was not accepted; sign in again."),
    WrongRepository => ("wrong_repository", 403, false, Reconnect, "This token is for another repository; connect this checkout again."),
    Busy => ("busy", 503, true, Retry, "The Hub is busy; retry later with the same idempotency key or operation id."),
    Unavailable => ("unavailable", 503, true, Retry, "The Hub is unavailable; retry later with the same idempotency key or operation id."),
    WorkStorageCapacity => ("work_storage_capacity", 503, false, ContactOperator, "Repository work storage capacity was reached; ask the Hub operator to compact retained Work."),
    WorkScopeOverlap => ("work_scope_overlap", 409, false, InspectWork, "Declared scope overlaps an existing assignment; split the scope, join or arrange a handover."),
    WorkCoordinationRequired => ("work_coordination_required", 409, false, InspectWork, "Scope was not changed; inspect the durable coordination request and coordinate with its leads."),
    WorkOverlapIncomplete => ("work_overlap_incomplete", 503, true, Retry, "Overlap coverage is incomplete (more than 100 items to compare); no scope was claimed, so retry the same request before relying on scope coverage."),
    WorkPreflightRequired => ("work_preflight_required", 409, false, InspectWork, "Source preflight is changed, incomplete or conflicting; inspect it before claiming Work."),
    WorkCheckpointSequenceConflict => ("work_checkpoint_sequence_conflict", 409, false, InspectWork, "The Work sequence changed and the checkpoint was not committed; inspect current Work before preparing a new request."),
    SessionRequired => ("session_required", 400, false, StartSession, "A session is required; pass your own native session ID on every AgentSCM call."),
    LeadSessionMismatch => ("lead_session_mismatch", 400, false, InspectWork, "The current session does not lead this Work; inspect Work before joining or taking over."),
    LeaseLapsed => ("lease_lapsed", 400, false, InspectWork, "The Work reservation lapsed; request fresh admission before editing."),
    NotYourAgent => ("not_your_agent", 403, false, AskOwner, "That agent is not yours, or it is no longer connected"),
    NotYourWork => ("not_your_work", 403, false, AskOwner, "Only the Work's owner or a Hub administrator can change it"),
    LoginFailed => ("login_failed", 400, false, SignIn, "Sign-in did not finish; start signing in again."),
    DeviceFlowUnavailable => ("device_flow_unavailable", 409, false, SignIn, "Device sign-in is unavailable; use browser sign-in instead."),
    Csrf => ("csrf", 403, false, SignIn, "The sign-in request could not be verified; start signing in again."),
    InsufficientRole => ("insufficient_role", 403, false, AskOwner, "Your repository role is insufficient; ask a repository owner for more access."),
    LoginDenied => ("login_denied", 403, false, SignIn, "Sign-in was denied; start signing in again."),
    NotAllowed => ("not_allowed", 403, false, AskOwner, "This Hub only lets in people its admins have allowed; ask a Hub admin to add you"),
    NotConnected => ("not_connected", 403, false, AskOwner, "The repository is not connected to this Hub; ask a Hub admin to connect it."),
    UserDisabled => ("user_disabled", 403, false, AskOwner, "Your account is turned off on this Hub; ask a Hub admin to turn it back on"),
    TooManyLogins => ("too_many_logins", 429, true, Wait, "Too many sign-in requests were made; wait before trying again."),
    DeviceRevoked => ("device_revoked", 403, false, SignIn, "This computer is no longer signed in; sign in again."),
    AppNotAllowed => ("app_not_allowed", 403, false, None, "A connected app only reads, mints read-only tokens and answers requests for its person."),
    AppReadOnly => ("app_read_only", 403, false, ProvideField, "A connected app mints read-only tokens only; send read_only: true."),
    // The OAuth token endpoint's own codes (RFC 6749 section 5.2).
    InvalidClient => ("invalid_client", 400, false, ContactOperator, "The app's client ID or secret was not accepted; check what the Hub operator registered for it."),
    InvalidGrant => ("invalid_grant", 400, false, SignIn, "The authorization code is invalid, expired or already used; connect the app again."),
    PromiseConflict => ("promise_conflict", 409, false, InspectWork, "The promise changed; read its current version before sending a new request."),
    TooManyPublications => ("too_many_publications", 429, false, ContactOperator, "The publication limit was reached; ask the Hub operator to inspect retained publications."),
    AlreadyConnected => ("already_connected", 409, false, None, "This repository is already connected; use the existing connection."),
    WaiterPresent => ("waiter_present", 409, true, Wait, "Another waiter already waits for this session; ask again later."),
    NotConfigured => ("not_configured", 404, false, ContactOperator, "The requested integration is not configured; ask the Hub operator to configure it."),
    WorkNotFound => ("work_not_found", 404, false, InspectWork, "Work was not found; refresh the current Work list."),
    GenerationConflict => ("generation_conflict", 409, false, InspectWork, "The generation changed; read current state before sending a new request."),
    IdempotencyConflict => ("idempotency_conflict", 409, false, InspectWork, "This operation id already identifies a different request; inspect its receipt."),
    LastAdministrator => ("last_administrator", 409, false, AskOwner, "The last administrator cannot be removed; appoint another administrator first."),
    MemberLimit => ("member_limit", 409, false, ContactOperator, "The member limit was reached; ask the Hub operator to inspect membership."),
    NativeSubjectConflict => ("native_subject_conflict", 409, false, InspectWork, "The native subject changed; inspect its current identity before continuing."),
    CsrfRequired => ("csrf_required", 403, false, SignIn, "This request needs a current browser grant; sign in again."),
    ExecutionNotConfigured => ("execution_not_configured", 503, false, ContactOperator, "Execution is not configured; ask the Hub operator to configure it."),
    ExpectedJson => ("expected_json", 415, false, ProvideField, "The request must contain JSON; correct its content type and body."),
    ExpectedSourceBundle => ("expected_source_bundle", 415, false, ProvideField, "The request must contain a source bundle; correct its content type and body."),
    SourceBodyTooLarge => ("source_body_too_large", 413, false, ProvideField, "The source body exceeds the limit; send a smaller bundle."),
    SourceBodyInvalid => ("source_body_invalid", 413, false, ProvideField, "The source body is invalid; send a valid bounded bundle."),
    SourceBodyTimeout => ("source_body_timeout", 408, true, Retry, "The source upload timed out; retry with the same idempotency key."),
    SourceResponseInvalid => ("source_response_invalid", 500, false, ContactOperator, "The source response could not be produced; ask the Hub operator to inspect the reference."),
    IntegrityFailure => ("integrity_failure", 424, false, ContactOperator, "Stored content failed integrity verification; ask the Hub operator to inspect the reference."),
    RetryUpload => ("retry_upload", 503, true, Retry, "The upload is not ready; retry with the same idempotency key."),
    NoRepositoryRole => ("no_repository_role", 403, false, AskOwner, "Your repository role is insufficient; ask a repository owner for more access."),
    AccessCheckUnavailable => ("access_check_unavailable", 503, true, Wait, "The Hub cannot check repository access; ask a Hub admin to check the connection."),
    PermissionUnavailable => ("permission_unavailable", 503, true, Wait, "The forge did not answer the access check; nothing changed, so try again in a minute."),
    HubUnreachable => ("hub_unreachable", 503, true, Reconnect, "The Hub is unreachable; check the connection before relying on coordination."),
    InvalidResponse => ("invalid_response", 502, false, UpdateClient, "The Hub reply could not be read; check client and Hub compatibility."),
    SignInRequired => ("sign_in_required", 401, false, SignIn, "This computer isn't signed in to this Hub any more; choose Sign In in the AgentSCM menu bar app or run agentscm hub login"),
    InvalidOrChanged => ("invalid_or_changed", 400, false, InspectWork, "The request is invalid or changed; refresh current Work before sending a new request."),
    NotAdopted => ("not_adopted", 409, false, AskOwner, "The skill is not adopted; ask the repository owner to review its adoption."),
    RunFull => ("run_full", 409, false, ContactOperator, "The run evidence limit was reached; ask the operator to inspect retained evidence."),
    UnknownGate => ("unknown_gate", 409, false, ProvideField, "The gate does not accept this evidence; check the adopted skill definition."),
    StaleHead => ("stale_head", 409, false, InspectWork, "The run head changed; read current evidence before recording another result."),
    NotIndependent => ("not_independent", 409, false, AskOwner, "This agent token and session recorded author evidence; use an independent reviewer."),
    ForgeAuthoritative => ("forge_authoritative", 409, false, AskOwner, "Adoption comes from the forge default branch; commit and merge the skill lock there."),
    MethodNotAllowed => ("method_not_allowed", 405, false, ProvideField, "This route does not accept that HTTP method; use its documented method."),
    RequestTooLarge => ("request_too_large", 413, false, ProvideField, "The request exceeds the body limit; send a smaller request."),
    RequestTimeout => ("request_timeout", 408, true, Retry, "The request timed out; retry with the same idempotency key or operation id."),
    RateLimited => ("rate_limited", 429, true, Wait, "The request rate limit was reached; wait and retry with the same idempotency key or operation id."),
    ContextTooLarge => ("context_too_large", 503, false, ContactOperator, "The retained context exceeds the response limit; ask the operator to inspect its coverage."),
    InvalidParams => ("invalid_params", 400, false, ProvideField, "The parameters are invalid; correct the required request fields."),
    EncodingFailed => ("encoding_failed", 500, false, ContactOperator, "The response could not be encoded; ask the operator to inspect the reference."),
    UnsupportedProvider => ("unsupported_provider", 409, false, UpdateClient, "This forge provider is unsupported; use a supported publication target."),
    OutboundDisabled => ("outbound_disabled", 409, false, AskOwner, "Outbound publication is disabled; ask the repository owner to review its policy."),
    CandidateEndpointMismatch => ("candidate_endpoint_mismatch", 409, false, ProvideField, "The candidate endpoint does not match; correct the publication target."),
    AmbiguousRepositoryName => ("ambiguous_repository_name", 409, false, ProvideField, "The repository name is ambiguous; use its canonical forge identity."),
    CandidateDefaultUnavailable => ("candidate_default_unavailable", 409, true, Retry, "The default branch could not be checked; retry with the same idempotency key."),
    UnsupportedCandidateDefault => ("unsupported_candidate_default", 409, false, ProvideField, "The candidate default branch is unsupported; select a supported target."),
    CandidateOutputIsDefaultOrAlias => ("candidate_output_is_default_or_alias", 409, false, ProvideField, "The output targets the default branch or its alias; select a separate candidate branch."),
    PublicationConflict => ("publication_conflict", 409, false, InspectWork, "The publication conflicts with retained state; inspect it before sending a new request."),
    WorkspacePublicationRequired => ("workspace_publication_required", 409, false, UpdateClient, "This publication requires the workspace workflow; use its publication command."),
    BlobsMissing => ("blobs_missing", 409, false, ProvideField, "Required content blobs are missing; upload them before publishing."),
    CandidateRequired => ("candidate_required", 409, false, ProvideField, "A publication candidate is required; provide one before publishing."),
    InvalidRequestId => ("invalid_request_id", 400, false, ProvideField, "The request id is invalid; provide a valid id for the corrected request."),
    UnknownRepository => ("unknown_repository", 404, false, None, "The repository was not found; check its identifier."),
    InvalidExpiry => ("invalid_expiry", 400, false, ProvideField, "The expiry is invalid; provide a supported future expiry."),
    EnrollmentInvalid => ("enrollment_invalid", 400, false, ProvideField, "The enrollment statement or signature is invalid; correct it before enrolling."),
    UnknownDevice => ("unknown_device", 404, false, None, "The device was not found; check its identifier."),
    DeviceExists => ("device_exists", 409, false, InspectWork, "This device is already enrolled; inspect its current enrollment."),
    StaleGeneration => ("stale_generation", 409, false, InspectWork, "The device generation changed; read it again before sending a new request."),
    SharingOff => ("sharing_off", 403, false, AskOwner, "Conversation sharing is off for this repository or person; nothing was stored."),
    RequestConflict => ("request_conflict", 409, false, InspectWork, "The request id already identifies a different request; inspect the retained result."),
    DuplicateKey => ("duplicate_key", 409, false, ProvideField, "This key was already enrolled; use a new device key."),
    OwnerRoleMissing => ("owner_role_missing", 409, false, AskOwner, "The owner lacks the required repository role; ask an authorized owner to enroll."),
    RoleCheckUnavailable => ("role_check_unavailable", 503, true, Retry, "The owner role could not be checked; retry later with the same request id."),
    UnknownOwner => ("unknown_owner", 404, false, AskOwner, "The owner was not found; ask a Hub administrator to check enrollment."),
    OwnerDisabled => ("owner_disabled", 409, false, AskOwner, "The owner is disabled; ask a Hub administrator to review access."),
    OwnerWrongForge => ("owner_wrong_forge", 409, false, ProvideField, "The owner belongs to another forge; select an owner on this repository's forge."),
    RepositoryRenamed => ("repository_renamed", 409, false, InspectWork, "The repository identity changed; refresh it before enrolling."),
}

impl ErrorCode {
    pub fn for_status(status: u16) -> Self {
        match status {
            401 => Self::Unauthorized,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            405 => Self::MethodNotAllowed,
            408 => Self::RequestTimeout,
            410 => Self::Gone,
            413 => Self::RequestTooLarge,
            415 => Self::ExpectedJson,
            429 => Self::RateLimited,
            500..=599 => Self::Unavailable,
            _ => Self::InvalidRequest,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldProblem {
    Missing,
    Unknown,
    WrongType,
    Invalid,
    TooLong,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InputProblem {
    pub name: String,
    pub problem: FieldProblem,
    pub hint: String,
}

pub fn field_identifier(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !(name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl InputProblem {
    pub fn new(name: &str, problem: FieldProblem, hint: &'static str) -> Option<Self> {
        field_identifier(name).then(|| Self {
            name: name.into(),
            problem,
            hint: hint.into(),
        })
    }

    pub fn from_json_error(detail: &str) -> Option<Self> {
        let detail = detail
            .strip_prefix("Failed to deserialize the JSON body into the target type: ")
            .unwrap_or(detail);
        for (prefix, problem, hint) in [
            (
                "missing field `",
                FieldProblem::Missing,
                "Provide this required request field.",
            ),
            (
                "unknown field `",
                FieldProblem::Unknown,
                "Remove this unknown request field.",
            ),
        ] {
            if let Some(rest) = detail.strip_prefix(prefix) {
                return Self::new(rest.split_once('`')?.0, problem, hint);
            }
        }
        for (marker, problem) in [
            (": invalid type:", FieldProblem::WrongType),
            (": invalid value:", FieldProblem::Invalid),
            (": unknown variant ", FieldProblem::Invalid),
            (": invalid length ", FieldProblem::Invalid),
        ] {
            if let Some((name, _)) = detail.split_once(marker) {
                return Self::new(
                    name,
                    problem,
                    "Use the documented type and values for this field.",
                );
            }
        }
        None
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorEnvelope {
    #[serde(rename = "error")]
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
    pub recovery: Recovery,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<InputProblem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(flatten)]
    pub details: Map<String, Value>,
    #[serde(skip)]
    pub status: u16,
}

impl ErrorEnvelope {
    pub fn local(code: ErrorCode) -> Self {
        Self {
            code: code.as_str().into(),
            message: code.message().into(),
            retryable: code.retryable(),
            retry_after_seconds: None,
            recovery: code.action().recovery(),
            fields: None,
            correlation_id: None,
            details: Map::new(),
            status: code.status(),
        }
    }

    pub fn new(code: ErrorCode) -> Self {
        Self::with_entropy(code, |random| getrandom::fill(random).is_ok())
    }

    fn with_entropy(code: ErrorCode, fill: impl FnOnce(&mut [u8; 16]) -> bool) -> Self {
        let mut envelope = Self::local(code);
        let mut random = [0_u8; 16];
        if fill(&mut random) {
            envelope.correlation_id =
                Some(format!("cor_{}", crate::team::auth::lower_hex(&random)));
        }
        envelope
    }

    pub fn render(&self) -> String {
        let mut text = format!(
            "{}; {}",
            self.message.trim_end_matches('.'),
            self.recovery.action.sentence()
        );
        if let Some(command) = &self.recovery.command
            && !text.contains(command)
        {
            text.push_str(&format!("; run {command}"));
        }
        if let Some(reference) = &self.correlation_id {
            text.push_str(&format!("; ref {reference}"));
        }
        text
    }

    pub fn hook_note(&self) -> String {
        format!(
            "AgentSCM: coordination unknown ({}). Other agents' work on these files is not visible.",
            self.render()
        )
    }

    pub fn mcp(&self) -> Value {
        let mut error = serde_json::to_value(self).expect("error envelope is JSON");
        let object = error.as_object_mut().expect("error envelope is an object");
        object.remove("error");
        object.insert("kind".into(), json!(self.code));
        object.insert("code".into(), json!(self.code));
        object
            .entry("alternatives")
            .or_insert_with(|| json!([self.recovery.action.sentence()]));
        json!({"content":[{"type":"text","text":self.render()}],"structuredContent":{"error":error},"isError":true})
    }
}

impl fmt::Display for ErrorEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

#[derive(Debug)]
pub struct ReviewedRejection {
    pub(super) code: ErrorCode,
    pub(super) message: String,
    mapped: Option<Box<ErrorEnvelope>>,
}

impl ReviewedRejection {
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
    pub(super) fn new(code: ErrorCode, message: String) -> Self {
        assert!(matches!(
            code,
            ErrorCode::SessionRequired
                | ErrorCode::LeadSessionMismatch
                | ErrorCode::LeaseLapsed
                | ErrorCode::InvalidOrChanged
        ));
        Self {
            code,
            message,
            mapped: None,
        }
    }
}

impl HubError {
    pub(super) fn reviewed(code: ErrorCode) -> Self {
        Self::Rejected(ReviewedRejection {
            code,
            message: code.message().into(),
            mapped: None,
        })
    }

    pub(super) fn session_required() -> Self {
        Self::Rejected(ReviewedRejection::new(
            ErrorCode::SessionRequired,
            super::session::SESSION_REQUIRED.into(),
        ))
    }

    pub(super) fn lead_session_mismatch(message: String) -> Self {
        Self::Rejected(ReviewedRejection::new(
            ErrorCode::LeadSessionMismatch,
            message,
        ))
    }

    pub(super) fn lease_lapsed(message: String) -> Self {
        Self::Rejected(ReviewedRejection::new(ErrorCode::LeaseLapsed, message))
    }

    /// A Work-route refusal whose Hub-authored reason (a fence, a missing field, a current
    /// sequence number) is what the agent needs to correct the next request.
    pub(super) fn invalid_or_changed(message: String) -> Self {
        Self::Rejected(ReviewedRejection::new(ErrorCode::InvalidOrChanged, message))
    }
}

impl From<Box<ErrorEnvelope>> for HubError {
    fn from(envelope: Box<ErrorEnvelope>) -> Self {
        let Some(code) = ErrorCode::parse(&envelope.code) else {
            return Self::Unavailable("unregistered internal error envelope".into());
        };
        Self::Rejected(ReviewedRejection {
            code,
            message: code.message().into(),
            mapped: Some(envelope),
        })
    }
}

pub fn from_hub_error(error: &HubError) -> ErrorEnvelope {
    if let HubError::Rejected(rejection) = error
        && let Some(envelope) = &rejection.mapped
    {
        return envelope.as_ref().clone();
    }
    let code = match error {
        HubError::Invalid(detail) => match detail.as_str() {
            "workspace_forbidden" => ErrorCode::Forbidden,
            "workspace_csrf" => ErrorCode::CsrfRequired,
            "workspace_generation_conflict" => ErrorCode::GenerationConflict,
            "workspace_idempotency_conflict" => ErrorCode::IdempotencyConflict,
            "workspace_native_conflict" => ErrorCode::NativeSubjectConflict,
            "workspace_last_administrator" => ErrorCode::LastAdministrator,
            "workspace_member_limit" => ErrorCode::MemberLimit,
            _ => ErrorCode::InvalidRequest,
        },
        HubError::Rejected(rejection) => rejection.code,
        HubError::Input(_) => ErrorCode::InvalidRequest,
        HubError::Unauthorized => ErrorCode::Unauthorized,
        HubError::Busy => ErrorCode::Busy,
        HubError::WorkCapacity(_) => ErrorCode::WorkStorageCapacity,
        HubError::WorkOverlap(_) => ErrorCode::WorkScopeOverlap,
        HubError::WorkCoordinationRequired { .. } => ErrorCode::WorkCoordinationRequired,
        HubError::WorkOverlapIncomplete => ErrorCode::WorkOverlapIncomplete,
        HubError::WorkPreflight(_) => ErrorCode::WorkPreflightRequired,
        HubError::WorkCheckpointSequenceConflict { .. } => {
            ErrorCode::WorkCheckpointSequenceConflict
        }
        HubError::Unavailable(_) => ErrorCode::Unavailable,
    };
    let mut envelope = ErrorEnvelope::new(code);
    match error {
        HubError::Rejected(rejection) => envelope.message = server_message(&rejection.message),
        HubError::Input(fields) => {
            envelope.message = "Provide exactly one Work identity: work_id, source or request_key; do not use a title as identity.".into();
            envelope.fields = Some(fields.clone());
        }
        HubError::WorkCheckpointSequenceConflict { current_seq } => {
            envelope
                .details
                .insert("current_seq".into(), json!(current_seq));
        }
        HubError::WorkCapacity(capacity) => {
            envelope
                .details
                .insert("limit".into(), json!(capacity.limit.as_str()));
            // Only full storage needs the operator; every other limit is the agent's to clear.
            if let Some(remedy) = capacity.limit.agent_remedy() {
                let what = capacity.limit.what();
                envelope.message = format!("{}{}: {remedy}.", what[..1].to_uppercase(), &what[1..]);
                envelope.recovery = RecoveryAction::InspectWork.recovery();
            }
        }
        HubError::WorkOverlap(overlaps) => {
            envelope.details.insert("overlaps".into(), json!(overlaps));
            envelope.details.insert(
                "alternatives".into(),
                json!([
                    "Split the declared scope or join the existing work as helper or reviewer.",
                    "Arrange a deliberate handover with the current lead."
                ]),
            );
        }
        HubError::WorkCoordinationRequired { overlaps, request } => {
            envelope.details.insert("overlaps".into(), json!(overlaps));
            envelope
                .details
                .insert("coordination_request".into(), json!(request));
        }
        HubError::WorkPreflight(preflight) => {
            envelope
                .details
                .insert("preflight".into(), json!(preflight));
        }
        _ => {}
    }
    envelope.details = envelope
        .details
        .into_iter()
        .map(|(key, value)| (key, server_detail(&value)))
        .collect();
    diagnostic(&envelope, &error.to_string());
    envelope
}

pub fn from_code(code: ErrorCode) -> ErrorEnvelope {
    let envelope = ErrorEnvelope::new(code);
    // A 4xx names the caller's mistake and carries no server cause, so a probe does not fill the log.
    if code.status() >= 500 {
        diagnostic(&envelope, code.message());
    }
    envelope
}

pub fn diagnostic(envelope: &ErrorEnvelope, detail: &str) {
    let entropy_note = if envelope.correlation_id.is_none()
        && !ENTROPY_FAILURE_REPORTED.swap(true, Ordering::Relaxed)
    {
        " correlation_id omitted: operating-system entropy unavailable"
    } else {
        ""
    };
    eprintln!("{}{entropy_note}", diagnostic_line(envelope, detail));
}

pub fn log_internal(detail: &impl fmt::Debug) {
    diagnostic(
        &ErrorEnvelope::new(ErrorCode::Unavailable),
        &format!("{detail:?}"),
    );
}

pub fn diagnostic_line(envelope: &ErrorEnvelope, detail: &str) -> String {
    let detail: String = sanitize(detail).chars().take(DIAGNOSTIC_LIMIT).collect();
    format!(
        "hub error: code={} ref={} detail={detail}",
        envelope.code,
        envelope.correlation_id.as_deref().unwrap_or("unavailable")
    )
}

pub fn valid_correlation_id(value: &str) -> bool {
    value.len() == 36
        && value.starts_with("cor_")
        && value.as_bytes()[4..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

pub fn sanitize(text: &str) -> String {
    sanitize_text(text, true)
}

pub(super) fn server_message(text: &str) -> String {
    sanitize_text(text, false)
}

fn sanitize_text(text: &str, redact_paths: bool) -> String {
    let bounded: String = text.chars().take(16_384).collect();
    let text = bounded
        .lines()
        .map(|line| {
            line.to_ascii_lowercase().find("authorization").map_or_else(
                || line.to_owned(),
                |offset| format!("{} [redacted]", &line[..offset]),
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut clean = String::new();
    let mut escape = false;
    let mut control_string = false;
    for character in text.chars() {
        if control_string {
            if character == '\u{7}' {
                control_string = false;
            }
            continue;
        }
        if escape {
            if character == ']' {
                control_string = true;
                escape = false;
            } else if character != '[' && ('@'..='~').contains(&character) {
                escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            escape = true;
            continue;
        }
        if character.is_control() {
            clean.push(' ');
        } else {
            clean.push(character);
        }
    }
    let mut redact_next = false;
    let words = clean
        .split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            let sensitive_label = lower.contains("authorization")
                || lower.trim_matches(|character: char| !character.is_ascii_alphabetic())
                    == "bearer"
                || lower.contains("token_file")
                || lower.contains("token_path");
            let secret = word
                .as_bytes()
                .windows(64)
                .any(|part| part.iter().all(u8::is_ascii_hexdigit))
                || [
                    "tok_",
                    "token=",
                    "token:",
                    "digest=",
                    "digest:",
                    "token_file",
                    "token_path",
                ]
                .iter()
                .any(|prefix| lower.contains(prefix))
                || (redact_paths && (word.contains('/') || word.contains('\\')))
                || (word.len() > 1
                    && word
                        .trim_start_matches(['\'', '"', '(', '['])
                        .starts_with(['/', '~', '\\']))
                || word.contains(":\\")
                || word.contains("file://");
            let redacted = redact_next || sensitive_label || secret;
            redact_next = sensitive_label;
            if redacted { "[redacted]" } else { word }
        })
        .collect::<Vec<_>>()
        .join(" ");
    words.chars().take(TEXT_LIMIT).collect()
}

pub fn decode(status: u16, body: &Value) -> ErrorEnvelope {
    let body = body.get("structuredContent").unwrap_or(body);
    let body = if body["error"].is_object() {
        body["error"]
            .get("data")
            .filter(|data| data.is_object())
            .unwrap_or(&body["error"])
    } else {
        body
    };
    let supplied = body["error"]
        .as_str()
        .or_else(|| body["code"].as_str())
        .or_else(|| body["kind"].as_str());
    let valid_code = supplied.filter(|code| {
        !code.is_empty()
            && code.len() <= 64
            && !(code.len() == 64 && code.bytes().all(|byte| byte.is_ascii_hexdigit()))
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    });
    let known = valid_code.and_then(ErrorCode::parse);
    let fallback = ErrorCode::for_status(status);
    let mut envelope = ErrorEnvelope::local(known.unwrap_or(fallback));
    envelope.status = status;
    if let Some(code) = valid_code.filter(|_| known.is_none()) {
        envelope.code = code.into();
        envelope.retryable = matches!(status, 408 | 429 | 502..=504);
        envelope.message = "The Hub refused the request; inspect the error code and contact the operator if needed.".into();
        envelope.recovery = if envelope.retryable {
            RecoveryAction::Retry
        } else {
            RecoveryAction::ContactOperator
        }
        .recovery();
    }
    if let Some(message) = body["message"].as_str() {
        let message = server_message(message);
        if !message.is_empty() {
            envelope.message = message;
        }
    }
    if let Some(retryable) = body["retryable"].as_bool()
        && known.is_some()
    {
        envelope.retryable = retryable;
    }
    if envelope.retryable {
        envelope.retry_after_seconds = body["retry_after_seconds"]
            .as_u64()
            .and_then(|seconds| u32::try_from(seconds).ok());
    }
    if let Ok(action) = serde_json::from_value::<RecoveryAction>(body["recovery"]["action"].clone())
    {
        envelope.recovery = action.recovery();
    }
    envelope.correlation_id = body["correlation_id"]
        .as_str()
        .filter(|reference| valid_correlation_id(reference))
        .map(str::to_owned);
    if let Some(fields) = body["fields"].as_array() {
        let fields: Vec<_> = fields
            .iter()
            .take(16)
            .filter_map(|field| {
                let name = field["name"].as_str()?;
                let problem = serde_json::from_value(field["problem"].clone()).ok()?;
                InputProblem::new(name, problem, "Correct this request field.")
            })
            .collect();
        if !fields.is_empty() {
            envelope.fields = Some(fields);
        }
    }
    for key in [
        "current_seq",
        "overlaps",
        "preflight",
        "coordination_request",
        "alternatives",
        "login",
        "repository",
        "visibility",
        "has",
        "needs",
        "fork_of",
        "refused_paths",
        "current_generation",
        "reason",
    ] {
        if let Some(value) = body.get(key) {
            envelope
                .details
                .insert(key.into(), sanitized_value(value, server_message));
        }
    }
    envelope
}

pub(super) fn server_detail(value: &Value) -> Value {
    sanitized_value(value, server_message)
}

fn sanitized_value(value: &Value, clean: fn(&str) -> String) -> Value {
    match value {
        Value::String(text) => json!(clean(text)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| sanitized_value(value, clean))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let name = key.to_ascii_lowercase();
                    let value = if matches!(
                        name.as_str(),
                        "authorization"
                            | "token"
                            | "token_id"
                            | "digest"
                            | "secret"
                            | "credential"
                            | "token_file"
                            | "token_path"
                            | "body"
                            | "request_body"
                            | "raw_body"
                            | "session"
                            | "session_id"
                    ) || name.ends_with("_token")
                        || name.ends_with("_digest")
                        || name.ends_with("_session")
                    {
                        json!("[redacted]")
                    } else {
                        sanitized_value(value, clean)
                    };
                    (clean(key), value)
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_registered_code_is_documented_in_the_api_reference() {
        let docs = include_str!("../../docs/api.md");
        for code in ErrorCode::ALL {
            assert!(
                docs.contains(&format!("| `{}` |", code.as_str())),
                "docs/api.md lacks the error code {}",
                code.as_str()
            );
        }
    }

    use std::collections::HashSet;

    use super::*;

    #[test]
    fn entropy_failure_omits_the_reference_without_panicking() {
        let envelope = ErrorEnvelope::with_entropy(ErrorCode::Busy, |_| false);
        assert!(envelope.correlation_id.is_none());
        assert!(!envelope.render().contains("ref cor_"));
        assert_eq!(envelope.code, "busy");
        let deterministic = ErrorEnvelope::with_entropy(ErrorCode::Busy, |random| {
            random.fill(0xab);
            true
        });
        assert_eq!(
            deterministic.correlation_id.as_deref(),
            Some("cor_abababababababababababababababab")
        );
    }

    #[test]
    fn diagnostics_preserve_bounded_causes_and_redact_credentials() {
        for error in [
            HubError::Unavailable("db error: connection refused".into()),
            HubError::Invalid(
                "durable default work is already bound to another hook session".into(),
            ),
        ] {
            let envelope = from_hub_error(&error);
            let line = diagnostic_line(&envelope, &error.to_string());
            assert!(line.contains(&error.to_string()));
            assert!(line.contains(&format!(
                "code={} ref={}",
                envelope.code,
                envelope.correlation_id.as_deref().unwrap()
            )));
            assert!(!envelope.message.contains("db error:"));
            assert!(!envelope.message.contains("durable default work"));
        }
        let envelope = ErrorEnvelope::new(ErrorCode::Unavailable);
        for detail in [
            format!("db cause {} still useful", "a".repeat(64)),
            "db cause Bearer opaque-value still useful".into(),
            "db cause \"Bearer opaque-value\" still useful".into(),
            "db cause Authorization: opaque-value tail-secret\nnext cause".into(),
            "db cause tok_synthetic-secret /private/example/token.json still useful".into(),
            "db cause token_file relative-secret.json still useful".into(),
        ] {
            let line = diagnostic_line(&envelope, &detail);
            assert!(line.contains("db cause"), "{line}");
            for secret in [
                &"a".repeat(64),
                "opaque-value",
                "tail-secret",
                "tok_synthetic-secret",
                "/private/example",
                "relative-secret.json",
            ] {
                assert!(!line.contains(secret), "{line}");
            }
        }
        let line = diagnostic_line(&envelope, &format!("{}\u{1b}[31m\n", "x".repeat(1000)));
        assert_eq!(
            line.split_once(" detail=").unwrap().1.chars().count(),
            DIAGNOSTIC_LIMIT
        );
        assert!(!line.contains(['\n', '\u{1b}']));
    }

    #[test]
    fn workspace_symbols_resolve_through_the_shared_registry() {
        for (symbol, code) in [
            ("workspace_forbidden", ErrorCode::Forbidden),
            ("workspace_csrf", ErrorCode::CsrfRequired),
            (
                "workspace_generation_conflict",
                ErrorCode::GenerationConflict,
            ),
            (
                "workspace_idempotency_conflict",
                ErrorCode::IdempotencyConflict,
            ),
            (
                "workspace_native_conflict",
                ErrorCode::NativeSubjectConflict,
            ),
            ("workspace_last_administrator", ErrorCode::LastAdministrator),
            ("workspace_member_limit", ErrorCode::MemberLimit),
            ("unreviewed internal cause", ErrorCode::InvalidRequest),
        ] {
            let envelope = from_hub_error(&HubError::Invalid(symbol.into()));
            assert_eq!(envelope.code, code.as_str());
            assert_eq!(envelope.status, code.status());
            assert_eq!(envelope.message, code.message());
        }
    }

    #[tokio::test]
    async fn viewer_masked_work_errors_hide_other_owner_sessions_and_tokens_on_both_surfaces() {
        use crate::hub::viewer::Viewer;
        use axum::response::IntoResponse;

        let item = json!({
            "id":"wrk_example", "repository":"example/widgets", "owner":"usr_other", "owner_login":"other",
            "source":{"kind":"native","request_key":"synthetic-work"}, "title":"Synthetic Work", "state":"active",
            "claim_status":"active", "last_checkin_at":1, "generation":1, "lead_token":"tok_other_private",
            "lead_agent":"client-example-host-abcd1234", "lead_session":"other-private-native-session",
            "participants":[{"owner":"usr_other","login":"other","token":"tok_other_private", "agent":"client-example-host-abcd1234",
                "session":"other-private-native-session","role":"lead"}], "scope":["src/example.rs"], "depends_on":[],
            "related_work":[], "created_at":1,"updated_at":1,"created_seq":1,"updated_seq":1
        });
        let overlaps = json!([{"item":item,"your_scope":["src/example.rs"],"their_scope":["src/example.rs"],"reasons":[]}]);
        let request = serde_json::from_value(json!({
            "id":"req_example", "requester":{"token":"tok_other_private","agent":"client-example-host-abcd1234",
                "session":"other-private-native-session","owner":"usr_other","login":"other"},
            "proposal":{"title":"Synthetic Work","scope":["src/example.rs"]}, "targets":[], "status":"pending",
            "acknowledgements":[],"acknowledgements_complete":true,"responses":[],"responses_complete":true,
            "created_at":1,"updated_at":1,"created_seq":1,"updated_seq":1
        })).unwrap();
        let preflight = serde_json::from_value(json!({
            "status":"review_required", "current_merge_through":0,"source_relation_checked":false,
            "subject_match":item,"scope_overlaps":overlaps,"merges":[],"checked_observations":[],"reasons":[],"complete":false
        })).unwrap();
        for (mut error, code) in [
            (
                HubError::WorkOverlap(serde_json::from_value(overlaps.clone()).unwrap()),
                "work_scope_overlap",
            ),
            (
                HubError::WorkCoordinationRequired {
                    overlaps: serde_json::from_value(overlaps).unwrap(),
                    request: Box::new(request),
                },
                "work_coordination_required",
            ),
            (
                HubError::WorkPreflight(Box::new(preflight)),
                "work_preflight_required",
            ),
        ] {
            error.show(Viewer("usr_viewer"));
            let envelope = from_hub_error(&error);
            assert_eq!(envelope.code, code);
            let response = crate::hub::http::ApiError::from(error).into_response();
            let bytes = axum::body::to_bytes(response.into_body(), 32_768)
                .await
                .unwrap();
            for text in [
                String::from_utf8(bytes.to_vec()).unwrap(),
                envelope.mcp().to_string(),
            ] {
                assert!(text.contains(code));
                for secret in [
                    "other-private-native-session",
                    "tok_other_private",
                    "example-host",
                ] {
                    assert!(!text.contains(secret), "{code} leaked {secret}: {text}");
                }
            }
        }
    }

    #[test]
    fn registry_is_unique_complete_and_has_safe_defaults() {
        let mut codes = HashSet::new();
        for code in ErrorCode::ALL {
            assert!(codes.insert(code.as_str()));
            assert_eq!(ErrorCode::parse(code.as_str()), Some(*code));
            assert!((400..600).contains(&code.status()));
            assert!(!code.message().is_empty());
            assert!(!code.action().sentence().is_empty());
            let envelope = ErrorEnvelope::new(*code);
            assert_eq!(envelope.retryable, code.retryable());
            assert_eq!(envelope.recovery, code.action().recovery());
        }
    }

    #[test]
    fn every_hub_variant_has_one_mapping_and_preserves_code_details() {
        let request = serde_json::from_value(json!({
            "id":"req_example", "requester":{"token":"tok_example","agent":"example","session":"example","owner":"usr_example","login":"example"},
            "proposal":{"title":"Synthetic task","scope":[]}, "targets":[],"status":"pending",
            "acknowledgements":[],"acknowledgements_complete":true,"responses":[],"responses_complete":true,
            "created_at":1,"updated_at":1,"created_seq":1,"updated_seq":1
        })).unwrap();
        let preflight = serde_json::from_value(json!({
            "status":"unknown","current_merge_through":0,"source_relation_checked":false,
            "scope_overlaps":[],"merges":[],"checked_observations":[],"reasons":[],"complete":false
        }))
        .unwrap();
        for (error, code) in [
            (
                HubError::Invalid("untrusted detail".into()),
                ErrorCode::InvalidRequest,
            ),
            (HubError::session_required(), ErrorCode::SessionRequired),
            (
                HubError::lead_session_mismatch("LEAD_SESSION_MISMATCH".into()),
                ErrorCode::LeadSessionMismatch,
            ),
            (
                HubError::lease_lapsed("LEASE_LAPSED".into()),
                ErrorCode::LeaseLapsed,
            ),
            (HubError::Input(Vec::new()), ErrorCode::InvalidRequest),
            (HubError::Unauthorized, ErrorCode::Unauthorized),
            (HubError::Busy, ErrorCode::Busy),
            (
                HubError::capacity(crate::hub::capacity::CapacityLimit::WorkStorage, None),
                ErrorCode::WorkStorageCapacity,
            ),
            (
                HubError::WorkOverlap(Vec::new()),
                ErrorCode::WorkScopeOverlap,
            ),
            (
                HubError::WorkCoordinationRequired {
                    overlaps: Vec::new(),
                    request: Box::new(request),
                },
                ErrorCode::WorkCoordinationRequired,
            ),
            (
                HubError::WorkOverlapIncomplete,
                ErrorCode::WorkOverlapIncomplete,
            ),
            (
                HubError::WorkPreflight(Box::new(preflight)),
                ErrorCode::WorkPreflightRequired,
            ),
            (
                HubError::WorkCheckpointSequenceConflict { current_seq: 17 },
                ErrorCode::WorkCheckpointSequenceConflict,
            ),
            (
                HubError::Unavailable("untrusted row value".into()),
                ErrorCode::Unavailable,
            ),
        ] {
            let envelope = from_hub_error(&error);
            assert_eq!(envelope.code, code.as_str());
            if matches!(error, HubError::WorkCheckpointSequenceConflict { .. }) {
                assert_eq!(envelope.details["current_seq"], 17);
            }
            if code == ErrorCode::WorkCoordinationRequired {
                assert_eq!(
                    envelope.details["coordination_request"]["requester"]["token"],
                    "[redacted]"
                );
                assert_eq!(
                    envelope.details["coordination_request"]["requester"]["session"],
                    "[redacted]"
                );
                assert_eq!(
                    envelope.details["coordination_request"]["id"],
                    "req_example"
                );
            }
        }
    }

    #[test]
    fn correlation_ids_are_random_unique_and_never_decoded_from_caller_values() {
        let mut references = HashSet::new();
        for _ in 0..256 {
            let reference = ErrorEnvelope::new(ErrorCode::Unauthorized)
                .correlation_id
                .unwrap();
            assert!(valid_correlation_id(&reference));
            assert!(references.insert(reference));
        }
        assert!(
            decode(401, &json!({"correlation_id":"forged"}))
                .correlation_id
                .is_none()
        );
        assert!(
            ErrorEnvelope::local(ErrorCode::HubUnreachable)
                .correlation_id
                .is_none()
        );
    }

    #[test]
    fn frozen_old_hub_shapes_decode_and_old_clients_still_read_error() {
        let fixtures: Vec<Value> = serde_json::from_str(include_str!(
            "../../tests/fixtures/hub_errors/http-baseline.json"
        ))
        .unwrap();
        for fixture in fixtures {
            let code = ErrorCode::parse(fixture["error"].as_str().unwrap()).unwrap();
            let envelope = decode(code.status(), &fixture);
            assert_eq!(envelope.code, code.as_str());
            assert!(!envelope.message.is_empty());
            assert_eq!(envelope.recovery, code.action().recovery());
            assert!(envelope.correlation_id.is_none());
            let modern = serde_json::to_value(ErrorEnvelope::new(code)).unwrap();
            assert_eq!(modern["error"], fixture["error"]);
        }
        let old: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/hub_errors/mcp-baseline.json"
        ))
        .unwrap();
        assert_eq!(decode(200, &old).code, "busy");
        assert_eq!(
            decode(
                400,
                &json!({"error":{"code":-32000,"message":"refused","data":{"error":"read_only"}}})
            )
            .code,
            "read_only"
        );
    }

    #[test]
    fn future_codes_are_opaque_and_only_explicit_statuses_allow_retry() {
        assert_eq!(
            decode(409, &json!({"error":"future_code","retryable":true})).code,
            "future_code"
        );
        assert!(!decode(409, &json!({"error":"future_code","retryable":true})).retryable);
        assert!(decode(503, &json!({"error":"future_code"})).retryable);
        assert!(!decode(500, &json!({"error":"future_code"})).retryable);
        assert_eq!(
            decode(400, &json!({"error":"bad\ncode"})).code,
            "invalid_request"
        );
    }

    #[test]
    fn outage_bad_reply_and_rejection_have_distinct_recovery() {
        let unreachable = ErrorEnvelope::local(ErrorCode::HubUnreachable);
        let unavailable = decode(503, &json!({"error":"unavailable"}));
        let rejected = decode(401, &json!({"error":"unauthorized"}));
        let forbidden = decode(403, &json!({"error":"forbidden"}));
        assert_ne!(unreachable.code, unavailable.code);
        assert_ne!(unavailable.recovery, rejected.recovery);
        assert_ne!(rejected.recovery, forbidden.recovery);
        assert!(!rejected.retryable && !forbidden.retryable);
    }

    #[test]
    fn every_registry_code_redacts_secrets_on_all_rendered_surfaces() {
        let token = "a".repeat(64);
        let digest = "b".repeat(64);
        let hostile = format!(
            "\u{1b}[31munsafe\u{1b}[0m\nAuthorization: Bearer secret-value {token} digest={digest} /private/example/token.json C:\\example\\token.json"
        );
        for code in ErrorCode::ALL {
            let envelope = decode(
                code.status(),
                &json!({"error":code.as_str(),"message":hostile,"reason":hostile,"preflight":{"detail":hostile,"Authorization":"secret-value","token_file":"relative.token"},"recovery":{"action":"reconnect","command":"attacker-controlled-command"}}),
            );
            for output in [
                envelope.render(),
                envelope.hook_note(),
                envelope.mcp().to_string(),
                serde_json::to_string(&envelope).unwrap(),
                diagnostic_line(&envelope, &hostile),
            ] {
                for secret in [
                    &token[..],
                    &digest[..],
                    "secret-value",
                    "/private/example",
                    "C:\\example",
                    "attacker-controlled-command",
                    "relative.token",
                    "\u{1b}",
                    "\n",
                ] {
                    assert!(
                        !output.contains(secret),
                        "{} leaked {secret}",
                        code.as_str()
                    );
                }
            }
            assert!(envelope.message.len() < 1200);
        }
        let invalid = from_hub_error(&HubError::Invalid(hostile.clone()));
        let unavailable = from_hub_error(&HubError::Unavailable(hostile));
        assert!(!invalid.render().contains(&token));
        assert!(!unavailable.render().contains(&digest));
    }

    #[tokio::test]
    async fn every_http_code_redacts_messages_and_nested_details_and_keeps_its_reference() {
        use axum::response::IntoResponse;
        let token = "a".repeat(64);
        let hostile =
            format!("Authorization: Bearer example-secret {token} /private/example/token.json");
        for code in ErrorCode::ALL {
            let response = super::super::http::ApiError::new(
                axum::http::StatusCode::from_u16(code.status()).unwrap(),
                code.as_str(),
            )
            .with_message(hostile.clone())
            .with_field(
                "preflight",
                json!({"detail":hostile,"scope":["src/example.rs"]}),
            )
            .into_response();
            let reference = response.headers()["x-agentscm-correlation-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let bytes = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains(&token));
            assert!(!text.contains("example-secret"));
            assert!(!text.contains("/private/example"));
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"], code.as_str());
            assert_eq!(body["correlation_id"], reference);
            assert_eq!(body["preflight"]["scope"][0], "src/example.rs");
        }
    }

    #[test]
    fn fields_accept_only_identifiers_and_never_echo_values() {
        assert_eq!(
            InputProblem::from_json_error("missing field `request_key`")
                .unwrap()
                .name,
            "request_key"
        );
        assert!(InputProblem::from_json_error("unknown field `/private/secret`").is_none());
        assert!(InputProblem::from_json_error("unknown field `bad\nname`").is_none());
        assert_eq!(
            InputProblem::from_json_error("role: unknown variant `secret-value`, expected `lead`")
                .unwrap()
                .name,
            "role"
        );
        assert!(
            InputProblem::from_json_error(&format!("unknown field `{}`", "a".repeat(64))).is_none()
        );
        assert!(
            InputProblem::from_json_error(
                "invalid type: string \"missing field `secret`\", expected a number"
            )
            .is_none()
        );
        assert!(
            InputProblem::from_json_error(&format!("missing field `{}`", "a".repeat(65))).is_none()
        );
        let envelope = decode(
            400,
            &json!({"fields":[{"name":"request_key","problem":"missing","hint":"secret-value"},{"name":"bad\nname","problem":"unknown"}]}),
        );
        assert_eq!(envelope.fields.as_ref().unwrap().len(), 1);
        assert!(!envelope.render().contains("secret-value"));
    }
}
