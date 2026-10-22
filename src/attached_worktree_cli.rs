use std::path::{Path, PathBuf};

use agentscm::attached_worktree::{AttachRequest, AttachmentJournal, SourceMode};
use agentscm::authority::Authority;
use agentscm::git_export::remote::{Request as RemoteRequest, State as RemoteState};
use agentscm::git_export::{CandidateExportRequest, CandidateSubject, CommitMetadata, Mirror};
use agentscm::store::Engine;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum AttachedWorktreeCommand {
    /// Bind an actor to this worktree and capture its target baseline and selected source.
    Attach {
        #[arg(long, default_value = "/usr/bin/git")]
        git: PathBuf,
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        actor: String,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        output: String,
        #[arg(long, value_enum, default_value = "committed")]
        source: CliSourceMode,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long)]
        operation_id: String,
    },
    /// Inspect retained attachment identity and exact Git/native mappings.
    State {
        #[arg(long)]
        id: String,
    },
    /// Capture the frozen configured source without modifying Git state.
    Capture {
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        id: String,
        #[arg(long)]
        operation_id: String,
    },
    /// Import an advanced target and merge it into the native candidate if disjoint.
    Refresh {
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        id: String,
        #[arg(long)]
        operation_id: String,
    },
    /// Bind the attachment to an existing exact Authority Change/context.
    BindContext {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        change: String,
        #[arg(long)]
        operation_id: String,
    },
    /// Recheck the current Git source, Authority pointer, policy and configured evidence.
    Compare {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
    },
    /// Build an exact candidate commit in a separately owned bare mirror.
    Prepare {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        mirror: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: String,
        #[arg(long)]
        timestamp: u64,
        #[arg(long)]
        message: String,
    },
    /// Abandon a prepare reservation only if candidate construction never began.
    AbandonPrepare {
        #[arg(long)]
        id: String,
        #[arg(long)]
        mirror: PathBuf,
        #[arg(long)]
        operation_id: String,
    },
    /// Publish a prepared candidate to an ordinary non-target branch with an exact lease.
    PublishBranch {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        mirror: PathBuf,
        #[command(flatten)]
        access: TransportArgs,
        #[arg(long)]
        candidate: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        expected_oid: Option<String>,
    },
    /// Create or find the same pull or merge request for the exactly published branch.
    PublishPr {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
        #[arg(long)]
        mirror: PathBuf,
        #[command(flatten)]
        access: TransportArgs,
        #[arg(long)]
        candidate: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        previous_publication: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Observe an uncertain Hub PR operation without creating a PR.
    RecoverPr {
        #[arg(long)]
        id: String,
        #[arg(long)]
        publication: String,
    },
    /// Observe an uncertain branch push without retrying it.
    RecoverBranch {
        #[arg(long)]
        id: String,
        #[arg(long)]
        mirror: PathBuf,
        #[arg(long)]
        operation_id: String,
        #[command(flatten)]
        access: TransportArgs,
    },
    /// Preview the bounded capture selector without consuming an operation ID.
    Preview {
        #[arg(long)]
        id: String,
        #[arg(long)]
        expected_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum CliSourceMode {
    Committed,
    Index,
    /// Raw selected filesystem bytes, without Git text/EOL/encoding conversion.
    Worktree,
}
impl From<CliSourceMode> for SourceMode {
    fn from(value: CliSourceMode) -> Self {
        match value {
            CliSourceMode::Committed => Self::Committed,
            CliSourceMode::Index => Self::Index,
            CliSourceMode::Worktree => Self::Worktree,
        }
    }
}

pub async fn run(data_dir: &Path, command: AttachedWorktreeCommand) -> Result<Value> {
    let data = data_dir.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "attached-worktree commands require an initialized repository data directory".into(),
        ));
    }
    if let AttachedWorktreeCommand::Attach {
        repository: source, ..
    } = &command
    {
        let source = source.canonicalize()?;
        if source.starts_with(&data) || data.starts_with(&source) {
            return Err(Error::Invalid(
                "source and native data directories must be separate".into(),
            ));
        }
    }
    let repository = Authority::open(&data)?.repository_id()?;
    let engine = Engine::open(&data)?;
    match command {
        AttachedWorktreeCommand::Attach {
            git,
            repository: worktree,
            actor,
            remote,
            target,
            output,
            source,
            paths,
            operation_id,
        } => {
            let journal = AttachmentJournal::open(&data, &repository)?;
            let state = journal.attach(
                &engine,
                &operation_id,
                &AttachRequest {
                    git,
                    worktree,
                    actor,
                    remote,
                    target,
                    output,
                    source: source.into(),
                    paths,
                },
            )?;
            Ok(json!({"ok":true,"attachment":state}))
        }
        AttachedWorktreeCommand::State { id } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"attachment":journal.state(&id)?,"mappings":journal.mappings(&id,None,256)?}),
            )
        }
        AttachedWorktreeCommand::Capture {
            id,
            operation_id,
            expected_generation,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"capture":journal.capture(&engine,&operation_id,&id,expected_generation)?}),
            )
        }
        AttachedWorktreeCommand::Refresh {
            id,
            operation_id,
            expected_generation,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            Ok(
                json!({"ok":true,"attachment":journal.refresh(&engine,&operation_id,&id,expected_generation)?}),
            )
        }
        AttachedWorktreeCommand::BindContext {
            id,
            expected_generation,
            change,
            operation_id,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            let authority = Authority::open(&data)?;
            let change = agentscm::context::ChangeId {
                repository,
                id: change,
            };
            Ok(
                json!({"ok":true,"attachment":journal.bind_context(&authority,&operation_id,&id,expected_generation,&change)?}),
            )
        }
        AttachedWorktreeCommand::Preview {
            id,
            expected_generation,
        } => {
            let journal = AttachmentJournal::open_existing(&data, &repository)?;
            let selected = journal.preview(&engine, &id, expected_generation)?;
            Ok(
                json!({"ok":true,"preview":{"revision":selected.revision,"changed_paths":selected.changed_paths,"excluded_total":selected.excluded_total,"fingerprint":selected.fingerprint},"historical":true}),
            )
        }
        AttachedWorktreeCommand::Compare {
            id,
            expected_generation,
        } => {
            let subject = qualify(&data, &engine, &repository, &id, expected_generation)?;
            Ok(
                json!({"ok":true,"historical":true,"subject":subject,"qualification":"current_at_comparison"}),
            )
        }
        AttachedWorktreeCommand::AbandonPrepare {
            id,
            mirror,
            operation_id,
        } => {
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            let owned = Mirror::open(Path::new(&state.git), &mirror, &repository)?;
            owned.abandon_candidate_operation(&operation_id)?;
            Ok(
                json!({"ok":true,"operation_id":operation_id,"outcome":"abandoned_no_candidate_effect"}),
            )
        }
        AttachedWorktreeCommand::Prepare {
            id,
            expected_generation,
            operation_id,
            mirror,
            name,
            email,
            timestamp,
            message,
        } => {
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            if !mirror.exists() {
                qualify(&data, &engine, &repository, &id, expected_generation)?;
            }
            let mut owned = if mirror.exists() {
                Mirror::open(Path::new(&state.git), &mirror, &repository)?
            } else {
                Mirror::initialize_with_seed(
                    Path::new(&state.git),
                    &mirror,
                    &repository,
                    &state.target,
                    Path::new(&state.worktree),
                    &state.target_commit,
                )?
            };
            let metadata = CommitMetadata {
                name,
                email,
                timestamp,
                message,
            };
            let input =
                json!({"attachment":id,"generation":expected_generation,"metadata":metadata});
            if let Some(request) = owned.candidate_operation(&operation_id, &input)? {
                if let Some(record) = owned.candidate_record(&request.subject.id)?
                    && record.state == agentscm::git_export::ExportState::Mirrored
                {
                    return Ok(
                        json!({"ok":false,"historical":true,"candidate":record,"qualification":"not_checked"}),
                    );
                }
                require_current(
                    &data,
                    &engine,
                    &repository,
                    &id,
                    expected_generation,
                    &request.subject,
                )?;
                owned.import_candidate_baseline(
                    Path::new(&state.worktree),
                    &request.subject.baseline_commit,
                )?;
                let record = owned.export_candidate_guarded(&engine, &request, || {
                    require_current(
                        &data,
                        &engine,
                        &repository,
                        &id,
                        expected_generation,
                        &request.subject,
                    )
                })?;
                let current = require_current(
                    &data,
                    &engine,
                    &repository,
                    &id,
                    expected_generation,
                    &request.subject,
                );
                return Ok(
                    json!({"ok":current.is_ok(),"historical":true,"candidate":record,"qualification":qualification(current)}),
                );
            }
            let mut subject = qualify(&data, &engine, &repository, &id, expected_generation)?;
            if let Some(record) = owned.candidate_for_subject(&subject, &metadata)? {
                return Ok(
                    json!({"ok":false,"candidate":record,"historical":true,"qualification":"not_checked"}),
                );
            }
            subject.parent_commit = owned
                .owned_oid()?
                .ok_or_else(|| Error::Conflict("owned mirror has no parent".into()))?;
            subject.id = subject_id(&subject)?;
            let request = CandidateExportRequest {
                expected_oid: subject.parent_commit.clone(),
                subject: subject.clone(),
                metadata,
            };
            owned.reserve_candidate_operation(&operation_id, &input, &request)?;
            require_current(
                &data,
                &engine,
                &repository,
                &id,
                expected_generation,
                &subject,
            )?;
            owned.import_candidate_baseline(Path::new(&state.worktree), &state.target_commit)?;
            let record = owned.export_candidate_guarded(&engine, &request, || {
                require_current(
                    &data,
                    &engine,
                    &repository,
                    &id,
                    expected_generation,
                    &subject,
                )
            })?;
            let current = require_current(
                &data,
                &engine,
                &repository,
                &id,
                expected_generation,
                &subject,
            );
            Ok(
                json!({"ok":current.is_ok(),"candidate":record,"historical":true,"qualification":qualification(current)}),
            )
        }
        AttachedWorktreeCommand::PublishBranch {
            id,
            expected_generation,
            mirror,
            access,
            candidate,
            operation_id,
            expected_oid,
        } => {
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            let owned = Mirror::open(Path::new(&state.git), &mirror, &repository)?;
            let access = bind_transport(&owned, &state, access)?;
            let request = RemoteRequest::PublishCandidate {
                operation_id: operation_id.clone(),
                candidate_id: candidate.clone(),
                expected_oid,
            };
            if let Some(receipt) = owned.remote_lookup(&operation_id)? {
                if receipt.request != request {
                    return Err(Error::Conflict(
                        "operation id has different fixed inputs".into(),
                    ));
                }
                return Ok(
                    json!({"ok":false,"historical":true,"qualification":"not_checked","record":receipt}),
                );
            }
            let record = owned
                .candidate_record(&candidate)?
                .ok_or_else(|| Error::NotFound("candidate".into()))?;
            require_current(
                &data,
                &engine,
                &repository,
                &id,
                expected_generation,
                &record.request.subject,
            )?;
            let receipt = owned.remote_run_guarded(&request, &access, || {
                require_current(
                    &data,
                    &engine,
                    &repository,
                    &id,
                    expected_generation,
                    &record.request.subject,
                )
            })?;
            let current = require_current(
                &data,
                &engine,
                &repository,
                &id,
                expected_generation,
                &record.request.subject,
            );
            let ok = current.is_ok()
                && matches!(
                    receipt.state,
                    RemoteState::Published | RemoteState::ObservedDesired
                );
            Ok(
                json!({"ok":ok,"historical":true,"qualification":qualification(current),"record":receipt}),
            )
        }
        AttachedWorktreeCommand::RecoverPr { id, publication } => {
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            let receipt =
                crate::publish_cli::recover_candidate(&state.remote_url, &id, &publication).await?;
            Ok(
                json!({"ok":false,"historical":true,"qualification":"not_checked","publication":receipt}),
            )
        }
        AttachedWorktreeCommand::RecoverBranch {
            id,
            mirror,
            operation_id,
            access,
        } => {
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            let owned = Mirror::open(Path::new(&state.git), &mirror, &repository)?;
            let access = bind_transport(&owned, &state, access)?;
            Ok(
                json!({"ok":false,"historical":true,"qualification":"not_checked","record":owned.remote_recover_with(&operation_id, &access)?}),
            )
        }
        AttachedWorktreeCommand::PublishPr {
            id,
            expected_generation,
            mirror,
            access,
            candidate,
            operation_id,
            previous_publication,
            title,
            body,
        } => {
            // The caller operation is the existing exact branch receipt, not a second push.
            let state = AttachmentJournal::open_existing(&data, &repository)?.state(&id)?;
            let owned = Mirror::open(Path::new(&state.git), &mirror, &repository)?;
            let access = bind_transport(&owned, &state, access)?;
            let record = owned
                .candidate_record(&candidate)?
                .ok_or_else(|| Error::NotFound("candidate".into()))?;
            let push = owned
                .remote_lookup(&operation_id)?
                .ok_or_else(|| Error::Conflict("branch publication receipt is required".into()))?;
            if !matches!(&push.request, RemoteRequest::PublishCandidate { candidate_id, .. } if candidate_id == &candidate)
                || !matches!(
                    push.state,
                    RemoteState::Published | RemoteState::ObservedDesired
                )
                || record.commit.as_deref() != Some(&push.commit)
            {
                return Err(Error::Conflict(
                    "branch receipt differs from candidate".into(),
                ));
            }
            let guard = || {
                require_current(
                    &data,
                    &engine,
                    &repository,
                    &id,
                    expected_generation,
                    &record.request.subject,
                )?;
                if owned.remote_observe_with(&access)? != record.commit {
                    return Err(Error::Conflict("remote candidate head moved".into()));
                }
                Ok(())
            };
            guard()?;
            let result = crate::publish_cli::publish_candidate(
                crate::publish_cli::CandidatePublication {
                    mirror: &owned,
                    subject: record.request.subject.clone(),
                    endpoint: state.remote_url,
                    branch: state.output,
                    title,
                    body,
                    previous_publication,
                },
                guard,
            )
            .await?;
            if result["receipt_replayed"] == true {
                return Ok(
                    json!({"ok":false,"historical":true,"qualification":"not_checked","publication":result}),
                );
            }
            let current = require_current(
                &data,
                &engine,
                &repository,
                &id,
                expected_generation,
                &record.request.subject,
            );
            Ok(
                json!({"ok":current.is_ok() && result.get("error").is_none(),"historical":true,"qualification":qualification(current),"publication":result}),
            )
        }
    }
}

#[derive(Debug, Args)]
pub struct TransportArgs {
    /// Explicit mode-0600 HTTPS username/password JSON. Never retained.
    #[arg(long)]
    credential_file: Option<PathBuf>,
    /// Explicit HTTPS CA PEM; omitted uses system roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,
}

fn bind_transport(
    mirror: &Mirror,
    state: &agentscm::attached_worktree::AttachmentState,
    args: TransportArgs,
) -> Result<agentscm::git_export::remote::Access> {
    use agentscm::git_export::remote::{Access, Endpoint, HttpsTrust};
    if state.output == state.target {
        return Err(Error::Conflict("target cannot be the output branch".into()));
    }
    let access = Access {
        credential_file: args.credential_file,
        credential_helper: Some(std::env::current_exe()?),
        ca_file: args.ca_file,
        ssh: None,
    };
    if state.remote_url.starts_with("https://") {
        let endpoint = Endpoint::Https {
            url: state.remote_url.clone(),
            trust: match &access.ca_file {
                Some(path) => HttpsTrust::CaFile {
                    sha256: agentscm::git_export::remote::network::trust_digest(path)?,
                },
                None => HttpsTrust::System,
            },
        };
        mirror.remote_bind_network(endpoint, &state.output)?;
    } else if Path::new(&state.remote_url).is_absolute() {
        mirror.remote_bind(Path::new(&state.remote_url), &state.output)?;
    } else {
        return Err(Error::Invalid("attached publication supports canonical HTTPS or an absolute local bare remote; SSH source mapping is not qualified".into()));
    }
    Ok(access)
}

fn qualify(
    data: &Path,
    engine: &Engine,
    repository: &str,
    id: &str,
    generation: u64,
) -> Result<CandidateSubject> {
    agentscm::candidate::qualify_attached_candidate_native(data, engine, repository, id, generation)
}

fn require_current(
    data: &Path,
    engine: &Engine,
    repository: &str,
    id: &str,
    generation: u64,
    expected: &CandidateSubject,
) -> Result<()> {
    let mut current = qualify(data, engine, repository, id, generation)?;
    current.id = expected.id.clone();
    current.parent_commit = expected.parent_commit.clone();
    if current != *expected {
        return Err(Error::Conflict("prepared subject is stale".into()));
    }
    Ok(())
}
fn qualification(result: Result<()>) -> Value {
    match result {
        Ok(()) => json!({"state":"current_at_comparison"}),
        Err(error) => json!({"state":"stale_or_unknown","detail":error.to_string()}),
    }
}
fn subject_id(s: &CandidateSubject) -> Result<String> {
    s.canonical_id()
}
