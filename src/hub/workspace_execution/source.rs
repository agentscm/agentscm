//! Bounded laptop source transfer into an operator-pinned workspace profile.
//!
//! Laptop source identity is provenance, not native authority. The authenticated
//! workspace route and operator configuration select the destination repository,
//! target, workflow requirement and immutable review-context inputs.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ExecutionProfile, ExecutionService, Queue, RunSubject, validate_id};
use crate::authority::Authority;
use crate::context::{BaseState, ChangeId, REVIEW_CONTEXT_VERSION, ReviewContext};
use crate::model::{LandingPreparation, PrepareLandingRequest, TargetState};
use crate::store::Engine;
use crate::sync::{SourceOffer, SourceRequest};
use crate::{Error, Result as NativeResult};

use super::super::workspace::{WorkspaceAction, WorkspaceActor};
use super::super::{HubError, Result};

pub const SOURCE_TRANSFER_VERSION: u32 = 1;
pub const MAX_SOURCE_OFFER_BYTES: usize = 1024 * 1024;
pub const MAX_SOURCE_OBJECTS: usize = 16_384;
pub const MAX_SOURCE_BUNDLE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RETAINED_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SOURCE_TRANSFERS: i64 = 256;
const TERMINAL_METADATA_RESERVE: u64 = 1024;
const SOURCE_CHANGE_DOMAIN: &[u8] = b"agentscm.workspace-source.change.v1\0";
const SOURCE_REQUEST_DOMAIN: &[u8] = b"agentscm.workspace-source.request.v1\0";
const COMPACT_REQUEST_PREFIX: &[u8] = b"sha256:";

/// Opt-in operator configuration joined to an existing execution profile.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProfile {
    pub repository_id: String,
    pub profile_id: String,
    pub template_context_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextDeclaration {
    pub intent: String,
    pub acceptance_criteria: Vec<String>,
    pub scope: Vec<String>,
    pub exclusions: Vec<String>,
    pub omissions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceBootstrap {
    pub version: u32,
    pub actor_id: String,
    pub actor_generation: i64,
    pub repository_id: String,
    pub profile_id: String,
    pub native_repository: String,
    pub target: Option<TargetState>,
    pub requirement_digest: String,
    pub template_context: ReviewContext,
    pub offer: SourceOffer,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceDownloadRequest {
    pub offer: SourceOffer,
    pub request: SourceRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceTransferRequest {
    pub version: u32,
    pub request_id: String,
    pub expected_generation: i64,
    pub source_repository: String,
    pub source_change: String,
    pub source_revision: String,
    pub expected_change_version: u64,
    pub context: ContextDeclaration,
    pub offer: SourceOffer,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceTransferState {
    AwaitingUpload,
    Importing,
    Complete,
    Rejected,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceTransfer {
    pub version: u32,
    pub transfer_id: String,
    pub request_id: String,
    pub repository_id: String,
    pub profile_id: String,
    pub native_repository: String,
    pub source_repository: String,
    pub source_change: String,
    pub input_revision: String,
    pub state: SourceTransferState,
    pub request: SourceRequest,
    pub subject: Option<RunSubject>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredSourceRequest {
    route_repository: String,
    route_profile: String,
    request: SourceTransferRequest,
}

pub(super) fn initialize_source_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS workspace_source_transfers(
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            transfer_id TEXT NOT NULL UNIQUE,
            actor_id TEXT NOT NULL,
            request_id TEXT NOT NULL,
            repository_id TEXT NOT NULL,
            profile_id TEXT NOT NULL,
            request_json BLOB NOT NULL,
            transfer_json BLOB NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('awaiting_upload','importing','complete','rejected','unknown')),
            bundle_sha256 TEXT,
            body_bytes INTEGER NOT NULL DEFAULT 0 CHECK(body_bytes>=0),
            reserved_bytes INTEGER NOT NULL DEFAULT 0 CHECK(reserved_bytes>=0),
            UNIQUE(actor_id,request_id)
        ) STRICT;",
    )?;
    Ok(())
}

pub(super) fn recover_source_transfers(db: &Connection) -> Result<()> {
    let mut statement = db.prepare(
        "SELECT transfer_id,transfer_json FROM workspace_source_transfers WHERE state='importing'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut interrupted = Vec::new();
    for row in rows {
        interrupted.push(row?);
    }
    drop(statement);
    for (id, bytes) in interrupted {
        let mut transfer: SourceTransfer = decode(&bytes)?;
        transfer.state = SourceTransferState::Unknown;
        transfer.subject = None;
        transfer.detail = "Server interrupted source admission after accepting the bundle; inspect native source and Change state. This transfer will not be replayed.".into();
        db.execute(
            "UPDATE workspace_source_transfers SET state='unknown',transfer_json=?1 WHERE transfer_id=?2 AND state='importing'",
            params![serde_json::to_vec(&transfer).map_err(native_json)?, id],
        )?;
    }
    Ok(())
}

impl ExecutionService {
    pub fn source_authorize(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
    ) -> Result<()> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        let (execution, source) = self.source_profile(repository, profile)?;
        let (authority, _) = execution.current_requirement()?;
        let template = authority
            .review_context(&source.template_context_digest)
            .map_err(HubError::from)?;
        validate_template(execution, &template)
    }

    fn source_profile(
        &self,
        repository: &str,
        profile: &str,
    ) -> Result<(&ExecutionProfile, &SourceProfile)> {
        let execution = self.profile(repository, profile)?;
        let source = self
            .source_profiles
            .get(&(repository.to_owned(), profile.to_owned()))
            .ok_or(HubError::Unauthorized)?;
        Ok((execution, source))
    }

    pub fn source_bootstrap(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
    ) -> Result<SourceBootstrap> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        let (profile, source) = self.source_profile(repository, profile)?;
        let (authority, requirement) = profile.current_requirement()?;
        let template = authority
            .review_context(&source.template_context_digest)
            .map_err(HubError::from)?;
        validate_template(profile, &template)?;
        let target = authority.target(&profile.target).map_err(HubError::from)?;
        let root = target
            .as_ref()
            .map(|target| target.revision.as_str())
            .unwrap_or(template.revision.as_str());
        let engine = Engine::open(&profile.data_dir).map_err(HubError::from)?;
        let offer =
            crate::sync::source_offer(&engine, &profile.native_repository, &[root.to_owned()])
                .map_err(HubError::from)?;
        validate_offer(&offer, &profile.native_repository, root)?;
        Ok(SourceBootstrap {
            version: SOURCE_TRANSFER_VERSION,
            actor_id: actor.user_id().to_owned(),
            actor_generation: actor.generation(),
            repository_id: repository.to_owned(),
            profile_id: profile.profile_id.clone(),
            native_repository: profile.native_repository.clone(),
            target,
            requirement_digest: requirement.digest().map_err(HubError::from)?,
            template_context: template,
            offer,
        })
    }

    pub fn source_download(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        request: SourceDownloadRequest,
    ) -> Result<Vec<u8>> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        let _body = self.source_worker.try_lock().map_err(|_| HubError::Busy)?;
        let current = self.source_bootstrap(actor, repository, profile)?;
        if request.offer != current.offer {
            return Err(HubError::Invalid(
                "bootstrap offer changed; refresh before downloading source".into(),
            ));
        }
        let (execution, _) = self.source_profile(repository, profile)?;
        let engine = Engine::open(&execution.data_dir).map_err(HubError::from)?;
        let expected =
            crate::sync::request_for_offer(&engine, &request.offer).map_err(HubError::from)?;
        validate_source_request(&request.request, &request.offer)?;
        // The receiver chooses any valid subset of missing objects. The server
        // verifies the request against the exact current offer; it does not claim
        // knowledge of the receiver's object store.
        if request.request.offer_sha256 != expected.offer_sha256 {
            return Err(HubError::Invalid(
                "bootstrap request does not bind the offer".into(),
            ));
        }
        let directory =
            crate::store::create_directory_durable(execution.data_dir.join("source-transfer-temp"))
                .map_err(HubError::from)?;
        let offer_file = TemporaryFile::create(&directory, "offer")?;
        let request_file = TemporaryFile::create(&directory, "request")?;
        let bundle_file = TemporaryFile::reserve(&directory, "bundle")?;
        let offer_path = offer_file.path();
        let request_path = request_file.path();
        let bundle_path = bundle_file.path();
        fs::write(
            offer_path,
            serde_json::to_vec(&request.offer).map_err(native_json)?,
        )?;
        fs::write(
            request_path,
            serde_json::to_vec(&request.request).map_err(native_json)?,
        )?;
        crate::sync::write_bundle_bound(
            &engine,
            offer_path,
            request_path,
            bundle_path,
            MAX_SOURCE_BUNDLE_BYTES as u64,
            MAX_SOURCE_OBJECTS,
        )
        .map_err(HubError::from)?;
        read_bounded(bundle_path, MAX_SOURCE_BUNDLE_BYTES)
    }

    pub fn source_offer(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        request: SourceTransferRequest,
    ) -> Result<SourceTransfer> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        validate_id(&request.request_id)?;
        if request.version != SOURCE_TRANSFER_VERSION {
            return Err(HubError::Invalid(
                "unsupported source transfer version".into(),
            ));
        }
        let stored = StoredSourceRequest {
            route_repository: repository.to_owned(),
            route_profile: profile.to_owned(),
            request,
        };
        let bytes = serde_json::to_vec(&stored).map_err(native_json)?;
        let queue = self.lock()?;
        if let Some((saved, transfer)) = queue
            .db
            .query_row(
                "SELECT request_json,transfer_json FROM workspace_source_transfers WHERE actor_id=?1 AND request_id=?2",
                params![actor.user_id(), stored.request.request_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if !request_matches(&saved, &bytes) {
                return Err(HubError::Invalid("workspace_idempotency_conflict".into()));
            }
            return decode(&transfer);
        }
        if stored.request.expected_generation != actor.generation() {
            return Err(HubError::Invalid("workspace_generation_conflict".into()));
        }
        let (execution, source) = self.source_profile(repository, profile)?;
        let (authority, _) = execution.current_requirement()?;
        let template = authority
            .review_context(&source.template_context_digest)
            .map_err(HubError::from)?;
        validate_template(execution, &template)?;
        validate_provenance(&stored.request.source_repository)?;
        validate_provenance(&stored.request.source_change)?;
        validate_offer(
            &stored.request.offer,
            &stored.request.source_repository,
            &stored.request.source_revision,
        )?;
        let engine = Engine::open(&execution.data_dir).map_err(HubError::from)?;
        let missing = crate::sync::request_for_offer(&engine, &stored.request.offer)
            .map_err(HubError::from)?;
        let change = source_change_id(
            &execution.native_repository,
            actor.user_id(),
            repository,
            profile,
            &stored.request,
        );
        let context = build_context(&authority, execution, &template, &change, &stored.request)?;
        context.validate().map_err(HubError::from)?;
        validate_expected_change(&authority, &change, stored.request.expected_change_version)?;
        let transfer_id = super::random_id()?;
        let transfer = SourceTransfer {
            version: SOURCE_TRANSFER_VERSION,
            transfer_id: transfer_id.clone(),
            request_id: stored.request.request_id.clone(),
            repository_id: repository.to_owned(),
            profile_id: profile.to_owned(),
            native_repository: execution.native_repository.clone(),
            source_repository: stored.request.source_repository.clone(),
            source_change: stored.request.source_change.clone(),
            input_revision: stored.request.source_revision.clone(),
            state: SourceTransferState::AwaitingUpload,
            request: missing,
            subject: None,
            detail: "Awaiting the exact negotiated immutable source bundle".into(),
        };
        let transfer_bytes = serde_json::to_vec(&transfer).map_err(native_json)?;
        ensure_offer_capacity(
            &queue.db,
            repository,
            profile,
            bytes.len(),
            transfer_bytes.len(),
        )?;
        self.workspace.with_authorization(
            actor,
            repository,
            WorkspaceAction::ExecuteWorkflow,
            || -> NativeResult<()> {
                queue.db.execute(
                    "INSERT INTO workspace_source_transfers(transfer_id,actor_id,request_id,repository_id,profile_id,request_json,transfer_json,state) VALUES(?1,?2,?3,?4,?5,?6,?7,'awaiting_upload')",
                    params![transfer_id,actor.user_id(),stored.request.request_id,repository,profile,bytes,transfer_bytes],
                )?;
                Ok(())
            },
        )?;
        Ok(transfer)
    }

    pub fn source_transfer(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        transfer: &str,
    ) -> Result<SourceTransfer> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        validate_id(transfer)?;
        self.lock()?
            .source_transfer(actor.user_id(), repository, profile, transfer)
    }

    pub fn source_upload(
        &self,
        actor: &WorkspaceActor,
        repository: &str,
        profile: &str,
        transfer_id: &str,
        body: &[u8],
    ) -> Result<SourceTransfer> {
        self.workspace
            .authorize(actor, repository, WorkspaceAction::ExecuteWorkflow)?;
        validate_id(transfer_id)?;
        if body.len() > MAX_SOURCE_BUNDLE_BYTES {
            return Err(HubError::Invalid("source bundle exceeds 64 MiB".into()));
        }
        let body_digest = format!("{:x}", Sha256::digest(body));
        let _body = self.source_worker.try_lock().map_err(|_| HubError::Busy)?;
        let (stored, mut transfer, saved_digest) = {
            let queue = self.lock()?;
            queue.source_transfer_record(actor.user_id(), repository, profile, transfer_id)?
        };
        if let Some(saved) = saved_digest.as_deref()
            && saved != body_digest
        {
            return Err(HubError::Invalid("workspace_idempotency_conflict".into()));
        }
        match transfer.state {
            SourceTransferState::Complete | SourceTransferState::Rejected => return Ok(transfer),
            SourceTransferState::Importing => return Err(HubError::Busy),
            SourceTransferState::Unknown => {
                return Err(HubError::Invalid("workspace_source_unknown".into()));
            }
            SourceTransferState::AwaitingUpload => {}
        }
        let request: StoredSourceRequest = decode(&stored)?;
        if request.request.expected_generation != actor.generation() {
            return Err(HubError::Invalid("workspace_generation_conflict".into()));
        }
        let (execution, source) = self.source_profile(repository, profile)?;
        // Validate all current server-owned bindings before durably entering the
        // uncertain mutation window.
        let (authority, _) = execution.current_requirement()?;
        let template = authority
            .review_context(&source.template_context_digest)
            .map_err(HubError::from)?;
        validate_template(execution, &template)?;
        let change = source_change_id(
            &execution.native_repository,
            actor.user_id(),
            repository,
            profile,
            &request.request,
        );
        validate_expected_change(&authority, &change, request.request.expected_change_version)?;
        let body_bytes = i64::try_from(body.len())
            .map_err(|_| HubError::Invalid("source bundle exceeds platform limit".into()))?;
        {
            let queue = self.lock()?;
            transfer.state = SourceTransferState::Importing;
            transfer.detail = "Bundle accepted; admission is in progress. If the server is interrupted, inspect retained native source and Change state because this transfer becomes unknown and is never replayed automatically.".into();
            let transfer_bytes = serde_json::to_vec(&transfer).map_err(native_json)?;
            ensure_upload_capacity(
                &queue.db,
                repository,
                profile,
                transfer_id,
                body.len(),
                transfer_bytes.len(),
            )?;
            if queue.db.execute(
                "UPDATE workspace_source_transfers SET state='importing',bundle_sha256=?1,body_bytes=?2,reserved_bytes=?3,transfer_json=?4 WHERE transfer_id=?5 AND state='awaiting_upload'",
                params![body_digest,body_bytes,TERMINAL_METADATA_RESERVE as i64,transfer_bytes,transfer_id],
            )? != 1 {
                return Err(HubError::Busy);
            }
        }
        let mut mutation_started = false;
        let outcome = self.workspace.with_authorization(
            actor,
            repository,
            WorkspaceAction::ExecuteWorkflow,
            || {
                admit_source(
                    execution,
                    source,
                    actor.user_id(),
                    transfer_id,
                    &request,
                    &transfer.request,
                    body,
                    &mut mutation_started,
                )
            },
        );
        let mut final_transfer = match outcome {
            Ok(subject) => {
                transfer.state = SourceTransferState::Complete;
                transfer.subject = Some(subject);
                transfer.detail = "Immutable source admitted and bound to the exact current server Change context".into();
                transfer
            }
            Err(error) => {
                transfer.state = failed_source_state(mutation_started);
                transfer.subject = None;
                transfer.detail = if mutation_started {
                    "Source admission did not complete conclusively; inspect retained native source and Change state. This transfer will not be replayed.".into()
                } else {
                    "Source admission was rejected before native Change mutation. Refresh bootstrap state and submit a new source request.".into()
                };
                if transfer.state == SourceTransferState::Rejected {
                    transfer.request.missing.clear();
                }
                let mut queue = self.lock()?;
                let compact = (transfer.state == SourceTransferState::Rejected)
                    .then(|| compact_request(&stored));
                queue.save_source_transfer(&transfer, compact.as_deref())?;
                if transfer.state == SourceTransferState::Rejected {
                    return queue.source_transfer(
                        actor.user_id(),
                        repository,
                        profile,
                        transfer_id,
                    );
                }
                return Err(error);
            }
        };
        final_transfer.request.missing.clear();
        let mut queue = self.lock()?;
        let compact = compact_request(&stored);
        queue.save_source_transfer(&final_transfer, Some(&compact))?;
        // Re-read the persisted representation before returning it.
        final_transfer =
            queue.source_transfer(actor.user_id(), repository, profile, transfer_id)?;
        Ok(final_transfer)
    }
}

impl Queue {
    fn source_transfer(
        &self,
        actor: &str,
        repository: &str,
        profile: &str,
        transfer: &str,
    ) -> Result<SourceTransfer> {
        self.source_transfer_record(actor, repository, profile, transfer)
            .map(|(_, transfer, _)| transfer)
    }

    fn source_transfer_record(
        &self,
        actor: &str,
        repository: &str,
        profile: &str,
        transfer: &str,
    ) -> Result<(Vec<u8>, SourceTransfer, Option<String>)> {
        let row = self
            .db
            .query_row(
                "SELECT request_json,transfer_json,bundle_sha256 FROM workspace_source_transfers WHERE actor_id=?1 AND repository_id=?2 AND profile_id=?3 AND transfer_id=?4",
                params![actor, repository, profile, transfer],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Option<String>>(2)?)),
            )
            .optional()?
            .ok_or(HubError::Unauthorized)?;
        Ok((row.0, decode(&row.1)?, row.2))
    }

    fn save_source_transfer(
        &mut self,
        transfer: &SourceTransfer,
        compact_request: Option<&[u8]>,
    ) -> Result<()> {
        let state = match transfer.state {
            SourceTransferState::AwaitingUpload => "awaiting_upload",
            SourceTransferState::Importing => "importing",
            SourceTransferState::Complete => "complete",
            SourceTransferState::Rejected => "rejected",
            SourceTransferState::Unknown => "unknown",
        };
        let transfer_id = transfer.transfer_id.clone();
        let transfer = serde_json::to_vec(transfer).map_err(native_json)?;
        let updated = match compact_request {
            Some(request) => self.db.execute(
                "UPDATE workspace_source_transfers SET state=?1,request_json=?2,transfer_json=?3,reserved_bytes=0 WHERE transfer_id=?4 AND state='importing'",
                params![state,request,transfer,transfer_id],
            )?,
            None => self.db.execute(
                "UPDATE workspace_source_transfers SET state=?1,transfer_json=?2,reserved_bytes=0 WHERE transfer_id=?3 AND state='importing'",
                params![state,transfer,transfer_id],
            )?,
        };
        if updated != 1 {
            return Err(HubError::Unavailable(
                "source transfer state changed".into(),
            ));
        }
        Ok(())
    }
}

fn compact_request(request: &[u8]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(SOURCE_REQUEST_DOMAIN);
    hash.update(request);
    let mut compact = Vec::with_capacity(COMPACT_REQUEST_PREFIX.len() + 64);
    compact.extend_from_slice(COMPACT_REQUEST_PREFIX);
    compact.extend_from_slice(format!("{:x}", hash.finalize()).as_bytes());
    compact
}

fn request_matches(saved: &[u8], request: &[u8]) -> bool {
    saved == request || saved == compact_request(request)
}

fn retained_profile_bytes(db: &Connection, repository: &str, profile: &str) -> Result<u64> {
    let retained: i64 = db.query_row(
        "SELECT coalesce(sum(body_bytes+reserved_bytes+length(request_json)+length(transfer_json)),0) FROM workspace_source_transfers WHERE repository_id=?1 AND profile_id=?2",
        params![repository, profile],
        |row| row.get(0),
    )?;
    u64::try_from(retained)
        .map_err(|_| HubError::Unavailable("invalid retained source byte count".into()))
}

fn ensure_offer_capacity(
    db: &Connection,
    repository: &str,
    profile: &str,
    request_bytes: usize,
    transfer_bytes: usize,
) -> Result<()> {
    let active: i64 = db.query_row(
        "SELECT count(*) FROM workspace_source_transfers WHERE repository_id=?1 AND profile_id=?2 AND state NOT IN ('complete','rejected')",
        params![repository, profile],
        |row| row.get(0),
    )?;
    let added = (request_bytes as u64)
        .checked_add(transfer_bytes as u64)
        .ok_or_else(|| HubError::Invalid("source metadata size overflow".into()))?;
    if active >= MAX_SOURCE_TRANSFERS
        || retained_profile_bytes(db, repository, profile)?
            .checked_add(added)
            .is_none_or(|total| total > MAX_RETAINED_SOURCE_BYTES)
    {
        return Err(HubError::Busy);
    }
    Ok(())
}

fn ensure_upload_capacity(
    db: &Connection,
    repository: &str,
    profile: &str,
    transfer: &str,
    body_bytes: usize,
    transfer_bytes: usize,
) -> Result<()> {
    let (old_body, old_reserved, request_bytes, old_transfer): (i64, i64, i64, i64) = db.query_row(
        "SELECT body_bytes,reserved_bytes,length(request_json),length(transfer_json) FROM workspace_source_transfers WHERE repository_id=?1 AND profile_id=?2 AND transfer_id=?3",
        params![repository, profile, transfer],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let current = [old_body, old_reserved, request_bytes, old_transfer]
        .into_iter()
        .try_fold(0u64, |total, value| {
            u64::try_from(value)
                .ok()
                .and_then(|value| total.checked_add(value))
        })
        .ok_or_else(|| HubError::Unavailable("invalid retained source byte count".into()))?;
    let replacement = (request_bytes as u64)
        .checked_add(body_bytes as u64)
        .and_then(|total| total.checked_add(transfer_bytes as u64))
        .and_then(|total| total.checked_add(TERMINAL_METADATA_RESERVE))
        .ok_or_else(|| HubError::Invalid("source transfer size overflow".into()))?;
    let retained = retained_profile_bytes(db, repository, profile)?;
    if retained
        .checked_sub(current)
        .and_then(|total| total.checked_add(replacement))
        .is_none_or(|total| total > MAX_RETAINED_SOURCE_BYTES)
    {
        return Err(HubError::Busy);
    }
    Ok(())
}

// Keep the authority-write marker explicit at this admission boundary.
#[allow(clippy::too_many_arguments)]
fn admit_source(
    profile: &ExecutionProfile,
    source: &SourceProfile,
    actor_id: &str,
    transfer_id: &str,
    stored: &StoredSourceRequest,
    missing: &SourceRequest,
    body: &[u8],
    mutation_started: &mut bool,
) -> NativeResult<RunSubject> {
    let request = &stored.request;
    let repository = &stored.route_repository;
    let profile_id = &stored.route_profile;
    let directory =
        crate::store::create_directory_durable(profile.data_dir.join("source-transfer-temp"))?;
    let bundle = TemporaryFile::create(&directory, "upload").map_err(hub_to_native)?;
    let mut file = OpenOptions::new().write(true).open(bundle.path())?;
    file.write_all(body)?;
    file.sync_all()?;
    let engine = Engine::open(&profile.data_dir)?;
    let imports = crate::store::create_directory_durable(profile.data_dir.join("source-imports"))?;
    crate::sync::import_bundle_bound(
        &engine,
        bundle.path(),
        &imports,
        &request.offer,
        missing,
        MAX_SOURCE_BUNDLE_BYTES as u64,
        MAX_SOURCE_OBJECTS,
    )?;
    engine.load_revision(&request.source_revision)?;
    let mut authority = Authority::open(&profile.data_dir)?;
    if authority.repository_id()? != profile.native_repository {
        return Err(Error::Conflict("native repository mapping changed".into()));
    }
    let requirement = authority
        .workflow_requirement(&profile.target)?
        .ok_or_else(|| Error::Conflict("installed workflow requirement disappeared".into()))?;
    if requirement.digest()? != profile.requirement_digest {
        return Err(Error::Conflict(
            "installed workflow requirement changed".into(),
        ));
    }
    let template = authority.review_context(&source.template_context_digest)?;
    validate_template_native(profile, &template)?;
    let change = source_change_id(
        &profile.native_repository,
        actor_id,
        repository,
        profile_id,
        request,
    );
    if let Some(target) = authority.target(&profile.target)? {
        ensure_first_parent_descendant(&engine, &request.source_revision, &target.revision)?;
    }
    let context = build_context_native(&authority, profile, &template, &change, request)?;
    *mutation_started = true;
    let state = if request.expected_change_version == 0 {
        authority.start_change(&context)?
    } else {
        authority.update_change_context(request.expected_change_version, &context)?
    };
    let final_state = match authority.target(&profile.target)? {
        Some(target) => {
            let mut landing_context = context.clone();
            landing_context.change_version = state
                .version
                .checked_add(1)
                .ok_or_else(|| Error::Limit("change version exhausted".into()))?;
            match authority.prepare_landing(
                &engine,
                &PrepareLandingRequest {
                    operation_id: format!("ws-source-{transfer_id}"),
                    target: profile.target.clone(),
                    expected_target: target,
                    source_tip: request.source_revision.clone(),
                    change_version: state.version,
                    context: landing_context,
                },
            )? {
                LandingPreparation::Prepared { change, .. } => change,
                LandingPreparation::AlreadyCurrent { .. } => state,
            }
        }
        None => state,
    };
    let final_context = authority.review_context(&final_state.context_digest)?;
    Ok(RunSubject {
        change_id: final_context.change.id,
        change_version: final_context.change_version,
        context_digest: final_state.context_digest,
        revision: final_context.revision,
        requirement_digest: requirement.digest()?,
    })
}

fn failed_source_state(mutation_started: bool) -> SourceTransferState {
    if mutation_started {
        SourceTransferState::Unknown
    } else {
        SourceTransferState::Rejected
    }
}

fn validate_template(profile: &ExecutionProfile, template: &ReviewContext) -> Result<()> {
    validate_template_native(profile, template).map_err(HubError::from)
}

fn validate_template_native(
    profile: &ExecutionProfile,
    template: &ReviewContext,
) -> NativeResult<()> {
    if template.change.repository != profile.native_repository || template.target != profile.target
    {
        return Err(Error::Conflict(
            "source template belongs to another native repository or target".into(),
        ));
    }
    Ok(())
}

fn build_context(
    authority: &Authority,
    profile: &ExecutionProfile,
    template: &ReviewContext,
    change: &ChangeId,
    request: &SourceTransferRequest,
) -> Result<ReviewContext> {
    build_context_native(authority, profile, template, change, request).map_err(HubError::from)
}

fn build_context_native(
    authority: &Authority,
    profile: &ExecutionProfile,
    template: &ReviewContext,
    change: &ChangeId,
    request: &SourceTransferRequest,
) -> NativeResult<ReviewContext> {
    let target = authority.target(&profile.target)?;
    let context = ReviewContext {
        version: REVIEW_CONTEXT_VERSION,
        change: change.clone(),
        change_version: request
            .expected_change_version
            .checked_add(1)
            .ok_or_else(|| Error::Limit("change version exhausted".into()))?,
        revision: request.source_revision.clone(),
        base: target.map(|target| BaseState {
            revision: target.revision,
            target_version: target.version,
        }),
        target: profile.target.clone(),
        policy_epoch: authority.policy_epoch()?,
        intent: request.context.intent.clone(),
        acceptance_criteria: request.context.acceptance_criteria.clone(),
        scope: request.context.scope.clone(),
        exclusions: request.context.exclusions.clone(),
        omissions: request.context.omissions.clone(),
        dependencies: template.dependencies.clone(),
        recipe: template.recipe.clone(),
        toolchain: template.toolchain.clone(),
        environment: template.environment.clone(),
        required_evidence: template.required_evidence.clone(),
        required_capabilities: template.required_capabilities.clone(),
    };
    context.validate()?;
    Ok(context)
}

fn validate_expected_change(authority: &Authority, change: &ChangeId, expected: u64) -> Result<()> {
    match (expected, authority.change(change)) {
        (0, Err(Error::NotFound(_))) => Ok(()),
        (0, Ok(_)) => Err(HubError::Invalid("workspace_native_conflict".into())),
        (0, Err(error)) => Err(HubError::from(error)),
        (expected, Ok(state)) if state.version == expected => Ok(()),
        (_, Ok(_)) | (_, Err(Error::NotFound(_))) => {
            Err(HubError::Invalid("workspace_native_conflict".into()))
        }
        (_, Err(error)) => Err(HubError::from(error)),
    }
}

fn ensure_first_parent_descendant(engine: &Engine, source: &str, base: &str) -> NativeResult<()> {
    let mut current = source.to_owned();
    for _ in 0..=100_000 {
        if current == base {
            return Ok(());
        }
        let revision = engine.load_revision(&current)?;
        let Some(parent) = revision.parents.first() else {
            return Err(Error::Conflict(
                "uploaded source does not descend from the current target".into(),
            ));
        };
        current.clone_from(parent);
    }
    Err(Error::Limit(
        "uploaded source exceeds 100000 first-parent steps".into(),
    ))
}

fn source_change_id(
    native_repository: &str,
    actor: &str,
    repository: &str,
    profile: &str,
    request: &SourceTransferRequest,
) -> ChangeId {
    let mut hash = Sha256::new();
    hash.update(SOURCE_CHANGE_DOMAIN);
    for value in [
        actor,
        repository,
        profile,
        &request.source_repository,
        &request.source_change,
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    ChangeId {
        repository: native_repository.to_owned(),
        id: format!("ws-{:x}", hash.finalize()),
    }
}

fn validate_offer(offer: &SourceOffer, origin: &str, revision: &str) -> Result<()> {
    let bytes = serde_json::to_vec(offer).map_err(native_json)?;
    if bytes.len() > MAX_SOURCE_OFFER_BYTES
        || offer.objects.len() > MAX_SOURCE_OBJECTS
        || offer.origin_repository != origin
        || offer.roots.as_slice() != [revision]
    {
        return Err(HubError::Invalid("invalid bounded source offer".into()));
    }
    Ok(())
}

fn validate_source_request(request: &SourceRequest, offer: &SourceOffer) -> Result<()> {
    if request.version != 1
        || request.missing.len() > offer.objects.len()
        || request.offer_sha256
            != format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(offer).map_err(native_json)?)
            )
    {
        return Err(HubError::Invalid("invalid source request".into()));
    }
    let offered: std::collections::BTreeSet<&str> = offer
        .objects
        .iter()
        .map(|object| object.id.as_str())
        .collect();
    let mut previous = None;
    for missing in &request.missing {
        if !offered.contains(missing.as_str())
            || previous.is_some_and(|value: &str| value >= missing.as_str())
        {
            return Err(HubError::Invalid("invalid source request".into()));
        }
        previous = Some(missing.as_str());
    }
    Ok(())
}

fn validate_provenance(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-:".contains(&byte))
    {
        return Err(HubError::Invalid("invalid source identity".into()));
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(HubError::Invalid("source bundle exceeds 64 MiB".into()));
    }
    Ok(bytes)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(native_json)
}

fn native_json(error: serde_json::Error) -> HubError {
    HubError::Unavailable(error.to_string())
}

fn hub_to_native(error: HubError) -> Error {
    Error::Conflict(format!("source transfer unavailable: {error}"))
}

struct TemporaryFile {
    path: PathBuf,
}

impl TemporaryFile {
    fn create(directory: &Path, kind: &str) -> Result<Self> {
        let temporary = Self::reserve(directory, kind)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary.path)?;
        Ok(temporary)
    }

    fn reserve(directory: &Path, kind: &str) -> Result<Self> {
        Ok(Self {
            path: directory.join(format!("{kind}-{}", super::random_id()?)),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ObjectDescriptor;

    fn transfer(state: SourceTransferState) -> SourceTransfer {
        SourceTransfer {
            version: SOURCE_TRANSFER_VERSION,
            transfer_id: "transfer".into(),
            request_id: "request".into(),
            repository_id: "owner/repository".into(),
            profile_id: "profile".into(),
            native_repository: "a".repeat(32),
            source_repository: "laptop".into(),
            source_change: "change".into(),
            input_revision: "b".repeat(64),
            state,
            request: SourceRequest {
                version: 1,
                offer_sha256: "c".repeat(64),
                missing: Vec::new(),
            },
            subject: None,
            detail: "test".into(),
        }
    }

    fn request() -> SourceTransferRequest {
        SourceTransferRequest {
            version: SOURCE_TRANSFER_VERSION,
            request_id: "request".into(),
            expected_generation: 1,
            source_repository: "laptop".into(),
            source_change: "change".into(),
            source_revision: "b".repeat(64),
            expected_change_version: 0,
            context: ContextDeclaration {
                intent: "test source".into(),
                acceptance_criteria: vec!["exact source".into()],
                scope: vec!["src/lib.rs".into()],
                exclusions: Vec::new(),
                omissions: Vec::new(),
            },
            offer: SourceOffer {
                version: 3,
                profile: crate::sync::SOURCE_PROFILE_V3.into(),
                required_capabilities: Vec::new(),
                origin_repository: "laptop".into(),
                roots: vec!["b".repeat(64)],
                objects: Vec::new(),
            },
        }
    }

    #[test]
    fn server_change_identity_binds_actor_profile_and_laptop_provenance() {
        let request = request();
        let first = source_change_id(
            &"a".repeat(32),
            "actor-one",
            "owner/repository",
            "profile",
            &request,
        );
        let other_actor = source_change_id(
            &"a".repeat(32),
            "actor-two",
            "owner/repository",
            "profile",
            &request,
        );
        let mut other_source = request.clone();
        other_source.source_change = "other".into();
        let other_source = source_change_id(
            &"a".repeat(32),
            "actor-one",
            "owner/repository",
            "profile",
            &other_source,
        );
        assert_eq!(first.repository, "a".repeat(32));
        assert_ne!(first.id, other_actor.id);
        assert_ne!(first.id, other_source.id);
    }

    #[test]
    fn bounded_offer_requires_exact_provenance_and_one_revision() {
        let mut request = request();
        assert!(
            validate_offer(
                &request.offer,
                &request.source_repository,
                &request.source_revision
            )
            .is_ok()
        );
        request.offer.origin_repository = "other".into();
        assert!(
            validate_offer(
                &request.offer,
                &request.source_repository,
                &request.source_revision
            )
            .is_err()
        );
        request.offer.origin_repository = request.source_repository.clone();
        request.offer.roots.push("d".repeat(64));
        assert!(
            validate_offer(
                &request.offer,
                &request.source_repository,
                &request.source_revision
            )
            .is_err()
        );
        request.offer.roots.pop();
        request.offer.objects = (0..=MAX_SOURCE_OBJECTS)
            .map(|index| ObjectDescriptor {
                id: format!("{index:064x}"),
                encoded_bytes: 1,
            })
            .collect();
        assert!(
            validate_offer(
                &request.offer,
                &request.source_repository,
                &request.source_revision
            )
            .is_err()
        );
    }

    #[test]
    fn interrupted_import_is_durably_fenced_unknown() {
        let db = Connection::open_in_memory().unwrap();
        initialize_source_schema(&db).unwrap();
        let transfer = transfer(SourceTransferState::Importing);
        db.execute(
            "INSERT INTO workspace_source_transfers(transfer_id,actor_id,request_id,repository_id,profile_id,request_json,transfer_json,state,bundle_sha256,body_bytes) VALUES(?1,'actor',?2,?3,?4,x'7b7d',?5,'importing',?6,10)",
            params![transfer.transfer_id,transfer.request_id,transfer.repository_id,transfer.profile_id,serde_json::to_vec(&transfer).unwrap(),"d".repeat(64)],
        )
        .unwrap();
        recover_source_transfers(&db).unwrap();
        let bytes: Vec<u8> = db
            .query_row(
                "SELECT transfer_json FROM workspace_source_transfers WHERE transfer_id='transfer' AND state='unknown'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let recovered: SourceTransfer = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recovered.state, SourceTransferState::Unknown);
        assert!(recovered.subject.is_none());
    }

    #[test]
    fn admission_failure_classification_tracks_native_mutation_boundary() {
        assert_eq!(failed_source_state(false), SourceTransferState::Rejected);
        assert_eq!(failed_source_state(true), SourceTransferState::Unknown);
    }

    #[test]
    fn compact_tombstone_accepts_only_the_exact_canonical_request() {
        let stored = StoredSourceRequest {
            route_repository: "owner/repository".into(),
            route_profile: "profile".into(),
            request: request(),
        };
        let bytes = serde_json::to_vec(&stored).unwrap();
        let compact = compact_request(&bytes);
        assert!(compact.starts_with(COMPACT_REQUEST_PREFIX));
        assert_eq!(compact.len(), COMPACT_REQUEST_PREFIX.len() + 64);
        assert!(request_matches(&bytes, &bytes));
        assert!(request_matches(&compact, &bytes));

        let mut changed = stored;
        changed.request.request_id = "different".into();
        let changed = serde_json::to_vec(&changed).unwrap();
        assert!(!request_matches(&compact, &changed));
    }

    #[test]
    fn completed_tombstones_do_not_exhaust_active_transfer_capacity() {
        let db = Connection::open_in_memory().unwrap();
        initialize_source_schema(&db).unwrap();
        for index in 0..=MAX_SOURCE_TRANSFERS {
            let mut retained = transfer(SourceTransferState::Complete);
            retained.transfer_id = format!("transfer-{index}");
            retained.request_id = format!("request-{index}");
            let request = compact_request(retained.request_id.as_bytes());
            db.execute(
                "INSERT INTO workspace_source_transfers(transfer_id,actor_id,request_id,repository_id,profile_id,request_json,transfer_json,state,bundle_sha256,body_bytes) VALUES(?1,'actor',?2,?3,?4,?5,?6,'complete',?7,1)",
                params![retained.transfer_id,retained.request_id,retained.repository_id,retained.profile_id,request,serde_json::to_vec(&retained).unwrap(),"d".repeat(64)],
            )
            .unwrap();
        }
        assert!(ensure_offer_capacity(&db, "owner/repository", "profile", 128, 256).is_ok());
        assert!(retained_profile_bytes(&db, "owner/repository", "profile").unwrap() > 0);
    }
}
