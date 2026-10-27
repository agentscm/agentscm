//! Outbound HTTP to forges: one hardened client and bounded JSON helpers.
use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Url};
use serde_json::Value;

use super::{HubError, Result};

const FORGE_BODY_BYTES: usize = 64 * 1024;

/// The client for every outbound call, including the CLI's: no redirects (a
/// forge must not bounce tokens elsewhere), short timeouts, JSON replies.
pub fn client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Client::builder()
        .user_agent("agentscm-hub")
        .default_headers(headers)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| HubError::Unavailable("could not build HTTP client".into()))
}

/// `https://` with any host, or `http://` only for a loopback IP literal;
/// no credentials, query or fragment. Returns it without a trailing slash.
/// Errors never echo the input.
pub fn check_base_url(url: &str) -> Result<String> {
    let bad = || {
        HubError::Invalid(
            "URL must be https://host (http:// only for a loopback IP) \
             without credentials, query or fragment"
                .into(),
        )
    };
    let parsed = Url::parse(url).map_err(|_| bad())?;
    let host_ok = match parsed.scheme() {
        "https" => parsed.host_str().is_some_and(|host| !host.is_empty()),
        "http" => parsed
            .host_str()
            .map(|host| host.trim_matches(['[', ']']))
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback()),
        _ => false,
    };
    if !host_ok
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

pub struct Forge {
    client: Client,
}

impl Forge {
    pub fn new() -> Result<Self> {
        Ok(Self { client: client()? })
    }

    pub async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value)> {
        send(self.client.post(url).form(form), FORGE_BODY_BYTES).await
    }

    pub async fn get_json(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Value)> {
        let request = self.client.get(url);
        let request = match bearer {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        send(request, FORGE_BODY_BYTES).await
    }
}

/// Reads at most `max_bytes`; a body that is not JSON is `Null` beside its status.
pub async fn send(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<(u16, Value)> {
    let full = send_full(request, max_bytes).await?;
    Ok((full.status, full.body))
}

/// A reply with the headers GitHub's caching, paging and rate limits use.
pub struct Full {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Value,
}

/// Why [`send_capped`] failed: a body past the cap is its own case so callers can retry smaller.
#[derive(Debug)]
pub enum SendError {
    TooLarge,
    Failed(HubError),
}

impl From<SendError> for HubError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::TooLarge => Self::Unavailable("forge response too large".into()),
            SendError::Failed(error) => error,
        }
    }
}

/// Like [`send`], keeping the response headers.
pub async fn send_full(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<Full> {
    Ok(send_capped(request, max_bytes).await?)
}

/// [`send_full`] with an over-limit body reported as [`SendError::TooLarge`].
pub async fn send_capped(
    request: reqwest::RequestBuilder,
    max_bytes: usize,
) -> std::result::Result<Full, SendError> {
    // Fixed text only: a reqwest error can carry the URL and its query.
    let failed = |error: reqwest::Error| {
        let what = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "could not connect"
        } else {
            "failed"
        };
        SendError::Failed(HubError::Unavailable(format!("forge request {what}")))
    };
    let mut response = request.send().await.map_err(failed)?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    // Not content-length: it can be absent or wrong.
    while let Some(chunk) = response.chunk().await.map_err(failed)? {
        if body.len() + chunk.len() > max_bytes {
            return Err(SendError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Full {
        status,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

/// A loopback server for unit tests: answers one request with `reply` (status, extra header
/// lines, body) and returns the request line it saw.
#[cfg(test)]
pub(super) fn serve_once(
    status: u16,
    headers: &str,
    body: String,
) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let headers = headers.to_owned();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut seen = vec![0; 4096];
        let read = stream.read(&mut seen).unwrap();
        let text = String::from_utf8_lossy(&seen[..read]).into_owned();
        let reply = format!(
            "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{body}",
            body.len()
        );
        // The client may hang up early on an oversized body.
        let _ = stream.write_all(reply.as_bytes());
        text.lines().next().unwrap_or_default().to_owned()
    });
    (base, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_over_limit_body_is_a_typed_outcome() {
        let (base, server) = serve_once(200, "", format!("[\"{}\"]", "x".repeat(64)));
        let request = client().unwrap().get(format!("{base}/a"));
        assert!(matches!(
            send_capped(request, 32).await,
            Err(SendError::TooLarge)
        ));
        server.join().unwrap();
        // Callers that only know `HubError` keep today's text.
        let text = HubError::from(SendError::TooLarge).to_string();
        assert_eq!(text, "unavailable: forge response too large");
    }

    #[test]
    fn base_urls_are_vetted_and_never_echoed() {
        for (input, want) in [
            ("https://github.com", "https://github.com"),
            (
                "https://gitlab.example.com/sub/",
                "https://gitlab.example.com/sub",
            ),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
            ("http://127.9.9.9", "http://127.9.9.9"),
            ("http://[::1]:9", "http://[::1]:9"),
        ] {
            assert_eq!(check_base_url(input).unwrap(), want);
        }
        for input in [
            "http://example.com",
            "http://localhost",
            "http://127.0.0.1.evil.com",
            "ftp://127.0.0.1",
            "https://u:p4ss@github.com",
            "https://github.com?x=1",
            "https://github.com/#frag",
            "https://github.com?",
            "github.com",
            "",
        ] {
            let message = check_base_url(input).unwrap_err().to_string();
            assert!(
                !message.contains("p4ss") && !message.contains("evil"),
                "{message}"
            );
        }
    }
}
