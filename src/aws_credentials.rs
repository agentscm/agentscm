//! Ambient AWS credentials for [`crate::backend_s3`]: the keys a container or instance role
//! provides without any configured secret. Sources are tried in the order the AWS SDKs use:
//! environment variables, the container credential endpoint (ECS tasks, EKS Pod Identity) and
//! the EC2 instance metadata service (IMDSv2).
// shortcut: no shared profile files, SSO or web-identity token exchange; add them, or move to
// `aws-config`, when a deployment cannot use one of the sources above.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, RequestBuilder, Url};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::hub::github::parse_rfc3339;

/// Keys are renewed this long before they lapse, so a request never signs with a dying key.
const REFRESH_MARGIN_SECONDS: u64 = 300;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY_BYTES: usize = 16 * 1024;
const CONTAINER_HOST: &str = "http://169.254.170.2";
const METADATA_HOST: &str = "http://169.254.169.254";

#[derive(Clone)]
pub(crate) struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Unix seconds; `None` for keys that never expire.
    expires_at: Option<u64>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .field("session_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    /// Worth signing with now, and not yet due for renewal.
    fn fresh(&self, now: u64) -> bool {
        self.expires_at
            .is_none_or(|end| now + REFRESH_MARGIN_SECONDS < end)
    }

    fn unexpired(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|end| now < end)
    }
}

/// The reply of the container and instance-metadata endpoints.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Document {
    access_key_id: String,
    secret_access_key: String,
    token: Option<String>,
    expiration: Option<String>,
}

pub(crate) struct Ambient {
    client: Client,
    env: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
    container_host: String,
    metadata_host: String,
    cached: Mutex<Option<Credentials>>,
}

impl Ambient {
    /// `client` must not follow redirects or use proxies: the replies carry secrets.
    pub(crate) fn new(client: Client) -> Self {
        Self::with(
            client,
            Box::new(|name| std::env::var(name).ok().filter(|value| !value.is_empty())),
            CONTAINER_HOST.into(),
            METADATA_HOST.into(),
        )
    }

    pub(crate) fn with(
        client: Client,
        env: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
        container_host: String,
        metadata_host: String,
    ) -> Self {
        Self {
            client,
            env,
            container_host,
            metadata_host,
            cached: Mutex::new(None),
        }
    }

    /// Current keys, renewed when close to expiry. A failed renewal keeps serving keys that
    /// have not expired yet.
    pub(crate) async fn credentials(&self) -> Result<Credentials> {
        let mut cached = self.cached.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if let Some(keys) = cached.as_ref().filter(|keys| keys.fresh(now)) {
            return Ok(keys.clone());
        }
        match self.fetch().await {
            Ok(keys) => {
                *cached = Some(keys.clone());
                Ok(keys)
            }
            Err(error) => cached
                .as_ref()
                .filter(|keys| keys.unexpired(now))
                .cloned()
                .ok_or(error),
        }
    }

    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    async fn fetch(&self) -> Result<Credentials> {
        if let (Some(access_key_id), Some(secret_access_key)) = (
            self.var("AWS_ACCESS_KEY_ID"),
            self.var("AWS_SECRET_ACCESS_KEY"),
        ) {
            return Ok(Credentials {
                access_key_id,
                secret_access_key,
                session_token: self.var("AWS_SESSION_TOKEN"),
                expires_at: None,
            });
        }
        if let Some(path) = self.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            if !path.starts_with('/') {
                return Err(unavailable(
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI is not a path",
                ));
            }
            let url = format!("{}{path}", self.container_host);
            return self
                .document(self.client.get(url), "container endpoint")
                .await;
        }
        if let Some(url) = self.var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
            let url = trusted_container_url(&url)?;
            let mut request = self.client.get(url);
            let token = match self.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
                Some(path) => Some(
                    std::fs::read_to_string(path)
                        .map_err(|_| unavailable("container authorization token file"))?
                        .trim()
                        .to_owned(),
                ),
                None => self.var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
            };
            if let Some(token) = token {
                request = request.header("authorization", token);
            }
            return self.document(request, "container endpoint").await;
        }
        if self
            .var("AWS_EC2_METADATA_DISABLED")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            return Err(none_found());
        }
        self.instance_role().await.map_err(|_| none_found())
    }

    async fn instance_role(&self) -> Result<Credentials> {
        let base = &self.metadata_host;
        let session = self
            .read(
                self.client
                    .put(format!("{base}/latest/api/token"))
                    .header("x-aws-ec2-metadata-token-ttl-seconds", "21600"),
                "instance metadata",
            )
            .await?;
        let session = String::from_utf8(session).map_err(|_| unavailable("instance metadata"))?;
        let get = |role: &str| {
            self.client
                .get(format!(
                    "{base}/latest/meta-data/iam/security-credentials/{role}"
                ))
                .header("x-aws-ec2-metadata-token", session.trim())
        };
        let listing = self.read(get(""), "instance metadata").await?;
        let listing = String::from_utf8_lossy(&listing);
        let role = listing.lines().next().unwrap_or("").trim();
        if role.is_empty()
            || !role
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+=,.@_-".contains(&c))
        {
            return Err(unavailable("instance metadata"));
        }
        self.document(get(role), "instance metadata").await
    }

    async fn document(&self, request: RequestBuilder, from: &str) -> Result<Credentials> {
        let body = self.read(request, from).await?;
        let document: Document = serde_json::from_slice(&body).map_err(|_| unavailable(from))?;
        let expires_at = match document.expiration {
            Some(text) => Some(
                parse_rfc3339(&text)
                    .and_then(|seconds| u64::try_from(seconds).ok())
                    .ok_or_else(|| unavailable(from))?,
            ),
            None => None,
        };
        if document.access_key_id.is_empty() || document.secret_access_key.is_empty() {
            return Err(unavailable(from));
        }
        Ok(Credentials {
            access_key_id: document.access_key_id,
            secret_access_key: document.secret_access_key,
            session_token: document.token.filter(|token| !token.is_empty()),
            expires_at,
        })
    }

    /// The body of a successful reply, at most [`MAX_REPLY_BYTES`]. Errors name only `from`.
    async fn read(&self, request: RequestBuilder, from: &str) -> Result<Vec<u8>> {
        let mut response = request
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|_| unavailable(from))?;
        if !response.status().is_success() {
            return Err(unavailable(from));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable(from))? {
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err(unavailable(from));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// `AWS_CONTAINER_CREDENTIALS_FULL_URI` may name only loopback or the link-local addresses of
/// the ECS and EKS Pod Identity agents, so a stray variable cannot send the token elsewhere.
fn trusted_container_url(text: &str) -> Result<Url> {
    let url = Url::parse(text).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
                .is_some_and(|ip| {
                    ip.is_loopback()
                        || ip == IpAddr::V4(Ipv4Addr::new(169, 254, 170, 2))
                        || ip == IpAddr::V4(Ipv4Addr::new(169, 254, 170, 23))
                        || ip == IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x23))
                })
    });
    url.ok_or_else(|| {
        unavailable("AWS_CONTAINER_CREDENTIALS_FULL_URI is not loopback or an agent address")
    })
}

fn unavailable(what: &str) -> Error {
    Error::Io(std::io::Error::other(format!(
        "AWS credentials unavailable: {what}"
    )))
}

fn none_found() -> Error {
    unavailable("no environment keys, container role or instance role found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex as StdMutex};

    type Seen = Arc<StdMutex<Vec<String>>>;

    /// Serves `routes` (request line prefix -> reply body) until the test ends; records each
    /// request's line and headers.
    fn serve(routes: Vec<(&'static str, String)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buffer = [0_u8; 4096];
                let length = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..length]).into_owned();
                let line = request.lines().next().unwrap_or("").to_owned();
                let reply = routes
                    .iter()
                    .find(|(prefix, _)| line.starts_with(prefix))
                    .map(|(_, body)| body.clone());
                log.lock().unwrap().push(request);
                let (status, body) = match reply {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", String::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (host, seen)
    }

    fn document(key: &str, expiration: &str) -> String {
        format!(
            r#"{{"AccessKeyId":"{key}","SecretAccessKey":"secret-{key}","Token":"token-{key}","Expiration":"{expiration}"}}"#
        )
    }

    fn ambient(vars: &[(&str, &str)], container: &str, metadata: &str) -> Ambient {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Ambient::with(
            Client::builder().no_proxy().build().unwrap(),
            Box::new(move |name| vars.get(name).cloned()),
            container.into(),
            metadata.into(),
        )
    }

    #[tokio::test]
    async fn environment_keys_win_and_never_expire() {
        let source = ambient(
            &[
                ("AWS_ACCESS_KEY_ID", "AKIDENV"),
                ("AWS_SECRET_ACCESS_KEY", "secret-env"),
                ("AWS_SESSION_TOKEN", "token-env"),
                ("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/ignored"),
            ],
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
        );
        let keys = source.credentials().await.unwrap();
        assert_eq!(keys.access_key_id, "AKIDENV");
        assert_eq!(keys.session_token.as_deref(), Some("token-env"));
        assert!(keys.fresh(u64::MAX / 2));
    }

    #[tokio::test]
    async fn a_container_role_is_fetched_once_and_renewed_before_it_expires() {
        let (host, seen) = serve(vec![(
            "GET /creds/task",
            document("AKIDTASK", "2099-01-01T00:00:00Z"),
        )]);
        let source = ambient(
            &[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/creds/task")],
            &host,
            "http://127.0.0.1:1",
        );
        for _ in 0..3 {
            let keys = source.credentials().await.unwrap();
            assert_eq!(keys.access_key_id, "AKIDTASK");
            assert_eq!(keys.session_token.as_deref(), Some("token-AKIDTASK"));
        }
        assert_eq!(seen.lock().unwrap().len(), 1);

        // Already inside the renewal margin: every call asks again.
        let (host, seen) = serve(vec![(
            "GET /creds/task",
            document("AKIDSOON", "2000-01-01T00:00:00Z"),
        )]);
        let source = ambient(
            &[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/creds/task")],
            &host,
            "http://127.0.0.1:1",
        );
        source.credentials().await.unwrap();
        source.credentials().await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_instance_role_uses_a_session_token_and_never_echoes_secrets_in_errors() {
        let (host, seen) = serve(vec![
            ("PUT /latest/api/token", "session-1".into()),
            (
                "GET /latest/meta-data/iam/security-credentials/ ",
                "role-a\n".into(),
            ),
            (
                "GET /latest/meta-data/iam/security-credentials/role-a",
                document("AKIDROLE", "2099-01-01T00:00:00Z"),
            ),
        ]);
        let source = ambient(&[], "http://127.0.0.1:1", &host);
        let keys = source.credentials().await.unwrap();
        assert_eq!(keys.access_key_id, "AKIDROLE");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[1]
                .to_ascii_lowercase()
                .contains("x-aws-ec2-metadata-token: session-1")
        );
        assert!(
            seen[2]
                .to_ascii_lowercase()
                .contains("x-aws-ec2-metadata-token: session-1")
        );
        drop(seen);

        let nothing = ambient(&[], "http://127.0.0.1:1", "http://127.0.0.1:1");
        let error = nothing.credentials().await.unwrap_err().to_string();
        assert!(error.contains("no environment keys"), "{error}");
        let disabled = ambient(&[("AWS_EC2_METADATA_DISABLED", "TRUE")], &host, &host);
        assert!(disabled.credentials().await.is_err());
    }

    #[tokio::test]
    async fn a_failed_renewal_keeps_unexpired_keys_only() {
        let (host, _) = serve(vec![]);
        let source = ambient(
            &[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/gone")],
            &host,
            "http://127.0.0.1:1",
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let keys = |expires_at| Credentials {
            access_key_id: "AKIDOLD".into(),
            secret_access_key: "secret-old".into(),
            session_token: None,
            expires_at: Some(expires_at),
        };
        *source.cached.lock().await = Some(keys(now + 100));
        assert_eq!(source.credentials().await.unwrap().access_key_id, "AKIDOLD");
        *source.cached.lock().await = Some(keys(now - 1));
        assert!(source.credentials().await.is_err());
    }

    #[test]
    fn the_full_uri_is_limited_to_loopback_and_agent_addresses() {
        for good in [
            "http://127.0.0.1:8080/creds",
            "http://[::1]/creds",
            "http://169.254.170.2/v2/credentials/x",
            "http://169.254.170.23/v1/credentials",
            "http://[fd00:ec2::23]/v1/credentials",
        ] {
            assert!(trusted_container_url(good).is_ok(), "{good}");
        }
        for bad in [
            "http://example.com/creds",
            "http://10.0.0.1/creds",
            "http://localhost/creds",
            "ftp://127.0.0.1/creds",
            "http://user:pw@127.0.0.1/creds",
            "not a url",
        ] {
            assert!(trusted_container_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn debug_output_hides_secrets() {
        let keys = Credentials {
            access_key_id: "AKIDX".into(),
            secret_access_key: "very-secret".into(),
            session_token: Some("very-token".into()),
            expires_at: None,
        };
        let text = format!("{keys:?}");
        assert!(!text.contains("very-secret") && !text.contains("very-token"));
    }
}
