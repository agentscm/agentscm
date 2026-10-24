//! A hub on this machine in Docker. Docker itself cannot run in CI, so the commands and
//! files are built by small pure functions (unit tested); `start` only runs them in order.
use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agentscm::hub::slack::setup::{bundle_file_name, bundle_file_names};

use super::HubRuntime;
use super::SlackBundle;
use super::detect::which;
use super::hub_setup::{self, Runtime, Started};
use super::manifest::{LocalHubRecord, Manifest};
use super::process::{Output, diagnostic, run};
use crate::hub_cli::write_private;
use crate::prompt::Ui;

const SECRETS: &str = "/data/secrets";
const QUICK: Duration = Duration::from_secs(20);
const SLACK_HELPER_LABEL: &str = "com.counterbranch.agentscm.slack-helper";
const SLACK_PROJECT_LABEL: &str = "com.counterbranch.agentscm.slack-project";
static SLACK_HELPER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Options {
    pub(crate) runtime: HubRuntime,
    pub(crate) port: u16,
    pub(crate) project: String,
    pub(crate) slack: Option<SlackBundle>,
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
      AGENTSCM_SLACK_FILE: ${AGENTSCM_SLACK_FILE:-}
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

fn staged_env_text(
    existing: &str,
    updates: &[(&str, &str)],
    stale: &[&str],
    slack_names: Option<&str>,
) -> Result<(String, String), String> {
    let before_volume = env_text(existing, updates, stale)?;
    let published = if let Some(names) = slack_names {
        env_text(
            &before_volume,
            &[
                ("AGENTSCM_SLACK_FILE", SLACK_CONFIG),
                (SLACK_FILES_ENV, names),
            ],
            &[],
        )?
    } else {
        before_volume.clone()
    };
    Ok((before_volume, published))
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
        let temp = dir.join(format!(".agentscm-quickstart-{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        write_private(&temp, text).map_err(|e| e.to_string())?;
        super::sync_file(&temp)?;
        std::fs::rename(&temp, path).map_err(|error| error.to_string())?;
        return super::sync_dir(dir);
    }
    Err("quickstart setup file has no parent".into())
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
                let _ = restore_file(compose, Some(&bytes));
            }
            None => {
                let _ = std::fs::remove_file(compose);
            }
        }
        match old_env {
            Some(bytes) => {
                let _ = restore_file(env, Some(&bytes));
            }
            None => {
                let _ = std::fs::remove_file(env);
            }
        }
        return Err(error);
    }
    Ok(())
}

fn restore_file(path: &Path, prior: Option<&Vec<u8>>) -> Result<(), String> {
    match prior {
        Some(bytes) => write(
            path,
            std::str::from_utf8(bytes)
                .map_err(|_| "The previous Docker setup file is not UTF-8.".to_owned())?,
        ),
        None => match std::fs::remove_file(path) {
            Ok(()) => super::sync_dir(path.parent().ok_or("Docker setup file has no parent")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

const SLACK_CONFIG: &str = "/data/secrets/slack/config.json";
const SLACK_FILES_ENV: &str = "AGENTSCM_SLACK_BUNDLE_FILES";
const SLACK_RECEIPT: &str = ".slack-bundle-receipt";
const SLACK_ROLLBACK_RECEIPT: &str = ".slack-bundle-receipt.rollback";

const VERIFY_SLACK: &str = r#"set -eu
target=/data/secrets/slack
receipt="$target/.agentscm-receipt"
[ -f /run/agentscm-slack/.agentscm-expected-receipt ] && [ ! -L /run/agentscm-slack/.agentscm-expected-receipt ]
[ "$(stat -c %a /run/agentscm-slack/.agentscm-expected-receipt)" = 600 ]
[ -d "$target" ] && [ ! -L "$target" ] && [ -f "$receipt" ] && [ ! -L "$receipt" ]
[ "$(stat -c %u "$target")" = 10001 ] && [ "$(stat -c %a "$target")" = 700 ]
[ "$(stat -c %u "$receipt")" = 10001 ] && [ "$(stat -c %a "$receipt")" = 600 ]
cd "$target"
cmp -s /run/agentscm-slack/.agentscm-expected-receipt .agentscm-receipt
sha256sum -c .agentscm-receipt >/dev/null
[ "$(wc -l < .agentscm-receipt | tr -d ' ')" = "$#" ]
for file in "$@"; do
  [ -f "$file" ] && [ ! -L "$file" ]
  [ "$(stat -c %u "$file")" = 10001 ] && [ "$(stat -c %a "$file")" = 600 ]
done
for entry in ./* ./.[!.]* ./..?*; do
  [ -e "$entry" ] || continue
  name="${entry#./}"
  case "$name" in
    .agentscm-receipt) ;;
    install.sqlite|install.sqlite-wal|install.sqlite-shm)
      [ -f "$entry" ] && [ ! -L "$entry" ]
      [ "$(stat -c %u "$entry")" = 10001 ] && [ "$(( 0$(stat -c %a "$entry") & 077 ))" = 0 ] ;;
    *)
      [ -f "$entry" ] && [ ! -L "$entry" ]
      case "$name" in
        config.json|signing-secret|bot-token) ;;
        agent-*.token) index="${name#agent-}"; index="${index%.token}"; case "$index" in ''|*[!0-9]*) exit 1;; esac ;;
        *) exit 1 ;;
      esac
      awk -v wanted="$name" '$2 == wanted { found=1 } END { exit !found }' .agentscm-receipt ;;
  esac
done
"#;

const CHECK_SLACK: &str = r#"set -eu
source=/run/agentscm-slack
target=/data/secrets/slack
receipt="$target/.agentscm-receipt"
if [ ! -d "$target" ] || [ -L "$target" ] || [ ! -f "$receipt" ] || [ -L "$receipt" ]; then
  printf 'agentscm-slack-changed\n'; exit 0
fi
cd "$target"
same=true
for file in "$@"; do
  [ -f "$source/$file" ] && [ ! -L "$source/$file" ]
  if [ ! -f "$file" ] || [ -L "$file" ] || ! cmp -s "$source/$file" "$file"; then same=false; fi
done
if [ "$same" = true ] && [ "$(wc -l < .agentscm-receipt | tr -d ' ')" = "$#" ]; then
  printf 'agentscm-slack-unchanged\n'
else
  printf 'agentscm-slack-changed\n'
fi
"#;

const CHECK_SLACK_PENDING: &str = r#"set -eu
if [ ! -e /data/secrets ]; then printf 'agentscm-slack-clean\n'; exit 0; fi
[ -d /data/secrets ] && [ ! -L /data/secrets ]
[ "$(stat -c %u /data/secrets)" = 10001 ] && [ "$(stat -c %a /data/secrets)" = 700 ]
for pending in /data/secrets/.slack-rollback /data/secrets/.slack-next /data/secrets/.slack-rollback-next; do
  if [ -e "$pending" ]; then
    [ -d "$pending" ] && [ ! -L "$pending" ]
    [ "$(stat -c %u "$pending")" = 10001 ] && [ "$(stat -c %a "$pending")" = 700 ]
    printf 'agentscm-slack-pending\n'; exit 0
  fi
done
printf 'agentscm-slack-clean\n'
"#;

const VALIDATE_SLACK_TXN: &str = r#"valid_leaf() {
  case "$1" in
    config.json|signing-secret|bot-token) return 0 ;;
    agent-*.token)
      index="${1#agent-}"; index="${index%.token}"
      case "$index" in ''|*[!0-9]*) return 1;; esac
      [ "$index" = "$(printf '%s' "$index" | sed 's/^0*//;s/^$/0/')" ] ;;
    *) return 1 ;;
  esac
}
private_file() {
  [ -f "$1" ] && [ ! -L "$1" ]
  [ "$(stat -c %u "$1")" = 10001 ] && [ "$(stat -c %a "$1")" = 600 ]
}
private_dir() {
  [ -d "$1" ] && [ ! -L "$1" ]
  [ "$(stat -c %u "$1")" = 10001 ] && [ "$(stat -c %a "$1")" = 700 ]
}
validate_list() {
  list="$1"
  private_file "$list"
  count="$(wc -l < "$list" | tr -d ' ')"
  [ "$count" -ge 4 ] && [ "$count" -le 35 ]
  [ -z "$(sort "$list" | uniq -d)" ]
  while IFS= read -r name; do valid_leaf "$name"; done < "$list"
  for required in config.json signing-secret bot-token; do grep -Fqx "$required" "$list"; done
  agents=$((count - 3)); index=0
  while [ "$index" -lt "$agents" ]; do grep -Fqx "agent-$index.token" "$list"; index=$((index + 1)); done
}
validate_receipt() {
  receipt_file="$1"; list="$2"; payload="${3-}"
  private_file "$receipt_file"
  [ "$(wc -l < "$receipt_file" | tr -d ' ')" = "$(wc -l < "$list" | tr -d ' ')" ]
  seen=
  while read -r digest name extra; do
    [ -z "${extra-}" ] && [ "${#digest}" = 64 ]
    case "$digest" in *[!0-9a-f]*) return 1;; esac
    valid_leaf "$name" && grep -Fqx "$name" "$list"
    case "$seen" in *"|$name|"*) return 1;; esac
    seen="$seen|$name|"
    if [ -n "$payload" ]; then
      private_file "$payload/$name"
      [ "$(sha256sum "$payload/$name" | cut -d ' ' -f 1)" = "$digest" ]
    fi
  done < "$receipt_file"
}
validate_target_entries() {
  target="$1"; first="$2"; second="${3-}"
  private_dir "$target"
  for entry in "$target"/* "$target"/.[!.]* "$target"/..?*; do
    [ -e "$entry" ] || continue
    name="${entry##*/}"
    case "$name" in
      .agentscm-receipt) private_file "$entry" ;;
      install.sqlite|install.sqlite-wal|install.sqlite-shm)
        [ -f "$entry" ] && [ ! -L "$entry" ]
        [ "$(stat -c %u "$entry")" = 10001 ] && [ "$(( 0$(stat -c %a "$entry") & 077 ))" = 0 ] ;;
      *)
        valid_leaf "$name" && private_file "$entry"
        grep -Fqx "$name" "$first" || { [ -n "$second" ] && grep -Fqx "$name" "$second"; } ;;
    esac
  done
}
receipt_has_name() {
  wanted="$1"; receipt_file="$2"
  while read -r digest name extra; do [ "$name" != "$wanted" ] || return 0; done < "$receipt_file"
  return 1
}
validate_live_target() {
  target="$1"; receipt_file="$target/.agentscm-receipt"
  private_dir "$target"; private_file "$receipt_file"
  count="$(wc -l < "$receipt_file" | tr -d ' ')"
  [ "$count" -ge 4 ] && [ "$count" -le 35 ]
  seen=
  while read -r digest name extra; do
    [ -z "${extra-}" ] && [ "${#digest}" = 64 ]
    case "$digest" in *[!0-9a-f]*) return 1;; esac
    valid_leaf "$name"
    case "$seen" in *"|$name|"*) return 1;; esac
    seen="$seen|$name|"
    private_file "$target/$name"
    [ "$(sha256sum "$target/$name" | cut -d ' ' -f 1)" = "$digest" ]
  done < "$receipt_file"
  for required in config.json signing-secret bot-token; do receipt_has_name "$required" "$receipt_file"; done
  agents=$((count - 3)); index=0
  while [ "$index" -lt "$agents" ]; do receipt_has_name "agent-$index.token" "$receipt_file"; index=$((index + 1)); done
  for entry in "$target"/* "$target"/.[!.]* "$target"/..?*; do
    [ -e "$entry" ] || continue
    name="${entry##*/}"
    case "$name" in
      .agentscm-receipt) private_file "$entry" ;;
      install.sqlite|install.sqlite-wal|install.sqlite-shm)
        [ -f "$entry" ] && [ ! -L "$entry" ]
        [ "$(stat -c %u "$entry")" = 10001 ] && [ "$(( 0$(stat -c %a "$entry") & 077 ))" = 0 ] ;;
      *) valid_leaf "$name" && private_file "$entry" && receipt_has_name "$name" "$receipt_file" ;;
    esac
  done
}
validate_txn() {
  backup=/data/secrets/.slack-rollback
  private_dir /data/secrets; private_dir "$backup"
  for pending in /data/secrets/.slack-next /data/secrets/.slack-rollback-next; do
    [ ! -e "$pending" ] || private_dir "$pending"
  done
  for required in ready old-files new-files new-receipt; do private_file "$backup/$required"; done
  if [ -e "$backup/rollback" ]; then private_file "$backup/rollback"; else private_file "$backup/rollback-marker"; fi
  if [ -e "$backup/commit" ]; then private_file "$backup/commit"; else private_file "$backup/commit-marker"; fi
  validate_list "$backup/new-files"
  validate_receipt "$backup/new-receipt" "$backup/new-files"
  if [ -s "$backup/old-files" ]; then
    validate_list "$backup/old-files"
    validate_receipt "$backup/.agentscm-receipt" "$backup/old-files" "$backup"
    private_file "$backup/had-receipt"
  else
    private_file "$backup/old-files"
    [ ! -e "$backup/.agentscm-receipt" ] && [ ! -e "$backup/had-receipt" ]
  fi
  for marker in rollback commit; do [ ! -e "$backup/$marker" ] || private_file "$backup/$marker"; done
  for entry in "$backup"/* "$backup"/.[!.]* "$backup"/..?*; do
    [ -e "$entry" ] || continue
    name="${entry##*/}"
    case "$name" in
      ready|old-files|new-files|new-receipt|.agentscm-receipt|had-receipt|rollback|commit|rollback-marker|commit-marker) private_file "$entry" ;;
      *) valid_leaf "$name" && grep -Fqx "$name" "$backup/old-files" && private_file "$entry" ;;
    esac
  done
}
"#;

const LOCK_SLACK_MUTATION: &str = r#"if [ -e /data/secrets ]; then
  private_dir /data/secrets
else
  mkdir -m 700 /data/secrets
  chown 10001:10001 /data/secrets
fi
lock=/data/secrets/.agentscm-slack.lock
if [ -e "$lock" ]; then private_file "$lock"; else : > "$lock"; chmod 600 "$lock"; chown 10001:10001 "$lock"; fi
exec 9>"$lock"
flock -x 9
"#;

const INSTALL_SLACK: &str = r#"set -eu
source=/run/agentscm-slack
target=/data/secrets/slack
receipt="$target/.agentscm-receipt"
next=/data/secrets/.slack-next
backup=/data/secrets/.slack-rollback
backup_stage=/data/secrets/.slack-rollback-next
if [ -e /data/secrets ]; then
  [ -d /data/secrets ] && [ ! -L /data/secrets ]
  [ "$(stat -c %u /data/secrets)" = 10001 ] && [ "$(stat -c %a /data/secrets)" = 700 ]
else
  mkdir -m 700 /data/secrets
  chown 10001:10001 /data/secrets
fi
if [ -e "$target" ]; then
  validate_live_target "$target"
  cd "$target"
  same=true
  for file in "$@"; do
    [ -f "$source/$file" ] && [ ! -L "$source/$file" ]
    if [ ! -f "$file" ] || [ -L "$file" ] || ! cmp -s "$source/$file" "$file"; then same=false; fi
  done
  if [ "$same" = true ] && [ "$(wc -l < .agentscm-receipt | tr -d ' ')" = "$#" ]; then
    printf 'agentscm-slack-unchanged\n'
    exit 0
  fi
fi
[ ! -e "$next" ] && [ ! -e "$backup" ] && [ ! -e "$backup_stage" ]
mkdir -m 700 "$next"
chown 10001:10001 "$next"
rollback() {
  validate_txn
  [ -e "$backup/rollback" ] || mv "$backup/rollback-marker" "$backup/rollback"
  sync -f "$backup"
  [ ! -e "$target" ] || validate_target_entries "$target" "$backup/new-files" "$backup/old-files"
  if [ ! -e "$target" ]; then mkdir -m 700 "$target"; chown 10001:10001 "$target"; fi
  if [ -f "$backup/new-files" ]; then
    while read -r file; do rm -f "$target/$file"; done < "$backup/new-files"
  fi
  if [ -f "$backup/old-files" ]; then
    while read -r file; do rm -f "$target/$file"; cp "$backup/$file" "$target/$file"; chmod 600 "$target/$file"; chown 10001:10001 "$target/$file"; done < "$backup/old-files"
  fi
  if [ -e "$backup/.agentscm-receipt" ]; then rm -f "$target/.agentscm-receipt"; cp "$backup/.agentscm-receipt" "$target/.agentscm-receipt"; chmod 600 "$target/.agentscm-receipt"; chown 10001:10001 "$target/.agentscm-receipt"; elif [ ! -e "$backup/had-receipt" ]; then rm -f "$target/.agentscm-receipt"; fi
  rm -rf "$next"
  # A first install's rollback also drops the journal its Hub wrote, so a retry finds no target.
  if [ ! -s "$backup/old-files" ]; then rm -f "$target/install.sqlite" "$target/install.sqlite-wal" "$target/install.sqlite-shm"; fi
  if [ -d "$target" ] && [ -z "$(find "$target" -mindepth 1 -maxdepth 1 -print -quit)" ]; then rmdir "$target"; fi
  sync -f /data/secrets
  [ ! -d "$target" ] || sync -f "$target"
}
cleanup_unready() {
  rm -rf "$next" "$backup_stage"
}
trap cleanup_unready EXIT HUP INT TERM
for file in "$@"; do
  [ -f "$source/$file" ] && [ ! -L "$source/$file" ]
  cp "$source/$file" "$next/$file"
  chmod 600 "$next/$file"
  chown 10001:10001 "$next/$file"
done
cd "$next"
sha256sum "$@" > .agentscm-receipt
chmod 600 .agentscm-receipt
chown 10001:10001 .agentscm-receipt
chown 10001:10001 "$next"
mkdir -m 700 "$backup_stage"
chown 10001:10001 "$backup_stage"
printf '%s\n' "$@" > "$backup_stage/new-files"
: > "$backup_stage/old-files"
if [ -e "$target" ]; then
  while read -r digest file; do printf '%s\n' "$file" >> "$backup_stage/old-files"; done < "$receipt"
  cp "$receipt" "$backup_stage/.agentscm-receipt"
  while read -r file; do cp "$target/$file" "$backup_stage/$file"; chmod 600 "$backup_stage/$file"; chown 10001:10001 "$backup_stage/$file"; done < "$backup_stage/old-files"
fi
cp "$next/.agentscm-receipt" "$backup_stage/new-receipt"
chmod 600 "$backup_stage/new-files" "$backup_stage/old-files"
if [ -f "$receipt" ]; then : > "$backup_stage/had-receipt"; chmod 600 "$backup_stage/had-receipt"; fi
: > "$backup_stage/ready"
: > "$backup_stage/rollback-marker"
: > "$backup_stage/commit-marker"
chmod 600 "$backup_stage/ready" "$backup_stage/new-receipt" "$backup_stage/rollback-marker" "$backup_stage/commit-marker"
chown -R 10001:10001 "$backup_stage"
sync -f "$next"
sync -f "$backup_stage"
mv "$backup_stage" "$backup"
sync -f /data/secrets
validate_txn
trap rollback EXIT HUP INT TERM
if [ ! -e "$target" ]; then mkdir -m 700 "$target"; chown 10001:10001 "$target"; fi
for file in "$@"; do mv "$next/$file" "$target/$file"; done
while read -r file; do
  if ! receipt_has_name "$file" "$backup/new-receipt"; then rm -f "$target/$file"; fi
done < "$backup/old-files"
mv "$next/.agentscm-receipt" "$target/.agentscm-receipt"
rmdir "$next"
sync -f "$target"
sync -f /data/secrets
trap - EXIT HUP INT TERM
cd "$target"
sha256sum -c .agentscm-receipt >/dev/null
for file in "$@" .agentscm-receipt; do
  [ "$(stat -c %u "$file")" = 10001 ] && [ "$(stat -c %a "$file")" = 600 ]
done
printf 'agentscm-slack-changed\n'
"#;

const ROLLBACK_SLACK: &str = r#"set -eu
target=/data/secrets/slack
backup=/data/secrets/.slack-rollback
next=/data/secrets/.slack-next
backup_stage=/data/secrets/.slack-rollback-next
if [ ! -e "$backup" ]; then
  if [ ! -e /data/secrets ]; then printf 'agentscm-slack-clean\n'; exit 0; fi
  private_dir /data/secrets
  for pending in "$next" "$backup_stage"; do
    [ ! -e "$pending" ] || private_dir "$pending"
  done
  rm -rf "$next" "$backup_stage"
  sync -f /data/secrets
  printf 'agentscm-slack-clean\n'
  exit 0
fi
validate_txn
[ ! -e "$target" ] || validate_target_entries "$target" "$backup/new-files" "$backup/old-files"
[ -e "$backup/rollback" ] || mv "$backup/rollback-marker" "$backup/rollback"
sync -f "$backup"
if [ ! -e "$target" ]; then mkdir -m 700 "$target"; chown 10001:10001 "$target"; fi
if [ -f "$backup/new-files" ]; then while read -r file; do rm -f "$target/$file"; done < "$backup/new-files"; fi
if [ -f "$backup/old-files" ]; then while read -r file; do rm -f "$target/$file"; cp "$backup/$file" "$target/$file"; chmod 600 "$target/$file"; chown 10001:10001 "$target/$file"; done < "$backup/old-files"; fi
if [ -e "$backup/.agentscm-receipt" ]; then rm -f "$target/.agentscm-receipt"; cp "$backup/.agentscm-receipt" "$target/.agentscm-receipt"; chmod 600 "$target/.agentscm-receipt"; chown 10001:10001 "$target/.agentscm-receipt"; elif [ ! -e "$backup/had-receipt" ]; then rm -f "$target/.agentscm-receipt"; fi
rm -rf /data/secrets/.slack-next
if [ ! -s "$backup/old-files" ]; then rm -f "$target/install.sqlite" "$target/install.sqlite-wal" "$target/install.sqlite-shm"; fi
if [ -d "$target" ] && [ -z "$(find "$target" -mindepth 1 -maxdepth 1 -print -quit)" ]; then rmdir "$target"; fi
[ ! -d "$target" ] || sync -f "$target"
sync -f /data/secrets
printf 'agentscm-slack-rollback\n'
"#;

const VERIFY_NO_SLACK: &str = r#"set -eu
target=/data/secrets/slack
[ -e "$target" ] || exit 0
[ -d "$target" ] && [ ! -L "$target" ]
[ "$(stat -c %u "$target")" = 10001 ] && [ "$(stat -c %a "$target")" = 700 ]
for entry in "$target"/* "$target"/.[!.]* "$target"/..?*; do
  [ -e "$entry" ] || continue
  name="${entry##*/}"
  case "$name" in
    install.sqlite|install.sqlite-wal|install.sqlite-shm)
      [ -f "$entry" ] && [ ! -L "$entry" ]
      [ "$(stat -c %u "$entry")" = 10001 ] && [ "$(( 0$(stat -c %a "$entry") & 077 ))" = 0 ] ;;
    *) exit 1 ;;
  esac
done
"#;

const MARK_SLACK_COMMIT: &str = r#"set -eu
backup=/data/secrets/.slack-rollback
target=/data/secrets/slack
validate_txn
[ ! -e "$backup/rollback" ]
validate_target_entries "$target" "$backup/new-files"
validate_receipt "$target/.agentscm-receipt" "$backup/new-files" "$target"
cmp -s /run/agentscm-slack/.agentscm-expected-receipt "$target/.agentscm-receipt"
sync -f "$target"
[ -e "$backup/commit" ] || mv "$backup/commit-marker" "$backup/commit"
sync -f "$backup"
"#;

const COMMIT_SLACK: &str = r#"set -eu
backup=/data/secrets/.slack-rollback
target=/data/secrets/slack
validate_txn
[ ! -e "$backup/rollback" ] && private_file "$backup/commit"
validate_target_entries "$target" "$backup/new-files"
validate_receipt "$target/.agentscm-receipt" "$backup/new-files" "$target"
cmp -s /run/agentscm-slack/.agentscm-expected-receipt "$target/.agentscm-receipt"
rm -rf "$backup"
sync -f /data/secrets
"#;

const RECONCILE_SLACK_COMMIT: &str = r#"set -eu
backup=/data/secrets/.slack-rollback
target=/data/secrets/slack
next=/data/secrets/.slack-next
backup_stage=/data/secrets/.slack-rollback-next
if [ ! -e "$backup" ]; then
  [ ! -e "$next" ] && [ ! -e "$backup_stage" ]
  validate_live_target "$target"
  cmp -s /run/agentscm-slack/.agentscm-expected-receipt "$target/.agentscm-receipt"
  printf 'agentscm-slack-commit-clean\n'; exit 0
fi
validate_txn
[ ! -e "$backup/rollback" ] && private_file "$backup/commit"
validate_target_entries "$target" "$backup/new-files"
validate_receipt "$target/.agentscm-receipt" "$backup/new-files" "$target"
cmp -s "$backup/new-receipt" "$target/.agentscm-receipt"
cmp -s /run/agentscm-slack/.agentscm-expected-receipt "$target/.agentscm-receipt"
rm -rf "$backup" "$next" "$backup_stage"
sync -f /data/secrets
printf 'agentscm-slack-commit\n'
"#;

const RECOVER_SLACK: &str = r#"set -eu
backup=/data/secrets/.slack-rollback
next=/data/secrets/.slack-next
backup_stage=/data/secrets/.slack-rollback-next
if [ ! -e "$backup" ]; then
  for pending in "$next" "$backup_stage"; do
    if [ -e "$pending" ]; then private_dir "$pending"; rm -rf "$pending"; fi
  done
  sync -f /data/secrets
  printf 'agentscm-slack-clean\n'; exit 0
fi
validate_txn
if [ ! -e "$backup/rollback" ] && [ -e "$backup/commit" ]; then
  target=/data/secrets/slack
  validate_target_entries "$target" "$backup/new-files"
  validate_receipt "$target/.agentscm-receipt" "$backup/new-files" "$target"
  cmp -s "$backup/new-receipt" "$target/.agentscm-receipt"
  cmp -s /run/agentscm-slack/.agentscm-expected-receipt "$target/.agentscm-receipt"
  rm -rf "$backup" "$next" "$backup_stage"
  sync -f /data/secrets
  printf 'agentscm-slack-commit\n'; exit 0
fi
target=/data/secrets/slack
old=
if [ -f "$backup/old-files" ]; then while read -r file; do old="${old:+$old,}$file"; done < "$backup/old-files"; fi
[ ! -e "$target" ] || validate_target_entries "$target" "$backup/new-files" "$backup/old-files"
[ -e "$backup/rollback" ] || mv "$backup/rollback-marker" "$backup/rollback"
sync -f "$backup"
if [ ! -e "$target" ]; then mkdir -m 700 "$target"; chown 10001:10001 "$target"; fi
if [ -f "$backup/new-files" ]; then while read -r file; do rm -f "$target/$file"; done < "$backup/new-files"; fi
if [ -f "$backup/old-files" ]; then while read -r file; do rm -f "$target/$file"; cp "$backup/$file" "$target/$file"; chmod 600 "$target/$file"; chown 10001:10001 "$target/$file"; done < "$backup/old-files"; fi
if [ -e "$backup/.agentscm-receipt" ]; then rm -f "$target/.agentscm-receipt"; cp "$backup/.agentscm-receipt" "$target/.agentscm-receipt"; chmod 600 "$target/.agentscm-receipt"; chown 10001:10001 "$target/.agentscm-receipt"; elif [ ! -e "$backup/had-receipt" ]; then rm -f "$target/.agentscm-receipt"; fi
rm -rf "$next" "$backup_stage"
if [ ! -s "$backup/old-files" ]; then rm -f "$target/install.sqlite" "$target/install.sqlite-wal" "$target/install.sqlite-shm"; fi
if [ -d "$target" ] && [ -z "$(find "$target" -mindepth 1 -maxdepth 1 -print -quit)" ]; then rmdir "$target"; fi
[ ! -d "$target" ] || sync -f "$target"
sync -f /data/secrets
printf 'agentscm-slack-rollback:%s\n' "$old"
"#;

const FINALIZE_SLACK_ROLLBACK: &str = r#"set -eu
backup=/data/secrets/.slack-rollback
validate_txn
private_file "$backup/rollback"
rm -rf "$backup" /data/secrets/.slack-next
sync -f /data/secrets
"#;

/// Exactly the files of a bundle with 1 to 32 bindings, in any order and without repeats.
fn valid_slack_files(files: &[String]) -> bool {
    (4..=35).contains(&files.len())
        && files.iter().cloned().collect::<BTreeSet<_>>()
            == bundle_file_names(files.len() - 3).collect()
}

fn slack_run_args(
    dir: &Path,
    project: &str,
    stage: Option<&Path>,
    user: &str,
    mutating: bool,
    script: &str,
    files: &[String],
) -> Result<Vec<String>, String> {
    let script = if mutating {
        format!("set -eu\n{VALIDATE_SLACK_TXN}\n{LOCK_SLACK_MUTATION}\n{script}")
    } else {
        script.to_owned()
    };
    let sequence = SLACK_HELPER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let helper = format!("agentscm-slack-{}-{sequence}", std::process::id());
    let mut rest = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--no-deps".to_owned(),
        "--name".to_owned(),
        helper,
        "--label".to_owned(),
        format!("{SLACK_HELPER_LABEL}=1"),
        "--label".to_owned(),
        format!("{SLACK_PROJECT_LABEL}={project}"),
        "--user".to_owned(),
        user.to_owned(),
        "--entrypoint".to_owned(),
        "/bin/sh".to_owned(),
        "-T".to_owned(),
    ];
    let reads_private_stage_as_root = stage.is_some() && user == "0:0";
    if mutating || reads_private_stage_as_root {
        // The Hub service drops every capability. A root helper needs DAC_OVERRIDE to cross
        // both the host-owned private stage and the UID 10001-owned volume boundary. Only a
        // mutating installer also needs CHOWN to assign copied files to the runtime UID.
        let mut capabilities = vec!["--cap-add".into(), "DAC_OVERRIDE".into()];
        if mutating {
            capabilities.extend(["--cap-add".into(), "CHOWN".into()]);
        }
        rest.splice(5..5, capabilities);
    }
    if let Some(stage) = stage {
        let stage = stage.display().to_string();
        if stage.contains(':') || stage.chars().any(char::is_control) {
            return Err("The Slack staging path is not usable as a Docker bind mount.".into());
        }
        rest.extend(["--volume".into(), format!("{stage}:/run/agentscm-slack:ro")]);
    }
    rest.extend(["hub".into(), "-c".into(), script, "agentscm-slack".into()]);
    rest.extend(files.iter().cloned());
    Ok(compose_args(
        dir,
        project,
        &rest.iter().map(String::as_str).collect::<Vec<_>>(),
    ))
}

fn helper_name(args: &[String]) -> Result<&str, String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == "--name").then_some(pair[1].as_str()))
        .ok_or_else(|| "The Docker Slack helper has no owned name.".into())
}

fn terminate_slack_helper(name: &str, project: &str) -> Result<(), String> {
    let inspect = run(
        std::process::Command::new(docker()?).args([
            "inspect",
            "--format",
            &format!(
                "{{{{ index .Config.Labels \"{SLACK_HELPER_LABEL}\" }}}}|{{{{ index .Config.Labels \"{SLACK_PROJECT_LABEL}\" }}}}"
            ),
            name,
        ]),
        QUICK,
    )?;
    if !inspect.success {
        return confirm_slack_helper_absent(name);
    }
    if inspect.stdout.trim() != format!("1|{project}") {
        return Err(
            "A Docker container using the Slack helper name is not owned by this setup.".into(),
        );
    }
    let removed = run(
        std::process::Command::new(docker()?).args(["rm", "-f", name]),
        QUICK,
    )?;
    if !removed.success {
        return Err("Docker could not stop an owned Slack helper container.".into());
    }
    confirm_slack_helper_absent(name)
}

fn confirm_slack_helper_absent(identifier: &str) -> Result<(), String> {
    let filter = if identifier.starts_with("agentscm-slack-") {
        format!("name=^/{identifier}$")
    } else if (12..=64).contains(&identifier.len())
        && identifier.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        format!("id={identifier}")
    } else {
        return Err("The Docker Slack helper identifier is invalid.".into());
    };
    let listing = run(
        std::process::Command::new(docker()?).args([
            "ps",
            "-aq",
            "--no-trunc",
            "--filter",
            &filter,
        ]),
        QUICK,
    )?;
    if !listing.success {
        return Err("Docker could not confirm that the Slack helper stopped.".into());
    }
    if let Some(id) = listing
        .stdout
        .lines()
        .map(str::trim)
        .find(|id| !id.is_empty())
    {
        if !(12..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Docker returned an invalid Slack helper container id.".into());
        }
        return Err("An owned Docker Slack helper is still running.".into());
    }
    Ok(())
}

fn quiesce_slack_helpers(project: &str) -> Result<(), String> {
    let output = run(
        std::process::Command::new(docker()?).args([
            "ps",
            "-aq",
            "--filter",
            &format!("label={SLACK_HELPER_LABEL}=1"),
            "--filter",
            &format!("label={SLACK_PROJECT_LABEL}={project}"),
        ]),
        QUICK,
    )?;
    if !output.success {
        return Err("Docker could not inspect owned Slack helper containers.".into());
    }
    for id in output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if !(12..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Docker returned an invalid Slack helper container id.".into());
        }
        terminate_slack_helper(id, project)?;
    }
    Ok(())
}

fn run_slack(project: &str, args: &[String], deadline: Duration) -> Result<Output, String> {
    quiesce_slack_helpers(project)?;
    let name = helper_name(args)?.to_owned();
    let output = match run(std::process::Command::new(docker()?).args(args), deadline) {
        Ok(output) => output,
        Err(error) => {
            return match terminate_slack_helper(&name, project) {
                Ok(()) => Err(error),
                Err(termination) => Err(format!(
                    "{error}; the Docker Slack helper termination could not be confirmed: {termination}"
                )),
            };
        }
    };
    if !output.success {
        terminate_slack_helper(&name, project).map_err(|error| {
            format!(
                "The Docker Slack helper failed, and its termination could not be confirmed: {error}"
            )
        })?;
    }
    Ok(output)
}

fn run_slack_visible(project: &str, args: &[String]) -> Result<bool, String> {
    let output = run_slack(project, args, Duration::from_secs(60))?;
    if !output.success && !diagnostic(&output).is_empty() {
        eprintln!("{}", diagnostic(&output));
    }
    Ok(output.success)
}

fn cleanup_slack_stages(dir: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| "The Docker Hub directory contains a non-UTF-8 name.".to_owned())?;
        let Some(suffix) = name.strip_prefix(".slack-stage-") else {
            continue;
        };
        if suffix.len() != 24
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        // SAFETY: getuid has no preconditions and only reads the current process user id.
        let owned = metadata.uid() == unsafe { libc::getuid() }
            && metadata.permissions().mode() & 0o777 == 0o700;
        #[cfg(not(unix))]
        let owned = true;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || !owned {
            return Err("A retained Docker Slack staging path is not privately owned.".into());
        }
        std::fs::remove_dir_all(entry.path()).map_err(|error| error.to_string())?;
    }
    super::sync_dir(dir)
}

fn slack_receipt_path(dir: &Path) -> PathBuf {
    dir.join(SLACK_RECEIPT)
}

fn slack_rollback_receipt_path(dir: &Path) -> PathBuf {
    dir.join(SLACK_ROLLBACK_RECEIPT)
}

fn slack_bundle_receipt(bundle: &SlackBundle) -> Vec<u8> {
    bundle
        .files
        .iter()
        .flat_map(|(name, bytes)| {
            format!("{}  {name}\n", super::manifest::sha_hex(bytes)).into_bytes()
        })
        .collect()
}

fn validate_host_slack_receipt(receipt: &[u8]) -> Result<Vec<String>, String> {
    let text = std::str::from_utf8(receipt)
        .map_err(|_| "The Docker Slack digest receipt is not UTF-8.".to_owned())?;
    let mut files = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2
            || fields[0].len() != 64
            || !fields[0]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !bundle_file_name(fields[1])
        {
            return Err("The Docker Slack digest receipt is invalid.".into());
        }
        files.push(fields[1].to_owned());
    }
    valid_slack_files(&files)
        .then_some(files)
        .ok_or_else(|| "The Docker Slack digest receipt has an invalid file set.".into())
}

fn read_private_receipt(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    #[cfg(unix)]
    // SAFETY: getuid has no preconditions and only reads the current process user id.
    let private = metadata.permissions().mode() & 0o777 == 0o600
        && metadata.uid() == unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let private = true;
    if !metadata.is_file() || metadata.file_type().is_symlink() || !private {
        return Err("The recorded Docker Slack receipt is not a private regular file.".into());
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn read_slack_receipt(dir: &Path) -> Result<Option<Vec<u8>>, String> {
    read_private_receipt(&slack_receipt_path(dir))
}

fn write_slack_receipt(path: &Path, receipt: &[u8]) -> Result<(), String> {
    write(
        path,
        std::str::from_utf8(receipt)
            .map_err(|_| "The Docker Slack receipt is not UTF-8.".to_owned())?,
    )
}

fn remove_slack_receipt(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => super::sync_dir(path.parent().ok_or("Slack receipt has no parent")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn restore_slack_receipt(path: &Path, receipt: Option<&[u8]>) -> Result<(), String> {
    match receipt {
        Some(receipt) if !receipt.is_empty() => write_slack_receipt(path, receipt),
        Some(_) | None => remove_slack_receipt(path),
    }
}

fn prepare_slack_receipt(dir: &Path, prior: Option<&[u8]>) -> Result<(), String> {
    write_slack_receipt(&slack_rollback_receipt_path(dir), prior.unwrap_or_default())
}

fn publish_slack_receipt(dir: &Path, next: &[u8]) -> Result<(), String> {
    write_slack_receipt(&slack_receipt_path(dir), next)
}

fn finish_host_slack_receipt(dir: &Path) -> Result<(), String> {
    remove_slack_receipt(&slack_rollback_receipt_path(dir))
}

fn with_staged_slack_receipt<T>(
    dir: &Path,
    receipt: &[u8],
    operation: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    validate_host_slack_receipt(receipt)?;
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| "random source unavailable".to_owned())?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let stage = dir.join(format!(".slack-stage-{suffix}"));
    std::fs::create_dir(&stage).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let result = (|| {
        let text = std::str::from_utf8(receipt)
            .map_err(|_| "The Docker Slack receipt is not UTF-8.".to_owned())?;
        write(&stage.join(".agentscm-expected-receipt"), text)?;
        operation(&stage)
    })();
    if result.is_ok() {
        std::fs::remove_dir_all(&stage).map_err(|error| error.to_string())?;
        super::sync_dir(dir)?;
    }
    result
}

fn verify_slack_volume(dir: &Path, project: &str, files: &[String]) -> Result<(), String> {
    let receipt = read_slack_receipt(dir)?
        .ok_or_else(|| "The recorded Docker Slack digest receipt is missing.".to_owned())?;
    verify_slack_volume_with_receipt(dir, project, files, &receipt)
}

fn verify_slack_volume_with_receipt(
    dir: &Path,
    project: &str,
    files: &[String],
    receipt: &[u8],
) -> Result<(), String> {
    if validate_host_slack_receipt(receipt)? != files {
        return Err("The Docker Slack digest receipt does not match its recorded file set.".into());
    }
    with_staged_slack_receipt(dir, receipt, |stage| {
        run_slack_visible(
            project,
            &slack_run_args(dir, project, Some(stage), "0:0", false, VERIFY_SLACK, files)?,
        )?
        .then_some(())
        .ok_or_else(|| {
            "The recorded Docker Slack bundle is missing, unowned or has drifted.".into()
        })
    })
}

fn slack_volume_pending(dir: &Path, project: &str) -> Result<bool, String> {
    let args = slack_run_args(
        dir,
        project,
        None,
        "10001:10001",
        false,
        CHECK_SLACK_PENDING,
        &[],
    )?;
    let output = run_slack(project, &args, Duration::from_secs(60))?;
    if !output.success {
        return Err("Docker could not inspect the private Slack transaction state.".into());
    }
    if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-pending")
    {
        Ok(true)
    } else if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-clean")
    {
        Ok(false)
    } else {
        Err("Docker returned an invalid Slack transaction state.".into())
    }
}

fn finish_slack_volume_change(dir: &Path, project: &str) -> Result<(), String> {
    let receipt = read_slack_receipt(dir)?
        .ok_or_else(|| "The committed Docker Slack digest receipt is missing.".to_owned())?;
    with_staged_slack_receipt(dir, &receipt, |stage| {
        run_slack_visible(
            project,
            &slack_run_args(dir, project, Some(stage), "0:0", true, COMMIT_SLACK, &[])?,
        )?
        .then_some(())
        .ok_or_else(|| "Docker could not finalize the private Slack bundle update.".into())
    })?;
    finish_host_slack_receipt(dir)
}

fn reconcile_slack_volume_commit(dir: &Path, project: &str) -> Result<(), String> {
    let receipt = read_slack_receipt(dir)?
        .ok_or_else(|| "The committed Docker Slack digest receipt is missing.".to_owned())?;
    with_staged_slack_receipt(dir, &receipt, |stage| {
        run_slack_visible(
            project,
            &slack_run_args(
                dir,
                project,
                Some(stage),
                "0:0",
                true,
                RECONCILE_SLACK_COMMIT,
                &[],
            )?,
        )?
        .then_some(())
        .ok_or_else(|| "Docker could not reconcile the committed private Slack bundle.".into())
    })?;
    finish_host_slack_receipt(dir)
}

fn mark_slack_volume_commit(dir: &Path, project: &str) -> Result<(), String> {
    let receipt = read_slack_receipt(dir)?
        .ok_or_else(|| "The pending Docker Slack digest receipt is missing.".to_owned())?;
    with_staged_slack_receipt(dir, &receipt, |stage| {
        run_slack_visible(
            project,
            &slack_run_args(
                dir,
                project,
                Some(stage),
                "0:0",
                true,
                MARK_SLACK_COMMIT,
                &[],
            )?,
        )?
        .then_some(())
        .ok_or_else(|| "Docker could not record the pending Slack bundle update.".into())
    })
}

fn finalize_slack_volume_rollback(dir: &Path, project: &str) -> Result<(), String> {
    run_slack_visible(
        project,
        &slack_run_args(
            dir,
            project,
            None,
            "0:0",
            true,
            FINALIZE_SLACK_ROLLBACK,
            &[],
        )?,
    )?
    .then_some(())
    .ok_or_else(|| "Docker could not finalize the restored private Slack bundle.".into())
}

fn recover_slack_volume(
    dir: &Path,
    project: &str,
    env: &Path,
    existing_env: &str,
) -> Result<String, String> {
    let receipt = read_slack_receipt(dir)?;
    let rollback_receipt = read_private_receipt(&slack_rollback_receipt_path(dir))?;
    let execute = |stage: Option<&Path>| {
        let args = slack_run_args(dir, project, stage, "0:0", true, RECOVER_SLACK, &[])?;
        run_slack(project, &args, Duration::from_secs(60))
    };
    let output = match receipt.as_deref() {
        Some(receipt) => with_staged_slack_receipt(dir, receipt, |stage| execute(Some(stage)))?,
        None => execute(None)?,
    };
    if !output.success {
        return Err("Docker could not reconcile a pending Slack bundle update.".into());
    }
    if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-clean")
    {
        if receipt.is_none() && rollback_receipt.as_deref().is_some_and(<[u8]>::is_empty) {
            // A fresh-install rollback may already have finalized the empty old generation
            // before a crash left the newly rendered setup files behind. Prove the volume is
            // still credential-free, durably remove that stale wiring, then retire the anchor.
            // Every step is replayable while the empty anchor remains.
            validate_previous_slack_volume(dir, project, None, None)?;
            let text = env_text(existing_env, &[], &["AGENTSCM_SLACK_FILE", SLACK_FILES_ENV])?;
            write(env, &text)?;
            finish_host_slack_receipt(dir)?;
            return Ok(text);
        }
        if rollback_receipt.is_some() {
            let files = recorded_slack_files(existing_env)?;
            validate_previous_slack_volume(dir, project, files.as_deref(), receipt.as_deref())?;
            finish_host_slack_receipt(dir)?;
        }
        return Ok(existing_env.to_owned());
    }
    if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-commit")
    {
        let files = recorded_slack_files(existing_env)?.ok_or_else(|| {
            "The committed Docker Slack wiring is missing from the environment.".to_owned()
        })?;
        validate_previous_slack_volume(dir, project, Some(&files), receipt.as_deref())?;
        finish_host_slack_receipt(dir)?;
        return Ok(existing_env.to_owned());
    }
    let old = output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("agentscm-slack-rollback:"))
        .ok_or_else(|| "Docker returned an invalid Slack recovery result.".to_owned())?;
    let prior_receipt = rollback_receipt.ok_or_else(|| {
        "The pending Docker Slack update has no durable prior digest receipt.".to_owned()
    })?;
    restore_slack_receipt(
        &slack_receipt_path(dir),
        (!prior_receipt.is_empty()).then_some(prior_receipt.as_slice()),
    )?;
    let text = if old.is_empty() {
        validate_previous_slack_volume(dir, project, None, None)?;
        env_text(existing_env, &[], &["AGENTSCM_SLACK_FILE", SLACK_FILES_ENV])?
    } else {
        let files: Vec<String> = old.split(',').map(str::to_owned).collect();
        if !valid_slack_files(&files) {
            return Err("Docker recovered an invalid prior Slack file set.".into());
        }
        validate_previous_slack_volume(dir, project, Some(&files), Some(&prior_receipt))?;
        env_text(
            existing_env,
            &[
                ("AGENTSCM_SLACK_FILE", SLACK_CONFIG),
                (SLACK_FILES_ENV, old),
            ],
            &[],
        )?
    };
    write(env, &text)?;
    finalize_slack_volume_rollback(dir, project)?;
    finish_host_slack_receipt(dir)?;
    Ok(text)
}

fn rollback_slack_volume(
    dir: &Path,
    project: &str,
    previous_files: Option<&[String]>,
    previous_receipt: Option<&[u8]>,
) -> Result<bool, String> {
    let args = slack_run_args(dir, project, None, "0:0", true, ROLLBACK_SLACK, &[])?;
    let output = run_slack(project, &args, Duration::from_secs(60))?;
    if !output.success {
        return Err("Docker could not restore the previous private Slack bundle.".into());
    }
    let pending = if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-rollback")
    {
        true
    } else if output
        .stdout
        .lines()
        .any(|line| line == "agentscm-slack-clean")
    {
        false
    } else {
        return Err("Docker returned an invalid Slack rollback result.".into());
    };
    restore_slack_receipt(&slack_receipt_path(dir), previous_receipt)?;
    validate_previous_slack_volume(dir, project, previous_files, previous_receipt)?;
    Ok(pending)
}

fn validate_previous_slack_volume(
    dir: &Path,
    project: &str,
    previous_files: Option<&[String]>,
    previous_receipt: Option<&[u8]>,
) -> Result<(), String> {
    if let Some(files) = previous_files {
        let receipt = previous_receipt
            .ok_or_else(|| "The previous Docker Slack digest receipt is missing.".to_owned())?;
        return verify_slack_volume_with_receipt(dir, project, files, receipt);
    }
    run_slack_visible(
        project,
        &slack_run_args(
            dir,
            project,
            None,
            "10001:10001",
            false,
            VERIFY_NO_SLACK,
            &[],
        )?,
    )?
    .then_some(())
    .ok_or_else(|| "The restored Docker volume contains unrecorded Slack credentials.".into())
}

fn with_staged_slack_bundle<T>(
    dir: &Path,
    bundle: &SlackBundle,
    operation: impl FnOnce(&Path, &[String]) -> Result<T, String>,
) -> Result<T, String> {
    let files: Vec<String> = bundle.files.iter().map(|(name, _)| name.clone()).collect();
    if bundle.config != Path::new(SLACK_CONFIG) || !valid_slack_files(&files) {
        return Err("The prepared Slack bundle has an invalid Docker target.".into());
    }
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| "random source unavailable".to_owned())?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let stage = dir.join(format!(".slack-stage-{suffix}"));
    std::fs::create_dir(&stage).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let result = (|| {
        for (name, bytes) in &bundle.files {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| "A prepared Slack bundle file is not UTF-8.".to_owned())?;
            write(&stage.join(name), text)?;
        }
        let receipt = slack_bundle_receipt(bundle);
        write(
            &stage.join(".agentscm-expected-receipt"),
            std::str::from_utf8(&receipt)
                .map_err(|_| "The Docker Slack receipt is not UTF-8.".to_owned())?,
        )?;
        operation(&stage, &files)
    })();
    match result {
        Ok(value) => {
            std::fs::remove_dir_all(&stage).map_err(|error| error.to_string())?;
            super::sync_dir(dir)?;
            Ok(value)
        }
        Err(error) => Err(format!(
            "{error} The private Slack credential stage at {} was retained. The next quickstart run removes it after confirming no owned helper remains.",
            stage.display()
        )),
    }
}

fn verify_expected_slack_volume(
    dir: &Path,
    project: &str,
    bundle: &SlackBundle,
) -> Result<(), String> {
    with_staged_slack_bundle(dir, bundle, |stage, files| {
        let check = slack_run_args(dir, project, Some(stage), "0:0", true, CHECK_SLACK, files)?;
        let output = run_slack(project, &check, Duration::from_secs(60))?;
        if !output.success
            || !output
                .stdout
                .lines()
                .any(|line| line == "agentscm-slack-unchanged")
        {
            return Err("The committed Docker Slack bundle does not match the requested credentials. The Hub remains stopped.".into());
        }
        let receipt = slack_bundle_receipt(bundle);
        verify_slack_volume_with_receipt(dir, project, files, &receipt)
    })
}

fn install_slack_volume(dir: &Path, project: &str, bundle: &SlackBundle) -> Result<bool, String> {
    with_staged_slack_bundle(dir, bundle, |stage, files| {
        let check = slack_run_args(dir, project, Some(stage), "0:0", true, CHECK_SLACK, files)?;
        let output = run_slack(project, &check, Duration::from_secs(60))?;
        if !output.success {
            return Err("Docker could not compare the prepared private Slack bundle.".to_owned());
        }
        if output
            .stdout
            .lines()
            .any(|line| line == "agentscm-slack-unchanged")
        {
            let receipt = slack_bundle_receipt(bundle);
            verify_slack_volume_with_receipt(dir, project, files, &receipt)?;
            return Ok(false);
        }
        if !output
            .stdout
            .lines()
            .any(|line| line == "agentscm-slack-changed")
        {
            return Err("Docker did not confirm whether the Slack bundle changed.".to_owned());
        }
        if !down(dir, project, false) {
            return Err("Docker could not stop the Hub before replacing its Slack bundle.".into());
        }
        let args = slack_run_args(dir, project, Some(stage), "0:0", true, INSTALL_SLACK, files)?;
        let output = run_slack(project, &args, Duration::from_secs(60))?;
        if !output.success {
            if !diagnostic(&output).is_empty() {
                eprintln!("{}", diagnostic(&output));
            }
            return Err(
                "Docker could not install the private Slack bundle in its owned volume.".to_owned(),
            );
        }
        let changed = match output
            .stdout
            .lines()
            .find(|line| line.starts_with("agentscm-slack-"))
        {
            Some("agentscm-slack-changed") => true,
            Some("agentscm-slack-unchanged") => false,
            _ => return Err("Docker did not confirm the private Slack bundle update.".into()),
        };
        let receipt = slack_bundle_receipt(bundle);
        verify_slack_volume_with_receipt(dir, project, files, &receipt).map(|()| changed)
    })
}

fn recorded_slack_files(env: &str) -> Result<Option<Vec<String>>, String> {
    let config = env_value(env, "AGENTSCM_SLACK_FILE");
    let files = env_value(env, SLACK_FILES_ENV);
    match (config, files) {
        (None, None) => Ok(None),
        (Some(SLACK_CONFIG), Some(files)) => {
            let files: Vec<String> = files.split(',').map(str::to_owned).collect();
            if !valid_slack_files(&files) {
                return Err("The recorded Docker Slack wiring is invalid.".into());
            }
            Ok(Some(files))
        }
        _ => Err("The recorded Docker Slack wiring is incomplete or has drifted.".into()),
    }
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

struct SlackRecovery<'a> {
    prior_record: &'a Option<LocalHubRecord>,
    compose: &'a Path,
    prior_compose: Option<&'a Vec<u8>>,
    env: &'a Path,
    prior_env: Option<&'a Vec<u8>>,
    prior_slack_files: Option<&'a [String]>,
    prior_slack_receipt: Option<&'a [u8]>,
}

struct SlackCommitRecovery<'a> {
    manifest: &'a Manifest,
    compose: &'a Path,
    expected_compose: &'a str,
    env: &'a Path,
    expected_env: &'a str,
    bundle: &'a SlackBundle,
    port: u16,
}

fn same_optional_file(path: &Path, expected: Option<&Vec<u8>>) -> Result<(), String> {
    match (std::fs::read(path), expected) {
        (Ok(actual), Some(expected)) if &actual == expected => Ok(()),
        (Err(error), None) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(format!("{} was not restored exactly", path.display())),
    }
}

fn validate_docker_restoration(
    context: &SlackRecovery<'_>,
    manifest: &Manifest,
) -> Result<(), String> {
    same_optional_file(context.compose, context.prior_compose)?;
    same_optional_file(context.env, context.prior_env)?;
    let record_matches = match (&manifest.local_hub, context.prior_record) {
        (None, None) => true,
        (Some(actual), Some(expected)) => {
            actual.dir == expected.dir
                && actual.project == expected.project
                && actual.port == expected.port
        }
        _ => false,
    };
    if !record_matches {
        return Err("the previous quickstart record was not restored".into());
    }
    let env = match context.prior_env {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| "the previous Docker environment is not UTF-8".to_owned())?,
        None => "",
    };
    if recorded_slack_files(env)?.as_deref() != context.prior_slack_files {
        return Err("the restored Docker environment does not match the prior Slack bundle".into());
    }
    if read_slack_receipt(
        context
            .env
            .parent()
            .ok_or("the Docker environment has no parent")?,
    )?
    .as_deref()
        != context.prior_slack_receipt
    {
        return Err(
            "the restored Docker Slack digest receipt does not match the prior bundle".into(),
        );
    }
    Ok(())
}

fn validate_docker_commit(
    runtime: &DockerRuntime,
    context: &SlackCommitRecovery<'_>,
) -> Result<(), String> {
    if std::fs::read(context.compose).ok().as_deref() != Some(context.expected_compose.as_bytes()) {
        return Err("the committed compose.yaml does not match the requested Hub".into());
    }
    if std::fs::read(context.env).ok().as_deref() != Some(context.expected_env.as_bytes()) {
        return Err("the committed .env does not match the requested Slack wiring".into());
    }
    let Some(record) = context.manifest.local_hub.as_ref() else {
        return Err("the committed quickstart record is missing".into());
    };
    if record.dir != runtime.dir || record.project != runtime.project || record.port != context.port
    {
        return Err("the committed quickstart record does not match the requested Hub".into());
    }
    let expected_files: Vec<String> = context
        .bundle
        .files
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    if recorded_slack_files(context.expected_env)?.as_deref() != Some(&expected_files) {
        return Err("the committed environment does not name the requested Slack bundle".into());
    }
    verify_expected_slack_volume(&runtime.dir, &runtime.project, context.bundle)
}

fn restart_previous_docker<F>(errors: &mut Vec<String>, previous_exists: bool, restart: F) -> bool
where
    F: FnOnce() -> Result<(), String>,
{
    if !errors.is_empty() {
        return false;
    }
    if !previous_exists {
        return true;
    }
    match restart() {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("restarting the previous Hub failed: {error}"));
            false
        }
    }
}

fn finalize_docker_recovery<F>(errors: &mut Vec<String>, pending: bool, finalize: F) -> bool
where
    F: FnOnce() -> Result<(), String>,
{
    if !errors.is_empty() {
        return false;
    }
    if pending && let Err(error) = finalize() {
        errors.push(error);
        return false;
    }
    true
}

async fn recover_slack_update(
    runtime: &DockerRuntime,
    manifest: &mut Manifest,
    recovery_context: &SlackRecovery<'_>,
    cause: String,
) -> String {
    let mut recovery = Vec::new();
    if !down(&runtime.dir, &runtime.project, false) {
        return format!(
            "{cause} Recovery also failed: stopping the replacement Hub failed; the pending Slack transaction was retained."
        );
    }
    let slack_restored = rollback_slack_volume(
        &runtime.dir,
        &runtime.project,
        recovery_context.prior_slack_files,
        recovery_context.prior_slack_receipt,
    );
    if let Err(error) = &slack_restored {
        recovery.push(error.clone());
    }
    let rollback_pending = slack_restored == Ok(true);
    let finalize_before_restore = rollback_pending
        && (recovery_context.prior_compose.is_none() || recovery_context.prior_env.is_none());
    if finalize_before_restore {
        finalize_docker_recovery(&mut recovery, true, || {
            finalize_slack_volume_rollback(&runtime.dir, &runtime.project)
        });
    }
    if !recovery.is_empty() {
        return format!("{cause} Recovery also failed: {}.", recovery.join("; "));
    }
    if let Err(error) = restore_file(recovery_context.compose, recovery_context.prior_compose) {
        recovery.push(format!("restoring compose.yaml failed: {error}"));
    }
    if let Err(error) = restore_file(recovery_context.env, recovery_context.prior_env) {
        recovery.push(format!("restoring .env failed: {error}"));
    }
    manifest.local_hub = recovery_context.prior_record.clone();
    if let Err(error) = manifest.save() {
        recovery.push(format!("restoring the quickstart record failed: {error}"));
    }
    if recovery.is_empty()
        && let Err(error) = validate_docker_restoration(recovery_context, manifest)
    {
        recovery.push(format!(
            "validating the restored Docker Hub failed: {error}"
        ));
    }
    if !finalize_before_restore {
        finalize_docker_recovery(&mut recovery, rollback_pending, || {
            finalize_slack_volume_rollback(&runtime.dir, &runtime.project)
        });
    }
    if recovery.is_empty()
        && let Err(error) = finish_host_slack_receipt(&runtime.dir)
    {
        recovery.push(format!(
            "finalizing the restored Slack digest receipt failed: {error}"
        ));
    }
    let restarted = restart_previous_docker(
        &mut recovery,
        recovery_context.prior_record.is_some(),
        || runtime.up(true),
    );
    if restarted
        && recovery_context.prior_record.is_some()
        && let Err(error) = wait_healthy(&runtime.url).await
    {
        recovery.push(format!(
            "the restarted previous Hub was not healthy: {error}"
        ));
        if !down(&runtime.dir, &runtime.project, false) {
            recovery.push("stopping the unhealthy previous Hub failed".into());
        }
    }
    if recovery.is_empty() {
        format!("{cause} The previous Docker Hub and Slack bundle were restored.")
    } else {
        format!("{cause} Recovery also failed: {}.", recovery.join("; "))
    }
}

async fn reconcile_committed_slack_update(
    runtime: &DockerRuntime,
    context: SlackCommitRecovery<'_>,
    cause: String,
) -> Result<(), String> {
    if !down(&runtime.dir, &runtime.project, false) {
        return Err(format!(
            "{cause} Reconciliation also failed: stopping the Hub before validating the committed Slack generation failed."
        ));
    }
    let reconcile = reconcile_slack_volume_commit(&runtime.dir, &runtime.project)
        .and_then(|()| validate_docker_commit(runtime, &context));
    if let Err(error) = reconcile {
        return Err(format!(
            "{cause} Reconciliation also failed: {error} The Hub remains stopped."
        ));
    }
    if let Err(error) = runtime.up(true) {
        return Err(format!(
            "{cause} The committed Slack generation was validated, but restarting its Hub failed: {error}"
        ));
    }
    if let Err(error) = wait_healthy(&runtime.url).await {
        let stopped = down(&runtime.dir, &runtime.project, false);
        return Err(format!(
            "{cause} The committed Slack generation was validated, but its Hub was unhealthy: {error}{}",
            if stopped {
                " The Hub was stopped."
            } else {
                " Stopping the unhealthy Hub also failed."
            }
        ));
    }
    Ok(())
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
    if options.slack.is_some() {
        println!(
            "Will copy the prepared Slack configuration and credentials into the Hub's private Docker volume before the Hub starts."
        );
    }
    if !ui.confirm("Go ahead?", true) {
        return Err("Skipped the local hub.".into());
    }
    quiesce_slack_helpers(&options.project)?;
    cleanup_slack_stages(&dir)?;
    let had_complete_setup = compose.exists() && env.exists();
    let existing_env = std::fs::read_to_string(&env).unwrap_or_default();
    let (existing_env, mut recovered_slack_restart) = if had_complete_setup {
        // A Slack install writes a host receipt before it touches the volume. Without one there
        // is no transaction to find, and a resume must not need the previous compose image.
        let slack_used = slack_receipt_path(&dir).symlink_metadata().is_ok()
            || slack_rollback_receipt_path(&dir).symlink_metadata().is_ok();
        let pending = slack_used && slack_volume_pending(&dir, &options.project)?;
        if pending || read_private_receipt(&slack_rollback_receipt_path(&dir))?.is_some() {
            if !down(&dir, &options.project, false) {
                return Err(
                    "Docker could not stop the Hub before recovering its pending Slack transaction."
                        .into(),
                );
            }
            let existing_env = recover_slack_volume(&dir, &options.project, &env, &existing_env)?;
            (existing_env, true)
        } else {
            (existing_env, false)
        }
    } else {
        (existing_env, false)
    };
    let prior_slack_files = recorded_slack_files(&existing_env)?;
    let prior_slack_receipt = read_slack_receipt(&dir)?;
    if let Some(files) = prior_slack_files.as_deref() {
        verify_slack_volume(&dir, &options.project, files)?;
    }
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
    let slack_names = options.slack.as_ref().map(|bundle| {
        bundle
            .files
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    });
    let (preinstall_env, rendered_env) =
        staged_env_text(&existing_env, &updates, &stale, slack_names.as_deref())?;
    let rendered_compose = compose_text_for(platform.as_deref(), options.port);
    // Keep the previous Slack wiring published while the private volume transaction is prepared.
    // A restart before its durable ready marker therefore validates and recovers the old state.
    write_setup_files(&compose, &rendered_compose, &env, &preinstall_env)?;
    if !had_complete_setup
        && (slack_volume_pending(&dir, &options.project)?
            || read_private_receipt(&slack_rollback_receipt_path(&dir))?.is_some())
    {
        if !down(&dir, &options.project, false) {
            return Err(
                "Docker could not stop the Hub before recovering its pending Slack transaction."
                    .into(),
            );
        }
        let recovered_env = recover_slack_volume(&dir, &options.project, &env, &preinstall_env)?;
        if recovered_env != preinstall_env {
            return Err(
                "The recovered Docker Slack wiring did not match the fresh setup state.".into(),
            );
        }
        recovered_slack_restart = true;
    }
    let mut runtime = DockerRuntime {
        dir: dir.clone(),
        project: options.project.clone(),
        url: url.clone(),
    };
    let recovery = SlackRecovery {
        prior_record: &prior_record,
        compose: &compose,
        prior_compose: prior_compose.as_ref(),
        env: &env,
        prior_env: prior_env.as_ref(),
        prior_slack_files: prior_slack_files.as_deref(),
        prior_slack_receipt: prior_slack_receipt.as_deref(),
    };
    let slack_result = if let Some(bundle) = options.slack.as_ref() {
        prepare_slack_receipt(&dir, prior_slack_receipt.as_deref())
            .and_then(|()| install_slack_volume(&dir, &options.project, bundle))
    } else if let Some(files) = recorded_slack_files(&rendered_env)? {
        verify_slack_volume(&dir, &options.project, &files).map(|()| false)
    } else {
        Ok(false)
    };
    let slack_changed = match slack_result {
        Ok(changed) => changed,
        Err(error) => {
            return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
        }
    };
    if slack_changed {
        let bundle = options
            .slack
            .as_ref()
            .ok_or_else(|| "The changed Docker Slack bundle is unavailable.".to_owned())?;
        if let Err(error) = publish_slack_receipt(&dir, &slack_bundle_receipt(bundle)) {
            return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
        }
    } else if options.slack.is_some()
        && let Err(error) = finish_host_slack_receipt(&dir)
    {
        return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
    }
    if rendered_env != preinstall_env
        && let Err(error) = write(&env, &rendered_env)
    {
        return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
    }
    manifest.local_hub = Some(LocalHubRecord {
        dir: dir.clone(),
        project: options.project.clone(),
        port: options.port,
    });
    if let Err(error) = manifest.save() {
        if slack_changed {
            return Err(
                recover_slack_update(&runtime, manifest, &recovery, error.to_string()).await,
            );
        }
        manifest.local_hub = prior_record;
        restore_file(&compose, prior_compose.as_ref())?;
        restore_file(&env, prior_env.as_ref())?;
        return Err(error.to_string());
    }
    if slack_changed && let Err(error) = mark_slack_volume_commit(&dir, &options.project) {
        return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
    }
    let startup = async {
        runtime.up(slack_changed || recovered_slack_restart)?;
        wait_healthy(&url).await
    }
    .await;
    if let Err(error) = startup {
        if slack_changed {
            return Err(recover_slack_update(&runtime, manifest, &recovery, error).await);
        }
        return Err(format!("{error} Its setup remains recorded for recovery."));
    }
    if slack_changed && let Err(error) = finish_slack_volume_change(&dir, &options.project) {
        let bundle = options
            .slack
            .as_ref()
            .ok_or_else(|| "The committed Docker Slack bundle is unavailable.".to_owned())?;
        reconcile_committed_slack_update(
            &runtime,
            SlackCommitRecovery {
                manifest,
                compose: &compose,
                expected_compose: &rendered_compose,
                env: &env,
                expected_env: &rendered_env,
                bundle,
                port: options.port,
            },
            error,
        )
        .await?;
    }

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
        assert!(compose_text().contains("AGENTSCM_SLACK_FILE: ${AGENTSCM_SLACK_FILE:-}"));
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

    #[test]
    fn new_slack_wiring_is_published_only_after_volume_preparation() {
        let old = format!(
            "AGENTSCM_SLACK_FILE={SLACK_CONFIG}\n{SLACK_FILES_ENV}=config.json,signing-secret,bot-token,agent-0.token\n"
        );
        let next = "config.json,signing-secret,bot-token,agent-0.token,agent-1.token";
        let (before_volume, published) = staged_env_text(
            &old,
            &[("AGENTSCM_HUB_IMAGE", "synthetic:v1")],
            &[],
            Some(next),
        )
        .unwrap();
        assert!(before_volume.contains("agent-0.token\n"));
        assert!(!before_volume.contains("agent-1.token"));
        assert!(published.contains(next));
        assert!(before_volume.contains("AGENTSCM_HUB_IMAGE=synthetic:v1"));
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

    #[test]
    fn docker_slack_wiring_is_explicit_and_uses_only_fixed_leaf_names() {
        let env = format!(
            "AGENTSCM_SLACK_FILE={SLACK_CONFIG}\n{SLACK_FILES_ENV}=config.json,signing-secret,bot-token,agent-0.token\n"
        );
        assert_eq!(
            recorded_slack_files(&env).unwrap().unwrap(),
            [
                "config.json",
                "signing-secret",
                "bot-token",
                "agent-0.token"
            ]
        );
        assert!(
            recorded_slack_files(
                "AGENTSCM_SLACK_FILE=/tmp/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=config.json\n"
            )
            .is_err()
        );
        assert!(recorded_slack_files(
            "AGENTSCM_SLACK_FILE=/data/secrets/slack/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=../token\n"
        )
        .is_err());
        assert!(validate_host_slack_receipt(b"00  ../token\n").is_err());
        assert!(
            validate_host_slack_receipt(format!("{}  config.json\n", "0".repeat(64)).as_bytes())
                .is_err()
        );
        assert!(VALIDATE_SLACK_TXN.contains(&agentscm::hub::slack::setup::JOURNAL_FILES.join("|")));
        assert!(!INSTALL_SLACK.contains("mv \"$target\""));
    }

    #[cfg(unix)]
    #[test]
    fn docker_slack_setup_command_contains_no_credential_bytes() {
        let files = vec![
            "config.json".into(),
            "signing-secret".into(),
            "bot-token".into(),
            "agent-0.token".into(),
        ];
        let args = slack_run_args(
            Path::new("/h/.agentscm/hub"),
            "agentscm-hub",
            Some(Path::new("/h/.agentscm/hub/.slack-stage-test")),
            "0:0",
            true,
            INSTALL_SLACK,
            &files,
        )
        .unwrap();
        let rendered = args.join(" ");
        assert!(rendered.contains("--user 0:0"));
        assert!(rendered.contains("--cap-add DAC_OVERRIDE --cap-add CHOWN"));
        assert!(rendered.contains("--name agentscm-slack-"));
        assert!(rendered.contains("com.counterbranch.agentscm.slack-helper=1"));
        assert!(rendered.contains("com.counterbranch.agentscm.slack-project=agentscm-hub"));
        assert!(rendered.contains("flock -x 9"));
        assert!(
            rendered.find("set -eu").unwrap() < rendered.find("private_dir /data/secrets").unwrap()
        );
        assert!(rendered.contains("/run/agentscm-slack:ro"));
        assert!(!rendered.contains("xoxb-") && !rendered.contains("synthetic-signing"));
        let verify = slack_run_args(
            Path::new("/h/.agentscm/hub"),
            "agentscm-hub",
            None,
            "10001:10001",
            false,
            VERIFY_SLACK,
            &files,
        )
        .unwrap()
        .join(" ");
        assert!(verify.contains("--user 10001:10001"));
        assert!(!verify.contains("--cap-add"));
        let staged_verify = slack_run_args(
            Path::new("/h/.agentscm/hub"),
            "agentscm-hub",
            Some(Path::new("/h/.agentscm/hub/.slack-stage-test")),
            "0:0",
            false,
            VERIFY_SLACK,
            &files,
        )
        .unwrap()
        .join(" ");
        assert!(staged_verify.contains("--user 0:0 --entrypoint"));
        assert!(staged_verify.contains("--cap-add DAC_OVERRIDE"));
        assert!(!staged_verify.contains("--cap-add CHOWN"));
        assert!(!VERIFY_SLACK.contains("grep -q") && !INSTALL_SLACK.contains("grep -q"));
        assert!(VERIFY_SLACK.contains("$2 == wanted"));
        assert!(VERIFY_SLACK.contains("*[!0-9]*") && VERIFY_SLACK.contains("*) exit 1"));
        assert!(INSTALL_SLACK.contains("cp \"$target/$file\" \"$backup_stage/$file\""));
        assert!(
            INSTALL_SLACK
                .find("cp \"$target/$file\" \"$backup_stage/$file\"")
                .unwrap()
                < INSTALL_SLACK.find(": > \"$backup_stage/ready\"").unwrap()
        );
        assert!(
            INSTALL_SLACK
                .find("mv \"$backup_stage\" \"$backup\"")
                .unwrap()
                < INSTALL_SLACK
                    .find("mv \"$next/$file\" \"$target/$file\"")
                    .unwrap()
        );
        assert!(ROLLBACK_SLACK.contains("cp \"$backup/$file\" \"$target/$file\""));
        // A first install's rollback drops the Hub's journal before removing the empty target.
        for script in [INSTALL_SLACK, ROLLBACK_SLACK, RECOVER_SLACK] {
            let drop = script
                .find("then rm -f \"$target/install.sqlite\"")
                .unwrap();
            assert!(drop < script.rfind("rmdir \"$target\"").unwrap());
        }
        assert!(RECOVER_SLACK.contains("agentscm-slack-rollback:%s"));
        assert!(MARK_SLACK_COMMIT.contains("$backup/commit"));
        assert!(COMMIT_SLACK.contains("rm -rf \"$backup\""));
    }

    #[cfg(unix)]
    #[test]
    fn docker_slack_stage_cleanup_is_exact_and_owner_private() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join(".slack-stage-0123456789abcdef01234567");
        std::fs::create_dir(&stage).unwrap();
        std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).unwrap();
        let unrelated = temp.path().join(".slack-stage-not-owned-by-the-pattern");
        std::fs::create_dir(&unrelated).unwrap();
        cleanup_slack_stages(temp.path()).unwrap();
        assert!(!stage.exists());
        assert!(unrelated.exists());

        let unsafe_stage = temp.path().join(".slack-stage-fedcba9876543210fedcba98");
        std::fs::create_dir(&unsafe_stage).unwrap();
        std::fs::set_permissions(&unsafe_stage, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(cleanup_slack_stages(temp.path()).is_err());
        assert!(unsafe_stage.exists());
    }

    #[cfg(unix)]
    #[test]
    fn docker_slack_failed_operation_retains_its_private_stage() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let receipt = [
            "config.json",
            "signing-secret",
            "bot-token",
            "agent-0.token",
        ]
        .into_iter()
        .map(|name| format!("{}  {name}\n", "0".repeat(64)))
        .collect::<String>();
        assert!(
            with_staged_slack_receipt(temp.path(), receipt.as_bytes(), |_| {
                Err::<(), _>("synthetic unconfirmed helper failure".into())
            })
            .is_err()
        );
        let retained: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        cleanup_slack_stages(temp.path()).unwrap();
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());

        let bundle = SlackBundle {
            config: PathBuf::from(SLACK_CONFIG),
            files: vec![
                ("config.json".into(), b"{}".to_vec()),
                ("signing-secret".into(), b"synthetic".to_vec()),
                ("bot-token".into(), b"synthetic".to_vec()),
                ("agent-0.token".into(), vec![b'a'; 64]),
            ],
        };
        let error = with_staged_slack_bundle(temp.path(), &bundle, |_, _| {
            Err::<(), _>("synthetic confirmed failure".into())
        })
        .unwrap_err();
        assert!(error.contains("The private Slack credential stage at"));
        assert!(error.contains("The next quickstart run removes it"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn docker_slack_lock_failure_aborts_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let secrets = temp.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        let marker = temp.path().join("mutation-ran");
        let script = format!(
            "set -eu\n{VALIDATE_SLACK_TXN}\n{LOCK_SLACK_MUTATION}\ntouch '{}'",
            marker.display()
        )
        .replace("/data/secrets", &secrets.display().to_string());
        let status = std::process::Command::new("sh")
            .args(["-c", &script])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(!marker.exists());
    }

    #[test]
    fn docker_recovery_keeps_consumer_stopped_and_retry_evidence_on_failure() {
        let restarted = std::cell::Cell::new(false);
        let finalized = std::cell::Cell::new(false);
        let mut errors = vec!["synthetic post-install rollback failure".to_owned()];
        assert!(!restart_previous_docker(&mut errors, true, || {
            restarted.set(true);
            Ok(())
        }));
        finalize_docker_recovery(&mut errors, true, || {
            finalized.set(true);
            Ok(())
        });
        assert!(
            !restarted.get(),
            "an unvalidated old generation stayed stopped"
        );
        assert!(
            !finalized.get(),
            "the pending rollback evidence remained available for retry"
        );

        let mut errors = Vec::new();
        assert!(restart_previous_docker(&mut errors, true, || {
            restarted.set(true);
            Ok(())
        }));
        errors.push("synthetic old-generation health failure".into());
        finalize_docker_recovery(&mut errors, true, || {
            finalized.set(true);
            Ok(())
        });
        assert!(restarted.get());
        assert!(!finalized.get(), "failed health retained rollback evidence");
    }
}
