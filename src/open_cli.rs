//! Compatibility cleanup for the retired detached local dashboard.
use std::time::{Duration, Instant};

use agentscm::{Error, Result};
use clap::Args;
use reqwest::Method;

use crate::local_service::{self, ServiceRecord};

const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Args)]
pub(crate) struct OpenArgs {
    /// Stop a local dashboard left running by an earlier version.
    #[arg(long)]
    stop: bool,
}

pub(crate) async fn run(args: OpenArgs) -> Result<()> {
    if !args.stop {
        return Err(Error::Invalid("the local dashboard was removed; use `agentscm client status`, `agentscm client doctor` or your team's Hub. Use `agentscm open --stop` to stop an older local dashboard".into()));
    }
    println!(
        "{}",
        if stop().await? {
            "Stopped the previous local AgentSCM dashboard."
        } else {
            "No previous local AgentSCM dashboard is running."
        }
    );
    Ok(())
}

/// Requests to the previous local service never use a configured HTTP proxy.
fn loopback_client() -> Result<reqwest::Client> {
    crate::hub_cli::direct_client().map_err(|_| Error::Invalid("HTTP client unavailable".into()))
}

/// Whether the listener at the record's address is the process that wrote it. Asked without
/// credentials, so no secret goes to another process that took the port.
async fn confirmed(client: &reqwest::Client, record: &ServiceRecord) -> bool {
    matches!(
        crate::hub_cli::call(
            client,
            Method::GET,
            &format!("{}/v1/local/identity", record.base),
            None,
            None,
        )
        .await,
        Ok((200, identity)) if identity["instance"] == record.instance
    )
}

/// Stop the running front door after confirming which process it is, and wait for it to exit.
pub(crate) async fn stop() -> Result<bool> {
    let Some(record) = local_service::load()? else {
        return Ok(false);
    };
    if !confirmed(&loopback_client()?, &record).await {
        return Err(Error::Invalid(format!(
            "could not confirm the previous local dashboard; its process and recovery record were left unchanged. Inspect PID {} and the private recovery directory {} before retrying `agentscm open --stop`, then rerun `agentscm uninstall` if needed. Remove a stale record only after confirming that PID is not an AgentSCM dashboard; do not remove it while the dashboard may still be running",
            record.pid,
            local_service::dir()?.display()
        )));
    }
    let pid = libc::pid_t::try_from(record.pid)
        .map_err(|_| Error::Invalid("the local Hub page's process id is invalid".into()))?;
    // SAFETY: sends SIGTERM to the confirmed service process; no memory is shared.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while local_service::process_alive(record.pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if local_service::process_alive(record.pid) {
        return Err(Error::Invalid(
            "the previous local dashboard did not stop; its recovery record was retained".into(),
        ));
    }
    Ok(true)
}
