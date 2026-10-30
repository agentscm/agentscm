use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::findings::{FindingClaim, FindingIdentity};
use crate::checks::CheckSubject;
use crate::{Error, Result};

pub const REVIEW_PROTOCOL: &str = "agentscm.model-review/v1";
pub const MAX_ROWS: usize = 128;
pub const MAX_ROW_BYTES: usize = 16 * 1024;
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;
pub const MAX_FINDINGS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewLimits {
    pub max_source_bytes: u32,
    pub max_request_bytes: u32,
    pub max_total_request_bytes: u32,
    pub max_response_bytes: u32,
    pub max_calls: u32,
    pub max_input_tokens: u64,
    pub max_output_tokens: u32,
    pub max_total_tokens: u64,
    pub max_cost_micros: Option<u64>,
    pub timeout_ms: u32,
}

impl ReviewLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_source_bytes == 0
            || self.max_source_bytes as usize > MAX_SOURCE_BYTES
            || self.max_request_bytes == 0
            || self.max_request_bytes as usize > MAX_REQUEST_BYTES
            || self.max_total_request_bytes < self.max_request_bytes
            || self.max_total_request_bytes > 8 * 1024 * 1024
            || self.max_response_bytes == 0
            || self.max_response_bytes as usize > MAX_RESPONSE_BYTES
            || !(1..=16).contains(&self.max_calls)
            || !(1..=1_000_000).contains(&self.max_input_tokens)
            || !(1..=16_384).contains(&self.max_output_tokens)
            || !(1..=2_000_000).contains(&self.max_total_tokens)
            || self.max_cost_micros == Some(0)
            || !(1..=60_000).contains(&self.timeout_ms)
        {
            return Err(Error::Limit(
                "model review limits exceed the supported profile".into(),
            ));
        }
        Ok(())
    }
}

/// C authorizes this exact selector before reads, including cache hits and misses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceBinding {
    pub profile: String,
    /// C-issued digest binds manifest, profile, incarnation, lease and authorization
    /// generations. This value alone is not an authorization or closure proof.
    pub permit_sha256: String,
}

impl ReviewSourceBinding {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.profile)?;
        validate_digest(&self.permit_sha256)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceSelector {
    pub source: ReviewSourceBinding,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub raw_sha256: String,
    pub rendered_sha256: String,
    pub required: bool,
}

impl ReviewSourceSelector {
    pub fn row_id(&self) -> Result<String> {
        self.validate()?;
        domain_digest("review-source-row/v1", self)
    }

    pub fn validate(&self) -> Result<()> {
        if self.path.is_empty()
            || self.path.len() > 1024
            || !self.path.is_ascii()
            || self
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".." || part.len() > 255)
            || self
                .path
                .bytes()
                .any(|b| b.is_ascii_control() || matches!(b, b'\\' | b':'))
            || self.start_line == 0
            || self.end_line < self.start_line
            || self.end_line - self.start_line >= 16_384
        {
            return Err(Error::Invalid("invalid model source selector".into()));
        }
        self.source.validate()?;
        validate_digest(&self.raw_sha256)?;
        validate_digest(&self.rendered_sha256)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewRecipe {
    pub version: u32,
    pub model: String,
    pub objective: String,
    pub provider_scope_sha256: String,
    pub installed_policy_sha256: String,
    pub policy_epoch: u64,
    pub source: Vec<ReviewSourceSelector>,
    pub limits: ReviewLimits,
}

impl ReviewRecipe {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut recipe = self.clone();
        recipe.source.sort_by(|a, b| {
            (&a.path, a.start_line, a.end_line).cmp(&(&b.path, b.start_line, b.end_line))
        });
        Ok(serde_json::to_vec(&recipe)?)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(crate::checks::sha256(&self.canonical_bytes()?))
    }

    pub fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        validate_identifier(&self.model)?;
        validate_digest(&self.provider_scope_sha256)?;
        validate_digest(&self.installed_policy_sha256)?;
        if self.version != 1
            || self.objective.trim().is_empty()
            || self.objective.len() > 4096
            || self.source.is_empty()
            || self.source.len() > MAX_ROWS
            || self.policy_epoch > i64::MAX as u64
        {
            return Err(Error::Invalid("unsupported model review recipe".into()));
        }
        let mut keys = std::collections::HashSet::new();
        for row in &self.source {
            row.validate()?;
            if !keys.insert((&row.path, row.start_line, row.end_line)) {
                return Err(Error::Invalid("duplicate model source selector".into()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub attempt_id: String,
    pub run_generation: u64,
    pub subject: CheckSubject,
    pub recipe: ReviewRecipe,
}

impl ReviewRequest {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.attempt_id)?;
        self.subject.change.validate()?;
        if self.subject.repository != self.subject.change.repository
            || self.subject.change_version == 0
            || self.subject.change_version > i64::MAX as u64
            || self.run_generation == 0
            || self.run_generation > i64::MAX as u64
        {
            return Err(Error::Invalid(
                "invalid model review subject or run generation".into(),
            ));
        }
        for digest in [
            &self.subject.revision,
            &self.subject.context_digest,
            &self.subject.recipe_sha256,
            &self.subject.toolchain_sha256,
            &self.subject.environment_sha256,
        ] {
            validate_digest(digest)?;
        }
        if self.subject.recipe_sha256 != self.recipe.digest()? {
            return Err(Error::Invalid(
                "model recipe does not match its subject".into(),
            ));
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.recipe = serde_json::from_slice(&self.recipe.canonical_bytes()?)?;
        domain_digest(REVIEW_PROTOCOL, &canonical)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReviewCancellation(Arc<AtomicBool>);

impl ReviewCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceRow {
    pub row_id: String,
    pub selector: ReviewSourceSelector,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceReadFailure {
    Denied,
    Missing,
    Indeterminate,
    Limit,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceRendering {
    ExactUtf8,
    Redacted,
    Truncated,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSourceObservation {
    pub source: ReviewSourceBinding,
    pub raw: Vec<u8>,
    pub rendered: String,
    pub rendering: SourceRendering,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceMetadata {
    pub source: ReviewSourceBinding,
    pub raw_bytes: u64,
    pub raw_sha256: Option<String>,
    pub rendered_bytes: u64,
    pub rendered_sha256: Option<String>,
    pub rendering: SourceRendering,
}

pub trait ReviewSource {
    /// Enforce max_bytes before allocation. No fallback to an unrestricted source reader.
    fn read(
        &mut self,
        request: &ReviewRequest,
        selector: &ReviewSourceSelector,
        max_bytes: usize,
    ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure>;
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RowExposure {
    Unread,
    Read,
    Prepared,
    PossiblyDisclosed,
    Acknowledged,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewCoverage {
    pub row_id: String,
    pub selector: ReviewSourceSelector,
    pub exposure: RowExposure,
    pub read_bytes: u64,
    pub source_observation: Option<ReviewSourceMetadata>,
    pub read_failure: Option<SourceReadFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewDecision {
    RequestRows {
        row_ids: Vec<String>,
    },
    Finish {
        findings: Vec<FindingClaim>,
        unresolved: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewUsage {
    pub basis: UsageBasis,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub prompt_details: Option<PromptTokenUsage>,
    pub completion_details: Option<CompletionTokenUsage>,
}

impl ReviewUsage {
    pub fn validate(&self) -> Result<()> {
        if self.input_tokens == 0
            || self.output_tokens == 0
            || self.input_tokens.checked_add(self.output_tokens) != Some(self.total_tokens)
            || self.prompt_details.as_ref().is_some_and(|u| {
                u.cached_tokens.is_some_and(|n| n > self.input_tokens)
                    || u.audio_tokens.is_some_and(|n| n != 0)
            })
            || self.completion_details.as_ref().is_some_and(|u| {
                u.reasoning_tokens.is_some_and(|n| n > self.output_tokens)
                    || u.audio_tokens.is_some_and(|n| n != 0)
                    || u.accepted_prediction_tokens
                        .is_some_and(|n| n > self.output_tokens)
                    || u.rejected_prediction_tokens
                        .is_some_and(|n| n > self.output_tokens)
                    || [
                        u.reasoning_tokens,
                        u.accepted_prediction_tokens,
                        u.rejected_prediction_tokens,
                    ]
                    .into_iter()
                    .flatten()
                    .try_fold(0u64, |sum, n| sum.checked_add(n))
                    .is_none_or(|sum| sum > self.output_tokens)
            })
        {
            return Err(Error::Invalid(
                "inconsistent or unsupported model usage report".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageBasis {
    Fixture,
    ServerReportedChatCompletionsV1,
    ClaudeCodeResultV1,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptTokenUsage {
    pub cached_tokens: Option<u64>,
    pub audio_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletionTokenUsage {
    pub reasoning_tokens: Option<u64>,
    pub audio_tokens: Option<u64>,
    pub accepted_prediction_tokens: Option<u64>,
    pub rejected_prediction_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelReply {
    pub response_id: String,
    pub model: String,
    pub usage: ReviewUsage,
    pub decision: ReviewDecision,
    pub content_sha256: String,
    pub system_fingerprint: Option<String>,
    pub service_tier: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportBasis {
    Fixture,
    LocalHttpUnqualified,
    ClaudeCode,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    NotStarted,
    Uncertain,
    Acknowledged,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailure {
    Cancelled,
    Deadline,
    Connection,
    HttpStatus,
    ResponseLimit,
    InvalidResponse,
    Identity,
    Usage,
    OutputLimit,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelCallObservation {
    pub state: CallState,
    pub basis: TransportBasis,
    pub reply: Option<ModelReply>,
    pub failure: Option<ModelFailure>,
    pub http_status: Option<u16>,
    pub captured_response_bytes: u64,
    pub captured_response_sha256: String,
    pub captured_response: Vec<u8>,
    pub response_capture_complete: bool,
}

impl ModelCallObservation {
    pub(crate) fn consistent_with(&self, call: &PreparedModelCall, basis: TransportBasis) -> bool {
        if self.basis != basis
            || self.captured_response.len() > call.max_response_bytes
            || self.captured_response_bytes != self.captured_response.len() as u64
            || self.captured_response_sha256 != crate::checks::sha256(&self.captured_response)
        {
            return false;
        }
        match self.state {
            CallState::NotStarted => {
                self.reply.is_none()
                    && matches!(
                        self.failure,
                        Some(
                            ModelFailure::Cancelled
                                | ModelFailure::Deadline
                                | ModelFailure::InvalidResponse
                        )
                    )
                    && self.http_status.is_none()
                    && self.captured_response.is_empty()
                    && !self.response_capture_complete
            }
            CallState::Uncertain => self.reply.is_none() && self.failure.is_some(),
            CallState::Acknowledged => {
                let Some(reply) = &self.reply else {
                    return false;
                };
                self.failure.is_none()
                    && self.response_capture_complete
                    && !self.captured_response.is_empty()
                    && (basis != TransportBasis::LocalHttpUnqualified
                        || self.http_status == Some(200))
                    && validate_identifier(&reply.response_id).is_ok()
                    && validate_digest(&reply.content_sha256).is_ok()
                    && reply.model == call.expected_model
                    && reply.usage.validate().is_ok()
                    && matches!(
                        (basis, reply.usage.basis),
                        (TransportBasis::Fixture, UsageBasis::Fixture)
                            | (
                                TransportBasis::LocalHttpUnqualified,
                                UsageBasis::ServerReportedChatCompletionsV1
                            )
                            | (TransportBasis::ClaudeCode, UsageBasis::ClaudeCodeResultV1)
                    )
                    && reply
                        .system_fingerprint
                        .as_ref()
                        .is_none_or(|v| v.len() <= 256)
                    && reply.service_tier.as_ref().is_none_or(|v| v.len() <= 64)
                    && match basis {
                        TransportBasis::LocalHttpUnqualified => super::wire::decode_reply(
                            &self.captured_response,
                            &call.expected_model,
                            call.max_output_tokens,
                        )
                        .is_ok_and(|decoded| decoded == *reply),
                        TransportBasis::Fixture => {
                            // Fixture captures use the full typed reply as their explicit
                            // synthetic envelope; content hashes the canonical decision.
                            serde_json::from_slice::<ModelReply>(&self.captured_response)
                                .is_ok_and(|decoded| decoded == *reply)
                                && serde_json::to_vec(&reply.decision).is_ok_and(|bytes| {
                                    crate::checks::sha256(&bytes) == reply.content_sha256
                                })
                        }
                        #[cfg(unix)]
                        TransportBasis::ClaudeCode => super::claude_code::decode_reply(
                            &self.captured_response,
                            &call.expected_model,
                            call.max_output_tokens,
                        )
                        .is_ok_and(|decoded| decoded == *reply),
                        #[cfg(not(unix))]
                        TransportBasis::ClaudeCode => false,
                    }
            }
        }
    }

    pub(crate) fn reject_inconsistent(&mut self, limit: usize) {
        // Derive bounded capture facts in the controller. An invalid transport
        // report cannot establish complete capture, non-disclosure or usage truth.
        self.captured_response =
            self.captured_response[..self.captured_response.len().min(limit)].to_vec();
        self.captured_response_bytes = self.captured_response.len() as u64;
        self.captured_response_sha256 = crate::checks::sha256(&self.captured_response);
        self.response_capture_complete = false;
        self.state = CallState::Uncertain;
        self.failure = Some(ModelFailure::InvalidResponse);
        self.reply = None;
    }

    pub(crate) fn not_started(basis: TransportBasis, failure: ModelFailure) -> Self {
        Self {
            state: CallState::NotStarted,
            basis,
            reply: None,
            failure: Some(failure),
            http_status: None,
            captured_response_bytes: 0,
            captured_response_sha256: crate::checks::sha256(&[]),
            captured_response: Vec::new(),
            response_capture_complete: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedModelCall {
    pub(crate) body: Vec<u8>,
    pub(crate) expected_model: String,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_output_tokens: u32,
    pub(crate) timeout_ms: u32,
}

impl PreparedModelCall {
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn request_sha256(&self) -> String {
        crate::checks::sha256(&self.body)
    }
    pub fn timeout_ms(&self) -> u32 {
        self.timeout_ms
    }
}

/// Trusted finite adapter. Construction, polling and destruction of invoke futures
/// must not block. The controller bounds a pending future, but cannot preempt blocking code.
/// Dropping a future never proves remote cancellation or absence of disclosure.
pub trait ModelTransport {
    fn scope_sha256(&self) -> String;
    fn basis(&self) -> TransportBasis;
    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> Pin<Box<dyn Future<Output = ModelCallObservation> + Send + 'a>>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallReservation {
    pub attempt_id: String,
    pub run_generation: u64,
    pub call_index: u32,
    pub subject: CheckSubject,
    pub provider_scope_sha256: String,
    pub model: String,
    pub transport_basis: TransportBasis,
    pub qualification: ReviewQualification,
    pub review_request_sha256: String,
    pub request_sha256: String,
    pub request_body: Vec<u8>,
    pub source_row_ids: Vec<String>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u32,
    pub max_cost_micros: Option<u64>,
}

impl CallReservation {
    pub fn digest(&self) -> Result<String> {
        domain_digest("review-call-reservation/v1", self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CallTicket {
    pub reservation_sha256: String,
    pub run_generation: u64,
    pub durable_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "state",
    content = "ticket",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReservationOutcome {
    Reserved(CallTicket),
    Existing,
}

pub trait ReviewJournal {
    /// C atomically reserves aggregate budget and journals possible exposure before returning.
    /// C must also enforce current authorization, disclosure policy, leases and required
    /// transport qualifications/caps before allowing any call. This library supplies no
    /// verified live transport. A diagnostic reservation grants no admission authority.
    /// A crash or uncertain call retains the reservation; it is not automatically refunded.
    /// Return Reserved only for a new atomically acquired dispatch. Repeated keys
    /// return Existing, even if the earlier dispatch outcome is unknown. They must
    /// never grant a second dispatch merely because the caller lost its reply.
    fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome>;
    /// C uses generation/version CAS and rejects late finishers. An uncertain result is retained.
    fn record_call(
        &mut self,
        ticket: &CallTicket,
        observation: &ModelCallObservation,
    ) -> Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordedModelCall {
    pub reservation: CallReservation,
    pub ticket: CallTicket,
    pub observation: ModelCallObservation,
    pub journal_recorded: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCompletion {
    FinishedObservations,
    Incomplete,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewQualification {
    Unverified,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStop {
    ModelFinished,
    RequiredScopeUnvisited,
    UnresolvedScope,
    CallsExhausted,
    SourceRead,
    SourceMismatch,
    SourceRendering,
    SourceLimit,
    RequestLimit,
    InvalidDecision,
    InvalidFinding,
    Journal,
    ExistingReservation,
    Transport,
    TokenLimit,
    Deadline,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub identity: FindingIdentity,
    pub claim: FindingClaim,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewRecord {
    pub version: u32,
    pub request_sha256: String,
    pub request: ReviewRequest,
    pub completion: ReviewCompletion,
    pub stop: ReviewStop,
    /// Hard token/cost caps, server provenance and real usefulness are not qualified here.
    pub qualification: ReviewQualification,
    pub coverage: Vec<ReviewCoverage>,
    pub calls: Vec<RecordedModelCall>,
    /// A reservation may have persisted even when its reply failed validation or was lost.
    pub unconfirmed_reservation: Option<CallReservation>,
    pub findings: Vec<ReviewFinding>,
    pub unresolved: Vec<String>,
}

pub(crate) fn domain_digest<T: Serialize>(domain: &str, value: &T) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(value)?);
    Ok(format!("{:x}", digest.finalize()))
}

pub(crate) fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("expected lowercase SHA-256 digest".into()));
    }
    Ok(())
}

pub(crate) fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
    {
        return Err(Error::Invalid("invalid model review identifier".into()));
    }
    Ok(())
}
