//! A hub on this machine in Docker. Docker itself cannot run in CI, so the commands and
//! files are built by small pure functions (unit tested); `start` only runs them in order.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::detect::{capture, which};
use super::manifest::{LocalHubRecord, Manifest};
use crate::hub_cli::{open_url, write_private};
use crate::prompt::Ui;

pub(crate) const PROJECT: &str = "agentscm-hub";
pub(crate) const URL: &str = "http://127.0.0.1:7332";
const SECRETS: &str = "/data/secrets";
const QUICK: Duration = Duration::from_secs(20);

/// The published image for this build: the bare Cargo version, never `latest`.
pub(crate) fn default_image() -> String {
    format!(
        "ghcr.io/counterbranch/agentscm:{}",
        env!("CARGO_PKG_VERSION")
    )
}

fn image() -> String {
    std::env::var("AGENTSCM_HUB_IMAGE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default_image)
}

/// The default profile of compose.hub.yaml, run from a published image.
const COMPOSE: &str = r#"# Written by `agentscm quickstart`: the default profile of compose.hub.yaml, using a published image.
services:
  hub:
    image: ${AGENTSCM_HUB_IMAGE:-@IMAGE@}
    command: ["hub", "serve", "--bind", "0.0.0.0:7332", "--public", "--public-url", "${AGENTSCM_PUBLIC_URL:-http://127.0.0.1:7332}", "--poll-seconds", "${AGENTSCM_POLL_SECONDS:-300}"]
    ports:
      - "127.0.0.1:7332:7332"
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

pub(crate) fn compose_text() -> String {
    COMPOSE.replace("@IMAGE@", &default_image())
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

pub(crate) fn compose_args(dir: &Path, rest: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "compose",
        "-p",
        PROJECT,
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

pub(crate) fn exec_args(dir: &Path, command: &[&str]) -> Vec<String> {
    let mut rest = vec!["exec", "-T", "hub"];
    rest.extend(command);
    compose_args(dir, &rest)
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

fn login_ok(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) struct Started {
    /// Where to install the hub's GitHub App, when known.
    pub(crate) install_url: Option<String>,
}

fn docker() -> Result<PathBuf, String> {
    which("docker").ok_or_else(|| "Docker was not found on PATH.".to_owned())
}

fn run_visible(args: &[String]) -> Result<bool, String> {
    let status = std::process::Command::new(docker()?)
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    Ok(status.success())
}

fn run_capture(args: &[String], limit: Duration) -> Option<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    capture(&docker().ok()?, &args, None, limit)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        super::private_dir(dir).map_err(|e| e.to_string())?;
    }
    write_private(path, text).map_err(|e| e.to_string())
}

/// The GitHub login of the hub's administrator. With `--yes` only a login that was given or
/// that `gh` verified is used; interactively `git config` may prefill the question.
fn github_login(given: Option<&str>, ui: &Ui) -> Result<String, String> {
    if let Some(login) = given {
        return if login_ok(login) {
            Ok(login.to_owned())
        } else {
            Err("--github-login isn't a GitHub login.".into())
        };
    }
    let verified = which("gh")
        .and_then(|gh| capture(&gh, &["api", "user", "-q", ".login"], None, QUICK))
        .filter(|login| login_ok(login));
    let login = if ui.yes {
        verified.unwrap_or_default()
    } else {
        let guess = verified
            .or_else(|| capture(&which("git")?, &["config", "github.user"], None, QUICK))
            .unwrap_or_default();
        ui.ask(
            "Your GitHub login (this account becomes the hub's administrator)",
            &guess,
        )
    };
    if login_ok(&login) {
        Ok(login)
    } else {
        Err("I need your GitHub login to make you the hub's administrator; pass --github-login (with --yes, or install gh and run gh auth login).".into())
    }
}

/// Starts (or re-uses) the local hub and completes first-run GitHub setup.
pub(crate) async fn start(
    ui: &Ui,
    login: Option<&str>,
    tty: bool,
    home: &Path,
    repo: Option<&str>,
    manifest: &mut Manifest,
) -> Result<Started, String> {
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
    // The image goes into .env only when the user chose one; a stale one we wrote is dropped.
    let mut updates = Vec::new();
    let mut stale = vec![];
    let image = std::env::var("AGENTSCM_HUB_IMAGE").unwrap_or_default();
    if image.is_empty() {
        stale.push("AGENTSCM_HUB_IMAGE");
    } else {
        updates.push(("AGENTSCM_HUB_IMAGE", image.as_str()));
    }
    write(&compose, &compose_text())?;
    write(&env, &env_text(&existing_env, &updates, &stale)?)?;
    manifest.local_hub = Some(LocalHubRecord {
        dir: dir.clone(),
        project: PROJECT.into(),
    });
    manifest.save().map_err(|e| e.to_string())?;

    let image = self::image();
    let inspect = ["image".to_owned(), "inspect".to_owned(), image.clone()];
    if run_capture(&inspect, QUICK).is_none() {
        println!("Pulling {image} ...");
        if !run_visible(&["pull".into(), image.clone()])? {
            return Err(format!(
                "The hub image {image} isn't available yet. Set AGENTSCM_HUB_IMAGE, or connect to an existing hub."
            ));
        }
    }
    let up = compose_args(&dir, &["up", "-d", "--pull", "never"]);
    if !run_visible(&up)? {
        return Err("Docker could not start the hub (is port 7332 already in use?).".into());
    }
    wait_healthy().await?;

    let ls = ["sh", "-c", "ls /data/secrets/con_*.json 2>/dev/null"];
    let listed = || connection_ids(&run_capture(&exec_args(&dir, &ls), QUICK).unwrap_or_default());
    if listed().is_empty() {
        // The hub writes its one-time link when it starts; a restart makes sure it is fresh.
        println!("Restarting the hub so its first-run link is fresh ...");
        if !run_visible(&compose_args(&dir, &FORCE_RECREATE))? {
            return Err("Docker could not restart the hub.".into());
        }
        wait_healthy().await?;
    }
    let link = run_capture(
        &exec_args(&dir, &["cat", &format!("{SECRETS}/bootstrap-setup-url")]),
        QUICK,
    )
    .filter(|url| url.starts_with(&format!("{URL}/")) && !url.chars().any(char::is_control));
    let mut connection = listed();
    if let Some(link) = link {
        println!(
            "\nFirst run: the hub needs a GitHub App. Open this link (it works once, for 10 minutes):\n  {link}"
        );
        println!(
            "Create the app on GitHub, then on the success page click Install and choose your repository."
        );
        if tty {
            open_url(&link);
        }
        let deadline = Instant::now() + Duration::from_secs(600);
        while connection.is_empty() {
            if Instant::now() >= deadline {
                return Err("GitHub App setup wasn't finished in 10 minutes. Run quickstart again to continue.".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            connection = listed();
        }
        let repo = repo.unwrap_or("your repository");
        ui.pause(&format!(
            "On the GitHub page, click Install and pick {repo}, then press Enter."
        ));
    }
    let Some(con) = connection.first() else {
        return Ok(Started { install_url: None });
    };
    let app: Value = run_capture(
        &exec_args(&dir, &["cat", &format!("{SECRETS}/{con}.json")]),
        QUICK,
    )
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or_default();
    let install_url = app["slug"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        })
        .map(|slug| format!("https://github.com/apps/{slug}/installations/new"));
    let client_id = app["client_id"].as_str().unwrap_or_default();
    let current_env = std::fs::read_to_string(&env).unwrap_or_default();
    if !client_id.is_empty() && !env_has(&current_env, "AGENTSCM_ADMINS") {
        let login = match preset {
            Some(login) => login,
            None => github_login(login, ui)?,
        };
        let secret_file = format!("{SECRETS}/{con}.client_secret");
        let admins = format!("github:{login}");
        let mut all = updates;
        all.extend([
            ("AGENTSCM_GITHUB_CLIENT_ID", client_id),
            ("AGENTSCM_GITHUB_CLIENT_SECRET_FILE", secret_file.as_str()),
            ("AGENTSCM_ADMINS", admins.as_str()),
        ]);
        write(&env, &env_text(&current_env, &all, &[])?)?;
        println!("Restarting the hub with sign-in enabled ...");
        if !run_visible(&up)? {
            return Err("Docker could not restart the hub with sign-in enabled.".into());
        }
        wait_healthy().await?;
    }
    Ok(Started { install_url })
}

async fn wait_healthy() -> Result<(), String> {
    if super::wait_healthy(URL, Duration::from_secs(60)).await {
        Ok(())
    } else {
        Err(format!(
            "The hub at {URL} didn't become healthy within 60 seconds."
        ))
    }
}

/// `docker compose down`, with `-v` to delete the hub's data as well.
pub(crate) fn down(dir: &Path, volumes: bool) -> bool {
    let mut rest = vec!["down"];
    if volumes {
        rest.push("-v");
    }
    run_visible(&compose_args(dir, &rest)).unwrap_or(false)
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
            compose_args(dir, &["up", "-d", "--pull", "never"]).join(" "),
            "compose -p agentscm-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env up -d --pull never"
        );
        assert_eq!(
            compose_args(dir, &FORCE_RECREATE).join(" "),
            "compose -p agentscm-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env up -d --pull never --force-recreate"
        );
        assert_eq!(
            exec_args(dir, &["cat", "/data/secrets/bootstrap-setup-url"]).join(" "),
            "compose -p agentscm-hub -f /h/.agentscm/hub/compose.yaml --env-file /h/.agentscm/hub/.env exec -T hub cat /data/secrets/bootstrap-setup-url"
        );
    }

    #[test]
    fn finds_connection_files() {
        let listing = "/data/secrets/con_0a1b.json\n/data/secrets/con_0a1b.client_secret\n/data/secrets/other.json\n/data/secrets/con_x;y.json\n";
        assert_eq!(connection_ids(listing), ["con_0a1b"]);
        assert!(login_ok("octo-cat") && !login_ok("a b") && !login_ok(""));
    }
}
