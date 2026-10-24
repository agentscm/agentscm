//! `agentscm skill`: browse and adopt the embedded supported skills, report adoption to the Hub,
//! read a branch's run, record self-reported evidence and enforce the Stop gate.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Duration;

use agentscm::forge::encode_path_segment;
use agentscm::hub::forge_http;
use agentscm::skill_workflow::supported::{
    self, AdoptionStatus, Assurance, CatalogEntry, Detail, Enforce, Evidence, EvidenceKind,
    GateState, LOCK_PATH, Params, RunView, SkillLock, SkillPin,
};
use agentscm::{Error, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::agent_stop::{self, Prepared, StopAgent, StopInput};
use crate::client_config;

const REPLY_BYTES: usize = 1024 * 1024;
const HUB_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_SKILL: &str = "pr-gate";
const MAX_REASON: usize = 2000;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AgentArg {
    Claude,
    Codex,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List the supported skills in this binary and which ones this repository adopted.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one supported skill in full, including its gates.
    Show {
        name: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Adopt a skill (or change its version or parameters) in the repository lock file.
    Add {
        name: String,
        #[arg(long)]
        version: Option<u32>,
        /// `key=value`; `key=null` resets the parameter to its default. Repeatable.
        #[arg(long = "param")]
        params: Vec<String>,
    },
    /// Remove a skill from the repository lock file.
    Remove { name: String },
    /// Send this checkout's lock file to the Hub as the repository's adoption.
    ReportAdoption,
    /// Show where a branch stands in its skill run, as the Hub sees it.
    Status {
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record self-reported evidence for a gate at the current HEAD.
    Evidence {
        #[arg(long)]
        gate: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "pr")]
        pull_request: Option<u64>,
        #[arg(long, default_value = DEFAULT_SKILL)]
        skill: String,
        /// JSON file with the evidence detail (command, exit_code, findings, resolved, ...).
        #[arg(long)]
        detail_json: Option<PathBuf>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Stop-hook mode: read the hook JSON on standard input and block while a gate needs the agent.
    Stop {
        #[arg(long, value_enum)]
        agent: AgentArg,
    },
}

pub async fn run(data_dir: &Path, command: SkillCommand) -> Result<()> {
    match command {
        SkillCommand::List { json } => list(json),
        SkillCommand::Show {
            name,
            version,
            json,
        } => show(&name, version, json),
        SkillCommand::Add {
            name,
            version,
            params,
        } => add(&name, version, &params),
        SkillCommand::Remove { name } => remove(&name),
        SkillCommand::ReportAdoption => report_adoption().await,
        SkillCommand::Status { branch, json } => status(branch, json).await,
        SkillCommand::Evidence {
            gate,
            kind,
            summary,
            url,
            pull_request,
            skill,
            detail_json,
            branch,
        } => {
            let args = EvidenceArgs {
                gate,
                kind,
                summary,
                url,
                pull_request,
                skill,
                detail_json,
                branch,
            };
            record(args).await
        }
        SkillCommand::Stop { agent } => {
            let agent = match agent {
                AgentArg::Claude => StopAgent::Claude,
                AgentArg::Codex => StopAgent::Codex,
            };
            println!("{}", stop(data_dir, agent, std::io::stdin()).await);
            Ok(())
        }
    }
}

// ---- git and repository ----------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn repo_root(dir: &Path) -> Result<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .ok_or_else(|| invalid("not inside a Git repository"))
}

fn cwd() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]).filter(|b| !b.is_empty())
}

fn default_branch(dir: &Path) -> String {
    git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .and_then(|r| r.strip_prefix("origin/").map(str::to_owned))
    .unwrap_or_else(|| "main".into())
}

/// Commits on HEAD that the default branch lacks; `None` when neither ref resolves.
fn commits_ahead(dir: &Path, default: &str) -> Option<u64> {
    [
        format!("origin/{default}..HEAD"),
        format!("{default}..HEAD"),
    ]
    .iter()
    .find_map(|range| git(dir, &["rev-list", "--count", range])?.parse().ok())
}

fn read_lock(root: &Path) -> Result<Option<SkillLock>> {
    match std::fs::read(root.join(LOCK_PATH)) {
        Ok(bytes) => SkillLock::parse(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ---- list, show ------------------------------------------------------------------------

/// "adopted", another status, "pinned at vN" or "not adopted" for one catalog entry.
fn adoption_label(lock: Option<&SkillLock>, entry: &CatalogEntry) -> String {
    let Some(lock) = lock else {
        return "not adopted".into();
    };
    let Some(status) = supported::resolve(lock)
        .into_iter()
        .find(|p| p.pin.name == entry.name)
    else {
        return "not adopted".into();
    };
    if status.pin.workflow_version != entry.workflow_version {
        return format!("pinned at v{}", status.pin.workflow_version);
    }
    match status.status {
        AdoptionStatus::Adopted => "adopted",
        AdoptionStatus::UnknownVersion => "unknown_version",
        AdoptionStatus::UnsupportedSource => "unsupported_source",
        AdoptionStatus::InvalidParams => "invalid_params",
    }
    .into()
}

fn lock_for_cwd() -> Result<Option<SkillLock>> {
    match repo_root(&cwd()?) {
        Ok(root) => read_lock(&root),
        Err(_) => Ok(None),
    }
}

fn describe(
    entry: &CatalogEntry,
    label: Option<&str>,
    full: bool,
    walkthrough: &[String],
) -> String {
    let a = &entry.applicability;
    let mut out = format!("{} v{}", entry.name, entry.workflow_version);
    if let Some(label) = label {
        let _ = write!(out, " [{label}]");
    }
    let _ = writeln!(out, "\n  {}", entry.summary);
    let _ = writeln!(out, "  Follow when: {}", a.triggers.join("; "));
    if !a.prerequisites.is_empty() {
        let _ = writeln!(out, "  Prerequisites: {}", a.prerequisites.join("; "));
    }
    if !a.not_for.is_empty() {
        let _ = writeln!(out, "  Not for: {}", a.not_for.join("; "));
    }
    for p in &entry.parameters {
        let _ = writeln!(
            out,
            "  Parameter {} ({:?}, default {}): {}",
            p.name, p.kind, p.default, p.description
        );
    }
    if full {
        let _ = writeln!(
            out,
            "  sha256 (bundle of skill.json and SKILL.md): {}",
            entry.sha256
        );
        let _ = writeln!(out, "  How it works:");
        for line in walkthrough {
            let _ = writeln!(out, "    {line}");
        }
        let _ = writeln!(out, "  Procedure:");
        for line in entry.procedure.lines() {
            let _ = writeln!(out, "    {line}");
        }
    } else {
        let _ = writeln!(
            out,
            "  Gates: {}",
            entry
                .gates
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    out
}

fn list(as_json: bool) -> Result<()> {
    let lock = lock_for_cwd()?;
    if as_json {
        let skills: Vec<Value> = supported::catalog()
            .iter()
            .map(|e| {
                let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
                v["status"] = json!(adoption_label(lock.as_ref(), e));
                v["adopted"] = json!(v["status"] == "adopted");
                v
            })
            .collect();
        println!("{}", json!({"skills": skills}));
        return Ok(());
    }
    for e in supported::catalog() {
        println!(
            "{}",
            describe(e, Some(&adoption_label(lock.as_ref(), e)), false, &[])
        );
    }
    Ok(())
}

/// The requested version, else the newest embedded one.
fn pick(name: &str, version: Option<u32>) -> Result<&'static CatalogEntry> {
    let versions: Vec<u32> = supported::catalog()
        .iter()
        .filter(|e| e.name == name)
        .map(|e| e.workflow_version)
        .collect();
    let Some(latest) = versions.iter().max() else {
        let names: Vec<&str> = supported::catalog()
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        return Err(invalid(format!(
            "unknown skill {name}; available: {}",
            names.join(", ")
        )));
    };
    let wanted = version.unwrap_or(*latest);
    supported::catalog_entry(name, wanted).ok_or_else(|| {
        invalid(format!(
            "skill {name} has no version {wanted}; available: {versions:?}"
        ))
    })
}

fn show(name: &str, version: Option<u32>, as_json: bool) -> Result<()> {
    let entry = pick(name, version)?;
    if as_json {
        println!("{}", serde_json::to_value(entry)?);
    } else {
        let lock = lock_for_cwd()?;
        // The adopted pin's settings change the wording (CI label, merge method).
        let pin = lock.as_ref().and_then(|l| {
            supported::resolve(l)
                .into_iter()
                .find(|p| p.pin.sha256 == entry.sha256 && p.status == AdoptionStatus::Adopted)
        });
        let params = entry
            .manifest
            .effective_params(pin.as_ref().and_then(|p| p.pin.params.as_ref()))?;
        let lines = supported::walkthrough(&entry.manifest, Some(&params));
        println!(
            "{}",
            describe(
                entry,
                Some(&adoption_label(lock.as_ref(), entry)),
                true,
                &lines
            )
        );
    }
    Ok(())
}

// ---- add, remove -----------------------------------------------------------------------

/// `key=value` typed by the manifest; `None` means reset to the default.
fn parse_param(entry: &CatalogEntry, raw: &str) -> Result<(String, Option<Value>)> {
    use supported::ParameterType::{Bool, Integer, String as Text};
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| invalid(format!("--param {raw:?} must be key=value")))?;
    let def = entry
        .parameters
        .iter()
        .find(|p| p.name == key)
        .ok_or_else(|| {
            let known: Vec<&str> = entry.parameters.iter().map(|p| p.name.as_str()).collect();
            invalid(format!(
                "{} has no parameter {key}; known: {}",
                entry.name,
                known.join(", ")
            ))
        })?;
    if value == "null" {
        return Ok((key.into(), None));
    }
    let typed = match def.kind {
        Text => json!(value),
        Bool => json!(
            value
                .parse::<bool>()
                .map_err(|_| invalid(format!("{key} must be true or false")))?
        ),
        Integer => json!(
            value
                .parse::<i64>()
                .map_err(|_| invalid(format!("{key} must be an integer")))?
        ),
    };
    Ok((key.into(), Some(typed)))
}

fn pin_text(pin: Option<&SkillPin>) -> String {
    pin.map_or_else(
        || "(not adopted)".into(),
        |p| serde_json::to_string_pretty(p).unwrap_or_default(),
    )
}

fn diff(before: &str, after: &str) -> String {
    let mut out = String::new();
    for line in before.lines() {
        let _ = writeln!(out, "- {line}");
    }
    for line in after.lines() {
        let _ = writeln!(out, "+ {line}");
    }
    out
}

fn write_lock_file(root: &Path, lock: &SkillLock) -> Result<()> {
    std::fs::write(root.join(LOCK_PATH), supported::write_lock(lock)?)?;
    Ok(())
}

fn add(name: &str, version: Option<u32>, raw_params: &[String]) -> Result<()> {
    let root = repo_root(&cwd()?)?;
    let entry = pick(name, version)?;
    let mut lock = read_lock(&root)?.unwrap_or(SkillLock {
        version: 1,
        skills: Vec::new(),
    });
    let existing = lock.skills.iter().find(|p| p.name == name).cloned();
    let mut params: Params = existing
        .as_ref()
        .and_then(|p| p.params.clone())
        .unwrap_or_default();
    for raw in raw_params {
        match parse_param(entry, raw)? {
            (key, Some(value)) => {
                params.insert(key, value);
            }
            (key, None) => {
                params.remove(&key);
            }
        }
    }
    entry.manifest.effective_params(Some(&params))?;
    let pin = SkillPin::first_party(entry, Some(params));
    let (before, after) = (pin_text(existing.as_ref()), pin_text(Some(&pin)));
    supported::upsert_pin(&mut lock, pin);
    write_lock_file(&root, &lock)?;
    println!("{LOCK_PATH}:\n{}", diff(&before, &after));
    println!("Commit {LOCK_PATH}; the lock file in the repository is the adoption.");
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let root = repo_root(&cwd()?)?;
    let mut lock =
        read_lock(&root)?.ok_or_else(|| invalid("no skills lock file in this repository"))?;
    let before = pin_text(lock.skills.iter().find(|p| p.name == name));
    if !supported::remove_pin(&mut lock, name) {
        return Err(invalid(format!("{name} is not adopted in this repository")));
    }
    write_lock_file(&root, &lock)?;
    println!("{LOCK_PATH}:\n{}", diff(&before, &pin_text(None)));
    println!("Commit {LOCK_PATH} to make the removal effective.");
    Ok(())
}

// ---- Hub calls -------------------------------------------------------------------------

struct Hub {
    base: String,
    /// `owner/name` as the connection records it.
    raw_repo: String,
    repo: String,
    token: String,
}

impl Hub {
    fn for_dir(dir: &Path) -> Result<Self> {
        let connections = client_config::load()?;
        let connection = client_config::connection_for_path(&connections, dir)
            .filter(|c| client_config::is_current(c))
            .ok_or_else(|| {
                invalid(
                    "no client connection for this checkout; run `agentscm client connect` first",
                )
            })?;
        Ok(Self {
            base: connection.hub.clone(),
            raw_repo: connection.repo.clone(),
            repo: encode_path_segment(&connection.repo),
            token: crate::client_cli::read_token(&connection.token_file)?,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        tail: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let url = format!("{}/v1/hub/repos/{}/skills/{tail}", self.base, self.repo);
        self.send(method, url, query, body).await
    }

    /// Whether the Hub has `name` adopted for this repository.
    async fn adopted(&self, name: &str) -> Result<bool> {
        let url = format!("{}/v1/hub/skills", self.base);
        let (code, body) = self
            .send(reqwest::Method::GET, url, &[("repo", &self.raw_repo)], None)
            .await?;
        if code != 200 {
            return Err(invalid(format!("the Hub returned status {code}")));
        }
        Ok(body["adoption"]["pins"].as_array().is_some_and(|pins| {
            pins.iter()
                .any(|p| p["name"] == name && p["status"] == "adopted")
        }))
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: String,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let client = forge_http::client().map_err(|_| invalid("Hub client unavailable"))?;
        let mut request = client
            .request(method, url)
            .query(query)
            .bearer_auth(&self.token)
            .timeout(HUB_TIMEOUT);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(body)?);
        }
        forge_http::send(request, REPLY_BYTES)
            .await
            .map_err(|_| invalid("the Hub is unreachable"))
    }
}

async fn report_adoption() -> Result<()> {
    let dir = cwd()?;
    let root = repo_root(&dir)?;
    let lock =
        read_lock(&root)?.ok_or_else(|| invalid(format!("no {LOCK_PATH} in this repository")))?;
    let hub = Hub::for_dir(&root)?;
    let body = serde_json::to_value(&lock)?;
    let (status, reply) = hub
        .call(reqwest::Method::PUT, "adoption", &[], Some(&body))
        .await?;
    if status == 409 && reply["error"] == "forge_authoritative" {
        println!(
            "Nothing reported: {} Commit and merge {LOCK_PATH} to the default branch.",
            reply["message"]
                .as_str()
                .unwrap_or("the Hub reads the lock from the forge.")
        );
        return Ok(());
    }
    if status != 200 {
        return Err(invalid(format!(
            "the Hub refused the adoption report (status {status})"
        )));
    }
    let view = reply.get("adoption").unwrap_or(&reply);
    let source = view["source"].as_str().unwrap_or("unknown");
    println!("Hub adoption source now in effect: {source}");
    if source != "reported_by_checkout" {
        println!("Your report was stored but the Hub uses {source} for this repository.");
    }
    Ok(())
}

fn render(view: &RunView) -> String {
    let short = view.head_sha.get(..7).unwrap_or(&view.head_sha);
    let pr = view.pull_request.map_or("none".into(), |n| format!("#{n}"));
    let params = view
        .params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "{} v{} on {} (PR {pr}, head {short})\nParams: {}\n",
        view.skill.name,
        view.skill.workflow_version,
        view.branch,
        if params.is_empty() {
            "defaults"
        } else {
            &params
        }
    );
    for g in &view.gates {
        let state = match g.state {
            GateState::Done => "done",
            GateState::Current => "current",
            GateState::NotReached => "not reached",
        };
        let label = if g.assurance == Some(Assurance::SelfReported) {
            " [self-reported]"
        } else {
            ""
        };
        let _ = writeln!(out, "  {:<12} {state}{label}", g.id);
    }
    let _ = writeln!(out, "Next: {}", view.next_action);
    let _ = write!(
        out,
        "Finishing blocked: {}",
        if view.blocks_finish { "yes" } else { "no" }
    );
    out
}

fn decode_run(status: u16, body: Value) -> Result<RunView> {
    if status != 200 {
        return Err(invalid(format!("the Hub returned status {status}")));
    }
    Ok(serde_json::from_value(body)?)
}

async fn status(branch: Option<String>, as_json: bool) -> Result<()> {
    let root = repo_root(&cwd()?)?;
    let branch = branch
        .or_else(|| current_branch(&root))
        .ok_or_else(|| invalid("detached HEAD; pass --branch"))?;
    let hub = Hub::for_dir(&root)?;
    let (code, body) = hub
        .call(
            reqwest::Method::GET,
            "runs/by-branch",
            &[("branch", &branch)],
            None,
        )
        .await?;
    if code == 404 {
        println!(
            "No pr-gate run for branch {branch} yet. Start one by recording the branch gate:\n  agentscm skill evidence --gate branch --kind task_branch --summary \"<what this branch is for>\""
        );
        return Ok(());
    }
    let view = decode_run(code, body.clone())?;
    println!(
        "{}",
        if as_json {
            body.to_string()
        } else {
            render(&view)
        }
    );
    Ok(())
}

struct EvidenceArgs {
    gate: String,
    kind: String,
    summary: String,
    url: Option<String>,
    pull_request: Option<u64>,
    skill: String,
    detail_json: Option<PathBuf>,
    branch: Option<String>,
}

async fn record(args: EvidenceArgs) -> Result<()> {
    let root = repo_root(&cwd()?)?;
    let branch = args
        .branch
        .or_else(|| current_branch(&root))
        .ok_or_else(|| invalid("detached HEAD; pass --branch"))?;
    let head_sha =
        git(&root, &["rev-parse", "HEAD"]).ok_or_else(|| invalid("no commit at HEAD"))?;
    let kind: EvidenceKind = serde_json::from_value(json!(args.kind))
        .map_err(|_| invalid(format!("unknown evidence kind {}", args.kind)))?;
    let detail: Option<Detail> = match &args.detail_json {
        Some(path) => {
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 {
                return Err(Error::Limit("--detail-json exceeds 64 KiB".into()));
            }
            Some(serde_json::from_slice(&bytes)?)
        }
        None => None,
    };
    let evidence = Evidence {
        skill: Some(args.skill),
        gate: args.gate,
        kind,
        head_sha,
        assurance: Assurance::SelfReported,
        summary: args.summary,
        url: args.url,
        branch: Some(branch),
        pull_request: args.pull_request,
        detail,
    };
    let hub = Hub::for_dir(&root)?;
    let (code, body) = hub
        .call(
            reqwest::Method::POST,
            "evidence",
            &[],
            Some(&serde_json::to_value(&evidence)?),
        )
        .await?;
    if code == 409 {
        return Err(invalid(format!(
            "evidence refused ({}): {}",
            body["error"].as_str().unwrap_or("unknown"),
            body["message"].as_str().unwrap_or("")
        )));
    }
    if code != 200 {
        return Err(invalid(format!(
            "the Hub rejected the evidence (status {code}): {}",
            body["error"].as_str().unwrap_or("")
        )));
    }
    println!("{}", render(&decode_run(code, body)?));
    println!("Evidence recorded as self-reported.");
    Ok(())
}

// ---- Stop hook -------------------------------------------------------------------------

/// A pass: the hook output that lets the agent stop.
fn allow(why: impl std::fmt::Display) -> Value {
    eprintln!("agentscm skill stop: {why}; not blocking");
    json!({})
}

enum Verdict {
    Allow,
    Block(String),
}

fn bound(text: &str) -> String {
    text.chars().take(MAX_REASON).collect()
}

/// Only gates the agent can act on block; ci, approval, merge and post_merge never do. A local
/// HEAD the run has not seen blocks at any gate before the merge: the evidence is for an older
/// commit, so the tests and the review must be redone.
fn verdict_for_run(view: &RunView, local_head: Option<&str>) -> Verdict {
    let before_merge = view
        .current_gate
        .as_deref()
        .is_some_and(|g| !matches!(g, "merge" | "post_merge"));
    if let Some(local) = local_head.filter(|l| before_merge && *l != view.head_sha) {
        let short = |s: &str| s.chars().take(7).collect::<String>();
        return Verdict::Block(bound(&format!(
            "HEAD is {} but pr-gate last saw {}. Record tests for {} (`agentscm skill evidence --gate tests ...`); the review must be redone for the new commit.",
            short(local),
            short(&view.head_sha),
            short(local)
        )));
    }
    let waiting = matches!(
        view.current_gate.as_deref(),
        Some("ci" | "approval" | "merge" | "post_merge")
    );
    let stop_gate = view
        .gates
        .iter()
        .any(|g| Some(&g.id) == view.current_gate.as_ref() && g.enforce.contains(&Enforce::Stop));
    if view.blocks_finish && !waiting && stop_gate {
        Verdict::Block(bound(&view.next_action))
    } else {
        Verdict::Allow
    }
}

async fn decide(
    root: &Path,
    branch: &str,
    ahead: Option<u64>,
    local_head: Option<&str>,
) -> std::result::Result<Verdict, String> {
    let hub = Hub::for_dir(root).map_err(|e| e.to_string())?;
    let (code, body) = hub
        .call(
            reqwest::Method::GET,
            "runs/by-branch",
            &[("branch", branch)],
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    match code {
        200 => {
            let view = decode_run(code, body).map_err(|e| e.to_string())?;
            Ok(verdict_for_run(&view, local_head))
        }
        // No run: a lock the Hub has not adopted yet cannot have one, and must not block.
        404 if !hub
            .adopted(DEFAULT_SKILL)
            .await
            .map_err(|e| e.to_string())? =>
        {
            Err(format!(
                "{DEFAULT_SKILL} is pinned in {LOCK_PATH} here but not adopted on the hub yet: adoption takes effect once the lock is on the default branch (forge connected), or run `agentscm skill report-adoption` when no forge is connected"
            ))
        }
        404 if ahead.is_some_and(|n| n > 0) => Ok(Verdict::Block(format!(
            "This repository adopted pr-gate and branch {branch} has commits but no run. Start it: agentscm skill evidence --gate branch --kind task_branch --summary \"<what this branch is for>\""
        ))),
        404 => Ok(Verdict::Allow),
        other => Err(format!("the Hub returned status {other}")),
    }
}

pub async fn stop(data_dir: &Path, agent: StopAgent, mut stdin: impl Read) -> Value {
    let mut bytes = Vec::new();
    if stdin
        .by_ref()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return allow("cannot read hook input");
    }
    let input = match StopInput::read(&bytes[..]) {
        Ok(input) => input,
        Err(e) => return allow(e),
    };
    let dir = input.cwd.clone().or_else(|| std::env::current_dir().ok());
    let Some(root) = dir.as_deref().and_then(|d| repo_root(d).ok()) else {
        return allow("not a Git repository");
    };
    let adopted = match read_lock(&root) {
        Ok(Some(lock)) => supported::resolve(&lock)
            .iter()
            .any(|p| p.pin.name == DEFAULT_SKILL && p.status == AdoptionStatus::Adopted),
        Ok(None) => false,
        Err(e) => return allow(format!("unreadable lock file: {e}")),
    };
    if !adopted {
        return json!({});
    }
    let Some(branch) = current_branch(&root) else {
        return allow("detached HEAD");
    };
    let default = default_branch(&root);
    if branch == default {
        return json!({});
    }
    let guard = match agent_stop::prepare(data_dir, agent, Some(&root), &branch, &bytes[..]) {
        Ok(Prepared::Done(output)) => return output,
        Ok(Prepared::Run(guard)) => guard,
        Err(e) => return allow(e),
    };
    let ahead = commits_ahead(&root, &default);
    let local_head = git(&root, &["rev-parse", "HEAD"]);
    let result = match decide(&root, &branch, ahead, local_head.as_deref()).await {
        Ok(Verdict::Block(reason)) => json!({"failures": [], "next_action": reason}),
        Ok(Verdict::Allow) => json!({"receipt": "allow", "workspace_changed_during_run": false}),
        Err(e) => return allow(e),
    };
    let reason = result["next_action"].as_str().map(str::to_owned);
    match guard.finish(&result, "Run `agentscm skill status`.", MAX_REASON) {
        Ok(mut out) => {
            // The guard wraps the reason as JSON; the agent should read the plain action.
            if let (Some(reason), Some("block")) = (reason, out["decision"].as_str()) {
                out["reason"] = json!(reason);
            }
            out
        }
        Err(e) => allow(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_gate() -> &'static CatalogEntry {
        pick("pr-gate", None).unwrap()
    }

    #[test]
    fn params_are_typed_by_the_manifest() {
        let e = pr_gate();
        assert_eq!(
            parse_param(e, "ci_label=ci-extended").unwrap().1,
            Some(json!("ci-extended"))
        );
        assert_eq!(
            parse_param(e, "retarget_stacked=false").unwrap().1,
            Some(json!(false))
        );
        assert_eq!(parse_param(e, "ci_label=null").unwrap().1, None);
        assert!(parse_param(e, "retarget_stacked=maybe").is_err());
        assert!(parse_param(e, "nope=1").is_err());
        assert!(parse_param(e, "ci_label").is_err());
    }

    #[test]
    fn enum_values_and_unknown_skills_are_refused() {
        let e = pr_gate();
        let mut p = Params::new();
        p.insert("merge_method".into(), json!("octopus"));
        assert!(e.manifest.effective_params(Some(&p)).is_err());
        assert!(pick("nope", None).is_err());
        assert!(pick("pr-gate", Some(99)).is_err());
    }

    #[test]
    fn diff_marks_both_sides() {
        assert_eq!(diff("a", "b"), "- a\n+ b\n");
    }
}
