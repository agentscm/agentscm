//! Explicit Git exchange over an existing owned export mirror.
//! Remote ref observations and publication receipts confer no native authority.
pub mod network;
pub use network::{Access, Endpoint, HttpsTrust, SshAccess};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::git::{Git, MAX_CLOSURE_OBJECTS, PushExit, validate_oid};
use super::{ExportState, LOCK, Mirror};
use crate::lease::Lease;
use crate::{Error, Result};

const JOURNAL: &str = "agentscm-remote.sqlite";
const APPLICATION: i64 = 0x4153_4754;
const MAX_OPERATIONS: i64 = 1000;
const MAX_RECORD: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub profile: String,
    pub remote: PathBuf,
    pub branch: String,
    device: u64,
    inode: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Endpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Fetch {
        operation_id: String,
        /// The exact commit observed by the operator, never a moving branch expression.
        expected_oid: String,
    },
    Publish {
        operation_id: String,
        event_sequence: u64,
        /// None requires an absent remote branch.
        expected_oid: Option<String>,
    },
    PublishCandidate {
        operation_id: String,
        candidate_id: String,
        /// None requires an absent remote branch.
        expected_oid: Option<String>,
    },
}

impl Request {
    fn id(&self) -> &str {
        match self {
            Self::Fetch { operation_id, .. }
            | Self::Publish { operation_id, .. }
            | Self::PublishCandidate { operation_id, .. } => operation_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Intent is durable. It may have performed a write; retry never dispatches it again.
    Pending,
    Fetched,
    Published,
    /// A command failed, a response was lost, or a later read could not confirm it.
    Unknown,
    /// Recovery saw the desired commit, without proving who wrote it.
    ObservedDesired,
    /// Recovery saw the expected preimage, without proving a write never happened.
    ObservedExpected,
    /// A network peer advertised no matching ref; this does not prove absence.
    ObservedUnadvertised,
    Diverged,
    /// The remote conclusively refused the push and a re-read shows the expected preimage.
    /// Terminal and without effect; it does not fence new operations.
    Rejected,
}

/// Closed classification of a conclusive refusal. Server text is never retained.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectionKind {
    HookDeclined,
    ProtectedBranch,
    StaleInfo,
    NonFastForward,
    FetchFirst,
    Other,
}

enum Dispatched {
    Done(Option<String>),
    /// `server` is true for `[remote rejected]`, false for a client-side `[rejected]`.
    Refused(RejectionKind, bool),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub request: Request,
    pub commit: String,
    pub state: State,
    /// Historical read made by this operation; absent can mean no branch.
    pub observed_oid: Option<String>,
    /// Only fetch operations have a retained local ref.
    pub local_ref: Option<String>,
    /// Present only with `State::Rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<RejectionKind>,
}

impl Mirror {
    /// Bind once to an existing, operator-trusted local bare SHA-1 repository.
    /// Paths must be absolute; URLs, network authentication and worktree remotes are unsupported.
    pub fn remote_bind(&self, remote: &Path, branch: &str) -> Result<Binding> {
        self.validate()?;
        let _lease = self.remote_lease()?;
        crate::git_snapshot::git_version(&self.git.executable())?;
        validate_branch(branch)?;
        server_command(&self.git.executable(), "receive-pack")?;
        if !remote.is_absolute() {
            return Err(invalid(
                "remote must be an absolute local bare repository path",
            ));
        }
        let remote = remote.canonicalize()?;
        if remote.starts_with(&self.owner.destination)
            || self.owner.destination.starts_with(&remote)
        {
            return Err(invalid(
                "remote and mirror must be separate, non-nested directories",
            ));
        }
        let git = Git::new(&self.git.executable(), remote.clone())?;
        validate_remote(&git)?;
        let metadata = fs::metadata(&remote)?;
        let binding = Binding {
            profile: "agentscm-local-git-remote/v1".into(),
            remote,
            branch: branch.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
            network: None,
        };
        self.write_remote_binding(binding)
    }

    /// Bind an explicit authenticated endpoint/trust profile, without contacting it.
    pub fn remote_bind_network(&self, endpoint: Endpoint, branch: &str) -> Result<Binding> {
        self.validate()?;
        let _lease = self.remote_lease()?;
        crate::git_snapshot::git_version(&self.git.executable())?;
        reject_command_policy()?;
        validate_branch(branch)?;
        endpoint.validate()?;
        self.write_remote_binding(Binding {
            profile: "agentscm-git-remote/v2".into(),
            remote: PathBuf::new(),
            branch: branch.into(),
            device: 0,
            inode: 0,
            network: Some(endpoint),
        })
    }

    fn write_remote_binding(&self, binding: Binding) -> Result<Binding> {
        let path = self.owner.destination.join(JOURNAL);
        if path.exists() {
            let (existing, _) = self.remote_journal()?;
            return if existing == binding {
                Ok(existing)
            } else {
                Err(invalid("mirror already has a different remote binding"))
            };
        }
        // Exclusive creation makes interrupted initialization visible, never silently adopted.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .sync_all()?;
        let db = Connection::open(&path)?;
        db.execute_batch(&format!(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
             PRAGMA application_id={APPLICATION}; PRAGMA user_version=1;
             CREATE TABLE binding(record BLOB NOT NULL) STRICT;
             CREATE TABLE operations(id TEXT PRIMARY KEY, record BLOB NOT NULL) STRICT;"
        ))?;
        db.execute(
            "INSERT INTO binding(record) VALUES (?1)",
            [serde_json::to_vec(&binding)?],
        )?;
        fs::File::open(&self.owner.destination)?.sync_all()?;
        Ok(binding)
    }

    /// A fresh read, separate from saved operation outcomes. No refs are changed.
    pub fn remote_observe(&self) -> Result<Option<String>> {
        self.remote_observe_with(&Access::default())
    }

    pub fn remote_observe_with(&self, access: &Access) -> Result<Option<String>> {
        self.validate()?;
        let _lease = self.remote_lease()?;
        let (binding, _) = self.remote_journal()?;
        self.validate_remote_access(&binding, access)?;
        self.observe_remote(&binding, access)
    }

    /// Returns a historical receipt. In particular, Pending and Unknown are not retried.
    pub fn remote_lookup(&self, operation_id: &str) -> Result<Option<Record>> {
        self.validate()?;
        validate_id(operation_id)?;
        let _lease = self.remote_lease()?;
        let (binding, db) = self.remote_journal()?;
        self.remote_record(&db, operation_id, &binding)
    }

    /// Reserve intent before any transport effects. A repeated id with equal input returns the
    /// saved receipt; changed input is rejected. Pending/Unknown operations fence new work.
    pub fn remote_run(&self, request: &Request) -> Result<Record> {
        self.remote_run_with(request, &Access::default())
    }

    pub fn remote_run_with(&self, request: &Request, access: &Access) -> Result<Record> {
        self.remote_run_guarded(request, access, || Ok(()))
    }

    pub fn remote_run_guarded(
        &self,
        request: &Request,
        access: &Access,
        mut qualify: impl FnMut() -> Result<()>,
    ) -> Result<Record> {
        self.validate()?;
        validate_request(request)?;
        let _lease = self.remote_lease()?;
        let (binding, db) = self.remote_journal()?;
        self.validate_remote_access(&binding, access)?;
        if let Some(existing) = self.remote_record(&db, request.id(), &binding)? {
            if existing.request != *request {
                return Err(invalid("remote operation id has different input"));
            }
            return Ok(existing);
        }
        let records: i64 = db.query_row("SELECT count(*) FROM operations", [], |r| r.get(0))?;
        if records >= MAX_OPERATIONS {
            return Err(Error::Limit(
                "remote journal is full at 1000 operations".into(),
            ));
        }
        let mut statement = db.prepare(
            "SELECT CASE WHEN typeof(id)='text' AND octet_length(id)<=64 THEN id END FROM operations"
        )?;
        for id in statement.query_map([], |r| r.get::<_, Option<String>>(0))? {
            let id =
                id?.ok_or_else(|| invalid("remote operation key exceeds its supported bounds"))?;
            validate_id(&id)?;
            if lookup(&db, &id, binding.network.is_some())?
                .is_some_and(|r| matches!(r.state, State::Pending | State::Unknown))
            {
                return Err(Error::Conflict(
                    "recover the pending or unknown remote operation first".into(),
                ));
            }
        }
        let (commit, local_ref) = match request {
            Request::Fetch { expected_oid, .. } => (
                expected_oid.clone(),
                Some(format!("refs/agentscm-fetch/{}", request.id())),
            ),
            Request::Publish { event_sequence, .. } => {
                let status = self
                    .status(*event_sequence)?
                    .ok_or_else(|| invalid("export record is missing"))?;
                if !matches!(
                    status.record.state,
                    ExportState::Verified | ExportState::Mirrored
                ) || status.object_error.is_some()
                {
                    return Err(invalid("publication requires an intact verified export"));
                }
                (
                    status
                        .record
                        .commit
                        .ok_or_else(|| invalid("export commit is missing"))?,
                    None,
                )
            }
            Request::PublishCandidate { candidate_id, .. } => {
                let record = self
                    .candidate_record(candidate_id)?
                    .ok_or_else(|| invalid("candidate export record is missing"))?;
                if record.state != ExportState::Mirrored || self.owned_oid()? != record.commit {
                    return Err(invalid(
                        "publication requires an intact verified candidate export",
                    ));
                }
                (
                    record
                        .commit
                        .ok_or_else(|| invalid("candidate export commit is missing"))?,
                    None,
                )
            }
        };
        let mut record = Record {
            request: request.clone(),
            commit,
            state: State::Pending,
            observed_oid: None,
            local_ref,
            rejection: None,
        };
        db.execute(
            "INSERT INTO operations(id,record) VALUES (?1,?2)",
            params![request.id(), serde_json::to_vec(&record)?],
        )?;
        qualify()?;
        // Preserve the durable authentication/no-replay boundary even when
        // default discovery cannot complete. A proven unsupported profile is
        // reported explicitly, with the same retained uncertain operation.
        if matches!(request, Request::PublishCandidate { .. })
            && let Err(error) = self.check_candidate_output(&binding, access)
        {
            record.state = State::Unknown;
            save(&db, &record)?;
            return match error {
                Error::Invalid(_) => Err(error),
                _ => Ok(record),
            };
        }
        // Failures after reservation retain Unknown, including a lost push response.
        // Only a parsed, conclusive refusal can become Rejected.
        match self.dispatch_remote(&binding, &record, access, &mut qualify) {
            Ok(Dispatched::Done(observed)) => {
                record.observed_oid = observed;
                record.state = match request {
                    Request::Fetch { .. } => State::Fetched,
                    Request::Publish { .. } | Request::PublishCandidate { .. } => State::Published,
                };
            }
            Ok(Dispatched::Refused(kind, server)) => {
                self.classify_refusal(&mut record, &binding, access, kind, server)
            }
            Err(_) => record.state = State::Unknown,
        }
        save(&db, &record)?;
        Ok(record)
    }

    // The refusal came from the server's report; the re-read must agree with it.
    // A network create-only push has no preimage to compare beyond "not advertised",
    // which is accepted because every later push still uses the exact lease. That
    // reliance holds only for a server-side `[remote rejected]`: a client-side
    // `[rejected]` (stale info, fetch first, non-fast-forward) implies the ref was
    // advertised, so an unadvertised re-read is never recorded as an absence claim.
    fn classify_refusal(
        &self,
        record: &mut Record,
        binding: &Binding,
        access: &Access,
        kind: RejectionKind,
        server: bool,
    ) {
        let Ok(current) = self.observe_remote(binding, access) else {
            record.state = State::Unknown;
            return;
        };
        let expected = match &record.request {
            Request::Fetch { expected_oid, .. } => Some(expected_oid),
            Request::Publish { expected_oid, .. }
            | Request::PublishCandidate { expected_oid, .. } => expected_oid.as_ref(),
        };
        let hidden_client_refusal = current.is_none() && binding.network.is_some() && !server;
        record.state = if current.as_ref() == expected && !hidden_client_refusal {
            record.rejection = Some(kind);
            State::Rejected
        } else if current.as_deref() == Some(record.commit.as_str()) {
            State::ObservedDesired
        } else if current.is_none() && binding.network.is_some() {
            State::ObservedUnadvertised
        } else {
            State::Diverged
        };
        record.observed_oid = current;
    }

    /// Explicit, read-only resolution of uncertain publication. Never repeats a push.
    /// A recovered fetch may be run again only under a new operation id.
    pub fn remote_recover(&self, operation_id: &str) -> Result<Record> {
        self.remote_recover_with(operation_id, &Access::default())
    }

    pub fn remote_recover_with(&self, operation_id: &str, access: &Access) -> Result<Record> {
        self.validate()?;
        validate_id(operation_id)?;
        let _lease = self.remote_lease()?;
        let (binding, db) = self.remote_journal()?;
        let mut record = self
            .remote_record(&db, operation_id, &binding)?
            .ok_or_else(|| invalid("remote operation is missing"))?;
        if !matches!(record.state, State::Pending | State::Unknown) {
            return Ok(record);
        }
        // If observation fails, leave the existing uncertain state intact.
        self.validate_remote_access(&binding, access)?;
        let current = self.observe_remote(&binding, access)?;
        record.state = if current.is_none() && binding.network.is_some() {
            State::ObservedUnadvertised
        } else if current.as_deref() == Some(record.commit.as_str()) {
            State::ObservedDesired
        } else {
            let expected = match &record.request {
                Request::Fetch { expected_oid, .. } => Some(expected_oid),
                Request::Publish { expected_oid, .. }
                | Request::PublishCandidate { expected_oid, .. } => expected_oid.as_ref(),
            };
            if current.as_ref() == expected {
                State::ObservedExpected
            } else {
                State::Diverged
            }
        };
        record.observed_oid = current;
        save(&db, &record)?;
        Ok(record)
    }

    fn remote_record(
        &self,
        db: &Connection,
        id: &str,
        binding: &Binding,
    ) -> Result<Option<Record>> {
        let Some(record) = lookup(db, id, binding.network.is_some())? else {
            return Ok(None);
        };
        if let Request::Publish { event_sequence, .. } = &record.request {
            let export = self
                .record(*event_sequence)?
                .ok_or_else(|| invalid("remote publication export mapping is missing"))?;
            if export.commit.as_ref() != Some(&record.commit) {
                return Err(invalid(
                    "remote publication differs from its export mapping",
                ));
            }
        }
        if let Request::PublishCandidate { candidate_id, .. } = &record.request {
            let candidate = self
                .candidate_record(candidate_id)?
                .ok_or_else(|| invalid("remote publication candidate mapping is missing"))?;
            if candidate.commit.as_ref() != Some(&record.commit) {
                return Err(invalid(
                    "remote publication differs from its candidate mapping",
                ));
            }
        }
        Ok(Some(record))
    }

    fn remote_lease(&self) -> Result<Lease> {
        Lease::try_exclusive(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.owner.destination.join(LOCK))?,
        )
        .map_err(|_| Error::Conflict("mirror export or remote operation is already running".into()))
    }

    fn remote_journal(&self) -> Result<(Binding, Connection)> {
        crate::git_snapshot::git_version(&self.git.executable())?;
        let path = self.owner.destination.join(JOURNAL);
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err(invalid("remote journal must be a regular file"));
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let application: i64 = db.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if application != APPLICATION || version != 1 {
            return Err(invalid("unsupported remote journal"));
        }
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;")?;
        let bytes: Vec<u8> = db.query_row(
            "SELECT CASE WHEN length(record)<=16384 THEN record END FROM binding LIMIT 1",
            [],
            |r| r.get(0),
        )?;
        if bytes.len() > MAX_RECORD {
            return Err(invalid("oversized remote binding"));
        }
        let binding: Binding = serde_json::from_slice(&bytes)?;
        validate_branch(&binding.branch)?;
        match &binding.network {
            None if binding.profile == "agentscm-local-git-remote/v1"
                && binding.remote.is_absolute() => {}
            Some(endpoint)
                if binding.profile == "agentscm-git-remote/v2"
                    && binding.remote.as_os_str().is_empty()
                    && binding.device == 0
                    && binding.inode == 0 =>
            {
                endpoint.validate()?
            }
            _ => return Err(invalid("invalid remote binding")),
        }
        Ok((binding, db))
    }

    fn validate_remote_access(&self, binding: &Binding, access: &Access) -> Result<()> {
        if let Some(endpoint) = &binding.network {
            endpoint.validate_access(access)
        } else {
            server_command(&self.git.executable(), "receive-pack").map(|_| ())
        }
    }

    fn observe_remote(&self, binding: &Binding, access: &Access) -> Result<Option<String>> {
        if let Some(endpoint) = &binding.network {
            let reference = format!("refs/heads/{}", binding.branch);
            let bytes = endpoint.run(
                &self.git,
                access,
                &["ls-remote", "--refs", "--", endpoint.url(), &reference],
                8192,
            )?;
            return advertised_ref(&bytes, &reference);
        }
        let metadata = fs::metadata(&binding.remote)?;
        if metadata.dev() != binding.device
            || metadata.ino() != binding.inode
            || binding.remote.canonicalize()? != binding.remote
        {
            return Err(Error::Conflict("bound local remote was replaced".into()));
        }
        let remote_git = Git::new(&self.git.executable(), binding.remote.clone())?;
        validate_remote(&remote_git)?;
        let reference = format!("refs/heads/{}", binding.branch);
        let direct = remote_git.current(&reference)?;
        let upload = server_command(&self.git.executable(), "upload-pack")?;
        let bytes = self.git.run(
            &[
                "ls-remote",
                "--refs",
                &format!("--upload-pack={upload}"),
                "--",
                path_text(&binding.remote)?,
                &reference,
            ],
            None,
            8192,
        )?;
        let advertised = advertised_ref(&bytes, &reference)?;
        if advertised != direct || remote_git.current(&reference)? != direct {
            return Err(Error::Conflict(
                "direct and advertised remote refs disagree".into(),
            ));
        }
        Ok(advertised)
    }

    fn candidate_default_branch(&self, binding: &Binding, access: &Access) -> Result<String> {
        let bytes = if let Some(endpoint) = &binding.network {
            endpoint.run(
                &self.git,
                access,
                &["ls-remote", "--symref", "--", endpoint.url(), "HEAD"],
                8192,
            )?
        } else {
            let upload = server_command(&self.git.executable(), "upload-pack")?;
            self.git.run(
                &[
                    "ls-remote",
                    "--symref",
                    &format!("--upload-pack={upload}"),
                    "--",
                    path_text(&binding.remote)?,
                    "HEAD",
                ],
                None,
                8192,
            )?
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("unreadable remote HEAD"))?;
        let defaults: Vec<_> = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("ref: refs/heads/")
                    .and_then(|s| s.strip_suffix("\tHEAD"))
            })
            .collect();
        if defaults.len() != 1 || !crate::hub::github::valid_ref_name(defaults[0]) {
            return Err(invalid(
                "unsupported candidate profile: remote default branch is unknown",
            ));
        }
        Ok(defaults[0].to_owned())
    }

    fn check_candidate_output(&self, binding: &Binding, access: &Access) -> Result<()> {
        let default = self.candidate_default_branch(binding, access)?;
        if binding.branch == default
            || binding.branch == "HEAD"
            || binding.branch.starts_with("refs/")
        {
            return Err(invalid(
                "candidate output must not name the remote default branch or a ref alias",
            ));
        }
        Ok(())
    }

    fn check_candidate_target(
        &self,
        binding: &Binding,
        record: &Record,
        access: &Access,
    ) -> Result<()> {
        if let Request::PublishCandidate { candidate_id, .. } = &record.request {
            let candidate = self
                .candidate_record(candidate_id)?
                .ok_or_else(|| invalid("candidate missing"))?;
            let subject = &candidate.request.subject;
            let endpoint = match &binding.network {
                Some(endpoint) => endpoint.url(),
                None => path_text(&binding.remote)?,
            };
            if subject.endpoint != endpoint
                || subject.output != binding.branch
                || subject.target == binding.branch
            {
                return Err(invalid(
                    "candidate differs from attached remote/output identity",
                ));
            }
            self.check_candidate_output(binding, access)?;
            let mut target = binding.clone();
            target.branch = subject.target.clone();
            if self.observe_remote(&target, access)?.as_deref() != Some(&subject.baseline_commit) {
                return Err(Error::Conflict(
                    "attached remote target changed or is unadvertised; refresh and requalify"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    fn dispatch_remote(
        &self,
        binding: &Binding,
        record: &Record,
        access: &Access,
        qualify: &mut impl FnMut() -> Result<()>,
    ) -> Result<Dispatched> {
        let current = self.observe_remote(binding, access)?;
        match &record.request {
            Request::Fetch { expected_oid, .. } => {
                if current.as_ref() != Some(expected_oid) {
                    return Err(Error::Conflict("remote branch changed before fetch".into()));
                }
                self.fetch_remote(binding, expected_oid, access)?;
                self.git
                    .verify_closure(expected_oid, None, MAX_CLOSURE_OBJECTS)?;
                let local_ref = record
                    .local_ref
                    .as_deref()
                    .ok_or_else(|| invalid("fetch ref is missing"))?;
                self.git.run(
                    &[
                        "update-ref",
                        "--no-deref",
                        local_ref,
                        expected_oid,
                        &"0".repeat(40),
                    ],
                    None,
                    8192,
                )?;
                Ok(Dispatched::Done(current))
            }
            Request::Publish { expected_oid, .. }
            | Request::PublishCandidate { expected_oid, .. } => {
                if current != *expected_oid {
                    return Err(Error::Conflict(
                        "remote branch differs from expected ref".into(),
                    ));
                }
                if let Some(expected) = expected_oid {
                    self.fetch_remote(binding, expected, access)?;
                    self.git.run(
                        &["merge-base", "--is-ancestor", expected, &record.commit],
                        None,
                        8192,
                    )?;
                }
                self.check_candidate_target(binding, record, access)?;
                qualify()?;
                if matches!(record.request, Request::PublishCandidate { .. })
                    && current.as_deref() == Some(&record.commit)
                {
                    return Ok(Dispatched::Done(current));
                }
                // The exact lease is a compare-and-swap; the ancestry check above
                // refuses a rewind. Remote hooks and receive policy remain in force.
                let lease = format!(
                    "--force-with-lease=refs/heads/{}:{}",
                    binding.branch,
                    expected_oid.as_deref().unwrap_or("")
                );
                let refspec = format!("{}:refs/heads/{}", record.commit, binding.branch);
                let reference = format!("refs/heads/{}", binding.branch);
                let exit = if let Some(endpoint) = &binding.network {
                    endpoint.run_exit(
                        &self.git,
                        access,
                        &[
                            "push",
                            "--porcelain",
                            &lease,
                            "--",
                            endpoint.url(),
                            &refspec,
                        ],
                        16384,
                    )?
                } else {
                    let receive = server_command(&self.git.executable(), "receive-pack")?;
                    self.git.run_push(
                        &[
                            "push",
                            "--porcelain",
                            &format!("--receive-pack={receive}"),
                            &lease,
                            "--",
                            path_text(&binding.remote)?,
                            &refspec,
                        ],
                        16384,
                    )?
                };
                if let Some((kind, server)) = parse_porcelain(&exit, &reference) {
                    return Ok(Dispatched::Refused(kind, server));
                }
                if !exit.finished || exit.code != Some(0) {
                    return Err(Error::Conflict("push did not complete".into()));
                }
                self.check_candidate_target(binding, record, access)?;
                qualify()?;
                let observed = self.observe_remote(binding, access)?;
                if observed.as_deref() != Some(record.commit.as_str()) {
                    return Err(Error::Conflict(
                        "remote ref could not be confirmed after push".into(),
                    ));
                }
                Ok(Dispatched::Done(observed))
            }
        }
    }

    fn fetch_remote(&self, binding: &Binding, oid: &str, access: &Access) -> Result<()> {
        if let Some(endpoint) = &binding.network {
            endpoint.run(
                &self.git,
                access,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--no-recurse-submodules",
                    "--",
                    endpoint.url(),
                    oid,
                ],
                16384,
            )?;
            return Ok(());
        }
        let upload = server_command(&self.git.executable(), "upload-pack")?;
        self.git.run(
            &[
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                &format!("--upload-pack={upload}"),
                "--",
                path_text(&binding.remote)?,
                oid,
            ],
            None,
            16384,
        )?;
        Ok(())
    }
}

/// Classify `git push --porcelain` output as a conclusive refusal of exactly `reference`.
/// Anything unfinished, unparseable or ambiguous is not a refusal. The exit code is not trusted.
/// The flag is true for a server-side `[remote rejected]`, false for a client-side `[rejected]`.
fn parse_porcelain(exit: &PushExit, reference: &str) -> Option<(RejectionKind, bool)> {
    if !exit.finished {
        return None;
    }
    let text = std::str::from_utf8(&exit.stdout).ok()?;
    if text.contains('\r') {
        return None;
    }
    let mut refusal = None;
    for line in text.lines() {
        if line.is_empty() || line == "Done" || line.starts_with("To ") {
            continue;
        }
        if refusal.is_some() {
            return None;
        }
        let mut fields = line.splitn(3, '\t');
        let (flag, refs, summary) = (fields.next()?, fields.next()?, fields.next()?);
        let (_, to) = refs.split_once(':')?;
        if flag != "!" || to != reference {
            return None;
        }
        let (server, reason) = match summary.strip_prefix("[remote rejected]") {
            Some(rest) => (true, rest),
            None => (false, summary.strip_prefix("[rejected]")?),
        };
        let reason = reason.to_ascii_lowercase();
        let kind = if reason.contains("protected branch") {
            RejectionKind::ProtectedBranch
        } else if reason.contains("hook declined") {
            RejectionKind::HookDeclined
        } else if reason.contains("stale info") {
            RejectionKind::StaleInfo
        } else if reason.contains("non-fast-forward") {
            RejectionKind::NonFastForward
        } else if reason.contains("fetch first") {
            RejectionKind::FetchFirst
        } else {
            RejectionKind::Other
        };
        refusal = Some((kind, server));
    }
    refusal
}

fn advertised_ref(bytes: &[u8], reference: &str) -> Result<Option<String>> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("invalid remote ref response"))?;
    if text.is_empty() {
        return Ok(None);
    }
    let (oid, name) = text
        .trim_end_matches('\n')
        .split_once('\t')
        .ok_or_else(|| invalid("invalid remote ref response"))?;
    validate_oid(oid)?;
    if name != reference {
        return Err(invalid("ambiguous remote ref response"));
    }
    Ok(Some(oid.into()))
}

fn validate_remote(git: &Git) -> Result<()> {
    if git.text(&["rev-parse", "--is-bare-repository"])? != "true" || git.object_format()? != "sha1"
    {
        return Err(invalid("remote must be a local bare SHA-1 repository"));
    }
    git.reject_incomplete_history_metadata()
}

fn lookup(db: &Connection, id: &str, network: bool) -> Result<Option<Record>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT CASE WHEN length(record)<=16384 THEN record END FROM operations WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(bytes) = bytes else {
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
            [id],
            |r| r.get(0),
        )?;
        return if present {
            Err(invalid("oversized remote operation"))
        } else {
            Ok(None)
        };
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    validate_request(&record.request)?;
    validate_oid(&record.commit)?;
    if let Some(observed) = &record.observed_oid {
        validate_oid(observed)?;
    }
    let expected_ref = match &record.request {
        Request::Fetch { expected_oid, .. } => {
            if expected_oid != &record.commit {
                return Err(invalid("fetch commit differs from its request"));
            }
            Some(format!("refs/agentscm-fetch/{id}"))
        }
        Request::Publish { .. } | Request::PublishCandidate { .. } => None,
    };
    if record.local_ref != expected_ref {
        return Err(invalid("remote operation ref does not match request"));
    }
    let expected = match &record.request {
        Request::Fetch { expected_oid, .. } => Some(expected_oid),
        Request::Publish { expected_oid, .. } | Request::PublishCandidate { expected_oid, .. } => {
            expected_oid.as_ref()
        }
    };
    let desired_observed = record.observed_oid.as_ref() == Some(&record.commit);
    let valid_state = match record.state {
        State::Pending | State::Unknown => record.observed_oid.is_none(),
        State::Published => {
            matches!(
                record.request,
                Request::Publish { .. } | Request::PublishCandidate { .. }
            ) && desired_observed
        }
        State::Fetched => matches!(record.request, Request::Fetch { .. }) && desired_observed,
        State::ObservedDesired => desired_observed,
        State::ObservedExpected => {
            !desired_observed
                && record.observed_oid.as_ref() == expected
                && (!network || record.observed_oid.is_some())
        }
        State::ObservedUnadvertised => network && record.observed_oid.is_none(),
        State::Diverged => {
            !desired_observed
                && record.observed_oid.as_ref() != expected
                && (!network || record.observed_oid.is_some())
        }
        State::Rejected => {
            matches!(
                record.request,
                Request::Publish { .. } | Request::PublishCandidate { .. }
            ) && record.rejection.is_some()
                && !desired_observed
                && record.observed_oid.as_ref() == expected
        }
    } && (record.state == State::Rejected) == record.rejection.is_some();
    if !valid_state {
        return Err(invalid("inconsistent remote operation state"));
    }
    if record.request.id() != id {
        return Err(invalid("remote operation key mismatch"));
    }
    Ok(Some(record))
}

fn save(db: &Connection, record: &Record) -> Result<()> {
    db.execute(
        "UPDATE operations SET record=?1 WHERE id=?2",
        params![serde_json::to_vec(record)?, record.request.id()],
    )?;
    Ok(())
}

fn validate_request(request: &Request) -> Result<()> {
    validate_id(request.id())?;
    match request {
        Request::Fetch { expected_oid, .. } => validate_oid(expected_oid),
        Request::Publish {
            event_sequence,
            expected_oid,
            ..
        } => {
            if *event_sequence == 0 {
                return Err(invalid("export event sequence must be positive"));
            }
            expected_oid.as_deref().map(validate_oid).transpose()?;
            Ok(())
        }
        Request::PublishCandidate {
            candidate_id,
            expected_oid,
            ..
        } => {
            crate::store::validate_id(candidate_id)?;
            expected_oid.as_deref().map(validate_oid).transpose()?;
            Ok(())
        }
    }
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("invalid remote operation id"));
    }
    Ok(())
}

fn validate_branch(branch: &str) -> Result<()> {
    if !crate::hub::github::valid_ref_name(branch)
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"))
        || !branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(invalid("invalid remote branch"));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("Git transport requires UTF-8 paths"))
}

// Git propagates client -c options in GIT_CONFIG_PARAMETERS. Start without
// those or numbered overrides, then restore the operator's policy lookup and
// executable search environment. Unset policy sources retain Git's defaults.
// The remote is operator-trusted: its hooks/config can execute local programs.
fn reject_command_policy() -> Result<()> {
    // Original per-command policy cannot be distinguished from child-added -c
    // overrides once Git has propagated it. Refuse it before effects.
    if std::env::vars_os().any(|(key, _)| {
        let key = key.to_string_lossy();
        matches!(
            key.as_ref(),
            "GIT_CONFIG" | "GIT_CONFIG_PARAMETERS" | "GIT_CONFIG_COUNT"
        ) || key.starts_with("GIT_CONFIG_KEY_")
            || key.starts_with("GIT_CONFIG_VALUE_")
    }) {
        return Err(invalid(
            "remote exchange does not support per-command Git policy overrides",
        ));
    }
    Ok(())
}

fn server_command(executable: &Path, service: &str) -> Result<String> {
    reject_command_policy()?;
    let mut command = String::from("/usr/bin/env -i LC_ALL=C");
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "PATH",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
    ] {
        let Some(value) = std::env::var_os(key) else {
            continue;
        };
        let value = value
            .to_str()
            .ok_or_else(|| invalid("remote policy environment must be UTF-8"))?;
        let value = match key {
            "PATH" => {
                let paths = std::env::split_paths(value)
                    .map(|path| absolute_policy_path(&path))
                    .collect::<Result<Vec<_>>>()?;
                std::env::join_paths(paths)
                    .map_err(|_| invalid("unsupported remote executable search path"))?
                    .into_string()
                    .map_err(|_| invalid("remote policy environment must be UTF-8"))?
            }
            "HOME" | "XDG_CONFIG_HOME" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_GLOBAL"
                if !value.is_empty() =>
            {
                path_text(&absolute_policy_path(Path::new(value))?)?.to_owned()
            }
            _ => value.to_owned(),
        };
        command.push(' ');
        command.push_str(&shell_quote(&format!("{key}={value}")));
    }
    command.push(' ');
    command.push_str(&shell_quote(path_text(executable)?));
    command.push(' ');
    command.push_str(service);
    Ok(command)
}

fn absolute_policy_path(path: &Path) -> Result<PathBuf> {
    // Anchor paths to the controller's cwd before Git changes directories.
    // Do not resolve symlinks: that can change relative config include paths.
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: &str = "refs/heads/work";
    const OID: &str = "0123456789012345678901234567890123456789";

    fn parse(stdout: &[u8], finished: bool) -> Option<(RejectionKind, bool)> {
        let exit = PushExit {
            finished,
            code: Some(1),
            stdout: stdout.to_vec(),
        };
        parse_porcelain(&exit, REF)
    }

    fn report(summary: &str) -> Vec<u8> {
        format!("To /srv/synthetic\n!\t{OID}:{REF}\t{summary}\nDone\n").into_bytes()
    }

    #[test]
    fn classifies_conclusive_refusals() {
        for (summary, kind, server) in [
            (
                "[remote rejected] (pre-receive hook declined)",
                RejectionKind::HookDeclined,
                true,
            ),
            (
                "[remote rejected] (hook declined)",
                RejectionKind::HookDeclined,
                true,
            ),
            (
                "[remote rejected] (protected branch hook declined)",
                RejectionKind::ProtectedBranch,
                true,
            ),
            ("[rejected] (stale info)", RejectionKind::StaleInfo, false),
            (
                "[rejected] (non-fast-forward)",
                RejectionKind::NonFastForward,
                false,
            ),
            ("[rejected] (fetch first)", RejectionKind::FetchFirst, false),
            (
                "[remote rejected] (something new)",
                RejectionKind::Other,
                true,
            ),
        ] {
            assert_eq!(
                parse(&report(summary), true),
                Some((kind, server)),
                "{summary}"
            );
        }
    }

    #[test]
    fn everything_ambiguous_is_not_a_refusal() {
        let ok = report("[remote rejected] (pre-receive hook declined)");
        let text = String::from_utf8(ok.clone()).unwrap();
        assert!(parse(&ok, true).is_some());
        assert_eq!(parse(&ok, false), None, "stopped");
        assert_eq!(
            parse(&report("[remote failure] (unpack failed)"), true),
            None
        );
        assert_eq!(parse(&report("[up to date]"), true), None);
        assert_eq!(
            parse(b"To /srv/synthetic\nDone\n", true),
            None,
            "missing line"
        );
        assert_eq!(parse(b"", true), None, "empty");
        let two = text.replace(
            "Done\n",
            &format!("!\t{OID}:{REF}\t[rejected] (fetch first)\nDone\n"),
        );
        assert_eq!(parse(two.as_bytes(), true), None, "two lines");
        let other = text.replace(REF, "refs/heads/other");
        assert_eq!(parse(other.as_bytes(), true), None, "other ref");
        assert_eq!(
            parse(text.replace('\n', "\r\n").as_bytes(), true),
            None,
            "crlf"
        );
        let mut bytes = ok.clone();
        bytes.extend_from_slice(&[0xff, b'\n']);
        assert_eq!(parse(&bytes, true), None, "non-utf8");
        let flag = text.replace("!\t", "=\t");
        assert_eq!(parse(flag.as_bytes(), true), None, "wrong flag");
    }
}
