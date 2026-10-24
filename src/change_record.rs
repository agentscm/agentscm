//! Versioned, bounded contracts for local capture and source-bound action receipts.
//!
//! These types deliberately contain no conversation text, commands, source paths or process
//! output. Conversation bodies remain in the local capture store until the shared-retention
//! privacy lifecycle is implemented and qualified.

use serde::{Deserialize, Serialize};

use crate::hub::{HubError, Result};

pub const SCHEMA_VERSION_V1: u16 = 1;
pub const MAX_EVENTS_PER_BATCH: usize = 100;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
pub const MAX_ARTIFACTS_PER_ACTION: usize = 16;
pub const MAX_GAPS_PER_SESSION: usize = 32;
pub const MAX_CHANGE_RECORD_RESPONSE_BYTES: usize = 48 * 1024;
pub const MAX_PROJECTED_SCOPE: usize = 16;
pub const MAX_PROJECTED_DECISION_REFS: usize = 16;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarnessV1 {
    ClaudeCode,
    Codex,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureModeV1 {
    MetadataOnly,
    LocalConversation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionIdentityV1 {
    pub harness: AgentHarnessV1,
    pub adapter_version: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub capture_mode: CaptureModeV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapturedEventKindV1 {
    SessionStarted,
    UserPrompt,
    AssistantMessage,
    ToolStarted,
    ToolFinished,
    CheckStarted,
    CheckFinished,
    SessionEnded,
    Gap,
    TurnEnded,
    Compaction,
    SubagentStarted,
    SubagentStopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKindV1 {
    Read,
    Edit,
    Write,
    Search,
    Shell,
    Test,
    Model,
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolFactV1 {
    pub kind: ToolKindV1,
    /// Opaque provider request identity, restricted to identifier characters.
    pub request_id: Option<String>,
    /// Raw host tool name (for example `mcp__server__tool`), bounded to 128 identifier bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the host reported when the tool finished. Absent on start events and when unobserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<ToolCompletionV1>,
}

/// A host completion hook proves the call ended, not that it achieved anything.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCompletionV1 {
    Returned,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubagentFactV1 {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactMediaV1 {
    Json,
    Text,
    JsonLines,
    Binary,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClassV1 {
    EphemeralLocal,
    WorkLocal,
    SharedApproved,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentAvailabilityV1 {
    Local,
    Shared,
    External,
    Missing,
    Expired,
    Deleted,
    Denied,
    IntegrityFailure,
    Mixed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SetupComponentV1 {
    Model,
    Instructions,
    Skills,
    Tools,
    Permissions,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SetupObservationV1 {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub effort: Option<String>,
    /// Opaque identity of the sanitized native setup facts retained in the private spool.
    pub artifact: ArtifactRefV1,
    /// Components the adapter could not observe at this event boundary.
    pub unknown_components: Vec<SetupComponentV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRefV1 {
    /// SHA-256 of the retained bytes. It is an identity, not evidence that content is available.
    pub digest: String,
    pub size_bytes: u64,
    pub media: ArtifactMediaV1,
    pub retention: RetentionClassV1,
    pub availability: ContentAvailabilityV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GapReasonV1 {
    ProviderHistoryUnavailable,
    AdapterStartedLate,
    IdentityUncertain,
    SequenceMissing,
    LocalRetentionExpired,
    Redacted,
    CaptureInterrupted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GapFactV1 {
    pub first_missing: u64,
    pub last_missing: u64,
    pub reason: GapReasonV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapturedEventV1 {
    pub event_id: String,
    pub sequence: u64,
    pub observed_at: i64,
    pub kind: CapturedEventKindV1,
    pub tool: Option<ToolFactV1>,
    /// Opaque content identity only. The Hub never receives a local path or transcript body.
    pub artifact: Option<ArtifactRefV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupObservationV1>,
    pub gap: Option<GapFactV1>,
    /// Allowlisted session boundary: SessionStart `source` or SessionEnd `reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentFactV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureBatchV1 {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: String,
    pub generation: i64,
    pub session: SessionIdentityV1,
    pub events: Vec<CapturedEventV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "record", rename_all = "snake_case")]
// Keep the version-1 value-shaped DTO stable at the capture/store boundary. Boxing only the
// larger arm would add ownership churn to every validator and spool caller without changing the
// bounded wire representation.
#[allow(clippy::large_enum_variant)]
pub enum CaptureEnvelopeV1 {
    EventBatch(CaptureBatchV1),
    ActionStart(ActionStartV1),
    ActionFinish(ActionFinishV1),
    Usage(crate::hub::insights::UsageRecord),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureOmissionV1 {
    UntrackedContent,
    IgnoredContent,
    WorkingTreeContent,
    IndexState,
    SubmoduleContent,
    SymlinkTarget,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceBindingV1 {
    Native {
        repository: String,
        revision: String,
        change_id: String,
        context_digest: String,
        source_generation: u64,
    },
    Git {
        repository: String,
        worktree_id: String,
        head: String,
        tree: String,
        base: String,
        dirty: bool,
        untracked_count: u32,
        omissions: Vec<CaptureOmissionV1>,
        /// Content-addressed manifest of the captured working source, never a host path.
        captured_snapshot: Option<ArtifactRefV1>,
        snapshot_coverage: CoverageV1,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionKindV1 {
    Execution,
    Verification,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeProvenanceV1 {
    ObservedNativeInput,
    ConfiguredExactMatch,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionStartV1 {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
    pub action_id: String,
    pub source: SourceBindingV1,
    pub kind: ActionKindV1,
    pub recipe_provenance: RecipeProvenanceV1,
    pub recipe_digest: String,
    pub setup_digest: String,
    /// Durable caller-local causal order. Zero means the adapter could not establish one.
    #[serde(default)]
    pub observed_order: u64,
    /// Opaque durable identity for the local observation ledger. Absent means incomparable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_scope_id: Option<String>,
    pub started_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckResultV1 {
    Passed,
    Failed,
    Incomplete,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplicabilityV1 {
    Current,
    Stale,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssuranceBasisV1 {
    CallerAsserted,
    ObservedTrustedLocal,
    ProtectedExecution,
    IndependentVerification,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoverageV1 {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriftV1 {
    NoneObserved,
    SourceChanged,
    SetupChanged,
    RecipeChanged,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcomeV1 {
    Succeeded,
    Failed,
    Incomplete,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionFinishV1 {
    pub schema_version: u16,
    pub operation_id: String,
    pub work_id: String,
    pub generation: i64,
    pub action_id: String,
    pub outcome: ActionOutcomeV1,
    pub result: CheckResultV1,
    pub assurance: AssuranceBasisV1,
    pub coverage: CoverageV1,
    pub finished_at: i64,
    pub drift: DriftV1,
    pub output_artifacts: Vec<ArtifactRefV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IngestReceiptV1 {
    pub work_id: String,
    pub operation_id: String,
    pub accepted: usize,
    pub duplicate: usize,
    pub watermark: u64,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionMutationReceiptV1 {
    pub work_id: String,
    pub operation_id: String,
    pub action_id: String,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKindV1 {
    Native,
    Git,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubjectSummaryV1 {
    pub source_id: String,
    pub source_kind: SubjectKindV1,
    /// Inspectable typed source facts. Artifact references remain opaque and contain no host path.
    pub source: SourceBindingV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionCoverageV1 {
    pub count: usize,
    pub coverage: CoverageV1,
    pub gaps: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionSourceRefV1 {
    RepositoryPath {
        path: String,
        line: Option<u32>,
    },
    Commit {
        oid: String,
        path: Option<String>,
    },
    Work {
        work_id: String,
    },
    CapturedEvent {
        session_id: String,
        event_id: String,
    },
    PriorWorkVersion {
        work_id: String,
        event_seq: i64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntentSummaryV1 {
    /// Existing bounded Work title; it is not derived from conversation content.
    pub title: String,
    pub scope: Vec<String>,
    pub scope_omitted: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionSummaryV1 {
    pub action_id: String,
    pub source_id: String,
    pub kind: ActionKindV1,
    pub outcome: ActionOutcomeV1,
    pub recipe_provenance: RecipeProvenanceV1,
    pub recipe_digest: String,
    pub setup_digest: String,
    pub result: CheckResultV1,
    pub applicability: ApplicabilityV1,
    pub assurance: AssuranceBasisV1,
    pub coverage: CoverageV1,
    pub evidence_availability: ContentAvailabilityV1,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub drift: DriftV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatusV1 {
    Missing,
    Proposed,
    Approved,
    Rejected,
    Superseded,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAuthorKindV1 {
    Agent,
    Human,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DecisionSummaryV1 {
    pub status: DecisionStatusV1,
    /// Opaque identity of an existing authoritative decision record.
    pub reference: Option<String>,
    /// Existing bounded Work checkpoint summary; never synthesized from a transcript.
    pub summary: Option<String>,
    pub source_refs: Vec<DecisionSourceRefV1>,
    pub source_refs_omitted: usize,
    pub author_kind: Option<DecisionAuthorKindV1>,
    pub author_agent: Option<String>,
    pub at: Option<i64>,
    pub generation: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletenessV1 {
    pub events: CoverageV1,
    pub source: CoverageV1,
    pub setup: CoverageV1,
    pub recipe: CoverageV1,
    pub result: CoverageV1,
    pub usage: CoverageV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnknownFactV1 {
    CaptureCoverage,
    SourceApplicability,
    SetupIdentity,
    RecipeIdentity,
    Result,
    Usage,
    HumanDecision,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeRecordSummaryV1 {
    pub schema_version: u16,
    pub work_id: String,
    pub generation: i64,
    pub as_of: i64,
    pub intent: IntentSummaryV1,
    pub subject: Option<SubjectSummaryV1>,
    pub sessions: SessionCoverageV1,
    pub latest_action: Option<ActionSummaryV1>,
    pub decision: DecisionSummaryV1,
    /// Existing bounded work checkpoint field, not transcript-derived content.
    pub next_action: Option<String>,
    pub completeness: CompletenessV1,
    pub unknowns: Vec<UnknownFactV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordAvailabilityV1 {
    Available,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeRecordResponseV1 {
    pub version: u16,
    pub availability: RecordAvailabilityV1,
    pub summary: ChangeRecordSummaryV1,
}

impl ChangeRecordResponseV1 {
    pub fn bounded(summary: ChangeRecordSummaryV1) -> Result<Self> {
        let mut response = Self {
            version: SCHEMA_VERSION_V1,
            availability: RecordAvailabilityV1::Available,
            summary,
        };
        while serde_json::to_vec(&response)
            .map_err(|_| HubError::Invalid("change record response is invalid".into()))?
            .len()
            > MAX_CHANGE_RECORD_RESPONSE_BYTES
        {
            let scope_bytes = response
                .summary
                .intent
                .scope
                .last()
                .map(|value| serde_json::to_vec(value).map(|bytes| bytes.len()))
                .transpose()
                .map_err(|_| HubError::Invalid("change record response is invalid".into()))?;
            let reference_bytes = response
                .summary
                .decision
                .source_refs
                .last()
                .map(|value| serde_json::to_vec(value).map(|bytes| bytes.len()))
                .transpose()
                .map_err(|_| HubError::Invalid("change record response is invalid".into()))?;
            match (scope_bytes, reference_bytes) {
                (Some(scope), Some(reference)) if scope >= reference => {
                    response.summary.intent.scope.pop();
                    response.summary.intent.scope_omitted += 1;
                }
                (_, Some(_)) => {
                    response.summary.decision.source_refs.pop();
                    response.summary.decision.source_refs_omitted += 1;
                }
                (Some(_), None) => {
                    response.summary.intent.scope.pop();
                    response.summary.intent.scope_omitted += 1;
                }
                (None, None) => {
                    return Err(HubError::Invalid(
                        "change record core exceeds the bounded projection".into(),
                    ));
                }
            }
        }
        Ok(response)
    }

    pub fn validate_encoded_size(&self) -> Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| HubError::Invalid("change record response is invalid".into()))?;
        if bytes.len() > MAX_CHANGE_RECORD_RESPONSE_BYTES {
            return Err(HubError::Invalid(
                "change record response exceeds the bounded projection".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureTargetV1 {
    pub version: u16,
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
}

impl CaptureTargetV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != SCHEMA_VERSION_V1 {
            return Err(invalid("unsupported capture target version"));
        }
        validate_work(&self.work_id, self.generation)?;
        identifier("session_id", &self.session_id, 255)
    }
}

impl CaptureEnvelopeV1 {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::EventBatch(batch) => batch.validate(),
            Self::ActionStart(start) => start.validate(),
            Self::ActionFinish(finish) => finish.validate(),
            Self::Usage(usage) => usage.validate(),
        }
    }
}

impl CaptureBatchV1 {
    pub fn validate(&self) -> Result<()> {
        validate_schema(self.schema_version)?;
        validate_operation_id(&self.operation_id)?;
        validate_work(&self.work_id, self.generation)?;
        self.session.validate()?;
        if self.events.is_empty() || self.events.len() > MAX_EVENTS_PER_BATCH {
            return Err(invalid("capture batch must contain 1..100 events"));
        }
        let mut previous = None;
        let mut gaps = 0;
        for event in &self.events {
            event.validate()?;
            if previous.is_some_and(|sequence| event.sequence <= sequence) {
                return Err(invalid(
                    "capture events must have increasing sequence numbers",
                ));
            }
            previous = Some(event.sequence);
            gaps += usize::from(event.gap.is_some());
        }
        if gaps > MAX_GAPS_PER_SESSION {
            return Err(invalid("capture batch has too many gap records"));
        }
        if serde_json::to_vec(self)
            .map_err(|_| invalid("capture batch could not be encoded"))?
            .len()
            > MAX_BATCH_BYTES
        {
            return Err(invalid("capture batch exceeds 256 KiB"));
        }
        Ok(())
    }
}

impl SessionIdentityV1 {
    fn validate(&self) -> Result<()> {
        identifier("adapter_version", &self.adapter_version, 64)?;
        identifier("session_id", &self.session_id, 255)?;
        if let Some(parent) = &self.parent_session_id {
            identifier("parent_session_id", parent, 255)?;
            if parent == &self.session_id {
                return Err(invalid("parent session must differ from session"));
            }
        }
        Ok(())
    }
}

impl CapturedEventV1 {
    fn validate(&self) -> Result<()> {
        identifier("event_id", &self.event_id, 255)?;
        if self.sequence == 0 || self.observed_at < 0 {
            return Err(invalid(
                "event sequence must be positive and time non-negative",
            ));
        }
        if let Some(tool) = &self.tool
            && let Some(request_id) = &tool.request_id
        {
            identifier("tool request_id", request_id, 255)?;
        }
        if let Some(tool) = &self.tool
            && let Some(name) = &tool.name
        {
            identifier("tool name", name, 128)?;
        }
        if let Some(subagent) = &self.subagent {
            identifier("subagent agent_id", &subagent.agent_id, 255)?;
            if let Some(agent_type) = &subagent.agent_type {
                identifier("subagent agent_type", agent_type, 128)?;
            }
        }
        if let Some(boundary) = &self.boundary
            && !matches!(
                (self.kind, boundary.as_str()),
                (
                    CapturedEventKindV1::SessionStarted,
                    "startup" | "resume" | "clear" | "compact"
                ) | (
                    CapturedEventKindV1::SessionEnded,
                    "clear" | "logout" | "prompt_input_exit" | "other"
                )
            )
        {
            return Err(invalid("event boundary is not allowlisted for its kind"));
        }
        if matches!(
            self.kind,
            CapturedEventKindV1::SubagentStarted | CapturedEventKindV1::SubagentStopped
        ) != self.subagent.is_some()
        {
            return Err(invalid(
                "subagent facts must appear exactly on subagent events",
            ));
        }
        if self
            .tool
            .as_ref()
            .is_some_and(|tool| tool.completion.is_some())
            && !matches!(
                self.kind,
                CapturedEventKindV1::ToolFinished | CapturedEventKindV1::CheckFinished
            )
        {
            return Err(invalid("tool completion must appear on finish events"));
        }
        let tool_event = matches!(
            self.kind,
            CapturedEventKindV1::ToolStarted
                | CapturedEventKindV1::ToolFinished
                | CapturedEventKindV1::CheckStarted
                | CapturedEventKindV1::CheckFinished
        );
        if tool_event != self.tool.is_some() {
            return Err(invalid("tool facts must appear exactly on tool events"));
        }
        if let Some(artifact) = &self.artifact {
            artifact.validate()?;
            if artifact.retention == RetentionClassV1::SharedApproved {
                return Err(invalid(
                    "shared artifact retention is unavailable in schema v1",
                ));
            }
        }
        if let Some(setup) = &self.setup {
            setup.validate()?;
        }
        if let Some(gap) = &self.gap
            && (gap.first_missing == 0 || gap.last_missing < gap.first_missing)
        {
            return Err(invalid("event gap range is invalid"));
        }
        if matches!(self.kind, CapturedEventKindV1::Gap) != self.gap.is_some() {
            return Err(invalid("gap facts must appear exactly on gap events"));
        }
        Ok(())
    }
}

impl SetupObservationV1 {
    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("setup provider", self.provider.as_deref()),
            ("setup model", self.model.as_deref()),
            ("setup permission_mode", self.permission_mode.as_deref()),
            ("setup effort", self.effort.as_deref()),
        ] {
            if let Some(value) = value {
                identifier(name, value, 255)?;
            }
        }
        self.artifact.validate()?;
        if self.artifact.retention == RetentionClassV1::SharedApproved {
            return Err(invalid(
                "shared setup artifact retention is unavailable in schema v1",
            ));
        }
        if self.unknown_components.len() > 5
            || self
                .unknown_components
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid(
                "setup unknown components must be unique and ordered",
            ));
        }
        Ok(())
    }
}

impl ArtifactRefV1 {
    fn validate(&self) -> Result<()> {
        digest("artifact digest", &self.digest)?;
        if self.size_bytes > 1024 * 1024 * 1024 {
            return Err(invalid("artifact size exceeds 1 GiB"));
        }
        Ok(())
    }
}

impl ActionStartV1 {
    pub fn validate(&self) -> Result<()> {
        validate_schema(self.schema_version)?;
        validate_operation_id(&self.operation_id)?;
        validate_work(&self.work_id, self.generation)?;
        identifier("session_id", &self.session_id, 255)?;
        identifier("action_id", &self.action_id, 255)?;
        self.source.validate()?;
        digest("recipe_digest", &self.recipe_digest)?;
        digest("setup_digest", &self.setup_digest)?;
        if self.started_at < 0 {
            return Err(invalid("action start time must be non-negative"));
        }
        if self.observed_order > i64::MAX as u64 {
            return Err(invalid("action observed order exceeds storage range"));
        }
        if let Some(scope) = &self.observation_scope_id {
            identifier("observation_scope_id", scope, 255)?;
        }
        Ok(())
    }
}

impl SourceBindingV1 {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Native {
                repository,
                revision,
                change_id,
                context_digest,
                source_generation,
            } => {
                repository_identifier(repository)?;
                digest("native revision", revision)?;
                identifier("change_id", change_id, 255)?;
                digest("context_digest", context_digest)?;
                if *source_generation == 0 {
                    return Err(invalid("native source generation must be positive"));
                }
            }
            Self::Git {
                repository,
                worktree_id,
                head,
                tree,
                base,
                dirty,
                untracked_count,
                omissions,
                captured_snapshot,
                snapshot_coverage,
                ..
            } => {
                repository_identifier(repository)?;
                identifier("worktree_id", worktree_id, 255)?;
                oid("head", head)?;
                oid("tree", tree)?;
                oid("base", base)?;
                if omissions.len() > 8 {
                    return Err(invalid("source binding has too many omissions"));
                }
                if let Some(snapshot) = captured_snapshot {
                    snapshot.validate()?;
                    if snapshot.retention == RetentionClassV1::SharedApproved {
                        return Err(invalid(
                            "shared artifact retention is unavailable in schema v1",
                        ));
                    }
                }
                if (*dirty || *untracked_count > 0)
                    && *snapshot_coverage == CoverageV1::Complete
                    && captured_snapshot.is_none()
                {
                    return Err(invalid(
                        "complete dirty Git source requires a retained captured snapshot",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn stable_id(&self) -> Result<String> {
        let encoded =
            serde_json::to_vec(self).map_err(|_| invalid("source binding could not be encoded"))?;
        Ok(crate::hub::store::lower_hex_of(&encoded))
    }

    pub fn kind(&self) -> SubjectKindV1 {
        match self {
            Self::Native { .. } => SubjectKindV1::Native,
            Self::Git { .. } => SubjectKindV1::Git,
        }
    }

    pub fn repository(&self) -> &str {
        match self {
            Self::Native { repository, .. } | Self::Git { repository, .. } => repository,
        }
    }

    pub fn coverage(&self) -> CoverageV1 {
        match self {
            Self::Native { .. } => CoverageV1::Complete,
            Self::Git {
                dirty,
                untracked_count,
                omissions,
                captured_snapshot,
                snapshot_coverage,
                ..
            } => {
                if !dirty && *untracked_count == 0 && omissions.is_empty() {
                    CoverageV1::Complete
                } else if captured_snapshot.is_some() {
                    *snapshot_coverage
                } else {
                    CoverageV1::Partial
                }
            }
        }
    }
}

impl ActionFinishV1 {
    pub fn validate(&self) -> Result<()> {
        validate_schema(self.schema_version)?;
        validate_operation_id(&self.operation_id)?;
        validate_work(&self.work_id, self.generation)?;
        identifier("action_id", &self.action_id, 255)?;
        if self.finished_at < 0 || self.output_artifacts.len() > MAX_ARTIFACTS_PER_ACTION {
            return Err(invalid(
                "action finish time or artifact count is out of bounds",
            ));
        }
        let consistent = match self.result {
            CheckResultV1::Passed => self.outcome == ActionOutcomeV1::Succeeded,
            CheckResultV1::Failed => self.outcome == ActionOutcomeV1::Failed,
            CheckResultV1::Incomplete => matches!(
                self.outcome,
                ActionOutcomeV1::Failed | ActionOutcomeV1::Incomplete
            ),
            CheckResultV1::Unknown => true,
        };
        if !consistent {
            return Err(invalid(
                "action outcome and verification result are inconsistent",
            ));
        }
        for artifact in &self.output_artifacts {
            artifact.validate()?;
            if artifact.retention == RetentionClassV1::SharedApproved {
                return Err(invalid(
                    "shared artifact retention is unavailable in schema v1",
                ));
            }
        }
        Ok(())
    }
}

fn validate_schema(version: u16) -> Result<()> {
    if version != SCHEMA_VERSION_V1 {
        return Err(invalid("unsupported change record schema version"));
    }
    Ok(())
}

fn validate_operation_id(value: &str) -> Result<()> {
    identifier("operation_id", value, 255)
}

fn validate_work(work_id: &str, generation: i64) -> Result<()> {
    if !work_id.starts_with("wrk_") {
        return Err(invalid("work_id must be a durable work identity"));
    }
    identifier("work_id", work_id, 64)?;
    if generation < 1 {
        return Err(invalid("generation must be positive"));
    }
    Ok(())
}

fn identifier(name: &str, value: &str, max: usize) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        });
    if !valid {
        return Err(invalid(&format!(
            "{name} must be 1..{max} bytes of identifier characters"
        )));
    }
    Ok(())
}

fn repository_identifier(value: &str) -> Result<()> {
    let valid = value.len() <= 255
        && value.split('/').count() >= 2
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    if !valid {
        return Err(invalid("repository must be a bounded owner/name identity"));
    }
    Ok(())
}

fn digest(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(&format!("{name} must be a lowercase SHA-256")));
    }
    Ok(())
}

fn oid(name: &str, value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(&format!(
            "{name} must be a full lowercase Git object ID"
        )));
    }
    Ok(())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> SessionIdentityV1 {
        SessionIdentityV1 {
            harness: AgentHarnessV1::Codex,
            adapter_version: "1.0.0".into(),
            session_id: "session-1".into(),
            parent_session_id: None,
            capture_mode: CaptureModeV1::LocalConversation,
        }
    }

    fn summary() -> ChangeRecordSummaryV1 {
        serde_json::from_value(json!({
            "schema_version": 1,
            "work_id": "wrk_0123456789abcdef0123456789abcdef",
            "generation": 1,
            "as_of": 1,
            "intent": {"title": "bounded handoff", "scope": [], "scope_omitted": 0},
            "subject": null,
            "sessions": {"count": 0, "coverage": "unknown", "gaps": 0},
            "latest_action": null,
            "decision": {
                "status": "missing",
                "reference": null,
                "summary": null,
                "source_refs": [],
                "source_refs_omitted": 0,
                "author_kind": null,
                "author_agent": null,
                "at": null,
                "generation": null
            },
            "next_action": null,
            "completeness": {
                "events": "unknown",
                "source": "unknown",
                "setup": "unknown",
                "recipe": "unknown",
                "result": "unknown",
                "usage": "unknown"
            },
            "unknowns": []
        }))
        .unwrap()
    }

    fn pad_to_single_removal(summary: &mut ChangeRecordSummaryV1, removed_bytes: usize) {
        for size in 0..MAX_CHANGE_RECORD_RESPONSE_BYTES {
            summary.next_action = Some("n".repeat(size));
            let response = ChangeRecordResponseV1 {
                version: SCHEMA_VERSION_V1,
                availability: RecordAvailabilityV1::Available,
                summary: summary.clone(),
            };
            let encoded = serde_json::to_vec(&response).unwrap().len();
            if encoded > MAX_CHANGE_RECORD_RESPONSE_BYTES
                && encoded - removed_bytes <= MAX_CHANGE_RECORD_RESPONSE_BYTES
            {
                return;
            }
        }
        panic!("could not construct a response requiring exactly one removal");
    }

    #[test]
    fn rejects_shared_retention_and_unstructured_identifiers() {
        let mut batch = CaptureBatchV1 {
            schema_version: 1,
            operation_id: "upload-1".into(),
            work_id: "wrk_0123456789abcdef0123456789abcdef".into(),
            generation: 1,
            session: session(),
            events: vec![CapturedEventV1 {
                event_id: "event-1".into(),
                sequence: 1,
                observed_at: 1,
                kind: CapturedEventKindV1::AssistantMessage,
                tool: None,
                artifact: Some(ArtifactRefV1 {
                    digest: "a".repeat(64),
                    size_bytes: 10,
                    media: ArtifactMediaV1::JsonLines,
                    retention: RetentionClassV1::SharedApproved,
                    availability: ContentAvailabilityV1::Local,
                }),
                setup: None,
                gap: None,
                boundary: None,
                subagent: None,
            }],
        };
        assert!(batch.validate().is_err());
        batch.events[0].artifact = None;
        batch.events[0].event_id = "/private/tmp/transcript.jsonl".into();
        assert!(batch.validate().is_err());
    }

    fn batch_of(event: serde_json::Value) -> CaptureBatchV1 {
        serde_json::from_value(json!({
            "schema_version": 1,
            "operation_id": "upload-1",
            "work_id": "wrk_0123456789abcdef0123456789abcdef",
            "generation": 1,
            "session": serde_json::to_value(session()).unwrap(),
            "events": [event],
        }))
        .unwrap()
    }

    #[test]
    fn old_event_shapes_deserialize_and_new_fields_stay_off_the_wire() {
        let batch = batch_of(json!({
            "event_id": "event-1", "sequence": 1, "observed_at": 1,
            "kind": "tool_finished",
            "tool": {"kind": "read", "request_id": "toolu-1"},
            "artifact": null, "gap": null,
        }));
        batch.validate().unwrap();
        let event = &batch.events[0];
        assert!(event.boundary.is_none() && event.subagent.is_none());
        let tool = event.tool.as_ref().unwrap();
        assert!(tool.name.is_none() && tool.completion.is_none());
        let encoded = serde_json::to_string(&batch).unwrap();
        for field in ["boundary", "subagent", "completion", "\"name\""] {
            assert!(!encoded.contains(field), "{field} leaked: {encoded}");
        }
    }

    #[test]
    fn new_event_shapes_round_trip_and_are_bounded() {
        let tool = batch_of(json!({
            "event_id": "event-1", "sequence": 1, "observed_at": 1,
            "kind": "tool_finished",
            "tool": {"kind": "other", "request_id": null,
                     "name": "mcp__server__tool", "completion": "returned"},
            "artifact": null, "gap": null,
        }));
        tool.validate().unwrap();
        let tool_again: CaptureBatchV1 =
            serde_json::from_slice(&serde_json::to_vec(&tool).unwrap()).unwrap();
        assert_eq!(tool, tool_again);
        assert_eq!(
            tool.events[0].tool.as_ref().unwrap().completion,
            Some(ToolCompletionV1::Returned)
        );

        let mut overlong = tool.clone();
        overlong.events[0].tool.as_mut().unwrap().name = Some("n".repeat(129));
        assert!(overlong.validate().is_err());
        let mut started = tool.clone();
        started.events[0].kind = CapturedEventKindV1::ToolStarted;
        assert!(
            started.validate().is_err(),
            "completion needs a finish event"
        );

        let subagent = batch_of(json!({
            "event_id": "event-2", "sequence": 1, "observed_at": 1,
            "kind": "subagent_started", "tool": null, "artifact": null, "gap": null,
            "subagent": {"agent_id": "agent-1", "agent_type": "Explore"},
        }));
        subagent.validate().unwrap();
        let mut bad = subagent.clone();
        bad.events[0].subagent.as_mut().unwrap().agent_id = "a b".into();
        assert!(bad.validate().is_err());
        let mut missing = subagent;
        missing.events[0].subagent = None;
        assert!(missing.validate().is_err());

        let turn_ended = |kind: &str, boundary: Option<&str>| {
            batch_of(json!({
                "event_id": "event-3", "sequence": 1, "observed_at": 1,
                "kind": kind, "tool": null, "artifact": null, "gap": null,
                "boundary": boundary,
            }))
        };
        turn_ended("turn_ended", None).validate().unwrap();
        turn_ended("compaction", None).validate().unwrap();
        turn_ended("session_started", Some("compact"))
            .validate()
            .unwrap();
        turn_ended("session_ended", Some("other"))
            .validate()
            .unwrap();
        assert!(
            turn_ended("session_started", Some("logout"))
                .validate()
                .is_err()
        );
        assert!(
            turn_ended("session_ended", Some("/tmp/x"))
                .validate()
                .is_err()
        );
        assert!(
            turn_ended("turn_ended", Some("startup"))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn requires_strict_event_order() {
        let event = |id: &str, sequence| CapturedEventV1 {
            event_id: id.into(),
            sequence,
            observed_at: 1,
            kind: CapturedEventKindV1::ToolStarted,
            tool: Some(ToolFactV1 {
                kind: ToolKindV1::Test,
                request_id: Some(format!("request-{sequence}")),
                name: None,
                completion: None,
            }),
            artifact: None,
            setup: None,
            gap: None,
            boundary: None,
            subagent: None,
        };
        let batch = CaptureBatchV1 {
            schema_version: 1,
            operation_id: "upload-1".into(),
            work_id: "wrk_0123456789abcdef0123456789abcdef".into(),
            generation: 1,
            session: session(),
            events: vec![event("event-2", 2), event("event-1", 1)],
        };
        assert!(batch.validate().is_err());
    }

    #[test]
    fn bounded_response_removes_larger_trailing_item_and_counts_it() {
        let mut summary = summary();
        summary.intent.scope = vec!["s".repeat(1_024)];
        summary.decision.source_refs = vec![DecisionSourceRefV1::Work {
            work_id: "w".repeat(64),
        }];
        let removed_bytes = serde_json::to_vec(summary.intent.scope.last().unwrap())
            .unwrap()
            .len()
            + 1;
        pad_to_single_removal(&mut summary, removed_bytes);

        let response = ChangeRecordResponseV1::bounded(summary).unwrap();
        assert!(response.summary.intent.scope.is_empty());
        assert_eq!(response.summary.intent.scope_omitted, 1);
        assert_eq!(response.summary.decision.source_refs.len(), 1);
        assert_eq!(response.summary.decision.source_refs_omitted, 0);
        response.validate_encoded_size().unwrap();
    }

    #[test]
    fn bounded_response_breaks_equal_size_ties_by_removing_scope() {
        let mut summary = summary();
        let reference = DecisionSourceRefV1::Work {
            work_id: "w".repeat(64),
        };
        let reference_bytes = serde_json::to_vec(&reference).unwrap().len();
        summary.intent.scope = vec!["s".repeat(reference_bytes - 2)];
        summary.decision.source_refs = vec![reference];
        assert_eq!(
            serde_json::to_vec(summary.intent.scope.last().unwrap())
                .unwrap()
                .len(),
            reference_bytes
        );
        pad_to_single_removal(&mut summary, reference_bytes + 1);

        let response = ChangeRecordResponseV1::bounded(summary).unwrap();
        assert!(response.summary.intent.scope.is_empty());
        assert_eq!(response.summary.intent.scope_omitted, 1);
        assert_eq!(response.summary.decision.source_refs.len(), 1);
        assert_eq!(response.summary.decision.source_refs_omitted, 0);
        response.validate_encoded_size().unwrap();
    }

    #[test]
    fn bounded_response_refuses_an_oversized_semantic_core() {
        let mut summary = summary();
        summary.intent.title = "t".repeat(MAX_CHANGE_RECORD_RESPONSE_BYTES);
        let error = ChangeRecordResponseV1::bounded(summary).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("change record core exceeds the bounded projection")
        );
    }
}
