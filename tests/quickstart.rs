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
        }
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
            .env("PATH", self.bin.path())
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

fn backups(settings: &Path) -> Vec<PathBuf> {
    let prefix = format!(
        "{}.agentscm-backup-",
        settings.file_name().unwrap().to_string_lossy()
    );
    let mut found: Vec<_> = std::fs::read_dir(settings.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .collect();
    found.sort();
    found
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

const SETTINGS: &str = ".claude/settings.local.json";

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

/// An Edit hook call from `cwd`, through the hub.json quickstart wrote.
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
fn end_to_end_with_a_given_token_then_the_hook_works() {
    let (hub, machine) = (Hub::start(), Machine::transient());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    // User scope keeps the command form: one env var cannot serve several repositories.
    let args = quick(&hub.url, &token, &["--hook", "user", "--no-collisions"]);
    let output = machine.run(repo.path(), &args);
    let said = ok(&output);
    assert!(
        said.lines()
            .any(|l| l == "The agent token is valid for acme/widgets."),
        "{said}"
    );
    assert!(said.contains(" uninstall`"), "{said}");
    assert_eq!(
        last_line(&output),
        format!("Live warnings: on (hub {}, repo acme/widgets)", hub.url)
    );

    let hub_json = machine.config().join("hub.json");
    assert_eq!(mode(&hub_json), 0o600);
    let config = json_file(&hub_json);
    let token_path = token.canonicalize().unwrap();
    assert_eq!(config["url"], hub.url.as_str());
    assert_eq!(
        Path::new(config["token_file"].as_str().unwrap()),
        token_path
    );
    assert_eq!(
        Path::new(config["tokens"][REPO].as_str().unwrap()),
        token_path
    );
    assert_eq!(config.as_object().unwrap().len(), 3);

    // This test program is not installed anywhere stable, so it is copied in the
    // installer's layout and the hooks call the symlink.
    let (bin, version) = (machine.bin_dir(), env!("CARGO_PKG_VERSION"));
    let link = bin.join("agentscm");
    let versioned = bin.join(format!("agentscm-{version}"));
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new(&format!("agentscm-{version}"))
    );
    assert_eq!(
        versioned.metadata().unwrap().len(),
        Path::new(env!("CARGO_BIN_EXE_agentscm"))
            .metadata()
            .unwrap()
            .len()
    );
    assert_eq!(mode(&versioned), 0o755);
    assert_eq!(mode(&bin), 0o700);
    assert_eq!(names_in(&bin).len(), 2);
    assert!(
        said.contains(&format!("Run AgentSCM commands with: {}", link.display())),
        "{said}"
    );
    assert!(
        said.contains(&format!("export PATH={}:\"$PATH\"", bin.display())),
        "{said}"
    );
    let command_help = Command::new(&link).arg("--help").output().unwrap();
    assert!(command_help.status.success());

    let settings = machine.home.path().join(".claude/settings.json");
    assert_eq!(mode(&settings), 0o600);
    assert!(json_file(&settings).get("env").is_none());
    let exe = link.display().to_string();
    let hooks = &json_file(&settings)["hooks"];
    assert_eq!(
        hooks["SessionStart"][0]["hooks"][0]["command"],
        format!("{exe} intent start --hook claude")
    );
    assert_eq!(hooks["PreToolUse"][0]["matcher"], "Edit|Write|MultiEdit");
    assert_eq!(
        hooks["PreToolUse"][0]["hooks"][0]["command"],
        format!("{exe} intent check --hook claude")
    );
    assert_eq!(
        hooks["SessionEnd"][0]["hooks"][0]["command"],
        format!("{exe} intent done --hook claude")
    );

    let record = machine.config().join("quickstart.json");
    assert_eq!(mode(&record), 0o600);
    let record = json_file(&record);
    assert_eq!(record["hooks"][0]["added"].as_array().unwrap().len(), 3);
    assert_eq!(record["hooks"][0]["created"], true);
    assert_eq!(record["hub_json"]["path"], hub_json.to_str().unwrap());
    assert_eq!(record["hub_json"]["hash"].as_str().unwrap().len(), 64);
    assert_eq!(record["tokens"][0]["repo"], REPO);
    assert_eq!(record["tokens"][0]["owned"], false);
    assert_eq!(record["bin_dir"], bin.to_str().unwrap());

    // The configuration it wrote is enough for the hook: no AGENTSCM_* variables are set.
    let reply = ok(&edit(&machine, repo.path(), "s1", "src/main.rs"));
    assert!(!reply.contains("unknown"), "{reply}");
    assert!(live_paths(&hub, REPO).contains(&vec!["src/main.rs".to_owned()]));

    // Uninstall removes the copy and the symlink it made.
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(!bin.exists());
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
        "--hook",
        "project",
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
    let hooks = json_file(&repo.path().join(SETTINGS));
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        format!("{} intent start --hook claude", link.display())
    );
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
        &["quickstart", "--yes", "--hub", "skip", "--hook", "none"],
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

const OTHER_HOOKS: &str = r#"{"model":"x","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"other"}]}]}}"#;

fn project_args() -> [&'static str; 7] {
    [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--hook",
        "project",
        "--no-collisions",
    ]
}

#[test]
fn hook_merge_keeps_everything_else_and_is_idempotent() {
    let machine = Machine::new();
    let repo = machine.repo();
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, OTHER_HOOKS).unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o640)).unwrap();
    ok(&machine.run(repo.path(), &project_args()));

    let merged = json_file(&settings);
    assert_eq!(merged["model"], "x");
    let pre = merged["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0]["hooks"][0]["command"], "other");
    assert!(merged["hooks"]["SessionStart"].is_array() && merged["hooks"]["SessionEnd"].is_array());
    assert_eq!(mode(&settings), 0o640);
    let saved = backups(&settings);
    assert_eq!(saved.len(), 1);
    assert_eq!(std::fs::read(&saved[0]).unwrap(), OTHER_HOOKS.as_bytes());

    let before = std::fs::read(&settings).unwrap();
    let output = machine.run(repo.path(), &project_args());
    let said = ok(&output);
    assert!(
        said.contains("already has the AgentSCM hooks; nothing to change."),
        "{said}"
    );
    assert!(last_line(&output).starts_with("Live warnings: off — no hub chosen"));
    assert_eq!(std::fs::read(&settings).unwrap(), before);
    assert_eq!(backups(&settings).len(), 1);
}

#[test]
fn an_unparsable_or_unreadable_settings_file_is_left_alone() {
    let machine = Machine::new();
    let repo = machine.repo();
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{ not json").unwrap();
    let said = ok(&machine.run(repo.path(), &project_args()));
    assert!(
        said.contains("isn't valid JSON, so I left it alone"),
        "{said}"
    );
    assert_eq!(std::fs::read(&settings).unwrap(), b"{ not json");
    assert!(backups(&settings).is_empty());
    assert!(!machine.config().join("quickstart.json").exists());

    // Not UTF-8, or not readable: that is an error, not an absent file.
    std::fs::write(&settings, [0xff, 0xfe, 0x00]).unwrap();
    let said = ok(&machine.run(repo.path(), &project_args()));
    assert!(said.contains("I couldn't read"), "{said}");
    assert_eq!(std::fs::read(&settings).unwrap(), [0xff, 0xfe, 0x00]);
    std::fs::write(&settings, "{}").unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o000)).unwrap();
    let said = ok(&machine.run(repo.path(), &project_args()));
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(said.contains("I couldn't read"), "{said}");
    assert_eq!(std::fs::read(&settings).unwrap(), b"{}");
    assert!(backups(&settings).is_empty());
    assert!(!machine.config().join("quickstart.json").exists());
}

#[test]
fn a_symlinked_settings_file_stays_a_symlink() {
    let machine = Machine::new();
    let repo = machine.repo();
    let real = machine.home.path().join("dotfiles/settings.local.json");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, OTHER_HOOKS).unwrap();
    let link = repo.path().join(SETTINGS);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&real, &link).unwrap();
    ok(&machine.run(repo.path(), &project_args()));
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        json_file(&real)["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(backups(&real).len(), 1);
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&real).unwrap(), OTHER_HOOKS.as_bytes());
}

#[test]
fn uninstall_restores_the_original_files() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = "{\n  \"model\": \"x\"\n}\n";
    std::fs::write(&settings, original).unwrap();
    // A second file quickstart creates, in the user-level settings.
    let user = machine.home.path().join(".claude/settings.json");

    ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--hook", "project", "--no-collisions"]),
    ));
    ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--hook", "user", "--no-collisions"]),
    ));
    assert!(user.exists() && machine.config().join("hub.json").exists());

    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("Removed quickstart's record"), "{said}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    assert!(!user.exists());
    assert!(!machine.config().join("hub.json").exists());
    assert!(!machine.config().join("quickstart.json").exists());
    assert!(!machine.bin_dir().exists());
    // The token file was the user's own, so it stays.
    assert!(token.exists());
    // Nothing left to undo.
    assert!(out(&machine.run(repo.path(), &["uninstall", "--yes"])).contains("Nothing to undo"));
}

#[test]
fn uninstall_after_the_user_edited_the_file_removes_only_our_hooks() {
    let machine = Machine::new();
    let repo = machine.repo();
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, OTHER_HOOKS).unwrap();
    ok(&machine.run(repo.path(), &project_args()));
    let mut edited = json_file(&settings);
    edited["extra"] = json!(1);
    std::fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();

    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("removed 3 hook entries"), "{said}");
    let after = json_file(&settings);
    assert_eq!(after["extra"], 1);
    assert_eq!(after["model"], "x");
    assert_eq!(after["hooks"].as_object().unwrap().len(), 1);
    assert_eq!(
        after["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "other"
    );
    assert!(!machine.config().join("quickstart.json").exists());
}

#[test]
fn uninstall_keeps_what_it_could_not_undo() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--hook", "none", "--no-collisions"]),
    ));
    let record_path = machine.config().join("quickstart.json");
    let mut record = json_file(&record_path);
    record["tokens"][0]["owned"] = json!(true);
    std::fs::write(&record_path, record.to_string()).unwrap();
    let hub_json = machine.config().join("hub.json");
    let written = std::fs::read_to_string(&hub_json).unwrap();
    let changed = format!("{written}\n\n");
    std::fs::write(&hub_json, &changed).unwrap();

    let output = machine.run(repo.path(), &["uninstall", "--yes"]);
    assert!(!output.status.success());
    let said = out(&output);
    assert!(
        said.contains("you changed it after quickstart wrote it"),
        "{said}"
    );
    assert!(said.contains("To finish by hand"), "{said}");
    assert!(
        said.contains(&format!("review {}", hub_json.display())),
        "{said}"
    );
    assert_eq!(std::fs::read_to_string(&hub_json).unwrap(), changed);
    assert!(!token.exists());
    // Only what is left stays in the record.
    let left = json_file(&record_path);
    assert!(left["hub_json"].is_object());
    assert!(left["tokens"].as_array().unwrap().is_empty());
    assert!(left["hooks"].as_array().unwrap().is_empty());

    // Once it is as quickstart wrote it again, uninstall finishes.
    std::fs::write(&hub_json, &written).unwrap();
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(!hub_json.exists() && !record_path.exists());
}

#[test]
fn uninstall_ends_the_sign_in_quickstart_created() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--hook", "none", "--no-collisions"]),
    ));
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
}

#[test]
fn a_hub_json_for_another_hub_is_replaced_with_a_backup_and_restored_by_uninstall() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let hub_json = machine.config().join("hub.json");
    std::fs::create_dir_all(hub_json.parent().unwrap()).unwrap();
    let original = r#"{"url":"http://127.0.0.1:9","token_file":"/old/token","tokens":{"acme/other":"/old/other"}}"#;
    std::fs::write(&hub_json, original).unwrap();
    let said = ok(&machine.run(
        repo.path(),
        &quick(&hub.url, &token, &["--hook", "none", "--no-collisions"]),
    ));
    assert!(
        said.contains("switching moves your other repositories"),
        "{said}"
    );
    let now = json_file(&hub_json);
    assert_eq!(now["url"], hub.url.as_str());
    assert_eq!(now["tokens"].as_object().unwrap().len(), 1);
    let saved = backups(&hub_json);
    assert_eq!(saved.len(), 1);
    assert_eq!(std::fs::read(&saved[0]).unwrap(), original.as_bytes());

    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert_eq!(std::fs::read(&hub_json).unwrap(), original.as_bytes());
}

#[test]
fn several_repositories_share_one_hub_with_their_own_tokens() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let (one, two) = (
        machine.repo(),
        machine.repo_at("https://github.com/acme/gadgets.git"),
    );
    let token_one = hub.token_file_for(machine.home.path(), "acme/widgets", "one.token");
    let token_two = hub.token_file_for(machine.home.path(), "acme/gadgets", "two.token");
    let go = |dir: &Path, token: &Path| {
        let output = machine.run(
            dir,
            &quick(&hub.url, token, &["--hook", "none", "--no-collisions"]),
        );
        ok(&output);
    };
    go(one.path(), &token_one);
    go(two.path(), &token_two);

    let config = json_file(&machine.config().join("hub.json"));
    let tokens = config["tokens"].as_object().unwrap();
    assert_eq!(tokens.len(), 2);
    assert_eq!(
        Path::new(tokens["acme/widgets"].as_str().unwrap()),
        token_one.canonicalize().unwrap()
    );
    assert_eq!(
        Path::new(tokens["acme/gadgets"].as_str().unwrap()),
        token_two.canonicalize().unwrap()
    );
    assert_eq!(
        json_file(&machine.config().join("quickstart.json"))["tokens"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Each checkout announces with its own token.
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
    assert!(!machine.config().join("hub.json").exists());
    assert!(token_one.exists() && token_two.exists());
}

#[test]
fn a_requested_hub_that_is_down_fails_but_keeps_the_planned_hook() {
    let machine = Machine::new();
    let repo = machine.repo();
    let url = closed_port_url();
    let output = machine.run(
        repo.path(),
        &[
            "quickstart",
            "--yes",
            "--hub",
            "existing",
            "--hub-url",
            &url,
            "--hook",
            "project",
            "--no-collisions",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(
        said.contains(&format!(
            "The hub at {url} isn't answering; live warnings will start working once it is."
        )),
        "{said}"
    );
    assert!(repo.path().join(SETTINGS).exists());
    assert!(!machine.config().join("hub.json").exists());
    assert_eq!(
        last_line(&output),
        format!(
            "Live warnings: off — the hub at {url} isn't answering (start it, then run `agentscm quickstart` again)"
        )
    );

    // With a token the user supplies, the settings are written even though it cannot be checked.
    let token = machine.home.path().join("t.token");
    std::fs::write(&token, "tok\n").unwrap();
    let output = machine.run(
        repo.path(),
        &quick(&url, &token, &["--hook", "none", "--no-collisions"]),
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let said = out(&output);
    assert!(said.contains("hub isn't answering"), "{said}");
    assert_eq!(
        json_file(&machine.config().join("hub.json"))["url"],
        url.as_str()
    );
}

#[test]
fn with_yes_the_hooks_wait_for_a_hub() {
    let machine = Machine::new();
    let repo = machine.repo();
    std::fs::create_dir_all(machine.home.path().join(".claude")).unwrap();
    let output = machine.run(
        repo.path(),
        &["quickstart", "--yes", "--hub", "skip", "--no-collisions"],
    );
    let said = ok(&output);
    assert!(said.contains("Not adding the Claude Code hooks"), "{said}");
    assert!(!repo.path().join(".claude").exists());
    assert!(!machine.config().join("quickstart.json").exists());
}

#[test]
fn without_a_terminal_it_refuses_and_writes_nothing() {
    let machine = Machine::new();
    let repo = machine.repo();
    let output = machine.run(repo.path(), &["quickstart", "--hook", "project"]);
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    for flag in [
        "--yes",
        "--hub local|existing|skip",
        "--hub-url",
        "--agent-token-file",
        "--hook user|project|none",
        "--no-collisions",
    ] {
        assert!(said.contains(flag), "{flag}: {said}");
    }
    assert!(!repo.path().join(".claude").exists());
    assert!(!machine.config().exists());
    assert!(!machine.home.path().join(".agentscm").exists());
    let output = machine.run(repo.path(), &["uninstall"]);
    assert!(!output.status.success());
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
        "--hook",
        "none",
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
fn uninstall_after_two_writes_keeps_edits_made_in_between() {
    let machine = Machine::new();
    let repo = machine.repo();
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, OTHER_HOOKS).unwrap();
    ok(&machine.run(repo.path(), &project_args()));
    // The user adds a setting and drops one of our hooks; quickstart puts it back.
    let mut edited = json_file(&settings);
    edited["extra"] = json!(1);
    edited["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("SessionEnd");
    std::fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    ok(&machine.run(repo.path(), &project_args()));
    assert_eq!(backups(&settings).len(), 2);
    assert!(json_file(&settings)["hooks"]["SessionEnd"].is_array());

    let said = ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert!(said.contains("hook entries"), "{said}");
    let after = json_file(&settings);
    assert_eq!(after["extra"], 1);
    assert_eq!(after["model"], "x");
    assert_eq!(after["hooks"].as_object().unwrap().len(), 1);
    assert_eq!(
        after["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "other"
    );
}

fn bytes_of(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[test]
fn project_hooks_with_a_hub_are_http_and_keep_the_token_in_env() {
    let (hub, machine) = (Hub::start(), Machine::transient());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let secret = std::fs::read_to_string(&token).unwrap().trim().to_owned();
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, OTHER_HOOKS).unwrap();
    // An older run left the command form (and a copied program) behind.
    ok(&machine.run(repo.path(), &project_args()));
    assert!(machine.bin_dir().exists());
    assert!(json_file(&settings)["hooks"]["SessionStart"].is_array());

    let args = quick(&hub.url, &token, &["--hook", "project", "--no-collisions"]);
    let output = machine.run(repo.path(), &args);
    let said = ok(&output);
    assert!(!said.contains(&secret), "the token was printed");
    assert!(said.contains("<your agent token>"), "{said}");
    assert!(said.contains("older command-form"), "{said}");
    assert!(
        said.contains("holds your agent token and isn't ignored by Git"),
        "{said}"
    );
    let after = json_file(&settings);
    assert_eq!(after["model"], "x");
    assert_eq!(after["env"]["AGENTSCM_HUB_TOKEN"], secret.as_str());
    let hooks = after["hooks"].as_object().unwrap();
    assert_eq!(hooks.len(), 2, "{hooks:?}");
    // The repository comes from the checkout's origin remote.
    let url = format!("{}/v1/hub/hook/claude?repo=acme/widgets", hub.url);
    let pre = hooks["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0]["hooks"][0]["command"], "other");
    assert_eq!(pre[1]["hooks"][0]["type"], "http");
    assert_eq!(pre[1]["hooks"][0]["url"], url.as_str());
    assert_eq!(
        pre[1]["hooks"][0]["headers"]["Authorization"],
        "Bearer $AGENTSCM_HUB_TOKEN"
    );
    assert_eq!(hooks["SessionEnd"][0]["hooks"][0]["url"], url.as_str());
    assert!(
        !std::fs::read_to_string(machine.config().join("quickstart.json"))
            .unwrap()
            .contains(&secret)
    );

    // A second run changes nothing.
    let before = bytes_of(&settings);
    let said = ok(&machine.run(repo.path(), &args));
    assert!(said.contains("nothing to change"), "{said}");
    assert_eq!(bytes_of(&settings), before);

    // Uninstall gives back the file as it was before quickstart first touched it.
    ok(&machine.run(repo.path(), &["uninstall", "--yes"]));
    assert_eq!(
        json_file(&settings),
        serde_json::from_str::<Value>(OTHER_HOOKS).unwrap()
    );
    assert!(!machine.bin_dir().exists());
}

#[test]
fn a_new_project_settings_file_with_a_token_is_private_and_a_foreign_token_is_kept() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let (fresh, other) = (machine.repo(), machine.repo());
    let token = hub.token_file(machine.home.path());
    let args = quick(&hub.url, &token, &["--hook", "project", "--no-collisions"]);
    ok(&machine.run(fresh.path(), &args));
    let settings = fresh.path().join(SETTINGS);
    assert_eq!(mode(&settings), 0o600);
    ok(&machine.run(fresh.path(), &["uninstall", "--yes"]));
    assert!(!settings.exists());

    let settings = other.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, r#"{"env":{"AGENTSCM_HUB_TOKEN":"mine"}}"#).unwrap();
    let said = ok(&machine.run(other.path(), &args));
    assert!(said.contains("a value I didn't write"), "{said}");
    assert!(!said.contains("<your agent token>"), "{said}");
    let kept = json_file(&settings);
    assert_eq!(kept["env"]["AGENTSCM_HUB_TOKEN"], "mine");
    assert_eq!(kept["hooks"]["SessionEnd"][0]["hooks"][0]["type"], "http");
    ok(&machine.run(other.path(), &["uninstall", "--yes"]));
    assert_eq!(
        json_file(&settings),
        json!({"env": {"AGENTSCM_HUB_TOKEN": "mine"}})
    );
}

#[test]
fn an_existing_world_readable_settings_file_becomes_private_with_its_backup() {
    use std::os::unix::fs::PermissionsExt;
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let token = hub.token_file(machine.home.path());
    let settings = repo.path().join(SETTINGS);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, r#"{"model":"x"}"#).unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
    let args = quick(&hub.url, &token, &["--hook", "project", "--no-collisions"]);
    ok(&machine.run(repo.path(), &args));
    assert_eq!(mode(&settings), 0o600);
    for backup in backups(&settings) {
        assert_eq!(mode(&backup), 0o600);
    }
}

#[test]
fn local_hub_never_generates_team_repository_config_even_with_init() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    let instructions = "@AGENTS.md\nKeep the project's instructions.\n";
    std::fs::write(repo.path().join("CLAUDE.md"), instructions).unwrap();
    let token = hub.token_file(machine.home.path());
    let said = ok(&machine.run(
        repo.path(),
        &quick(
            &hub.url,
            &token,
            &["--hook", "none", "--no-collisions", "--init"],
        ),
    ));
    for file in [".mcp.json", ".claude/settings.json", "AGENTS.md"] {
        assert!(!repo.path().join(file).exists(), "{file}");
    }
    assert_eq!(
        std::fs::read_to_string(repo.path().join("CLAUDE.md")).unwrap(),
        instructions
    );
    assert!(said.contains("Skipping repository agent config"), "{said}");
    assert!(
        !said.contains("Repository files to review and commit"),
        "{said}"
    );
}

#[test]
fn worktree_sessions_default_to_user_hooks_and_resolve_repository_token() {
    let (hub, machine) = (Hub::start(), Machine::new());
    let repo = machine.repo();
    machine.git(repo.path(), &["commit", "--allow-empty", "-qm", "fixture"]);
    let tree = repo.path().join(".claude/worktrees/agent");
    machine.git(
        repo.path(),
        &["worktree", "add", "-qb", "agent", tree.to_str().unwrap()],
    );
    std::fs::create_dir_all(machine.home.path().join(".claude")).unwrap();
    let token = hub.token_file(machine.home.path());
    let said = ok(&machine.run(repo.path(), &quick(&hub.url, &token, &["--no-collisions"])));
    let settings = machine.home.path().join(".claude/settings.json");
    assert!(settings.exists(), "{said}");
    assert!(!repo.path().join(SETTINGS).exists());
    assert!(!tree.join(SETTINGS).exists());
    assert!(json_file(&settings)["hooks"]["PreToolUse"].is_array());
    ok(&edit(&machine, &tree, "worktree-session", "src/lib.rs"));
    assert_eq!(live_paths(&hub, REPO), vec![vec!["src/lib.rs".to_owned()]]);
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
            "--hook",
            "none",
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
                        "--hook",
                        "none",
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
                "--hook",
                "none",
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
    assert!(!machine.config().join("hub.json").exists());
    assert!(
        json_file(&machine.config().join("quickstart.json"))["tokens"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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
            "--hook",
            "none",
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
                    "--hook",
                    "none",
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

#[test]
fn no_op_rerun_keeps_manifest_bytes_and_reports_no_changes() {
    let machine = Machine::new();
    let repo = machine.repo();
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--hook",
        "user",
        "--no-collisions",
    ];
    ok(&machine.run(repo.path(), &args));
    let record = machine.config().join("quickstart.json");
    let before = std::fs::read(&record).unwrap();
    let said = ok(&machine.run(repo.path(), &args));
    assert!(said.contains("No changes made."), "{said}");
    assert!(said.contains("Quickstart's recorded setup:"), "{said}");
    assert_eq!(std::fs::read(record).unwrap(), before);
}

#[test]
fn no_op_rerun_repairs_a_world_readable_manifest_mode() {
    let machine = Machine::new();
    let repo = machine.repo();
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--hook",
        "user",
        "--no-collisions",
    ];
    ok(&machine.run(repo.path(), &args));
    let record = machine.config().join("quickstart.json");
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = ok(&machine.run(repo.path(), &args));
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
                        "--hook",
                        "none",
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
                    "--hook",
                    "none",
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
                    "--hook",
                    "none",
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
                        "--hook",
                        "none",
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

    let machine = Machine::new();
    let first_repo = machine.repo();
    let second_repo = machine.repo_at("git@github.com:acme/other.git");
    let args = [
        "quickstart",
        "--yes",
        "--hub",
        "skip",
        "--hook",
        "project",
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
        let child = machine
            .command(first_repo.path(), &args)
            .env("AGENTSCM_HUB_URL", url)
            .env(
                "AGENTSCM_HUB_TOKEN_FILE",
                machine.home.path().join("absent.token"),
            )
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
    let hooks_while_paused = second_repo.path().join(SETTINGS).exists();
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
    assert!(!hooks_while_paused);
    ok(&first_output);
    assert_eq!(json_file(&record)["hooks"].as_array().unwrap().len(), 1);
    ok(&machine.run(second_repo.path(), &args));
    assert_eq!(json_file(&record)["hooks"].as_array().unwrap().len(), 2);

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
    ok(&machine.run(first_repo.path(), &["uninstall", "--yes"]));
    assert!(!record.exists());
    assert!(!first_repo.path().join(SETTINGS).exists());
    assert!(!second_repo.path().join(SETTINGS).exists());
    use std::os::unix::fs::MetadataExt;
    assert_eq!(lock_path.metadata().unwrap().ino(), lock_inode);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn native_program_repairs_are_reported_but_true_noop_preserves_record() {
    for missing in ["link", "versioned"] {
        let (machine, hub) = (Machine::new(), Hub::start());
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
            "--hook",
            "none",
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
