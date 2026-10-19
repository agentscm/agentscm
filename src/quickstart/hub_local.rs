//! A hub on this machine in Docker. Docker itself cannot run in CI, so the commands and
//! files are built by small pure functions (unit tested); `start` only runs them in order.
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::HubRuntime;
use super::detect::which;
use super::hub_setup::{self, Runtime, Started};
use super::manifest::{LocalHubRecord, Manifest};
use super::process::{diagnostic, run};
use crate::hub_cli::write_private;
use crate::prompt::Ui;

const SECRETS: &str = "/data/secrets";
const QUICK: Duration = Duration::from_secs(20);

pub(crate) struct Options {
    pub(crate) runtime: HubRuntime,
    pub(crate) port: u16,
    pub(crate) project: String,
}

/// The published image for this build: the bare Cargo version, never `latest`.
pub(crate) fn default_image() -> String {
    format!(
        "ghcr.io/counterbranch/agentscm:{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
const TEST_PLATFORM: &str = "linux/amd64";

fn env_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (found, value) = line.split_once('=')?;
        (found.trim() == key && !value.is_empty()).then_some(value)
    })
}

/// An explicit process setting wins, followed by a persisted choice and the daemon's native
/// Linux architecture. Every image, including an override, is checked against this selection.
fn selected_platform(
    process_platform: Option<String>,
    existing_env: &str,
    daemon_platform: &str,
) -> Option<String> {
    process_platform
        .filter(|value| !value.is_empty())
        .or_else(|| env_value(existing_env, "DOCKER_DEFAULT_PLATFORM").map(str::to_owned))
        .or_else(|| Some(daemon_platform.to_owned()))
}

fn platform_ok(platform: &str) -> bool {
    !platform.is_empty()
        && platform
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_.-".contains(&byte))
}

/// The default profile of compose.hub.yaml, run from a published image.
const COMPOSE: &str = r#"# Written by `agentscm quickstart`: the default profile of compose.hub.yaml, using a published image.
services:
  hub:
    image: ${AGENTSCM_HUB_IMAGE:-@IMAGE@}
@PLATFORM@
    command: ["hub", "serve", "--bind", "0.0.0.0:@PORT@", "--public", "--public-url", "${AGENTSCM_PUBLIC_URL:-http://127.0.0.1:@PORT@}", "--poll-seconds", "${AGENTSCM_POLL_SECONDS:-300}"]
    ports:
      - "127.0.0.1:@PORT@:@PORT@"
    environment:
      AGENTSCM_GITHUB_CLIENT_ID: ${AGENTSCM_GITHUB_CLIENT_ID:-}
      AGENTSCM_GITHUB_CLIENT_SECRET: ${AGENTSCM_GITHUB_CLIENT_SECRET:-}
      AGENTSCM_GITHUB_CLIENT_SECRET_FILE: ${AGENTSCM_GITHUB_CLIENT_SECRET_FILE:-}
      AGENTSCM_GITHUB_PAT: ${AGENTSCM_GITHUB_PAT:-}
      AGENTSCM_GITHUB_PAT_FILE: ${AGENTSCM_GITHUB_PAT_FILE:-}
      AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET: ${AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET:-}
      AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET_FILE: ${AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET_FILE:-}
      AGENTSCM_GITHUB_APP_NAME: ${AGENTSCM_GITHUB_APP_NAME:-}
      AGENTSCM_GITHUB_APP_ORG: ${AGENTSCM_GITHUB_APP_ORG:-}
      AGENTSCM_GITLAB_URL: ${AGENTSCM_GITLAB_URL:-}
      AGENTSCM_GITLAB_CLIENT_ID: ${AGENTSCM_GITLAB_CLIENT_ID:-}
      AGENTSCM_GITLAB_CLIENT_SECRET: ${AGENTSCM_GITLAB_CLIENT_SECRET:-}
      AGENTSCM_GITLAB_CLIENT_SECRET_FILE: ${AGENTSCM_GITLAB_CLIENT_SECRET_FILE:-}
      AGENTSCM_GITLAB_TOKEN: ${AGENTSCM_GITLAB_TOKEN:-}
      AGENTSCM_GITLAB_TOKEN_FILE: ${AGENTSCM_GITLAB_TOKEN_FILE:-}
      AGENTSCM_GITLAB_WEBHOOK_SECRET: ${AGENTSCM_GITLAB_WEBHOOK_SECRET:-}
      AGENTSCM_GITLAB_WEBHOOK_SECRET_FILE: ${AGENTSCM_GITLAB_WEBHOOK_SECRET_FILE:-}
      AGENTSCM_ADMINS: ${AGENTSCM_ADMINS:-}
      AGENTSCM_ALLOWED_USERS: ${AGENTSCM_ALLOWED_USERS:-}
    read_only: true
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true
    tmpfs:
      - /tmp:size=16m,mode=1777
    pids_limit: 128
    mem_limit: 512m
    cpus: 1.0
    restart: unless-stopped
    volumes:
      - hub-data:/data

volumes:
  hub-data:
"#;

const FORCE_RECREATE: [&str; 5] = ["up", "-d", "--pull", "never", "--force-recreate"];

#[cfg(test)]
pub(crate) fn compose_text() -> String {
    compose_text_for(Some(TEST_PLATFORM), 7332)
}

fn compose_text_for(platform: Option<&str>, port: u16) -> String {
    let platform = platform
        .map(|value| format!("    platform: \"{value}\""))
        .unwrap_or_default();
    COMPOSE
        .replace("@IMAGE@", &default_image())
        .replace("@PLATFORM@", &platform)
        .replace("@PORT@", &port.to_string())
}

/// `KEY=value` lines, in order, with `updates` replacing or appending. Values must be
/// plain (no whitespace, quotes or `$`), because compose would reinterpret them.
pub(crate) fn env_text(
    existing: &str,
    updates: &[(&str, &str)],
    remove: &[&str],
) -> Result<String, String> {
    let mut pairs: Vec<(String, String)> = existing
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.to_owned()))
        .filter(|(k, _)| !remove.contains(&k.as_str()))
        .collect();
    for (key, value) in updates {
        if value.is_empty()
            || value
                .chars()
                .any(|c| c.is_whitespace() || "\"'$\\`".contains(c))
        {
            return Err(format!("{key} has a value that cannot be written to .env"));
        }
        match pairs.iter_mut().find(|(k, _)| k == key) {
            Some(pair) => pair.1 = (*value).to_owned(),
            None => pairs.push(((*key).to_owned(), (*value).to_owned())),
        }
    }
    let mut text = String::from("# Written by `agentscm quickstart`.\n");
    for (key, value) in pairs {
        text.push_str(&format!("{key}={value}\n"));
    }
    Ok(text)
}

fn env_has(text: &str, key: &str) -> bool {
    text.lines().any(|l| {
        l.split_once('=')
            .is_some_and(|(k, v)| k == key && !v.is_empty())
    })
}

pub(crate) fn compose_args(dir: &Path, project: &str, rest: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "compose",
        "-p",
        project,
        "-f",
        &dir.join("compose.yaml").display().to_string(),
        "--env-file",
        &dir.join(".env").display().to_string(),
    ]
    .map(str::to_owned)
    .into();
    args.extend(rest.iter().map(|s| (*s).to_owned()));
    args
}

pub(crate) fn exec_args(dir: &Path, project: &str, command: &[&str]) -> Vec<String> {
    let mut rest = vec!["exec", "-T", "hub"];
    rest.extend(command);
    compose_args(dir, project, &rest)
}

/// Connection ids named by `ls /data/secrets/con_*.json`.
pub(crate) fn connection_ids(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.trim().rsplit('/').next()?.strip_suffix(".json"))
        .filter(|id| {
            id.starts_with("con_") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .map(str::to_owned)
        .collect()
}

fn docker() -> Result<PathBuf, String> {
    which("docker").ok_or_else(|| "Docker was not found on PATH.".to_owned())
}

fn run_visible(args: &[String]) -> Result<bool, String> {
    let output = run(
        std::process::Command::new(docker()?).args(args),
        Duration::from_secs(60),
    )?;
    if !output.success && !diagnostic(&output).is_empty() {
        eprintln!("{}", diagnostic(&output));
    }
    Ok(output.success)
}

fn run_capture(args: &[String], limit: Duration) -> Option<String> {
    let output = run(std::process::Command::new(docker().ok()?).args(args), limit).ok()?;
    output.success.then_some(output.stdout)
}

fn matches_platform(found: &str, requested: &str) -> bool {
    found == requested
        || (requested.split('/').count() == 2
            && found
                .strip_prefix(requested)
                .is_some_and(|rest| rest.starts_with('/')))
        || (requested == "linux/arm64/v8" && found == "linux/arm64")
}

fn acquire_image(image: &str, platform: Option<&str>) -> Result<(), String> {
    let mut inspect = vec!["image".to_owned(), "inspect".to_owned()];
    if let Some(platform) = platform {
        inspect.extend([
            "--format".into(),
            "{{.Os}}/{{.Architecture}}{{if .Variant}}/{{.Variant}}{{end}}".into(),
        ]);
        inspect.push(image.to_owned());
        if run_capture(&inspect, QUICK).is_some_and(|found| matches_platform(&found, platform)) {
            return Ok(());
        }
    } else {
        inspect.push(image.to_owned());
        if run_capture(&inspect, QUICK).is_some() {
            return Ok(());
        }
    }

    println!(
        "Pulling {image}{} ...",
        platform.map_or(String::new(), |value| format!(" for {value}"))
    );
    let mut pull = vec!["pull".to_owned()];
    if let Some(platform) = platform {
        pull.extend(["--platform".into(), platform.to_owned()]);
    }
    pull.push(image.to_owned());
    let output = run(
        std::process::Command::new(docker()?).args(&pull),
        Duration::from_secs(180),
    )?;
    if output.success {
        Ok(())
    } else {
        let platform = platform
            .map(|value| format!(" for platform {value}"))
            .unwrap_or_default();
        let emulation = if platform.contains("linux/arm64") {
            " If only an AMD64 image exists, explicitly set DOCKER_DEFAULT_PLATFORM=linux/amd64 to opt into Docker emulation, when your Docker runtime supports it. Emulation can be slower and is not guaranteed; no architecture was switched automatically."
        } else {
            ""
        };
        Err(format!(
            "Docker could not pull the hub image {image}{platform}: {}. Set AGENTSCM_HUB_IMAGE to a usable image, or connect to an existing hub.{emulation}",
            diagnostic(&output)
        ))
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        super::private_dir(dir).map_err(|e| e.to_string())?;
    }
    write_private(path, text).map_err(|e| e.to_string())
}

fn write_setup_files(
    compose: &Path,
    compose_text: &str,
    env: &Path,
    env_text: &str,
) -> Result<(), String> {
    let old_compose = std::fs::read(compose).ok();
    let old_env = std::fs::read(env).ok();
    write(compose, compose_text)?;
    if let Err(error) = write(env, env_text) {
        match old_compose {
            Some(bytes) => {
                let _ = std::fs::write(compose, bytes);
            }
            None => {
                let _ = std::fs::remove_file(compose);
            }
        }
        match old_env {
            Some(bytes) => {
                let _ = std::fs::write(env, bytes);
            }
            None => {
                let _ = std::fs::remove_file(env);
            }
        }
        return Err(error);
    }
    Ok(())
}

struct DockerRuntime {
    dir: PathBuf,
    project: String,
    url: String,
}

impl DockerRuntime {
    fn up(&self, force: bool) -> Result<(), String> {
        let rest: &[&str] = if force {
            &FORCE_RECREATE
        } else {
            &["up", "-d", "--pull", "never"]
        };
        run_visible(&compose_args(&self.dir, &self.project, rest))?
            .then_some(())
            .ok_or_else(|| "Docker could not start the hub.".into())
    }
}

impl Runtime for DockerRuntime {
    fn url(&self) -> &str {
        &self.url
    }

    fn connection_ids(&self) -> Result<Vec<String>, String> {
        let command = ["sh", "-c", "ls /data/secrets/con_*.json 2>/dev/null"];
        Ok(connection_ids(
            &run_capture(&exec_args(&self.dir, &self.project, &command), QUICK).unwrap_or_default(),
        ))
    }

    fn read_secret(&self, name: &str) -> Result<String, String> {
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err("invalid Hub secret name".into());
        }
        run_capture(
            &exec_args(
                &self.dir,
                &self.project,
                &["cat", &format!("{SECRETS}/{name}")],
            ),
            QUICK,
        )
        .ok_or_else(|| format!("could not read Hub secret {name}"))
    }

    fn restart_fresh(&mut self) -> Result<(), String> {
        self.up(true)
    }

    async fn configure_github(
        &mut self,
        connection: &str,
        client_id: &str,
        login: &str,
    ) -> Result<(), String> {
        let env = self.dir.join(".env");
        let existing = std::fs::read_to_string(&env).unwrap_or_default();
        if env_has(&existing, "AGENTSCM_ADMINS") {
            return Ok(());
        }
        let secret_file = format!("{SECRETS}/{connection}.client_secret");
        let admins = format!("github:{login}");
        let text = env_text(
            &existing,
            &[
                ("AGENTSCM_GITHUB_CLIENT_ID", client_id),
                ("AGENTSCM_GITHUB_CLIENT_SECRET_FILE", &secret_file),
                ("AGENTSCM_ADMINS", &admins),
            ],
            &[],
        )?;
        write(&env, &text)?;
        self.up(false)
    }
}

/// The GitHub login of the hub's administrator. With `--yes` only a login that was given or
/// that `gh` verified is used; interactively `git config` may prefill the question.
fn github_login(given: Option<&str>, ui: &Ui) -> Result<String, String> {
    hub_setup::github_login(given, ui)
}

/// Starts (or re-uses) the local hub and completes first-run GitHub setup.
pub(crate) async fn start(
    ui: &Ui,
    login: Option<&str>,
    tty: bool,
    home: &Path,
    repo: Option<&str>,
    manifest: &mut Manifest,
    options: &Options,
) -> Result<Started, String> {
    if options.runtime != HubRuntime::Docker {
        return Err("the Docker Hub backend received another runtime".into());
    }
    let url = format!("http://127.0.0.1:{}", options.port);
    if manifest.local_hub.is_none() {
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, options.port))
            .map(drop)
            .map_err(|_| {
                format!(
                    "127.0.0.1:{} is already in use; choose another --hub-port. No existing service was stopped.",
                    options.port
                )
            })?;
    }
    // Without a terminal the administrator must be known before anything starts.
    let preset = if ui.yes {
        Some(github_login(login, ui)?)
    } else {
        None
    };
    let dir = home.join(".agentscm").join("hub");
    let (compose, env) = (dir.join("compose.yaml"), dir.join(".env"));
    println!(
        "Will write {}/compose.yaml and {}/.env (0600), then start the hub with Docker.",
        dir.display(),
        dir.display()
    );
    if !ui.confirm("Go ahead?", true) {
        return Err("Skipped the local hub.".into());
    }
    let existing_env = std::fs::read_to_string(&env).unwrap_or_default();
    let prior_compose = std::fs::read(&compose).ok();
    let prior_env = std::fs::read(&env).ok();
    let prior_record = manifest.local_hub.clone();
    let configured_image = std::env::var("AGENTSCM_HUB_IMAGE")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| env_value(&existing_env, "AGENTSCM_HUB_IMAGE").map(str::to_owned));
    let selected_image = configured_image.clone().unwrap_or_else(default_image);
    let requested_platform = std::env::var("DOCKER_DEFAULT_PLATFORM")
        .ok()
        .filter(|value| !value.is_empty());
    let daemon = super::detect::docker_daemon()?;
    if daemon.os != "linux" || !matches!(daemon.arch.as_str(), "amd64" | "arm64") {
        return Err(format!(
            "The Docker daemon reports {}/{}; the local Hub supports Linux amd64 or arm64 daemons.",
            daemon.os, daemon.arch
        ));
    }
    let daemon_platform = format!("{}/{}", daemon.os, daemon.arch);
    let platform = selected_platform(requested_platform.clone(), &existing_env, &daemon_platform);
    if platform.as_deref().is_some_and(|value| !platform_ok(value)) {
        return Err("DOCKER_DEFAULT_PLATFORM isn't a valid Docker platform.".into());
    }
    let explicit_platform = requested_platform
        .as_deref()
        .or_else(|| env_value(&existing_env, "DOCKER_DEFAULT_PLATFORM"));
    if explicit_platform.is_some() {
        println!(
            "Docker daemon platform: {daemon_platform}; requested platform: {}. Any cross-architecture emulation was explicitly selected with DOCKER_DEFAULT_PLATFORM.",
            explicit_platform.unwrap_or("Docker default")
        );
    } else {
        println!("Docker daemon platform: {daemon_platform}; no emulation platform was requested.");
    }

    // Reject malformed image references before Docker can interpret one as an option.
    if selected_image.starts_with('-')
        || selected_image
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "\"'$\\`".contains(c))
    {
        return Err("AGENTSCM_HUB_IMAGE must be an image reference without leading hyphens, whitespace or shell interpolation.".into());
    }
    // Do not leave setup files or an uninstall record behind when image acquisition fails.
    acquire_image(&selected_image, platform.as_deref())?;

    // Keep explicit image overrides across resume and retained-data reinstall.
    let mut updates = Vec::new();
    let mut stale = vec![];
    if let Some(image) = configured_image.as_deref() {
        updates.push(("AGENTSCM_HUB_IMAGE", image));
    } else {
        stale.push("AGENTSCM_HUB_IMAGE");
    }
    if let Some(platform) = requested_platform.as_deref() {
        updates.push(("DOCKER_DEFAULT_PLATFORM", platform));
    }
    let rendered_env = env_text(&existing_env, &updates, &stale)?;
    let rendered_compose = compose_text_for(platform.as_deref(), options.port);
    write_setup_files(&compose, &rendered_compose, &env, &rendered_env)?;
    manifest.local_hub = Some(LocalHubRecord {
        dir: dir.clone(),
        project: options.project.clone(),
        port: options.port,
    });
    if let Err(error) = manifest.save() {
        manifest.local_hub = prior_record;
        for (path, prior) in [(&compose, prior_compose), (&env, prior_env)] {
            match prior {
                Some(bytes) => {
                    let _ = std::fs::write(path, bytes);
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        return Err(error.to_string());
    }

    let up = compose_args(&dir, &options.project, &["up", "-d", "--pull", "never"]);
    if !run_visible(&up)? {
        return Err(format!(
            "Docker could not start the hub on 127.0.0.1:{}; its setup remains recorded for recovery.",
            options.port
        ));
    }
    wait_healthy(&url).await?;

    let mut runtime = DockerRuntime {
        dir,
        project: options.project.clone(),
        url,
    };
    hub_setup::finish(&mut runtime, ui, login, preset, tty, repo).await
}

async fn wait_healthy(url: &str) -> Result<(), String> {
    if super::wait_healthy(url, Duration::from_secs(60)).await {
        Ok(())
    } else {
        Err(format!(
            "The hub at {url} didn't become healthy within 60 seconds."
        ))
    }
}

/// `docker compose down`, with `-v` to delete the hub's data as well.
pub(crate) fn down(dir: &Path, project: &str, volumes: bool) -> bool {
    let mut rest = vec!["down"];
    if volumes {
        rest.push("-v");
    }
    run_visible(&compose_args(dir, project, &rest)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_image_is_the_bare_cargo_version() {
        assert_eq!(
            default_image(),
            format!(
                "ghcr.io/counterbranch/agentscm:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert!(!default_image().contains(":v") && !default_image().ends_with("latest"));
        assert!(compose_text().contains(&format!("${{AGENTSCM_HUB_IMAGE:-{}}}", default_image())));
        assert!(compose_text().contains("    platform: \"linux/amd64\"\n"));
    }

    #[test]
    fn platform_selection_respects_explicit_and_persisted_choices() {
        assert_eq!(
            selected_platform(
                Some("linux/arm64".into()),
                "DOCKER_DEFAULT_PLATFORM=linux/amd64\n",
                "linux/amd64",
            ),
            Some("linux/arm64".into())
        );
        assert_eq!(
            selected_platform(
                None,
                "DOCKER_DEFAULT_PLATFORM=linux/arm64/v8\n",
                "linux/amd64"
            ),
            Some("linux/arm64/v8".into())
        );
        assert_eq!(
            selected_platform(None, "", "linux/arm64"),
            Some("linux/arm64".into())
        );
        assert_eq!(
            selected_platform(None, "", "linux/arm64"),
            Some("linux/arm64".into())
        );
        assert_eq!(
            selected_platform(None, "DOCKER_DEFAULT_PLATFORM=linux/amd64\n", "linux/arm64"),
            Some("linux/amd64".into())
        );
        assert!(matches_platform("linux/arm64", "linux/arm64/v8"));
        assert!(matches_platform("linux/arm64/v8", "linux/arm64"));
        assert!(!matches_platform("linux/arm/v6", "linux/arm/v7"));
        assert!(platform_ok("linux/arm64/v8"));
        assert!(!platform_ok("linux/amd64\ncommand: nope"));
        assert!(!platform_ok("linux:${ARCH}"));
        assert!(!compose_text_for(None, 7444).contains("platform:"));
        let compose = compose_text_for(Some("linux/arm64"), 7444);
        assert!(compose.contains("    platform: \"linux/arm64\"\n"));
        assert!(compose.contains("127.0.0.1:7444:7444"));
    }

    #[test]
    fn compose_file_tracks_the_repository_profile() {
        let source = include_str!("../../compose.hub.yaml");
        let command = source
            .lines()
            .find(|l| l.trim_start().starts_with("command:"))
            .unwrap();
        assert!(compose_text().contains(command.trim()));
        for line in source
            .lines()
            .filter(|l| l.contains("AGENTSCM_"))
            .filter(|l| l.contains(":-}"))
        {
            assert!(compose_text().contains(line.trim()), "{line}");
        }
        assert!(!compose_text().contains("build:"));
    }

    #[test]
    fn env_file_keeps_order_updates_in_place_and_rejects_odd_values() {
        let first = env_text("", &[("AGENTSCM_ADMINS", "github:me")], &[]).unwrap();
        assert_eq!(
            first,
            "# Written by `agentscm quickstart`.\nAGENTSCM_ADMINS=github:me\n"
        );
        let next = env_text(
            &first,
            &[
                ("AGENTSCM_GITHUB_CLIENT_ID", "Iv1.abc"),
                ("AGENTSCM_ADMINS", "github:you"),
            ],
            &[],
        )
        .unwrap();
        assert!(next.ends_with("AGENTSCM_ADMINS=github:you\nAGENTSCM_GITHUB_CLIENT_ID=Iv1.abc\n"));
        assert!(env_text("", &[("A", "x y")], &[]).is_err());
        assert!(env_text("", &[("A", "$x")], &[]).is_err());
        // A stale image is dropped, other lines stay.
        let stale = "AGENTSCM_HUB_IMAGE=old\nAGENTSCM_ADMINS=github:me\n";
        assert_eq!(
            env_text(stale, &[], &["AGENTSCM_HUB_IMAGE"]).unwrap(),
            first
        );
        assert!(env_has(&next, "AGENTSCM_ADMINS") && !env_has(&next, "AGENTSCM_NOPE"));
    }

    #[cfg(unix)] // expects POSIX path separators
    #[test]
    fn docker_commands_are_exact() {
        let dir = Path::new("/h/.agentscm/hub");
        assert_eq!(
            compose_args(dir, "my-hub", &["up", "-d", "--pull", "never"]).join(" "),
            "compose -p my-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env up -d --pull never"
        );
        assert_eq!(
            compose_args(dir, "agentscm-hub", &FORCE_RECREATE).join(" "),
            "compose -p agentscm-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env up -d --pull never --force-recreate"
        );
        assert_eq!(
            exec_args(
                dir,
                "agentscm-hub",
                &["cat", "/data/secrets/bootstrap-setup-url"]
            )
            .join(" "),
            "compose -p agentscm-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env exec -T hub cat /data/secrets/bootstrap-setup-url"
        );
    }

    #[test]
    fn finds_connection_files() {
        let listing = "/data/secrets/con_0a1b.json\n/data/secrets/con_0a1b.client_secret\n/data/secrets/other.json\n/data/secrets/con_x;y.json\n";
        assert_eq!(connection_ids(listing), ["con_0a1b"]);
    }
}
