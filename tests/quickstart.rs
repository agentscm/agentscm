//! The real binary's `quickstart` and `uninstall` against an in-process hub, with a
//! temporary HOME so nothing outside the test is read or written. Docker and the
//! first-run GitHub App flow cannot run here; their commands are unit tested.
#![cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use agentscm::backend::LocalBackend;
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::{HubState, router};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPO: &str = "acme/widgets";
const REMOTE: &str = "git@github.com:acme/widgets.git";

struct Hub {
    url: String,
    store: Arc<dyn Store>,
    _runtime: tokio::runtime::Runtime,
    _dir: TempDir,
}

impl Hub {
    fn start() -> Self {
        Self::start_with_health_guard(None)
    }

    fn start_with_health_guard(marker: Option<PathBuf>) -> Self {
        let dir = TempDir::new().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.sqlite").display());
        let store = store::open(&url).unwrap();
        let objects = LocalBackend::open(dir.path().join("objects")).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let app = router(HubState::new(store.clone(), Arc::new(objects))).layer(
            axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let marker = marker.clone();
                    async move {
                        if request.uri().path() == "/v1/hub/health"
                            && marker.as_ref().is_some_and(|path| {
                                std::fs::read(path).ok().as_deref() == Some(b"broken")
                            })
                        {
                            axum::response::Response::builder()
                                .status(503)
                                .body(axum::body::Body::empty())
                                .unwrap()
                        } else {
                            next.run(request).await
                        }
                    }
                },
            ),
        );
        let address = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            address
        });
        Self {
            url: format!("http://{address}"),
            store,
            _runtime: runtime,
            _dir: dir,
        }
    }

    /// An agent token for `repo`, written to a file in `dir`.
    fn token_file_for(&self, dir: &Path, repo: &str, name: &str) -> PathBuf {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: 1,
        };
        let owner = self.store.upsert_user(&identity, "octo").unwrap();
        let (_, secret) = mint_agent_token(&*self.store, &owner.id, "t", repo, 3600).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, format!("{}\n", secret.expose())).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        file
    }

    fn token_file(&self, dir: &Path) -> PathBuf {
        self.token_file_for(dir, REPO, "given.token")
    }
}

/// The test binary at `dest`: a hard link where the filesystem allows, else a copy.
fn place_binary(dest: &Path) {
    let exe = env!("CARGO_BIN_EXE_agentscm");
    if std::fs::hard_link(exe, dest).is_err() {
        std::fs::copy(exe, dest).unwrap();
    }
}

fn find_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH")
}

/// A machine with its own HOME and a PATH holding only git, so no Docker, gh or Claude is found.
struct Machine {
    home: TempDir,
    bin: TempDir,
    /// Whether `/usr/bin:/bin` is on PATH, which the fake `claude` script needs.
    system_path: bool,
}

impl Machine {
    /// With `agentscm` on PATH (a link to the test binary), so hooks call it bare and
    /// nothing is copied into the install directory.
    fn new() -> Self {
        let machine = Self::transient();
        symlink(
            env!("CARGO_BIN_EXE_agentscm"),
            machine.bin.path().join("agentscm"),
        )
        .unwrap();
        machine
    }

    /// No `agentscm` on PATH: the test binary looks transient and gets copied.
    fn transient() -> Self {
        let bin = TempDir::new().unwrap();
        symlink(find_git(), bin.path().join("git")).unwrap();
        Self {
            home: TempDir::new().unwrap(),
            bin,
            system_path: false,
        }
    }

    fn path(&self) -> std::ffi::OsString {
        let mut path = self.bin.path().as_os_str().to_owned();
        if self.system_path {
            path.push(":/usr/bin:/bin");
        }
        path
    }

    fn config(&self) -> PathBuf {
        self.home.path().join(".config").join("agentscm")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        self.command_as(Path::new(env!("CARGO_BIN_EXE_agentscm")), cwd, args)
    }

    fn command_as(&self, program: &Path, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(cwd)
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("PATH", self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    fn git(&self, cwd: &Path, args: &[&str]) {
        let status = Command::new(self.bin.path().join("git"))
            .current_dir(cwd)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
            ])
            .args(args)
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repo_at(&self, remote: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        self.git(dir.path(), &["init", "-q", "-b", "main"]);
        self.git(dir.path(), &["remote", "add", "origin", remote]);
        dir
    }

    fn repo(&self) -> TempDir {
        self.repo_at(REMOTE)
    }

    /// `~/.agentscm/bin`.
    fn bin_dir(&self) -> PathBuf {
        self.home.path().join(".agentscm/bin")
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    out(output)
}

fn json_file(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn mode(path: &Path) -> u32 {
    path.metadata().unwrap().permissions().mode() & 0o777
}

fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

fn quick<'a>(url: &'a str, token: &'a Path, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["quickstart", "--yes", "--hub", "existing", "--hub-url", url];
    args.extend(["--agent-token-file", token.to_str().unwrap()]);
    args.extend(extra);
    args
}

fn last_line(output: &Output) -> String {
    out(output).lines().last().unwrap_or_default().to_owned()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn hook_json(session: &str, cwd: &Path, file: &str) -> String {
    json!({"session_id": session, "transcript_path": "/t.jsonl", "cwd": cwd,
        "permission_mode": "default", "hook_event_name": "PreToolUse", "tool_name": "Edit",
        "tool_input": {"file_path": file, "old_string": "a", "new_string": "b"},
        "tool_use_id": "toolu_1"})
    .to_string()
}

/// An Edit hook call from `cwd`, through the checkout's client connection.
fn edit(machine: &Machine, cwd: &Path, session: &str, file: &str) -> Output {
    let mut check = machine.command(cwd, &["intent", "check", "--hook", "claude"]);
    check.stdin(Stdio::piped());
    let mut child = check.spawn().unwrap();
    let input = hook_json(session, cwd, file);
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn live_paths(hub: &Hub, repo: &str) -> Vec<Vec<String>> {
    let live = hub.store.live_intents(repo, unix_now(), 100).unwrap();
    live.into_iter().map(|i| i.paths).collect()
}

#[test]
fn a_program_already_in_the_install_directory_is_used_through_its_symlink() {
    let machine = Machine::new();
    let repo = machine.repo();
    let bin = machine.bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    place_binary(&bin.join("agentscm-0.0.0"));
    symlink("agentscm-0.0.0", bin.join("agentscm")).unwrap();
    std::fs::write(bin.join("agentscm-0.0.1"), b"stale").unwrap();
    std::fs::create_dir(bin.join("agentscm-dir")).unwrap();
    let link = bin.join("agentscm");
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--no-agents",
        "--no-collisions",
    ];
    ok(&machine
        .command_as(&link, repo.path(), &args)
        .output()
        .unwrap());
    // Nothing was copied or replaced.
    assert_eq!(
        names_in(&bin),
        [
            "agentscm",
            "agentscm-0.0.0",
            "agentscm-0.0.1",
            "agentscm-dir"
        ]
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("agentscm-0.0.0")
    );
    assert!(!repo.path().join(".claude").exists());
    // Uninstall removes the installer's files, even ones quickstart did not make, and
    // only those named agentscm or agentscm-*: not a directory.
    let said = ok(&machine
        .command_as(&link, repo.path(), &["uninstall", "--yes"])
        .output()
        .unwrap());
    assert!(
        said.contains("agentscm, agentscm-0.0.0, agentscm-0.0.1"),
        "{said}"
    );
    assert_eq!(names_in(&bin), ["agentscm-dir"]);
}

#[test]
fn collisions_are_summarised() {
    let machine = Machine::new();
    let repo = machine.repo();
    let (dir, git) = (repo.path(), |args: &[&str]| machine.git(repo.path(), args));
    std::fs::write(dir.join("f"), "base\n").unwrap();
    git(&["add", "f"]);
    git(&["commit", "-q", "-m", "base"]);
    for (branch, text) in [("feat-a", "a\n"), ("feat-b", "b\n")] {
        git(&["checkout", "-q", "-b", branch, "main"]);
        std::fs::write(dir.join("f"), text).unwrap();
        git(&["commit", "-q", "-am", branch]);
    }
    git(&["checkout", "-q", "main"]);
    let version = Command::new(find_git()).arg("--version").output().unwrap();
    let said = ok(&machine.run(
        dir,
        &["quickstart", "--yes", "--hub", "skip", "--no-agents"],
    ));
    if String::from_utf8_lossy(&version.stdout).contains("git version 2.")
        && !said.contains("Couldn't check")
    {
        assert!(
            said.contains(
                "2 branches/worktrees checked: 1 would conflict, 0 touch the same files."
            ),
            "{said}"
        );
        assert!(
            said.contains("feat-a and feat-b: f (would conflict)"),
            "{said}"
        );
    } else {
        eprintln!("old Git: collision summary not exercised: {said}");
    }
    assert!(!dir.join(".claude").exists());
}

#[test]
fn hosted_prints_the_guide_and_sets_up_no_hub() {
    let machine = Machine::new();
    let repo = machine.repo();
    let output = machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hosted",
            "--no-agents",
            "--no-collisions",
        ],
    );
    let said = ok(&output);
    assert!(said.contains("not supported in the alpha"), "{said}");
    assert!(said.contains("docs/hosted.md"), "{said}");
    assert!(last_line(&output).starts_with("Live warnings: off — no hub chosen"));
    assert!(!machine.config().join("quickstart.json").exists());
    assert!(!machine.home.path().join(".agentscm/hub").exists());
    let output = machine.run(repo.path(), &["quickstart", "--yes", "--local", "--hosted"]);
    assert!(!output.status.success());
}

#[test]
fn a_first_local_hub_with_yes_needs_its_administrator_before_any_write() {
    let machine = Machine::new();
    let repo = machine.repo();
    for hub in [&["--local"][..], &["--hub", "local"]] {
        let mut args = vec!["quickstart", "--yes", "--no-collisions"];
        args.extend_from_slice(hub);
        let output = machine.run(repo.path(), &args);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains("--github-login"), "{said}");
        assert!(!machine.config().exists());
        assert!(!machine.home.path().join(".agentscm").exists());
    }
}

#[test]
fn running_from_the_install_directory_is_undoable_without_any_hook() {
    let machine = Machine::transient();
    let repo = machine.repo();
    let bin = machine.bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    place_binary(&bin.join("agentscm-0.0.0"));
    symlink("agentscm-0.0.0", bin.join("agentscm")).unwrap();
    let link = bin.join("agentscm");
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--no-agents",
        "--no-collisions",
    ];
    let said = ok(&machine
        .command_as(&link, repo.path(), &args)
        .output()
        .unwrap());
    // No hooks were written, so the summary must not claim hooks call the program.
    assert!(
        said.contains(&format!(
            "{}: the agentscm program\n",
            bin.display()
                .to_string()
                .replace(machine.home.path().to_str().unwrap(), "~")
        )),
        "{said}"
    );
    assert_eq!(
        json_file(&machine.config().join("quickstart.json"))["bin_dir"],
        bin.to_str().unwrap()
    );
    ok(&machine
        .command_as(&link, repo.path(), &["uninstall", "--yes"])
        .output()
        .unwrap());
    assert!(!bin.exists());
    assert!(!machine.config().join("quickstart.json").exists());
}

#[test]
fn init_writes_only_instruction_files_even_for_a_local_hub() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let said = ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--no-collisions", "--init"]),
    ));
    // Instructions name no hub, so a hub on this machine no longer skips them.
    assert!(repo.path().join("AGENTS.md").exists(), "{said}");
    assert!(!repo.path().join(".claude/settings.json").exists());
    assert!(said.contains("Repository files to review and commit"), "{said}");
}

#[test]
fn no_op_run_explicitly_reports_no_changes_and_writes_no_record() {
    let machine = Machine::new();
    let repo = machine.repo();
    let said = ok(&machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hub",
            "skip",
            "--no-agents",
            "--no-collisions",
        ],
    ));
    assert!(said.contains("No changes made."), "{said}");
    assert!(!machine.config().join("quickstart.json").exists());
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn run_with_hub_port_retry<F>(run: F) -> Output
where
    F: FnMut(&str) -> Output,
{
    run_with_hub_port_retry_using(unused_port, run)
}

fn run_with_hub_port_retry_using<S, F>(mut select_port: S, mut run: F) -> Output
where
    S: FnMut() -> u16,
    F: FnMut(&str) -> Output,
{
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 0..MAX_ATTEMPTS {
        let port = select_port().to_string();
        let output = run(&port);
        let preflight = format!(
            "127.0.0.1:{port} is already in use; choose another --hub-port. No existing service was stopped."
        );
        let stdout_has_preflight = out(&output).lines().any(|line| line == preflight);
        if output.status.code() != Some(1) || !stdout_has_preflight || attempt + 1 == MAX_ATTEMPTS {
            return output;
        }
    }

    unreachable!("the bounded hub port retry loop always returns")
}

#[test]
fn hub_port_retry_reaches_docker_failure_after_an_occupied_first_port() {
    let machine = Machine::new();
    let repo = machine.repo();
    fake_docker(&machine);
    let held_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let held_port = held_listener.local_addr().unwrap().port();
    let mut first_attempt = true;
    let mut attempts = 0;
    let output = run_with_hub_port_retry_using(
        || {
            if first_attempt {
                first_attempt = false;
                held_port
            } else {
                unused_port()
            }
        },
        |port| {
            attempts += 1;
            machine
                .command(
                    repo.path(),
                    &[
                        "quickstart",
                        "--yes",
                        "--hub",
                        "local",
                        "--hub-runtime",
                        "docker",
                        "--hub-port",
                        port,
                        "--github-login",
                        "octo",
                        "--no-agents",
                        "--no-collisions",
                    ],
                )
                .output()
                .unwrap()
        },
    );
    drop(held_listener);
    assert!((2..=3).contains(&attempts));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(out(&output).contains("Docker could not pull"), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("registry denied pull"));
    assert!(!machine.home.path().join(".agentscm/hub").exists());
    assert!(!machine.config().join("quickstart.json").exists());
}

fn fake_docker(machine: &Machine) {
    let docker = machine.bin.path().join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/docker.log"
case "$1 $2" in
  'version '*|'compose version') exit 0 ;;
  'info --format') echo "${FAKE_DOCKER_PLATFORM:-linux/amd64}"; exit 0 ;;
  'context inspect') echo "${FAKE_DOCKER_ENDPOINT:-unix:///var/run/docker.sock}"; exit 0 ;;
  'image inspect')
    if [ "${FAKE_DOCKER_IMAGE_PRESENT:-}" = 1 ]; then echo 'linux/amd64'; exit 0; fi
    exit 1
    ;;
  'pull '*) echo 'fixture: registry denied pull' >&2; exit 17 ;;
  'compose -p')
    case "$*" in
      *' exec -T hub cat /data/secrets/bootstrap-setup-url') echo "$FAKE_HUB_URL/setup" ;;
    esac
    exit 0
    ;;
esac
exit 1
"#,
    )
    .unwrap();
    std::fs::set_permissions(docker, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn noninteractive_docker_bootstrap_uses_the_recorded_dynamic_port_and_exits_pending() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    fake_docker(&machine);
    let port = hub.url.rsplit(':').next().unwrap();
    let dir = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir": dir, "project": "fixture-hub", "port": port.parse::<u16>().unwrap()}
        })
        .to_string(),
    )
    .unwrap();
    let output = machine
        .command(
            repo.path(),
            &[
                "quickstart",
                "--yes",
                "--hub",
                "local",
                "--hub-runtime",
                "docker",
                "--hub-port",
                port,
                "--hub-project",
                "fixture-hub",
                "--github-login",
                "octo",
                "--no-agents",
                "--no-collisions",
            ],
        )
        .env("FAKE_DOCKER_IMAGE_PRESENT", "1")
        .env("FAKE_HUB_URL", &hub.url)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains(&format!(
            "Open this link (it works once, for 10 minutes):\n  {}/setup",
            hub.url
        )),
        "{said}"
    );
    assert!(
        said.contains(&format!(
            "--hub-runtime docker --hub-port {port} --hub-project fixture-hub"
        )),
        "{said}"
    );
    assert!(last_line(&output).contains("the local hub is waiting for GitHub App setup"));
    assert!(machine.connections().is_empty());
}

#[test]
fn invalid_local_runtime_project_and_port_fail_without_setup_files() {
    let cases: &[(&[&str], i32, &str)] = &[
        (&["--hub-runtime", "other"], 2, "invalid value"),
        (
            &["--hub-runtime", "docker", "--hub-port", "0"],
            2,
            "invalid value",
        ),
        (
            &["--hub-runtime", "docker", "--hub-project", "Bad_Name"],
            1,
            "--hub-project must start with a lowercase letter",
        ),
    ];
    for (extra, code, message) in cases {
        let machine = Machine::new();
        let repo = machine.repo();
        fake_docker(&machine);
        let mut args = vec![
            "quickstart",
            "--yes",
            "--hub",
            "local",
            "--github-login",
            "octo",
            "--no-agents",
            "--no-collisions",
        ];
        args.extend(*extra);
        let output = machine.run(repo.path(), &args);
        assert_eq!(output.status.code(), Some(*code), "{output:?}");
        assert!(
            format!(
                "{}{}",
                out(&output),
                String::from_utf8_lossy(&output.stderr)
            )
            .contains(message),
            "{output:?}"
        );
        assert!(!machine.home.path().join(".agentscm/hub").exists());
        assert!(!machine.config().join("quickstart.json").exists());
    }
}

#[test]
fn failed_hub_pull_leaves_no_setup_files_or_record_and_honors_platform() {
    for platform in [None, Some("linux/arm64")] {
        let machine = Machine::new();
        let repo = machine.repo();
        fake_docker(&machine);
        let result = run_with_hub_port_retry(|port| {
            let mut command = machine.command(
                repo.path(),
                &[
                    "quickstart",
                    "--yes",
                    "--hub",
                    "local",
                    "--hub-runtime",
                    "docker",
                    "--hub-port",
                    port,
                    "--github-login",
                    "octo",
                    "--no-agents",
                    "--no-collisions",
                ],
            );
            if let Some(platform) = platform {
                command.env("DOCKER_DEFAULT_PLATFORM", platform);
            }
            command.output().unwrap()
        });
        assert_eq!(result.status.code(), Some(1), "{result:?}");
        let said = out(&result);
        assert!(said.contains("Docker could not pull"), "{said}");
        if platform == Some("linux/arm64") {
            assert!(
                said.contains("DOCKER_DEFAULT_PLATFORM=linux/amd64"),
                "{said}"
            );
            assert!(
                said.contains("no architecture was switched automatically"),
                "{said}"
            );
        }
        assert!(said.contains("No changes made."), "{said}");
        assert!(String::from_utf8_lossy(&result.stderr).contains("registry denied pull"));
        assert!(!machine.home.path().join(".agentscm/hub").exists());
        assert!(!machine.config().join("quickstart.json").exists());
        let commands = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
        assert!(
            commands.contains(&format!(
                "pull --platform {} ghcr.io/counterbranch/agentscm:",
                platform.unwrap_or("linux/amd64")
            )),
            "{commands}"
        );
        assert!(!commands.contains("up -d"), "{commands}");
    }
}

#[test]
fn uninstall_explains_app_retention_and_keeps_hub_data_by_default() {
    let machine = Machine::new();
    let repo = machine.repo();
    fake_docker(&machine);
    let hub = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&hub).unwrap();
    std::fs::write(hub.join("compose.yaml"), "fixture").unwrap();
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir": hub, "project": "agentscm-hub"}
        })
        .to_string(),
    )
    .unwrap();
    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("will be kept for reinstall"), "{said}");
    assert!(
        said.contains("GitHub App and its installations stay on GitHub"),
        "{said}"
    );
    assert!(hub.join("compose.yaml").exists());
    let commands = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert!(commands.contains("down"));
    assert!(!commands.contains("down -v"));
}

/// The test binary placed in the installer's directory, so a run from it is recorded and undoable.
fn install_dir_program(machine: &Machine) -> PathBuf {
    let bin = machine.bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    place_binary(&bin.join("agentscm-0.0.0"));
    symlink("agentscm-0.0.0", bin.join("agentscm")).unwrap();
    bin.join("agentscm")
}

fn run_as(machine: &Machine, link: &Path, cwd: &Path, args: &[&str]) -> Output {
    machine.command_as(link, cwd, args).output().unwrap()
}

#[test]
fn no_op_rerun_keeps_manifest_bytes_and_reports_no_changes() {
    let machine = Machine::transient();
    let (repo, link) = (machine.repo(), install_dir_program(&machine));
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--no-agents",
        "--no-collisions",
    ];
    ok(&run_as(&machine, &link, repo.path(), &args));
    let record = machine.config().join("quickstart.json");
    let before = std::fs::read(&record).unwrap();
    let said = ok(&run_as(&machine, &link, repo.path(), &args));
    assert!(said.contains("No changes made."), "{said}");
    assert!(said.contains("Quickstart's recorded setup:"), "{said}");
    assert_eq!(std::fs::read(record).unwrap(), before);
}

#[test]
fn no_op_rerun_repairs_a_world_readable_manifest_mode() {
    let machine = Machine::transient();
    let (repo, link) = (machine.repo(), install_dir_program(&machine));
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--no-agents",
        "--no-collisions",
    ];
    ok(&run_as(&machine, &link, repo.path(), &args));
    let record = machine.config().join("quickstart.json");
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = ok(&run_as(&machine, &link, repo.path(), &args));
    assert_eq!(mode(&record), 0o600);
    assert!(!said.contains("No changes made."), "{said}");
}

#[test]
fn malformed_custom_image_never_reaches_image_inspect_or_pull() {
    for image in ["--help", "-q", "bad image", "bad\nimage", "${IMAGE}"] {
        let machine = Machine::new();
        let repo = machine.repo();
        fake_docker(&machine);
        let output = run_with_hub_port_retry(|port| {
            machine
                .command(
                    repo.path(),
                    &[
                        "quickstart",
                        "--yes",
                        "--hub",
                        "local",
                        "--hub-runtime",
                        "docker",
                        "--hub-port",
                        port,
                        "--github-login",
                        "octo",
                        "--no-agents",
                        "--no-collisions",
                    ],
                )
                .env("AGENTSCM_HUB_IMAGE", image)
                .output()
                .unwrap()
        });
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let said = out(&output);
        assert!(
            said.contains("AGENTSCM_HUB_IMAGE must be an image reference"),
            "{said}"
        );
        assert!(said.contains("No changes made."), "{said}");
        let commands = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
        assert!(
            !commands.contains("image inspect") && !commands.contains("pull"),
            "{commands}"
        );
        assert!(!machine.home.path().join(".agentscm/hub").exists());
        assert!(!machine.config().join("quickstart.json").exists());
    }
}

#[test]
fn custom_image_drops_previous_platform_choice_and_failed_pull_preserves_existing_files() {
    let machine = Machine::new();
    let repo = machine.repo();
    fake_docker(&machine);
    let dir = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&dir).unwrap();
    let old_env = "DOCKER_DEFAULT_PLATFORM=linux/amd64\nAGENTSCM_ADMINS=github:octo\n";
    std::fs::write(dir.join(".env"), old_env).unwrap();
    std::fs::write(dir.join("compose.yaml"), "previous compose\n").unwrap();
    let output = run_with_hub_port_retry(|port| {
        machine
            .command(
                repo.path(),
                &[
                    "quickstart",
                    "--yes",
                    "--hub",
                    "local",
                    "--hub-runtime",
                    "docker",
                    "--hub-port",
                    port,
                    "--github-login",
                    "octo",
                    "--no-agents",
                    "--no-collisions",
                ],
            )
            .env("AGENTSCM_HUB_IMAGE", "example.invalid/custom:arm64")
            .output()
            .unwrap()
    });
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    let commands = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert!(
        commands.contains("pull --platform linux/amd64 example.invalid/custom:arm64"),
        "{commands}"
    );
    assert_eq!(std::fs::read_to_string(dir.join(".env")).unwrap(), old_env);
    assert_eq!(
        std::fs::read_to_string(dir.join("compose.yaml")).unwrap(),
        "previous compose\n"
    );
    assert!(said.contains("No changes made."), "{said}");
    assert!(!machine.config().join("quickstart.json").exists());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_fixture(machine: &Machine, hub: &Hub) -> (PathBuf, PathBuf) {
    use sha2::{Digest, Sha256};
    let digest = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let home = machine.home.path();
    let project = "fixture-native";
    let config = home.join(".agentscm/hub-native").join(project);
    let data = config.join("data");
    std::fs::create_dir_all(data.join("secrets")).unwrap();
    std::fs::create_dir_all(machine.bin_dir()).unwrap();
    let executable = machine
        .bin_dir()
        .join(format!("agentscm-{}", env!("CARGO_PKG_VERSION")));
    place_binary(&executable);
    symlink(
        executable.file_name().unwrap(),
        machine.bin_dir().join("agentscm"),
    )
    .unwrap();
    let executable = executable.canonicalize().unwrap();
    let plist = home.join("Library/LaunchAgents/com.counterbranch.agentscm.fixture-native.plist");
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(&plist, "previous plist").unwrap();
    std::fs::create_dir_all(machine.config()).unwrap();
    let label = format!(
        "com.counterbranch.agentscm.{project}.{}",
        &digest(home.as_os_str().as_encoded_bytes())[..10]
    );
    let record = json!({
        "label": label, "plist": plist, "config_dir": config, "data_dir": data,
        "executable": executable, "executable_sha256": digest(&std::fs::read(&executable).unwrap()),
        "plist_sha256": digest(b"previous plist"), "port": hub.url.rsplit(':').next().unwrap().parse::<u16>().unwrap(),
        "project": project, "environment": {"AGENTSCM_DATABASE_URL": format!("sqlite://{}",data.join("hub.sqlite").display())}
    });
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({"native_hub":record,"bin_dir":machine.bin_dir()}).to_string(),
    )
    .unwrap();
    std::fs::write(
        data.join("secrets/con_fixture.json"),
        json!({"client_id":"fixture-client","slug":"fixture-app"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        data.join("secrets/con_fixture.client_secret"),
        "synthetic-secret",
    )
    .unwrap();
    symlink("/usr/bin/id", machine.bin.path().join("id")).unwrap();
    let launchctl = machine.bin.path().join("launchctl");
    std::fs::write(&launchctl, r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/launchctl.log"
case "$1" in
 print)
  case "$FAKE_PRINT_MODE" in
   unavailable) echo "fixture: GUI domain unavailable" >&2; exit 5 ;;
   absent) printf 'Could not find service "%s" in domain for user gui\n' "${2##*/}" >&2; exit 113 ;;
  esac
  printf 'program = %s\n' "$FAKE_EXECUTABLE" ;;
 bootstrap|bootout)
  if /usr/bin/grep -q AGENTSCM_ADMINS "$FAKE_PLIST"; then
   [ "$FAKE_FAILURE" != "$1" ] || { echo "fixture $1 failed" >&2; exit 17; }
   if [ "$FAKE_FAILURE" = health ] && [ "$1" = bootstrap ]; then printf broken > "$FAKE_HEALTH_MARKER"; fi
  else
   if [ -n "$FAKE_HEALTH_MARKER" ]; then printf ok > "$FAKE_HEALTH_MARKER"; fi
  fi ;;
esac
exit 0
"#).unwrap();
    std::fs::set_permissions(launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    (plist, executable)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_configuration_failure_restores_plist_record_and_previous_service() {
    for failure in ["bootstrap", "bootout", "health"] {
        let machine = Machine::new();
        let marker = machine.home.path().join("health-marker");
        let hub = Hub::start_with_health_guard(Some(marker.clone()));
        let repo = machine.repo();
        let (plist, _) = native_fixture(&machine, &hub);
        let output = machine
            .command(
                repo.path(),
                &[
                    "quickstart",
                    "--yes",
                    "--hub",
                    "local",
                    "--hub-runtime",
                    "native",
                    "--github-login",
                    "octo",
                    "--no-agents",
                    "--no-collisions",
                ],
            )
            .env("FAKE_FAILURE", failure)
            .env("FAKE_PLIST", &plist)
            .env("FAKE_HEALTH_MARKER", &marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{failure}: {output:?}");
        assert!(
            out(&output).contains("previous Hub configuration was restored"),
            "{failure}: {output:?}"
        );
        assert!(
            !std::fs::read_to_string(&plist)
                .unwrap()
                .contains("AGENTSCM_ADMINS")
        );
        let record = json_file(&machine.config().join("quickstart.json"));
        assert!(
            record["native_hub"]["environment"]
                .get("AGENTSCM_ADMINS")
                .is_none()
        );
        let response = std::net::TcpStream::connect(hub.url.trim_start_matches("http://"));
        assert!(
            response.is_ok(),
            "previous fixture service remains reachable"
        );
        let calls = std::fs::read_to_string(machine.home.path().join("launchctl.log")).unwrap();
        assert!(
            calls
                .lines()
                .filter(|line| line.starts_with("bootstrap "))
                .count()
                >= if failure == "bootout" { 1 } else { 3 },
            "{calls}"
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_uninstall_rejects_another_home_and_stops_when_the_executable_is_missing() {
    let (machine, other, hub) = (Machine::new(), Machine::new(), Hub::start());
    let repo = machine.repo();
    let (other_plist, _) = native_fixture(&other, &hub);
    let forged = json_file(&other.config().join("quickstart.json"));
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({"native_hub":forged["native_hub"]}).to_string(),
    )
    .unwrap();
    let output = machine.run(repo.path(), &["uninstall", "--yes", "--remove-hub-data"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(other_plist.exists());
    assert!(
        other
            .home
            .path()
            .join(".agentscm/hub-native/fixture-native/data/secrets/con_fixture.json")
            .exists()
    );
    assert!(!machine.home.path().join("launchctl.log").exists());
    let (plist, executable) = native_fixture(&machine, &hub);
    std::fs::remove_file(executable).unwrap();
    let output = machine.run(repo.path(), &["uninstall", "--yes"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "binary drift remains recorded"
    );
    assert!(
        !plist.exists(),
        "owned service cleanup must not depend on executable availability: {output:?}"
    );
    assert!(
        machine
            .home
            .path()
            .join(".agentscm/hub-native/fixture-native/retained.json")
            .exists()
    );
    let record = json_file(&machine.config().join("quickstart.json"));
    assert!(record["native_hub"].is_null());
    assert!(record["bin_dir"].is_string());
}

#[test]
fn docker_platform_and_context_selection_use_the_effective_local_daemon() {
    let cases = [
        (
            "linux/aarch64",
            "unix:///fixture.sock",
            "",
            "",
            "pull --platform linux/arm64",
        ),
        (
            "linux/x86_64",
            "unix:///fixture.sock",
            "tcp://remote.invalid:2376",
            "",
            "requires a local Docker context",
        ),
        (
            "linux/aarch64",
            "ssh://remote.invalid",
            "unix:///fixture.sock",
            "remote",
            "requires a local Docker context",
        ),
        (
            "linux/aarch64",
            "unix:///fixture.sock",
            "ssh://remote.invalid",
            "local",
            "pull --platform linux/arm64",
        ),
        (
            "linux/ppc64le",
            "unix:///fixture.sock",
            "",
            "",
            "unsupported architecture",
        ),
    ];
    for (platform, endpoint, host, context, expected) in cases {
        let machine = Machine::new();
        let repo = machine.repo();
        fake_docker(&machine);
        let output = run_with_hub_port_retry(|port| {
            machine
                .command(
                    repo.path(),
                    &[
                        "quickstart",
                        "--yes",
                        "--hub",
                        "local",
                        "--hub-runtime",
                        "docker",
                        "--hub-port",
                        port,
                        "--github-login",
                        "octo",
                        "--no-agents",
                        "--no-collisions",
                    ],
                )
                .env("FAKE_DOCKER_PLATFORM", platform)
                .env("FAKE_DOCKER_ENDPOINT", endpoint)
                .env("DOCKER_HOST", host)
                .env("DOCKER_CONTEXT", context)
                .env("AGENTSCM_HUB_IMAGE", "example.invalid/fixture:custom")
                .env("FAKE_DOCKER_IMAGE_PRESENT", "1")
                .output()
                .unwrap()
        });
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let calls = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
        let text = format!(
            "{}{}{}",
            out(&output),
            String::from_utf8_lossy(&output.stderr),
            calls
        );
        assert!(text.contains(expected), "{text}");
        assert!(!calls.contains("up -d"), "{calls}");
        assert!(
            !machine
                .home
                .path()
                .join(".agentscm/hub/compose.yaml")
                .exists()
        );
        assert!(!machine.config().join("quickstart.json").exists());
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_uninstall_recovers_a_missing_plist_but_preserves_unknown_service_state() {
    for mode in ["missing-plist", "unavailable", "absent"] {
        let (machine, hub) = (Machine::new(), Hub::start());
        let repo = machine.repo();
        let (plist, executable) = native_fixture(&machine, &hub);
        if mode == "missing-plist" {
            std::fs::remove_file(&plist).unwrap();
        }
        let output = machine
            .command(repo.path(), &["uninstall", "--yes"])
            .env("FAKE_PRINT_MODE", mode)
            .env("FAKE_EXECUTABLE", &executable)
            .output()
            .unwrap();
        if mode == "unavailable" {
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(plist.exists());
            assert!(machine.config().join("quickstart.json").exists());
            assert!(
                !std::fs::read_to_string(machine.home.path().join("launchctl.log"))
                    .unwrap()
                    .contains("bootout")
            );
        } else {
            ok(&output);
            assert!(!plist.exists());
            assert!(!machine.config().join("quickstart.json").exists());
            assert!(
                machine
                    .home
                    .path()
                    .join(".agentscm/hub-native/fixture-native/retained.json")
                    .exists()
            );
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn forged_native_dot_projects_cannot_delete_the_native_root_or_parent() {
    let (machine, hub) = (Machine::new(), Hub::start());
    let repo = machine.repo();
    let _ = native_fixture(&machine, &hub);
    let original = json_file(&machine.config().join("quickstart.json"));
    let marker = machine.home.path().join(".agentscm/keep.txt");
    let native_marker = machine.home.path().join(".agentscm/hub-native/keep.txt");
    std::fs::write(&marker, "preserve").unwrap();
    std::fs::write(&native_marker, "preserve").unwrap();
    for project in [".", ".."] {
        let mut forged = original.clone();
        let r = &mut forged["native_hub"];
        r["project"] = json!(project);
        use sha2::{Digest, Sha256};
        let home_hash = format!(
            "{:x}",
            Sha256::digest(machine.home.path().as_os_str().as_encoded_bytes())
        );
        r["label"] = json!(format!(
            "com.counterbranch.agentscm.{project}.{}",
            &home_hash[..10]
        ));
        r["config_dir"] = json!(
            machine
                .home
                .path()
                .join(".agentscm/hub-native")
                .join(project)
        );
        r["data_dir"] = json!(Path::new(r["config_dir"].as_str().unwrap()).join("data"));
        r["plist"] = json!(
            machine
                .home
                .path()
                .join("Library/LaunchAgents")
                .join(format!("com.counterbranch.agentscm.{project}.plist"))
        );
        std::fs::write(Path::new(r["plist"].as_str().unwrap()), "previous plist").unwrap();
        std::fs::write(machine.config().join("quickstart.json"), forged.to_string()).unwrap();
        let output = machine.run(repo.path(), &["uninstall", "--yes", "--remove-hub-data"]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "preserve");
        assert_eq!(std::fs::read_to_string(&native_marker).unwrap(), "preserve");
        assert!(!machine.home.path().join("launchctl.log").exists());
    }
}

#[test]
fn overlapping_install_and_uninstall_preserve_ownership_and_release_after_exit() {
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    let machine = Machine::transient();
    let bin = machine.bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    place_binary(&bin.join("agentscm-0.0.0"));
    symlink("agentscm-0.0.0", bin.join("agentscm")).unwrap();
    let link = bin.join("agentscm");
    let first_repo = machine.repo();
    let second_repo = machine.repo_at("git@github.com:acme/other.git");
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--no-agents",
        "--no-collisions",
    ];
    // The first child pauses in status after loading its ownership record. A second
    // installer must be refused before it can load and later overwrite stale state.
    let pause = |machine: &Machine| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream.read_exact(&mut [0; 1]).unwrap();
            entered_tx.send(()).unwrap();
            let _ = resume_rx.recv_timeout(Duration::from_secs(30));
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}",
            );
        });
        // Status asks the hub of the checkout's saved connection, so that is where it pauses.
        let connection = json!({"version": 1, "connections": [{"root": first_repo.path().canonicalize().unwrap(),
            "origin_host": "github.com", "hub": url, "repo": REPO,
            "token_file": machine.home.path().join("absent.token")}]});
        std::fs::create_dir_all(machine.config()).unwrap();
        std::fs::write(
            machine.config().join("connections.json"),
            connection.to_string(),
        )
        .unwrap();
        let child = machine
            .command_as(&link, first_repo.path(), &args)
            .spawn()
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        (child, resume_tx, server)
    };
    let (first, resume, server) = pause(&machine);
    let record = machine.config().join("quickstart.json");
    let record_before = std::fs::read(&record).ok();
    let second = machine.run(second_repo.path(), &args);
    let uninstall = machine.run(first_repo.path(), &["uninstall", "--yes"]);
    let record_while_paused = std::fs::read(&record).ok();
    resume.send(()).unwrap();
    let first_output = first.wait_with_output().unwrap();
    server.join().unwrap();
    for output in [&second, &uninstall] {
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("another agentscm quickstart or uninstall is running"),
            "{output:?}"
        );
    }
    assert_eq!(record_before, record_while_paused);
    ok(&first_output);
    assert!(record.exists());
    ok(&machine
        .command_as(&link, second_repo.path(), &args)
        .output()
        .unwrap());

    // A killed owner releases the operating-system lock; the stable lock file is
    // reused, rather than removed and recreated while another process may hold it.
    let lock_path = machine.config().join("quickstart.lock");
    let lock_inode = {
        use std::os::unix::fs::MetadataExt;
        lock_path.metadata().unwrap().ino()
    };
    let (mut killed, resume, server) = pause(&machine);
    killed.kill().unwrap();
    killed.wait().unwrap();
    let _ = resume.send(());
    server.join().unwrap();
    // The scaffolding connection would keep the program files, so drop it first.
    std::fs::remove_file(machine.config().join("connections.json")).unwrap();
    ok(&machine.run(first_repo.path(), &["uninstall", "--yes"]));
    assert!(!record.exists());
    use std::os::unix::fs::MetadataExt;
    assert_eq!(lock_path.metadata().unwrap().ino(), lock_inode);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_program_repairs_are_reported_but_true_noop_preserves_record() {
    for missing in ["link", "versioned"] {
        let (machine, hub) = (Machine::new().with_claude(), Hub::start());
        let repo = machine.repo();
        let (plist, executable) = native_fixture(&machine, &hub);
        let token = hub.token_file(machine.home.path());
        let args = [
            "quickstart",
            "--yes",
            "--hub",
            "local",
            "--hub-runtime",
            "native",
            "--github-login",
            "octo",
            "--agent-token-file",
            token.to_str().unwrap(),
            "--no-collisions",
        ];
        let run = || {
            machine
                .command(repo.path(), &args)
                .env("FAKE_PLIST", &plist)
                .output()
                .unwrap()
        };
        ok(&run());
        let record = machine.config().join("quickstart.json");
        let before = std::fs::read(&record).unwrap();
        let stable = machine.bin_dir().join("agentscm");
        std::fs::remove_file(if missing == "link" {
            &stable
        } else {
            &executable
        })
        .unwrap();
        let repaired = ok(&run());
        assert!(stable.is_file(), "{missing}");
        assert!(
            !repaired.contains("No changes made."),
            "{missing}: {repaired}"
        );
        assert_eq!(std::fs::read(&record).unwrap(), before, "{missing}");
        let noop = ok(&run());
        assert!(noop.contains("No changes made."), "{missing}: {noop}");
        assert_eq!(std::fs::read(&record).unwrap(), before, "{missing}");
    }
}

/// A stand-in for the `claude` CLI: it keeps the plugin and marketplace state in files next
/// to its log and answers the calls `client setup` and `client remove` make.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
S="__STATE__"
printf '%s\n' "$*" >> "$S/log"
touch "$S/plugins" "$S/markets"
for last in "$@"; do :; done
ok() { printf '{"outcome":"ok"}\n'; }
list() {
  printf '['
  first=1
  while IFS='|' read -r a b c; do
    [ -n "$a" ] || continue
    [ "$first" = 1 ] || printf ','
    first=0
    if [ "$1" = plugins ]; then
      printf '{"id":"%s","scope":"user","enabled":true,"version":"%s","installPath":"%s"}' "$a" "$b" "$c"
    else
      printf '{"name":"%s","path":"%s"}' "$a" "$b"
    fi
  done < "$S/$1"
  printf ']\n'
}
install() {
  path=$(cut -d'|' -f2 "$S/markets")
  src=$(sed -n 's/.*"source": *"\([^"]*\)".*/\1/p' "$path/.claude-plugin/marketplace.json" | head -1)
  plugin="$path/$src"
  version=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$plugin/.claude-plugin/plugin.json" | head -1)
  rm -rf "$S/cache"
  mkdir -p "$S/cache"
  cp -Rp "$plugin/." "$S/cache/"
  printf '%s|%s|%s\n' "$last" "$version" "$S/cache" > "$S/plugins"
  ok
}
case "$2" in
  list) list plugins ;;
  install|update) install ;;
  enable) ok ;;
  uninstall) : > "$S/plugins" ;;
  marketplace)
    case "$3" in
      list) list markets ;;
      add) printf 'agentscm-claude-client|%s\n' "$last" > "$S/markets"; ok ;;
      update) ok ;;
      remove) : > "$S/markets" ;;
    esac ;;
esac
"#;

impl Machine {
    /// Puts a fake `claude` first on PATH.
    fn with_claude(mut self) -> Self {
        let state = self.home.path().join("fake-claude");
        std::fs::create_dir_all(&state).unwrap();
        let script = self.bin.path().join("claude");
        std::fs::write(
            &script,
            FAKE_CLAUDE.replace("__STATE__", state.to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        self.system_path = true;
        self
    }

    /// Every call made to the fake `claude`, one line each.
    fn claude_log(&self) -> String {
        std::fs::read_to_string(self.home.path().join("fake-claude/log")).unwrap_or_default()
    }

    fn connections(&self) -> Vec<Value> {
        std::fs::read_to_string(self.config().join("connections.json"))
            .map(|text| {
                serde_json::from_str::<Value>(&text).unwrap()["connections"]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .unwrap_or_default()
    }
}

/// The agents the hub's Connect agents page lists for the repository, as `(agent, state)`.
fn onboarding(hub: &Hub, token: &Path) -> Vec<(String, String)> {
    let secret = std::fs::read_to_string(token).unwrap();
    let reply = hub._runtime.block_on(async {
        reqwest::Client::new()
            .get(format!("{}/v1/hub/onboarding", hub.url))
            .bearer_auth(secret.trim())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .map(|text| serde_json::from_str::<Value>(&text).unwrap())
            .unwrap()
    });
    reply
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["agent"].as_str().unwrap().to_owned(),
                row["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Runs `agentscm` with `input` on stdin and returns what it printed.
fn feed(machine: &Machine, cwd: &Path, args: &[&str], input: &str) -> Output {
    let mut child = machine
        .command(cwd, args)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn fresh_quickstart_connects_claude_code_through_client_setup_and_the_connect_page_sees_it() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let output = machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"]));
    let said = ok(&output);
    assert_eq!(
        last_line(&output),
        format!("Live warnings: on (hub {}, repo acme/widgets)", hub.url)
    );
    assert!(said.contains("agents set up: claude"), "{said}");

    let root = repo.path().canonicalize().unwrap();
    let connections = machine.connections();
    assert_eq!(connections.len(), 1, "{connections:?}");
    assert_eq!(connections[0]["root"], root.to_str().unwrap());
    assert_eq!(connections[0]["repo"], REPO);
    assert_eq!(connections[0]["hub"], hub.url.as_str());
    assert_eq!(connections[0]["agents"], json!(["claude"]));
    let mcp = json_file(&repo.path().join(".mcp.json"));
    let server = &mcp["mcpServers"]["agentscm"];
    assert_eq!(server["type"], "stdio");
    assert!(
        server["args"].to_string().contains(root.to_str().unwrap()),
        "{server}"
    );
    let log = machine.claude_log();
    assert!(
        log.contains("plugin install --scope user --json agentscm@agentscm-claude-client"),
        "{log}"
    );
    // Quickstart no longer writes its own hooks, hub.json or per-repository token files.
    assert!(!machine.config().join("hub.json").exists());
    assert!(!machine.home.path().join(".claude").exists());
    assert!(!repo.path().join(".claude").exists());
    let record = json_file(&machine.config().join("quickstart.json"));
    assert_eq!(record["clients"], json!([root.to_str().unwrap()]));
    assert!(record.get("hooks").is_none() && record.get("tokens").is_none());

    // Nothing is proven until a native session and the MCP server have both spoken.
    assert!(onboarding(&hub, &token).is_empty());
    let session = "quickstart-session";
    let hook = |event: &str, tool: bool| {
        let mut payload = json!({"session_id": session, "cwd": root, "hook_event_name": event});
        if tool {
            payload["tool_name"] = json!("Write");
            payload["tool_input"] = json!({"file_path": root.join("src/new.rs")});
        }
        ok(&feed(
            &machine,
            &root,
            &["--no-telemetry", "hook", "claude", "--client-connection"],
            &payload.to_string(),
        ))
    };
    hook("SessionStart", false);
    let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
        "name":"agentscm_setup_status","arguments":{"agent":"claude","session":session}}});
    let mcp = ok(&feed(
        &machine,
        &root,
        &["client", "mcp", "--root", root.to_str().unwrap()],
        &format!("{call}\n"),
    ));
    let reply: Value = serde_json::from_str(mcp.lines().next().unwrap()).unwrap();
    let status: Value =
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["mcp_verified"], true, "{reply}");
    assert_eq!(
        onboarding(&hub, &token),
        [("claude".to_owned(), "unverified".to_owned())]
    );
    hook("PreToolUse", true);
    assert_eq!(
        onboarding(&hub, &token),
        [("claude".to_owned(), "ready".to_owned())]
    );
}

#[test]
fn uninstall_removes_everything_quickstart_set_up() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    assert_eq!(machine.connections().len(), 1);
    assert!(repo.path().join(".mcp.json").exists());

    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("Removed the client connection"), "{said}");
    assert!(machine.connections().is_empty());
    // The token file was the user's own, so it stays; nothing managed is left behind.
    assert!(token.exists());
    assert!(names_in(&machine.config().join("tokens")).is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    let log = machine.claude_log();
    assert!(
        log.contains("plugin uninstall --scope user agentscm@agentscm-claude-client"),
        "{log}"
    );
    assert!(
        log.contains("plugin marketplace remove agentscm-claude-client"),
        "{log}"
    );
    assert!(!machine.config().join("quickstart.json").exists());
    assert!(out(&machine.run(repo.path(), &["uninstall", "--yes"])).contains("Nothing to undo"));
}

#[test]
fn uninstall_keeps_what_the_user_added_to_mcp_json_and_ends_the_sign_in() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    std::fs::write(
        repo.path().join(".mcp.json"),
        r#"{"mcpServers":{"other":{"command":"x"}}}"#,
    )
    .unwrap();
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    assert!(json_file(&repo.path().join(".mcp.json"))["mcpServers"]["agentscm"].is_object());
    let session = machine.config().join("hub-session.json");
    std::fs::write(
        &session,
        json!({"hub": hub.url, "session": "x", "expires_at": 0}).to_string(),
    )
    .unwrap();
    let record_path = machine.config().join("quickstart.json");
    let mut record = json_file(&record_path);
    record["session"] = json!({"hub": hub.url});
    std::fs::write(&record_path, record.to_string()).unwrap();

    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("Signed out of"), "{said}");
    assert!(!session.exists() && !record_path.exists());
    assert_eq!(
        json_file(&repo.path().join(".mcp.json")),
        json!({"mcpServers":{"other":{"command":"x"}}})
    );
}

#[test]
fn several_repositories_share_one_hub_with_their_own_connections() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let (one, two) = (
        machine.repo(),
        machine.repo_at("https://github.com/acme/gadgets.git"),
    );
    let token_one = hub.token_file_for(machine.home.path(), "acme/widgets", "one.token");
    let token_two = hub.token_file_for(machine.home.path(), "acme/gadgets", "two.token");
    for (dir, token) in [(&one, &token_one), (&two, &token_two)] {
        ok(&machine.run(dir.path(), &quick(&hub.url, token, &["--no-collisions"])));
    }
    let connections = machine.connections();
    assert_eq!(connections.len(), 2, "{connections:?}");
    let repos: Vec<_> = connections
        .iter()
        .map(|c| c["repo"].as_str().unwrap())
        .collect();
    assert!(repos.contains(&"acme/widgets") && repos.contains(&"acme/gadgets"));
    assert_eq!(
        json_file(&machine.config().join("quickstart.json"))["clients"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Each checkout announces with its own connection's token.
    ok(&edit(&machine, one.path(), "a", "src/one.rs"));
    ok(&edit(&machine, two.path(), "b", "src/two.rs"));
    assert_eq!(
        live_paths(&hub, "acme/widgets"),
        [vec!["src/one.rs".to_owned()]]
    );
    assert_eq!(
        live_paths(&hub, "acme/gadgets"),
        [vec!["src/two.rs".to_owned()]]
    );

    ok(&machine.run(one.path(), &["uninstall", "--yes"]));
    assert!(machine.connections().is_empty());
    assert!(token_one.exists() && token_two.exists());
    assert!(!one.path().join(".mcp.json").exists() && !two.path().join(".mcp.json").exists());
}

#[test]
fn a_requested_hub_that_is_down_connects_no_agent() {
    let machine = Machine::new().with_claude();
    let repo = machine.repo();
    let url = closed_port_url();
    let token = machine.home.path().join("t.token");
    std::fs::write(&token, "tok\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = machine.run(repo.path(), &quick(&url, &token, &["--no-collisions"]));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains(&format!(
            "The hub at {url} isn't answering, so no agent was connected to it."
        )),
        "{said}"
    );
    assert!(machine.connections().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(machine.claude_log().is_empty());
    let last = last_line(&output);
    assert!(
        last.starts_with(&format!(
            "Live warnings: off — the hub at {url} isn't answering (start it, then run `"
        )) && last.ends_with(" quickstart` again)"),
        "{last}"
    );
}

#[test]
fn with_yes_and_no_hub_no_agent_is_set_up() {
    let machine = Machine::new().with_claude();
    let repo = machine.repo();
    let output = machine.run(
        repo.path(),
        &["quickstart", "--yes", "--hub", "skip", "--no-collisions"],
    );
    let said = ok(&output);
    assert!(
        said.contains("Not setting up claude: agents need a hub that answers."),
        "{said}"
    );
    assert!(machine.connections().is_empty() && machine.claude_log().is_empty());
    assert!(!repo.path().join(".claude").exists());
    assert!(!machine.config().join("quickstart.json").exists());
}

#[test]
fn yes_without_claude_sets_up_no_agent() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let output = machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"]));
    // The requested hub is up but this checkout is not connected, so the run is incomplete.
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains("Claude Code wasn't found, so I'm not setting up a coding agent"),
        "{said}"
    );
    assert!(machine.connections().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(last_line(&output).contains("this checkout isn't connected to the hub"));
}

#[test]
fn no_agents_skips_setup_even_with_claude_installed() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let output = machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--no-collisions", "--no-agents"]),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(machine.connections().is_empty() && machine.claude_log().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(last_line(&output).contains("this checkout isn't connected to the hub"));
}

#[test]
fn without_a_terminal_it_refuses_and_writes_nothing() {
    let machine = Machine::new().with_claude();
    let repo = machine.repo();
    let output = machine.run(repo.path(), &["quickstart"]);
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    for flag in [
        "--yes",
        "--hub local|existing|skip",
        "--hub-url",
        "--agent-token-file",
        "--agent claude|codex|cursor|copilot|gemini",
        "--no-agents",
        "--no-collisions",
    ] {
        assert!(said.contains(flag), "{flag}: {said}");
    }
    assert!(!repo.path().join(".claude").exists() && !repo.path().join(".mcp.json").exists());
    assert!(!machine.config().exists());
    assert!(!machine.home.path().join(".agentscm").exists());
    assert!(machine.claude_log().is_empty());
    // With nothing recorded there is nothing to refuse.
    let output = machine.run(repo.path(), &["uninstall"]);
    assert!(output.status.success());
    assert!(out(&output).contains("nothing was installed by quickstart"));
}

#[test]
fn yes_without_a_token_or_saved_sign_in_does_not_try_to_sign_in() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let output = machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hub",
            "existing",
            "--hub-url",
            &hub.url,
            "--no-collisions",
        ],
    );
    let said = out(&output);
    assert!(said.contains("pass --agent-token-file"), "{said}");
    assert!(machine.connections().is_empty() && machine.claude_log().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(!machine.config().join("hub-session.json").exists());
}

#[test]
fn a_foreign_agentscm_hook_makes_setup_refuse_before_anything_is_written() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let settings = repo.path().join(".claude/settings.local.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let foreign = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"/opt/agentscm intent check --hook claude"}]}]}}"#;
    std::fs::write(&settings, foreign).unwrap();
    let output = machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"]));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains("Couldn't finish setting up claude: ")
            && said.contains("existing AgentSCM Claude command hooks were found"),
        "{said}"
    );
    assert!(machine.connections().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), foreign);
    // Preflight runs first, so the plugin was never touched and nothing was minted.
    assert!(!machine.claude_log().contains("install"));
    assert!(!machine.config().join("tokens").exists());
    assert!(!said.contains("agents set up: claude"), "{said}");
}

#[test]
fn a_fresh_repository_says_there_is_nothing_to_compare() {
    let machine = Machine::new();
    let repo = machine.repo();
    let said = ok(&machine.run(
        repo.path(),
        &["quickstart", "--yes", "--hub", "skip", "--no-agents"],
    ));
    if !said.contains("Couldn't check") {
        assert!(
            said.contains("No other branches or worktrees to compare yet."),
            "{said}"
        );
    }
    assert!(!said.contains("0 branches/worktrees checked"), "{said}");
}

#[test]
fn uninstall_removes_the_program_directory_quickstart_created() {
    let (hub, machine) = (Hub::start(), Machine::transient().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    assert!(machine.bin_dir().is_dir());
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(!machine.home.path().join(".agentscm").exists());
}

#[test]
fn uninstall_without_a_terminal_still_needs_yes_when_something_is_recorded() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    let output = machine.run(repo.path(), &["uninstall"]);
    assert!(!output.status.success());
    assert!(repo.path().join(".mcp.json").exists());
    assert_eq!(machine.connections().len(), 1);
}
