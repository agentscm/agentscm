//! An MCP-owned, same-user hook relay. Credentials and HTTPS pools stay in memory.
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::{JoinHandle, JoinSet};

use crate::hub::forge_http::{check_base_url, send};

#[cfg(unix)]
#[path = "hook_relay/unix.rs"]
mod transport;
#[cfg(windows)]
#[path = "hook_relay/windows.rs"]
mod transport;

pub const STDIN_BYTES: usize = 4 << 20;
pub const REPLY_BYTES: usize = 1 << 20;
const HEAD_BYTES: usize = 64 << 10;
const PROBE: Duration = Duration::from_millis(10);
const MAX_TIME: Duration = Duration::from_millis(4500);
const CONNECTIONS: usize = 32;
const OBSERVERS: usize = 128;
// Wall time includes sleep; 30 seconds keeps edit bursts pooled without trusting long-idle sockets.
const POOL_IDLE: Duration = Duration::from_secs(30);
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

fn unavailable() -> io::Error {
    io::Error::other("hook relay unavailable")
}

/// No URL comes from a socket peer: an address is bound to one configured Hub.
#[derive(Clone)]
pub struct Address {
    hub: String,
    key: String,
    #[cfg(unix)]
    directory: std::path::PathBuf,
}

impl Address {
    pub fn new(config: &Path, hub: &str) -> io::Result<Self> {
        let hub = check_base_url(hub).map_err(|_| unavailable())?;
        let mut digest = Sha256::new();
        for value in [
            b"agentscm-hook-relay-v1".as_slice(),
            env!("CARGO_PKG_VERSION").as_bytes(),
            crate::BUILD_COMMIT.as_bytes(),
            config.as_os_str().as_encoded_bytes(),
            hub.as_bytes(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
        let executable = std::fs::metadata(std::env::current_exe()?)?;
        digest.update(executable.len().to_be_bytes());
        digest.update(
            executable
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| unavailable())?
                .as_nanos()
                .to_be_bytes(),
        );
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            let value = std::env::var_os(name).unwrap_or_default();
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_encoded_bytes());
        }
        Ok(Self {
            hub,
            key: format!("{:x}", digest.finalize()),
            #[cfg(unix)]
            directory: transport::directory(),
        })
    }
}

/// Only hook headers cross the local transport; tokens must never be persisted or logged.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequest {
    pub agent: String,
    pub repo: Option<String>,
    pub token: String,
    pub root: Option<String>,
    pub client: Option<String>,
    pub report_completion: bool,
    #[serde(skip)]
    pub body: Vec<u8>,
}

impl HookRequest {
    pub fn request(self, client: &Client, hub: &str) -> io::Result<reqwest::RequestBuilder> {
        if self.agent.is_empty()
            || !self
                .agent
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || self.body.len() > STDIN_BYTES
        {
            return Err(unavailable());
        }
        let mut url = reqwest::Url::parse(&format!("{hub}/v1/hub/hook/{}", self.agent))
            .map_err(|_| unavailable())?;
        if let Some(repo) = &self.repo {
            url.query_pairs_mut().append_pair("repo", repo);
        }
        let mut request = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(&self.token)
            .body(self.body);
        if let Some(root) = self
            .root
            .as_deref()
            .and_then(|root| root.parse::<reqwest::header::HeaderValue>().ok())
        {
            request = request.header("X-Agentscm-Root", root);
        }
        if let Some(client) = &self.client {
            request = request.header(crate::hub::onboarding::CLIENT_HEADER, client);
        }
        if self.report_completion {
            request = request.header(crate::hub::insights::COMPLETION_HEADER, "true");
        }
        Ok(request)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    request: HookRequest,
    remaining_ms: u64,
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin), limit: usize) -> io::Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length > limit {
        return Err(unavailable());
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    body: &[u8],
    limit: usize,
) -> io::Result<()> {
    if body.len() > limit {
        return Err(unavailable());
    }
    writer.write_u32(body.len() as u32).await?;
    writer.write_all(body).await
}

/// `None` means nothing was submitted and direct fallback is safe. Once the handshake
/// succeeds, every failure is `Some(Err)`: the non-idempotent POST must not be resent.
pub async fn try_forward(
    address: &Address,
    request: &HookRequest,
    deadline: Instant,
) -> Option<io::Result<(u16, Value)>> {
    if deadline <= Instant::now() {
        return None;
    }
    let probe_deadline = (Instant::now() + PROBE).min(deadline);
    let connected = async {
        let mut stream = transport::connect(address).await?;
        let mut hello = [0; 64];
        stream.read_exact(&mut hello).await?;
        if hello != address.key.as_bytes() {
            return Err(unavailable());
        }
        Ok::<_, io::Error>(stream)
    };
    let mut stream = tokio::time::timeout_at(probe_deadline.into(), connected)
        .await
        .ok()?
        .ok()?;
    let forwarded = async {
        let head = serde_json::to_vec(&serde_json::json!({
            "request": request,
            "remaining_ms": deadline.saturating_duration_since(Instant::now()).min(MAX_TIME).as_millis() as u64,
        }))
        .map_err(|_| unavailable())?;
        write_frame(&mut stream, &head, HEAD_BYTES).await?;
        write_frame(&mut stream, &request.body, STDIN_BYTES).await?;
        let reply = read_frame(&mut stream, REPLY_BYTES + 64).await?;
        serde_json::from_slice::<Option<(u16, Value)>>(&reply)
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)
    };
    Some(
        tokio::time::timeout_at(deadline.into(), forwarded)
            .await
            .unwrap_or_else(|_| Err(unavailable()))
            .map_err(|_| unavailable()),
    )
}

async fn wait_for_owner(address: &Address) -> io::Result<()> {
    let subscribe = async {
        let mut stream = transport::connect(address).await?;
        let mut hello = [0; 64];
        stream.read_exact(&mut hello).await?;
        if hello != address.key.as_bytes() {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        write_frame(&mut stream, &[], HEAD_BYTES).await?;
        if stream.read_u8().await? != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(stream)
    };
    let mut stream = tokio::time::timeout(MAX_TIME, subscribe)
        .await
        .map_err(|_| unavailable())??;
    let mut byte = [0];
    if stream.read(&mut byte).await? != 0 {
        return Err(unavailable());
    }
    Ok(())
}

struct Pool {
    client: Client,
    created: SystemTime,
    last_reply: Option<SystemTime>,
}

impl Pool {
    fn new(client: Client) -> Self {
        Self {
            client,
            created: SystemTime::now(),
            last_reply: None,
        }
    }

    fn client(&mut self) -> io::Result<Client> {
        if self
            .last_reply
            .unwrap_or(self.created)
            .elapsed()
            .map_or(true, |idle| idle >= POOL_IDLE)
        {
            self.client = crate::hub::forge_http::client().map_err(|_| unavailable())?;
            self.created = SystemTime::now();
            self.last_reply = None;
        }
        Ok(self.client.clone())
    }
}

/// Drop this with the MCP stdio server: no independent process or orphan lifecycle.
pub struct Relay(JoinHandle<()>);

impl Relay {
    pub fn spawn(address: Address) -> Self {
        Self(tokio::spawn(async move {
            let Ok(client) = crate::hub::forge_http::client() else {
                eprintln!("agentscm: hook relay client unavailable; hooks can use direct HTTPS");
                return;
            };
            let pool = Arc::new(Mutex::new(Pool::new(client)));
            let mut retry = RETRY_MIN;
            loop {
                match transport::Listener::bind(&address) {
                    Ok(Some(mut listener)) => {
                        let mut calls = JoinSet::new();
                        let mut observers = JoinSet::new();
                        loop {
                            tokio::select! {
                                connected = listener.accept(), if calls.len() < CONNECTIONS => {
                                    match connected {
                                        Ok(stream) => {
                                            calls.spawn(serve(stream, address.clone(), pool.clone()));
                                        }
                                        Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => {
                                            tokio::time::sleep(PROBE).await;
                                        }
                                        Err(_) => {
                                            eprintln!("agentscm: hook relay accept failed; hooks can use direct HTTPS");
                                            return;
                                        }
                                    }
                                }
                                completed = calls.join_next(), if !calls.is_empty() => {
                                    if let Some(Ok(Some(mut stream))) = completed {
                                        if observers.len() < OBSERVERS {
                                            observers.spawn(async move {
                                                if stream.write_u8(0).await.is_ok() {
                                                    let mut byte = [0];
                                                    let _ = stream.read(&mut byte).await;
                                                }
                                            });
                                        } else {
                                            let _ = tokio::time::timeout(PROBE, stream.write_u8(1)).await;
                                        }
                                    }
                                }
                                _ = observers.join_next(), if !observers.is_empty() => {}
                            }
                        }
                    }
                    Ok(None) => match wait_for_owner(&address).await {
                        Ok(()) => retry = RETRY_MIN,
                        Err(error) if error.kind() == io::ErrorKind::InvalidData => return,
                        Err(_) => {
                            tokio::time::sleep(retry).await;
                            retry = (retry * 2).min(RETRY_MAX);
                        }
                    },
                    Err(_) => {
                        eprintln!("agentscm: hook relay bind failed; hooks can use direct HTTPS");
                        return;
                    }
                }
            }
        }))
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    mut stream: transport::ServerStream,
    address: Address,
    pool: Arc<Mutex<Pool>>,
) -> Option<transport::ServerStream> {
    let started = Instant::now();
    let exchange = async {
        stream.write_all(address.key.as_bytes()).await?;
        let head = read_frame(&mut stream, HEAD_BYTES).await?;
        if head.is_empty() {
            return Ok(true);
        }
        let mut head: Head = serde_json::from_slice(&head).map_err(|_| unavailable())?;
        head.request.body = read_frame(&mut stream, STDIN_BYTES).await?;
        let deadline = started + Duration::from_millis(head.remaining_ms).min(MAX_TIME);
        let response = async {
            if deadline <= Instant::now() {
                return Err(unavailable());
            }
            let client = pool.lock().map_err(|_| unavailable())?.client()?;
            let request = head.request.request(&client, &address.hub)?;
            let reply = send(request, REPLY_BYTES)
                .await
                .map_err(|_| unavailable())?;
            pool.lock().map_err(|_| unavailable())?.last_reply = Some(SystemTime::now());
            Ok::<_, io::Error>(reply)
        };
        let response = tokio::time::timeout_at(deadline.into(), response)
            .await
            .ok()
            .and_then(Result::ok);
        let reply = serde_json::to_vec(&response).map_err(|_| unavailable())?;
        write_frame(&mut stream, &reply, REPLY_BYTES + 64).await?;
        Ok::<_, io::Error>(false)
    };
    match tokio::time::timeout(MAX_TIME, exchange).await {
        Ok(Ok(true)) => Some(stream),
        _ => None,
    }
}

#[cfg(test)]
#[path = "hook_relay/tests.rs"]
mod tests;
