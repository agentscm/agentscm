//! Private, local preparation for an interrupted GitHub or GitLab workflow.
//!
//! Saved drafts and caller-asserted check receipts confer no review, CI, approval or
//! publication authority. This module has no network client or publication executor.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
))]
use std::{ffi::CString, os::fd::AsRawFd, os::unix::ffi::OsStrExt};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::change_capture::{
    self, GitSourceRequest, LocalSpool, ObservedCommandRequest, SpoolIdentityV1, SpoolLimits,
};
use crate::change_record::{ActionFinishV1, CoverageV1, DriftV1, SourceBindingV1};
use crate::{Error, Result};

const MAX_ROWS: i64 = 256;
const MAX_TEXT: usize = 32 * 1024;
/// This identifies only the continuity execution profile, never the host environment.
const EXECUTION_PROFILE: &[u8] = b"agentscm-continuity-v1:explicit-caller-command:partial-coverage:environment-and-toolchain-unmeasured:timeout-1..300s:output-1MiB";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ForgeProvider {
    Github,
    Gitlab,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForgeIdentity {
    pub provider: ForgeProvider,
    pub host: String,
    pub repository: String,
}

impl ForgeIdentity {
    pub fn validate(&self) -> Result<()> {
        if self.host.is_empty()
            || self.host.len() > 253
            || !self
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(invalid("invalid forge host"));
        }
        let parts: Vec<_> = self.repository.split('/').collect();
        if parts.len() < 2
            || (self.provider == ForgeProvider::Github && parts.len() != 2)
            || self.repository.len() > 512
            || parts.iter().any(|p| {
                p.is_empty()
                    || *p == "."
                    || *p == ".."
                    || !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        {
            return Err(invalid("invalid forge repository"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub created_at: u64,
    pub checkout: PathBuf,
    pub checkout_device: u64,
    pub checkout_inode: u64,
    pub forge: ForgeIdentity,
    pub forge_identity_assurance: String,
    pub base_ref: String,
    pub base_tip: String,
    pub context: String,
    pub source: SourceBindingV1,
    pub spool_identity: SpoolIdentityV1,
    /// Durable cleanup intent. Operations refuse this task until deletion is retried.
    #[serde(default)]
    pub deletion_pending: Option<DeletionPending>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionPending {
    pub started_at: u64,
    /// Pin both the root inode and logical spool identity before any file is removed.
    pub spool_identity: SpoolIdentityV1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationDraft {
    pub id: String,
    pub created_at: u64,
    pub forge: ForgeIdentity,
    /// `pull_request` for GitHub; `merge_request` for GitLab.
    pub publication_kind: String,
    pub commit: String,
    pub tree: String,
    pub base_tip: String,
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub body: String,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRecord {
    pub operation_id: String,
    pub created_at: u64,
    pub argv: Vec<String>,
    pub timeout_seconds: u64,
    /// Reserved attempts remain unknown after interruption and are never rerun.
    pub state: String,
    pub source: SourceBindingV1,
    pub base_tip: String,
    pub finish: Option<ActionFinishV1>,
    pub exit_code: Option<i32>,
    pub stop: Option<String>,
    pub storage_warnings: Vec<String>,
    pub environment_assurance: String,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub task: Task,
    pub checks: Vec<CheckRecord>,
    pub drafts: Vec<PublicationDraft>,
}

#[derive(Debug, Serialize)]
pub struct Revalidation {
    pub local_blockers: Vec<String>,
    pub checks: Vec<CheckApplicability>,
    pub remote_freshness: String,
    pub publication_authorized: bool,
    pub evidence_scope: String,
}

#[derive(Debug, Serialize)]
pub struct CheckApplicability {
    pub operation_id: String,
    pub applicability: String,
}

/// A journal belongs to one canonical checkout. SQLite serializes durable reservations.
/// Private state is trusted local data, rather than an input from another user.
pub struct Journal {
    root: PathBuf,
    checkout: PathBuf,
    directory: File,
    db: Connection,
}

impl Journal {
    pub fn open(state_dir: &Path, checkout: &Path) -> Result<Self> {
        let checkout = checkout.canonicalize()?;
        if git(&checkout, &["rev-parse", "--show-toplevel"])? != checkout.to_string_lossy() {
            return Err(invalid("checkout must be the canonical Git root"));
        }
        if !state_dir.is_absolute()
            || state_dir
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(invalid(
                "state directory must be an absolute path without traversal",
            ));
        }
        if state_dir.starts_with(&checkout) || checkout.starts_with(state_dir) {
            return Err(invalid(
                "state directory must be separate from the checkout",
            ));
        }
        let mut ancestor = PathBuf::new();
        for component in state_dir.components() {
            ancestor.push(component);
            match fs::symlink_metadata(&ancestor) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(invalid("state directory cannot use symlinks"));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    DirBuilder::new().mode(0o700).create(&ancestor)?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        let root = state_dir.canonicalize()?;
        if root.starts_with(&checkout) || checkout.starts_with(&root) {
            return Err(invalid(
                "state directory must be separate from the checkout",
            ));
        }
        let metadata = fs::symlink_metadata(&root)?;
        // SAFETY: geteuid has no arguments or memory access requirements.
        if !metadata.is_dir()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(invalid(
                "state directory must be private and owned by this user",
            ));
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&root)?;
        // Hold an advisory descriptor lock during each journal call, including an explicit
        // command execution. A concurrent owner fails closed rather than rerunning it.
        directory
            .try_lock()
            .map_err(|_| Error::Conflict("continuity journal is in use".into()))?;
        let db_path = root.join("journal.sqlite3");
        match fs::symlink_metadata(&db_path) {
            Ok(m)
                if !m.is_file()
                    || m.nlink() != 1
                    || m.mode() & 0o077 != 0
                    || m.uid() != metadata.uid() =>
            {
                return Err(invalid("unsafe continuity journal file"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                initialize_journal(&root, &directory, &checkout)?;
            }
            Err(e) => return Err(e.into()),
        }
        for suffix in ["-journal", "-wal", "-shm"] {
            match fs::symlink_metadata(root.join(format!("journal.sqlite3{suffix}"))) {
                Ok(m)
                    if !m.is_file()
                        || m.nlink() != 1
                        || m.mode() & 0o077 != 0
                        || m.uid() != metadata.uid() =>
                {
                    return Err(invalid("unsafe SQLite auxiliary file"));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut header = [0u8; 100];
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&db_path)?
            .read_exact(&mut header)
            .map_err(|_| invalid("unrecognized continuity journal"))?;
        if &header[..16] != b"SQLite format 3\0"
            || u32::from_be_bytes(header[60..64].try_into().expect("fixed header field")) != 1
            || u32::from_be_bytes(header[68..72].try_into().expect("fixed header field"))
                != 0x41474354
        {
            return Err(invalid("unsupported or unrelated continuity journal"));
        }
        let db =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let bound: String = db.query_row("SELECT checkout FROM binding", [], |r| r.get(0))?;
        if bound != checkout.to_string_lossy() {
            return Err(invalid("journal belongs to another checkout"));
        }
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;",
        )?;
        let result = Self {
            root,
            checkout,
            directory,
            db,
        };
        result.verify_path()?;
        Ok(result)
    }

    fn verify_path(&self) -> Result<()> {
        let m = fs::symlink_metadata(&self.root)?;
        let held = self.directory.metadata()?;
        if m.file_type().is_symlink()
            || m.dev() != held.dev()
            || m.ino() != held.ino()
            || self.checkout.canonicalize()? != self.checkout
        {
            return Err(invalid("continuity directory or checkout binding changed"));
        }
        Ok(())
    }

    pub fn start(
        &self,
        id: &str,
        base_ref: &str,
        context: &str,
        forge: ForgeIdentity,
    ) -> Result<Task> {
        self.verify_path()?;
        safe_id(id)?;
        bounded(context, MAX_TEXT)?;
        forge.validate()?;
        valid_ref(base_ref)?;
        if origin(&self.checkout)? != (forge.host.clone(), forge.repository.clone()) {
            return Err(invalid("declared forge does not match checkout origin"));
        }
        if self
            .db
            .query_row("SELECT count(*) FROM tasks WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })?
            != 0
        {
            return Err(Error::Conflict("task ID already exists".into()));
        }
        self.capacity("tasks", None)?;
        let base_tip = git(
            &self.checkout,
            &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
        )?;
        let created_at = now()?;
        let checkout_metadata = self.checkout.metadata()?;
        let path = self.root.join(format!("spool-{id}"));
        // Never adopt an unbound spool left by an interrupted start. Only this
        // invocation's newly created directory is eligible for failure cleanup.
        DirBuilder::new().mode(0o700).create(&path)?;
        let created = fs::symlink_metadata(&path)?;
        let result = (|| {
            let spool = LocalSpool::open(&path).map_err(capture_error)?;
            let request = self.request(id, base_ref, &forge);
            let source =
                change_capture::capture_git_source(&request, &spool).map_err(capture_error)?;
            let task = Task {
                id: id.into(),
                created_at,
                checkout: self.checkout.clone(),
                checkout_device: checkout_metadata.dev(),
                checkout_inode: checkout_metadata.ino(),
                forge,
                forge_identity_assurance:
                    "declared provider and local origin match; remote immutable identity unverified"
                        .into(),
                base_ref: base_ref.into(),
                base_tip,
                context: context.into(),
                source,
                spool_identity: spool.identity(),
                deletion_pending: None,
            };
            self.db.execute(
                "INSERT INTO tasks(id,body) VALUES(?1,?2)",
                params![id, serde_json::to_string(&task)?],
            )?;
            Ok(task)
        })();
        if result.is_err() {
            self.verify_path()?;
            let current = fs::symlink_metadata(&path)?;
            if !current.is_dir()
                || current.file_type().is_symlink()
                || current.dev() != created.dev()
                || current.ino() != created.ino()
            {
                return Err(invalid(
                    "failed-start spool was replaced; replacement preserved",
                ));
            }
            bounded_remove(&path, &mut 0)?;
            self.directory.sync_all()?;
        }
        result
    }

    pub fn status(&self, id: &str) -> Result<Status> {
        self.verify_path()?;
        safe_id(id)?;
        let body: Option<String> = self
            .db
            .query_row("SELECT body FROM tasks WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        let task: Task =
            serde_json::from_str(&body.ok_or_else(|| Error::NotFound("continuity task".into()))?)?;
        safe_id(&task.id)?;
        task.forge.validate()?;
        valid_ref(&task.base_ref)?;
        if task.id != id || task.checkout != self.checkout {
            return Err(invalid("task binding differs from journal"));
        }
        let metadata = self.checkout.metadata()?;
        if task.checkout_device != metadata.dev() || task.checkout_inode != metadata.ino() {
            return Err(invalid("checkout directory identity changed"));
        }
        Ok(Status {
            task,
            checks: self.rows("checks", id)?,
            drafts: self.rows("drafts", id)?,
        })
    }

    pub fn check(
        &self,
        id: &str,
        operation_id: &str,
        argv: Vec<String>,
        timeout_seconds: u64,
    ) -> Result<CheckRecord> {
        safe_id(operation_id)?;
        if !(1..=300).contains(&timeout_seconds)
            || argv.is_empty()
            || !Path::new(&argv[0]).is_absolute()
            || argv.len() > 128
            || argv
                .iter()
                .try_fold(0usize, |total, arg| total.checked_add(arg.len()))
                .is_none_or(|bytes| bytes > MAX_TEXT)
            || argv.iter().any(|s| s.contains('\0'))
        {
            return Err(invalid("invalid bounded check command"));
        }
        let status = self.status(id)?;
        require_active(&status.task)?;
        if let Some(prior) = status
            .checks
            .into_iter()
            .find(|c| c.operation_id == operation_id)
        {
            if prior.argv != argv || prior.timeout_seconds != timeout_seconds {
                return Err(Error::Conflict(
                    "operation ID already binds another command".into(),
                ));
            }
            return Ok(prior);
        }
        self.capacity("checks", Some(id))?;
        if origin(&self.checkout)?
            != (
                status.task.forge.host.clone(),
                status.task.forge.repository.clone(),
            )
        {
            return Err(invalid("checkout origin identity changed"));
        }
        let spool = self.spool(&status.task)?;
        let source_request = self.request(id, &status.task.base_ref, &status.task.forge);
        let mut record = CheckRecord {
            operation_id: operation_id.into(),
            created_at: now()?,
            argv: argv.clone(),
            timeout_seconds,
            state: "unknown".into(),
            source: change_capture::capture_git_source(&source_request, &spool)
                .map_err(capture_error)?,
            base_tip: git(
                &self.checkout,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", status.task.base_ref),
                ],
            )?,
            finish: None,
            exit_code: None,
            stop: None,
            storage_warnings: Vec::new(),
            environment_assurance: "environment, toolchain and dependencies unmeasured".into(),
        };
        // Commit the reservation before launch. Any later error leaves an unknown attempt.
        self.db.execute(
            "INSERT INTO checks(task,id,body) VALUES(?1,?2,?3)",
            params![id, operation_id, serde_json::to_string(&record)?],
        )?;
        let observed = change_capture::run_observed_command(
            ObservedCommandRequest {
                start_operation_id: format!("{id}:{operation_id}:start"),
                finish_operation_id: format!("{id}:{operation_id}:finish"),
                // Private spool identity only; this is never a Hub Work or uploaded envelope.
                work_id: format!(
                    "wrk_continuity_{}",
                    &crate::checks::sha256(id.as_bytes())[..32]
                ),
                generation: 1,
                session_id: format!("continuity-{id}"),
                action_id: operation_id.into(),
                source: source_request,
                argv,
                setup_digest: crate::checks::sha256(EXECUTION_PROFILE),
                coverage: CoverageV1::Partial,
                timeout: Duration::from_secs(timeout_seconds),
                max_output_bytes: 1024 * 1024,
            },
            &spool,
        )
        .map_err(capture_error)?;
        record.storage_warnings = observed.storage_warnings;
        if record.source != observed.start.source {
            record
                .storage_warnings
                .push("source changed between reservation and runner start".into());
        }
        record.source = observed.start.source;
        if record.base_tip
            != git(
                &self.checkout,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", status.task.base_ref),
                ],
            )?
        {
            record
                .storage_warnings
                .push("base tip changed during command observation".into());
        }
        record.state = if record.storage_warnings.is_empty() {
            "finished"
        } else {
            "unknown"
        }
        .into();
        record.exit_code = observed.exit_code;
        record.stop = Some(format!("{:?}", observed.stop));
        record.finish = Some(observed.finish);
        self.verify_path()?;
        self.db.execute(
            "UPDATE checks SET body=?3 WHERE task=?1 AND id=?2",
            params![id, operation_id, serde_json::to_string(&record)?],
        )?;
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draft(
        &self,
        id: &str,
        draft_id: &str,
        source_branch: &str,
        target_branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PublicationDraft> {
        safe_id(draft_id)?;
        valid_ref(source_branch)?;
        valid_ref(target_branch)?;
        bounded(title, 1024)?;
        bounded(body, MAX_TEXT)?;
        if title.is_empty() {
            return Err(invalid("draft title is required"));
        }
        let status = self.status(id)?;
        require_active(&status.task)?;
        let base_target = status
            .task
            .base_ref
            .strip_prefix("refs/remotes/origin/")
            .or_else(|| status.task.base_ref.strip_prefix("refs/heads/"))
            .or_else(|| status.task.base_ref.strip_prefix("origin/"))
            .unwrap_or(&status.task.base_ref);
        if base_target != target_branch {
            return Err(invalid("draft target must match the saved task base ref"));
        }
        if origin(&self.checkout)?
            != (
                status.task.forge.host.clone(),
                status.task.forge.repository.clone(),
            )
        {
            return Err(invalid("checkout origin identity changed"));
        }
        self.capacity("drafts", Some(id))?;
        let source = change_capture::observe_git(&self.request(
            id,
            &status.task.base_ref,
            &status.task.forge,
        ))
        .map_err(capture_error)?;
        let SourceBindingV1::Git {
            head, tree, dirty, ..
        } = source
        else {
            return Err(invalid("expected Git source"));
        };
        if dirty {
            return Err(invalid(
                "publication draft requires a clean source checkout",
            ));
        }
        if git(&self.checkout, &["symbolic-ref", "--quiet", "HEAD"])?
            != format!("refs/heads/{source_branch}")
        {
            return Err(invalid("source branch is not the current branch"));
        }
        let draft = PublicationDraft {
            id: draft_id.into(),
            created_at: now()?,
            forge: status.task.forge.clone(),
            publication_kind: match status.task.forge.provider {
                ForgeProvider::Github => "pull_request",
                ForgeProvider::Gitlab => "merge_request",
            }
            .into(),
            commit: head,
            tree,
            base_tip: git(
                &self.checkout,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", status.task.base_ref),
                ],
            )?,
            source_branch: source_branch.into(),
            target_branch: target_branch.into(),
            title: title.into(),
            body: body.into(),
            cancelled: false,
        };
        self.db.execute(
            "INSERT INTO drafts(task,id,body) VALUES(?1,?2,?3)",
            params![id, draft_id, serde_json::to_string(&draft)?],
        )?;
        Ok(draft)
    }

    pub fn cancel_draft(&self, id: &str, draft_id: &str) -> Result<()> {
        safe_id(draft_id)?;
        let status = self.status(id)?;
        require_active(&status.task)?;
        let mut draft = status
            .drafts
            .into_iter()
            .find(|d| d.id == draft_id)
            .ok_or_else(|| Error::NotFound("publication draft".into()))?;
        draft.cancelled = true;
        self.db.execute(
            "UPDATE drafts SET body=?3 WHERE task=?1 AND id=?2",
            params![id, draft_id, serde_json::to_string(&draft)?],
        )?;
        Ok(())
    }

    pub fn revalidate(&self, id: &str) -> Result<Revalidation> {
        let status = self.status(id)?;
        require_active(&status.task)?;
        let mut local_blockers = Vec::new();
        match origin(&self.checkout) {
            Ok(identity)
                if identity
                    == (
                        status.task.forge.host.clone(),
                        status.task.forge.repository.clone(),
                    ) => {}
            _ => local_blockers.push("checkout origin identity changed or is unreadable".into()),
        }
        let spool = self.spool(&status.task)?;
        let current = change_capture::capture_git_source(
            &self.request(id, &status.task.base_ref, &status.task.forge),
            &spool,
        )
        .map_err(capture_error)?;
        if matches!(&current, SourceBindingV1::Git { dirty: true, .. }) {
            local_blockers.push("checkout has uncommitted source".into());
        }
        let current_base_tip = git(
            &self.checkout,
            &[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", status.task.base_ref),
            ],
        )?;
        let mut checks = Vec::new();
        for check in status.checks {
            let applicability = if check.state == "unknown"
                || check
                    .finish
                    .as_ref()
                    .is_none_or(|f| f.drift == DriftV1::Unknown)
            {
                "unknown"
            } else if check.base_tip != current_base_tip
                || check.source != current
                || check
                    .finish
                    .as_ref()
                    .is_some_and(|f| f.drift != DriftV1::NoneObserved)
            {
                "stale"
            } else {
                "current"
            };
            checks.push(CheckApplicability {
                operation_id: check.operation_id,
                applicability: applicability.into(),
            });
        }
        if let SourceBindingV1::Git { head, .. } = &current {
            for draft in status.drafts.iter().filter(|d| !d.cancelled) {
                if draft.commit != *head {
                    local_blockers.push(format!("draft {} retains an earlier commit", draft.id));
                }
                if draft.base_tip != current_base_tip {
                    local_blockers.push(format!(
                        "draft {} local base ref moved since draft",
                        draft.id
                    ));
                }
                if git(&self.checkout, &["symbolic-ref", "--quiet", "HEAD"])
                    .ok()
                    .as_deref()
                    != Some(format!("refs/heads/{}", draft.source_branch).as_str())
                {
                    local_blockers.push(format!(
                        "draft {} current symbolic source branch differs or is detached",
                        draft.id
                    ));
                }
                match git(
                    &self.checkout,
                    &[
                        "rev-parse",
                        "--verify",
                        &format!("refs/heads/{}^{{commit}}", draft.source_branch),
                    ],
                ) {
                    Ok(tip) if tip == draft.commit => {}
                    Ok(_) => local_blockers.push(format!(
                        "draft {} saved local source branch moved",
                        draft.id
                    )),
                    Err(_) => local_blockers.push(format!(
                        "draft {} saved local source branch is missing or unreadable",
                        draft.id
                    )),
                }
            }
        }
        Ok(Revalidation {
            local_blockers,
            checks,
            remote_freshness: "unknown".into(),
            publication_authorized: false,
            evidence_scope: "captured tracked/nonignored source; ignored files, submodules, environment and dependencies remain outside verification".into(),
        })
    }

    /// Delete one task's private rows and bounded spool. Unknown execution attempts must
    /// remain visible: they cannot be deleted through this command.
    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.delete_task(id, false)
    }

    /// Explicitly discard unknown attempts after obtaining the exclusive journal lock.
    /// This discards evidence; it never retries or changes the execution outcome.
    pub fn delete_discarding_unknown(&mut self, id: &str) -> Result<()> {
        self.delete_task(id, true)
    }

    fn delete_task(&mut self, id: &str, discard_unknown: bool) -> Result<()> {
        let mut status = self.status(id)?;
        if status.task.deletion_pending.is_none() {
            if !discard_unknown && status.checks.iter().any(|c| c.state == "unknown") {
                return Err(Error::Conflict(
                    "unknown attempts must be retained for inspection".into(),
                ));
            }
            // Strict normal admission precedes the marker. A missing/replaced unmarked
            // spool is not evidence that this task's private data was deleted.
            let path = self.root.join(format!("spool-{id}"));
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.dev() != status.task.spool_identity.root.device
                || metadata.ino() != status.task.spool_identity.root.inode
            {
                return Err(invalid("unmarked task spool is missing or replaced"));
            }
            let spool = self.spool(&status.task)?;
            status.task.deletion_pending = Some(DeletionPending {
                started_at: now()?,
                spool_identity: spool.identity(),
            });
            self.db.execute(
                "UPDATE tasks SET body=?2 WHERE id=?1",
                params![id, serde_json::to_string(&status.task)?],
            )?;
        }
        let marker = status
            .task
            .deletion_pending
            .as_ref()
            .expect("deletion marker retained");
        if marker.spool_identity != status.task.spool_identity {
            return Err(invalid(
                "deletion marker differs from the task spool identity",
            ));
        }
        let path = self.root.join(format!("spool-{id}"));
        self.verify_path()?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                let expected = marker.spool_identity.root;
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.dev() != expected.device
                    || metadata.ino() != expected.inode
                {
                    return Err(invalid(
                        "deleting spool root was replaced; replacement preserved",
                    ));
                }
                // Partial removal may already have removed the logical identity files.
                // The durable marker and original root inode authorize only this cleanup.
                bounded_remove(&path, &mut 0)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.directory.sync_all()?;
        // A failed SQL transaction leaves the marker available for retry, including
        // when the original spool is already absent after successful cleanup.
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM checks WHERE task=?1", [id])?;
        tx.execute("DELETE FROM drafts WHERE task=?1", [id])?;
        tx.execute("DELETE FROM tasks WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    fn request(&self, id: &str, base_ref: &str, forge: &ForgeIdentity) -> GitSourceRequest {
        GitSourceRequest {
            repository: format!("{}/{}", forge.host, forge.repository),
            worktree_id: id.into(),
            repository_path: self.checkout.clone(),
            base_ref: base_ref.into(),
        }
    }
    fn spool(&self, task: &Task) -> Result<LocalSpool> {
        LocalSpool::open_with_limits_expected(
            self.root.join(format!("spool-{}", task.id)),
            SpoolLimits::default(),
            task.spool_identity,
        )
        .map_err(capture_error)
    }
    fn rows<T: serde::de::DeserializeOwned>(&self, table: &str, id: &str) -> Result<Vec<T>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT body FROM {table} WHERE task=?1 ORDER BY id LIMIT 257"
        ))?;
        let bodies = statement
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if bodies.len() > MAX_ROWS as usize {
            return Err(Error::Limit("too many continuity records".into()));
        }
        bodies
            .into_iter()
            .map(|s| serde_json::from_str(&s).map_err(Error::from))
            .collect()
    }
    fn capacity(&self, table: &str, task: Option<&str>) -> Result<()> {
        let count: i64 = if let Some(task) = task {
            self.db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE task=?1"),
                [task],
                |r| r.get(0),
            )?
        } else {
            self.db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?
        };
        if count >= MAX_ROWS {
            return Err(Error::Limit("continuity record limit reached".into()));
        }
        Ok(())
    }
}

fn bounded_remove(path: &Path, count: &mut usize) -> Result<()> {
    fn inspect(path: &Path, count: &mut usize) -> Result<()> {
        *count += 1;
        if *count > 20_000 {
            return Err(Error::Limit("retention deletion entry limit".into()));
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            for entry in fs::read_dir(path)? {
                inspect(&entry?.path(), count)?;
            }
        }
        Ok(())
    }
    inspect(path, count)?;
    // The standard library's Unix remove_dir_all uses descriptor-relative traversal
    // and does not follow symlinks. Preflight the bound before removing any entry.
    fs::remove_dir_all(path)?;
    Ok(())
}

fn safe_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid(
            "ID must contain 1..64 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}
fn bounded(value: &str, limit: usize) -> Result<()> {
    if value.len() > limit || value.contains('\0') {
        return Err(Error::Limit(
            "continuity text exceeds its bound or contains NUL".into(),
        ));
    }
    Ok(())
}
fn valid_ref(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        || value.contains(['~', '^', ':', '?', '*', '[', '\\'])
        || value.contains("..")
        || value.contains("@{")
    {
        return Err(invalid("invalid Git reference"));
    }
    Ok(())
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| invalid("clock precedes Unix epoch"))
}
fn capture_error(e: change_capture::CaptureError) -> Error {
    invalid(&e.to_string())
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let exe = crate::repo_explorer::git::find_git()?;
    // Share source capture's sanitized configuration, disabled lazy fetch and
    // deadline-owned process group. Local inspection must never fetch objects.
    let git = crate::git_snapshot::Git {
        exe: &exe,
        repository: root,
        deadline: Some(Instant::now() + Duration::from_secs(15)),
    };
    let output = git
        .run(args, None, 64 * 1024)
        .map_err(|_| invalid("local Git inspection failed"))?;
    String::from_utf8(output)
        .map(|s| s.trim_end().to_owned())
        .map_err(|_| invalid("Git inspection returned non-UTF-8"))
}

fn origin(root: &Path) -> Result<(String, String)> {
    let output = git(root, &["remote", "get-url", "--all", "origin"])?;
    let mut identities = Vec::new();
    for line in output.lines() {
        let (host, path) = if line.contains("://") {
            let url = reqwest::Url::parse(line).map_err(|_| invalid("invalid forge origin"))?;
            if !matches!(url.scheme(), "https" | "ssh")
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid("unsupported forge origin"));
            }
            (
                url.host_str()
                    .ok_or_else(|| invalid("missing forge host"))?
                    .to_ascii_lowercase(),
                url.path().trim_matches('/').to_owned(),
            )
        } else {
            let (host, path) = line
                .split_once(':')
                .ok_or_else(|| invalid("unsupported forge origin"))?;
            if host.contains('/') {
                return Err(invalid("unsupported forge origin"));
            }
            (
                host.rsplit('@').next().unwrap_or("").to_ascii_lowercase(),
                path.trim_matches('/').to_owned(),
            )
        };
        identities.push((host, path.strip_suffix(".git").unwrap_or(&path).to_owned()));
    }
    identities.sort();
    identities.dedup();
    if identities.len() != 1 {
        return Err(invalid("ambiguous forge origin"));
    }
    Ok(identities.remove(0))
}

fn require_active(task: &Task) -> Result<()> {
    if task.deletion_pending.is_some() {
        return Err(Error::Conflict(
            "task deletion pending; retry delete".into(),
        ));
    }
    Ok(())
}

fn initialize_journal(root: &Path, directory: &File, checkout: &Path) -> Result<()> {
    let temporary = tempfile::Builder::new()
        .prefix(".continuity-init-")
        .tempfile_in(root)?;
    let db = Connection::open_with_flags(
        temporary.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON; BEGIN IMMEDIATE; CREATE TABLE tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE checks(task TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(task,id)); CREATE TABLE drafts(task TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(task,id)); CREATE TABLE binding(checkout TEXT NOT NULL); PRAGMA user_version=1; PRAGMA application_id=1095189332;")?;
    db.execute(
        "INSERT INTO binding(checkout) VALUES(?1)",
        [checkout.to_string_lossy().as_ref()],
    )?;
    db.execute_batch("COMMIT;")?;
    db.close().map_err(|(_, error)| Error::Sql(error))?;
    temporary.as_file().sync_all()?;
    // Same-directory no-clobber publication makes only complete, checkout-bound
    // databases visible at the final name. An interrupted temp file is harmless.
    publish_initial_journal(directory, temporary.path())?;
    drop(temporary);
    directory.sync_all()?;
    Ok(())
}

// Match the existing spool export's no-replace syscall boundary. Never fall back
// to link/unlink: interruption between those calls can leave an unexpected hardlink.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
))]
fn publish_initial_journal(directory: &File, temporary: &Path) -> Result<()> {
    let from = CString::new(
        temporary
            .file_name()
            .ok_or_else(|| invalid("missing initialization file name"))?
            .as_bytes(),
    )
    .map_err(|_| invalid("invalid initialization file name"))?;
    let to = c"journal.sqlite3";
    // SAFETY: both names and the retained parent descriptor remain live throughout
    // the call. These kernel flags atomically refuse any existing destination.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    // Use the raw syscall for musl release builds without a renameat2 wrapper.
    // SAFETY: the same descriptor/name lifetime contract applies to this syscall.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android"
)))]
fn publish_initial_journal(_directory: &File, _temporary: &Path) -> Result<()> {
    Err(invalid(
        "atomic continuity initialization is unsupported on this platform",
    ))
}
