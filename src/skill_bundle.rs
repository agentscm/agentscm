//! Bounded, immutable packaging for untrusted skill resources.
//!
//! This module preserves resource bytes and identifies the resulting package. It does not parse
//! `SKILL.md`, load paths from a filesystem, authenticate a publisher, or grant permissions. A
//! controller must independently bind a bundle to its native repository, revision, source
//! profile, and allowed operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::{Error, Result};

const DIGEST_DOMAIN: &[u8] = b"agentscm.skill-bundle.v1\0";
pub const MAX_SKILL_RESOURCES: usize = 64;
pub const MAX_SKILL_RESOURCE_BYTES: usize = 1024 * 1024;
pub const MAX_SKILL_RESOURCE_PATH_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillResource {
    path: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillBundle {
    resources: BTreeMap<String, Vec<u8>>,
    identity: SkillBundleIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillBundleIdentity(String);

impl SkillResource {
    pub fn new(path: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            bytes: bytes.into(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl SkillBundle {
    pub fn new(resources: Vec<SkillResource>) -> Result<Self> {
        if resources.len() > MAX_SKILL_RESOURCES {
            return Err(Error::Limit(format!(
                "skill bundle exceeds {MAX_SKILL_RESOURCES} resources"
            )));
        }

        let mut total_bytes = 0usize;
        let mut canonical = BTreeMap::new();
        for resource in resources {
            validate_path(&resource.path)?;
            total_bytes = total_bytes
                .checked_add(resource.path.len())
                .and_then(|total| total.checked_add(resource.bytes.len()))
                .ok_or_else(|| Error::Limit("skill bundle byte count overflow".into()))?;
            if total_bytes > MAX_SKILL_RESOURCE_BYTES {
                return Err(Error::Limit(format!(
                    "skill bundle exceeds {MAX_SKILL_RESOURCE_BYTES} resource bytes"
                )));
            }
            if canonical.insert(resource.path, resource.bytes).is_some() {
                return Err(Error::Invalid(
                    "skill bundle contains a duplicate resource path".into(),
                ));
            }
        }
        if !canonical.contains_key("SKILL.md") {
            return Err(Error::Invalid(
                "skill bundle is missing the SKILL.md entrypoint".into(),
            ));
        }
        validate_layout(&canonical)?;

        let identity = digest(&canonical);
        Ok(Self {
            resources: canonical,
            identity,
        })
    }

    pub fn identity(&self) -> &SkillBundleIdentity {
        &self.identity
    }

    pub fn skill_bytes(&self) -> &[u8] {
        self.resources
            .get("SKILL.md")
            .expect("validated skill bundle has an entrypoint")
    }

    pub fn resource(&self, path: &str) -> Option<&[u8]> {
        self.resources.get(path).map(Vec::as_slice)
    }

    pub fn resources(&self) -> impl ExactSizeIterator<Item = (&str, &[u8])> {
        self.resources
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
    }
}

impl SkillBundleIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillBundleIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SkillBundleIdentity {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(Error::Invalid(
                "skill bundle identity must use sha256".into(),
            ));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::Invalid(
                "skill bundle identity must be a lowercase SHA-256 digest".into(),
            ));
        }
        Ok(Self(value.into()))
    }
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::Invalid("skill resource path is empty".into()));
    }
    if path.len() > MAX_SKILL_RESOURCE_PATH_BYTES {
        return Err(Error::Limit(format!(
            "skill resource path exceeds {MAX_SKILL_RESOURCE_PATH_BYTES} bytes"
        )));
    }
    if crate::store::validate_path(path).is_err() {
        return Err(Error::Invalid(
            "skill resource path must be canonical portable ASCII".into(),
        ));
    }
    Ok(())
}

fn validate_layout(resources: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let mut spellings = BTreeMap::<String, String>::new();
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for path in resources.keys() {
        files.insert(path.to_ascii_lowercase());
        for end in path
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(std::iter::once(path.len()))
        {
            let prefix = &path[..end];
            let portable = prefix.to_ascii_lowercase();
            if let Some(existing) = spellings.insert(portable, prefix.into())
                && existing != prefix
            {
                return Err(Error::Invalid(
                    "skill resource paths have ambiguous case spellings".into(),
                ));
            }
            if end != path.len() {
                directories.insert(prefix.to_ascii_lowercase());
            }
        }
    }
    if files.iter().any(|path| directories.contains(path)) {
        return Err(Error::Invalid(
            "skill resource path is both a file and a directory".into(),
        ));
    }
    Ok(())
}

fn digest(resources: &BTreeMap<String, Vec<u8>>) -> SkillBundleIdentity {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    add_length(&mut hasher, resources.len());
    for (path, bytes) in resources {
        add_length(&mut hasher, path.len());
        hasher.update(path.as_bytes());
        add_length(&mut hasher, bytes.len());
        hasher.update(bytes);
    }
    SkillBundleIdentity(format!("sha256:{:x}", hasher.finalize()))
}

fn add_length(hasher: &mut Sha256, length: usize) {
    hasher.update(u64::try_from(length).unwrap_or(u64::MAX).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(resources: &[(&str, &[u8])]) -> Result<SkillBundle> {
        SkillBundle::new(
            resources
                .iter()
                .map(|(path, bytes)| SkillResource::new(*path, *bytes))
                .collect(),
        )
    }

    #[test]
    fn preserves_bytes_and_canonicalizes_resource_order() {
        let first = bundle(&[("references/z.bin", &[0, 255]), ("SKILL.md", b"skill")]).unwrap();
        let second = bundle(&[("SKILL.md", b"skill"), ("references/z.bin", &[0, 255])]).unwrap();

        assert_eq!(first.identity(), second.identity());
        assert_eq!(first.skill_bytes(), b"skill");
        assert_eq!(
            first.resource("references/z.bin"),
            Some([0, 255].as_slice())
        );
        assert_eq!(
            first.resources().map(|(path, _)| path).collect::<Vec<_>>(),
            vec!["SKILL.md", "references/z.bin"]
        );
    }

    #[test]
    fn any_path_or_content_change_changes_identity() {
        let original = bundle(&[("SKILL.md", b"skill"), ("a", b"bytes")]).unwrap();
        let changed_bytes = bundle(&[("SKILL.md", b"skill"), ("a", b"byteS")]).unwrap();
        let changed_path = bundle(&[("SKILL.md", b"skill"), ("b", b"bytes")]).unwrap();

        assert_ne!(original.identity(), changed_bytes.identity());
        assert_ne!(original.identity(), changed_path.identity());
        assert_eq!(
            original.identity().as_str(),
            "sha256:891f7fb11b5fca436fc4f0d003bc5ace8aede2f7c489fe174ad89350aad55f5b"
        );
        assert_eq!(
            original
                .identity()
                .as_str()
                .parse::<SkillBundleIdentity>()
                .unwrap(),
            original.identity().clone()
        );
    }

    #[test]
    fn rejects_missing_duplicate_or_noncanonical_paths() {
        assert!(bundle(&[("resource", b"x")]).is_err());
        assert!(bundle(&[("SKILL.md", b"a"), ("SKILL.md", b"b")]).is_err());
        for path in [
            "", "/x", "./x", "a/../x", "a//x", "a/", "a\\b", "C:/x", "x\0y", "cönfig", "con", "a. ",
        ] {
            assert!(
                bundle(&[("SKILL.md", b"skill"), (path, b"x")]).is_err(),
                "accepted {path:?}"
            );
        }
        assert!(bundle(&[("SKILL.md", b"skill"), ("A/x", b"x"), ("a/y", b"y")]).is_err());
        assert!(bundle(&[("SKILL.md", b"skill"), ("a", b"x"), ("a/y", b"y")]).is_err());
    }

    #[test]
    fn enforces_count_and_aggregate_byte_limits() {
        let mut too_many = vec![SkillResource::new("SKILL.md", Vec::new())];
        for index in 0..MAX_SKILL_RESOURCES {
            too_many.push(SkillResource::new(format!("r/{index}"), Vec::new()));
        }
        assert!(matches!(SkillBundle::new(too_many), Err(Error::Limit(_))));
        assert!(matches!(
            bundle(&[("SKILL.md", &vec![0; MAX_SKILL_RESOURCE_BYTES])]),
            Err(Error::Limit(_))
        ));
    }

    #[test]
    fn identity_accepts_only_supported_canonical_form() {
        let valid = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            valid.parse::<SkillBundleIdentity>().unwrap().as_str(),
            valid
        );
        for invalid in [
            "a",
            "sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "sha256:abc",
        ] {
            assert!(invalid.parse::<SkillBundleIdentity>().is_err());
        }
    }
}
