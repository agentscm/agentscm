//! Trusted-local intent task adapter. Supplied human excerpts and model results remain
//! caller assertions; this host observes source bytes, pins skill bytes and owns the clock.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::change_capture::{GitSourceRequest, LocalSpool, capture_git_source};
use crate::change_record::{
    ArtifactMediaV1, ArtifactRefV1, AssuranceBasisV1, ContentAvailabilityV1, CoverageV1,
    RetentionClassV1,
};
use crate::skill_workflow::intent::*;
use crate::workflow_library::{AgentSkillPackage, Library};
use crate::{Error, Result};

pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const SOURCE_EVIDENCE: &str = "host_source";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostEvidence {
    pub id: String,
    pub text: String,
    pub origin: String,
    pub coverage: CoverageV1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum IntentReviewRequest {
    Start {
        request_id: String,
        work_id: String,
        #[serde(default)]
        root_work_id: Option<String>,
        contract: IntentContract,
        scope: IntentScope,
        workflow_version_id: String,
        step_id: String,
        limits: IntentLimits,
        #[serde(default)]
        evidence: Vec<HostEvidence>,
    },
    Status {
        work_id: String,
    },
    Claim {
        request_id: String,
        work_id: String,
        attempt_id: String,
        session_id: String,
        phase: IntentPhase,
        lease_ms: u64,
        #[serde(default)]
        requested_model: Option<String>,
        #[serde(default)]
        evidence: Vec<HostEvidence>,
    },
    Heartbeat {
        request_id: String,
        work_id: String,
        attempt_id: String,
        session_id: String,
        fence: u64,
        lease_ms: u64,
    },
    Report {
        request_id: String,
        work_id: String,
        attempt_id: String,
        session_id: String,
        fence: u64,
        execution: IntentExecution,
        #[serde(default)]
        assessment: Option<IntentAssessment>,
        #[serde(default)]
        observed_model: Option<String>,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        evidence: Vec<HostEvidence>,
    },
    Cancel {
        request_id: String,
        work_id: String,
        session_id: String,
        reason: String,
    },
    Recover {
        request_id: String,
        work_id: String,
        session_id: String,
        reason: String,
        authorization: IntentReference,
    },
    Amend {
        request_id: String,
        work_id: String,
        contract: IntentContract,
    },
    Disposition {
        request_id: String,
        work_id: String,
        expected_subject_digest: String,
        kind: IntentDispositionKind,
        criteria: Vec<String>,
        reason: String,
        authorization: IntentReference,
    },
    Stop {
        work_id: String,
    },
}

pub struct IntentReviewHost {
    root: PathBuf,
    data_dir: PathBuf,
    repository: String,
    human_authority: bool,
}

impl IntentReviewHost {
    /// `repository` and `root` are the operator's selected binding, never request fields.
    pub fn new(
        root: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        repository: impl Into<String>,
    ) -> Result<Self> {
        let root = root.into().canonicalize()?;
        let repository = repository.into();
        if repository.is_empty()
            || repository.len() > 256
            || repository.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "invalid selected repository identity".into(),
            ));
        }
        Ok(Self {
            root,
            data_dir: data_dir.into(),
            repository,
            human_authority: false,
        })
    }

    /// Trusted-local operator capability; an excerpt alone cannot enable human operations.
    /// This is not proof of who typed the excerpt or permission outside this workflow.
    pub fn with_human_authority(mut self) -> Self {
        self.human_authority = true;
        self
    }

    pub fn status(&self, work_id: &str) -> Result<IntentView> {
        let (subject, _) = self.current(work_id)?;
        IntentService::new(&self.data_dir).status(&subject, now_ms()?)
    }

    pub fn dispatch(&self, request: IntentReviewRequest) -> Result<Value> {
        if serde_json::to_vec(&request)?.len() > MAX_REQUEST_BYTES {
            return Err(Error::Limit("intent request exceeds 256 KiB".into()));
        }
        let service = IntentService::new(&self.data_dir);
        let now = now_ms()?;
        let view = match request {
            IntentReviewRequest::Start {
                request_id,
                work_id,
                root_work_id,
                mut contract,
                scope,
                workflow_version_id,
                step_id,
                limits,
                evidence,
            } => {
                normalize_contract(&mut contract)?;
                crate::store::Engine::open(&self.data_dir)?;
                let package =
                    Library::open(&self.data_dir)?.agent_skill(&workflow_version_id, &step_id)?;
                let mut subject = IntentSubject {
                    repository: self.repository.clone(),
                    root_work_id: root_work_id.unwrap_or_else(|| work_id.clone()),
                    work_id,
                    contract_digest: contract.digest()?,
                    scope,
                    source: self.capture("HEAD")?,
                    evidence: Vec::new(),
                    skill: package.binding,
                    evaluator_profile_digest: profile_digest(),
                };
                self.attach_evidence(&mut subject, evidence)?;
                service.create(&CreateIntent {
                    request_id,
                    contract,
                    subject,
                    limits,
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Status { work_id } => self.status(&work_id)?,
            IntentReviewRequest::Claim {
                request_id,
                work_id,
                attempt_id,
                session_id,
                phase,
                lease_ms,
                requested_model,
                evidence,
            } => {
                let (mut subject, package) = self.current(&work_id)?;
                self.attach_evidence(&mut subject, evidence)?;
                let artifacts = self.artifact_texts(&subject)?;
                let view = service.claim(&IntentClaim {
                    request_id,
                    subject,
                    attempt_id,
                    session_id,
                    phase,
                    lease_ms,
                    requested_model,
                    now_ms: now,
                })?;
                return Ok(
                    json!({ "view": view, "task": { "phase": phase, "contract": view.record.contracts.last(), "subject": view.record.subject, "skill": package, "artifacts": artifacts, "evidence_assurance": "supplied evidence, model selection and model reports are caller_asserted; the evaluator profile identifies this adapter configuration", "next_action": "Read the pinned skill package, use existing authorized tools, then report this attempt and fence." } }),
                );
            }
            IntentReviewRequest::Heartbeat {
                request_id,
                work_id,
                attempt_id,
                session_id,
                fence,
                lease_ms,
            } => {
                let (subject, _) = self.current(&work_id)?;
                service.heartbeat(&IntentHeartbeat {
                    request_id,
                    subject,
                    attempt_id,
                    session_id,
                    fence,
                    lease_ms,
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Report {
                request_id,
                work_id,
                attempt_id,
                session_id,
                fence,
                execution,
                assessment,
                observed_model,
                detail,
                evidence,
            } => {
                let (mut subject, _) = self.current(&work_id)?;
                self.attach_evidence(&mut subject, evidence)?;
                service.report(&IntentReport {
                    request_id,
                    subject,
                    attempt_id,
                    session_id,
                    fence,
                    execution,
                    assessment,
                    observed_model,
                    detail,
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Cancel {
                request_id,
                work_id,
                session_id,
                reason,
            } => {
                let (subject, _) = self.current(&work_id)?;
                service.cancel(&IntentControl {
                    request_id,
                    subject,
                    session_id,
                    reason,
                    authorization: None,
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Recover {
                request_id,
                work_id,
                session_id,
                reason,
                mut authorization,
            } => {
                self.require_human()?;
                normalize_reference(&mut authorization)?;
                let (subject, _) = self.current(&work_id)?;
                service.recover(&IntentControl {
                    request_id,
                    subject,
                    session_id,
                    reason,
                    authorization: Some(authorization),
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Amend {
                request_id,
                work_id,
                mut contract,
            } => {
                self.require_human()?;
                normalize_contract(&mut contract)?;
                let (subject, _) = self.current(&work_id)?;
                service.amend(&IntentAmendment {
                    request_id,
                    subject,
                    contract,
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Disposition {
                request_id,
                work_id,
                expected_subject_digest,
                kind,
                criteria,
                reason,
                mut authorization,
            } => {
                self.require_human()?;
                normalize_reference(&mut authorization)?;
                let (subject, _) = self.current(&work_id)?;
                let subject_digest = subject.digest()?;
                if expected_subject_digest != subject_digest {
                    return Err(Error::Conflict(
                        "candidate or context changed before human disposition".into(),
                    ));
                }
                service.disposition(&IntentDispositionRequest {
                    request_id,
                    subject,
                    disposition: IntentDisposition {
                        kind,
                        criteria,
                        reason,
                        authorization,
                        subject_digest,
                    },
                    now_ms: now,
                })?
            }
            IntentReviewRequest::Stop { work_id } => {
                let view = self.status(&work_id)?;
                let next_action = match view.next {
                    IntentNext::Assess => {
                        "Claim an assessment task and execute its pinned skill with existing tools."
                    }
                    IntentNext::Correct => {
                        "Claim a correction task within the remaining budget, then reassess the new candidate."
                    }
                    IntentNext::Investigate => {
                        "Inspect source omissions and authorized exclusions first; extra evidence cannot resolve uncaptured source. Retrieve missing evidence or request human scope clarification."
                    }
                    IntentNext::InProgress => {
                        "Finish or explicitly cancel the current leased attempt; do not launch a duplicate."
                    }
                    IntentNext::RecoveryRequired => {
                        "Request explicit human-authorized recovery; do not retry the uncertain attempt."
                    }
                    IntentNext::BudgetExhausted => {
                        "Stop automatic continuation and request a human decision."
                    }
                    IntentNext::Complete => {
                        "Current evidence meets recorded criteria; follow existing independent review and approval requirements."
                    }
                    IntentNext::HumanDisposition => {
                        "Inspect the retained human disposition and follow the existing approval workflow."
                    }
                    IntentNext::Cancelled => {
                        "Keep the work stopped until explicit human-authorized recovery."
                    }
                };
                return Ok(
                    json!({ "work_id": work_id, "next": view.next, "outcome": view.outcome, "current": view.current, "next_action": next_action, "admission": view.admission, "pending_task": {"contract_version": view.record.contracts.last().map(|contract| contract.version), "criteria": view.record.subject.scope.criteria, "assessed_source": view.record.subject.source, "source_exclusions": view.record.subject.scope.source_exclusions, "latest_assessment": view.record.attempts.iter().rev().find_map(|attempt| attempt.assessment.as_ref()), "root_invocations": view.root_invocations, "root_correction_rounds": view.root_correction_rounds}, "boundary": "Cooperative guidance only; existing stop hooks, permissions and admission requirements still apply." }),
                );
            }
        };
        Ok(json!({ "view": view }))
    }

    fn require_human(&self) -> Result<()> {
        if !self.human_authority {
            return Err(Error::Invalid(
                "explicit trusted-local human authority is required".into(),
            ));
        }
        Ok(())
    }

    fn capture(&self, base_ref: &str) -> Result<crate::change_record::SourceBindingV1> {
        let store = self.data_dir.canonicalize()?;
        if store.starts_with(&self.root) {
            let ignored = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["check-ignore", "--quiet", "--"])
                .arg(&store)
                .status()?;
            if !ignored.success() {
                return Err(Error::Invalid("intent store inside source must be Git-ignored; select a store outside the checkout".into()));
            }
        }
        let worktree_id = crate::store::digest(self.root.as_os_str().as_encoded_bytes());
        capture_git_source(
            &GitSourceRequest {
                repository: self.repository.clone(),
                worktree_id,
                repository_path: self.root.clone(),
                base_ref: base_ref.into(),
            },
            &self.spool()?,
        )
        .map_err(|error| Error::Invalid(format!("current source capture failed: {error}")))
    }

    fn current(&self, work_id: &str) -> Result<(IntentSubject, AgentSkillPackage)> {
        match std::fs::symlink_metadata(self.data_dir.join("skill-workflows")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound("intent Work".into()));
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let mut subject = IntentService::new(&self.data_dir)
            .inspect(&self.repository, work_id)?
            .subject;
        let package = Library::open(&self.data_dir)?
            .agent_skill(&subject.skill.workflow_version_id, &subject.skill.step_id)?;
        if subject.skill != package.binding {
            return Err(Error::Conflict("pinned intent skill bytes changed".into()));
        }
        let crate::change_record::SourceBindingV1::Git { base, .. } = &subject.source else {
            return Err(Error::Invalid(
                "intent review requires retained Git source".into(),
            ));
        };
        subject.source = self.capture(base)?;
        subject.evaluator_profile_digest = profile_digest();
        self.attach_evidence(&mut subject, Vec::new())?;
        Ok((subject, package))
    }

    fn spool(&self) -> Result<LocalSpool> {
        LocalSpool::open(self.data_dir.join("intent-review-spool"))
            .map_err(|error| Error::Invalid(format!("intent spool unavailable: {error}")))
    }

    fn attach_evidence(
        &self,
        subject: &mut IntentSubject,
        supplied: Vec<HostEvidence>,
    ) -> Result<()> {
        if supplied.len() > 64 {
            return Err(Error::Limit("too many supplied intent artifacts".into()));
        }
        let spool = self.spool()?;
        let candidate = subject.candidate_digest()?;
        // Source metadata identifies the host-observed candidate; it is not a test result.
        let bytes = serde_json::to_vec(&subject.source)?;
        let source = spool
            .retain_artifact(&bytes, ArtifactMediaV1::Json, RetentionClassV1::WorkLocal)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        subject
            .evidence
            .retain(|evidence| evidence.id != SOURCE_EVIDENCE);
        subject.evidence.push(IntentEvidence {
            id: SOURCE_EVIDENCE.into(),
            sha256: source.digest,
            origin: "host-captured Git source identity".into(),
            candidate_digest: Some(candidate.clone()),
            coverage: CoverageV1::Complete,
            assurance: AssuranceBasisV1::ObservedTrustedLocal,
            available: true,
        });
        for evidence in supplied {
            if evidence.id == SOURCE_EVIDENCE
                || evidence.text.is_empty()
                || evidence.text.len() > MAX_TEXT_BYTES
            {
                return Err(Error::Invalid(
                    "invalid supplied intent artifact identity or size".into(),
                ));
            }
            let artifact = spool
                .retain_artifact(
                    evidence.text.as_bytes(),
                    ArtifactMediaV1::Text,
                    RetentionClassV1::WorkLocal,
                )
                .map_err(|error| Error::Invalid(error.to_string()))?;
            let fact = IntentEvidence {
                id: evidence.id,
                sha256: artifact.digest,
                origin: evidence.origin,
                candidate_digest: Some(candidate.clone()),
                coverage: evidence.coverage,
                assurance: AssuranceBasisV1::CallerAsserted,
                available: true,
            };
            if let Some(prior) = subject.evidence.iter().find(|prior| prior.id == fact.id) {
                if prior != &fact {
                    return Err(Error::Conflict(
                        "use a new evidence ID for changed evidence".into(),
                    ));
                }
            } else {
                subject.evidence.push(fact);
            }
        }
        subject
            .evidence
            .sort_by(|left, right| left.id.cmp(&right.id));
        for evidence in &mut subject.evidence {
            evidence.available = self.read_artifact_text(&spool, evidence).is_ok();
        }
        Ok(())
    }

    fn read_artifact_text(&self, spool: &LocalSpool, evidence: &IntentEvidence) -> Result<String> {
        let metadata =
            std::fs::symlink_metadata(spool.root().join("artifacts").join(&evidence.sha256))?;
        if !metadata.is_file() || metadata.len() > MAX_TEXT_BYTES as u64 {
            return Err(Error::Invalid(
                "intent artifact is unavailable or oversized".into(),
            ));
        }
        let bytes = spool
            .read_artifact(&ArtifactRefV1 {
                digest: evidence.sha256.clone(),
                size_bytes: metadata.len(),
                media: ArtifactMediaV1::Text,
                retention: RetentionClassV1::WorkLocal,
                availability: ContentAvailabilityV1::Local,
            })
            .map_err(|error| Error::Invalid(error.to_string()))?;
        String::from_utf8(bytes).map_err(|_| Error::Invalid("intent artifact is not UTF-8".into()))
    }

    fn artifact_texts(&self, subject: &IntentSubject) -> Result<Value> {
        let spool = self.spool()?;
        let mut total = 0;
        let mut artifacts = Vec::new();
        for evidence in &subject.evidence {
            let text = self.read_artifact_text(&spool, evidence).ok();
            total += text.as_ref().map_or(0, String::len);
            if total > MAX_REQUEST_BYTES {
                return Err(Error::Limit(
                    "claimed evidence exceeds 256 KiB; narrow supplied evidence".into(),
                ));
            }
            artifacts.push(json!({"id": evidence.id, "sha256": evidence.sha256, "text": text, "assurance": evidence.assurance, "coverage": evidence.coverage}));
        }
        Ok(json!(artifacts))
    }
}

fn normalize_reference(reference: &mut IntentReference) -> Result<()> {
    if let Some(text) = &reference.text {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(Error::Invalid("invalid intent excerpt size".into()));
        }
        reference.sha256 = crate::store::digest(text.as_bytes());
    } else {
        reference.coverage = CoverageV1::Unknown;
        reference.reconstructed = true;
        reference.sha256 = crate::store::digest(b"unavailable intent excerpt");
    }
    Ok(())
}

fn normalize_contract(contract: &mut IntentContract) -> Result<()> {
    for reference in contract
        .request
        .iter_mut()
        .chain(&mut contract.amendments)
        .chain(&mut contract.focus)
    {
        normalize_reference(reference)?;
    }
    if contract
        .request
        .iter()
        .chain(&contract.amendments)
        .any(|reference| {
            reference.text.is_none()
                || reference.reconstructed
                || reference.coverage != CoverageV1::Complete
        })
        && contract.coverage == CoverageV1::Complete
    {
        contract.coverage = CoverageV1::Partial;
        contract
            .gaps
            .push("Original human context is incomplete or reconstructed.".into());
    }
    Ok(())
}

fn profile_digest() -> String {
    crate::store::digest(
        b"agentscm-intent-review-host-v1:trusted-local-git:caller-asserted-reports",
    )
}

fn now_ms() -> Result<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("system clock precedes epoch".into()))?;
    u64::try_from(now.as_millis()).map_err(|_| Error::Limit("system clock is out of range".into()))
}
