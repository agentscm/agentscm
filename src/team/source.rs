//! Authenticated native reads from one retained team publication.
//!
//! The publication manifest is the only source of backend keys.  This module
//! deliberately keeps the native reader synchronous: the HTTP layer owns the
//! blocking request slot while every object callback rechecks current authority
//! and the retained publication binding.

use std::cell::{Cell, RefCell};
use std::io::{self, Read};
use std::time::{Duration, Instant};

use postgres::Transaction;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::backend::{ImmutableBackend, ObjectKey, SOURCE_PROFILE_V1, SOURCE_PROFILE_V2};
use crate::history::{HeadRelation, history_page, relation_page};
use crate::model::{DiffEntry, Revision};
use crate::store::{EncodedObjectReader, MAX_OBJECT_BYTES, SourceReader, digest, validate_id};
use crate::{Error, Result as StorageResult};

use super::auth::{self, PrincipalState, Role};
use super::publication::{self, Manifest, PublicationAuthority, RepositoryScope, ReserveRequest};
use super::service::TeamService;
use super::{Result, TeamError};

const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024 + 1024;
const MAX_NATIVE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_PAGE_LIMIT: usize = 256;
const MAX_REQUEST_WORK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OBJECT_OPENS: usize = 4_096;
const MAX_CURSOR_BINDING_BYTES: usize = 8_192;
const MAX_CURSOR_STATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_DIFF_CURSOR_BYTES: usize = 64 * 1024;
const MAX_REPOSITORY_CURSORS: i64 = 1_024;
const MAX_PRINCIPAL_CURSORS: i64 = 128;
const MAX_REPOSITORY_CURSOR_BYTES: i64 = 256 * 1024 * 1024;
const MAX_PRINCIPAL_CURSOR_BYTES: i64 = 64 * 1024 * 1024;
const SESSION_RESERVATION_BYTES: i64 = 19 * 1024 * 1024;
const FILE_DIFF_SESSION_RESERVATION_BYTES: i64 = 3 * 1024 * 1024;
const TERMINAL_METADATA_BYTES: i64 = 2_048;
const CURSOR_TTL_MINUTES: i32 = 15;
const CURSOR_COLLISION_ATTEMPTS: usize = 3;
const WORK_DEADLINE: Duration = Duration::from_secs(10);
const CURSOR_DIGEST_DOMAIN: &[u8] = b"agentscm.team.source-cursor.v1\0";
const CURSOR_BINDING_DOMAIN: &[u8] = b"agentscm.team.source-binding.v1\0";
const CANCELLED_BINDING: &str = "{\"cancelled\":true}";

#[cfg(test)]
thread_local! {
    static COMPLETION_TEST_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    static CURSOR_CANDIDATE_TEST_HOOK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceInfo {
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub publication_generation: i64,
    pub publication: String,
    pub manifest_digest: String,
    pub root: String,
    pub profile: String,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeRevision {
    pub source: SourceInfo,
    pub id: String,
    pub revision: Revision,
}

/// File bytes are returned as an octet stream by the transport, so this DTO is
/// intentionally not serializable as JSON.
pub struct NativeFile {
    pub source: SourceInfo,
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeFileEntry {
    pub path: String,
    pub blob: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeFilePage {
    pub source: SourceInfo,
    pub entries: Vec<NativeFileEntry>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryPage {
    pub source: SourceInfo,
    pub entries: Vec<NativeHistoryEntry>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeHistoryEntry {
    pub id: String,
    pub revision: Revision,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeDiffPage {
    pub source: SourceInfo,
    pub left: String,
    pub right: String,
    pub entries: Vec<DiffEntry>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeRelationPage {
    pub source: SourceInfo,
    pub left: String,
    pub right: String,
    pub relation: HeadRelation,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePageRequest {
    pub operation: String,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceCancelRequest {
    pub operation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceCancelOutcome {
    pub scope: RepositoryScope,
    pub authority_generation: i64,
    pub operation: String,
    pub cancelled: bool,
}

#[derive(Clone, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorBinding {
    version: u32,
    scope: RepositoryScope,
    publication_generation: i64,
    publication: String,
    manifest_digest: String,
    root: String,
    profile: String,
    incarnation: String,
    action: &'static str,
    left: Option<String>,
    right: Option<String>,
    limit: usize,
}

struct RetainedSource {
    info: SourceInfo,
    incarnation: String,
    objects: Vec<crate::sync::ObjectDescriptor>,
    principal: String,
    principal_version: i64,
}

struct RequestContext {
    retained: RetainedSource,
    initial_work_bytes: u64,
}

enum PagePlan {
    Initial,
    Advance {
        version: i64,
        token_digest: String,
        continuation: String,
    },
}

enum PreparedPage<T> {
    Compute(PagePlan),
    Cached(T),
}

struct SessionRow {
    binding_digest: String,
    binding: String,
    state: String,
    version: i64,
    current_token_digest: Option<String>,
    current_continuation: Option<String>,
    initial_reply: Option<String>,
    initial_reply_digest: Option<String>,
    previous_token_digest: Option<String>,
    previous_reply: Option<String>,
    previous_reply_digest: Option<String>,
    reserved_bytes: i64,
}

struct CancelSessionRow {
    binding: String,
    state: String,
    version: i64,
    expired: bool,
}

trait CachedPage: Serialize + DeserializeOwned {
    fn source(&self) -> &SourceInfo;
    fn next(&self) -> Option<&str>;
}

macro_rules! cached_page {
    ($type:ty) => {
        impl CachedPage for $type {
            fn source(&self) -> &SourceInfo {
                &self.source
            }

            fn next(&self) -> Option<&str> {
                self.next.as_deref()
            }
        }
    };
}

cached_page!(NativeFilePage);
cached_page!(NativeHistoryPage);
cached_page!(NativeDiffPage);
cached_page!(NativeRelationPage);

struct NativeAccess<'a, B> {
    authority: RefCell<&'a mut PublicationAuthority>,
    backend: &'a B,
    retained: RetainedSource,
    work_bytes: Cell<u64>,
    opens: Cell<usize>,
    deadline: Instant,
}

impl TeamService {
    pub fn source_cancel(&mut self, request: &SourceCancelRequest) -> Result<SourceCancelOutcome> {
        publication::identifier(&request.operation)?;
        let scope = self.authority.scope.clone();
        let credentials = self
            .authority
            .credentials
            .clone()
            .ok_or(TeamError::Unauthorized)?;
        let current;
        let principal;
        let mut result;
        {
            let mut tx = self.authority.client.transaction()?;
            current =
                publication::lock_repository(&mut tx, &scope).map_err(hide_unknown_repository)?;
            principal = auth::authorize(&mut tx, &scope, &credentials, Some(Role::Reader), true)?;
            result = (|| {
                let mut loaded = load_cancel_session(
                    &mut tx,
                    &scope,
                    &principal.principal,
                    principal.version,
                    current,
                    &request.operation,
                )?;
                if loaded.as_ref().is_some_and(|row| row.expired) {
                    delete_session(
                        &mut tx,
                        &scope,
                        &principal.principal,
                        principal.version,
                        current,
                        &request.operation,
                    )?;
                    loaded = None;
                }
                clean_source_cursors(&mut tx, &scope, current)?;
                match loaded {
                    Some(row) if row.state == "cancelled" => {}
                    Some(row) => {
                        let next_version =
                            row.version.checked_add(1).ok_or(TeamError::Indeterminate)?;
                        let reservation = cancelled_reservation(
                            &scope,
                            &principal.principal,
                            &request.operation,
                            &row.binding,
                        )?;
                        let updated = tx.execute(
                            "UPDATE team_source_cursors SET state='cancelled',version=$7,current_token_digest=NULL,current_continuation=NULL,initial_reply=NULL,initial_reply_digest=NULL,previous_token_digest=NULL,previous_reply=NULL,previous_reply_digest=NULL,reserved_bytes=$8,expires_at=clock_timestamp()+$9::integer * interval '1 minute' WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6 AND state=$10 AND version=$11",
                            &[
                                &scope.trust_domain,
                                &scope.repository,
                                &principal.principal,
                                &principal.version,
                                &current,
                                &request.operation,
                                &next_version,
                                &reservation,
                                &CURSOR_TTL_MINUTES,
                                &row.state,
                                &row.version,
                            ],
                        )?;
                        if updated != 1 {
                            return Err(TeamError::Indeterminate);
                        }
                    }
                    None => {
                        let reservation = cancelled_reservation(
                            &scope,
                            &principal.principal,
                            &request.operation,
                            CANCELLED_BINDING,
                        )?;
                        reserve_session_capacity(
                            &mut tx,
                            &scope,
                            &principal.principal,
                            reservation,
                        )?;
                        let binding_digest = cursor_binding_digest(CANCELLED_BINDING);
                        tx.execute(
                            "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,reserved_bytes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'cancelled',0,$9,clock_timestamp()+$10::integer * interval '1 minute')",
                            &[
                                &scope.trust_domain,
                                &scope.repository,
                                &principal.principal,
                                &principal.version,
                                &current,
                                &request.operation,
                                &binding_digest,
                                &CANCELLED_BINDING,
                                &reservation,
                                &CURSOR_TTL_MINUTES,
                            ],
                        )?;
                    }
                }
                Ok(SourceCancelOutcome {
                    scope: scope.clone(),
                    authority_generation: current,
                    operation: request.operation.clone(),
                    cancelled: true,
                })
            })();
            if result.is_ok()
                && let Err(error) = tx.commit()
            {
                result = Err(error.into());
            }
        }
        complete_cancel(
            &mut self.authority,
            &scope,
            &credentials,
            current,
            &principal,
            result,
        )
    }

    pub fn source_revision<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
    ) -> Result<NativeRevision> {
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            let reader = SourceReader::new(&access);
            let revision = checked_revision(&reader, &access, &access.retained.info.root)?;
            let result = NativeRevision {
                source: access.retained.info.clone(),
                id: access.retained.info.root.clone(),
                revision,
            };
            access.check_json_output(&result)?;
            Ok(result)
        })();
        access.complete(result)
    }

    pub fn source_file<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
        path: &str,
    ) -> Result<NativeFile> {
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            let reader = SourceReader::new(&access);
            checked_revision(&reader, &access, &access.retained.info.root)?;
            let bytes = reader.read_file(&access.retained.info.root, path)?;
            if bytes.len() > MAX_NATIVE_OUTPUT_BYTES {
                return Err(Error::Limit("native file exceeds 1MiB response limit".into()).into());
            }
            access.reserve_work(bytes.len() as u64, 4)?;
            Ok(NativeFile {
                source: access.retained.info.clone(),
                path: path.to_owned(),
                bytes,
            })
        })();
        access.complete(result)
    }

    pub fn source_files<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
        page_request: &SourcePageRequest,
    ) -> Result<NativeFilePage> {
        validate_page_request(page_request)?;
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            let binding = access.binding("files", None, None, page_request.limit);
            let plan = match access.prepare_page::<NativeFilePage>(&binding, page_request)? {
                PreparedPage::Cached(page) => return Ok(page),
                PreparedPage::Compute(plan) => plan,
            };
            let reader = SourceReader::new(&access);
            checked_revision(&reader, &access, &access.retained.info.root)?;
            let page = reader.list_files(
                &access.retained.info.root,
                plan.continuation(),
                page_request.limit,
            )?;
            let native_next = page.next;
            let entries = page
                .entries
                .into_iter()
                .map(|entry| NativeFileEntry {
                    path: entry.path,
                    blob: entry.blob,
                })
                .collect();
            access.commit_page(
                &binding,
                page_request,
                plan,
                native_next.as_deref(),
                |next| NativeFilePage {
                    source: access.retained.info.clone(),
                    entries,
                    next,
                },
            )
        })();
        access.complete(result)
    }

    pub fn source_history<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
        page_request: &SourcePageRequest,
    ) -> Result<NativeHistoryPage> {
        validate_page_request(page_request)?;
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            let binding = access.binding("history", None, None, page_request.limit);
            let plan = match access.prepare_page::<NativeHistoryPage>(&binding, page_request)? {
                PreparedPage::Cached(page) => return Ok(page),
                PreparedPage::Compute(plan) => plan,
            };
            let reader = SourceReader::new(&access);
            checked_revision(&reader, &access, &access.retained.info.root)?;
            let page = history_page(
                &reader,
                &access.retained.info.root,
                plan.continuation(),
                page_request.limit,
            )?;
            if let Some(missing) = page.needed_objects.first() {
                return Err(Error::MissingObject(missing.clone()).into());
            }
            for entry in &page.entries {
                validate_revision_profile(&access.retained.info.profile, &entry.revision)?;
            }
            let native_next = page.next;
            let entries = page
                .entries
                .into_iter()
                .map(|entry| NativeHistoryEntry {
                    id: entry.id,
                    revision: entry.revision,
                })
                .collect();
            access.commit_page(
                &binding,
                page_request,
                plan,
                native_next.as_deref(),
                |next| NativeHistoryPage {
                    source: access.retained.info.clone(),
                    entries,
                    next,
                },
            )
        })();
        access.complete(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn source_diff<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
        left: &str,
        right: &str,
        page_request: &SourcePageRequest,
    ) -> Result<NativeDiffPage> {
        validate_page_request(page_request)?;
        validate_id(left)?;
        validate_id(right)?;
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            access.require_member(left)?;
            access.require_member(right)?;
            let binding = access.binding("diff", Some(left), Some(right), page_request.limit);
            let plan = match access.prepare_page::<NativeDiffPage>(&binding, page_request)? {
                PreparedPage::Cached(page) => return Ok(page),
                PreparedPage::Compute(plan) => plan,
            };
            let reader = SourceReader::new(&access);
            checked_revision(&reader, &access, left)?;
            checked_revision(&reader, &access, right)?;
            let page = reader.diff_page(left, right, plan.continuation(), page_request.limit)?;
            if let Some(missing) = page.needed_objects.first() {
                return Err(Error::MissingObject(missing.clone()).into());
            }
            let native_next = page.next;
            let entries = page.entries;
            access.commit_page(
                &binding,
                page_request,
                plan,
                native_next.as_deref(),
                |next| NativeDiffPage {
                    source: access.retained.info.clone(),
                    left: left.to_owned(),
                    right: right.to_owned(),
                    entries,
                    next,
                },
            )
        })();
        access.complete(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn source_relation<B: ImmutableBackend>(
        &mut self,
        backend: &B,
        generation: i64,
        publication: &str,
        left: &str,
        right: &str,
        page_request: &SourcePageRequest,
    ) -> Result<NativeRelationPage> {
        validate_page_request(page_request)?;
        validate_id(left)?;
        validate_id(right)?;
        let deadline = request_deadline();
        let request = load_request(&mut self.authority, generation, publication, deadline)?;
        let access = NativeAccess::new(&mut self.authority, backend, request, deadline);
        let result = (|| {
            access.require_member(left)?;
            access.require_member(right)?;
            let binding = access.binding("relation", Some(left), Some(right), page_request.limit);
            let plan = match access.prepare_page::<NativeRelationPage>(&binding, page_request)? {
                PreparedPage::Cached(page) => return Ok(page),
                PreparedPage::Compute(plan) => plan,
            };
            let reader = SourceReader::new(&access);
            checked_revision(&reader, &access, left)?;
            checked_revision(&reader, &access, right)?;
            let page = relation_page(
                &reader,
                left,
                right,
                plan.continuation(),
                page_request.limit,
            )?;
            if let Some(missing) = page.needed_objects.first() {
                return Err(Error::MissingObject(missing.clone()).into());
            }
            let native_next = page.next;
            let relation = page.relation;
            access.commit_page(
                &binding,
                page_request,
                plan,
                native_next.as_deref(),
                |next| NativeRelationPage {
                    source: access.retained.info.clone(),
                    left: left.to_owned(),
                    right: right.to_owned(),
                    relation,
                    next,
                },
            )
        })();
        access.complete(result)
    }
}

impl<'a, B: ImmutableBackend> NativeAccess<'a, B> {
    fn new(
        authority: &'a mut PublicationAuthority,
        backend: &'a B,
        request: RequestContext,
        deadline: Instant,
    ) -> Self {
        Self {
            authority: RefCell::new(authority),
            backend,
            retained: request.retained,
            work_bytes: Cell::new(request.initial_work_bytes),
            opens: Cell::new(0),
            deadline,
        }
    }

    fn ensure_time(&self) -> StorageResult<()> {
        if Instant::now() > self.deadline {
            return Err(Error::SourceIndeterminate);
        }
        Ok(())
    }

    fn reserve_work(&self, bytes: u64, copies: u64) -> StorageResult<()> {
        let charge = bytes
            .checked_mul(copies)
            .ok_or_else(|| Error::Limit("native read accounting overflow".into()))?;
        let total = self
            .work_bytes
            .get()
            .checked_add(charge)
            .ok_or_else(|| Error::Limit("native read accounting overflow".into()))?;
        if total > MAX_REQUEST_WORK_BYTES {
            return Err(Error::Limit(
                "native read exceeds 256MiB work budget".into(),
            ));
        }
        self.work_bytes.set(total);
        Ok(())
    }

    fn require_member(&self, id: &str) -> StorageResult<u64> {
        self.retained
            .objects
            .binary_search_by(|descriptor| descriptor.id.as_str().cmp(id))
            .map(|index| self.retained.objects[index].encoded_bytes)
            .map_err(|_| Error::SourceDenied)
    }

    fn recheck(&self) -> StorageResult<()> {
        let mut authority = self.authority.borrow_mut();
        let scope = authority.scope.clone();
        let credentials = authority.credentials.clone().ok_or(Error::SourceDenied)?;
        let mut tx = authority
            .client
            .transaction()
            .map_err(|_| Error::SourceIndeterminate)?;
        recheck_transaction(&mut tx, &scope, &credentials, &self.retained)?;
        tx.commit().map_err(|_| Error::SourceIndeterminate)?;
        // Post-I/O denial must dominate a cooperative deadline. Callers
        // preflight time before opening; final checks still observe revocation.
        self.ensure_time()?;
        Ok(())
    }

    fn binding(
        &self,
        action: &'static str,
        left: Option<&str>,
        right: Option<&str>,
        limit: usize,
    ) -> CursorBinding {
        CursorBinding {
            version: 1,
            scope: self.retained.info.scope.clone(),
            publication_generation: self.retained.info.publication_generation,
            publication: self.retained.info.publication.clone(),
            manifest_digest: self.retained.info.manifest_digest.clone(),
            root: self.retained.info.root.clone(),
            profile: self.retained.info.profile.clone(),
            incarnation: self.retained.incarnation.clone(),
            action,
            left: left.map(str::to_owned),
            right: right.map(str::to_owned),
            limit,
        }
    }

    fn complete<T>(&self, result: Result<T>) -> Result<T> {
        // Authorization and the retained publication binding always win over
        // native success and over details learned from a failed native read.
        #[cfg(test)]
        COMPLETION_TEST_HOOK.with(|hook| {
            if let Some(action) = hook.borrow_mut().take() {
                action();
            }
        });
        self.recheck()?;
        result
    }

    fn prepare_page<T: CachedPage>(
        &self,
        binding: &CursorBinding,
        request: &SourcePageRequest,
    ) -> Result<PreparedPage<T>> {
        self.ensure_time()?;
        let encoded_binding = publication::json(binding)?;
        if encoded_binding.len() > MAX_CURSOR_BINDING_BYTES {
            return Err(Error::Limit("source cursor binding exceeds 8192 bytes".into()).into());
        }
        let binding_digest = cursor_binding_digest(&encoded_binding);
        let mut authority = self.authority.borrow_mut();
        let scope = authority.scope.clone();
        let credentials = authority
            .credentials
            .clone()
            .ok_or(TeamError::Unauthorized)?;
        let mut tx = authority.client.transaction()?;
        let current =
            publication::lock_repository(&mut tx, &scope).map_err(hide_unknown_repository)?;
        let principal = authorize_reader(&mut tx, &scope, &credentials)?;
        require_captured_identity(current, &principal, &self.retained)?;
        recheck_retained_row(&mut tx, &scope, &self.retained)?;

        let loaded = load_session(
            &mut tx,
            &scope,
            &principal.principal,
            principal.version,
            current,
            &request.operation,
        )?;
        if loaded.as_ref().is_some_and(|(_, expired)| *expired) {
            delete_session(
                &mut tx,
                &scope,
                &principal.principal,
                principal.version,
                current,
                &request.operation,
            )?;
            tx.commit()?;
            return Err(TeamError::Expired);
        }
        clean_source_cursors(&mut tx, &scope, current)?;

        let Some((row, _)) = loaded else {
            if request.cursor.is_some() {
                tx.commit()?;
                return Err(TeamError::Unauthorized);
            }
            let reservation = session_reservation(binding.action);
            reserve_session_capacity(&mut tx, &scope, &principal.principal, reservation)?;
            tx.execute(
                "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,reserved_bytes,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'pending',0,$9,clock_timestamp()+$10::integer * interval '1 minute')",
                &[
                    &scope.trust_domain,
                    &scope.repository,
                    &principal.principal,
                    &principal.version,
                    &current,
                    &request.operation,
                    &binding_digest,
                    &encoded_binding,
                    &reservation,
                    &CURSOR_TTL_MINUTES,
                ],
            )?;
            tx.commit()?;
            return Ok(PreparedPage::Compute(PagePlan::Initial));
        };

        self.charge_session_row(&row)?;
        if row.state == "cancelled" {
            return Err(TeamError::Indeterminate);
        }
        require_session_binding(&row, &binding_digest, &encoded_binding)?;
        let prepared = match request.cursor.as_deref() {
            None if row.state == "pending" && row.version == 0 => {
                PreparedPage::Compute(PagePlan::Initial)
            }
            None if row.version <= 2 => {
                PreparedPage::Cached(decode_initial_reply::<T>(&row, &self.retained.info)?)
            }
            None => return Err(TeamError::Indeterminate),
            Some(token) => {
                let token_digest = cursor_token_digest(token)?;
                if row.previous_token_digest.as_deref() == Some(&token_digest) {
                    PreparedPage::Cached(decode_previous_reply::<T>(&row, &self.retained.info)?)
                } else if row.state == "active"
                    && row.current_token_digest.as_deref() == Some(&token_digest)
                {
                    PreparedPage::Compute(PagePlan::Advance {
                        version: row.version,
                        token_digest,
                        continuation: row.current_continuation.ok_or(TeamError::Indeterminate)?,
                    })
                } else {
                    return Err(TeamError::Indeterminate);
                }
            }
        };
        tx.commit()?;
        Ok(prepared)
    }

    fn commit_page<T: CachedPage>(
        &self,
        binding: &CursorBinding,
        request: &SourcePageRequest,
        plan: PagePlan,
        native_next: Option<&str>,
        build: impl FnOnce(Option<String>) -> T,
    ) -> Result<T> {
        self.ensure_time()?;
        let state_cap = native_state_cap(binding.action);
        if native_next.is_some_and(|next| next.len() > state_cap) {
            return Err(Error::Limit(format!(
                "native continuation exceeds {} byte action limit",
                state_cap
            ))
            .into());
        }
        if let Some(next) = native_next {
            self.reserve_work(next.len() as u64, 4)?;
        }
        let encoded_binding = publication::json(binding)?;
        let binding_digest = cursor_binding_digest(&encoded_binding);
        let mut authority = self.authority.borrow_mut();
        let scope = authority.scope.clone();
        let credentials = authority
            .credentials
            .clone()
            .ok_or(TeamError::Unauthorized)?;
        let mut tx = authority.client.transaction()?;
        let current =
            publication::lock_repository(&mut tx, &scope).map_err(hide_unknown_repository)?;
        let principal = authorize_reader(&mut tx, &scope, &credentials)?;
        require_captured_identity(current, &principal, &self.retained)?;
        recheck_retained_row(&mut tx, &scope, &self.retained)?;
        let Some((row, expired)) = load_session(
            &mut tx,
            &scope,
            &principal.principal,
            principal.version,
            current,
            &request.operation,
        )?
        else {
            return Err(TeamError::Indeterminate);
        };
        if expired {
            delete_session(
                &mut tx,
                &scope,
                &principal.principal,
                principal.version,
                current,
                &request.operation,
            )?;
            tx.commit()?;
            return Err(TeamError::Expired);
        }
        self.charge_session_row(&row)?;
        require_session_binding(&row, &binding_digest, &encoded_binding)?;

        let winner = match &plan {
            PagePlan::Initial => row.state == "pending" && row.version == 0,
            PagePlan::Advance {
                version,
                token_digest,
                ..
            } => {
                row.state == "active"
                    && row.version == *version
                    && row.current_token_digest.as_deref() == Some(token_digest)
            }
        };
        if !winner {
            let cached = match &plan {
                PagePlan::Initial if row.version <= 2 => {
                    decode_initial_reply::<T>(&row, &self.retained.info)?
                }
                PagePlan::Advance { token_digest, .. }
                    if row.previous_token_digest.as_deref() == Some(token_digest) =>
                {
                    decode_previous_reply::<T>(&row, &self.retained.info)?
                }
                _ => return Err(TeamError::Indeterminate),
            };
            tx.commit()?;
            return Ok(cached);
        }

        let (successor, successor_digest) = match native_next {
            Some(_) => {
                let (token, token_digest) = generate_unique_cursor(&mut tx, &scope)?;
                (Some(token), Some(token_digest))
            }
            None => (None, None),
        };
        let value = build(successor);
        let reply = self.encode_reply(&value)?;
        let reply_digest = digest(reply.as_bytes());
        let next_version = row.version.checked_add(1).ok_or(TeamError::Indeterminate)?;
        let state = if native_next.is_some() {
            "active"
        } else {
            "terminal"
        };
        let terminal_reservation = if state == "terminal" {
            Some(terminal_reservation(
                &scope,
                &principal.principal,
                &request.operation,
                &encoded_binding,
                if matches!(&plan, PagePlan::Initial) {
                    &reply
                } else {
                    row.initial_reply
                        .as_deref()
                        .ok_or(TeamError::Indeterminate)?
                },
                if matches!(&plan, PagePlan::Initial) {
                    None
                } else {
                    Some(reply.as_str())
                },
            )?)
        } else {
            None
        };
        let updated = match plan {
            PagePlan::Initial => tx.execute(
                "UPDATE team_source_cursors SET state=$7,version=1,current_token_digest=$8,current_continuation=$9,initial_reply=$10,initial_reply_digest=$11,reserved_bytes=COALESCE($12,reserved_bytes),expires_at=clock_timestamp()+$13::integer * interval '1 minute' WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6 AND state='pending' AND version=0",
                &[
                    &scope.trust_domain,
                    &scope.repository,
                    &principal.principal,
                    &principal.version,
                    &current,
                    &request.operation,
                    &state,
                    &successor_digest,
                    &native_next,
                    &reply,
                    &reply_digest,
                    &terminal_reservation,
                    &CURSOR_TTL_MINUTES,
                ],
            )?,
            PagePlan::Advance {
                version,
                token_digest,
                ..
            } => tx.execute(
                "UPDATE team_source_cursors SET state=$9,version=$8,current_token_digest=$10,current_continuation=$11,previous_token_digest=$12,previous_reply=$13,previous_reply_digest=$14,reserved_bytes=COALESCE($15,reserved_bytes),expires_at=clock_timestamp()+$16::integer * interval '1 minute' WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6 AND state='active' AND version=$7 AND current_token_digest=$12",
                &[
                    &scope.trust_domain,
                    &scope.repository,
                    &principal.principal,
                    &principal.version,
                    &current,
                    &request.operation,
                    &version,
                    &next_version,
                    &state,
                    &successor_digest,
                    &native_next,
                    &token_digest,
                    &reply,
                    &reply_digest,
                    &terminal_reservation,
                    &CURSOR_TTL_MINUTES,
                ],
            )?,
        };
        if updated != 1 {
            return Err(TeamError::Indeterminate);
        }
        tx.commit()?;
        Ok(value)
    }

    fn charge_session_row(&self, row: &SessionRow) -> Result<()> {
        self.reserve_work(session_loaded_bytes(row)?, 4)?;
        Ok(())
    }

    fn encode_reply(&self, value: &impl Serialize) -> Result<String> {
        self.ensure_time()?;
        let reply = publication::json(value)?;
        if reply.len() > MAX_NATIVE_OUTPUT_BYTES {
            return Err(Error::Limit("native response exceeds 1MiB limit".into()).into());
        }
        self.reserve_work(reply.len() as u64, 4)?;
        Ok(reply)
    }

    fn check_json_output(&self, value: &impl Serialize) -> Result<()> {
        self.encode_reply(value).map(|_| ())
    }
}

impl PagePlan {
    fn continuation(&self) -> Option<&str> {
        match self {
            Self::Initial => None,
            Self::Advance { continuation, .. } => Some(continuation),
        }
    }
}

impl<B: ImmutableBackend> EncodedObjectReader for NativeAccess<'_, B> {
    fn open_encoded(&self, id: &str) -> StorageResult<Box<dyn Read + '_>> {
        validate_id(id)?;
        self.ensure_time()?;
        let expected = self.require_member(id)?;
        let opened = self.opens.get();
        if opened >= MAX_OBJECT_OPENS {
            return Err(Error::SourceIndeterminate);
        }
        self.reserve_work(expected, 4)?;
        self.opens.set(opened + 1);
        self.recheck()?;
        let key = ObjectKey::new(
            &self.retained.info.scope.trust_domain,
            &self.retained.info.scope.repository,
            &self.retained.info.profile,
            id,
        )?;
        let raw = match self.backend.open_encoded(&key, &self.retained.incarnation) {
            Ok(raw) => raw,
            Err(Error::MissingObject(missing)) if missing == id => {
                self.recheck()?;
                return Err(Error::MissingObject(missing));
            }
            Err(Error::SourceDenied) => return Err(Error::SourceDenied),
            Err(_) => return Err(Error::SourceIndeterminate),
        };
        Ok(Box::new(CheckedStream {
            raw,
            access: self,
            expected,
            seen: 0,
            complete: false,
        }))
    }
}

struct CheckedStream<'reader, 'context, B> {
    raw: Box<dyn Read + Send + 'reader>,
    access: &'reader NativeAccess<'context, B>,
    expected: u64,
    seen: u64,
    complete: bool,
}

impl<B: ImmutableBackend> Read for CheckedStream<'_, '_, B> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.complete || buffer.is_empty() {
            return Ok(0);
        }
        self.access.ensure_time().map_err(storage_io)?;
        let remaining = self.expected.saturating_add(1).saturating_sub(self.seen);
        let cap = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = match self.raw.read(&mut buffer[..cap]) {
            Ok(read) => read,
            Err(error) => {
                self.access.recheck().map_err(storage_io)?;
                return Err(error);
            }
        };
        if read == 0 {
            self.access.recheck().map_err(storage_io)?;
            if self.seen != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "encoded object shorter than retained manifest descriptor",
                ));
            }
            self.complete = true;
            return Ok(0);
        }
        self.seen = self
            .seen
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("encoded object byte count overflow"))?;
        if self.seen > self.expected {
            self.access.recheck().map_err(storage_io)?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded object longer than retained manifest descriptor",
            ));
        }
        Ok(read)
    }
}

fn load_request(
    authority: &mut PublicationAuthority,
    generation: i64,
    publication_id: &str,
    deadline: Instant,
) -> Result<RequestContext> {
    publication::identifier(publication_id)?;
    if generation <= 0 {
        return Err(Error::Invalid("publication generation must be positive".into()).into());
    }
    if Instant::now() > deadline {
        return Err(TeamError::Unavailable);
    }
    let scope = authority.scope.clone();
    let credentials = authority
        .credentials
        .clone()
        .ok_or(TeamError::Unauthorized)?;
    let mut tx = authority.client.transaction()?;
    let current = publication::lock_repository(&mut tx, &scope).map_err(hide_unknown_repository)?;
    let principal = authorize_reader(&mut tx, &scope, &credentials)?;
    let row = tx
        .query_opt(
            "SELECT p.request,CASE WHEN octet_length(p.manifest) <= $5 THEN p.manifest ELSE NULL END,octet_length(p.manifest),p.manifest_digest,p.source_profile,r.root FROM team_publications p JOIN team_roots r USING(trust_domain,repository,generation,publication) WHERE p.trust_domain=$1 AND p.repository=$2 AND p.generation=$3 AND p.publication=$4 AND p.state='complete' FOR SHARE OF p,r",
            &[
                &scope.trust_domain,
                &scope.repository,
                &generation,
                &publication_id,
                &(MAX_MANIFEST_BYTES as i32),
            ],
        )?
        .ok_or_else(|| Error::NotFound("retained complete publication".into()))?;
    let request: ReserveRequest = publication::parse(&row.get::<_, String>(0))?;
    let manifest_bytes: Option<i32> = row.get(2);
    if manifest_bytes.is_some_and(|bytes| bytes < 0 || bytes as usize > MAX_MANIFEST_BYTES) {
        return Err(Error::Limit("retained manifest exceeds bound".into()).into());
    }
    let encoded: Option<String> = row.get(1);
    let encoded = encoded.ok_or(TeamError::Unavailable)?;
    let initial_work_bytes = (encoded.len() as u64)
        .checked_mul(4)
        .ok_or_else(|| Error::Limit("manifest accounting overflow".into()))?;
    if initial_work_bytes > MAX_REQUEST_WORK_BYTES {
        return Err(Error::Limit("retained manifest exceeds request work budget".into()).into());
    }
    let manifest_digest: Option<String> = row.get(3);
    let manifest_digest = manifest_digest.ok_or(TeamError::Unavailable)?;
    validate_id(&manifest_digest)?;
    if digest(encoded.as_bytes()) != manifest_digest {
        return Err(TeamError::Unavailable);
    }
    let stored_profile: String = row.get(4);
    validate_profile(&stored_profile)?;
    let retained_root: String = row.get(5);
    validate_id(&retained_root)?;
    let manifest: Manifest = publication::parse(&encoded)?;
    validate_manifest(&scope, &request, &manifest, &stored_profile, &retained_root)?;
    let info = SourceInfo {
        scope: scope.clone(),
        authority_generation: current,
        publication_generation: generation,
        publication: publication_id.to_owned(),
        manifest_digest,
        root: retained_root,
        profile: stored_profile,
    };
    let retained = RetainedSource {
        info,
        incarnation: manifest.incarnation,
        objects: manifest.objects,
        principal: principal.principal,
        principal_version: principal.version,
    };
    tx.commit()?;
    Ok(RequestContext {
        retained,
        initial_work_bytes,
    })
}

fn validate_manifest(
    scope: &RepositoryScope,
    request: &ReserveRequest,
    manifest: &Manifest,
    stored_profile: &str,
    retained_root: &str,
) -> Result<()> {
    if manifest.profile != stored_profile
        || manifest.root != retained_root
        || request.root != retained_root
    {
        return Err(TeamError::Unavailable);
    }
    validate_profile(&manifest.profile)?;
    publication::identifier(&manifest.incarnation)?;
    if manifest.objects.is_empty() || manifest.objects.len() > 100_000 {
        return Err(Error::Limit("retained manifest object count is invalid".into()).into());
    }
    let mut previous: Option<&str> = None;
    let mut root_found = false;
    for object in &manifest.objects {
        validate_id(&object.id)?;
        if object.encoded_bytes == 0 || object.encoded_bytes > MAX_OBJECT_BYTES {
            return Err(Error::Limit("retained object descriptor exceeds bound".into()).into());
        }
        if previous.is_some_and(|id| id >= object.id.as_str()) {
            return Err(Error::Invalid(
                "retained object descriptors are not canonical and unique".into(),
            )
            .into());
        }
        ObjectKey::new(
            &scope.trust_domain,
            &scope.repository,
            &manifest.profile,
            &object.id,
        )?;
        root_found |= object.id == retained_root;
        previous = Some(&object.id);
    }
    if !root_found {
        return Err(TeamError::Unavailable);
    }
    Ok(())
}

fn validate_profile(profile: &str) -> StorageResult<()> {
    if matches!(profile, SOURCE_PROFILE_V1 | SOURCE_PROFILE_V2) {
        Ok(())
    } else {
        Err(Error::Invalid("unsupported retained source profile".into()))
    }
}

fn validate_revision_profile(profile: &str, revision: &Revision) -> StorageResult<()> {
    match (profile, revision.version) {
        (SOURCE_PROFILE_V1, 1) | (SOURCE_PROFILE_V2, 1..=2) => Ok(()),
        _ => Err(Error::Invalid(
            "revision encoding does not match retained source profile".into(),
        )),
    }
}

fn checked_revision(
    reader: &SourceReader<'_>,
    access: &NativeAccess<'_, impl ImmutableBackend>,
    id: &str,
) -> Result<Revision> {
    access.require_member(id)?;
    let revision = reader.load_revision(id)?;
    validate_revision_profile(&access.retained.info.profile, &revision)?;
    Ok(revision)
}

fn recheck_transaction(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credentials: &super::auth::Credentials,
    retained: &RetainedSource,
) -> StorageResult<()> {
    let current = super::service::lock_read(tx, scope).map_err(source_auth_error)?;
    let principal = auth::authorize(tx, scope, credentials, Some(Role::Reader), false)
        .map_err(source_auth_error)?;
    require_captured_identity(current, &principal, retained).map_err(|_| Error::SourceDenied)?;
    recheck_retained_row(tx, scope, retained).map_err(|_| Error::SourceIndeterminate)
}

fn recheck_retained_row(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    retained: &RetainedSource,
) -> Result<()> {
    let row = tx.query_opt(
        "SELECT p.manifest_digest,p.source_profile,r.root FROM team_publications p JOIN team_roots r USING(trust_domain,repository,generation,publication) WHERE p.trust_domain=$1 AND p.repository=$2 AND p.generation=$3 AND p.publication=$4 AND p.state='complete' FOR SHARE OF p,r",
        &[
            &scope.trust_domain,
            &scope.repository,
            &retained.info.publication_generation,
            &retained.info.publication,
        ],
    )?;
    let Some(row) = row else {
        return Err(TeamError::Indeterminate);
    };
    if row.get::<_, Option<String>>(0).as_deref() != Some(&retained.info.manifest_digest)
        || row.get::<_, String>(1) != retained.info.profile
        || row.get::<_, String>(2) != retained.info.root
    {
        return Err(TeamError::Indeterminate);
    }
    Ok(())
}

fn authorize_reader(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    credentials: &super::auth::Credentials,
) -> Result<PrincipalState> {
    auth::authorize(tx, scope, credentials, Some(Role::Reader), false)
}

fn require_captured_identity(
    current: i64,
    principal: &PrincipalState,
    retained: &RetainedSource,
) -> Result<()> {
    if current != retained.info.authority_generation
        || principal.principal != retained.principal
        || principal.version != retained.principal_version
    {
        return Err(TeamError::Denied);
    }
    Ok(())
}

fn complete_cancel<T>(
    authority: &mut PublicationAuthority,
    scope: &RepositoryScope,
    credentials: &super::auth::Credentials,
    captured_generation: i64,
    captured_principal: &PrincipalState,
    result: Result<T>,
) -> Result<T> {
    #[cfg(test)]
    COMPLETION_TEST_HOOK.with(|hook| {
        if let Some(action) = hook.borrow_mut().take() {
            action();
        }
    });
    let mut tx = authority
        .client
        .transaction()
        .map_err(|_| Error::SourceIndeterminate)?;
    let current = super::service::lock_read(&mut tx, scope).map_err(source_auth_error)?;
    let principal = auth::authorize(&mut tx, scope, credentials, Some(Role::Reader), true)
        .map_err(source_auth_error)?;
    if current != captured_generation
        || principal.principal != captured_principal.principal
        || principal.version != captured_principal.version
    {
        return Err(Error::SourceDenied.into());
    }
    tx.commit().map_err(|_| Error::SourceIndeterminate)?;
    result
}

fn load_cancel_session(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
    principal_version: i64,
    authority_generation: i64,
    operation: &str,
) -> Result<Option<CancelSessionRow>> {
    let row = tx.query_opt(
        "SELECT binding,state,version,expires_at <= clock_timestamp() FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6 FOR UPDATE",
        &[
            &scope.trust_domain,
            &scope.repository,
            &principal,
            &principal_version,
            &authority_generation,
            &operation,
        ],
    )?;
    Ok(row.map(|row| CancelSessionRow {
        binding: row.get(0),
        state: row.get(1),
        version: row.get(2),
        expired: row.get(3),
    }))
}

fn load_session(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
    principal_version: i64,
    authority_generation: i64,
    operation: &str,
) -> Result<Option<(SessionRow, bool)>> {
    let row = tx.query_opt(
        "SELECT binding_digest,binding,state,version,current_token_digest,current_continuation,initial_reply,initial_reply_digest,previous_token_digest,previous_reply,previous_reply_digest,reserved_bytes,expires_at <= clock_timestamp() FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6 FOR UPDATE",
        &[
            &scope.trust_domain,
            &scope.repository,
            &principal,
            &principal_version,
            &authority_generation,
            &operation,
        ],
    )?;
    Ok(row.map(|row| {
        let expired = row.get(12);
        (
            SessionRow {
                binding_digest: row.get(0),
                binding: row.get(1),
                state: row.get(2),
                version: row.get(3),
                current_token_digest: row.get(4),
                current_continuation: row.get(5),
                initial_reply: row.get(6),
                initial_reply_digest: row.get(7),
                previous_token_digest: row.get(8),
                previous_reply: row.get(9),
                previous_reply_digest: row.get(10),
                reserved_bytes: row.get(11),
            },
            expired,
        )
    }))
}

fn delete_session(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
    principal_version: i64,
    authority_generation: i64,
    operation: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2 AND principal=$3 AND principal_version=$4 AND authority_generation=$5 AND operation=$6",
        &[
            &scope.trust_domain,
            &scope.repository,
            &principal,
            &principal_version,
            &authority_generation,
            &operation,
        ],
    )?;
    Ok(())
}

fn reserve_session_capacity(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    principal: &str,
    reservation: i64,
) -> Result<()> {
    let row = tx.query_one(
        "SELECT count(*),COALESCE(sum(reserved_bytes),0)::bigint,count(*) FILTER (WHERE principal=$3),COALESCE(sum(reserved_bytes) FILTER (WHERE principal=$3),0)::bigint FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2",
        &[&scope.trust_domain, &scope.repository, &principal],
    )?;
    let repository_count: i64 = row.get(0);
    let repository_bytes: i64 = row.get(1);
    let principal_count: i64 = row.get(2);
    let principal_bytes: i64 = row.get(3);
    let repository_fits = repository_bytes
        .checked_add(reservation)
        .is_some_and(|bytes| bytes <= MAX_REPOSITORY_CURSOR_BYTES);
    let principal_fits = principal_bytes
        .checked_add(reservation)
        .is_some_and(|bytes| bytes <= MAX_PRINCIPAL_CURSOR_BYTES);
    if repository_count >= MAX_REPOSITORY_CURSORS
        || principal_count >= MAX_PRINCIPAL_CURSORS
        || !repository_fits
        || !principal_fits
    {
        return Err(Error::Limit("source cursor capacity exceeded".into()).into());
    }
    Ok(())
}

fn native_state_cap(action: &str) -> usize {
    match action {
        "files" | "diff" => MAX_FILE_DIFF_CURSOR_BYTES,
        "history" | "relation" => MAX_CURSOR_STATE_BYTES,
        _ => MAX_CURSOR_STATE_BYTES,
    }
}

fn session_reservation(action: &str) -> i64 {
    match action {
        "files" | "diff" => FILE_DIFF_SESSION_RESERVATION_BYTES,
        "history" | "relation" => SESSION_RESERVATION_BYTES,
        _ => SESSION_RESERVATION_BYTES,
    }
}

fn require_session_binding(
    row: &SessionRow,
    expected_digest: &str,
    expected_binding: &str,
) -> Result<()> {
    if row.binding.len() > MAX_CURSOR_BINDING_BYTES
        || row.binding != expected_binding
        || row.binding_digest != expected_digest
        || cursor_binding_digest(&row.binding) != row.binding_digest
    {
        return Err(TeamError::Unauthorized);
    }
    Ok(())
}

fn session_loaded_bytes(row: &SessionRow) -> StorageResult<u64> {
    let mut total = row
        .binding_digest
        .len()
        .checked_add(row.binding.len())
        .and_then(|bytes| bytes.checked_add(row.state.len()))
        .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    for value in [
        row.current_token_digest.as_deref(),
        row.current_continuation.as_deref(),
        row.initial_reply.as_deref(),
        row.initial_reply_digest.as_deref(),
        row.previous_token_digest.as_deref(),
        row.previous_reply.as_deref(),
        row.previous_reply_digest.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        total = total
            .checked_add(value.len())
            .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    }
    let total = u64::try_from(total)
        .map_err(|_| Error::Limit("source cursor accounting overflow".into()))?;
    if row.reserved_bytes <= 0 || row.reserved_bytes > SESSION_RESERVATION_BYTES {
        return Err(Error::SourceIndeterminate);
    }
    Ok(total)
}

fn decode_initial_reply<T: CachedPage>(row: &SessionRow, source: &SourceInfo) -> Result<T> {
    let reply = row
        .initial_reply
        .as_deref()
        .ok_or(TeamError::Indeterminate)?;
    let reply_digest = row
        .initial_reply_digest
        .as_deref()
        .ok_or(TeamError::Indeterminate)?;
    decode_cached_reply(row, source, reply, reply_digest, true)
}

fn decode_previous_reply<T: CachedPage>(row: &SessionRow, source: &SourceInfo) -> Result<T> {
    let reply = row
        .previous_reply
        .as_deref()
        .ok_or(TeamError::Indeterminate)?;
    let reply_digest = row
        .previous_reply_digest
        .as_deref()
        .ok_or(TeamError::Indeterminate)?;
    decode_cached_reply(row, source, reply, reply_digest, false)
}

fn decode_cached_reply<T: CachedPage>(
    row: &SessionRow,
    source: &SourceInfo,
    reply: &str,
    reply_digest: &str,
    initial: bool,
) -> Result<T> {
    if reply.len() > MAX_NATIVE_OUTPUT_BYTES || digest(reply.as_bytes()) != reply_digest {
        return Err(TeamError::Indeterminate);
    }
    let value: T = publication::parse(reply).map_err(|_| TeamError::Indeterminate)?;
    if value.source() != source {
        return Err(TeamError::Indeterminate);
    }
    match value.next() {
        Some(token) => {
            let token_digest = cursor_token_digest(token).map_err(|_| TeamError::Indeterminate)?;
            let expected = if initial {
                row.current_token_digest.as_deref() == Some(&token_digest)
                    || row.previous_token_digest.as_deref() == Some(&token_digest)
            } else {
                row.state == "active" && row.current_token_digest.as_deref() == Some(&token_digest)
            };
            if !expected {
                return Err(TeamError::Indeterminate);
            }
        }
        None if row.state != "terminal" => return Err(TeamError::Indeterminate),
        None => {}
    }
    Ok(value)
}

fn generate_unique_cursor(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
) -> Result<(String, String)> {
    for _ in 0..CURSOR_COLLISION_ATTEMPTS {
        let token = next_cursor_candidate()?;
        let token_digest = cursor_token_digest(&token)?;
        let collision: bool = tx
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM team_source_cursors WHERE trust_domain=$1 AND repository=$2 AND (current_token_digest=$3 OR previous_token_digest=$3))",
                &[&scope.trust_domain, &scope.repository, &token_digest],
            )?
            .get(0);
        if !collision {
            return Ok((token, token_digest));
        }
    }
    Err(TeamError::Unavailable)
}

fn next_cursor_candidate() -> Result<String> {
    #[cfg(test)]
    if let Some(candidate) = CURSOR_CANDIDATE_TEST_HOOK.with(|candidates| {
        let mut candidates = candidates.borrow_mut();
        (!candidates.is_empty()).then(|| candidates.remove(0))
    }) {
        return Ok(candidate);
    }
    Ok(auth::generate_secret()?.expose().to_owned())
}

fn terminal_reservation(
    scope: &RepositoryScope,
    principal: &str,
    operation: &str,
    binding: &str,
    initial_reply: &str,
    previous_reply: Option<&str>,
) -> Result<i64> {
    let mut bytes = scope
        .trust_domain
        .len()
        .checked_add(scope.repository.len())
        .and_then(|value| value.checked_add(principal.len()))
        .and_then(|value| value.checked_add(operation.len()))
        .and_then(|value| value.checked_add(binding.len()))
        .and_then(|value| value.checked_add("terminal".len()))
        .and_then(|value| value.checked_add(initial_reply.len()))
        .and_then(|value| value.checked_add(128))
        .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    if let Some(previous_reply) = previous_reply {
        bytes = bytes
            .checked_add(previous_reply.len())
            .and_then(|value| value.checked_add(128))
            .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    }
    let bytes = i64::try_from(bytes)
        .map_err(|_| Error::Limit("source cursor accounting overflow".into()))?
        .checked_add(TERMINAL_METADATA_BYTES)
        .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    if bytes > SESSION_RESERVATION_BYTES {
        return Err(Error::Limit("terminal source cursor exceeds storage bound".into()).into());
    }
    Ok(bytes)
}

fn cancelled_reservation(
    scope: &RepositoryScope,
    principal: &str,
    operation: &str,
    binding: &str,
) -> Result<i64> {
    let bytes = scope
        .trust_domain
        .len()
        .checked_add(scope.repository.len())
        .and_then(|value| value.checked_add(principal.len()))
        .and_then(|value| value.checked_add(operation.len()))
        .and_then(|value| value.checked_add(binding.len()))
        .and_then(|value| value.checked_add("cancelled".len()))
        .and_then(|value| value.checked_add(64))
        .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    let bytes = i64::try_from(bytes)
        .map_err(|_| Error::Limit("source cursor accounting overflow".into()))?
        .checked_add(TERMINAL_METADATA_BYTES)
        .ok_or_else(|| Error::Limit("source cursor accounting overflow".into()))?;
    if bytes > SESSION_RESERVATION_BYTES {
        return Err(Error::Limit("cancelled source cursor exceeds storage bound".into()).into());
    }
    Ok(bytes)
}

fn clean_source_cursors(
    tx: &mut Transaction<'_>,
    scope: &RepositoryScope,
    current_generation: i64,
) -> Result<()> {
    tx.execute(
        "DELETE FROM team_source_cursors c WHERE c.trust_domain=$1 AND c.repository=$2 AND (c.expires_at <= clock_timestamp() OR c.authority_generation <> $3 OR NOT EXISTS (SELECT 1 FROM team_principals p WHERE p.trust_domain=c.trust_domain AND p.repository=c.repository AND p.principal=c.principal AND p.enabled AND p.version=c.principal_version))",
        &[&scope.trust_domain, &scope.repository, &current_generation],
    )?;
    Ok(())
}

fn cursor_token_digest(token: &str) -> Result<String> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TeamError::Unauthorized);
    }
    let mut value = Vec::with_capacity(CURSOR_DIGEST_DOMAIN.len() + token.len());
    value.extend_from_slice(CURSOR_DIGEST_DOMAIN);
    value.extend_from_slice(token.as_bytes());
    Ok(digest(&value))
}

fn cursor_binding_digest(binding: &str) -> String {
    let mut value = Vec::with_capacity(CURSOR_BINDING_DOMAIN.len() + binding.len());
    value.extend_from_slice(CURSOR_BINDING_DOMAIN);
    value.extend_from_slice(binding.as_bytes());
    digest(&value)
}

fn validate_page_request(request: &SourcePageRequest) -> Result<()> {
    validate_page_limit(request.limit)?;
    publication::identifier(&request.operation)?;
    if let Some(cursor) = request.cursor.as_deref() {
        cursor_token_digest(cursor)?;
    }
    Ok(())
}

fn validate_page_limit(limit: usize) -> StorageResult<()> {
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(Error::Limit(format!(
            "native page limit must be between 1 and {MAX_PAGE_LIMIT}"
        )));
    }
    Ok(())
}

fn request_deadline() -> Instant {
    Instant::now()
        .checked_add(WORK_DEADLINE)
        .unwrap_or_else(Instant::now)
}

fn cursor_team_error(error: TeamError) -> Error {
    match error {
        TeamError::Unauthorized | TeamError::Denied => Error::SourceDenied,
        _ => Error::SourceIndeterminate,
    }
}

fn source_auth_error(error: TeamError) -> Error {
    cursor_team_error(error)
}

fn hide_unknown_repository(error: TeamError) -> TeamError {
    match error {
        TeamError::Storage(Error::NotFound(_)) => TeamError::Unauthorized,
        error => error,
    }
}

fn storage_io(error: Error) -> io::Error {
    if matches!(error, Error::SourceDenied) {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "encoded source stream validation failed",
        )
    } else {
        io::Error::other("encoded source stream validation failed")
    }
}

// The tests start a PostgreSQL cluster on a Unix socket.
#[cfg(all(test, unix))]
mod tests {
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use postgres::NoTls;
    use tempfile::TempDir;

    use super::*;
    use crate::backend::{LocalBackend, ObjectMetadata, PutOutcome};
    use crate::model::FileChange;
    use crate::store::Engine;
    use crate::team::auth::Credentials;
    use crate::team::publication::{CompleteRequest, SourceNamespace};
    use crate::team::service::{
        BootstrapRequest, DatabaseConfig, ProvisionRequest, RetireAccessRequest, RolesRequest,
        TeamService, operator_bootstrap, operator_retire_access,
    };

    struct Cluster {
        directory: TempDir,
        config: DatabaseConfig,
        binaries: PathBuf,
    }

    impl Cluster {
        fn new() -> Self {
            let binaries = PathBuf::from(
                std::env::var_os("AGENTSCM_PG_BIN_DIR")
                    .expect("set AGENTSCM_PG_BIN_DIR to an explicit PostgreSQL18 directory"),
            );
            assert!(binaries.is_absolute());
            let version = Command::new(binaries.join("postgres"))
                .arg("--version")
                .output()
                .unwrap();
            assert!(version.status.success());
            assert!(
                String::from_utf8_lossy(&version.stdout).starts_with("postgres (PostgreSQL) 18.")
            );
            let directory = tempfile::Builder::new()
                .prefix("agentscm-source-browser-pg-")
                .tempdir_in("/tmp")
                .unwrap();
            let socket = directory.path().join("socket");
            std::fs::create_dir(&socket).unwrap();
            let initialized = Command::new(binaries.join("initdb"))
                .arg("-D")
                .arg(directory.path().join("data"))
                .args([
                    "--no-locale",
                    "--encoding=UTF8",
                    "--auth-local=trust",
                    "--auth-host=reject",
                ])
                .output()
                .unwrap();
            assert!(initialized.status.success());
            let cluster = Self {
                config: DatabaseConfig {
                    socket,
                    port: 5432,
                    database: "postgres".into(),
                },
                directory,
                binaries,
            };
            let options = format!(
                "-k {} -p 5432 -c listen_addresses='' -c max_connections=16",
                cluster.config.socket.display()
            );
            let started = Command::new(cluster.binaries.join("pg_ctl"))
                .arg("-D")
                .arg(cluster.directory.path().join("data"))
                .arg("-l")
                .arg(cluster.directory.path().join("postgres.log"))
                .args(["-w", "-t", "15", "-o", &options, "start"])
                .output()
                .unwrap();
            assert!(started.status.success());
            cluster
        }
    }

    impl Drop for Cluster {
        fn drop(&mut self) {
            let _ = Command::new(self.binaries.join("pg_ctl"))
                .arg("-D")
                .arg(self.directory.path().join("data"))
                .args(["-w", "-t", "15", "-m", "fast", "stop"])
                .output();
        }
    }

    type Hook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

    struct HookBackend {
        inner: LocalBackend,
        hook: Hook,
    }

    impl HookBackend {
        fn new(inner: LocalBackend) -> Self {
            Self {
                inner,
                hook: Arc::new(Mutex::new(None)),
            }
        }

        fn hook(&self, action: impl FnOnce() + Send + 'static) {
            *self.hook.lock().unwrap() = Some(Box::new(action));
        }
    }

    struct HookRead<'a> {
        inner: Box<dyn Read + Send + 'a>,
        hook: Hook,
    }

    impl Read for HookRead<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(bytes)?;
            if read > 0
                && let Some(action) = self.hook.lock().unwrap().take()
            {
                action();
            }
            Ok(read)
        }
    }

    impl ImmutableBackend for HookBackend {
        fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> StorageResult<()> {
            self.inner.prepare_publication(key, incarnation)
        }

        fn put_if_absent(
            &self,
            key: &ObjectKey,
            incarnation: &str,
            bytes: &[u8],
        ) -> StorageResult<PutOutcome> {
            self.inner.put_if_absent(key, incarnation, bytes)
        }

        fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> StorageResult<Vec<u8>> {
            self.inner.get_verified(key, incarnation)
        }

        fn inspect_verified(
            &self,
            key: &ObjectKey,
            incarnation: &str,
        ) -> StorageResult<ObjectMetadata> {
            self.inner.inspect_verified(key, incarnation)
        }

        fn open_encoded(
            &self,
            key: &ObjectKey,
            incarnation: &str,
        ) -> StorageResult<Box<dyn Read + Send + '_>> {
            Ok(Box::new(HookRead {
                inner: self.inner.open_encoded(key, incarnation)?,
                hook: self.hook.clone(),
            }))
        }

        fn get_range_verified(
            &self,
            key: &ObjectKey,
            incarnation: &str,
            offset: u64,
            length: u64,
        ) -> StorageResult<Vec<u8>> {
            self.inner
                .get_range_verified(key, incarnation, offset, length)
        }
    }

    fn browser_fixture(cluster: &Cluster, directory: &TempDir) -> (String, HookBackend, String) {
        browser_fixture_with_content(cluster, directory, b"private bytes".to_vec())
    }

    fn browser_fixture_with_content(
        cluster: &Cluster,
        directory: &TempDir,
        content: Vec<u8>,
    ) -> (String, HookBackend, String) {
        let repository = "browser-native";
        let scope = RepositoryScope {
            trust_domain: "team".into(),
            repository: repository.into(),
        };
        let credential = "a".repeat(64);
        operator_bootstrap(
            &cluster.config,
            &BootstrapRequest {
                scope: scope.clone(),
                authority_generation: 1,
                operation: "bootstrap".into(),
                max_bytes: 64 * 1024 * 1024,
                max_objects: 100_000,
                principal: "admin".into(),
                roles: vec![Role::Reader, Role::Contributor, Role::Administrator],
            },
            &credential,
        )
        .unwrap();
        let source = Engine::open(directory.path().join("source")).unwrap();
        let root = source
            .checkpoint(
                None,
                &[FileChange {
                    path: "private".into(),
                    content: Some(content),
                }],
            )
            .unwrap();
        let backend = LocalBackend::open(directory.path().join("backend")).unwrap();
        let mut controller = cluster.config.open(scope).unwrap();
        controller
            .reserve_profile(
                &ReserveRequest {
                    generation: 1,
                    publication: "publication".into(),
                    root: root.clone(),
                    byte_limit: 32 * 1024 * 1024,
                    object_limit: 100_000,
                    lease_seconds: 300,
                },
                SourceNamespace::V1,
            )
            .unwrap();
        let manifest = controller
            .upload_and_qualify(
                1,
                "publication",
                &source,
                &backend,
                &directory.path().join("offer"),
            )
            .unwrap();
        controller
            .complete(&CompleteRequest {
                generation: 1,
                publication: "publication".into(),
                operation: "complete".into(),
                manifest_digest: manifest,
            })
            .unwrap();
        (credential, HookBackend::new(backend), root)
    }

    #[test]
    #[ignore = "requires installed PG18; creates only an isolated local cluster"]
    fn completion_error_is_hidden_when_reader_role_is_removed_after_native_work() {
        let cluster = Cluster::new();
        let directory = tempfile::tempdir().unwrap();
        let (admin_credential, backend, _) = browser_fixture_with_content(
            &cluster,
            &directory,
            vec![b'x'; MAX_NATIVE_OUTPUT_BYTES + 1],
        );
        let scope = RepositoryScope {
            trust_domain: "team".into(),
            repository: "browser-native".into(),
        };
        let reader_credential = "b".repeat(64);
        let mut admin = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::bearer(&admin_credential).unwrap(),
        )
        .unwrap();
        admin
            .provision(&ProvisionRequest {
                authority_generation: 1,
                operation: "provision-final-fence-reader".into(),
                principal: "final-fence-reader".into(),
                credential: reader_credential.clone(),
                roles: vec![Role::Reader],
            })
            .unwrap();
        let mut reader = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::bearer(&reader_credential).unwrap(),
        )
        .unwrap();
        // Prove this is the post-SourceReader output-limit branch, rather than
        // an error that already passed through the native callback fence.
        assert!(matches!(
            reader.source_file(&backend, 1, "publication", "private"),
            Err(TeamError::Storage(Error::Limit(message))) if message.contains("native file exceeds")
        ));
        let hook_config = cluster.config.clone();
        COMPLETION_TEST_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let mut hook_admin = TeamService::connect(
                    &hook_config,
                    scope,
                    Credentials::bearer(&admin_credential).unwrap(),
                )
                .unwrap();
                hook_admin
                    .roles(
                        "final-fence-reader",
                        &RolesRequest {
                            authority_generation: 1,
                            operation: "strip-reader-at-final-fence".into(),
                            expected_version: 1,
                            roles: vec![Role::Contributor],
                        },
                    )
                    .unwrap();
            }));
        });
        assert!(matches!(
            reader.source_file(&backend, 1, "publication", "private"),
            Err(TeamError::Storage(Error::SourceDenied))
        ));
    }

    #[test]
    #[ignore = "requires installed PG18; creates only isolated local clusters"]
    fn elapsed_deadline_still_observes_committed_logout_and_retirement() {
        for retire in [false, true] {
            let cluster = Cluster::new();
            let directory = tempfile::tempdir().unwrap();
            let (credential, backend, root) = browser_fixture(&cluster, &directory);
            let scope = RepositoryScope {
                trust_domain: "team".into(),
                repository: "browser-native".into(),
            };
            let session =
                TeamService::login(&cluster.config, scope.clone(), &credential, 1, 300).unwrap();
            let credentials =
                Credentials::browser(&session.session_token, Some(&session.csrf_token)).unwrap();
            let mut reader =
                TeamService::connect(&cluster.config, scope.clone(), credentials.clone()).unwrap();
            let deadline = request_deadline();
            let request = load_request(&mut reader.authority, 1, "publication", deadline).unwrap();
            let mut access = NativeAccess::new(&mut reader.authority, &backend, request, deadline);
            let bytes = SourceReader::new(&access)
                .read_file(&root, "private")
                .unwrap();
            assert_eq!(bytes, b"private bytes");
            // Advance only the request clock boundary deterministically after
            // real authorized native I/O; no ten-second timing sleep is needed.
            access.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            assert!(matches!(
                access.complete(Ok(())),
                Err(TeamError::Storage(Error::SourceIndeterminate))
            ));
            if retire {
                operator_retire_access(
                    &cluster.config,
                    &RetireAccessRequest {
                        scope: scope.clone(),
                        authority_generation: 1,
                        operation: "retire-after-deadline".into(),
                    },
                )
                .unwrap();
            } else {
                TeamService::connect(&cluster.config, scope, credentials)
                    .unwrap()
                    .logout()
                    .unwrap();
            }
            assert!(matches!(
                access.complete(Ok(bytes)),
                Err(TeamError::Storage(Error::SourceDenied))
            ));
        }
    }

    #[test]
    #[ignore = "requires installed PG18; creates only an isolated local cluster"]
    fn cancel_rechecks_reader_after_committing_the_bounded_tombstone() {
        let cluster = Cluster::new();
        let directory = tempfile::tempdir().unwrap();
        let (admin_credential, backend, _) = browser_fixture(&cluster, &directory);
        let scope = RepositoryScope {
            trust_domain: "team".into(),
            repository: "browser-native".into(),
        };
        let reader_credential = "c".repeat(64);
        let mut admin = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::bearer(&admin_credential).unwrap(),
        )
        .unwrap();
        admin
            .provision(&ProvisionRequest {
                authority_generation: 1,
                operation: "provision-cancel-fence-reader".into(),
                principal: "cancel-fence-reader".into(),
                credential: reader_credential.clone(),
                roles: vec![Role::Reader],
            })
            .unwrap();
        let mut reader = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::bearer(&reader_credential).unwrap(),
        )
        .unwrap();
        reader
            .source_history(
                &backend,
                1,
                "publication",
                &SourcePageRequest {
                    operation: "cancel-final-fence".into(),
                    cursor: None,
                    limit: 1,
                },
            )
            .unwrap();
        let hook_config = cluster.config.clone();
        COMPLETION_TEST_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let mut hook_admin = TeamService::connect(
                    &hook_config,
                    scope,
                    Credentials::bearer(&admin_credential).unwrap(),
                )
                .unwrap();
                hook_admin
                    .roles(
                        "cancel-fence-reader",
                        &RolesRequest {
                            authority_generation: 1,
                            operation: "strip-reader-after-cancel".into(),
                            expected_version: 1,
                            roles: vec![Role::Contributor],
                        },
                    )
                    .unwrap();
            }));
        });
        assert!(matches!(
            reader.source_cancel(&SourceCancelRequest {
                operation: "cancel-final-fence".into(),
            }),
            Err(TeamError::Storage(Error::SourceDenied))
        ));
        let mut database = postgres::Config::new()
            .host_path(&cluster.config.socket)
            .port(cluster.config.port)
            .dbname(&cluster.config.database)
            .connect(NoTls)
            .unwrap();
        let state: String = database
            .query_one(
                "SELECT state FROM team_source_cursors WHERE repository='browser-native' AND principal='cancel-fence-reader' AND operation='cancel-final-fence'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(state, "cancelled");
    }

    #[test]
    #[ignore = "requires installed PG18; deterministic candidates exercise collision bound only"]
    fn cursor_generation_stops_after_three_repository_collisions_without_row_change() {
        let cluster = Cluster::new();
        let directory = tempfile::tempdir().unwrap();
        let _ = browser_fixture(&cluster, &directory);
        let candidates = vec!["1".repeat(64), "2".repeat(64), "3".repeat(64)];
        let mut database = postgres::Config::new()
            .host_path(&cluster.config.socket)
            .port(cluster.config.port)
            .dbname(&cluster.config.database)
            .connect(NoTls)
            .unwrap();
        for (index, candidate) in candidates.iter().enumerate() {
            let token_digest = cursor_token_digest(candidate).unwrap();
            database
                .execute(
                    "INSERT INTO team_source_cursors(trust_domain,repository,principal,principal_version,authority_generation,operation,binding_digest,binding,state,version,current_token_digest,current_continuation,initial_reply,initial_reply_digest,reserved_bytes,expires_at) VALUES('team','browser-native','admin',1,1,$1,$2,'{}','active',1,$3,'synthetic','{}',$4,4096,clock_timestamp()+interval '1 hour')",
                    &[
                        &format!("collision-{index}"),
                        &"a".repeat(64),
                        &token_digest,
                        &"b".repeat(64),
                    ],
                )
                .unwrap();
        }
        CURSOR_CANDIDATE_TEST_HOOK.with(|hook| *hook.borrow_mut() = candidates);
        let scope = RepositoryScope {
            trust_domain: "team".into(),
            repository: "browser-native".into(),
        };
        let mut transaction = database.transaction().unwrap();
        assert!(matches!(
            generate_unique_cursor(&mut transaction, &scope),
            Err(TeamError::Unavailable)
        ));
        transaction.rollback().unwrap();
        let count: i64 = database
            .query_one(
                "SELECT count(*) FROM team_source_cursors WHERE repository='browser-native'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(count, 3);
    }

    #[test]
    #[ignore = "requires installed PG18; creates only an isolated local cluster"]
    fn browser_logout_and_expiry_during_raw_read_deny_the_complete_result() {
        let cluster = Cluster::new();
        let directory = tempfile::tempdir().unwrap();
        let (credential, backend, _) = browser_fixture(&cluster, &directory);
        let scope = RepositoryScope {
            trust_domain: "team".into(),
            repository: "browser-native".into(),
        };

        let csrf_session =
            TeamService::login(&cluster.config, scope.clone(), &credential, 1, 300).unwrap();
        let mut valid_browser = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::browser(&csrf_session.session_token, Some(&csrf_session.csrf_token))
                .unwrap(),
        )
        .unwrap();
        valid_browser
            .source_history(
                &backend,
                1,
                "publication",
                &SourcePageRequest {
                    operation: "browser-cancel-csrf".into(),
                    cursor: None,
                    limit: 1,
                },
            )
            .unwrap();
        let mut wrong_csrf = TeamService::connect(
            &cluster.config,
            scope.clone(),
            Credentials::browser(&csrf_session.session_token, Some(&"d".repeat(64))).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            wrong_csrf.source_cancel(&SourceCancelRequest {
                operation: "browser-cancel-csrf".into(),
            }),
            Err(TeamError::Unauthorized)
        ));
        valid_browser
            .source_cancel(&SourceCancelRequest {
                operation: "browser-cancel-csrf".into(),
            })
            .unwrap();

        let logout_session =
            TeamService::login(&cluster.config, scope.clone(), &credential, 1, 300).unwrap();
        let logout_credentials = Credentials::browser(
            &logout_session.session_token,
            Some(&logout_session.csrf_token),
        )
        .unwrap();
        let mut logout_reader =
            TeamService::connect(&cluster.config, scope.clone(), logout_credentials.clone())
                .unwrap();
        let logout_config = cluster.config.clone();
        let logout_scope = scope.clone();
        backend.hook(move || {
            let mut logout =
                TeamService::connect(&logout_config, logout_scope, logout_credentials).unwrap();
            logout.logout().unwrap();
        });
        assert!(matches!(
            logout_reader.source_file(&backend, 1, "publication", "private"),
            Err(TeamError::Storage(Error::SourceDenied))
        ));

        let expiry_session =
            TeamService::login(&cluster.config, scope.clone(), &credential, 1, 300).unwrap();
        let mut expiry_reader = TeamService::connect(
            &cluster.config,
            scope,
            Credentials::browser(&expiry_session.session_token, None).unwrap(),
        )
        .unwrap();
        let expiry_config = cluster.config.clone();
        backend.hook(move || {
            let mut database = postgres::Config::new()
                .host_path(&expiry_config.socket)
                .port(expiry_config.port)
                .dbname(&expiry_config.database)
                .connect(NoTls)
                .unwrap();
            database
                .execute(
                    "UPDATE team_sessions SET expires_at=clock_timestamp()-interval '1 second' WHERE repository='browser-native' AND revoked=FALSE",
                    &[],
                )
                .unwrap();
        });
        assert!(matches!(
            expiry_reader.source_file(&backend, 1, "publication", "private"),
            Err(TeamError::Storage(Error::SourceDenied))
        ));
    }
}
