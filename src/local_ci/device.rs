//! Repository-scoped authorization of a local validation device.
//!
//! Enrollment means a device key is authorized for one repository and owner
//! now. A signature means only that the key holder signed the bytes. The
//! `TrustedDeviceWorkerAsserted` label is a claim by an authorized device, not
//! proof that any execution occurred or that the candidate source was
//! isolated. Coverage and policy sufficiency belong to issue #273. This module
//! is pure: it holds public keys only, performs no I/O and grants no admission.

use std::ffi::OsStr;
use std::path::{Component, Path};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{
    LOCAL_CI_CHECK_KIND, MAX_ITEMS, bounded_bytes, count, domain_digest, json, unique, version,
};
use crate::evidence::{
    EvaluationClock, is_canonical_ed25519_encoding, validate_identifier, validate_lower_hex,
};
use crate::policy::issuers::{
    CommonControl, ISSUER_REGISTRY_VERSION, IssuerKeyRecord, IssuerRecord, IssuerRegistrySnapshot,
    MAX_KEYS_PER_ISSUER, issuer_key_id,
};
use crate::{Error, Result};

pub const DEVICE_ID_PREFIX: &str = "lcd_";
const DIGEST_DOMAIN: &[u8] = b"agentscm.local-ci.device-authorization.v1\0";
const ENROLLMENT_DOMAIN: &[u8] = b"agentscm-local-ci-device-enrollment-v1\0";

/// Person identity at an identity provider, never a Hub computer or OAuth device.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OwnerIdentity {
    pub provider: String,
    pub host: String,
    pub person_id: String,
}

impl OwnerIdentity {
    fn validate(&self) -> Result<()> {
        validate_identifier("owner provider", &self.provider)?;
        validate_identifier("owner host", &self.host)?;
        validate_identifier("owner person id", &self.person_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceKey {
    /// Must equal `issuer_key_id(public_key_hex)`.
    pub key_id: String,
    pub public_key_hex: String,
    pub not_before_ms: u64,
    pub not_after_ms: u64,
    pub rotated_at_ms: Option<u64>,
    pub revoked_at_ms: Option<u64>,
}

impl DeviceKey {
    fn validate(&self) -> Result<()> {
        if self.key_id != issuer_key_id(&self.public_key_hex)? {
            return Err(Error::Invalid(
                "device key ID does not match its public key".into(),
            ));
        }
        if self.not_after_ms <= self.not_before_ms {
            return Err(Error::Invalid(
                "device key not-after must be later than not-before".into(),
            ));
        }
        for time in [Some(self.not_before_ms), Some(self.not_after_ms)]
            .into_iter()
            .chain([self.rotated_at_ms, self.revoked_at_ms])
            .flatten()
        {
            sql_time(time)?;
        }
        Ok(())
    }

    /// Earliest time at which the registry schema must treat the key as revoked.
    fn retired_at_ms(&self, device_revoked: Option<u64>) -> Option<u64> {
        [self.rotated_at_ms, self.revoked_at_ms, device_revoked]
            .into_iter()
            .flatten()
            .min()
    }
}

fn sql_time(value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(Error::Limit(
            "timestamp must fit a signed 64-bit integer".into(),
        ));
    }
    Ok(())
}

fn default_check_kinds() -> Vec<String> {
    vec![LOCAL_CI_CHECK_KIND.to_owned()]
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceAuthorization {
    pub version: u32,
    pub device_id: String,
    pub repository: String,
    pub owner: OwnerIdentity,
    pub generation: u64,
    pub revoked_at_ms: Option<u64>,
    #[serde(default = "default_check_kinds")]
    pub allowed_check_kinds: Vec<String>,
    pub keys: Vec<DeviceKey>,
}

impl DeviceAuthorization {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let value: Self = json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        device_id(&self.device_id)?;
        validate_identifier("repository", &self.repository)?;
        self.owner.validate()?;
        generation(self.generation)?;
        if let Some(time) = self.revoked_at_ms {
            sql_time(time)?;
        }
        count(
            "allowed check kinds",
            self.allowed_check_kinds.len(),
            MAX_ITEMS,
        )?;
        let mut kinds = std::collections::BTreeSet::new();
        for kind in &self.allowed_check_kinds {
            validate_identifier("check kind", kind)?;
            unique(&mut kinds, kind, "check kind")?;
        }
        count("device keys", self.keys.len(), MAX_KEYS_PER_ISSUER)?;
        let mut ids = std::collections::BTreeSet::new();
        let mut public = std::collections::BTreeSet::new();
        for key in &self.keys {
            key.validate()?;
            unique(&mut ids, &key.key_id, "device key ID")?;
            unique(&mut public, &key.public_key_hex, "device public key")?;
        }
        bounded_bytes(self).map(|_| ())
    }

    /// Keys and check kinds are sorted so input order cannot change the digest.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut value = self.clone();
        value.keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
        value.allowed_check_kinds.sort();
        bounded_bytes(&value)
    }

    pub fn digest(&self) -> Result<String> {
        domain_digest(DIGEST_DOMAIN, &self.canonical_bytes()?)
    }

    /// Supersedes every current key with `new_key` and advances the generation by one.
    /// At the key bound the oldest superseded key is dropped, ordered by supersession
    /// time, start time and key ID; its ID then assesses as unknown, which fails closed.
    pub fn rotate(&self, new_key: DeviceKey, now_ms: u64) -> Result<Self> {
        if self.revoked_at_ms.is_some() {
            return Err(Error::Invalid("device authorization is revoked".into()));
        }
        if new_key.rotated_at_ms.is_some() || new_key.revoked_at_ms.is_some() {
            return Err(Error::Invalid("a new device key must be unretired".into()));
        }
        if self
            .keys
            .iter()
            .any(|key| key.key_id == new_key.key_id || key.public_key_hex == new_key.public_key_hex)
        {
            return Err(Error::Invalid("device key is already enrolled".into()));
        }
        let mut next = self.advanced()?;
        for key in &mut next.keys {
            key.rotated_at_ms.get_or_insert(now_ms);
        }
        if next.keys.len() >= MAX_KEYS_PER_ISSUER {
            let oldest = next
                .keys
                .iter()
                .enumerate()
                .min_by_key(|(_, key)| (key.rotated_at_ms, key.not_before_ms, key.key_id.clone()))
                .map(|(index, _)| index);
            if let Some(index) = oldest {
                next.keys.remove(index);
            }
        }
        next.keys.push(new_key);
        next.validate()?;
        Ok(next)
    }

    /// Revokes the whole device and advances the generation by one.
    pub fn revoke(&self, now_ms: u64) -> Result<Self> {
        if self.revoked_at_ms.is_some() {
            return Err(Error::Invalid(
                "device authorization is already revoked".into(),
            ));
        }
        let mut next = self.advanced()?;
        next.revoked_at_ms = Some(now_ms);
        next.validate()?;
        Ok(next)
    }

    fn advanced(&self) -> Result<Self> {
        self.validate()?;
        let mut next = self.clone();
        next.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::Limit("authorization generation overflow".into()))?;
        Ok(next)
    }

    /// Projects this device as one issuer in the existing registry schema,
    /// which has no rotated state: rotation and device revocation both appear
    /// as `revoked_at_ms`, the earliest applicable time. The epoch is the generation.
    pub fn issuer_registry(&self) -> Result<IssuerRegistrySnapshot> {
        self.validate()?;
        let registry = IssuerRegistrySnapshot {
            version: ISSUER_REGISTRY_VERSION,
            repository: self.repository.clone(),
            epoch: self.generation,
            issuers: vec![IssuerRecord {
                issuer_id: self.device_id.clone(),
                trust_domain: self.device_id.clone(),
                common_control: CommonControl::Unknown {},
                allowed_check_kinds: self.allowed_check_kinds.clone(),
                keys: self
                    .keys
                    .iter()
                    .map(|key| IssuerKeyRecord {
                        key_id: key.key_id.clone(),
                        public_key_hex: key.public_key_hex.clone(),
                        not_before_ms: key.not_before_ms,
                        not_after_ms: key.not_after_ms,
                        revoked_at_ms: key.retired_at_ms(self.revoked_at_ms),
                    })
                    .collect(),
            }],
        };
        registry.validate()?;
        Ok(registry)
    }
}

fn device_id(value: &str) -> Result<()> {
    validate_identifier("device id", value)?;
    if value.len() <= DEVICE_ID_PREFIX.len() || !value.starts_with(DEVICE_ID_PREFIX) {
        return Err(Error::Invalid(format!(
            "device id must start with {DEVICE_ID_PREFIX}"
        )));
    }
    Ok(())
}

fn generation(value: u64) -> Result<()> {
    if value == 0 || value > i64::MAX as u64 {
        return Err(Error::Invalid(
            "authorization generation must be positive signed 64-bit".into(),
        ));
    }
    Ok(())
}

/// What an offline result asserts about itself. All fields are worker-asserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceClaim {
    pub repository: String,
    pub owner: OwnerIdentity,
    pub device_id: String,
    pub key_id: String,
    pub check_kind: String,
    pub issued_at_ms: u64,
    /// Authorization generation the worker observed.
    pub authorization_generation: u64,
}

/// Current standing of the owner, observed by the caller when accepting results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerStanding {
    Current,
    Disabled,
    RoleMissing,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceAssurance {
    /// An authorized device asserted the result. This is not proof of execution.
    TrustedDeviceWorkerAsserted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceRefusal {
    WrongRepository,
    WrongOwner,
    /// Also used when the claimed device id is not this device.
    UnknownKey,
    NotYetValid,
    Expired,
    Revoked,
    Rotated,
    FutureIssued,
    StaleAuthorization,
    OwnerDisabled,
    OwnerRoleMissing,
    CheckKindNotAllowed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceUnknown {
    ClockUncertain,
    OwnerRoleUnavailable,
    /// The stored authorization, repository or owner could not be read.
    AuthorizationUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceDecision {
    Current {
        assurance: DeviceAssurance,
        generation: u64,
    },
    Refused(DeviceRefusal),
    Unknown(DeviceUnknown),
}

/// Rechecks current authorization for a claim. A definite refusal wins over
/// `Unknown`, and `Current` is returned only when nothing is uncertain.
/// Revoked and rotated keys are refused whatever `issued_at_ms` says, since
/// that time is worker-asserted; there is no rotation grace period.
pub fn assess_device(
    authorization: &DeviceAuthorization,
    claim: &DeviceClaim,
    owner: OwnerStanding,
    clock: EvaluationClock,
) -> DeviceDecision {
    use DeviceRefusal::*;
    let refuse = DeviceDecision::Refused;
    if claim.repository != authorization.repository {
        return refuse(WrongRepository);
    }
    if claim.owner != authorization.owner {
        return refuse(WrongOwner);
    }
    if !authorization
        .allowed_check_kinds
        .contains(&claim.check_kind)
    {
        return refuse(CheckKindNotAllowed);
    }
    let key = authorization
        .keys
        .iter()
        .find(|key| key.key_id == claim.key_id);
    let Some(key) = key.filter(|_| claim.device_id == authorization.device_id) else {
        return refuse(UnknownKey);
    };
    if claim.authorization_generation != authorization.generation {
        return refuse(StaleAuthorization);
    }
    match owner {
        OwnerStanding::Disabled => return refuse(OwnerDisabled),
        OwnerStanding::RoleMissing => return refuse(OwnerRoleMissing),
        OwnerStanding::Current | OwnerStanding::Unavailable => {}
    }
    let now = match clock {
        EvaluationClock::Known { unix_ms } if unix_ms <= i64::MAX as u64 => Some(unix_ms),
        _ => None,
    };
    if let Some(now) = now {
        let reached = |time: Option<u64>| time.is_some_and(|time| time <= now);
        if reached(authorization.revoked_at_ms) || reached(key.revoked_at_ms) {
            return refuse(Revoked);
        }
        if reached(key.rotated_at_ms) {
            return refuse(Rotated);
        }
        if claim.issued_at_ms > now {
            return refuse(FutureIssued);
        }
        if claim.issued_at_ms < key.not_before_ms || now < key.not_before_ms {
            return refuse(NotYetValid);
        }
        if claim.issued_at_ms >= key.not_after_ms || now >= key.not_after_ms {
            return refuse(Expired);
        }
    }
    if now.is_none() {
        return DeviceDecision::Unknown(DeviceUnknown::ClockUncertain);
    }
    if owner == OwnerStanding::Unavailable {
        return DeviceDecision::Unknown(DeviceUnknown::OwnerRoleUnavailable);
    }
    DeviceDecision::Current {
        assurance: DeviceAssurance::TrustedDeviceWorkerAsserted,
        generation: authorization.generation,
    }
}

/// Signed by the candidate key to prove possession before it is enrolled.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentStatement {
    pub version: u32,
    pub repository: String,
    pub owner: OwnerIdentity,
    pub device_id: String,
    pub key_id: String,
    pub public_key_hex: String,
    /// Authorization generation this enrollment applies to.
    pub generation: u64,
    pub issued_at_ms: u64,
}

/// Values the enrolling administrator expects, from trusted state.
#[derive(Clone, Copy, Debug)]
pub struct EnrollmentExpectation<'a> {
    pub repository: &'a str,
    pub owner: &'a OwnerIdentity,
    pub device_id: &'a str,
    pub generation: u64,
}

impl EnrollmentStatement {
    pub fn validate(&self) -> Result<()> {
        version(self.version)?;
        validate_identifier("repository", &self.repository)?;
        self.owner.validate()?;
        device_id(&self.device_id)?;
        if self.key_id != issuer_key_id(&self.public_key_hex)? {
            return Err(Error::Invalid(
                "enrollment key ID does not match its public key".into(),
            ));
        }
        generation(self.generation)?;
        sql_time(self.issued_at_ms)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        bounded_bytes(self)
    }

    /// Bytes the device key signs; the domain is distinct from evidence signing.
    pub fn signing_message(&self) -> Result<Vec<u8>> {
        let mut message = ENROLLMENT_DOMAIN.to_vec();
        message.extend_from_slice(&self.canonical_bytes()?);
        Ok(message)
    }
}

/// Verifies that the statement names the expected repository, owner, device
/// and generation, and that the statement's own key signed it. This proves key
/// possession only; the caller still decides whether to enroll.
pub fn verify_enrollment(
    statement: &EnrollmentStatement,
    signature_hex: &str,
    expected: EnrollmentExpectation<'_>,
) -> Result<()> {
    let message = statement.signing_message()?;
    let mismatch = |what: &str| Err(Error::Invalid(format!("enrollment {what} mismatch")));
    if statement.repository != expected.repository {
        return mismatch("repository");
    }
    if &statement.owner != expected.owner {
        return mismatch("owner");
    }
    if statement.device_id != expected.device_id {
        return mismatch("device");
    }
    if statement.generation != expected.generation {
        return mismatch("generation");
    }
    let key_bytes = hex_bytes::<32>("device public key", &statement.public_key_hex)?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .ok()
        .filter(|key| is_canonical_ed25519_encoding(&key_bytes) && !key.is_weak())
        .ok_or_else(|| Error::Invalid("device public key is not a valid Ed25519 key".into()))?;
    let signature = Signature::from_bytes(&hex_bytes::<64>("enrollment signature", signature_hex)?);
    key.verify_strict(&message, &signature)
        .map_err(|_| Error::Invalid("enrollment signature is invalid".into()))
}

fn hex_bytes<const N: usize>(kind: &str, value: &str) -> Result<[u8; N]> {
    validate_lower_hex(kind, value, N)?;
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        // Validated lowercase hex, so both digits parse.
        *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("00"), 16).unwrap_or(0);
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyLocationRefusal {
    /// A path is relative or has `..`, so a lexical comparison cannot be trusted.
    NotNormalized,
    InsideSourceRoot,
    InsideDeclaredMount,
    ContainsDeclaredMount,
    NamedInEnvironment,
}

/// Lexically refuses a key path inside the checkout or run workspace
/// (`source_roots`), inside or containing a declared mount, or named in a
/// declared environment value. A mount that is a sibling or parent-adjacent
/// path is not a match; only a mount the key is under, or one under the key
/// path, is. Callers must canonicalize paths first; unnormalized input is refused.
///
/// This enforces only the declared mounts and environment. It does not defend
/// against host compromise, and other processes of the same user may still read
/// the key file; that limit is explicit.
pub fn key_location_refused(
    key_path: &Path,
    source_roots: &[&Path],
    declared_mounts: &[&Path],
    environment_values: &[&OsStr],
) -> Option<KeyLocationRefusal> {
    use KeyLocationRefusal::*;
    let normalized = |path: &Path| {
        path.is_absolute() && path.components().all(|part| part != Component::ParentDir)
    };
    if !normalized(key_path)
        || !source_roots
            .iter()
            .chain(declared_mounts)
            .all(|p| normalized(p))
    {
        return Some(NotNormalized);
    }
    if source_roots.iter().any(|root| key_path.starts_with(root)) {
        return Some(InsideSourceRoot);
    }
    if declared_mounts
        .iter()
        .any(|mount| key_path.starts_with(mount))
    {
        return Some(InsideDeclaredMount);
    }
    if declared_mounts
        .iter()
        .any(|mount| mount.starts_with(key_path))
    {
        return Some(ContainsDeclaredMount);
    }
    let needle = key_path.as_os_str().as_encoded_bytes();
    environment_values
        .iter()
        .any(|value| {
            value
                .as_encoded_bytes()
                .windows(needle.len())
                .any(|window| window == needle)
        })
        .then_some(NamedInEnvironment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::SIGNATURE_DOMAIN;
    use crate::policy::issuers::KeyTrustStatus;
    use ed25519_dalek::{Signer, SigningKey};

    const NOW: u64 = 5_000;

    fn signing(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn key(seed: u8) -> DeviceKey {
        let public_key_hex = hex(signing(seed).verifying_key().as_bytes());
        DeviceKey {
            key_id: issuer_key_id(&public_key_hex).unwrap(),
            public_key_hex,
            not_before_ms: 1_000,
            not_after_ms: 9_000,
            rotated_at_ms: None,
            revoked_at_ms: None,
        }
    }

    fn owner() -> OwnerIdentity {
        OwnerIdentity {
            provider: "oidc".into(),
            host: "idp.example".into(),
            person_id: "person:1".into(),
        }
    }

    fn auth() -> DeviceAuthorization {
        DeviceAuthorization {
            version: 1,
            device_id: "lcd_one".into(),
            repository: "github.com/acme/widget".into(),
            owner: owner(),
            generation: 3,
            revoked_at_ms: None,
            allowed_check_kinds: default_check_kinds(),
            keys: vec![key(1), key(2)],
        }
    }

    fn claim() -> DeviceClaim {
        DeviceClaim {
            repository: "github.com/acme/widget".into(),
            owner: owner(),
            device_id: "lcd_one".into(),
            key_id: key(1).key_id,
            check_kind: LOCAL_CI_CHECK_KIND.into(),
            issued_at_ms: 4_000,
            authorization_generation: 3,
        }
    }

    fn assess(a: &DeviceAuthorization, c: &DeviceClaim) -> DeviceDecision {
        assess_device(
            a,
            c,
            OwnerStanding::Current,
            EvaluationClock::Known { unix_ms: NOW },
        )
    }

    #[test]
    fn digest_ignores_key_order_and_matches_golden() {
        let a = auth();
        let mut b = a.clone();
        b.keys.reverse();
        assert_eq!(a.digest().unwrap(), b.digest().unwrap());
        assert_eq!(
            a.digest().unwrap(),
            "84a1c97818f8d36ed311983c0cd9c9b65640c19b3fc52105f642a78cdd469a43"
        );
    }

    #[test]
    fn decoding_is_strict() {
        let good = serde_json::to_vec(&auth()).unwrap();
        assert!(DeviceAuthorization::from_slice(&good).is_ok());
        let text = String::from_utf8(good.clone()).unwrap();
        let reject = |bytes: Vec<u8>| assert!(DeviceAuthorization::from_slice(&bytes).is_err());
        reject(text.replacen('{', "{\"extra\":1,", 1).into_bytes());
        reject(
            text.replacen("\"generation\":3,", "\"generation\":3,\"generation\":3,", 1)
                .into_bytes(),
        );
        reject(vec![b' '; super::super::MAX_DOCUMENT_BYTES + 1]);
        let mutate = |f: &dyn Fn(&mut DeviceAuthorization)| {
            let mut a = auth();
            f(&mut a);
            assert!(a.validate().is_err());
        };
        mutate(&|a| a.version = 2);
        mutate(&|a| a.generation = 0);
        mutate(&|a| a.device_id = "dev_one".into());
        mutate(&|a| a.device_id = "lcd_".into());
        mutate(&|a| a.repository = "a|b".into());
        mutate(&|a| a.repository = "r".repeat(129));
        mutate(&|a| a.owner.person_id = "a|b".into());
        mutate(&|a| a.keys = (1..=9).map(key).collect());
        mutate(&|a| a.keys.push(key(1)));
        mutate(&|a| a.keys[0].key_id = key(2).key_id);
        mutate(&|a| a.keys[0].not_after_ms = a.keys[0].not_before_ms);
        mutate(&|a| a.allowed_check_kinds = vec!["x".into(), "x".into()]);
        // Omitted check kinds default to the local CI kind.
        let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
        value.as_object_mut().unwrap().remove("allowed_check_kinds");
        let parsed = DeviceAuthorization::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.allowed_check_kinds, default_check_kinds());
    }

    #[test]
    fn rotation_supersedes_keys_and_advances_generation() {
        let rotated = auth().rotate(key(3), 4_500).unwrap();
        assert_eq!(rotated.generation, 4);
        assert_eq!(rotated.keys.len(), 3);
        assert!(
            rotated.keys[..2]
                .iter()
                .all(|k| k.rotated_at_ms == Some(4_500))
        );
        assert_eq!(rotated.keys[2], key(3));
        // Keys superseded earlier keep their original time.
        let again = rotated.rotate(key(4), 4_800).unwrap();
        assert_eq!(again.generation, 5);
        assert_eq!(again.keys[0].rotated_at_ms, Some(4_500));
        assert_eq!(again.keys[2].rotated_at_ms, Some(4_800));
        // The old key no longer assesses as current; the new one does.
        let mut old = claim();
        old.authorization_generation = 4;
        assert!(matches!(assess(&rotated, &old), DeviceDecision::Refused(_)));
        let mut fresh = claim();
        fresh.key_id = key(3).key_id;
        fresh.authorization_generation = 4;
        assert!(matches!(
            assess(&rotated, &fresh),
            DeviceDecision::Current { .. }
        ));
    }

    #[test]
    fn rotation_refusals() {
        let a = auth();
        assert!(a.rotate(key(1), 4_500).is_err());
        let mut same_public = key(3);
        same_public.key_id = key(1).key_id;
        assert!(a.rotate(same_public, 4_500).is_err());
        let mut retired = key(3);
        retired.rotated_at_ms = Some(1);
        assert!(a.rotate(retired, 4_500).is_err());
        assert!(a.revoke(4_500).unwrap().rotate(key(3), 4_600).is_err());
        assert!(a.rotate(key(3), u64::MAX).is_err());
        let mut top = a.clone();
        top.generation = i64::MAX as u64;
        assert!(top.rotate(key(3), 4_500).is_err());
        assert!(top.revoke(4_500).is_err());
    }

    #[test]
    fn rotation_at_the_key_bound_drops_the_oldest_superseded_key() {
        let mut a = auth();
        a.keys = (1..=8).map(key).collect();
        for (n, k) in a.keys.iter_mut().enumerate() {
            k.rotated_at_ms = Some(2_000 + [5, 3, 4, 9, 8, 7, 6, 10][n]);
        }
        let next = a.rotate(key(9), 4_000).unwrap();
        assert_eq!(next.keys.len(), 8);
        assert!(!next.keys.iter().any(|k| k.key_id == key(2).key_id));
        assert!(next.keys.iter().any(|k| k.key_id == key(9).key_id));
        // The dropped key is unknown, so a claim signed by it fails closed.
        let mut dropped = claim();
        dropped.key_id = key(2).key_id;
        dropped.authorization_generation = 4;
        assert_eq!(
            assess(&next, &dropped),
            DeviceDecision::Refused(DeviceRefusal::UnknownKey)
        );
        // All unrotated at the bound: the earliest start time then key ID decide.
        let mut b = auth();
        b.keys = (1..=8).map(key).collect();
        b.keys[3].not_before_ms = 500;
        let next = b.rotate(key(9), 4_000).unwrap();
        assert!(!next.keys.iter().any(|k| k.key_id == key(4).key_id));
        assert_eq!(next.keys.len(), 8);
    }

    #[test]
    fn revoke_advances_once_and_refuses_repeats() {
        let revoked = auth().revoke(4_500).unwrap();
        assert_eq!(
            (revoked.generation, revoked.revoked_at_ms),
            (4, Some(4_500))
        );
        assert!(revoked.revoke(4_600).is_err());
        let mut c = claim();
        c.authorization_generation = 4;
        assert!(matches!(assess(&revoked, &c), DeviceDecision::Refused(_)));
    }

    #[test]
    fn current_and_refusal_table() {
        use DeviceRefusal::*;
        assert_eq!(
            assess(&auth(), &claim()),
            DeviceDecision::Current {
                assurance: DeviceAssurance::TrustedDeviceWorkerAsserted,
                generation: 3
            }
        );
        type Case = (
            DeviceRefusal,
            fn(&mut DeviceAuthorization, &mut DeviceClaim),
        );
        let cases: [Case; 10] = [
            (WrongRepository, |_, c| {
                c.repository = "github.com/acme/other".into()
            }),
            (WrongOwner, |_, c| c.owner.person_id = "person:2".into()),
            (UnknownKey, |_, c| c.key_id = "0".repeat(64)),
            (UnknownKey, |_, c| c.device_id = "lcd_two".into()),
            (CheckKindNotAllowed, |_, c| c.check_kind = "other/v1".into()),
            (StaleAuthorization, |_, c| c.authorization_generation = 2),
            (StaleAuthorization, |a, _| a.generation = 4),
            (Revoked, |a, _| a.keys[0].revoked_at_ms = Some(NOW)),
            (Revoked, |a, _| a.revoked_at_ms = Some(NOW - 1)),
            (Rotated, |a, _| a.keys[0].rotated_at_ms = Some(NOW - 1)),
        ];
        for (expected, change) in cases {
            let (mut a, mut c) = (auth(), claim());
            change(&mut a, &mut c);
            assert_eq!(assess(&a, &c), DeviceDecision::Refused(expected));
        }
        let timed: [Case; 4] = [
            (FutureIssued, |_, c| c.issued_at_ms = NOW + 1),
            (NotYetValid, |a, _| a.keys[0].not_before_ms = NOW + 1),
            (NotYetValid, |_, c| c.issued_at_ms = 999),
            (Expired, |a, _| a.keys[0].not_after_ms = NOW),
        ];
        for (expected, change) in timed {
            let (mut a, mut c) = (auth(), claim());
            change(&mut a, &mut c);
            assert_eq!(assess(&a, &c), DeviceDecision::Refused(expected));
        }
        let (a, c) = (auth(), claim());
        let known = EvaluationClock::Known { unix_ms: NOW };
        assert_eq!(
            assess_device(&a, &c, OwnerStanding::Disabled, known),
            DeviceDecision::Refused(OwnerDisabled)
        );
        assert_eq!(
            assess_device(&a, &c, OwnerStanding::RoleMissing, known),
            DeviceDecision::Refused(OwnerRoleMissing)
        );
        assert_eq!(
            assess_device(&a, &c, OwnerStanding::Unavailable, known),
            DeviceDecision::Unknown(DeviceUnknown::OwnerRoleUnavailable)
        );
        assert_eq!(
            assess_device(&a, &c, OwnerStanding::Current, EvaluationClock::Uncertain),
            DeviceDecision::Unknown(DeviceUnknown::ClockUncertain)
        );
    }

    #[test]
    fn definite_refusal_beats_unknown() {
        let mut c = claim();
        c.repository = "github.com/acme/other".into();
        assert_eq!(
            assess_device(
                &auth(),
                &c,
                OwnerStanding::Unavailable,
                EvaluationClock::Uncertain
            ),
            DeviceDecision::Refused(DeviceRefusal::WrongRepository)
        );
        let mut a = auth();
        a.keys[0].revoked_at_ms = Some(10);
        assert_eq!(
            assess_device(
                &a,
                &claim(),
                OwnerStanding::Unavailable,
                EvaluationClock::Known { unix_ms: NOW }
            ),
            DeviceDecision::Refused(DeviceRefusal::Revoked)
        );
    }

    #[test]
    fn backdated_claims_on_retired_keys_are_refused() {
        for (field, expected) in [(0, DeviceRefusal::Revoked), (1, DeviceRefusal::Rotated)] {
            let mut a = auth();
            let k = &mut a.keys[0];
            if field == 0 {
                k.revoked_at_ms = Some(4_500);
            } else {
                k.rotated_at_ms = Some(4_500);
            }
            let mut c = claim();
            c.issued_at_ms = 1_500;
            assert_eq!(assess(&a, &c), DeviceDecision::Refused(expected));
        }
    }

    #[test]
    fn registry_projection_validates_and_reports_status() {
        let mut a = auth();
        a.keys[0].rotated_at_ms = Some(4_000);
        a.keys[1].revoked_at_ms = Some(4_500);
        a.keys.push(key(3));
        a.keys[2].not_after_ms = 4_000;
        let registry = a.issuer_registry().unwrap();
        registry.validate().unwrap();
        assert_eq!(registry.epoch, 3);
        let status = |i: usize| {
            registry
                .key_trust("lcd_one", &a.keys[i].key_id, 3_000, NOW)
                .unwrap()
                .status
        };
        assert_eq!(status(0), KeyTrustStatus::Revoked);
        assert_eq!(status(1), KeyTrustStatus::Revoked);
        assert_eq!(status(2), KeyTrustStatus::Expired);
        let mut healthy = auth();
        assert_eq!(
            healthy
                .issuer_registry()
                .unwrap()
                .key_trust("lcd_one", &healthy.keys[0].key_id, 3_000, NOW)
                .unwrap()
                .status,
            KeyTrustStatus::Trusted
        );
        healthy.revoked_at_ms = Some(2_000);
        let registry = healthy.issuer_registry().unwrap();
        assert_eq!(
            registry
                .key_trust("lcd_one", &healthy.keys[1].key_id, 1_500, NOW)
                .unwrap()
                .status,
            KeyTrustStatus::Revoked
        );
    }

    fn statement() -> EnrollmentStatement {
        let k = key(1);
        EnrollmentStatement {
            version: 1,
            repository: "github.com/acme/widget".into(),
            owner: owner(),
            device_id: "lcd_one".into(),
            key_id: k.key_id,
            public_key_hex: k.public_key_hex,
            generation: 3,
            issued_at_ms: NOW,
        }
    }

    fn sign(seed: u8, message: &[u8]) -> String {
        hex(&signing(seed).sign(message).to_bytes())
    }

    fn verify(s: &EnrollmentStatement, sig: &str) -> Result<()> {
        let o = owner();
        verify_enrollment(
            s,
            sig,
            EnrollmentExpectation {
                repository: "github.com/acme/widget",
                owner: &o,
                device_id: "lcd_one",
                generation: 3,
            },
        )
    }

    #[test]
    fn enrollment_requires_possession_and_expected_binding() {
        let s = statement();
        let message = s.signing_message().unwrap();
        let good = sign(1, &message);
        verify(&s, &good).unwrap();

        assert!(verify(&s, &sign(2, &message)).is_err(), "wrong signer");
        let mut tampered = s.clone();
        tampered.issued_at_ms += 1;
        assert!(verify(&tampered, &good).is_err());

        let o = owner();
        let expect = |repository, owner, device_id, generation| {
            verify_enrollment(
                &s,
                &good,
                EnrollmentExpectation {
                    repository,
                    owner,
                    device_id,
                    generation,
                },
            )
        };
        let other = OwnerIdentity {
            person_id: "person:2".into(),
            ..owner()
        };
        let r = "github.com/acme/widget";
        assert!(expect(r, &o, "lcd_one", 3).is_ok());
        assert!(expect("github.com/acme/other", &o, "lcd_one", 3).is_err());
        assert!(expect(r, &other, "lcd_one", 3).is_err());
        assert!(expect(r, &o, "lcd_two", 3).is_err());
        assert!(expect(r, &o, "lcd_one", 4).is_err());

        let canonical = s.canonical_bytes().unwrap();
        let mut evidence = SIGNATURE_DOMAIN.to_vec();
        evidence.extend_from_slice(&canonical);
        assert!(verify(&s, &sign(1, &evidence)).is_err());
        let mut prefixed = (SIGNATURE_DOMAIN.len() as u64).to_be_bytes().to_vec();
        prefixed.extend_from_slice(SIGNATURE_DOMAIN);
        prefixed.extend_from_slice(&(canonical.len() as u64).to_be_bytes());
        prefixed.extend_from_slice(&canonical);
        assert!(verify(&s, &sign(1, &prefixed)).is_err());
        assert!(verify(&s, "zz").is_err());
    }

    #[test]
    fn key_location_rules() {
        use KeyLocationRefusal::*;
        let key = Path::new("/home/u/keys/dev.key");
        let p = Path::new;
        let check = |roots: &[&Path], mounts: &[&Path], env: &[&OsStr]| {
            key_location_refused(key, roots, mounts, env)
        };
        assert_eq!(check(&[], &[], &[]), None);
        assert_eq!(
            check(&[p("/home/u/work")], &[p("/home/u/keys-other")], &[]),
            None
        );
        assert_eq!(check(&[p("/home/u")], &[], &[]), Some(InsideSourceRoot));
        assert_eq!(
            check(&[], &[p("/home/u/keys")], &[]),
            Some(InsideDeclaredMount)
        );
        assert_eq!(
            check(&[], &[p("/home/u/keys/dev.key")], &[]),
            Some(InsideDeclaredMount)
        );
        // A sibling mount under the same parent does not expose the key.
        assert_eq!(check(&[], &[p("/home/u/keys/other")], &[]), None);
        assert_eq!(
            key_location_refused(
                Path::new("/home/u/keys"),
                &[],
                &[p("/home/u/keys/other")],
                &[]
            ),
            Some(ContainsDeclaredMount)
        );
        assert_eq!(
            check(&[], &[], &[OsStr::new("--key=/home/u/keys/dev.key")]),
            Some(NamedInEnvironment)
        );
        assert_eq!(
            check(&[], &[], &[OsStr::new("/home/u/keys/dev.keys2x")]),
            Some(NamedInEnvironment)
        );
        assert_eq!(check(&[], &[], &[OsStr::new("/home/u/keys")]), None);
        assert_eq!(check(&[p("work")], &[], &[]), Some(NotNormalized));
        assert_eq!(
            check(&[p("/home/u/work/../keys")], &[], &[]),
            Some(NotNormalized)
        );
    }
}
