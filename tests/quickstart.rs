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

#[test]
fn invalid_slack_config_is_rejected_before_quickstart_runtime_writes() {
    let machine = Machine::new();
    let repo = machine.repo();
    let config = machine.home.path().join("invalid-slack.json");
    std::fs::write(&config, "{}\n").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hub",
            "local",
            "--slack-config-file",
            config.to_str().unwrap(),
            "--no-collisions",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("the prepared Slack configuration is invalid")
    );
    assert!(!machine.home.path().join(".agentscm").exists());
    assert!(!machine.config().join("quickstart.lock").exists());
    assert!(!machine.config().join("quickstart.json").exists());
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
    assert!(
        said.contains("Repository files to review and commit"),
        "{said}"
    );
    // The committed files carry no hooks or MCP, so the per-machine command is printed.
    assert!(
        said.contains("Each developer connects their own checkout once for each agent they use:")
            && said.contains(&format!(
                "agentscm client setup --agent claude --hub {}",
                hub.url
            )),
        "{said}"
    );
}

#[test]
fn init_without_an_agent_prints_the_setup_command_for_this_machine_too() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let said = ok(&machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hub",
            "existing",
            "--hub-url",
            &hub.url,
            "--no-agents",
            "--no-collisions",
            "--init",
        ],
    ));
    assert!(repo.path().join("AGENTS.md").exists(), "{said}");
    assert!(machine.connections().is_empty());
    assert!(
        said.contains("Each developer connects their own checkout once for each agent they use:")
            && said.contains(&format!(
                "agentscm client setup --agent claude --hub {}",
                hub.url
            )),
        "{said}"
    );
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

#[test]
fn a_busy_hub_port_prints_a_rerun_command_with_a_free_port() {
    let machine = Machine::new();
    let repo = machine.repo();
    fake_docker(&machine);
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let output = machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--local",
            "--hub-runtime",
            "docker",
            "--hub-port",
            &port,
            "--github-login",
            "octo",
            "--no-agents",
            "--no-collisions",
        ],
    );
    drop(held);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    let hint = said
        .lines()
        .find_map(|line| line.strip_prefix("To use a free port, run: "))
        .unwrap_or_else(|| panic!("{said}"));
    let free: u16 = hint
        .rsplit_once(" quickstart --local --hub-port ")
        .unwrap_or_else(|| panic!("{hint}"))
        .1
        .parse()
        .unwrap();
    assert_ne!(free.to_string(), port);
    assert!(!machine.home.path().join(".agentscm/hub").exists());
}

fn fake_docker(machine: &Machine) {
    let body = machine.home.path().join(".quickstart-fake-docker.sh");
    std::fs::write(
        &body,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/docker.log"
case "$1 $2" in
  'ps -aq') exit 0 ;;
  'inspect --format'|'inspect '* ) exit 1 ;;
  'rm -f') exit 0 ;;
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
      *'agentscm-slack-pending'*) echo agentscm-slack-clean ;;
      *' exec -T hub cat /data/secrets/bootstrap-setup-url') echo "$FAKE_HUB_URL/setup" ;;
    esac
    exit 0
    ;;
esac
exit 1
"#,
    )
    .unwrap();
    std::fs::set_permissions(body, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/quickstart_fake_docker.sh"
        ),
        machine.bin.path().join("docker"),
    )
    .unwrap();
}

fn slack_install_fixture(machine: &Machine) -> PathBuf {
    let dir = machine.home.path().join("slack-install-fixture");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in [
        ("signing", b"synthetic-signing".as_slice()),
        ("bot", b"xoxb-synthetic".as_slice()),
        ("agent", &[b'a'; 64]),
    ] {
        std::fs::write(dir.join(name), bytes).unwrap();
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "version": 1, "app_id": "A123", "bot_id": "B123", "team_id": "T123",
            "hub_url": "https://hub.example.test/", "signing_secret_file": dir.join("signing"),
            "bot_token_file": dir.join("bot"), "database": dir.join("install.sqlite"),
            "bindings": [{"slack_user":"U123","provider":"github","host":"github.com",
                "forge_user_id":42,"repository":"acme/widgets","repository_id":101,
                "agent_token_file":dir.join("agent")}]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    config
}

fn recovery_fake_docker(machine: &Machine) {
    let body = machine.home.path().join(".quickstart-fake-docker.sh");
    std::fs::write(
        &body,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/docker.log"
case "$1 $2" in
  'ps -aq')
    if [ "${FAKE_FAIL_HELPER_CONFIRM:-}" = 1 ] && [ -e "$FAKE_HELPER_CONFIRM_ATTEMPT" ]; then exit 17; fi
    exit 0 ;;
  'inspect --format')
    if [ "${FAKE_FAIL_HELPER_CONFIRM:-}" = 1 ]; then /usr/bin/touch "$FAKE_HELPER_CONFIRM_ATTEMPT"; fi
    exit 1 ;;
  'inspect '* ) exit 1 ;;
  'rm -f') exit 0 ;;
  'version '*|'compose version') exit 0 ;;
  'info --format') echo linux/amd64; exit 0 ;;
  'context inspect') echo unix:///var/run/docker.sock; exit 0 ;;
  'image inspect')
    if [ "${FAKE_FAIL_AFTER_CLEAN:-}" = 1 ] && [ -e "$FAKE_RECOVERED_CLEAN" ]; then exit 17; fi
    echo linux/amd64; exit 0 ;;
  'compose -p')
    all="$*"
    case "$all" in
      *' exec -T hub sh -c ls /data/secrets/con_'*) exit 0 ;;
      *' exec -T hub cat /data/secrets/bootstrap-setup-url') echo "$FAKE_HUB_URL/setup"; exit 0 ;;
      *' up -d '*'--force-recreate'*) /usr/bin/touch "$FAKE_RESTARTED" "$FAKE_RUNNING"; exit 0 ;;
      *' down'*)
        /bin/rm -f "$FAKE_RUNNING"; /usr/bin/touch "$FAKE_STOPPED"
        if [ -n "${FAKE_STOPPED_AFTER_FINALIZE:-}" ] && [ -e "$FAKE_FINALIZE_FAILED" ]; then
          /usr/bin/touch "$FAKE_STOPPED_AFTER_FINALIZE"
        fi
        exit 0 ;;
      *'agentscm-slack-pending'*)
        if [ -e "$FAKE_PENDING" ]; then echo agentscm-slack-pending; else echo agentscm-slack-clean; fi
        exit 0 ;;
      *'agentscm-slack-rollback:%s'*)
        [ ! -e "$FAKE_RUNNING" ] || { /usr/bin/touch "$FAKE_LIVE_MUTATION"; exit 17; }
        if [ "${FAKE_RECOVER_CLEAN:-}" = 1 ]; then
          /usr/bin/touch "$FAKE_RECOVERED_CLEAN"
          echo agentscm-slack-clean
          exit 0
        fi
        /bin/chmod 700 "$FAKE_CONFIG_DIR"
        if [ "${FAKE_FIRST_GENERATION:-}" = 1 ]; then
          /bin/rm -f "$FAKE_VOLUME_CONFIG"
          echo agentscm-slack-rollback:
        else
          /bin/cp "$FAKE_OLD_CONFIG" "$FAKE_VOLUME_CONFIG"
          echo agentscm-slack-rollback:config.json,signing-secret,bot-token,agent-0.token
        fi
        exit 0 ;;
      *'agentscm-slack-commit-clean'*)
        [ ! -e "$FAKE_RUNNING" ] || { /usr/bin/touch "$FAKE_LIVE_MUTATION"; exit 17; }
        if [ -e "$FAKE_PENDING" ]; then /bin/rm -f "$FAKE_PENDING"; echo agentscm-slack-commit; else echo agentscm-slack-commit-clean; fi
        exit 0 ;;
      *'rm -rf "$backup" /data/secrets/.slack-next'*)
        if [ "${FAKE_FAIL_FINALIZE:-}" = 1 ] && [ ! -e "$FAKE_FINALIZE_FAILED" ]; then
          /usr/bin/touch "$FAKE_FINALIZE_FAILED"; exit 17
        fi
        /bin/rm -f "$FAKE_PENDING"; exit 0 ;;
      *'mv "$backup/commit-marker" "$backup/commit"'*)
        [ "${FAKE_FAIL_MARK:-}" != 1 ] || exit 17
        /usr/bin/touch "$FAKE_COMMIT_MARKED"; exit 0 ;;
      *'trap rollback EXIT HUP INT TERM'*)
        stage=
        for argument in "$@"; do case "$argument" in *:/run/agentscm-slack:ro) stage="${argument%:/run/agentscm-slack:ro}" ;; esac; done
        [ -n "$stage" ] && /bin/cp "$stage/config.json" "$FAKE_VOLUME_CONFIG"
        /usr/bin/touch "$FAKE_PENDING"
        if [ "${FAKE_LOCK_MANIFEST:-}" = 1 ]; then /bin/chmod 500 "$FAKE_CONFIG_DIR"; fi
        echo agentscm-slack-changed; exit 0 ;;
      *'/run/agentscm-slack:ro'*)
        stage=
        for argument in "$@"; do case "$argument" in *:/run/agentscm-slack:ro) stage="${argument%:/run/agentscm-slack:ro}" ;; esac; done
        case "$all" in
         *'source=/run/agentscm-slack'*)
          if [ -n "$stage" ] && /usr/bin/cmp -s "$stage/config.json" "$FAKE_VOLUME_CONFIG"; then
            echo agentscm-slack-unchanged
          else
            echo agentscm-slack-changed
          fi ;;
         *'rm -rf "$backup"'*)
          /bin/rm -f "$FAKE_PENDING"
          [ "${FAKE_FAIL_FINAL_COMMIT:-}" != 1 ] || exit 17 ;;
         *)
          if [ "${FAKE_FAIL_POST_VERIFY:-}" = 1 ] && [ -e "$FAKE_PENDING" ] && [ ! -e "$FAKE_POST_VERIFY_FAILED" ]; then
            /usr/bin/touch "$FAKE_POST_VERIFY_FAILED"; exit 17
          fi ;;
        esac
        exit 0 ;;
      *'rm -rf "$backup"'*)
        /bin/rm -f "$FAKE_PENDING"
        [ "${FAKE_FAIL_FINAL_COMMIT:-}" != 1 ] || exit 17
        exit 0 ;;
      *'rm -rf /data/secrets/.slack-next'*)
        [ ! -e "$FAKE_RUNNING" ] || { /usr/bin/touch "$FAKE_LIVE_MUTATION"; exit 17; }
        [ "${FAKE_FAIL_ROLLBACK:-}" != 1 ] || exit 17
        /bin/chmod 700 "$FAKE_CONFIG_DIR"
        /bin/cp "$FAKE_OLD_CONFIG" "$FAKE_VOLUME_CONFIG"
        echo agentscm-slack-rollback; exit 0 ;;
    esac
    if [ "${FAKE_FAIL_POST_VERIFY:-}" = 1 ] && [ -e "$FAKE_PENDING" ] && [ ! -e "$FAKE_POST_VERIFY_FAILED" ]; then
      /usr/bin/touch "$FAKE_POST_VERIFY_FAILED"; exit 17
    fi
    exit 0 ;;
esac
exit 1
"#,
    )
    .unwrap();
    std::fs::set_permissions(body, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/quickstart_fake_docker.sh"
        ),
        machine.bin.path().join("docker"),
    )
    .unwrap();
}

fn seed_docker_slack_receipt(setup: &Path) {
    let digest = "0".repeat(64);
    let receipt = [
        "config.json",
        "signing-secret",
        "bot-token",
        "agent-0.token",
    ]
    .into_iter()
    .map(|name| format!("{digest}  {name}\n"))
    .collect::<String>();
    let path = setup.join(".slack-bundle-receipt");
    std::fs::write(&path, receipt).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn docker_slack_recovery_gates_restart_and_retains_retry_evidence() {
    for (failure, expected_restart, expected_pending) in [
        ("post-verify-rollback", false, true),
        ("manifest-save", true, false),
        ("commit-marker", true, false),
        ("helper-confirm", false, true),
    ] {
        let (hub, machine) = (Hub::start(), Machine::new());
        let repo = machine.repo();
        recovery_fake_docker(&machine);
        let config = slack_install_fixture(&machine);
        let port = hub.url.rsplit(':').next().unwrap();
        let setup = machine.home.path().join(".agentscm/hub");
        std::fs::create_dir_all(&setup).unwrap();
        let old_env = "AGENTSCM_SLACK_FILE=/data/secrets/slack/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=config.json,signing-secret,bot-token,agent-0.token\n";
        let old_compose = "previous compose\n";
        std::fs::write(setup.join(".env"), old_env).unwrap();
        std::fs::write(setup.join("compose.yaml"), old_compose).unwrap();
        seed_docker_slack_receipt(&setup);
        std::fs::create_dir_all(machine.config()).unwrap();
        let old_manifest = json!({
            "local_hub": {"dir":setup,"project":"fixture-hub","port":port.parse::<u16>().unwrap()}
        })
        .to_string();
        std::fs::write(machine.config().join("quickstart.json"), &old_manifest).unwrap();
        let pending = machine.home.path().join("pending-slack-transaction");
        let restarted = machine.home.path().join("previous-hub-restarted");
        let running = machine.home.path().join("hub-running");
        let stopped = machine.home.path().join("hub-stopped");
        let live_mutation = machine.home.path().join("live-volume-mutation");
        let old_volume_config = machine.home.path().join("old-volume-config");
        let volume_config = machine.home.path().join("volume-config");
        std::fs::write(&old_volume_config, b"synthetic-old-credential-generation").unwrap();
        std::fs::write(&volume_config, b"synthetic-old-credential-generation").unwrap();
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
                "--hub-project",
                "fixture-hub",
                "--github-login",
                "octo",
                "--slack-config-file",
                config.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        );
        command
            .env("FAKE_CONFIG_DIR", machine.config())
            .env("FAKE_PENDING", &pending)
            .env("FAKE_RESTARTED", &restarted)
            .env("FAKE_RUNNING", &running)
            .env("FAKE_STOPPED", &stopped)
            .env("FAKE_LIVE_MUTATION", &live_mutation)
            .env("FAKE_OLD_CONFIG", &old_volume_config)
            .env("FAKE_VOLUME_CONFIG", &volume_config)
            .env(
                "FAKE_FINALIZE_FAILED",
                machine.home.path().join("finalizer-failed"),
            )
            .env(
                "FAKE_COMMIT_MARKED",
                machine.home.path().join("commit-marked"),
            )
            .env(
                "FAKE_POST_VERIFY_FAILED",
                machine.home.path().join("post-verify-failed"),
            )
            .env(
                "FAKE_HELPER_CONFIRM_ATTEMPT",
                machine.home.path().join("helper-confirm-attempt"),
            )
            .env("FAKE_HUB_URL", &hub.url);
        match failure {
            "post-verify-rollback" => {
                command
                    .env("FAKE_FAIL_POST_VERIFY", "1")
                    .env("FAKE_FAIL_ROLLBACK", "1");
            }
            "manifest-save" => {
                command.env("FAKE_LOCK_MANIFEST", "1");
            }
            "commit-marker" => {
                command.env("FAKE_FAIL_MARK", "1");
            }
            "helper-confirm" => {
                command
                    .env("FAKE_FAIL_MARK", "1")
                    .env("FAKE_FAIL_HELPER_CONFIRM", "1");
            }
            _ => unreachable!(),
        }
        let output = command.output().unwrap();
        let docker_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
        assert!(docker_log.contains("com.counterbranch.agentscm.slack-helper=1"));
        assert!(docker_log.contains("com.counterbranch.agentscm.slack-project=fixture-hub"));
        if failure != "manifest-save" {
            assert!(
                docker_log.contains("inspect --format"),
                "{failure}: {docker_log}"
            );
            assert!(
                docker_log.contains("ps -aq --no-trunc"),
                "{failure}: {docker_log}"
            );
        }
        assert!(!docker_log.contains("xoxb-synthetic"));
        assert_eq!(
            output.status.code(),
            Some(1),
            "{failure}: {output:?}\n{docker_log}"
        );
        let installed_env = std::fs::read_to_string(setup.join(".env")).unwrap();
        if expected_pending {
            assert!(
                installed_env.ends_with(old_env),
                "{failure}: {output:?}\n{docker_log}"
            );
            assert!(setup.join(".slack-bundle-receipt").exists());
        } else {
            assert_eq!(
                installed_env, old_env,
                "{failure}: {output:?}\n{docker_log}"
            );
            assert_eq!(
                std::fs::read_to_string(setup.join("compose.yaml")).unwrap(),
                old_compose
            );
        }
        assert_eq!(
            restarted.exists(),
            expected_restart,
            "{failure}: {output:?}\n{docker_log}"
        );
        assert_eq!(
            pending.exists(),
            expected_pending,
            "{failure}: {output:?}\n{docker_log}"
        );
        assert!(!live_mutation.exists(), "{failure}: {docker_log}");
        let restored: Value = json_file(&machine.config().join("quickstart.json"));
        assert_eq!(restored["local_hub"]["project"], "fixture-hub");
    }
}

#[test]
fn docker_ambiguous_final_commit_keeps_the_exact_new_generation() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    recovery_fake_docker(&machine);
    let config = slack_install_fixture(&machine);
    let port = hub.url.rsplit(':').next().unwrap();
    let setup = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&setup).unwrap();
    let old_env = "AGENTSCM_SLACK_FILE=/data/secrets/slack/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=config.json,signing-secret,bot-token,agent-0.token\n";
    std::fs::write(setup.join(".env"), old_env).unwrap();
    std::fs::write(setup.join("compose.yaml"), "previous compose\n").unwrap();
    seed_docker_slack_receipt(&setup);
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir":setup,"project":"fixture-hub","port":port.parse::<u16>().unwrap()}
        })
        .to_string(),
    )
    .unwrap();
    let pending = machine.home.path().join("pending-slack-transaction");
    let restarted = machine.home.path().join("hub-restarted");
    let running = machine.home.path().join("hub-running");
    let stopped = machine.home.path().join("hub-stopped");
    let live_mutation = machine.home.path().join("live-volume-mutation");
    let old_volume_config = machine.home.path().join("old-volume-config");
    let volume_config = machine.home.path().join("volume-config");
    std::fs::write(&old_volume_config, b"synthetic-old-credential-generation").unwrap();
    std::fs::write(&volume_config, b"synthetic-old-credential-generation").unwrap();
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
                "--slack-config-file",
                config.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        )
        .env("FAKE_CONFIG_DIR", machine.config())
        .env("FAKE_PENDING", &pending)
        .env("FAKE_RESTARTED", &restarted)
        .env("FAKE_RUNNING", &running)
        .env("FAKE_STOPPED", &stopped)
        .env("FAKE_LIVE_MUTATION", &live_mutation)
        .env("FAKE_OLD_CONFIG", &old_volume_config)
        .env("FAKE_VOLUME_CONFIG", &volume_config)
        .env(
            "FAKE_FINALIZE_FAILED",
            machine.home.path().join("finalizer-failed"),
        )
        .env(
            "FAKE_COMMIT_MARKED",
            machine.home.path().join("commit-marked"),
        )
        .env(
            "FAKE_POST_VERIFY_FAILED",
            machine.home.path().join("post-verify-failed"),
        )
        .env("FAKE_HUB_URL", &hub.url)
        .env("FAKE_FAIL_FINAL_COMMIT", "1")
        .output()
        .unwrap();
    let docker_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}\n{docker_log}");
    assert!(!pending.exists(), "{output:?}\n{docker_log}");
    assert!(stopped.exists() && restarted.exists() && running.exists());
    assert!(!live_mutation.exists());
    assert_ne!(
        std::fs::read(&volume_config).unwrap(),
        std::fs::read(&old_volume_config).unwrap(),
        "identical filenames must not let the old credential digest pass"
    );
    assert_ne!(
        std::fs::read_to_string(setup.join("compose.yaml")).unwrap(),
        "previous compose\n"
    );
}

#[test]
fn docker_finalizer_failure_stops_before_retrying_pending_recovery() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    recovery_fake_docker(&machine);
    let config = slack_install_fixture(&machine);
    let port = hub.url.rsplit(':').next().unwrap();
    let setup = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&setup).unwrap();
    let old_env = "AGENTSCM_SLACK_FILE=/data/secrets/slack/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=config.json,signing-secret,bot-token,agent-0.token\n";
    std::fs::write(setup.join(".env"), old_env).unwrap();
    std::fs::write(setup.join("compose.yaml"), "previous compose\n").unwrap();
    seed_docker_slack_receipt(&setup);
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir":setup,"project":"fixture-hub","port":port.parse::<u16>().unwrap()}
        })
        .to_string(),
    )
    .unwrap();
    let pending = machine.home.path().join("pending-slack-transaction");
    let restarted = machine.home.path().join("hub-restarted");
    let running = machine.home.path().join("hub-running");
    let stopped = machine.home.path().join("hub-stopped");
    let stopped_after_finalize = machine.home.path().join("stopped-after-finalizer");
    let live_mutation = machine.home.path().join("live-volume-mutation");
    let old_volume_config = machine.home.path().join("old-volume-config");
    let volume_config = machine.home.path().join("volume-config");
    let finalizer_failed = machine.home.path().join("finalizer-failed");
    std::fs::write(&old_volume_config, b"synthetic-old-credential-generation").unwrap();
    std::fs::write(&volume_config, b"synthetic-old-credential-generation").unwrap();
    let run = |fail_finalizer: bool| {
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
                "--hub-project",
                "fixture-hub",
                "--github-login",
                "octo",
                "--slack-config-file",
                config.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        );
        command
            .env("FAKE_CONFIG_DIR", machine.config())
            .env("FAKE_PENDING", &pending)
            .env("FAKE_RESTARTED", &restarted)
            .env("FAKE_RUNNING", &running)
            .env("FAKE_STOPPED", &stopped)
            .env("FAKE_STOPPED_AFTER_FINALIZE", &stopped_after_finalize)
            .env("FAKE_LIVE_MUTATION", &live_mutation)
            .env("FAKE_OLD_CONFIG", &old_volume_config)
            .env("FAKE_VOLUME_CONFIG", &volume_config)
            .env("FAKE_FINALIZE_FAILED", &finalizer_failed)
            .env(
                "FAKE_COMMIT_MARKED",
                machine.home.path().join("commit-marked"),
            )
            .env(
                "FAKE_POST_VERIFY_FAILED",
                machine.home.path().join("post-verify-failed"),
            )
            .env("FAKE_HUB_URL", &hub.url);
        if fail_finalizer {
            command
                .env("FAKE_LOCK_MANIFEST", "1")
                .env("FAKE_FAIL_FINALIZE", "1");
        }
        command.output().unwrap()
    };

    let first = run(true);
    let first_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(first.status.code(), Some(1), "{first:?}\n{first_log}");
    assert!(finalizer_failed.exists(), "{first:?}\n{first_log}");
    assert!(pending.exists(), "{first:?}\n{first_log}");
    assert!(!running.exists(), "{first:?}\n{first_log}");
    assert!(!live_mutation.exists(), "{first_log}");

    let second = run(false);
    let second_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(second.status.code(), Some(3), "{second:?}\n{second_log}");
    assert!(stopped_after_finalize.exists(), "{second:?}\n{second_log}");
    assert!(!pending.exists(), "{second:?}\n{second_log}");
    assert!(running.exists() && restarted.exists());
    assert!(!live_mutation.exists(), "{second_log}");
}

#[test]
fn fresh_docker_slack_failure_retains_an_empty_anchor_and_retries() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    recovery_fake_docker(&machine);
    let config = slack_install_fixture(&machine);
    let port = hub.url.rsplit(':').next().unwrap();
    let setup = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir":setup,"project":"fixture-hub","port":port.parse::<u16>().unwrap()}
        })
        .to_string(),
    )
    .unwrap();
    let pending = machine.home.path().join("pending-slack-transaction");
    let running = machine.home.path().join("hub-running");
    let old_volume = machine.home.path().join("unused-old-volume-config");
    let volume = machine.home.path().join("volume-config");
    std::fs::write(&old_volume, b"unused synthetic generation").unwrap();
    let run = |fail: bool| {
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
                "--hub-project",
                "fixture-hub",
                "--github-login",
                "octo",
                "--slack-config-file",
                config.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        );
        command
            .env("FAKE_CONFIG_DIR", machine.config())
            .env("FAKE_PENDING", &pending)
            .env("FAKE_RESTARTED", machine.home.path().join("hub-restarted"))
            .env("FAKE_RUNNING", &running)
            .env("FAKE_STOPPED", machine.home.path().join("hub-stopped"))
            .env(
                "FAKE_LIVE_MUTATION",
                machine.home.path().join("live-mutation"),
            )
            .env("FAKE_OLD_CONFIG", &old_volume)
            .env("FAKE_VOLUME_CONFIG", &volume)
            .env(
                "FAKE_FINALIZE_FAILED",
                machine.home.path().join("finalize-failed"),
            )
            .env(
                "FAKE_COMMIT_MARKED",
                machine.home.path().join("commit-marked"),
            )
            .env(
                "FAKE_POST_VERIFY_FAILED",
                machine.home.path().join("verify-failed"),
            )
            .env("FAKE_FIRST_GENERATION", "1")
            .env("FAKE_HUB_URL", &hub.url);
        if fail {
            command
                .env("FAKE_FAIL_POST_VERIFY", "1")
                .env("FAKE_FAIL_ROLLBACK", "1");
        }
        command.output().unwrap()
    };

    let first = run(true);
    let first_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(first.status.code(), Some(1), "{first:?}\n{first_log}");
    assert!(pending.exists(), "{first:?}\n{first_log}");
    assert_eq!(
        std::fs::read(setup.join(".slack-bundle-receipt.rollback")).unwrap(),
        b""
    );
    assert!(!running.exists());

    let second = run(false);
    let log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(second.status.code(), Some(3), "{second:?}\n{log}");
    assert!(!pending.exists());
    assert!(!setup.join(".slack-bundle-receipt.rollback").exists());
    assert!(setup.join(".slack-bundle-receipt").exists());
    assert!(running.exists());
}

#[test]
fn fresh_docker_finalized_rollback_crash_recovers_empty_generation() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    recovery_fake_docker(&machine);
    let config = slack_install_fixture(&machine);
    let port = hub.url.rsplit(':').next().unwrap();
    let setup = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&setup).unwrap();
    std::fs::write(setup.join("compose.yaml"), "stale fresh compose\n").unwrap();
    std::fs::write(
        setup.join(".env"),
        "UNRELATED=kept\nAGENTSCM_SLACK_FILE=/data/secrets/slack/config.json\nAGENTSCM_SLACK_BUNDLE_FILES=config.json,signing-secret,bot-token,agent-0.token\n",
    )
    .unwrap();
    let rollback_receipt = setup.join(".slack-bundle-receipt.rollback");
    std::fs::write(&rollback_receipt, b"").unwrap();
    std::fs::set_permissions(&rollback_receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("quickstart.json"),
        json!({
            "local_hub": {"dir":setup,"project":"fixture-hub","port":port.parse::<u16>().unwrap()}
        })
        .to_string(),
    )
    .unwrap();
    let pending = machine.home.path().join("pending-slack-transaction");
    let recovered_clean = machine.home.path().join("recovered-clean-generation");
    let running = machine.home.path().join("hub-running");
    let volume = machine.home.path().join("volume-config");
    let old_volume = machine.home.path().join("unused-old-volume-config");
    let run = |fail_after_clean: bool| {
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
                "--hub-project",
                "fixture-hub",
                "--github-login",
                "octo",
                "--slack-config-file",
                config.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        );
        command
            .env("FAKE_CONFIG_DIR", machine.config())
            .env("FAKE_PENDING", &pending)
            .env("FAKE_RESTARTED", machine.home.path().join("hub-restarted"))
            .env("FAKE_RUNNING", &running)
            .env("FAKE_STOPPED", machine.home.path().join("hub-stopped"))
            .env(
                "FAKE_LIVE_MUTATION",
                machine.home.path().join("live-mutation"),
            )
            .env("FAKE_OLD_CONFIG", &old_volume)
            .env("FAKE_VOLUME_CONFIG", &volume)
            .env(
                "FAKE_FINALIZE_FAILED",
                machine.home.path().join("finalize-failed"),
            )
            .env(
                "FAKE_COMMIT_MARKED",
                machine.home.path().join("commit-marked"),
            )
            .env(
                "FAKE_POST_VERIFY_FAILED",
                machine.home.path().join("verify-failed"),
            )
            .env("FAKE_FIRST_GENERATION", "1")
            .env("FAKE_RECOVER_CLEAN", "1")
            .env("FAKE_RECOVERED_CLEAN", &recovered_clean)
            .env("FAKE_HUB_URL", &hub.url);
        if fail_after_clean {
            command.env("FAKE_FAIL_AFTER_CLEAN", "1");
        }
        command.output().unwrap()
    };

    let first = run(true);
    let first_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(first.status.code(), Some(1), "{first:?}\n{first_log}");
    assert!(recovered_clean.exists(), "{first:?}\n{first_log}");
    let recovered_env = std::fs::read_to_string(setup.join(".env")).unwrap();
    assert!(recovered_env.contains("UNRELATED=kept"));
    assert!(!recovered_env.contains("AGENTSCM_SLACK_"));
    assert!(!rollback_receipt.exists());
    assert!(!setup.join(".slack-bundle-receipt").exists());
    assert!(!pending.exists() && !running.exists());
    assert!(first_log.contains("install.sqlite|install.sqlite-wal|install.sqlite-shm"));

    let second = run(false);
    let second_log = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    assert_eq!(second.status.code(), Some(3), "{second:?}\n{second_log}");
    let installed_env = std::fs::read_to_string(setup.join(".env")).unwrap();
    assert!(installed_env.contains("AGENTSCM_SLACK_FILE=/data/secrets/slack/config.json"));
    assert!(!rollback_receipt.exists() && !pending.exists());
    assert!(
        setup.join(".slack-bundle-receipt").exists() && running.exists(),
        "the retried first generation must be recorded and running"
    );
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

#[test]
fn a_docker_resume_without_slack_does_not_run_the_previous_image() {
    let machine = Machine::new();
    let repo = machine.repo();
    fake_docker(&machine);
    let dir = machine.home.path().join(".agentscm/hub");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), "AGENTSCM_ADMINS=github:octo\n").unwrap();
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
            .output()
            .unwrap()
    });
    // The fake registry refuses the pull; what matters is what ran before it.
    assert!(out(&output).contains("Docker could not pull"), "{output:?}");
    let commands = std::fs::read_to_string(machine.home.path().join("docker.log")).unwrap();
    // No Slack receipt on the host: the previous compose image is never run to look for a
    // pending Slack transaction, which needs that old image present.
    assert!(!commands.contains("agentscm-slack-pending"), "{commands}");
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
    for private in [&config, &data, &data.join("secrets")] {
        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
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
    let retained = config.join("retained.json");
    std::fs::write(
        &retained,
        json!({
            "project": project,
            "environment": {"AGENTSCM_DATABASE_URL": format!("sqlite://{}",data.join("hub.sqlite").display())},
            "slack_files": {}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(retained, std::fs::Permissions::from_mode(0o600)).unwrap();
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
   wrong-label) echo 'Could not find service "com.counterbranch.agentscm.other" in domain for user gui' >&2; exit 113 ;;
   # Stay well past the 30-second control deadline so load cannot turn this into a successful exit.
   timeout) exec /bin/sleep 120 ;;
   absent) printf 'Could not find service "%s" in domain for user gui\n' "${2##*/}" >&2; exit 113 ;;
  esac
  if [ -e "$HOME/launchctl-stopped" ]; then
   printf 'Could not find service "%s" in domain for user gui\n' "${2##*/}" >&2
   exit 113
  fi
  printf 'program = %s\n' "$FAKE_EXECUTABLE" ;;
 bootstrap|bootout)
  if [ "$FAKE_FAILURE" = bootout ] && [ "$1" = bootout ]; then echo "fixture bootout failed" >&2; exit 17; fi
  if /usr/bin/grep -q AGENTSCM_ADMINS "$FAKE_PLIST"; then
   if [ "$FAKE_FAILURE" = health-stop ] && [ "$1" = bootout ]; then echo "fixture recovery bootout failed" >&2; exit 17; fi
   [ "$FAKE_FAILURE" != "$1" ] || { echo "fixture $1 failed" >&2; exit 17; }
   if { [ "$FAKE_FAILURE" = health ] || [ "$FAKE_FAILURE" = health-stop ]; } && [ "$1" = bootstrap ]; then printf broken > "$FAKE_HEALTH_MARKER"; fi
  else
   if [ -n "$FAKE_HEALTH_MARKER" ]; then printf ok > "$FAKE_HEALTH_MARKER"; fi
  fi ;;
esac
case "$1" in
 bootout) /usr/bin/touch "$HOME/launchctl-stopped" ;;
 bootstrap) /bin/rm -f "$HOME/launchctl-stopped" ;;
esac
exit 0
"#).unwrap();
    std::fs::set_permissions(launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    (plist, executable)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_pending_recovery_requires_a_known_service_state() {
    for mode in ["unavailable", "wrong-label", "timeout"] {
        let (machine, hub) = (Machine::new(), Hub::start());
        let repo = machine.repo();
        let (plist, executable) = native_fixture(&machine, &hub);
        let config = machine
            .home
            .path()
            .join(".agentscm/hub-native/fixture-native");
        let pending = config.join("data/secrets/.slack-rollback");
        std::fs::create_dir(&pending).unwrap();
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o700)).unwrap();
        let marker = pending.join("pending.json");
        std::fs::write(&marker, b"synthetic pending evidence").unwrap();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
        let watched = [
            plist.clone(),
            config.join("retained.json"),
            config.join("data/secrets/con_fixture.json"),
            config.join("data/secrets/con_fixture.client_secret"),
            marker,
        ];
        let manifest_path = machine.config().join("quickstart.json");
        let mut before_record = json_file(&manifest_path)["native_hub"].clone();
        before_record["slack_files"] = json!({});
        let before: Vec<_> = watched
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();

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
            .env("FAKE_PRINT_MODE", mode)
            .env("FAKE_EXECUTABLE", &executable)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{mode}: {output:?}");
        for (path, expected) in watched.iter().zip(&before) {
            assert_eq!(&std::fs::read(path).unwrap(), expected, "{mode}: {path:?}");
        }
        assert_eq!(
            json_file(&manifest_path)["native_hub"],
            before_record,
            "{mode}: the owned native Hub record changed"
        );
        let calls = std::fs::read_to_string(machine.home.path().join("launchctl.log")).unwrap();
        assert_eq!(
            calls.lines().count(),
            1,
            "{mode}: recovery must not mutate or stop with unknown service state: {calls}"
        );
        assert!(calls.starts_with("print "), "{mode}: {calls}");
    }
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
fn native_uninstall_settles_unhealthy_recovery_without_restarting_the_hub() {
    for wipe in [false, true] {
        let machine = Machine::new();
        let marker = machine.home.path().join("health-marker");
        let hub = Hub::start_with_health_guard(Some(marker.clone()));
        let repo = machine.repo();
        let (plist, _) = native_fixture(&machine, &hub);
        let config = machine
            .home
            .path()
            .join(".agentscm/hub-native/fixture-native");
        let journal = config.join(".github-update.json");
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
            .env("FAKE_FAILURE", "health-stop")
            .env("FAKE_PLIST", &plist)
            .env("FAKE_HEALTH_MARKER", &marker)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{wipe}: {output:?}");
        let said = out(&output);
        assert!(
            said.contains("configured native Hub did not become healthy"),
            "{said}"
        );
        assert!(said.contains("fixture recovery bootout failed"), "{said}");
        assert!(
            said.contains("launchctl still reports the native Hub loaded"),
            "{said}"
        );
        assert!(said.contains("Hub service state is unconfirmed"), "{said}");
        assert!(journal.exists(), "{wipe}: recovery evidence must remain");

        let launchctl_log = machine.home.path().join("launchctl.log");
        if !wipe {
            let manifest = machine.config().join("quickstart.json");
            let mut manifest_text = std::fs::read_to_string(&manifest).unwrap();
            let updated =
                manifest_text.find("\"updated_at\": ").unwrap() + "\"updated_at\": ".len();
            let comma = manifest_text[updated..].find(',').unwrap() + updated;
            manifest_text.replace_range(updated..comma, "0");
            std::fs::write(&manifest, manifest_text).unwrap();
            let receipt = config.join("retained.json");
            let watched = [&journal, &plist, &manifest, &receipt];
            let before: Vec<_> = watched
                .iter()
                .map(|path| std::fs::read(path).unwrap())
                .collect();
            let library = machine.home.path().join("Library");
            let real_library = machine.home.path().join("Library-real");
            std::fs::rename(&library, &real_library).unwrap();
            symlink(&real_library, &library).unwrap();
            std::fs::write(&launchctl_log, "").unwrap();

            let refused = machine.run(repo.path(), &["uninstall", "--yes", "--remove-hub-data"]);
            assert_eq!(refused.status.code(), Some(1), "{refused:?}");
            assert!(
                out(&refused).contains("Library directory is a symbolic link"),
                "{refused:?}"
            );
            for (path, expected) in watched.iter().zip(before) {
                assert_eq!(std::fs::read(path).unwrap(), expected, "{path:?}");
            }
            assert_eq!(
                std::fs::read_to_string(&launchctl_log).unwrap(),
                "",
                "the symlink guard must run before pending recovery calls launchctl"
            );
            std::fs::remove_file(&library).unwrap();
            std::fs::rename(&real_library, &library).unwrap();
            assert!(journal.exists());
        }
        std::fs::write(&launchctl_log, "").unwrap();
        let mut uninstall = vec!["uninstall", "--yes"];
        if wipe {
            uninstall.push("--remove-hub-data");
        }
        ok(&machine.run(repo.path(), &uninstall));
        let calls = std::fs::read_to_string(&launchctl_log).unwrap();
        assert!(!calls.contains("bootstrap "), "{wipe}: {calls}");
        assert!(!calls.contains("kickstart "), "{wipe}: {calls}");
        assert!(!plist.exists(), "{wipe}");
        assert!(!machine.config().join("quickstart.json").exists(), "{wipe}");
        assert_eq!(config.exists(), !wipe, "{wipe}");
        assert!(!journal.exists(), "{wipe}");
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_github_update_crash_recovery_chooses_only_a_valid_generation() {
    for phase in ["rollback", "commit", "drift"] {
        let (machine, hub) = (Machine::new(), Hub::start());
        let repo = machine.repo();
        let (plist, _) = native_fixture(&machine, &hub);
        let token = hub.token_file(machine.home.path());
        let config = machine
            .home
            .path()
            .join(".agentscm/hub-native/fixture-native");
        let receipt = config.join("retained.json");
        let manifest_path = machine.config().join("quickstart.json");
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
            "--no-agents",
            "--no-collisions",
        ];
        let connection = config.join("data/secrets/con_fixture.json");
        let connection_secret = config.join("data/secrets/con_fixture.client_secret");
        let connection_bytes = std::fs::read(&connection).unwrap();
        let connection_secret_bytes = std::fs::read(&connection_secret).unwrap();
        std::fs::remove_file(&connection).unwrap();
        std::fs::remove_file(&connection_secret).unwrap();
        ok(&machine
            .command(repo.path(), &args)
            .env("FAKE_PLIST", &plist)
            .output()
            .unwrap());
        std::fs::write(&connection, connection_bytes).unwrap();
        std::fs::write(&connection_secret, connection_secret_bytes).unwrap();
        let prior_manifest = json_file(&manifest_path);
        let prior_plist = std::fs::read_to_string(&plist).unwrap();
        let prior_receipt = std::fs::read_to_string(&receipt).unwrap();
        ok(&machine
            .command(repo.path(), &args)
            .env("FAKE_PLIST", &plist)
            .output()
            .unwrap());
        let next_manifest = json_file(&manifest_path);
        let next_plist = std::fs::read_to_string(&plist).unwrap();
        let next_receipt = std::fs::read_to_string(&receipt).unwrap();
        let pending = config.join(".github-update.json");
        std::fs::write(
            &pending,
            json!({
                "phase": if phase == "commit" { "commit" } else { "rollback" },
                "prior_record": prior_manifest["native_hub"],
                "next_record": next_manifest["native_hub"],
                "prior_plist": prior_plist.clone(),
                "prior_receipt": prior_receipt.clone(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&connection).unwrap();
        std::fs::remove_file(&connection_secret).unwrap();
        std::fs::write(machine.home.path().join("launchctl.log"), "").unwrap();

        match phase {
            "rollback" => {
                std::fs::write(&manifest_path, next_manifest.to_string()).unwrap();
                std::fs::write(&plist, &next_plist).unwrap();
                std::fs::write(&receipt, &prior_receipt).unwrap();
            }
            "commit" => {}
            "drift" => std::fs::write(&plist, "unrelated plist edit").unwrap(),
            _ => unreachable!(),
        }
        let before = [
            std::fs::read(&manifest_path).unwrap(),
            std::fs::read(&plist).unwrap(),
            std::fs::read(&receipt).unwrap(),
            std::fs::read(&pending).unwrap(),
        ];
        let output = machine
            .command(repo.path(), &args)
            .env("FAKE_PLIST", &plist)
            .output()
            .unwrap();
        if phase == "drift" {
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            assert!(out(&output).contains("no service or file was changed"));
            for (path, expected) in [&manifest_path, &plist, &receipt, &pending]
                .iter()
                .zip(before)
            {
                assert_eq!(std::fs::read(path).unwrap(), expected, "{path:?}");
            }
            let calls = std::fs::read_to_string(machine.home.path().join("launchctl.log")).unwrap();
            assert_eq!(calls.lines().count(), 1, "{calls}");
            assert!(calls.starts_with("print "), "{calls}");
            continue;
        }
        ok(&output);
        assert!(!pending.exists(), "{phase}");
        if phase == "commit" {
            let calls = std::fs::read_to_string(machine.home.path().join("launchctl.log")).unwrap();
            assert!(calls.starts_with("print "), "{calls}");
            assert!(!calls.contains("bootout "), "{calls}");
            assert!(!calls.contains("bootstrap "), "{calls}");
        }
        let expected = if phase == "commit" {
            &next_manifest["native_hub"]
        } else {
            &prior_manifest["native_hub"]
        };
        assert_eq!(
            &json_file(&manifest_path)["native_hub"],
            expected,
            "{phase}"
        );
        assert_eq!(
            std::fs::read_to_string(&plist).unwrap(),
            if phase == "commit" {
                next_plist.as_str()
            } else {
                prior_plist.as_str()
            },
            "{phase}"
        );
        let expected_receipt: serde_json::Value = serde_json::from_str(if phase == "commit" {
            &next_receipt
        } else {
            &prior_receipt
        })
        .unwrap();
        assert_eq!(json_file(&receipt), expected_receipt, "{phase}");
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_uninstall_journal_inspection_failure_changes_nothing() {
    let (machine, hub) = (Machine::new(), Hub::start());
    let repo = machine.repo();
    let (plist, _) = native_fixture(&machine, &hub);
    let config = machine
        .home
        .path()
        .join(".agentscm/hub-native/fixture-native");
    std::fs::remove_file(config.join("data/secrets/con_fixture.json")).unwrap();
    std::fs::remove_file(config.join("data/secrets/con_fixture.client_secret")).unwrap();
    let normalized = machine
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
        .env("FAKE_PLIST", &plist)
        .output()
        .unwrap();
    // --no-agents leaves the checkout unconnected on purpose, so this setup run succeeds.
    assert_eq!(normalized.status.code(), Some(0), "{normalized:?}");
    let journal = config.join(".github-update.json");
    std::fs::write(&journal, "synthetic private recovery evidence").unwrap();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
    let data = config.join("data/install.sqlite");
    std::fs::write(&data, "synthetic retained journal").unwrap();
    let manifest = machine.config().join("quickstart.json");
    let mut manifest_text = std::fs::read_to_string(&manifest).unwrap();
    let updated = manifest_text.find("\"updated_at\": ").unwrap() + "\"updated_at\": ".len();
    let comma = manifest_text[updated..].find(',').unwrap() + updated;
    manifest_text.replace_range(updated..comma, "0");
    std::fs::write(&manifest, manifest_text).unwrap();
    let paths = [&journal, &plist, &manifest, &data];
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let launchctl_log = machine.home.path().join("launchctl.log");
    std::fs::write(&launchctl_log, "").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = machine.run(repo.path(), &["uninstall", "--yes", "--remove-hub-data"]);
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        out(&output).contains("Could not inspect the native GitHub configuration recovery record"),
        "{output:?}"
    );
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), expected, "{path:?}");
    }
    assert_eq!(
        std::fs::read_to_string(launchctl_log).unwrap(),
        "",
        "uninstall must not call launchctl when journal inspection fails"
    );
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
fn a_first_native_install_that_fails_its_health_check_can_be_run_again() {
    let (machine, hub) = (Machine::new(), Hub::start());
    let repo = machine.repo();
    let (plist, _) = native_fixture(&machine, &hub);
    // Nothing installed yet: only the fake launchctl and the program remain from the fixture.
    let config = machine
        .home
        .path()
        .join(".agentscm/hub-native/fixture-native");
    std::fs::remove_dir_all(&config).unwrap();
    std::fs::remove_file(&plist).unwrap();
    std::fs::remove_file(machine.config().join("quickstart.json")).unwrap();
    // Nothing answers on this port, so the new Hub never becomes healthy.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let token = hub.token_file(machine.home.path());
    let run = || {
        machine
            .command(
                repo.path(),
                &[
                    "quickstart",
                    "--yes",
                    "--hub",
                    "local",
                    "--hub-runtime",
                    "native",
                    "--hub-project",
                    "fixture-native",
                    "--hub-port",
                    &port,
                    "--github-login",
                    "octo",
                    "--agent-token-file",
                    token.to_str().unwrap(),
                    "--no-agents",
                    "--no-collisions",
                ],
            )
            .env("FAKE_PLIST", &plist)
            .output()
            .unwrap()
    };
    for attempt in ["first", "rerun"] {
        let failed = run();
        assert_eq!(failed.status.code(), Some(1), "{attempt}: {failed:?}");
        // The rerun adopts the kept data directory instead of refusing it as unowned.
        assert!(
            out(&failed).contains("didn't become healthy"),
            "{attempt}: {failed:?}"
        );
        assert!(!plist.exists(), "{attempt}");
        // Kept with a receipt, as an uninstall that keeps data leaves it.
        assert!(config.join("retained.json").is_file(), "{attempt}");
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_resume_accepts_a_receipt_written_before_github_settings_were_kept_in_it() {
    let (machine, hub) = (Machine::new(), Hub::start());
    let repo = machine.repo();
    let (plist, _) = native_fixture(&machine, &hub);
    // Earlier releases kept GitHub settings in the record only, and wrote no `slack_files`.
    let manifest = machine.config().join("quickstart.json");
    let mut state = json_file(&manifest);
    let native = &mut state["native_hub"];
    let data = native["data_dir"].as_str().unwrap().to_owned();
    let config = PathBuf::from(native["config_dir"].as_str().unwrap());
    let environment = &mut native["environment"];
    environment["AGENTSCM_GITHUB_CLIENT_ID"] = json!("fixture-client");
    environment["AGENTSCM_GITHUB_CLIENT_SECRET_FILE"] =
        json!(format!("{data}/secrets/con_fixture.client_secret"));
    environment["AGENTSCM_ADMINS"] = json!("github:octo");
    std::fs::write(&manifest, state.to_string()).unwrap();
    let retained = config.join("retained.json");
    std::fs::write(
        &retained,
        json!({"project": "fixture-native",
            "environment": {"AGENTSCM_DATABASE_URL": format!("sqlite://{data}/hub.sqlite")}})
        .to_string(),
    )
    .unwrap();
    let token = hub.token_file(machine.home.path());
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
                "--agent-token-file",
                token.to_str().unwrap(),
                "--no-agents",
                "--no-collisions",
            ],
        )
        .env("FAKE_PLIST", &plist)
        .output()
        .unwrap();
    ok(&output);
    let receipt = json_file(&retained);
    assert_eq!(receipt["environment"]["AGENTSCM_ADMINS"], "github:octo");
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
    let said = ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    // The user-wide plugin is named as such before it is installed.
    assert!(
        said.contains("installed for your user account, so it is in all your projects"),
        "{said}"
    );
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
fn client_remove_honors_the_shared_lifecycle_lease() {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    let registered_root = PathBuf::from(
        machine.connections()[0]["root"]
            .as_str()
            .expect("saved connection root"),
    );
    let before = std::fs::read(machine.config().join("connections.json")).unwrap();
    let configured = repo.path().join(".mcp.json");
    assert!(configured.exists());

    let lease_path = machine.config().join("client-lifecycle.lock");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lease_path)
        .unwrap();
    // SAFETY: flock only reads the valid descriptor and has no pointer arguments.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);

    let blocked = machine.run(
        repo.path(),
        &[
            "client",
            "remove",
            "--root",
            registered_root.to_str().unwrap(),
        ],
    );
    assert!(!blocked.status.success(), "{}", out(&blocked));
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("client connections are busy"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert_eq!(
        std::fs::read(machine.config().join("connections.json")).unwrap(),
        before
    );
    assert!(configured.exists());
    assert!(token.exists());

    drop(lease);
    let removed = ok(&machine.run(
        repo.path(),
        &[
            "client",
            "remove",
            "--root",
            registered_root.to_str().unwrap(),
        ],
    ));
    assert!(
        removed.contains("Removed the client connection"),
        "{removed}"
    );
    assert!(machine.connections().is_empty());
    assert!(!configured.exists());
    assert!(token.exists());
}

#[test]
fn app_install_wait_releases_client_lifecycle_and_uninstall_fences_mutation() {
    for (command, independent_copy) in [("setup", false), ("connect", false), ("connect", true)] {
        app_install_wait_case(command, independent_copy);
    }
}

fn app_install_wait_case(client_command: &str, independent_copy: bool) {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let machine = Machine::transient().with_claude();
    let repo = machine.repo();
    let installed = install_dir_program(&machine);
    ok(&run_as(
        &machine,
        &installed,
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
    let invocation = if independent_copy {
        let path = machine.home.path().join("cache/agentscm");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(&installed, &path).unwrap();
        let installed_metadata = installed.metadata().unwrap();
        let copy_metadata = path.metadata().unwrap();
        assert_ne!(
            (installed_metadata.dev(), installed_metadata.ino()),
            (copy_metadata.dev(), copy_metadata.ino())
        );
        path
    } else {
        installed.clone()
    };
    #[cfg(target_os = "macos")]
    let tray_login = {
        let path = machine
            .home
            .path()
            .join("Library/LaunchAgents/com.counterbranch.agentscm.tray.plist");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stable = machine.bin_dir().join("agentscm");
        let config = machine.home.path().join(".config");
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>com.counterbranch.agentscm.tray</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--no-telemetry</string><string>tray</string><string>--foreground</string></array>\n<key>EnvironmentVariables</key><dict><key>XDG_CONFIG_HOME</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
            stable.display(),
            config.display()
        );
        std::fs::write(&path, body).unwrap();
        path
    };

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let (paused_tx, paused_rx) = mpsc::channel();
    let expires_at = unix_now() + 3600;
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut requests = Vec::new();
        let mut mints = 0;
        let mut deleted = false;
        while Instant::now() < deadline && !deleted {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("fake Hub accept failed: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            let first = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap()
                .to_owned();
            requests.push(first.clone());
            let (status, body) = match first.as_str() {
                "GET /v1/hub/session HTTP/1.1" => (
                    200,
                    json!({"user":{"provider":"github","host":"github.com"}}),
                ),
                "GET /v1/hub/connections HTTP/1.1" => (200, json!({"connections":[]})),
                "POST /v1/hub/tokens HTTP/1.1" => {
                    mints += 1;
                    if mints < 3 {
                        (403, json!({"error":"not_connected"}))
                    } else {
                        (201, json!({"id":"tok_wait","token":"minted-secret"}))
                    }
                }
                "GET /v1/hub/whoami HTTP/1.1" => (
                    200,
                    json!({
                        "token":{"repository":REPO,"read_only":false,"expires_at":expires_at},
                        "owner":{"provider":"github","host":"github.com","login":"octo"}
                    }),
                ),
                "DELETE /v1/hub/tokens/tok_wait HTTP/1.1" => {
                    deleted = true;
                    (200, json!({}))
                }
                other => panic!("unexpected fake Hub request: {other}"),
            };
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            if mints == 2 {
                paused_tx.send(()).unwrap();
            }
        }
        assert!(deleted, "setup did not revoke the unused minted token");
        requests
    });

    std::fs::create_dir_all(machine.config()).unwrap();
    std::fs::write(
        machine.config().join("hub-session.json"),
        json!({"hub":hub,"session":"saved-session","expires_at":expires_at}).to_string(),
    )
    .unwrap();

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initializes both descriptors; null termios and winsize request defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned two new owned descriptors above.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    // SAFETY: openpty returned two new owned descriptors above.
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let args = match client_command {
        "setup" => vec!["client", "setup", "--agent", "claude", "--hub", &hub],
        "connect" => vec!["client", "connect", "--hub", &hub],
        _ => unreachable!(),
    };
    let mut command = machine.command_as(&invocation, repo.path(), &args);
    command
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut setup = command.spawn().unwrap();
    paused_rx.recv_timeout(Duration::from_secs(10)).unwrap();

    // The setup process is now waiting for Enter. Uninstall must be able to take the lifecycle
    // lease, remove its owned program, and finish rather than racing with a stale setup writer.
    let uninstalled = machine.run(repo.path(), &["uninstall", "--yes"]);
    assert!(uninstalled.status.success(), "{uninstalled:?}");
    assert!(!machine.bin_dir().exists());
    #[cfg(target_os = "macos")]
    assert!(!tray_login.exists());
    master.write_all(b"\n").unwrap();
    assert!(
        !setup.wait().unwrap().success(),
        "{client_command} independent_copy={independent_copy} resumed successfully after uninstall"
    );
    let requests = server.join().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request == "DELETE /v1/hub/tokens/tok_wait HTTP/1.1"),
        "{client_command} independent_copy={independent_copy}: {requests:?}"
    );
    assert!(machine.connections().is_empty());
    assert!(!machine.config().join("tokens").exists());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(
        !machine.claude_log().contains("install"),
        "{client_command} independent_copy={independent_copy}"
    );
}

#[test]
fn client_connect_supports_explicit_config_without_home() {
    let hub = Hub::start();
    for empty_home in [false, true] {
        let machine = Machine::new();
        let repo = machine.repo();
        let token = hub.token_file(machine.home.path());
        let config = machine.home.path().join("explicit-config");
        let mut command = machine.command(
            repo.path(),
            &[
                "client",
                "connect",
                "--hub",
                &hub.url,
                "--token-file",
                token.to_str().unwrap(),
            ],
        );
        command
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env("XDG_CONFIG_HOME", &config);
        if empty_home {
            command.env("HOME", "");
        }
        ok(&command.output().unwrap());
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(config.join("agentscm/connections.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["connections"][0]["repo"], REPO);
    }
}

#[test]
fn client_connect_rejects_local_state_changed_during_hub_wait() {
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    for changed in ["binding", "origin", "token"] {
        let machine = Machine::new();
        let repo = machine.repo();
        let root = repo.path().canonicalize().unwrap();
        let token = machine.home.path().join("saved.token");
        std::fs::write(&token, "saved-secret\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hub = format!("http://{}", listener.local_addr().unwrap());
        let expires_at = unix_now() + 3600;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            assert_eq!(
                String::from_utf8_lossy(&request).lines().next(),
                Some("GET /v1/hub/whoami HTTP/1.1")
            );
            entered_tx.send(()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let body = json!({
                "token":{"repository":REPO,"read_only":false,"expires_at":expires_at},
                "owner":{"provider":"github","host":"github.com","login":"octo"}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        std::fs::create_dir_all(machine.config()).unwrap();
        let mut connection = json!({
            "root":root,
            "origin_host":"github.com",
            "hub":hub,
            "repo":REPO,
            "token_file":token
        });
        let connections = machine.config().join("connections.json");
        std::fs::write(
            &connections,
            json!({"version":1,"connections":[connection.clone()]}).to_string(),
        )
        .unwrap();
        let mut command = machine.command(repo.path(), &["client", "connect", "--hub", &hub]);
        let child = command.spawn().unwrap();
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        match changed {
            "binding" => {
                connection["report_completion"] = json!(true);
                std::fs::write(
                    &connections,
                    json!({"version":1,"connections":[connection]}).to_string(),
                )
                .unwrap();
            }
            "origin" => machine.git(
                repo.path(),
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "git@github.com:acme/other.git",
                ],
            ),
            "token" => std::fs::write(&token, "changed-secret\n").unwrap(),
            _ => unreachable!(),
        }
        resume_tx.send(()).unwrap();
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        assert!(!output.status.success(), "{changed}: {output:?}");
        assert!(
            !out(&output).contains("Connection is already valid"),
            "{changed}: {output:?}"
        );
    }
}

#[test]
fn final_uninstall_cleanup_excludes_setup_and_retains_program_when_busy() {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let installed = install_dir_program(&machine);
    ok(&run_as(
        &machine,
        &installed,
        repo.path(),
        &quick(&hub.url, &token, &["--no-collisions"]),
    ));
    let manifest = machine.config().join("quickstart.json");
    let bin = machine.home.path().join(".agentscm/bin");
    assert!(manifest.exists() && bin.join("agentscm").exists());
    let registered_root = machine.connections()[0]["root"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&machine.run(
        repo.path(),
        &["client", "remove", "--root", &registered_root],
    ));
    let mut record = json_file(&manifest);
    record["clients"] = serde_json::json!([]);
    record["session"] = serde_json::json!({"hub": hub.url.clone()});
    record["local_hub"] = serde_json::json!({
        "dir": machine.home.path().join(".agentscm/synthetic-hub"),
        "project": "synthetic-managed-hub",
        "port": 7332
    });
    std::fs::write(&manifest, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let lease_path = machine.config().join("client-lifecycle.lock");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lease_path)
        .unwrap();
    // SAFETY: flock only reads the valid descriptor and has no pointer arguments.
    assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);

    let setup = machine.run(
        repo.path(),
        &[
            "client",
            "setup",
            "--agent",
            "claude",
            "--hub",
            &hub.url,
            "--root",
            repo.path().to_str().unwrap(),
        ],
    );
    assert!(!setup.status.success(), "{}", out(&setup));
    assert!(
        String::from_utf8_lossy(&setup.stderr).contains("client connections are busy"),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let uninstall = machine.run(repo.path(), &["uninstall", "--yes"]);
    assert!(!uninstall.status.success(), "{}", out(&uninstall));
    assert!(bin.join("agentscm").exists());
    assert!(manifest.exists());
    let retained = json_file(&manifest);
    assert_eq!(retained["session"]["hub"], hub.url);
    assert_eq!(retained["local_hub"]["project"], "synthetic-managed-hub");
    assert!(!machine.home.path().join("docker.log").exists());

    let mut retry = retained;
    retry["session"] = serde_json::Value::Null;
    retry["local_hub"] = serde_json::Value::Null;
    std::fs::write(&manifest, serde_json::to_vec_pretty(&retry).unwrap()).unwrap();

    drop(lease);
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(!bin.join("agentscm").exists());
    assert!(!manifest.exists());
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
    // The hub is set up; with no agent found there is nothing to connect yet.
    let said = ok(&output);
    assert!(
        said.contains("Claude Code wasn't found, so I'm not setting up a coding agent"),
        "{said}"
    );
    for agent in ["claude", "codex", "cursor", "copilot", "gemini"] {
        let line = format!("client setup --agent {agent} --hub {}", hub.url);
        assert!(said.contains(&line), "{line}: {said}");
    }
    assert!(machine.connections().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(
        last_line(&output).contains("no coding agent was set up")
            && last_line(&output).contains("quickstart --agent claude|codex"),
        "{}",
        last_line(&output)
    );
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
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(machine.connections().is_empty() && machine.claude_log().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
    assert!(last_line(&output).contains("no coding agent was set up"));
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
fn yes_with_an_expired_sign_in_refuses_at_once_instead_of_signing_in_again() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    std::fs::create_dir_all(machine.config()).unwrap();
    let session = machine.config().join("hub-session.json");
    // The hub does not know this session, as after it expired or the hub restarted.
    std::fs::write(
        &session,
        json!({"hub": hub.url, "session": "expired", "expires_at": unix_now() + 3600}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).unwrap();
    let started = std::time::Instant::now();
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
    // A browser or device sign-in would wait for minutes.
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains("can't sign in again (no terminal, or --yes)")
            && said.contains("pass --agent-token-file"),
        "{said}"
    );
    assert!(
        !said.contains("device") && !said.contains("browser"),
        "{said}"
    );
    assert!(machine.connections().is_empty());
    assert!(!repo.path().join(".mcp.json").exists());
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
            && said.contains("that the plugin would duplicate"),
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
fn a_failed_agent_step_ends_non_zero_and_the_last_line_says_so() {
    let (hub, machine) = (Hub::start(), Machine::transient().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    // Fails when it cannot be asked about installed plugins, as a half-working install does.
    let shim = machine.bin.path().join("claude");
    let body = std::fs::read_to_string(&shim).unwrap();
    std::fs::write(
        &shim,
        body.replacen("\n", "\ncase \"$*\" in *install*) exit 1;; esac\n", 1),
    )
    .unwrap();
    let output = machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--no-collisions", "--init"]),
    );
    let line = last_line(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        line.starts_with("Live warnings: off — coding agent setup failed"),
        "{line}"
    );
    // Nothing committable is written for a connection that failed.
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("didn't add the agent instructions"), "{said}");
    assert!(!repo.path().join("AGENTS.md").exists());
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

const FAKE_CODEX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "__STATE__/codex-log"
[ "$*" = "plugin list --json" ] && printf '{"installed":[]}\n'
exit 0
"#;

impl Machine {
    /// Puts a fake `codex` next to the fake `claude`.
    fn with_codex(self) -> Self {
        let script = self.bin.path().join("codex");
        std::fs::write(
            &script,
            FAKE_CODEX.replace("__STATE__", self.home.path().to_str().unwrap()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    /// Turns the checkout's connection into one `client setup` minted itself after a sign-in:
    /// a new token in its own managed file. Returns the token's secret.
    fn own_the_connection(&self, hub: &Hub) -> String {
        use sha2::{Digest, Sha256};
        let path = self.config().join("connections.json");
        let mut all = json_file(&path);
        let connection = &mut all["connections"][0];
        let owner = hub
            .store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                "octo",
            )
            .unwrap();
        let (id, secret) = mint_agent_token(&*hub.store, &owner.id, "owned", REPO, 3600).unwrap();
        let mut digest = Sha256::new();
        for field in ["hub", "root", "origin_host", "repo"] {
            digest.update(connection[field].as_str().unwrap().as_bytes());
            digest.update([0]);
        }
        digest.update(id.as_bytes());
        let suffix: String = digest.finalize()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let tokens = self.config().join("tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::set_permissions(&tokens, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = tokens.join(format!("client-{suffix}.token"));
        std::fs::write(&file, format!("{}\n", secret.expose())).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        connection["token_file"] = json!(file);
        connection["owned_token_id"] = json!(id);
        connection["owned_token_expires_at"] = json!(unix_now() + 3600);
        std::fs::write(&path, all.to_string()).unwrap();
        secret.expose().to_owned()
    }
}

fn whoami_status(hub: &Hub, secret: &str) -> u16 {
    hub._runtime.block_on(async {
        reqwest::Client::new()
            .get(format!("{}/v1/hub/whoami", hub.url))
            .bearer_auth(secret)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

#[test]
fn uninstall_leaves_a_connection_quickstart_found_rather_than_made() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude().with_codex());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let token_text = std::fs::read(&token).unwrap();
    ok(&machine.run(
        repo.path(),
        &[
            "client",
            "setup",
            "--agent",
            "codex",
            "--hub",
            &hub.url,
            "--token-file",
            token.to_str().unwrap(),
        ],
    ));
    let codex_config = machine.home.path().join(".codex/config.toml");
    let codex_before = std::fs::read(&codex_config).unwrap();
    let said = ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    assert!(said.contains("existed before quickstart"), "{said}");
    assert!(
        json_file(&machine.config().join("quickstart.json"))["clients"]
            .as_array()
            .is_none_or(|clients| clients.is_empty())
    );
    let _ = machine.run(repo.path(), &["uninstall", "--yes"]);
    let connections = machine.connections();
    assert_eq!(connections.len(), 1, "{connections:?}");
    assert!(
        connections[0]["agents"]
            .as_array()
            .unwrap()
            .contains(&json!("codex"))
    );
    assert_eq!(std::fs::read(&token).unwrap(), token_text);
    assert_eq!(std::fs::read(&codex_config).unwrap(), codex_before);
    let secret = String::from_utf8(token_text).unwrap();
    assert_eq!(whoami_status(&hub, secret.trim()), 200);
}

#[test]
fn uninstall_revokes_a_minted_token_with_the_token_itself() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    let secret = machine.own_the_connection(&hub);
    assert_eq!(whoami_status(&hub, &secret), 200);
    // No sign-in is saved: the token revokes itself.
    assert!(!machine.config().join("hub-session.json").exists());
    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("revoked token tok_"), "{said}");
    assert_eq!(whoami_status(&hub, &secret), 401);
    assert!(machine.connections().is_empty());
    assert!(
        !machine
            .config()
            .join("tokens")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );
}

#[test]
fn uninstall_that_cannot_revoke_fails_and_keeps_what_a_retry_needs() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    machine.own_the_connection(&hub);
    drop(hub);
    let output = machine.run(repo.path(), &["uninstall", "--yes"]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let said = out(&output);
    assert!(said.contains("could not be revoked"), "{said}");
    let connections = machine.connections();
    assert_eq!(connections.len(), 1, "{connections:?}");
    assert!(
        connections[0]["agents"]
            .as_array()
            .is_none_or(|agents| agents.is_empty())
    );
    let file = PathBuf::from(connections[0]["token_file"].as_str().unwrap());
    assert!(file.exists());
    assert_eq!(
        json_file(&machine.config().join("quickstart.json"))["clients"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn failed_install_after_replacement_still_retires_the_old_managed_token() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let initial = hub.token_file(machine.home.path());
    ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &initial, &["--no-collisions"]),
    ));
    let old_secret = machine.own_the_connection(&hub);
    let old_file = PathBuf::from(machine.connections()[0]["token_file"].as_str().unwrap());
    assert_eq!(whoami_status(&hub, &old_secret), 200);

    let replacement = hub.token_file_for(machine.home.path(), REPO, "replacement.token");
    let shim = machine.bin.path().join("claude");
    let original = std::fs::read_to_string(&shim).unwrap();
    std::fs::write(
        &shim,
        original.replacen(
            '\n',
            "\ncase \"$2\" in install|update|enable) exit 1;; esac\n",
            1,
        ),
    )
    .unwrap();
    let output = machine.run(
        repo.path(),
        &[
            "client",
            "setup",
            "--agent",
            "claude",
            "--hub",
            &hub.url,
            "--token-file",
            replacement.to_str().unwrap(),
            "--replace",
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(whoami_status(&hub, &old_secret), 401);
    assert!(!old_file.exists());
    let saved = machine.connections();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(
        saved[0]["token_file"],
        replacement.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(saved[0]["owned_token_id"].is_null());
    assert_eq!(saved[0]["agents"], json!(["claude"]));

    std::fs::write(&shim, original).unwrap();
    ok(&machine.run(repo.path(), &["client", "remove"]));
    assert!(machine.connections().is_empty());
    assert!(replacement.exists());
}

#[test]
fn a_rerun_with_agents_changes_nothing() {
    let (hub, machine) = (Hub::start(), Machine::new().with_claude());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let args = quick(&hub.url, &token, &["--no-collisions"]);
    ok(&machine.run(repo.path(), &args));
    let files = [
        machine.config().join("quickstart.json"),
        machine.config().join("connections.json"),
        repo.path().join(".mcp.json"),
        token.clone(),
    ];
    let before: Vec<_> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    let said = ok(&machine.run(repo.path(), &args));
    assert!(said.contains("No changes made."), "{said}");
    let after: Vec<_> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
    assert_eq!(after, before);
}

/// Each agent is set up in its user configuration, next to what was already there, and
/// uninstall leaves that configuration as it was.
#[test]
fn agents_are_set_up_for_all_projects_and_uninstall_restores_user_configuration() {
    for (agent, file, original) in [
        (
            "cursor",
            ".cursor/mcp.json",
            r#"{"mcpServers":{"other":{"command":"other"}}}"#,
        ),
        ("gemini", ".gemini/settings.json", r#"{"theme":"dark"}"#),
        (
            "copilot",
            ".copilot/mcp-config.json",
            r#"{"mcpServers":{"other":{"type":"local","command":"other"}}}"#,
        ),
        (
            "codex",
            ".codex/config.toml",
            "model = \"o3\"\n\n[projects.\"/work/agentscm\"]\ntrust_level = \"trusted\"\n",
        ),
    ] {
        let (hub, machine) = (Hub::start(), Machine::new().with_codex());
        let repo = machine.repo();
        let token = hub.token_file(machine.home.path());
        let home = machine.home.path();
        let path = home.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();
        let said = ok(&machine.run(
            repo.path(),
            &quick(&hub.url, &token, &["--no-collisions", "--agent", agent]),
        ));
        assert!(
            said.contains("so it is in all your projects"),
            "{agent}: {said}"
        );
        // The diff of the shared file is shown before it is written.
        assert!(said.contains(&format!("+++ {}", path.display())), "{said}");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("agentscm"), "{agent}: {written}");
        // The entry finds the checkout where it starts, so it names none.
        let checkout = repo.path().canonicalize().unwrap();
        assert!(
            !written.contains(checkout.to_str().unwrap()),
            "{agent}: {written}"
        );
        for kept in ["other", "dark", "o3", "/work/agentscm"] {
            assert_eq!(
                written.contains(kept),
                original.contains(kept),
                "{agent} {kept}: {written}"
            );
        }
        let hooks = match agent {
            "cursor" => Some(home.join(".cursor/hooks.json")),
            "gemini" => Some(path.clone()),
            "copilot" => Some(home.join(".copilot/hooks/agentscm-client.json")),
            _ => None,
        };
        if let Some(hooks) = &hooks {
            let text = std::fs::read_to_string(hooks).unwrap();
            assert!(
                text.contains(&format!("hook {agent} --client-connection")),
                "{text}"
            );
        }
        let skill = home.join(format!(".{agent}/skills/coordinate-edits/SKILL.md"));
        assert_eq!(skill.exists(), agent != "codex", "{agent}");
        // Nothing for this agent is written into the checkout.
        for local in [".cursor", ".gemini", ".github", ".vscode", ".codex"] {
            assert!(!repo.path().join(local).exists(), "{agent} {local}");
        }
        assert_eq!(machine.connections()[0]["agents"], json!([agent]));

        let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
        assert!(said.contains("Took AgentSCM out of"), "{agent}: {said}");
        let left = std::fs::read_to_string(&path).unwrap();
        if agent == "codex" {
            assert_eq!(left, original);
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&left).unwrap(),
                serde_json::from_str::<Value>(original).unwrap(),
                "{agent}"
            );
        }
        if let Some(hooks) = hooks.filter(|hooks| *hooks != path) {
            assert!(!hooks.exists(), "{agent}");
        }
        assert!(
            !skill.exists() && !skill.parent().unwrap().exists(),
            "{agent}"
        );
    }
}

/// A checkout set up for one checkout only moves to the user configuration, and a checkout-only
/// setup is refused while the agent is set up for all projects, so no hook runs twice.
#[test]
fn checkout_and_user_scope_never_both_hold_an_agents_hooks() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let setup = |extra: &[&str]| {
        let mut args = vec!["client", "setup", "--agent", "gemini", "--hub", &hub.url];
        args.extend(["--token-file", token.to_str().unwrap()]);
        args.extend(extra);
        machine.run(repo.path(), &args)
    };
    ok(&setup(&["--checkout-only"]));
    let local = repo.path().join(".gemini/settings.json");
    assert!(std::fs::read_to_string(&local).unwrap().contains("--root"));
    assert!(!machine.home.path().join(".gemini").exists());

    let said = ok(&setup(&[]));
    assert!(said.contains("for all your projects"), "{said}");
    assert!(!local.exists() && !repo.path().join(".gemini").exists());
    let user = json_file(&machine.home.path().join(".gemini/settings.json"));
    assert_eq!(
        user["mcpServers"]["agentscm"]["args"],
        json!(["--no-telemetry", "client", "mcp"])
    );
    assert_eq!(user["hooks"]["SessionStart"].as_array().unwrap().len(), 1);

    let refused = setup(&["--checkout-only"]);
    assert!(!refused.status.success());
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("without --checkout-only"), "{said}");
    assert!(!local.exists());
}
