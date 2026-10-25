//! `agentscm open`: open this checkout's Hub page from this computer, already connected.
//!
//! It finds this user's local front door (`serve --front-door`) or starts it, mints a single-use
//! connection code with the service's admin secret, and opens the default browser on the
//! service's own page with the code in the URL fragment. The code reaches the browser through an
//! owner-only file, never a process argument other users could list. The tray's Open Hub calls
//! the same code.

use std::path::{Path, PathBuf};
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
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const LOG_BYTES: u64 = 1024 * 1024;
/// Handoff files older than this hold only expired codes and are removed.
const HANDOFF_MAX_AGE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Args)]
pub(crate) struct OpenArgs {
    /// Stop this computer's local Hub page instead of opening it.
    #[arg(long)]
    stop: bool,
}

/// What `open` connected: the repository, the page address (without the code), and the
/// owner-only file that carries the code to the browser.
pub(crate) struct Opened {
    pub repository: String,
    pub page: String,
    pub handoff: PathBuf,
}

pub(crate) async fn run(args: OpenArgs) -> Result<()> {
    if args.stop {
        println!(
            "{}",
            if stop().await? {
                "Stopped this computer's AgentSCM Hub page."
            } else {
                "This computer's AgentSCM Hub page is not running."
            }
        );
        return Ok(());
    }
    let opened = open_checkout(&std::env::current_dir()?).await?;
    println!(
        "Opened {} from this computer at {}",
        opened.repository, opened.page
    );
    println!(
        "If your browser did not open it, open {} in your browser within two minutes, or run `agentscm open` again.",
        opened.handoff.display()
    );
    Ok(())
}

/// Requests to this computer's own service never go through a configured HTTP proxy.
fn loopback_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| Error::Invalid("HTTP client unavailable".into()))
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
    let client = loopback_client()?;
    let (record, _) = running_or_start(&client).await?;
    let code = mint_code(&client, &record).await?;
    let page = format!("{}/ui/", record.base);
    let selector = format!("{}@{}", connection.repo, connection.hub);
    let target = format!("{page}#pair={code}&repo={}", fragment_value(&selector));
    let handoff = handoff(&target)?;
    crate::hub_cli::open_url(&handoff.display().to_string());
    Ok(Opened {
        repository: connection.repo.clone(),
        page,
        handoff,
    })
}

/// Percent-encode everything a URL fragment parameter cannot carry as-is.
fn fragment_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'@')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// An owner-only page that sends the browser on to `target`, so the code never appears in a
/// process argument. Older handoff files, whose codes have expired, are removed first.
fn handoff(target: &str) -> Result<PathBuf> {
    let dir = local_service::ensure_dir()?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > HANDOFF_MAX_AGE));
            if entry.file_name().to_string_lossy().starts_with("open-") && stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let path = dir.join(format!("open-{}.html", local_service::random_hex(8)?));
    // `target` holds only hex, a loopback address and percent-encoded text; quote it for the
    // script and the attribute anyway.
    let quoted = serde_json::to_string(target)?;
    let attribute = target.replace('&', "&amp;").replace('"', "&quot;");
    crate::hub_cli::write_private(
        &path,
        &format!(
            "<!doctype html><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\">\
             <title>Opening AgentSCM</title><script>location.replace({quoted})</script>\
             <noscript><meta http-equiv=\"refresh\" content=\"0;url={attribute}\"></noscript>\
             <p>Opening AgentSCM…</p>\n"
        ),
    )?;
    Ok(path)
}

/// The live front door, confirmed by the instance it reports, or a newly started one (`true`).
/// A lock keeps two concurrent `open`s from starting two services.
async fn running_or_start(client: &reqwest::Client) -> Result<(ServiceRecord, bool)> {
    let dir = local_service::ensure_dir()?;
    // Waiting for another `open` to finish starting blocks, so it waits off the async runtime.
    let lock_path = dir.join("open.lock");
    let _lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let lock = private_file(&lock_path, false)?;
        lock.lock()?;
        Ok(lock)
    })
    .await
    .map_err(|_| Error::Invalid("could not wait for another `agentscm open`".into()))??;
    if let Some(record) = local_service::load()?
        && confirmed(client, &record).await
    {
        return Ok((record, false));
    }
    let log_path = dir.join("service.log");
    if std::fs::metadata(&log_path).is_ok_and(|metadata| metadata.len() > LOG_BYTES) {
        let _ = std::fs::rename(&log_path, dir.join("service.log.1"));
    }
    let log = private_file(&log_path, true)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--no-telemetry")
        .arg("--data-dir")
        .arg(dir.join("store"))
        .args(["serve", "--front-door", "--bind", DEFAULT_BIND])
        .env_remove("AGENTSCM_SERVE_READ_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Its own process group: closing this terminal does not stop the page.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(Error::Invalid(format!(
                "the local Hub page stopped while starting ({status}); see {}",
                log_path.display()
            )));
        }
        if let Some(record) = local_service::load()?
            && record.pid == child.id()
            && confirmed(client, &record).await
        {
            // Reap the service when it exits, so a long-lived caller such as the tray keeps no
            // zombie process.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok((record, true));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    Err(Error::Invalid(format!(
        "the local Hub page did not start in time; see {}",
        log_path.display()
    )))
}

fn private_file(path: &Path, append: bool) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.read(true).write(true);
    }
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    // The mode above applies only when the file is created; an older file is narrowed too.
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(file)
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

#[cfg(target_os = "macos")]
/// The local page as started by a long-lived owner such as the tray: started in the background
/// when this computer has any connected checkout, and stopped again only if this owner started it.
pub(crate) struct LocalPage {
    started: std::sync::Arc<std::sync::Mutex<Option<u32>>>,
}

#[cfg(target_os = "macos")]
impl LocalPage {
    pub(crate) fn start_in_background() -> Self {
        let started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = started.clone();
        std::thread::spawn(move || {
            if !client_config::load().is_ok_and(|connections| !connections.is_empty()) {
                return;
            }
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let begun = runtime.block_on(async {
                let client = loopback_client()?;
                running_or_start(&client).await
            });
            if let Ok((service, true)) = begun {
                *record
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(service.pid);
            }
        });
        Self { started }
    }

    /// Stop the page if this owner started it and it is still that process.
    pub(crate) fn stop_if_started(&self) {
        let Some(pid) = *self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        else {
            return;
        };
        if local_service::load()
            .ok()
            .flatten()
            .is_none_or(|record| record.pid != pid)
        {
            return;
        }
        // The owner may be running inside an async runtime (the tray is), where a second one
        // cannot block; stop from a thread of its own and wait for it.
        let stopping = std::thread::spawn(|| {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                let _ = runtime.block_on(stop());
            }
        });
        let _ = stopping.join();
    }
}

/// Stop the running front door after confirming which process it is, and wait for it to exit.
pub(crate) async fn stop() -> Result<bool> {
    let Some(record) = local_service::load()? else {
        return Ok(false);
    };
    if !confirmed(&loopback_client()?, &record).await {
        return Ok(false);
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
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bind_is_loopback() {
        let address: std::net::SocketAddr = DEFAULT_BIND.parse().unwrap();
        assert!(address.ip().is_loopback());
    }

    #[test]
    fn fragment_values_keep_only_safe_characters() {
        assert_eq!(
            fragment_value("acme/widgets@https://hub.example:8443/base"),
            "acme/widgets@https://hub.example:8443/base"
        );
        assert_eq!(fragment_value("a&b+c%d#e f"), "a%26b%2Bc%25d%23e%20f");
    }

    #[test]
    fn the_handoff_file_is_owner_only_and_carries_the_link() {
        use std::os::unix::fs::PermissionsExt as _;
        let config = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process, so no other thread reads it.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config.path()) };
        let target = "http://127.0.0.1:7340/ui/#pair=aa&repo=acme/widgets@https://hub.example";
        let path = handoff(target).unwrap();
        assert!(path.starts_with(config.path()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        let page = std::fs::read_to_string(&path).unwrap();
        assert!(page.contains(&serde_json::to_string(target).unwrap()));
        assert!(page.contains("pair=aa&amp;repo="));
    }
}
