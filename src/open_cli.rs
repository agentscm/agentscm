//! `agentscm open`: open this checkout's Hub page from this computer, already connected.
//!
//! It finds this user's local front door (`serve --front-door`) or starts it, mints a single-use
//! connection code with the service's admin secret, and opens the default browser on the
//! service's own page with the code in the URL fragment. The tray's Open Hub calls the same code.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentscm::{Error, Result};
use clap::Args;
use reqwest::Method;

use crate::client_config;
use crate::local_service::{self, ServiceRecord, valid_hex};

/// The port the front door asks for first, so a bookmarked page usually keeps working.
const DEFAULT_BIND: &str = "127.0.0.1:7340";
const START_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Args)]
pub(crate) struct OpenArgs {
    /// Stop this computer's local Hub page instead of opening it.
    #[arg(long)]
    stop: bool,
}

/// What `open` connected: the repository and the page address (without the code).
pub(crate) struct Opened {
    pub repository: String,
    pub page: String,
}

pub(crate) async fn run(args: OpenArgs) -> Result<()> {
    if args.stop {
        return match stop().await? {
            true => {
                println!("Stopped this computer's AgentSCM Hub page.");
                Ok(())
            }
            false => {
                println!("This computer's AgentSCM Hub page is not running.");
                Ok(())
            }
        };
    }
    let opened = open_checkout(&std::env::current_dir()?).await?;
    println!(
        "Opened {} from this computer at {}",
        opened.repository, opened.page
    );
    Ok(())
}

/// Open the browser on `path`'s repository, starting the local service when needed.
pub(crate) async fn open_checkout(path: &Path) -> Result<Opened> {
    let connections = client_config::load()?;
    let (connection, _) =
        client_config::connection_for_checkout(&connections, path).ok_or_else(|| {
            Error::Invalid(
                "this folder is not connected to a Hub yet; run `agentscm client setup` here first"
                    .into(),
            )
        })?;
    if !client_config::is_current(connection) {
        return Err(Error::Invalid(
            "this checkout's Hub connection is out of date; run `agentscm client doctor` here"
                .into(),
        ));
    }
    if !crate::hub_cli::has_session(Some(&connection.hub))? {
        return Err(Error::Invalid(format!(
            "this computer is not signed in to {hub}; run `agentscm hub login --hub {hub}` first",
            hub = connection.hub
        )));
    }
    let client = agentscm::hub::forge_http::client()
        .map_err(|_| Error::Invalid("HTTP client unavailable".into()))?;
    let record = running_or_start(&client).await?;
    let code = mint_code(&client, &record).await?;
    let page = format!("{}/ui/", record.base);
    // Fragments are not sent to servers; the page removes this one from history before use.
    crate::hub_cli::open_url(&format!(
        "{page}#pair={code}&repo={}@{}",
        connection.repo, connection.hub
    ));
    Ok(Opened {
        repository: connection.repo.clone(),
        page,
    })
}

/// The live front door, confirmed by the instance it reports, or a newly started one.
async fn running_or_start(client: &reqwest::Client) -> Result<ServiceRecord> {
    if let Some(record) = local_service::load()?
        && confirmed(client, &record).await
    {
        return Ok(record);
    }
    let dir = local_service::ensure_dir()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("service.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--no-telemetry")
        .arg("--data-dir")
        .arg(dir.join("store"))
        .args(["serve", "--front-door", "--bind", DEFAULT_BIND])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Its own process group: closing this terminal does not stop the page.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(Error::Invalid(format!(
                "the local Hub page stopped while starting ({status}); see {}",
                dir.join("service.log").display()
            )));
        }
        if let Some(record) = local_service::load()?
            && record.pid == child.id()
            && confirmed(client, &record).await
        {
            return Ok(record);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    Err(Error::Invalid(format!(
        "the local Hub page did not start in time; see {}",
        dir.join("service.log").display()
    )))
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

async fn mint_code(client: &reqwest::Client, record: &ServiceRecord) -> Result<String> {
    let reply = crate::hub_cli::call(
        client,
        Method::POST,
        &format!("{}/v1/local/pairings", record.base),
        Some(&record.admin),
        None,
    )
    .await?;
    match reply {
        (201, reply) if reply["instance"] == record.instance => reply["code"]
            .as_str()
            .filter(|code| valid_hex(code, 64))
            .map(str::to_owned)
            .ok_or_else(|| Error::Invalid("the local Hub page returned no connection code".into())),
        _ => Err(Error::Invalid(
            "the local Hub page refused to create a connection code".into(),
        )),
    }
}

/// Stop the running front door after confirming which process it is.
pub(crate) async fn stop() -> Result<bool> {
    let Some(record) = local_service::load()? else {
        return Ok(false);
    };
    let client = agentscm::hub::forge_http::client()
        .map_err(|_| Error::Invalid("HTTP client unavailable".into()))?;
    if !confirmed(&client, &record).await {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(record.pid)
            .map_err(|_| Error::Invalid("the local Hub page's process id is invalid".into()))?;
        // SAFETY: sends SIGTERM to the confirmed service process; no memory is shared.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_default_bind_is_loopback() {
        let address: std::net::SocketAddr = super::DEFAULT_BIND.parse().unwrap();
        assert!(address.ip().is_loopback());
    }
}
