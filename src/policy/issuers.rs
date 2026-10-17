use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const ISSUER_REGISTRY_VERSION: u32 = 1;
pub const MAX_ISSUER_REGISTRY_BYTES: usize = 256 * 1024;
pub const MAX_ISSUERS: usize = 128;
pub const MAX_KEYS_PER_ISSUER: usize = 8;
pub const MAX_CONTROL_ROOTS: usize = 16;
pub const MAX_CHECK_KINDS: usize = 64;

const MAX_IDENTIFIER_BYTES: usize = 128;
const KEY_ID_DOMAIN: &[u8] = b"agentscm.ed25519-key/v1\0";

/// Operator-installed administrative metadata. Parsing or evaluating this value
/// does not authenticate it and never grants dispatch, signing, or admission.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuerRegistrySnapshot {
    pub version: u32,
    pub repository: String,
    pub epoch: u64,
    pub issuers: Vec<IssuerRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuerRecord {
    pub issuer_id: String,
    pub trust_domain: String,
    pub common_control: CommonControl,
    pub allowed_check_kinds: Vec<String>,
    pub keys: Vec<IssuerKeyRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommonControl {
    Known { roots: Vec<String> },
    Unknown {},
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssuerKeyRecord {
    pub key_id: String,
    pub public_key_hex: String,
    pub not_before_ms: u64,
    pub not_after_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyTrustStatus {
    UnknownIssuer,
    UnknownKey,
    FutureIssued,
    NotYetValid,
    Expired,
    Revoked,
    Trusted,
}

/// A registry lookup result. `Trusted` means only that the installed metadata
/// currently permits the key; a verifier must still validate the signature.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyTrust {
    pub status: KeyTrustStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key_hex: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndependenceAssessment {
    pub independent_count: usize,
    pub groups: Vec<Vec<String>>,
    pub unknown_issuer_ids: Vec<String>,
    pub unknown_control_issuer_ids: Vec<String>,
    pub disallowed_check_kind_issuer_ids: Vec<String>,
    pub duplicate_issuer_ids: Vec<String>,
}

#[derive(Serialize)]
struct CanonicalRegistry<'a> {
    version: u32,
    repository: &'a str,
    epoch: u64,
    issuers: &'a [IssuerRecord],
}

impl IssuerRegistrySnapshot {
    pub fn from_slice(input: &[u8]) -> Result<Self> {
        if input.len() > MAX_ISSUER_REGISTRY_BYTES {
            return Err(Error::Limit(format!(
                "issuer registry input exceeds {MAX_ISSUER_REGISTRY_BYTES} bytes"
            )));
        }
        let registry: Self = serde_json::from_slice(input)?;
        registry.validate()?;
        registry.canonical_bytes()?;
        Ok(registry.canonicalized())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != ISSUER_REGISTRY_VERSION {
            return Err(Error::Invalid(format!(
                "unsupported issuer registry version {}",
                self.version
            )));
        }
        validate_identifier("repository ID", &self.repository)?;
        validate_sql_integer("issuer registry epoch", self.epoch)?;
        validate_count("issuers", self.issuers.len(), MAX_ISSUERS)?;

        let mut issuer_ids = HashSet::with_capacity(self.issuers.len());
        let mut key_ids = HashSet::new();
        let mut public_keys = HashSet::new();
        for issuer in &self.issuers {
            issuer.validate()?;
            if !issuer_ids.insert(issuer.issuer_id.as_str()) {
                return Err(Error::Invalid(format!(
                    "duplicate issuer ID: {}",
                    issuer.issuer_id
                )));
            }
            for key in &issuer.keys {
                if !key_ids.insert(key.key_id.as_str()) {
                    return Err(Error::Invalid(format!(
                        "duplicate issuer key ID: {}",
                        key.key_id
                    )));
                }
                if !public_keys.insert(key.public_key_hex.as_str()) {
                    return Err(Error::Invalid(
                        "duplicate issuer public key material".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let canonical = self.canonicalized();
        let mut writer = LimitedWriter::new(MAX_ISSUER_REGISTRY_BYTES);
        let result = serde_json::to_writer(
            &mut writer,
            &CanonicalRegistry {
                version: canonical.version,
                repository: &canonical.repository,
                epoch: canonical.epoch,
                issuers: &canonical.issuers,
            },
        );
        if writer.exceeded {
            return Err(Error::Limit(format!(
                "canonical issuer registry exceeds {MAX_ISSUER_REGISTRY_BYTES} bytes"
            )));
        }
        result?;
        Ok(writer.bytes)
    }

    pub fn digest(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    /// Looks up current administrative key trust and returns verifier input.
    /// No status from this method proves that a signature is valid.
    pub fn key_trust(
        &self,
        issuer_id: &str,
        key_id: &str,
        issued_at_ms: u64,
        now_ms: u64,
    ) -> Result<KeyTrust> {
        self.validate()?;
        validate_sql_integer("receipt issuance timestamp", issued_at_ms)?;
        validate_sql_integer("evaluation timestamp", now_ms)?;

        let Some(issuer) = self
            .issuers
            .iter()
            .find(|issuer| issuer.issuer_id == issuer_id)
        else {
            return Ok(KeyTrust::without_key(KeyTrustStatus::UnknownIssuer));
        };
        let Some(key) = issuer.keys.iter().find(|key| key.key_id == key_id) else {
            return Ok(KeyTrust::without_key(KeyTrustStatus::UnknownKey));
        };

        let status = if key
            .revoked_at_ms
            .is_some_and(|revoked_at_ms| revoked_at_ms <= now_ms)
        {
            // Current revocation is authoritative even for a backdated receipt.
            KeyTrustStatus::Revoked
        } else if issued_at_ms > now_ms {
            KeyTrustStatus::FutureIssued
        } else if issued_at_ms < key.not_before_ms || now_ms < key.not_before_ms {
            KeyTrustStatus::NotYetValid
        } else if issued_at_ms >= key.not_after_ms || now_ms >= key.not_after_ms {
            KeyTrustStatus::Expired
        } else {
            KeyTrustStatus::Trusted
        };
        Ok(KeyTrust {
            status,
            public_key_hex: Some(key.public_key_hex.clone()),
        })
    }

    /// Returns an immutable installed key record for a caller that separately
    /// authenticates this registry snapshot.
    pub fn key_record(&self, issuer_id: &str, key_id: &str) -> Result<Option<&IssuerKeyRecord>> {
        self.validate()?;
        Ok(self
            .issuers
            .iter()
            .find(|issuer| issuer.issuer_id == issuer_id)
            .and_then(|issuer| issuer.keys.iter().find(|key| key.key_id == key_id)))
    }

    /// Groups selected eligible issuers by common control. The registry must be
    /// authenticated and installed by the caller; this pure result is not an
    /// admission decision.
    pub fn assess_independence(
        &self,
        selected_issuer_ids: &[String],
        check_kind: &str,
    ) -> Result<IndependenceAssessment> {
        self.validate()?;
        validate_identifier("check kind", check_kind)?;
        validate_count(
            "selected issuer IDs",
            selected_issuer_ids.len(),
            MAX_ISSUERS,
        )?;

        let issuer_indexes: HashMap<&str, usize> = self
            .issuers
            .iter()
            .enumerate()
            .map(|(index, issuer)| (issuer.issuer_id.as_str(), index))
            .collect();
        let mut components = Components::new(self.issuers.len());
        for left in 0..self.issuers.len() {
            for right in (left + 1)..self.issuers.len() {
                if shares_control(&self.issuers[left], &self.issuers[right]) {
                    components.union(left, right);
                }
            }
        }

        let mut seen = HashSet::with_capacity(selected_issuer_ids.len());
        let mut duplicate_issuer_ids = Vec::new();
        let mut unknown_issuer_ids = Vec::new();
        let mut unknown_control_issuer_ids = Vec::new();
        let mut disallowed_check_kind_issuer_ids = Vec::new();
        let mut eligible = Vec::new();

        for issuer_id in selected_issuer_ids {
            validate_identifier("selected issuer ID", issuer_id)?;
            if !seen.insert(issuer_id.as_str()) {
                duplicate_issuer_ids.push(issuer_id.clone());
                continue;
            }
            let Some(&index) = issuer_indexes.get(issuer_id.as_str()) else {
                unknown_issuer_ids.push(issuer_id.clone());
                continue;
            };
            let issuer = &self.issuers[index];
            let allowed = issuer
                .allowed_check_kinds
                .iter()
                .any(|allowed| allowed == check_kind);
            if !allowed {
                disallowed_check_kind_issuer_ids.push(issuer_id.clone());
            }
            let known_control = matches!(&issuer.common_control, CommonControl::Known { .. });
            if !known_control {
                unknown_control_issuer_ids.push(issuer_id.clone());
            }
            if allowed && known_control {
                eligible.push((components.find(index), issuer_id.clone()));
            }
        }

        let mut grouped: HashMap<usize, Vec<String>> = HashMap::new();
        for (component, issuer_id) in eligible {
            grouped.entry(component).or_default().push(issuer_id);
        }
        let mut groups: Vec<Vec<String>> = grouped.into_values().collect();
        for group in &mut groups {
            group.sort();
        }
        groups.sort();
        unknown_issuer_ids.sort();
        unknown_control_issuer_ids.sort();
        disallowed_check_kind_issuer_ids.sort();
        duplicate_issuer_ids.sort();
        duplicate_issuer_ids.dedup();

        Ok(IndependenceAssessment {
            independent_count: groups.len(),
            groups,
            unknown_issuer_ids,
            unknown_control_issuer_ids,
            disallowed_check_kind_issuer_ids,
            duplicate_issuer_ids,
        })
    }

    fn canonicalized(&self) -> Self {
        let mut canonical = self.clone();
        for issuer in &mut canonical.issuers {
            issuer.canonicalize();
        }
        canonical
            .issuers
            .sort_by(|left, right| left.issuer_id.cmp(&right.issuer_id));
        canonical
    }
}

impl IssuerRecord {
    fn validate(&self) -> Result<()> {
        validate_identifier("issuer ID", &self.issuer_id)?;
        validate_identifier("issuer trust domain", &self.trust_domain)?;
        self.common_control.validate()?;
        validate_count(
            "allowed check kinds",
            self.allowed_check_kinds.len(),
            MAX_CHECK_KINDS,
        )?;
        validate_unique_strings("allowed check kinds", &self.allowed_check_kinds)?;
        for check_kind in &self.allowed_check_kinds {
            validate_identifier("allowed check kind", check_kind)?;
        }
        validate_count("issuer keys", self.keys.len(), MAX_KEYS_PER_ISSUER)?;
        for key in &self.keys {
            key.validate()?;
        }
        Ok(())
    }

    fn canonicalize(&mut self) {
        if let CommonControl::Known { roots } = &mut self.common_control {
            roots.sort();
        }
        self.allowed_check_kinds.sort();
        self.keys
            .sort_by(|left, right| left.key_id.cmp(&right.key_id));
    }
}

impl CommonControl {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Known { roots } => {
                validate_count("common-control roots", roots.len(), MAX_CONTROL_ROOTS)?;
                if roots.is_empty() {
                    return Err(Error::Invalid(
                        "known common control must name at least one root".to_owned(),
                    ));
                }
                validate_unique_strings("common-control roots", roots)?;
                for root in roots {
                    validate_identifier("common-control root", root)?;
                }
                Ok(())
            }
            Self::Unknown {} => Ok(()),
        }
    }
}

impl IssuerKeyRecord {
    fn validate(&self) -> Result<()> {
        validate_digest("issuer key ID", &self.key_id)?;
        let expected_key_id = issuer_key_id(&self.public_key_hex)?;
        if self.key_id != expected_key_id {
            return Err(Error::Invalid(
                "issuer key ID does not match its Ed25519 public key".to_owned(),
            ));
        }
        validate_sql_integer("key not-before timestamp", self.not_before_ms)?;
        validate_positive_sql_integer("key not-after timestamp", self.not_after_ms)?;
        if self.not_after_ms <= self.not_before_ms {
            return Err(Error::Invalid(
                "key not-after timestamp must be later than not-before timestamp".to_owned(),
            ));
        }
        if let Some(revoked_at_ms) = self.revoked_at_ms {
            validate_sql_integer("key revocation timestamp", revoked_at_ms)?;
        }
        Ok(())
    }

    pub fn public_key_bytes(&self) -> Result<[u8; 32]> {
        decode_public_key_hex(&self.public_key_hex)
    }
}

impl KeyTrust {
    fn without_key(status: KeyTrustStatus) -> Self {
        Self {
            status,
            public_key_hex: None,
        }
    }
}

/// Derives an algorithm-bound identifier from exactly 32 encoded key bytes.
/// This validates the encoding and length only; cryptographic key validation
/// belongs to the strict signature verifier.
pub fn issuer_key_id(public_key_hex: &str) -> Result<String> {
    let key_bytes = decode_public_key_hex(public_key_hex)?;
    let mut hasher = Sha256::new();
    hasher.update(KEY_ID_DOMAIN);
    hasher.update(key_bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn decode_public_key_hex(public_key_hex: &str) -> Result<[u8; 32]> {
    if public_key_hex.len() != 64
        || !public_key_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "issuer public key must be 32 bytes of lowercase hexadecimal".to_owned(),
        ));
    }
    let mut decoded = [0u8; 32];
    for (index, pair) in public_key_hex.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(decoded)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("hex encoding was validated before decoding"),
    }
}

fn shares_control(left: &IssuerRecord, right: &IssuerRecord) -> bool {
    if left.trust_domain == right.trust_domain {
        return true;
    }
    let (CommonControl::Known { roots: left_roots }, CommonControl::Known { roots: right_roots }) =
        (&left.common_control, &right.common_control)
    else {
        return false;
    };
    left_roots
        .iter()
        .any(|left_root| right_roots.contains(left_root))
}

struct Components {
    parents: Vec<usize>,
}

impl Components {
    fn new(len: usize) -> Self {
        Self {
            parents: (0..len).collect(),
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let parent = self.parents[index];
        if parent == index {
            index
        } else {
            let root = self.find(parent);
            self.parents[index] = root;
            root
        }
    }

    fn union(&mut self, left: usize, right: usize) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root != right_root {
            let (first, second) = if left_root < right_root {
                (left_root, right_root)
            } else {
                (right_root, left_root)
            };
            self.parents[second] = first;
        }
    }
}

fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Error::Limit(format!(
            "{kind} must contain 1 to {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(Error::Invalid(format!(
            "{kind} contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_digest(kind: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(format!(
            "{kind} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

fn validate_count(kind: &str, count: usize, maximum: usize) -> Result<()> {
    if count > maximum {
        return Err(Error::Limit(format!("{kind} count exceeds {maximum}")));
    }
    Ok(())
}

fn validate_unique_strings(kind: &str, values: &[String]) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if !seen.insert(value.as_str()) {
            return Err(Error::Invalid(format!("{kind} contains a duplicate")));
        }
    }
    Ok(())
}

fn validate_positive_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(Error::Invalid(format!("{kind} must be positive")));
    }
    validate_sql_integer(kind, value)
}

fn validate_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(Error::Limit(format!("{kind} exceeds SQLite range")));
    }
    Ok(())
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "issuer registry exceeded its canonical byte limit",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
