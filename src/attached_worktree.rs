//! Git-authoritative, read-only attachment. Outcomes describe historical observations,
//! never a lock on arbitrary editors. Publication requires a fresh source comparison.
use crate::authority::Authority;
use crate::context::{ChangeId, ChangeState};
use crate::execution::Cancellation;
use crate::execution::process::capture_with_input;
use crate::git_snapshot::{self, SnapshotRequest};
use crate::local_recovery::AccessGuard;
use crate::store::{Engine, create_directory_durable, digest, validate_id};
use crate::workspace::{
    AttachedScan, attached_candidate_is_target_derived, capture_selected_worktree, merge_attached,
    validate_attachment_paths,
};
use crate::{Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
const APPLICATION_ID: i64 = 0x4147_4154;
const SCHEMA_VERSION: i64 = 2;
const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PATHS: usize = 256;
const MAX_JSON: usize = 262144;
const MAX_TOTAL_ROWS: i64 = 10000;
const MAX_TOTAL_JSON_BYTES: i64 = 256 * 1024 * 1024;
const MAX_ATTACHMENTS: i64 = 256;
const MAX_OPERATIONS_PER_ATTACHMENT: i64 = 4096;
const MAX_MAPPINGS_PER_ATTACHMENT: i64 = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Committed,
    Index,
    /// Raw filesystem bytes; no text/eol/ident/encoding conversion is applied.
    #[serde(rename = "worktree_raw")]
    Worktree,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AttachRequest {
    pub git: PathBuf,
    pub worktree: PathBuf,
    pub actor: String,
    pub remote: String,
    pub target: String,
    pub output: String,
    pub source: SourceMode,
    pub paths: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttachmentState {
    pub id: String,
    pub generation: u64,
    pub repository: String,
    pub git: String,
    pub worktree: String,
    pub git_common_dir: String,
    pub git_dir: String,
    pub index: String,
    pub actor: String,
    /// An actual Authority Change, set only by explicit verified binding.
    pub change_id: Option<ChangeId>,
    /// Invalidated on every source/target advancement. Phase 2 must also recheck Authority.
    pub context: Option<ChangeState>,
    pub branch: String,
    pub remote: String,
    pub remote_url: String,
    pub target: String,
    pub target_commit: String,
    pub output: String,
    pub head_commit: String,
    pub baseline_revision: String,
    pub candidate_revision: String,
    /// Last local source observation, kept separately from refreshed candidate content.
    pub source_revision: String,
    pub source_fingerprint: String,
    pub source: SourceMode,
    pub paths: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureOutcome {
    pub attachment: AttachmentState,
    pub revision: String,
    pub changed_paths: Vec<String>,
    pub excluded_total: usize,
    pub no_op: bool,
    /// Always true: filesystem observations cannot fence arbitrary editors.
    pub historical: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitMapping {
    pub commit: String,
    pub tree: String,
    pub revision: String,
}
#[derive(Clone, Debug)]
struct Observation {
    root: PathBuf,
    common: PathBuf,
    git_dir: PathBuf,
    index: PathBuf,
    branch: String,
    head: String,
    remote_url: String,
    target: String,
    index_sha256: String,
    index_len: u64,
}
pub struct AttachmentJournal {
    db: PathBuf,
    repository: String,
    _access: AccessGuard,
    #[cfg(test)]
    before_validation: Option<Box<dyn Fn()>>,
}
impl AttachmentJournal {
    pub fn open(data_dir: &Path, repository: &str) -> Result<Self> {
        Self::open_inner(data_dir, repository, true)
    }

    pub fn open_existing(data_dir: &Path, repository: &str) -> Result<Self> {
        Self::open_inner(data_dir, repository, false)
    }

    fn open_inner(data_dir: &Path, repository: &str, create: bool) -> Result<Self> {
        ChangeId {
            repository: repository.into(),
            id: "attachment-journal".into(),
        }
        .validate()?;
        let (access, directory) = if create {
            let access = AccessGuard::shared(data_dir)?;
            let root = create_directory_durable(data_dir)?;
            let directory = create_directory_durable(root.join("attachments"))?;
            (access, directory)
        } else {
            let root = data_dir.canonicalize()?;
            let directory = root.join("attachments");
            (AccessGuard::shared_existing(&root)?, directory)
        };
        if !fs::symlink_metadata(&directory)?.is_dir() {
            return Err(Error::Invalid(
                "attachment directory must not be a symlink".into(),
            ));
        }
        if create {
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let this = Self {
            db: directory.join("state.sqlite"),
            repository: repository.into(),
            _access: access,
            #[cfg(test)]
            before_validation: None,
        };
        if !create {
            let mut db = this.raw_connect()?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Deferred)?;
            this.verify_db(&tx)?;
            tx.commit()?;
            return Ok(this);
        }
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .mode(0o600)
            .open(&this.db)
        {
            Ok(file) => {
                file.sync_all()?;
            }
            Err(e) => return Err(e.into()),
        }
        let mut db = this.raw_connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let objects: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if app == 0 && version == 0 && objects == 0 {
            tx.execute_batch("CREATE TABLE identity(singleton INTEGER PRIMARY KEY CHECK(singleton=1),repository TEXT NOT NULL) STRICT;
            CREATE TABLE attachments(id TEXT PRIMARY KEY CHECK(length(id)=64),generation INTEGER NOT NULL CHECK(generation>=0),state_json BLOB NOT NULL CHECK(length(state_json)<=262144)) STRICT;
            CREATE TABLE bindings(change_id TEXT PRIMARY KEY CHECK(length(change_id)<=128),attachment TEXT NOT NULL UNIQUE REFERENCES attachments(id)) STRICT;
            CREATE TABLE mappings(id TEXT NOT NULL REFERENCES attachments(id),git_commit TEXT NOT NULL CHECK(length(git_commit) IN (40,64)),native_revision TEXT NOT NULL CHECK(length(native_revision)=64),git_tree TEXT NOT NULL CHECK(length(git_tree) IN (40,64)),PRIMARY KEY(id,git_commit)) STRICT;
            CREATE TABLE operations(operation TEXT PRIMARY KEY CHECK(length(operation)<=128),kind TEXT NOT NULL CHECK(kind IN ('attach','capture','refresh','bind_context')),attachment TEXT NOT NULL REFERENCES attachments(id),expected_generation INTEGER CHECK(expected_generation>=0),request_digest TEXT NOT NULL CHECK(length(request_digest)=64),outcome_json BLOB NOT NULL CHECK(length(outcome_json)<=262144)) STRICT;
            CREATE INDEX operations_attachment ON operations(attachment);")?;
            tx.execute("INSERT INTO identity VALUES(1,?1)", [&this.repository])?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if app != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(Error::Invalid(
                "unsupported attachment journal identity/version; no automatic migration".into(),
            ));
        }
        tx.commit()?;
        fs::File::open(&directory)?.sync_all()?;
        this.verify_db(&db)?;
        Ok(this)
    }
    fn raw_connect(&self) -> Result<Connection> {
        if !fs::symlink_metadata(
            self.db
                .parent()
                .ok_or_else(|| Error::Invalid("missing journal directory".into()))?,
        )?
        .is_dir()
        {
            return Err(Error::Invalid(
                "attachment directory must not be a symlink".into(),
            ));
        }
        let meta = fs::symlink_metadata(&self.db)?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid(
                "attachment journal must be a private regular file (0600)".into(),
            ));
        }
        let db = Connection::open_with_flags(
            &self.db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.pragma_update(None, "synchronous", "FULL")?;
        Ok(db)
    }
    fn verify_db(&self, db: &Connection) -> Result<()> {
        let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if app != APPLICATION_ID || version != SCHEMA_VERSION {
            return Err(Error::Invalid(
                "unsupported attachment journal identity/version; no automatic migration".into(),
            ));
        }
        verify_schema_shape(db)?;
        let repository: String = db.query_row(
            "SELECT repository FROM identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if repository != self.repository {
            return Err(Error::Conflict(
                "attachment journal belongs to another repository".into(),
            ));
        }
        let mut total = 0i64;
        for table in ["attachments", "bindings", "mappings", "operations"] {
            let count: i64 =
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
            total = total
                .checked_add(count)
                .ok_or_else(|| Error::Limit("attachment journal capacity exceeded".into()))?;
            if total > MAX_TOTAL_ROWS {
                return Err(Error::Limit("attachment journal capacity exceeded".into()));
            }
        }
        let bytes: i64 = db.query_row(
            "SELECT coalesce((SELECT sum(length(state_json)) FROM attachments),0)+coalesce((SELECT sum(length(outcome_json)) FROM operations),0)",
            [],
            |r| r.get(0),
        )?;
        if !(0..=MAX_TOTAL_JSON_BYTES).contains(&bytes) {
            return Err(Error::Limit(
                "attachment journal byte capacity exceeded".into(),
            ));
        }
        Ok(())
    }
    fn connect(&self) -> Result<Connection> {
        let db = self.raw_connect()?;
        self.verify_db(&db)?;
        Ok(db)
    }
    pub fn state(&self, id: &str) -> Result<AttachmentState> {
        read_state(&self.connect()?, id, &self.repository)
    }
    pub fn mappings(&self, id: &str, after: Option<&str>, limit: usize) -> Result<Vec<GitMapping>> {
        self.state(id)?;
        if limit == 0 || limit > 256 {
            return Err(Error::Limit("mapping page limit must be 1..256".into()));
        }
        let db = self.connect()?;
        let mut stmt = db.prepare("SELECT git_commit,git_tree,native_revision FROM mappings WHERE id=?1 AND git_commit>?2 ORDER BY git_commit LIMIT ?3")?;
        let rows = stmt.query_map(params![id, after.unwrap_or(""), limit], |r| {
            Ok(GitMapping {
                commit: r.get(0)?,
                tree: r.get(1)?,
                revision: r.get(2)?,
            })
        })?;
        let values = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        for m in &values {
            validate_oid(&m.commit)?;
            validate_oid(&m.tree)?;
            validate_id(&m.revision)?;
        }
        Ok(values)
    }
    pub fn attach(
        &self,
        engine: &Engine,
        operation: &str,
        request: &AttachRequest,
    ) -> Result<AttachmentState> {
        validate_operation(operation)?;
        validate_request(request)?;
        let request_digest = digest(&serde_json::to_vec(&("attach", request))?);
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(out) = retry_attach(&tx, operation, &request_digest, request)? {
            return Ok(out);
        }
        self.separation(engine, request)?;
        let before = observe(request)?;
        self.separation_observed(&before)?;
        require_forward(
            request,
            &before.target,
            &before.head,
            "initial HEAD relative to target",
        )?;
        let id = attachment_id(&before.git_dir, &before.index, &request.actor);
        if tx
            .query_row("SELECT 1 FROM attachments WHERE id=?1", [&id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Err(Error::Conflict("actor already attached; use capture or refresh; superseding an attachment is not supported".into()));
        }
        capacity(&tx, "attachments", None)?;
        let (baseline, target_tree) =
            capture_commit(engine, &self.repository, request, &before.target)?;
        let (head, head_tree) = capture_commit(engine, &self.repository, request, &before.head)?;
        let mut state = AttachmentState {
            id,
            generation: 0,
            repository: self.repository.clone(),
            git: path_string(&request.git),
            worktree: path_string(&before.root),
            git_common_dir: path_string(&before.common),
            git_dir: path_string(&before.git_dir),
            index: path_string(&before.index),
            actor: request.actor.clone(),
            change_id: None,
            context: None,
            branch: before.branch.clone(),
            remote: request.remote.clone(),
            remote_url: before.remote_url.clone(),
            target: request.target.clone(),
            target_commit: before.target.clone(),
            output: request.output.clone(),
            head_commit: before.head.clone(),
            baseline_revision: baseline.clone(),
            candidate_revision: head.clone(),
            source_revision: head.clone(),
            source_fingerprint: String::new(),
            source: request.source,
            paths: request.paths.clone(),
        };
        let local = self.local_source(engine, &state, &before, &head)?;
        state.source_revision = local.revision.clone();
        state.candidate_revision = local.revision;
        state.source_fingerprint = local.fingerprint;
        self.validate_observed(engine, &state, &before, &head)?;
        tx.execute(
            "INSERT INTO attachments VALUES(?1,0,?2)",
            params![state.id, encode(&state)?],
        )?;
        mapping(&tx, &state.id, &before.target, &baseline, &target_tree)?;
        mapping(&tx, &state.id, &before.head, &head, &head_tree)?;
        retain(
            &tx,
            operation,
            "attach",
            &state.id,
            None,
            &request_digest,
            &state,
        )?;
        tx.commit()?;
        Ok(state)
    }
    fn separation(&self, engine: &Engine, r: &AttachRequest) -> Result<()> {
        let source = r.worktree.canonicalize()?;
        let data = self
            .db
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Invalid("missing data root".into()))?
            .canonicalize()?;
        if engine.data_root().canonicalize()? != data {
            return Err(Error::Invalid(
                "attachment engine belongs to another data directory".into(),
            ));
        }
        if source.starts_with(&data) || data.starts_with(&source) {
            return Err(Error::Invalid(
                "source and native data directories must be separate".into(),
            ));
        }
        Ok(())
    }
    fn separation_observed(&self, o: &Observation) -> Result<()> {
        let data = self
            .db
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Invalid("missing data root".into()))?
            .canonicalize()?;
        for path in [&o.common, &o.git_dir, &o.index] {
            let path = path.canonicalize()?;
            if path.starts_with(&data) || data.starts_with(&path) {
                return Err(Error::Invalid(
                    "Git metadata and native data directories must be separate".into(),
                ));
            }
        }
        Ok(())
    }
    fn local_source(
        &self,
        engine: &Engine,
        state: &AttachmentState,
        observation: &Observation,
        head: &str,
    ) -> Result<crate::workspace::SelectedCapture> {
        if state.source == SourceMode::Committed {
            return Ok(crate::workspace::SelectedCapture {
                revision: head.into(),
                changed_paths: Vec::new(),
                excluded_total: 0,
                fingerprint: digest(head.as_bytes()),
            });
        }
        let r = state_request(state);
        let inventory = git(&r, &["ls-files", "--cached", "-z"])?;
        let tracked: Vec<String> = inventory
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if tracked.len() > 100000 {
            return Err(Error::Limit("Git inventory too large".into()));
        }
        capture_selected_worktree(
            engine,
            AttachedScan {
                root: &observation.root,
                probe: self.db.parent().unwrap(),
                git: &r.git,
                tracked: &tracked,
            },
            head,
            &state.paths,
        )
    }
    fn validate_observed(
        &self,
        engine: &Engine,
        state: &AttachmentState,
        before: &Observation,
        head: &str,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = &self.before_validation {
            hook();
        }
        let observed = self.local_source(engine, state, before, head)?;
        if observed.fingerprint != state.source_fingerprint {
            return Err(Error::Conflict(
                "selected bytes, absence or exclusion inputs changed during observation".into(),
            ));
        }
        ensure_unchanged(&state_request(state), before)
    }
    /// A fresh comparison, still historical when returned. Not a publication lease.
    pub fn validate_current(&self, engine: &Engine, id: &str, generation: u64) -> Result<String> {
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_state(&tx, id, &self.repository)?;
        fence(&state, generation)?;
        let r = state_request(&state);
        self.separation(engine, &r)?;
        let before = observe(&r)?;
        self.separation_observed(&before)?;
        validate_identity(&state, &before)?;
        if before.head != state.head_commit || before.target != state.target_commit {
            return Err(Error::Conflict(
                "Git source or target advanced; capture/refresh and requalify".into(),
            ));
        }
        let (head, _) = capture_commit(engine, &self.repository, &r, &before.head)?;
        self.validate_observed(engine, &state, &before, &head)?;
        Ok(digest(&serde_json::to_vec(&(
            state.generation,
            &state.candidate_revision,
            &state.target_commit,
            &state.source_fingerprint,
            observation_digest(&before),
        ))?))
    }
    pub fn capture(
        &self,
        engine: &Engine,
        operation: &str,
        id: &str,
        expected_generation: u64,
    ) -> Result<CaptureOutcome> {
        self.advance(engine, operation, id, expected_generation, false)
    }
    pub fn refresh(
        &self,
        engine: &Engine,
        operation: &str,
        id: &str,
        expected_generation: u64,
    ) -> Result<AttachmentState> {
        Ok(self
            .advance(engine, operation, id, expected_generation, true)?
            .attachment)
    }
    fn advance(
        &self,
        engine: &Engine,
        operation: &str,
        id: &str,
        expected_generation: u64,
        refresh: bool,
    ) -> Result<CaptureOutcome> {
        validate_operation(operation)?;
        validate_id(id)?;
        let request_digest = digest(&serde_json::to_vec(&(
            "advance",
            id,
            expected_generation,
            refresh,
        ))?);
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let kind = if refresh { "refresh" } else { "capture" };
        if let Some(out) = retry_advance(
            &tx,
            operation,
            &request_digest,
            kind,
            id,
            expected_generation,
        )? {
            return Ok(out);
        }
        let state = read_state(&tx, id, &self.repository)?;
        fence(&state, expected_generation)?;
        let r = state_request(&state);
        self.separation(engine, &r)?;
        let before = observe(&r)?;
        self.separation_observed(&before)?;
        validate_identity(&state, &before)?;
        require_forward(&r, &state.head_commit, &before.head, "attached HEAD")?;
        require_forward(&r, &state.target_commit, &before.target, "target")?;
        if !refresh && before.target != state.target_commit {
            return Err(Error::Conflict(
                "target advanced; refresh before capture".into(),
            ));
        }
        let (head, head_tree) = capture_commit(engine, &self.repository, &r, &before.head)?;
        let local = self.local_source(engine, &state, &before, &head)?;
        let mut next = state.clone();
        let target_integrated = (before.target != state.target_commit
            || before.head != state.head_commit)
            && is_ancestor(&r, &before.target, &before.head)?;
        next.candidate_revision = if target_integrated {
            ensure_candidate_target_derived(engine, &state)?;
            local.revision.clone()
        } else {
            merge_attached(
                engine,
                &state.source_revision,
                &state.candidate_revision,
                &local.revision,
            )?
        };
        next.source_revision = local.revision.clone();
        next.source_fingerprint = local.fingerprint;
        next.head_commit = before.head.clone();
        let incoming = if before.target != state.target_commit {
            let (incoming, tree) = capture_commit(engine, &self.repository, &r, &before.target)?;
            if !target_integrated {
                next.candidate_revision = merge_attached(
                    engine,
                    &state.baseline_revision,
                    &next.candidate_revision,
                    &incoming,
                )?;
            }
            next.baseline_revision = incoming.clone();
            next.target_commit = before.target.clone();
            Some((incoming, tree))
        } else {
            None
        };
        let no_op = next == state;
        if !no_op {
            next.generation = next
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Limit("generation exhausted".into()))?;
            next.context = None;
        }
        self.validate_observed(engine, &next, &before, &head)?;
        mapping(&tx, id, &before.head, &head, &head_tree)?;
        if let Some((revision, tree)) = incoming {
            mapping(&tx, id, &before.target, &revision, &tree)?;
        }
        write_state(&tx, &state, &next)?;
        let outcome = CaptureOutcome {
            revision: next.candidate_revision.clone(),
            attachment: next,
            changed_paths: local.changed_paths,
            excluded_total: local.excluded_total,
            no_op,
            historical: true,
        };
        retain(
            &tx,
            operation,
            kind,
            id,
            Some(expected_generation),
            &request_digest,
            &outcome,
        )?;
        tx.commit()?;
        Ok(outcome)
    }
    /// Bind an existing, actual Authority Change/context. This grants no landing authority.
    /// Phase 2 must recheck the Authority pointer and validate_current before publication.
    pub fn bind_context(
        &self,
        authority: &Authority,
        operation: &str,
        id: &str,
        generation: u64,
        change: &ChangeId,
    ) -> Result<AttachmentState> {
        validate_operation(operation)?;
        let request = digest(&serde_json::to_vec(&(
            "bind-context",
            id,
            generation,
            change,
        ))?);
        let mut db = self.connect()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if authority.data_root().canonicalize()? != self.data_root()? {
            return Err(Error::Invalid(
                "Authority and attachment journal belong to different data directories".into(),
            ));
        }
        if change.repository != self.repository || authority.repository_id()? != self.repository {
            return Err(Error::Invalid("foreign Authority repository".into()));
        }
        if let Some(out) = retry_bind(&tx, operation, &request, id, generation, change)? {
            validate_retained_context(authority, &out)?;
            return Ok(out);
        }
        let current = authority.change(change)?;
        let context = authority.review_context(&current.context_digest)?;
        let old = read_state(&tx, id, &self.repository)?;
        fence(&old, generation)?;
        if change.repository != self.repository
            || context.revision != old.candidate_revision
            || context.target != old.target
            || context.base.is_some()
            || authority.target(&context.target)?.is_some()
            || old.change_id.as_ref().is_some_and(|c| c != change)
        {
            return Err(Error::Conflict("Authority context does not match this attachment's exact candidate/target/base or stable Change".into()));
        }
        let bound: Option<String> = tx
            .query_row(
                "SELECT attachment FROM bindings WHERE change_id=?1",
                [&change.id],
                |r| r.get(0),
            )
            .optional()?;
        match bound {
            Some(existing) if existing != id => {
                return Err(Error::Conflict(
                    "Authority Change is bound to another attachment".into(),
                ));
            }
            Some(_) => {}
            None => {
                capacity(&tx, "bindings", Some(id))?;
                tx.execute("INSERT INTO bindings VALUES(?1,?2)", params![change.id, id])?;
            }
        }
        let mut next = old.clone();
        next.change_id = Some(change.clone());
        next.context = Some(current);
        if next != old {
            next.generation = next
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Limit("generation exhausted".into()))?;
        }
        write_state(&tx, &old, &next)?;
        retain(
            &tx,
            operation,
            "bind_context",
            id,
            Some(generation),
            &request,
            &next,
        )?;
        tx.commit()?;
        Ok(next)
    }
    fn data_root(&self) -> Result<PathBuf> {
        self.db
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Invalid("missing data root".into()))?
            .canonicalize()
            .map_err(Error::from)
    }
}

pub(crate) fn journal_has_retained_state(data_dir: &Path) -> Result<bool> {
    let directory = data_dir.join("attachments");
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "attachment directory must be an existing real directory".into(),
        ));
    }
    let path = directory.join("state.sqlite");
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "attachment journal must be a private regular file (0600)".into(),
        ));
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let app: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(Error::Invalid(
            "unsupported attachment journal identity/version; no automatic migration".into(),
        ));
    }
    verify_schema_shape(&db)?;
    let identity: Option<String> = db
        .query_row(
            "SELECT repository FROM identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(repository) = identity else {
        return Err(Error::Invalid("invalid attachment journal identity".into()));
    };
    ChangeId {
        repository,
        id: "attachment-journal".into(),
    }
    .validate()?;
    let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Error::Invalid(
            "attachment journal integrity check failed".into(),
        ));
    }
    let foreign: i64 = db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
        r.get(0)
    })?;
    if foreign != 0 {
        return Err(Error::Invalid(
            "attachment journal foreign-key check failed".into(),
        ));
    }
    let total: i64 = db.query_row(
        "SELECT (SELECT count(*) FROM attachments)+(SELECT count(*) FROM bindings)+(SELECT count(*) FROM mappings)+(SELECT count(*) FROM operations)",
        [],
        |r| r.get(0),
    )?;
    if !(0..=MAX_TOTAL_ROWS).contains(&total) {
        return Err(Error::Limit("attachment journal capacity exceeded".into()));
    }
    Ok(total != 0)
}

fn verify_schema_shape(db: &Connection) -> Result<()> {
    let objects: Vec<(String, String)> = {
        let mut statement = db.prepare(
            "SELECT type,name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    let expected = vec![
        ("index".into(), "operations_attachment".into()),
        ("table".into(), "attachments".into()),
        ("table".into(), "bindings".into()),
        ("table".into(), "identity".into()),
        ("table".into(), "mappings".into()),
        ("table".into(), "operations".into()),
    ];
    if objects != expected {
        return Err(Error::Invalid(
            "attachment journal schema objects are missing or unknown".into(),
        ));
    }
    for (table, expected) in [
        ("identity", &["singleton", "repository"][..]),
        ("attachments", &["id", "generation", "state_json"]),
        ("bindings", &["change_id", "attachment"]),
        (
            "mappings",
            &["id", "git_commit", "native_revision", "git_tree"],
        ),
        (
            "operations",
            &[
                "operation",
                "kind",
                "attachment",
                "expected_generation",
                "request_digest",
                "outcome_json",
            ],
        ),
    ] {
        let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(1))?
            .collect::<std::result::Result<_, _>>()?;
        if columns.iter().map(String::as_str).collect::<Vec<_>>() != expected {
            return Err(Error::Invalid(
                "attachment journal schema columns are inconsistent".into(),
            ));
        }
    }
    Ok(())
}
fn capacity(db: &Connection, table: &str, attachment: Option<&str>) -> Result<()> {
    let total: i64 = db.query_row(
        "SELECT (SELECT count(*) FROM attachments)+(SELECT count(*) FROM bindings)+(SELECT count(*) FROM mappings)+(SELECT count(*) FROM operations)",
        [],
        |r| r.get(0),
    )?;
    if total >= MAX_TOTAL_ROWS {
        return Err(Error::Limit(format!(
            "attachment {table} capacity exhausted; no automatic pruning"
        )));
    }
    let (count, limit) = match table {
        "attachments" => (
            db.query_row("SELECT count(*) FROM attachments", [], |r| r.get(0))?,
            MAX_ATTACHMENTS,
        ),
        "operations" => (
            db.query_row(
                "SELECT count(*) FROM operations WHERE attachment=?1",
                [attachment
                    .ok_or_else(|| Error::Invalid("missing attachment quota key".into()))?],
                |r| r.get(0),
            )?,
            MAX_OPERATIONS_PER_ATTACHMENT,
        ),
        "mappings" => (
            db.query_row(
                "SELECT count(*) FROM mappings WHERE id=?1",
                [attachment
                    .ok_or_else(|| Error::Invalid("missing attachment quota key".into()))?],
                |r| r.get(0),
            )?,
            MAX_MAPPINGS_PER_ATTACHMENT,
        ),
        "bindings" => (0, 1),
        _ => return Err(Error::Invalid("unknown attachment capacity table".into())),
    };
    if count >= limit {
        return Err(Error::Limit(format!(
            "attachment {table} per-attachment capacity exhausted; no automatic pruning"
        )));
    }
    Ok(())
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let b = serde_json::to_vec(value)?;
    if b.len() > MAX_JSON {
        return Err(Error::Limit("attachment JSON exceeds limit".into()));
    }
    Ok(b)
}
fn retained_bytes(
    db: &Connection,
    op: &str,
    request: &str,
    kind: &str,
    attachment: Option<&str>,
    expected_generation: Option<u64>,
) -> Result<Option<(String, Vec<u8>)>> {
    let row: Option<(String, String, String, Option<u64>, i64)> = db
        .query_row(
            "SELECT request_digest,kind,attachment,expected_generation,length(outcome_json) FROM operations WHERE operation=?1",
            [op],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((stored, stored_kind, stored_attachment, stored_generation, len)) = row else {
        return Ok(None);
    };
    if stored != request
        || stored_kind != kind
        || attachment.is_some_and(|id| id != stored_attachment)
        || stored_generation != expected_generation
    {
        return Err(Error::Conflict(
            "operation ID reused with different caller inputs".into(),
        ));
    }
    if len < 0 || len as usize > MAX_JSON {
        return Err(Error::Limit("retained outcome exceeds limit".into()));
    }
    let bytes: Vec<u8> = db.query_row(
        "SELECT substr(outcome_json,1,262145) FROM operations WHERE operation=?1",
        [op],
        |r| r.get(0),
    )?;
    if bytes.len() > MAX_JSON {
        return Err(Error::Limit("retained outcome exceeds limit".into()));
    }
    if attachment.is_none() && stored_attachment.len() != 64 {
        return Err(Error::Invalid(
            "retained operation attachment is invalid".into(),
        ));
    }
    Ok(Some((stored_attachment, bytes)))
}
fn retry_attach(
    db: &Connection,
    op: &str,
    request_digest: &str,
    request: &AttachRequest,
) -> Result<Option<AttachmentState>> {
    let Some((attachment, bytes)) = retained_bytes(db, op, request_digest, "attach", None, None)?
    else {
        return Ok(None);
    };
    let state: AttachmentState = serde_json::from_slice(&bytes)?;
    if state.id != attachment
        || state.worktree != path_string(&request.worktree.canonicalize()?)
        || state.generation != 0
        || state.git != path_string(&request.git)
        || state.actor != request.actor
        || state.remote != request.remote
        || state.target != request.target
        || state.output != request.output
        || state.source != request.source
        || state.paths != request.paths
    {
        return Err(Error::Invalid(
            "retained attach outcome is inconsistent".into(),
        ));
    }
    validate_historical_state(db, &state)?;
    Ok(Some(state))
}
fn retry_advance(
    db: &Connection,
    op: &str,
    request: &str,
    kind: &str,
    id: &str,
    generation: u64,
) -> Result<Option<CaptureOutcome>> {
    let Some((_, bytes)) = retained_bytes(db, op, request, kind, Some(id), Some(generation))?
    else {
        return Ok(None);
    };
    let out: CaptureOutcome = serde_json::from_slice(&bytes)?;
    validate_capture_outcome(db, &out, id, generation)?;
    Ok(Some(out))
}
fn retry_bind(
    db: &Connection,
    op: &str,
    request: &str,
    id: &str,
    generation: u64,
    change: &ChangeId,
) -> Result<Option<AttachmentState>> {
    let Some((_, bytes)) =
        retained_bytes(db, op, request, "bind_context", Some(id), Some(generation))?
    else {
        return Ok(None);
    };
    let state: AttachmentState = serde_json::from_slice(&bytes)?;
    if state.id != id
        || state.change_id.as_ref() != Some(change)
        || state.context.as_ref().map(|c| &c.id) != Some(change)
        || !matches!(state.generation, g if g == generation || g == generation.saturating_add(1))
    {
        return Err(Error::Invalid(
            "retained context binding is inconsistent".into(),
        ));
    }
    validate_historical_state(db, &state)?;
    Ok(Some(state))
}
fn validate_retained_context(authority: &Authority, state: &AttachmentState) -> Result<()> {
    let retained = state
        .context
        .as_ref()
        .ok_or_else(|| Error::Invalid("retained context binding is missing".into()))?;
    let context = authority.review_context(&retained.context_digest)?;
    if context.change != retained.id
        || context.change_version != retained.version
        || context.digest()? != retained.context_digest
        || context.revision != state.candidate_revision
        || context.target != state.target
        || context.base.is_some()
    {
        return Err(Error::Invalid(
            "retained Authority context is inconsistent".into(),
        ));
    }
    // The original bind checked that no native target existed. A target or current
    // Change pointer may have advanced since then; replay is only that historical
    // result and neither rechecks nor grants current qualification or authority.
    Ok(())
}
fn retain<T: Serialize>(
    db: &Connection,
    op: &str,
    kind: &str,
    attachment: &str,
    expected_generation: Option<u64>,
    request: &str,
    out: &T,
) -> Result<()> {
    capacity(db, "operations", Some(attachment))?;
    let encoded = encode(out)?;
    let bytes: i64 = db.query_row(
        "SELECT coalesce((SELECT sum(length(state_json)) FROM attachments),0)+coalesce((SELECT sum(length(outcome_json)) FROM operations),0)",
        [],
        |r| r.get(0),
    )?;
    if bytes < 0 || bytes.saturating_add(encoded.len() as i64) > MAX_TOTAL_JSON_BYTES {
        return Err(Error::Limit(
            "attachment journal byte capacity exhausted".into(),
        ));
    }
    db.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,?4,?5,?6)",
        params![op, kind, attachment, expected_generation, request, encoded],
    )?;
    Ok(())
}
fn fence(s: &AttachmentState, g: u64) -> Result<()> {
    if s.generation != g {
        return Err(Error::Conflict("attachment generation changed".into()));
    }
    Ok(())
}
fn write_state(db: &Connection, old: &AttachmentState, next: &AttachmentState) -> Result<()> {
    if db.execute(
        "UPDATE attachments SET generation=?1,state_json=?2 WHERE id=?3 AND generation=?4",
        params![next.generation, encode(next)?, next.id, old.generation],
    )? != 1
    {
        return Err(Error::Conflict("attachment generation changed".into()));
    }
    Ok(())
}
fn read_state(db: &Connection, id: &str, repository: &str) -> Result<AttachmentState> {
    validate_id(id)?;
    let (generation, len): (u64, i64) = db
        .query_row(
            "SELECT generation,length(state_json) FROM attachments WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound("attachment".into()))?;
    if len < 0 || len as usize > MAX_JSON {
        return Err(Error::Limit("attachment state exceeds limit".into()));
    }
    let bytes: Vec<u8> = db.query_row(
        "SELECT substr(state_json,1,262145) FROM attachments WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if bytes.len() > MAX_JSON {
        return Err(Error::Limit("attachment state exceeds limit".into()));
    }
    let state: AttachmentState = serde_json::from_slice(&bytes)?;
    validate_state(&state, id, repository, generation)?;
    validate_relations(db, &state, true)?;
    Ok(state)
}
fn validate_relations(
    db: &Connection,
    state: &AttachmentState,
    require_reverse: bool,
) -> Result<()> {
    let id = &state.id;
    let baseline: Option<String> = db
        .query_row(
            "SELECT native_revision FROM mappings WHERE id=?1 AND git_commit=?2",
            params![id, state.target_commit],
            |r| r.get(0),
        )
        .optional()?;
    let head: Option<(String, String)> = db
        .query_row(
            "SELECT native_revision,git_tree FROM mappings WHERE id=?1 AND git_commit=?2",
            params![id, state.head_commit],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if baseline.as_deref() != Some(state.baseline_revision.as_str()) || head.is_none() {
        return Err(Error::Invalid(
            "attachment Git mapping is missing or inconsistent".into(),
        ));
    }
    let (head_revision, head_tree) = head.unwrap();
    validate_id(&head_revision)?;
    validate_oid(&head_tree)?;
    if state.source == SourceMode::Committed && state.source_revision != head_revision {
        return Err(Error::Invalid(
            "committed attachment source mapping is inconsistent".into(),
        ));
    }
    let mapping_count: i64 =
        db.query_row("SELECT count(*) FROM mappings WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    if mapping_count <= 0 || mapping_count > MAX_MAPPINGS_PER_ATTACHMENT {
        return Err(Error::Limit("attachment mapping capacity exceeded".into()));
    }
    if let Some(change) = &state.change_id {
        let binding: Option<String> = db
            .query_row(
                "SELECT attachment FROM bindings WHERE change_id=?1",
                [&change.id],
                |r| r.get(0),
            )
            .optional()?;
        if binding.as_deref() != Some(id) {
            return Err(Error::Invalid(
                "attachment Change binding is inconsistent".into(),
            ));
        }
    }
    if require_reverse {
        let reverse: Option<String> = db
            .query_row(
                "SELECT change_id FROM bindings WHERE attachment=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if reverse.as_deref() != state.change_id.as_ref().map(|c| c.id.as_str()) {
            return Err(Error::Invalid(
                "attachment reverse Change binding is inconsistent".into(),
            ));
        }
    }
    Ok(())
}
fn validate_historical_state(db: &Connection, state: &AttachmentState) -> Result<()> {
    let repository: String = db.query_row(
        "SELECT repository FROM identity WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    validate_state(state, &state.id, &repository, state.generation)?;
    validate_relations(db, state, false)?;
    let current = read_state(db, &state.id, &repository)?;
    if state.generation > current.generation
        || state.repository != current.repository
        || state.git != current.git
        || state.worktree != current.worktree
        || state.git_common_dir != current.git_common_dir
        || state.git_dir != current.git_dir
        || state.index != current.index
        || state.actor != current.actor
        || state.branch != current.branch
        || state.remote != current.remote
        || state.remote_url != current.remote_url
        || state.target != current.target
        || state.output != current.output
        || state.source != current.source
        || state.paths != current.paths
        || state
            .change_id
            .as_ref()
            .is_some_and(|change| current.change_id.as_ref() != Some(change))
    {
        return Err(Error::Invalid(
            "retained attachment identity is inconsistent with current state".into(),
        ));
    }
    Ok(())
}
fn validate_capture_outcome(
    db: &Connection,
    out: &CaptureOutcome,
    id: &str,
    expected_generation: u64,
) -> Result<()> {
    let expected_result_generation = if out.no_op {
        expected_generation
    } else {
        expected_generation
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("retained capture generation overflow".into()))?
    };
    if out.attachment.id != id
        || out.attachment.generation != expected_result_generation
        || out.revision != out.attachment.candidate_revision
        || !out.historical
        || out.changed_paths.len() > 100_000
        || out.excluded_total > 100_000
        || out.changed_paths.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(Error::Invalid(
            "retained capture outcome is inconsistent".into(),
        ));
    }
    for path in &out.changed_paths {
        if path == "." || validate_attachment_paths(std::slice::from_ref(path)).is_err() {
            return Err(Error::Invalid(
                "retained capture changed path is invalid".into(),
            ));
        }
    }
    if out.attachment.source == SourceMode::Committed
        && (!out.changed_paths.is_empty() || out.excluded_total != 0)
    {
        return Err(Error::Invalid(
            "retained committed capture outcome is inconsistent".into(),
        ));
    }
    validate_historical_state(db, &out.attachment)
}
fn validate_state(
    state: &AttachmentState,
    id: &str,
    repository: &str,
    generation: u64,
) -> Result<()> {
    if state.id != id || state.repository != repository || state.generation != generation {
        return Err(Error::Invalid(
            "attachment row/state identity mismatch".into(),
        ));
    }
    validate_request(&state_request(state))?;
    for revision in [
        &state.baseline_revision,
        &state.candidate_revision,
        &state.source_revision,
        &state.source_fingerprint,
    ] {
        validate_id(revision)?;
    }
    for oid in [&state.head_commit, &state.target_commit] {
        validate_oid(oid)?;
    }
    for path in [
        &state.worktree,
        &state.git_common_dir,
        &state.git_dir,
        &state.index,
    ] {
        if !Path::new(path).is_absolute() {
            return Err(Error::Invalid("attachment path is not absolute".into()));
        }
    }
    if attachment_id(
        Path::new(&state.git_dir),
        Path::new(&state.index),
        &state.actor,
    ) != id
    {
        return Err(Error::Invalid(
            "attachment worktree identity mismatch".into(),
        ));
    }
    safe_remote(&state.remote_url)?;
    if let Some(change) = &state.change_id {
        change.validate()?;
        if change.repository != repository {
            return Err(Error::Invalid("foreign Change".into()));
        }
    }
    if let Some(context) = &state.context {
        if Some(&context.id) != state.change_id.as_ref() {
            return Err(Error::Invalid("invalid context binding".into()));
        }
        validate_id(&context.context_digest)?;
    }
    Ok(())
}
fn mapping(db: &Connection, id: &str, commit: &str, revision: &str, tree: &str) -> Result<()> {
    validate_oid(commit)?;
    validate_oid(tree)?;
    validate_id(revision)?;
    let existing: Option<(String, String)> = db
        .query_row(
            "SELECT native_revision,git_tree FROM mappings WHERE id=?1 AND git_commit=?2",
            params![id, commit],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some(pair) = existing {
        if pair != (revision.into(), tree.into()) {
            return Err(Error::Conflict("immutable Git mapping changed".into()));
        }
        return Ok(());
    }
    capacity(db, "mappings", Some(id))?;
    db.execute(
        "INSERT INTO mappings VALUES(?1,?2,?3,?4)",
        params![id, commit, revision, tree],
    )?;
    Ok(())
}
fn validate_oid(s: &str) -> Result<()> {
    if !matches!(s.len(), 40 | 64)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid("invalid Git object ID".into()));
    }
    Ok(())
}
fn capture_commit(
    engine: &Engine,
    repository: &str,
    r: &AttachRequest,
    commit: &str,
) -> Result<(String, String)> {
    git_snapshot::capture_immutable(
        engine,
        repository,
        &SnapshotRequest {
            git: r.git.clone(),
            repository: r.worktree.clone(),
            revision: commit.into(),
            exclude: Vec::new(),
        },
    )
}
fn safe_remote(url: &str) -> Result<()> {
    // Phase 1 admits no HTTP userinfo, query strings or fragments; errors never echo it.
    if url.contains('@')
        || url.len() > 4096
        || url.contains(['?', '#'])
        || url.contains("://")
            && url
                .split("://")
                .nth(1)
                .is_some_and(|s| s.split('/').next().unwrap_or("").contains('@'))
        || url.bytes().any(|b| b.is_ascii_control())
    {
        return Err(Error::Invalid(
            "credential-bearing or unsupported remote URL; use a credential-free remote identity"
                .into(),
        ));
    }
    Ok(())
}

fn validate_request(r: &AttachRequest) -> Result<()> {
    if !r.git.is_absolute() || !r.worktree.is_absolute() {
        return Err(Error::Invalid(
            "git executable path must be absolute".into(),
        ));
    }
    ChangeId {
        repository: "attachment".into(),
        id: r.actor.clone(),
    }
    .validate()?;
    for (label, value) in [
        ("remote", &r.remote),
        ("target", &r.target),
        ("output", &r.output),
    ] {
        if value.starts_with('-')
            || value.starts_with('/')
            || value.ends_with('/')
            || value.ends_with(".lock")
            || value.contains("//")
            || value.is_empty()
            || value.len() > 1024
            || value
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || value.contains("..")
            || value.contains("@{")
            || value.contains(['~', '^', ':', '?', '*', '[', '\\'])
        {
            return Err(Error::Invalid(format!("{label} is not a safe Git name")));
        }
    }
    if r.output == r.target {
        return Err(Error::Invalid(
            "output branch must differ from the target branch".into(),
        ));
    }
    if r.paths.len() > MAX_PATHS {
        return Err(Error::Limit(format!(
            "attachment selects more than {MAX_PATHS} paths"
        )));
    }
    if r.source == SourceMode::Worktree {
        validate_attachment_paths(&r.paths)?;
    }
    if r.source == SourceMode::Committed && r.paths != ["."] {
        return Err(Error::Invalid("committed mode requires explicit --path . for the entire immutable commit; narrower selection requires worktree mode".into()));
    }
    if r.source == SourceMode::Worktree && r.paths.is_empty() {
        return Err(Error::Invalid(
            "worktree source requires explicit --path selection".into(),
        ));
    }
    if r.source == SourceMode::Index {
        return Err(Error::Invalid("index source is not supported in phase 1; commit the intended index state and use --source committed, or select working-tree paths with --source worktree".into()));
    }
    Ok(())
}
fn validate_operation(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(Error::Invalid(
            "operation ID must be 1..128 portable identifier bytes".into(),
        ));
    }
    Ok(())
}
fn observe(r: &AttachRequest) -> Result<Observation> {
    git_snapshot::git_version(&r.git)?;
    let root = PathBuf::from(git(
        r,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )?);
    let common = PathBuf::from(git(
        r,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let git_dir = PathBuf::from(git(
        r,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?);
    let index = PathBuf::from(git(
        r,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let branch = git(r, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| {
        Error::Conflict(
            "detached HEAD is unsupported; attach a named local branch and retry".into(),
        )
    })?;
    let head = git(r, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if root.canonicalize()? != r.worktree.canonicalize()? {
        return Err(Error::Invalid(
            "attachment requires the exact worktree root".into(),
        ));
    }
    let remote_url = git(r, &["remote", "get-url", "--", &r.remote])?;
    safe_remote(&remote_url)?;
    if git_optional(r, &["config", "--get-regexp", r"^filter\."])?
        .is_some_and(|value| !value.is_empty())
    {
        return Err(Error::Conflict(
            "configured Git content filters make attached bytes unverifiable; disable the filters in this checkout and retry".into(),
        ));
    }
    let target = git(
        r,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/remotes/{}/{}^{{commit}}", r.remote, r.target),
        ],
    )
    .map_err(|_| {
        Error::Conflict("exact remote-tracking target missing; fetch it explicitly".into())
    })?;
    if !git(r, &["ls-files", "--unmerged", "-z"])?.is_empty() {
        return Err(Error::Conflict(
            "unmerged index is unsupported; resolve it explicitly".into(),
        ));
    }
    let flags = git(r, &["ls-files", "-v"])?;
    if flags.lines().any(|line| {
        line.as_bytes()
            .first()
            .is_some_and(|b| *b == b'S' || b.is_ascii_lowercase())
    }) {
        return Err(Error::Conflict("skip-worktree or assume-unchanged index flags hide working-tree state; clear them explicitly and retry".into()));
    }
    // Raw worktree semantics are explicit: no clean/smudge conversion is performed.
    // Immutable commits remain canonical Git bytes; a conversion-induced difference is real raw source.
    if fs::metadata(&index)?.len() > OUTPUT_LIMIT {
        return Err(Error::Limit("Git index exceeds observation limit".into()));
    }
    let mut bytes = Vec::new();
    fs::File::open(&index)?
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(Error::Limit("Git index exceeds observation limit".into()));
    }
    Ok(Observation {
        root,
        common,
        git_dir,
        index,
        branch,
        head,
        remote_url,
        target,
        index_len: bytes.len() as u64,
        index_sha256: format!("{:x}", Sha256::digest(bytes)),
    })
}
fn ensure_unchanged(r: &AttachRequest, before: &Observation) -> Result<()> {
    let after = observe(r)?;
    if observation_digest(before) != observation_digest(&after) {
        return Err(Error::Conflict("Git or working-tree identity changed during capture; no attachment state was advanced, retry with a new operation ID".into()));
    }
    Ok(())
}
fn require_forward(r: &AttachRequest, old: &str, new: &str, label: &str) -> Result<()> {
    if old == new {
        return Ok(());
    }
    if !is_ancestor(r, old, new)? {
        return Err(Error::Conflict(format!(
            "the {label} was rewritten or diverged; restore the recorded ancestry; supersede/rebase recovery is unsupported in phase 1"
        )));
    }
    Ok(())
}
fn is_ancestor(r: &AttachRequest, old: &str, new: &str) -> Result<bool> {
    let mut c = git_command(r);
    c.args(["merge-base", "--is-ancestor", old, new]);
    let out = capture_with_input(
        &mut c,
        None,
        GIT_TIMEOUT,
        OUTPUT_LIMIT,
        &Cancellation::default(),
    )?;
    match (out.stop, out.code) {
        (None, Some(0)) => Ok(true),
        (None, Some(1)) => Ok(false),
        _ => Err(Error::Invalid(
            "git merge-base failed; ancestry is unavailable (for example missing shallow/partial history or a process timeout)".into(),
        )),
    }
}
fn ensure_candidate_target_derived(engine: &Engine, state: &AttachmentState) -> Result<()> {
    if attached_candidate_is_target_derived(
        engine,
        &state.source_revision,
        &state.candidate_revision,
        &state.baseline_revision,
    )? {
        Ok(())
    } else {
        Err(Error::Conflict(
            "the native candidate contains changes not attributable to the recorded source or target; refusing to discard them while adopting Git's merge resolution".into(),
        ))
    }
}
fn validate_identity(s: &AttachmentState, o: &Observation) -> Result<()> {
    if s.worktree != path_string(&o.root)
        || s.git_common_dir != path_string(&o.common)
        || s.git_dir != path_string(&o.git_dir)
        || s.index != path_string(&o.index)
    {
        return Err(Error::Conflict("the Git common directory, linked-worktree directory or index changed; restore the original attached worktree; superseding is unsupported".into()));
    }
    if s.branch != o.branch {
        return Err(Error::Conflict("the attached branch was deleted, renamed or replaced; restore the original branch; superseding is unsupported".into()));
    }
    if s.remote_url != o.remote_url {
        return Err(Error::Conflict("the attached remote URL changed; restore the recorded credential-free remote; superseding is unsupported".into()));
    }
    Ok(())
}
fn observation_digest(o: &Observation) -> String {
    digest(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            path_string(&o.root),
            path_string(&o.common),
            path_string(&o.git_dir),
            path_string(&o.index),
            o.branch,
            o.head,
            o.remote_url,
            o.target,
            o.index_len,
            o.index_sha256
        )
        .as_bytes(),
    )
}
fn attachment_id(git_dir: &Path, index: &Path, actor: &str) -> String {
    digest(
        format!(
            "agentscm-attached-worktree-v1\0{}\0{}\0{}",
            path_string(git_dir),
            path_string(index),
            actor
        )
        .as_bytes(),
    )
}
fn state_request(s: &AttachmentState) -> AttachRequest {
    AttachRequest {
        git: PathBuf::from(&s.git),
        worktree: PathBuf::from(&s.worktree),
        actor: s.actor.clone(),
        remote: s.remote.clone(),
        target: s.target.clone(),
        output: s.output.clone(),
        source: s.source,
        paths: s.paths.clone(),
    }
}
fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
fn git(r: &AttachRequest, args: &[&str]) -> Result<String> {
    let mut c = git_command(r);
    c.args(args);
    let out = capture_with_input(
        &mut c,
        None,
        GIT_TIMEOUT,
        OUTPUT_LIMIT,
        &Cancellation::default(),
    )?;
    if out.stop.is_some() || out.code != Some(0) {
        return Err(Error::Invalid(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            "details suppressed to protect remote credentials"
        )));
    }
    let value = String::from_utf8(out.stdout)
        .map_err(|_| Error::Invalid("Git returned non-UTF-8 identity output".into()))?;
    Ok(value.trim_end_matches('\n').into())
}
fn git_command(r: &AttachRequest) -> Command {
    let mut c = Command::new(&r.git);
    c.env_clear();
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            c.env(name, value);
        }
    }
    c.env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .process_group(0)
        .arg("-C")
        .arg(&r.worktree)
        .args([
            "--no-replace-objects",
            "--no-lazy-fetch",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
    c
}

fn git_optional(r: &AttachRequest, args: &[&str]) -> Result<Option<String>> {
    let mut c = Command::new(&r.git);
    c.env_clear();
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            c.env(name, value);
        }
    }
    c.env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .process_group(0)
        .arg("-C")
        .arg(&r.worktree)
        .args([
            "--no-replace-objects",
            "--no-lazy-fetch",
            "--no-optional-locks",
        ])
        .args(args);
    let out = capture_with_input(
        &mut c,
        None,
        GIT_TIMEOUT,
        OUTPUT_LIMIT,
        &Cancellation::default(),
    )?;
    match out.code {
        Some(0) if out.stop.is_none() => Ok(Some(
            String::from_utf8(out.stdout)
                .map_err(|_| Error::Invalid("Git returned non-UTF-8 configuration".into()))?
                .trim_end_matches('\n')
                .into(),
        )),
        Some(1) if out.stop.is_none() => Ok(None),
        _ => Err(Error::Invalid(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            "details suppressed to protect remote credentials"
        ))),
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use tempfile::TempDir;
    fn run(root: &Path, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(root)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn ancestry_process_failure_is_not_rewritten_as_divergence() {
        let temporary = TempDir::new().unwrap();
        let script = temporary.path().join("git");
        fs::write(&script, "#!/bin/sh\nexit 2\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let request = AttachRequest {
            git: script,
            worktree: temporary.path().into(),
            actor: "actor".into(),
            remote: "origin".into(),
            target: "main".into(),
            output: "feature".into(),
            source: SourceMode::Committed,
            paths: vec![".".into()],
        };
        let error = require_forward(&request, &"a".repeat(40), &"b".repeat(40), "head")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ancestry is unavailable"), "{error}");
        assert!(!error.contains("rewritten"), "{error}");
    }
    #[test]
    fn mutation_at_completion_blocks_bytes_absence_and_rules() {
        for action in 0..3 {
            let root = TempDir::new().unwrap();
            let data = TempDir::new().unwrap();
            run(root.path(), &["init", "-q"]);
            fs::write(root.path().join("a"), b"base").unwrap();
            run(root.path(), &["add", "a"]);
            run(root.path(), &["commit", "-qm", "base"]);
            run(
                root.path(),
                &["remote", "add", "origin", "https://example.invalid/repo"],
            );
            run(
                root.path(),
                &["update-ref", "refs/remotes/origin/main", "HEAD"],
            );
            run(root.path(), &["config", "core.excludesFile", ""]);
            let git = data.path().join("synthetic-git");
            fs::write(&git,b"#!/bin/sh\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1\nexec /usr/bin/git \"$@\"\n").unwrap();
            fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
            let engine = Engine::open(data.path()).unwrap();
            let mut journal = AttachmentJournal::open(data.path(), "test").unwrap();
            let state = journal
                .attach(
                    &engine,
                    "attach",
                    &AttachRequest {
                        git,
                        worktree: root.path().into(),
                        actor: "actor".into(),
                        remote: "origin".into(),
                        target: "main".into(),
                        output: "feature".into(),
                        source: SourceMode::Worktree,
                        paths: vec!["a".into()],
                    },
                )
                .unwrap();
            let source = root.path().to_owned();
            journal.before_validation = Some(Box::new(move || match action {
                0 => fs::write(source.join("a"), b"race").unwrap(),
                1 => fs::remove_file(source.join("a")).unwrap(),
                _ => fs::write(source.join(".gitignore"), b"# race").unwrap(),
            }));
            assert!(
                journal
                    .capture(&engine, "capture", &state.id, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("changed")
            );
            assert_eq!(journal.state(&state.id).unwrap(), state);
        }
    }
}
