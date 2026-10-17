//! `agentscm demo`: three simulated agents, a throwaway in-process hub and the hub's real hook
//! endpoint, using file names from the Git repository it runs in. The repository is only read,
//! with `git rev-parse`, `git remote get-url`, `git log` and `git ls-files`; everything the hub
//! stores lives in a private temporary directory that is removed after the hub has stopped, on a
//! normal exit, Ctrl-C, SIGTERM or SIGHUP (not on SIGKILL).
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscm::backend::LocalBackend;
use agentscm::hub::session::never_hold;
use agentscm::hub::store::{
    self, ForgeIdentity, Store, mint_bound_agent_token, validate_repository,
};
use agentscm::hub::{HubState, router};
use agentscm::team::auth::generate_secret;
use agentscm::{Error, Result};
use clap::Args;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::hub_cli::{clean, map_hub_error, open_url};

const NOT_A_REPOSITORY: &str =
    "run agentscm demo inside a Git repository with at least two tracked files";
const NOT_ENOUGH_FILES: &str = "run agentscm demo inside a Git repository with at least two tracked files the hub can hold \
     (not lock files, generated files, vendored directories or submodules)";
/// The longest path the hub accepts as a file key.
const MAX_PATH_BYTES: usize = 1024;
/// How long the hub gets to stop before its task is aborted.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// The forge repository id every demo token is bound to; the status page scopes by it.
const REPOSITORY_ID: i64 = 1;
const TOKEN_TTL_SECONDS: i64 = 7 * 24 * 3600;
const HEARTBEAT: Duration = Duration::from_secs(60);
/// Heartbeats between session renewals: 25 minutes, inside the hub's 30-minute hold cap.
const RENEW_EVERY: u32 = 25;
const HISTORY_COMMITS: &str = "400";
/// Printed last: `npx` leaves no `agentscm` command installed, so the step names `npx agentscm`.
const NEXT_STEP: &str = "To set it up for real, run npx agentscm: guided setup that tells you what it will change before it does.";

#[derive(Debug, Args)]
pub struct DemoArgs {
    /// Do not open a browser; the status page link is printed either way.
    #[arg(long)]
    no_open: bool,
    /// Run the scripted steps, print, and exit.
    #[arg(long, hide = true)]
    once: bool,
}

/// Removes the temporary directory when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self> {
        let suffix = generate_secret().map_err(|_| Error::Invalid("random source".into()))?;
        let path = std::env::temp_dir().join(format!("agentscm-demo-{}", &suffix.expose()[..12]));
        #[cfg_attr(not(unix), allow(unused_mut))] // only unix sets a mode
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Agent {
    user: &'static str,
    tool: &'static str,
    /// The hook endpoint: `claude` or `codex`.
    hook: &'static str,
    /// The session id prefix; a rotation appends a counter.
    session_prefix: &'static str,
    session: String,
    secret: String,
    task: String,
    /// The files it edits; replayed when its session is renewed.
    files: Vec<String>,
}

enum Event<'a> {
    Start,
    Edit(&'a str),
    End,
}

impl Agent {
    fn rotate(&mut self, round: u32) {
        self.session = format!("{}-{round}", self.session_prefix);
    }
}

/// The throwaway hub: serves on a loopback port until `stop`.
struct Server {
    base: String,
    store: Arc<dyn Store>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(dir: &Path) -> Result<Self> {
        let store = store::open(&format!("sqlite://{}", dir.join("hub.sqlite").display()))
            .map_err(map_hub_error)?;
        let objects = LocalBackend::open(dir.join("objects"))?;
        let app = router(HubState::new(store.clone(), Arc::new(objects)).with_demo());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (shutdown, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Ok(Self {
            base,
            store,
            shutdown,
            task,
        })
    }

    /// A token for a synthetic user; every one is bound to the same repository id.
    fn mint(
        &self,
        id: i64,
        login: &str,
        name: &str,
        repository: &str,
        read_only: bool,
    ) -> Result<String> {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: id,
        };
        let user = self
            .store
            .upsert_user(&identity, login)
            .map_err(map_hub_error)?;
        let token = mint_bound_agent_token(
            &*self.store,
            &user.id,
            name,
            repository,
            REPOSITORY_ID,
            TOKEN_TTL_SECONDS,
            read_only,
        );
        Ok(token.map_err(map_hub_error)?.1.expose().to_owned())
    }

    /// Stops serving and waits (briefly) for the hub to let go of its database file.
    async fn stop(mut self) {
        let _ = self.shutdown.send(());
        if tokio::time::timeout(STOP_GRACE, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

struct Driver {
    client: Client,
    base: String,
    root: String,
    /// Round trips of the scripted steps; `None` once they are done.
    latencies: Option<Vec<Duration>>,
}

impl Driver {
    /// A session start, then its edits.
    async fn begin(&mut self, agent: &Agent) -> Result<()> {
        self.hook(agent, Event::Start).await?;
        for file in &agent.files {
            self.hook(agent, Event::Edit(file)).await?;
        }
        Ok(())
    }

    /// One real hook call; returns the hub's JSON answer.
    async fn hook(&mut self, agent: &Agent, event: Event<'_>) -> Result<Value> {
        let mut body = json!({"session_id": agent.session, "cwd": self.root});
        match event {
            Event::Start => {
                body["hook_event_name"] = json!("SessionStart");
                body["initial_prompt"] = json!(agent.task);
            }
            Event::End => body["hook_event_name"] = json!("SessionEnd"),
            Event::Edit(file) => {
                body["hook_event_name"] = json!("PreToolUse");
                if agent.hook == "codex" {
                    body["tool_name"] = json!("apply_patch");
                    body["tool_input"] = json!({"command": format!(
                        "*** Begin Patch\n*** Update File: {file}\n@@\n+demo\n*** End Patch\n")});
                } else {
                    body["tool_name"] = json!("Edit");
                    body["tool_input"] = json!({"file_path": format!("{}/{file}", self.root)});
                }
            }
        }
        let started = Instant::now();
        let reply = self
            .client
            .post(format!("{}/v1/hub/hook/{}", self.base, agent.hook))
            .bearer_auth(&agent.secret)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| Error::Invalid(format!("demo hub call failed: {error}")))?;
        let status = reply.status();
        let text = reply.text().await.unwrap_or_default();
        if let Some(latencies) = &mut self.latencies {
            latencies.push(started.elapsed());
        }
        if !status.is_success() {
            return Err(Error::Invalid(format!("demo hub answered {status}")));
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// One real MCP tool call; returns the result text, and fails when the tool reports an error.
    async fn tool(&self, agent: &Agent, name: &str, arguments: Value) -> Result<String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let reply = self
            .client
            .post(format!("{}/mcp", self.base))
            .bearer_auth(&agent.secret)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| Error::Invalid(format!("demo hub call failed: {error}")))?;
        let status = reply.status();
        let reply: Value = serde_json::from_str(&reply.text().await.unwrap_or_default())?;
        let result = &reply["result"];
        match result.pointer("/content/0/text").and_then(Value::as_str) {
            Some(text) if status.is_success() && result["isError"] != json!(true) => {
                Ok(text.to_owned())
            }
            _ => Err(Error::Invalid(format!(
                "the demo hub's {name} failed ({status}): {reply}"
            ))),
        }
    }

    /// Experimental handoff: `asker` is refused `file` (the refusal asks `holder` for it), `holder`
    /// hands it over, and the asker's retry passes.
    async fn handoff(&mut self, holder: &Agent, asker: &Agent, file: &str) -> Result<()> {
        let refusal = self.hook(asker, Event::Edit(file)).await?;
        if !is_denied(&refusal) {
            return Err(Error::Invalid(format!(
                "the demo hub did not refuse the edit: {refusal}"
            )));
        }
        let handed = json!({"path": file, "accept": true, "session": holder.session});
        let text = self.tool(holder, "agentscm_handoff", handed).await?;
        if !text.starts_with("AgentSCM: handed ") {
            return Err(Error::Invalid(format!(
                "unexpected agentscm_handoff reply: {text}"
            )));
        }
        let retry = self.hook(asker, Event::Edit(file)).await?;
        if is_denied(&retry) {
            return Err(Error::Invalid(format!(
                "the demo hub refused the retry after a handoff: {retry}"
            )));
        }
        Ok(())
    }
}

pub async fn run(args: DemoArgs) -> Result<()> {
    // Registered first, so a signal during setup is held until the cleanup below.
    let stop = stop_signal()?;
    let root = git(Path::new("."), &["rev-parse", "--show-toplevel"])
        .map(|out| out.trim().to_owned())
        .ok()
        .filter(|root| !root.is_empty())
        .ok_or_else(|| Error::Invalid(NOT_A_REPOSITORY.into()))?;
    let files = pick_files(&root)?;
    let repository = repository_name(&root);

    let dir = TempDir::create()?;
    let server = Server::start(&dir.0).await?;
    let result = tokio::select! {
        biased;
        () = stop => Ok(()),
        result = scenario(&args, &server, &root, &repository, &files) => result,
    };
    // The hub must release its database before the directory goes.
    server.stop().await;
    drop(dir);
    result
}

/// Resolves on Ctrl-C, and on unix on SIGTERM or SIGHUP. The handlers exist when this returns.
fn stop_signal() -> Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
                _ = hangup.recv() => {}
            }
        })
    }
    #[cfg(not(unix))]
    {
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        Ok(async move {
            interrupt.recv().await;
        })
    }
}

async fn scenario(
    args: &DemoArgs,
    server: &Server,
    root: &str,
    repository: &str,
    files: &[String],
) -> Result<()> {
    let base = server.base.clone();
    let mint = |id, login, name, read_only| server.mint(id, login, name, repository, read_only);
    let name = |file: &str| file.rsplit('/').next().unwrap_or(file).to_owned();
    // With only two files the third agent takes the last one and the first agent just one.
    let last = files.len().min(3) - 1;
    let (alex_files, riley_file) = (&files[..last], &files[last]);
    let agent = |id, user, tool, hook, prefix, task: String, files: &[String]| -> Result<Agent> {
        let secret = mint(id, user, tool, false)?;
        Ok(Agent {
            user,
            tool,
            hook,
            session_prefix: prefix,
            session: prefix.to_owned(),
            secret,
            task,
            files: files.to_vec(),
        })
    };
    let mut alex = agent(
        1,
        "demo-alex",
        "claude-code",
        "claude",
        "demo-a",
        format!("Refactor error handling in {}", name(&files[0])),
        alex_files,
    )?;
    // The refused agent has no files to replay: its one edit is the contested one.
    let mut sam = agent(
        2,
        "demo-sam",
        "claude-code",
        "claude",
        "demo-b",
        format!("Update docs for {}", name(&files[0])),
        &[],
    )?;
    let mut riley = agent(
        3,
        "demo-riley",
        "codex",
        "codex",
        "demo-c",
        format!("Add tests for {}", name(riley_file)),
        std::slice::from_ref(riley_file),
    )?;
    let reader = mint(4, "demo-viewer", "status-page", true)?;

    let mut driver = Driver {
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| Error::Invalid(error.to_string()))?,
        base: base.clone(),
        root: root.to_owned(),
        latencies: Some(Vec::new()),
    };
    driver.begin(&alex).await?;
    driver.begin(&riley).await?;
    driver.hook(&sam, Event::Start).await?;
    let refusal = driver.hook(&sam, Event::Edit(&files[0])).await?;
    let reason = refusal
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(Value::as_str)
        .filter(|_| is_denied(&refusal))
        .ok_or_else(|| {
            Error::Invalid(format!("the demo hub did not refuse the edit: {refusal}"))
        })?;
    let repeat = driver.hook(&sam, Event::Edit(&files[0])).await?;
    if is_denied(&repeat) {
        return Err(Error::Invalid(format!(
            "the demo hub refused a repeat: {repeat}"
        )));
    }
    // Needs a file alex holds that riley does not: with two files, files[1] is riley's own.
    let handoff = files.len() > 2;
    if handoff {
        driver.handoff(&alex, &riley, &files[1]).await?;
        // Renewal replays the new holds: riley holds files[1] now, alex no longer.
        alex.files.retain(|file| *file != files[1]);
        riley.files.push(files[1].clone());
    }
    let health = match driver
        .client
        .get(format!("{base}/v1/hub/health"))
        .send()
        .await
    {
        Ok(reply) => reply.text().await.unwrap_or_default(),
        Err(error) => return Err(Error::Invalid(format!("demo hub health failed: {error}"))),
    };
    let health: Value = serde_json::from_str(&health).unwrap_or(Value::Null);
    let mut latencies = driver.latencies.take().unwrap_or_default();

    let open = !args.no_open && !args.once;
    let page = format!("{base}/ui/");
    println!("Simulated agents, real hub, your file names.");
    println!("Repository: {}", clean(repository));
    println!("Hub: {base} (throwaway, this process only)");
    println!(
        "1. {} ({}) starts \"{}\" and edits {}.",
        alex.user,
        alex.tool,
        alex.task,
        alex_files.join(" and ")
    );
    println!(
        "2. {} ({}) starts \"{}\" and edits {riley_file}; no conflict.",
        riley.user, riley.tool, riley.task
    );
    println!(
        "3. {} ({}) starts \"{}\" and tries {}. The hub refuses:",
        sam.user, sam.tool, sam.task, files[0]
    );
    println!("     \"{}\"", clean(reason));
    println!(
        "4. {} repeats the same edit. Repeated: let through.",
        sam.user
    );
    if handoff {
        println!(
            "5. {} ({}) is refused on {}, asks {}'s agent to hand it over (experimental), and {} hands it over; {}'s retry passes.",
            riley.user, riley.tool, files[1], alex.user, alex.user, riley.user
        );
    }
    println!("{}", latency_line(&mut latencies, &health));
    // A throwaway read-only token on loopback: printed so the page can be opened by hand.
    println!("Status page: {page}#token={reader}");
    if !cfg!(feature = "ui") {
        println!(
            "This build has no status page (the page shows a notice); npx agentscm and the container image include it."
        );
    }
    if args.once {
        println!("Done; nothing was written to your repository.");
        println!("{NEXT_STEP}");
        return Ok(());
    }
    if open {
        open_url(&format!("{page}#token={reader}"));
        println!("Opened in your browser; if it did not open, use the link above.");
    }
    println!("Press Ctrl-C to stop; nothing was written to your repository.");
    println!("{NEXT_STEP}");

    // The hub caps a hold at 30 minutes, so every 25 minutes each agent ends its session and
    // starts a fresh one that replays its edits; the refused agent has none to replay.
    let mut tick = tokio::time::interval(HEARTBEAT);
    // After a sleep, carry on from now instead of firing every missed beat at once.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let mut agents = [&mut alex, &mut riley, &mut sam];
    // Runs until `run` drops this future on a stop signal.
    for beat in 1_u32.. {
        tick.tick().await;
        if server.task.is_finished() {
            return Err(Error::Invalid("the demo hub stopped".into()));
        }
        if beat % RENEW_EVERY == 0 {
            // Two passes: no new hold can be refused because an old one is still live.
            for agent in agents.iter() {
                let _ = driver.hook(agent, Event::End).await;
            }
            for agent in agents.iter_mut() {
                agent.rotate(beat);
                let _ = driver.begin(agent).await;
            }
            continue;
        }
        for agent in agents.iter() {
            let _ = match agent.files.first() {
                Some(file) => driver.hook(agent, Event::Edit(file)).await,
                None => driver.hook(agent, Event::Start).await,
            };
        }
    }
    Ok(())
}

/// Whether the hub refused the edit, in Claude Code's hook output format.
fn is_denied(reply: &Value) -> bool {
    reply.pointer("/hookSpecificOutput/permissionDecision") == Some(&json!("deny"))
}

fn latency_line(latencies: &mut [Duration], health: &Value) -> String {
    latencies.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let counter = |name: &str| health["coordination"][name].as_u64().unwrap_or(0);
    format!(
        "Hook latency: {:.1} ms median ({:.1}-{:.1} ms over {} calls); hub counted {} refused, {} repeated, p95 {} ms",
        ms(latencies[latencies.len() / 2]),
        ms(latencies[0]),
        ms(latencies[latencies.len() - 1]),
        latencies.len(),
        counter("denied"),
        counter("repeated"),
        counter("hook_p95_ms"),
    )
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off", "-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(Error::Invalid(NOT_A_REPOSITORY.into()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Tracked files the hub would hold, most often changed in recent history first, then the rest
/// in `git ls-files` order. At least two.
fn pick_files(root: &str) -> Result<Vec<String>> {
    let usable = |path: &str| {
        !never_hold(path)
            && path.len() <= MAX_PATH_BYTES
            && !path
                .chars()
                .any(|c| c.is_control() || matches!(c, '"' | '\\' | '*' | '\u{FFFD}'))
    };
    // `<mode> <object> <stage>\t<path>`; mode 160000 is a submodule, not a file.
    let staged = git(Path::new(root), &["ls-files", "--stage"])?;
    let mut tracked: Vec<&str> = staged
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(info, path)| !info.starts_with("160000 ") && usable(path))
        .map(|(_, path)| path)
        .collect();
    // A conflicted file is listed once per merge stage; the list is sorted by path.
    tracked.dedup();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let history = git(
        Path::new(root),
        &["log", "--name-only", "--format=", "-n", HISTORY_COMMITS],
    )
    .unwrap_or_default();
    for path in history.lines() {
        *counts.entry(path).or_default() += 1;
    }
    // Stable sort: ties and unchanged files keep ls-files order.
    let mut files = tracked;
    files.sort_by_key(|path| std::cmp::Reverse(counts.get(path).copied().unwrap_or(0)));
    if files.len() < 2 {
        return Err(Error::Invalid(NOT_ENOUGH_FILES.into()));
    }
    Ok(files.into_iter().take(3).map(str::to_owned).collect())
}

/// `owner/name` from the origin remote when it validates, else `local/<directory>`.
fn repository_name(root: &str) -> String {
    let remote = git(Path::new(root), &["remote", "get-url", "origin"]).unwrap_or_default();
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let mut parts = remote.rsplit(['/', ':']);
    if let (Some(name), Some(owner)) = (parts.next(), parts.next())
        && validate_repository(&format!("{owner}/{name}")).is_ok()
    {
        return format!("{owner}/{name}");
    }
    let directory: String = Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let directory = directory.trim_matches('.');
    format!(
        "local/{}",
        if directory.is_empty() {
            "repository"
        } else {
            directory
        }
    )
}
