use std::path::{Path, PathBuf};

use agentscm::authority::Authority;
use agentscm::git_export::Mirror;
use agentscm::git_export::remote::network::trust_digest;
use agentscm::git_export::remote::{Access, Endpoint, HttpsTrust, Request, SshAccess};
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub struct GitRemoteArgs {
    /// Existing owned git-export mirror; no repository is adopted or created here.
    #[arg(long)]
    mirror: PathBuf,
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
    /// Mode-0600 JSON with username/password; never stored in the journal.
    #[arg(long)]
    credential_file: Option<PathBuf>,
    /// Explicit CA PEM for HTTPS; omitted means strict system TLS roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// Explicit SSH private identity, without agent or default identity lookup.
    #[arg(long, requires = "known_hosts")]
    ssh_identity: Option<PathBuf>,
    #[arg(long, requires = "ssh_identity")]
    known_hosts: Option<PathBuf>,
    #[arg(long, default_value = "/usr/bin/ssh")]
    ssh: PathBuf,
    #[command(subcommand)]
    command: GitRemoteCommand,
}

#[derive(Debug, Subcommand)]
enum GitRemoteCommand {
    /// Bind this mirror once to an absolute local bare remote and branch.
    Bind {
        #[arg(long)]
        remote: PathBuf,
        #[arg(long)]
        branch: String,
    },
    /// Bind a canonical HTTPS or ssh:// endpoint and its explicit host trust.
    BindNetwork {
        #[arg(long)]
        url: String,
        #[arg(long)]
        branch: String,
    },
    /// Fresh read of the bound branch advertisement; saved receipts remain historical.
    Observe,
    /// Fetch one exact observed commit, retaining it under an operation ref.
    Fetch {
        operation_id: String,
        #[arg(long)]
        expected_oid: String,
    },
    /// Publish an already verified export with an exact expected remote ref.
    Publish {
        operation_id: String,
        #[arg(long)]
        event: u64,
        /// Omit only when the remote branch must be absent.
        #[arg(long)]
        expected_oid: Option<String>,
    },
    /// Inspect a saved receipt without dispatching Git transport.
    Lookup { operation_id: String },
    /// Resolve Pending/Unknown through a fresh read, without replaying a push.
    Recover { operation_id: String },
}

pub(crate) fn telemetry_action(args: &GitRemoteArgs) -> bool {
    matches!(
        args.command,
        GitRemoteCommand::Bind { .. }
            | GitRemoteCommand::BindNetwork { .. }
            | GitRemoteCommand::Fetch { .. }
            | GitRemoteCommand::Publish { .. }
            | GitRemoteCommand::Recover { .. }
    )
}

pub fn run(args: GitRemoteArgs, data_dir: &Path) -> Result<Value> {
    let data = data_dir.canonicalize()?;
    let mirror_path = args.mirror.canonicalize()?;
    if mirror_path.starts_with(&data) || data.starts_with(&mirror_path) {
        return Err(Error::Invalid(
            "mirror and native data must be separate, non-nested directories".into(),
        ));
    }
    let repository = Authority::open_read_only(&data)?.repository_id()?;
    let mirror = Mirror::open(&args.git, &mirror_path, &repository)?;
    let ssh_access = match (args.ssh_identity, args.known_hosts) {
        (Some(identity_file), Some(known_hosts_file)) => Some(SshAccess {
            executable: args.ssh,
            identity_file,
            known_hosts_file,
        }),
        (None, None) => None,
        _ => {
            return Err(Error::Invalid(
                "SSH requires both explicit identity and host trust".into(),
            ));
        }
    };
    let access = Access {
        credential_file: args.credential_file,
        credential_helper: Some(std::env::current_exe()?),
        ca_file: args.ca_file,
        ssh: ssh_access,
    };
    let result = match args.command {
        GitRemoteCommand::Bind { remote, branch } => {
            json!({"binding":mirror.remote_bind(&remote, &branch)?})
        }
        GitRemoteCommand::BindNetwork { url, branch } => {
            let endpoint = if url.starts_with("https://") {
                Endpoint::Https {
                    url,
                    trust: match &access.ca_file {
                        Some(path) => HttpsTrust::CaFile {
                            sha256: trust_digest(path)?,
                        },
                        None => HttpsTrust::System,
                    },
                }
            } else if url.starts_with("ssh://") {
                let ssh = access.ssh.as_ref().ok_or_else(|| {
                    Error::Invalid("SSH binding requires explicit host trust and identity".into())
                })?;
                ssh.validate_paths()?;
                Endpoint::Ssh {
                    url,
                    known_hosts_sha256: trust_digest(&ssh.known_hosts_file)?,
                }
            } else {
                return Err(Error::Invalid(
                    "network binding requires HTTPS or ssh://".into(),
                ));
            };
            json!({"binding":mirror.remote_bind_network(endpoint,&branch)?})
        }
        GitRemoteCommand::Observe => {
            json!({"observed_oid":mirror.remote_observe_with(&access)?,"freshness":"point_in_time_advertisement","missing_ref":"not_advertised_not_proven_absent"})
        }
        GitRemoteCommand::Fetch {
            operation_id,
            expected_oid,
        } => {
            json!({"record":mirror.remote_run_with(&Request::Fetch { operation_id, expected_oid }, &access)?})
        }
        GitRemoteCommand::Publish {
            operation_id,
            event,
            expected_oid,
        } => {
            json!({"record":mirror.remote_run_with(&Request::Publish { operation_id, event_sequence:event, expected_oid }, &access)?})
        }
        GitRemoteCommand::Lookup { operation_id } => {
            json!({"record":mirror.remote_lookup(&operation_id)?})
        }
        GitRemoteCommand::Recover { operation_id } => {
            json!({"record":mirror.remote_recover_with(&operation_id, &access)?})
        }
    };
    Ok(json!({"ok":true,"remote":result}))
}

/// Invoked only by Git's configured credential-helper child. Never include secret
/// bytes in errors: the caller receives them only over this private stdout pipe.
pub fn credential(operation: &str) -> Result<()> {
    use std::io::Write;
    if let Some(reply) = agentscm::git_export::remote::network::credential_reply(
        operation,
        &mut std::io::stdin().lock(),
    )? {
        std::io::stdout().lock().write_all(reply.as_bytes())?;
    }
    Ok(())
}
