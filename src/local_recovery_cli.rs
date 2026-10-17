use agentscm::Result;
use agentscm::local_recovery;
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum RecoveryCommand {
    /// Capture physically isolated local state, explicit workspaces and attachments.
    Backup {
        output: PathBuf,
        #[arg(long)]
        quiesced: bool,
        #[arg(long)]
        workspace: Vec<PathBuf>,
        #[arg(long)]
        attachment: Vec<PathBuf>,
        #[arg(long)]
        issue_config: Option<PathBuf>,
    },
    /// Verify the exact backup inventory, SQLite identities and source closure.
    Verify {
        backup: PathBuf,
        #[arg(long)]
        issue_config: Option<PathBuf>,
    },
    /// Restore to a new owned directory, initially quarantined.
    Restore {
        backup: PathBuf,
        destination: PathBuf,
        #[arg(long)]
        issue_config: Option<PathBuf>,
    },
    /// Acknowledge physical isolation and external reconciliation, invalidate approvals.
    Activate {
        destination: PathBuf,
        #[arg(long)]
        old_writer_disabled: bool,
        #[arg(long)]
        external_state_reconciled: bool,
        #[arg(long)]
        issue_config: Option<PathBuf>,
    },
}

pub fn run(data_dir: &Path, command: RecoveryCommand) -> Result<Value> {
    match command {
        RecoveryCommand::Backup {
            output,
            quiesced,
            workspace,
            attachment,
            issue_config,
        } => {
            let manifest = match issue_config {
                Some(path) => {
                    let config = local_recovery::IssueWorkflowRecoveryConfig::read(&path)?;
                    local_recovery::backup_with_issue_config(
                        data_dir,
                        &output,
                        &workspace,
                        &attachment,
                        quiesced,
                        &config,
                    )?
                }
                None => {
                    local_recovery::backup(data_dir, &output, &workspace, &attachment, quiesced)?
                }
            };
            Ok(json!({"ok":true,"manifest":manifest,"backup":output}))
        }
        RecoveryCommand::Verify {
            backup,
            issue_config,
        } => {
            let manifest = match issue_config {
                Some(path) => {
                    let config = local_recovery::IssueWorkflowRecoveryConfig::read(&path)?;
                    local_recovery::verify_with_issue_config(&backup, &config)?
                }
                None => local_recovery::verify(&backup)?,
            };
            Ok(json!({"ok":true,"manifest":manifest}))
        }
        RecoveryCommand::Restore {
            backup,
            destination,
            issue_config,
        } => {
            let manifest = match issue_config {
                Some(path) => {
                    let config = local_recovery::IssueWorkflowRecoveryConfig::read(&path)?;
                    local_recovery::restore_with_issue_config(&backup, &destination, &config)?
                }
                None => local_recovery::restore(&backup, &destination)?,
            };
            let instance = local_recovery::restore_instance(&destination)?;
            Ok(
                json!({"ok":true,"manifest":manifest,"quarantined":true,"destination":destination,"restore_instance":instance}),
            )
        }
        RecoveryCommand::Activate {
            destination,
            old_writer_disabled,
            external_state_reconciled,
            issue_config,
        } => {
            let policy_epoch = match issue_config {
                Some(path) => {
                    let config = local_recovery::IssueWorkflowRecoveryConfig::read(&path)?;
                    local_recovery::activate_with_issue_config(
                        &destination,
                        old_writer_disabled,
                        external_state_reconciled,
                        &config,
                    )?
                }
                None => local_recovery::activate(
                    &destination,
                    old_writer_disabled,
                    external_state_reconciled,
                )?,
            };
            Ok(json!({"ok":true,"policy_epoch":policy_epoch,"approvals_revoked":true}))
        }
    }
}
