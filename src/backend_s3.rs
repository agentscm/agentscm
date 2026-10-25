//! S3-compatible [`ImmutableBackend`]: path-style requests signed with SigV4.
//! Objects are written once with a conditional PUT and deleted only for
//! shared-conversation bodies after a database tombstone. Callers
//! must be blocking threads (as with the sync Postgres client), never async tasks.

use std::fmt;
use std::future::Future;
use std::io::Cursor;
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Method, Response, Url, redirect};
use tokio::runtime::{Builder, Runtime};

use crate::backend::{
    ImmutableBackend, ObjectKey, ObjectMetadata, PutOutcome, range_end, slice_range,
    validate_identifier,
};
use crate::error::{Error, Result};
use crate::hmac::hmac_sha256;
use crate::store::{MAX_OBJECT_BYTES, digest};

const ERROR_HEAD_BYTES: usize = 4096;
/// Keeps the full key well under S3's 1024-byte limit.
const MAX_PREFIX_BYTES: usize = 256;

#[derive(Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[redacted]")
            .finish()
    }
}

impl S3Config {
    /// Errors name the field, never its value.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Invalid(format!("S3 {what}")));
        let name = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_');
        if parse_endpoint(&self.endpoint).is_none() {
            return bad(
                "endpoint must be https://host[:port] (http:// only for a loopback IP) \
                 without path, credentials, query or fragment",
            );
        }
        if self.region.is_empty()
            || !self
                .region
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return bad("region must be lowercase letters, digits or '-'");
        }
        if !(3..=63).contains(&self.bucket.len())
            || !self
                .bucket
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.'))
        {
            return bad("bucket must be 3 to 63 bytes of a-z, 0-9, '.' or '-'");
        }
        if self.prefix.len() > MAX_PREFIX_BYTES
            || (!self.prefix.is_empty()
                && !self.prefix.split('/').all(|segment| {
                    !matches!(segment, "" | "." | "..") && segment.bytes().all(name)
                }))
        {
            return bad(
                "prefix must be at most 256 bytes of '/'-separated segments of letters, digits, \
                 '.', '-' or '_'",
            );
        }
        if self.access_key_id.is_empty()
            || !self
                .access_key_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
            || self.secret_access_key.is_empty()
        {
            return bad(
                "credentials need a non-empty secret and a key id of A-Z, a-z, 0-9, '.', '_', '+' or '-'",
            );
        }
        Ok(())
    }
}

/// Returns the `scheme://host[:port]` base and the `Host` header value.
fn parse_endpoint(endpoint: &str) -> Option<(String, String)> {
    let url = Url::parse(endpoint).ok()?;
    let host = url.host_str()?;
    let allowed = match url.scheme() {
        "https" => true,
        "http" => host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        _ => false,
    };
    if !allowed
        || url.path() != "/"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    Some((format!("{}://{host}", url.scheme()), host))
}

pub struct S3Backend {
    config: S3Config,
    base: String,
    host: String,
    client: Client,
    runtime: Option<Runtime>,
}

impl S3Backend {
    pub fn open(config: S3Config) -> Result<Self> {
        config.validate()?;
        let (base, host) = parse_endpoint(&config.endpoint).expect("validated endpoint");
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let client = runtime
            .block_on(async {
                Client::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(60))
                    .redirect(redirect::Policy::none())
                    // Proxy env vars would carry signed requests (plaintext for loopback http) elsewhere.
                    .no_proxy()
                    .build()
            })
            .map_err(|_| Error::Io(std::io::Error::other("object store client setup failed")))?;
        Ok(Self {
            config,
            base,
            host,
            client,
            runtime: Some(runtime),
        })
    }

    /// Operators may instead pre-create the bucket.
    // shortcut: empty CreateBucket body, so AWS regions other than us-east-1 need a pre-created bucket; add a LocationConstraint when one must be created there
    pub fn create_bucket(&self) -> Result<()> {
        let path = format!("/{}", self.config.bucket);
        self.block(async {
            let response = self
                .request(Method::PUT, &path, &[], digest(b""), Vec::new())
                .await?;
            match response.status().as_u16() {
                200 => Ok(()),
                409 => match failure(response).await {
                    (_, Some(code))
                        if code == "BucketAlreadyOwnedByYou" || code == "BucketAlreadyExists" =>
                    {
                        Ok(())
                    }
                    (status, code) => Err(http_error(status, code)),
                },
                _ => {
                    let (status, code) = failure(response).await;
                    Err(http_error(status, code))
                }
            }
        })
    }

    fn block<T>(&self, future: impl Future<Output = T>) -> T {
        self.runtime
            .as_ref()
            .expect("runtime lives until drop")
            .block_on(future)
    }

    // shortcut: path-style addressing only; add virtual-hosted when a provider requires it
    fn object_path(&self, key: &ObjectKey, incarnation: &str) -> Result<String> {
        key.validate()?;
        validate_identifier("physical incarnation", incarnation)?;
        // Every component is [A-Za-z0-9_-] or a validated prefix, so no escaping
        // is needed and distinct keys cannot alias.
        let prefix = if self.config.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.config.prefix)
        };
        Ok(format!(
            "/{}/{prefix}{}/{}/{}/{incarnation}/{}",
            self.config.bucket,
            key.trust_domain(),
            key.repository(),
            key.profile(),
            key.id()
        ))
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        extra: &[(&str, &str)],
        payload_sha256: String,
        body: Vec<u8>,
    ) -> Result<Response> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let signed = sign(
            &self.config,
            method.as_str(),
            &self.host,
            path,
            extra,
            &payload_sha256,
            now,
        );
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", payload_sha256)
            .header("authorization", signed.authorization)
            .body(body);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        request.send().await.map_err(transport)
    }

    /// None when the object does not exist. Never returns more than
    /// [`MAX_OBJECT_BYTES`]. Bytes are not verified.
    async fn fetch(&self, path: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let mut response = self
            .request(Method::GET, path, &[], digest(b""), Vec::new())
            .await?;
        if response.status().as_u16() != 200 {
            return match failure(response).await {
                // Only S3's own NoSuchKey means a missing object; any other 404 is misconfiguration.
                (404, Some(code)) if code == "NoSuchKey" => Ok(None),
                (status, code) => Err(http_error(status, code)),
            };
        }
        let too_large = || Error::Limit(format!("object {id} exceeds size limit"));
        if response
            .content_length()
            .is_some_and(|length| length > MAX_OBJECT_BYTES)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if (bytes.len() + chunk.len()) as u64 > MAX_OBJECT_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }

    fn read_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        let path = self.object_path(key, incarnation)?;
        let bytes = self.block(self.fetch(&path, key.id()))?.ok_or_else(|| {
            Error::NotFound(format!("object incarnation: {}/{}", key.id(), incarnation))
        })?;
        if digest(&bytes) != key.id() {
            return Err(Error::Invalid(format!(
                "object digest mismatch: {}",
                key.id()
            )));
        }
        Ok(bytes)
    }
}

impl Drop for S3Backend {
    fn drop(&mut self) {
        // Dropping a Runtime inside an async context panics; this does not.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl ImmutableBackend for S3Backend {
    /// S3 has no directories, so there is nothing to prepare.
    fn prepare_publication(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        key.validate()?;
        validate_identifier("physical incarnation", incarnation)
    }

    // shortcut: needs server support for conditional PUT (AWS S3, R2, versitygw); a server that ignores If-None-Match overwrites with identical bytes
    fn put_if_absent(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        bytes: &[u8],
    ) -> Result<PutOutcome> {
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::Limit("object exceeds size limit".into()));
        }
        let path = self.object_path(key, incarnation)?;
        if digest(bytes) != key.id() {
            return Err(Error::Invalid(format!(
                "object bytes do not match object id: {}",
                key.id()
            )));
        }
        self.block(async {
            // The payload hash is the object id.
            let response = self
                .request(
                    Method::PUT,
                    &path,
                    &[("if-none-match", "*")],
                    key.id().to_owned(),
                    bytes.to_vec(),
                )
                .await?;
            match response.status().as_u16() {
                200 => Ok(PutOutcome::Created),
                status @ (409 | 412) => match self.fetch(&path, key.id()).await {
                    Ok(Some(existing)) if existing == bytes => Ok(PutOutcome::AlreadyPresent),
                    Ok(None) => Err(http_error(status, None)),
                    Ok(Some(_)) | Err(Error::Limit(_)) => Err(Error::Conflict(format!(
                        "immutable object differs at backend key: {}",
                        key.id()
                    ))),
                    Err(error) => Err(error),
                },
                _ => {
                    let (status, code) = failure(response).await;
                    Err(http_error(status, code))
                }
            }
        })
    }

    fn get_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<Vec<u8>> {
        self.read_verified(key, incarnation)
    }

    fn inspect_verified(&self, key: &ObjectKey, incarnation: &str) -> Result<ObjectMetadata> {
        let bytes = self.read_verified(key, incarnation)?;
        Ok(ObjectMetadata {
            size: bytes.len() as u64,
            digest: key.id().to_owned(),
        })
    }

    fn open_encoded(
        &self,
        key: &ObjectKey,
        incarnation: &str,
    ) -> Result<Box<dyn std::io::Read + Send + '_>> {
        let path = self.object_path(key, incarnation)?;
        let bytes = self
            .block(self.fetch(&path, key.id()))?
            .ok_or_else(|| Error::MissingObject(key.id().to_owned()))?;
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn delete_incarnation(&self, key: &ObjectKey, incarnation: &str) -> Result<()> {
        let path = self.object_path(key, incarnation)?;
        self.block(async {
            let response = self
                .request(Method::DELETE, &path, &[], digest(b""), Vec::new())
                .await?;
            match response.status().as_u16() {
                // S3 answers 204 whether or not the key existed; some servers say 404.
                200 | 204 => Ok(()),
                404 => match failure(response).await {
                    (_, Some(code)) if code == "NoSuchKey" => Ok(()),
                    (status, code) => Err(http_error(status, code)),
                },
                _ => {
                    let (status, code) = failure(response).await;
                    Err(http_error(status, code))
                }
            }
        })
    }

    fn get_range_verified(
        &self,
        key: &ObjectKey,
        incarnation: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>> {
        let end = range_end(offset, length)?;
        slice_range(&self.read_verified(key, incarnation)?, offset, end)
    }
}

/// Transport failures get a fixed message: reqwest errors carry the request URL.
fn transport(error: reqwest::Error) -> Error {
    let what = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    Error::Io(std::io::Error::other(format!("object store {what}")))
}

/// Status and the S3 `<Code>` from the first 4 KiB of an error body; nothing else.
async fn failure(mut response: Response) -> (u16, Option<String>) {
    let status = response.status().as_u16();
    let mut head = Vec::new();
    while head.len() < ERROR_HEAD_BYTES {
        let Ok(Some(chunk)) = response.chunk().await else {
            break;
        };
        head.extend_from_slice(&chunk[..chunk.len().min(ERROR_HEAD_BYTES - head.len())]);
    }
    let text = String::from_utf8_lossy(&head);
    let code = text
        .split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| code)
        .filter(|code| code.len() <= 64 && code.bytes().all(|c| c.is_ascii_alphanumeric()))
        .filter(|code| !code.is_empty())
        .map(str::to_owned);
    (status, code)
}

fn http_error(status: u16, code: Option<String>) -> Error {
    Error::Io(std::io::Error::other(match code {
        Some(code) => format!("object store returned HTTP {status} ({code})"),
        None => format!("object store returned HTTP {status}"),
    }))
}

pub struct Signed {
    pub authorization: String,
    pub amz_date: String,
}

/// AWS Signature Version 4 for S3. `path` is unencoded; each segment is
/// percent-encoded here. The signed headers are `host`, `x-amz-content-sha256`,
/// `x-amz-date` and `extra` (which the caller must also send).
pub fn sign(
    config: &S3Config,
    method: &str,
    host: &str,
    path: &str,
    extra: &[(&str, &str)],
    payload_sha256: &str,
    unix_seconds: u64,
) -> Signed {
    let amz_date = format_date(unix_seconds);
    let day = &amz_date[..8];
    let mut headers: Vec<(String, &str)> = vec![
        ("host".into(), host),
        ("x-amz-content-sha256".into(), payload_sha256),
        ("x-amz-date".into(), &amz_date),
    ];
    headers.extend(
        extra
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim())),
    );
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let uri = path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    // The empty line is the (always empty) canonical query string.
    let canonical =
        format!("{method}\n{uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_sha256}");
    let scope = format!("{day}/{}/s3/aws4_request", config.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        digest(canonical.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", config.secret_access_key).as_bytes(),
        day.as_bytes(),
    );
    for part in [config.region.as_str(), "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature: String = hmac_sha256(&key, to_sign.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Signed {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            config.access_key_id
        ),
        amz_date,
    }
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` in UTC (civil-from-days).
fn format_date(unix_seconds: u64) -> String {
    let (days, rest) = ((unix_seconds / 86_400) as i64, unix_seconds % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> S3Config {
        S3Config {
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: "examplebucket".into(),
            prefix: String::new(),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    #[test]
    fn signature_matches_the_aws_get_object_example() {
        let signed = sign(
            &config("https://examplebucket.s3.amazonaws.com"),
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[("range", "bytes=0-9")],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            1_369_353_600,
        );
        assert_eq!(signed.amz_date, "20130524T000000Z");
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn dates_format_in_utc() {
        assert_eq!(format_date(0), "19700101T000000Z");
        assert_eq!(format_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(format_date(1_709_210_096), "20240229T123456Z");
    }

    #[test]
    fn config_validation_rejects_unsafe_values() {
        assert!(config("https://s3.example.com").validate().is_ok());
        assert!(config("http://127.0.0.1:9000").validate().is_ok());
        assert!(config("http://[::1]:9000").validate().is_ok());
        for endpoint in [
            "http://localhost:9000",
            "http://10.0.0.1",
            "ftp://s3.example.com",
            "https://s3.example.com/path",
            "https://s3.example.com?x=1",
            "https://user:pass@s3.example.com",
            "s3.example.com",
        ] {
            assert!(config(endpoint).validate().is_err(), "{endpoint}");
        }
        for bucket in ["ab", "UPPER", "under_score", &"a".repeat(64)] {
            let mut bad = config("https://s3.example.com");
            bad.bucket = bucket.into();
            assert!(bad.validate().is_err(), "{bucket}");
        }
        for prefix in ["a/b", "a-b/c.d", "x"] {
            let mut good = config("https://s3.example.com");
            good.prefix = prefix.into();
            assert!(good.validate().is_ok(), "{prefix}");
        }
        for prefix in ["/a", "a/", "a//b", ".", "a/../b", "a b", "é"] {
            let mut bad = config("https://s3.example.com");
            bad.prefix = prefix.into();
            assert!(bad.validate().is_err(), "{prefix}");
        }
        let mut bad = config("https://s3.example.com");
        bad.region = "US East".into();
        assert!(bad.validate().is_err());
        let mut bad = config("https://s3.example.com");
        bad.access_key_id = String::new();
        assert!(bad.validate().is_err());
        let mut bad = config("https://s3.example.com");
        bad.secret_access_key = String::new();
        assert!(bad.validate().is_err());
        assert!(!format!("{:?}", config("https://s3.example.com")).contains("wJalr"));
    }
}
