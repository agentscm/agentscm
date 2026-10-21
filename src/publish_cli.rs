//! `agentscm hub publish`: send one exported native revision to the hub, which pushes it
//! to GitHub as a branch and opens a pull request. Attached-worktree candidates, whose
//! branch the client has already pushed, get a pull request on GitHub or a merge request on
//! GitLab. Forge credentials never reach this process: it reads the local git-export mirror
//! and talks to the hub with the agent token.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::authority::Authority;
use agentscm::git_export::Mirror;
use agentscm::hub::forge_http::send;
use agentscm::hub::publish::derive_id;
use agentscm::{Error, Result};
use clap::Args;
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::intent_cli::Hub;

/// A publication makes many forge calls on the hub; the client waits for all of them.
const LIMIT: Duration = Duration::from_secs(600);
const BODY_BYTES: usize = 1024 * 1024;
const MAX_BODY_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Args)]
pub struct PublishArgs {
    /// The git-export mirror that holds the exported commit (opened read-only).
    #[arg(long)]
    mirror: PathBuf,
    /// The integration event whose export is published.
    #[arg(long)]
    event: u64,
    /// The new branch on the forge. It must not exist; nothing is ever force-pushed.
    #[arg(long)]
    branch: String,
    /// The branch the pull request targets; the mirror's base commit must be on the forge.
    #[arg(long)]
    base: String,
    #[arg(long)]
    title: String,
    /// Pull request description (a file of at most 64 KiB).
    #[arg(long)]
    body_file: Option<PathBuf>,
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

pub async fn run(args: PublishArgs, data_dir: &Path) -> Result<()> {
    let data = data_dir.canonicalize()?;
    if !data.join("state.sqlite").is_file() {
        return Err(invalid("publish requires initialized native data"));
    }
    let mirror_path = args.mirror.canonicalize()?;
    if mirror_path.starts_with(&data) || data.starts_with(&mirror_path) {
        return Err(invalid(
            "mirror and native data must be separate directories",
        ));
    }
    let repository = Authority::open_read_only(&data)?.repository_id()?;
    let mirror = Mirror::open(&args.git, &mirror_path, &repository)?;
    let export = mirror.publication(args.event)?;
    let body = match &args.body_file {
        Some(path) => {
            let mut text = String::new();
            std::fs::File::open(path)?
                .take(MAX_BODY_FILE_BYTES + 1)
                .read_to_string(&mut text)?;
            if text.len() as u64 > MAX_BODY_FILE_BYTES {
                return Err(invalid("the body file is over 64 KiB"));
            }
            text
        }
        None => String::new(),
    };

    // Hash every file once; the bytes are held so the upload sends what was hashed.
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut changes = Vec::new();
    for change in &export.changes {
        let sha256 = match &change.blob {
            Some(blob) => {
                let bytes = mirror.blob(blob, change.size)?;
                let sha256 = hex(&Sha256::digest(&bytes));
                blobs.insert(sha256.clone(), bytes);
                Some(sha256)
            }
            None => None,
        };
        changes.push(
            json!({"path": change.path, "mode": change.mode, "blob": change.blob,
            "sha256": sha256, "size": change.size}),
        );
    }

    let hub = Hub::connect(LIMIT)?;
    let whoami = call(&hub, Method::GET, "/v1/hub/whoami", None).await?;
    let name = whoami["token"]["repository"].as_str().unwrap_or_default();
    let path = format!("{}/publications", Hub::repo_path(name));
    let id = derive_id(name, args.event, &export.commit, &args.branch);
    let manifest = json!({"publication_id": id, "revision": export.revision,
        "event_seq": args.event, "commit": export.commit, "tree": export.tree,
        "parent": export.parent, "branch": args.branch, "base": args.base,
        "title": args.title, "body": body, "author": export.author,
        "committer": export.committer, "message": export.message, "changes": changes});
    let created = call(&hub, Method::POST, &path, Some(manifest)).await?;
    let result = upload_and_run(&hub, &path, &id, created, &blobs, || Ok(())).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    let rejected = result["states"]["branch"]["state"] == "rejected";
    match result["error"].as_str() {
        Some(error) => Err(Error::Invalid(format!(
            "publication incomplete: {error} (run the same command again to resume)"
        ))),
        None if rejected => Err(invalid(
            "the forge rejected the branch (see the message above); nothing was pushed",
        )),
        None => Ok(()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hub reply's JSON on success; the hub's error code otherwise.
fn refuse((status, reply): (u16, Value)) -> Result<Value> {
    match status {
        200..=299 => Ok(reply),
        401 => Err(invalid(
            "the agent token was rejected (expired or revoked; mint a new one)",
        )),
        _ => Err(Error::Invalid(format!(
            "hub refused ({status}): {}",
            reply["error"].as_str().unwrap_or("error")
        ))),
    }
}

async fn call(hub: &Hub, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
    let mut request = hub
        .client
        .request(method, format!("{}{path}", hub.base))
        .bearer_auth(&hub.token)
        .timeout(hub.limit);
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    refuse(
        send(request, BODY_BYTES)
            .await
            .map_err(|_| invalid("hub unreachable"))?,
    )
}

async fn upload_and_run(
    hub: &Hub,
    path: &str,
    id: &str,
    created: Value,
    blobs: &BTreeMap<String, Vec<u8>>,
    mut qualify: impl FnMut() -> Result<()>,
) -> Result<Value> {
    for sha256 in created["missing_blobs"].as_array().into_iter().flatten() {
        let sha256 = sha256.as_str().unwrap_or_default();
        let bytes = blobs
            .get(sha256)
            .ok_or_else(|| invalid("the hub asked for a file this publication does not have"))?;
        let url = format!("{}{path}/{id}/blobs/{sha256}", hub.base);
        let request = hub
            .client
            .put(url)
            .bearer_auth(&hub.token)
            .timeout(hub.limit)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes.clone());
        refuse(
            send(request, BODY_BYTES)
                .await
                .map_err(|_| invalid("hub unreachable"))?,
        )?;
    }
    qualify()?;
    let result = call(
        hub,
        Method::POST,
        &format!("{path}/{id}/run"),
        Some(json!({})),
    )
    .await?;
    Ok(result)
}

pub(crate) struct CandidatePublication<'a> {
    pub mirror: &'a Mirror,
    pub subject: agentscm::git_export::CandidateSubject,
    pub endpoint: String,
    pub branch: String,
    pub title: String,
    pub body: String,
    pub previous_publication: Option<String>,
}

pub(crate) async fn publish_candidate(
    request: CandidatePublication<'_>,
    mut qualify: impl FnMut() -> Result<()>,
) -> Result<Value> {
    use agentscm::hub::publish::{Candidate, Manifest, candidate_repository, derive_candidate_id};
    let export = request.mirror.candidate_publication(&request.subject.id)?;
    let hub = Hub::connect(LIMIT)?;
    let whoami = call(&hub, Method::GET, "/v1/hub/whoami", None).await?;
    let name = whoami["token"]["repository"].as_str().unwrap_or_default();
    if candidate_repository(&request.endpoint)
        .map_err(|e| invalid(&e.to_string()))?
        .1
        != name
    {
        return Err(invalid(
            "Hub token repository differs from attached endpoint",
        ));
    }
    let mut blobs = BTreeMap::new();
    let mut changes = Vec::new();
    for change in &export.changes {
        let sha256 = match &change.blob {
            Some(blob) => {
                let bytes = request.mirror.blob(blob, change.size)?;
                let hash = hex(&Sha256::digest(&bytes));
                blobs.insert(hash.clone(), bytes);
                Some(hash)
            }
            None => None,
        };
        changes.push(json!({"path":change.path,"mode":change.mode,"blob":change.blob,"sha256":sha256,"size":change.size}));
    }
    let id = derive_candidate_id(name, &request.subject.id, &export.commit, &request.branch);
    let mut manifest = json!({"revision":export.revision,"commit":export.commit,"tree":export.tree,"parent":export.parent,
        "branch":request.branch,"base":request.subject.target,"title":request.title,"body":request.body,
        "author":export.author,"committer":export.committer,"message":export.message,"changes":changes,
        "candidate":Candidate { subject:request.subject, endpoint:request.endpoint, previous_publication:request.previous_publication }});
    serde_json::from_value::<Manifest>(manifest.clone())?
        .validate()
        .map_err(|e| invalid(&e.to_string()))?;
    manifest["publication_id"] = json!(id);
    let path = format!("{}/publications", Hub::repo_path(name));
    qualify()?;
    let created = call(&hub, Method::POST, &path, Some(manifest)).await?;
    if created["state"]["pull_request"]["state"] == "opened"
        || created["state"]["pull_request"]["state"] == "merged"
    {
        let mut receipt = call(&hub, Method::GET, &format!("{path}/{id}"), None).await?;
        receipt["receipt_replayed"] = json!(true);
        return Ok(receipt);
    }
    upload_and_run(&hub, &path, &id, created, &blobs, qualify).await
}

/// Read-only recovery through the existing Hub publication controller.
pub(crate) async fn recover_candidate(
    endpoint: &str,
    attachment: &str,
    publication: &str,
) -> Result<Value> {
    let (_, name) = agentscm::hub::publish::candidate_repository(endpoint)
        .map_err(|e| invalid(&e.to_string()))?;
    if !publication.starts_with("pub_")
        || publication.len() != 36
        || !publication[4..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("invalid publication id"));
    }
    let hub = Hub::connect(LIMIT)?;
    let path = format!("{}/publications/{publication}", Hub::repo_path(&name));
    let retained = call(&hub, Method::GET, &path, None).await?;
    if retained["candidate"]["subject"]["attachment"] != attachment
        || retained["candidate"]["endpoint"] != endpoint
    {
        return Err(invalid("publication differs from attachment identity"));
    }
    call(
        &hub,
        Method::POST,
        &format!("{path}/recover"),
        Some(json!({})),
    )
    .await
}
