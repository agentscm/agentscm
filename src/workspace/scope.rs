use std::collections::BTreeSet;

use crate::{Error, Result};

const MAX_SCOPE_ROOTS: usize = 256;
const MAX_SCOPE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Scope {
    roots: Vec<String>,
}

impl Scope {
    pub(super) fn from_requested(paths: &[String]) -> Result<Option<Self>> {
        if paths.is_empty() {
            return Err(Error::Invalid(
                "scoped workspace requires at least one path root".into(),
            ));
        }
        if paths.iter().any(|path| path == ".") {
            if paths.len() != 1 {
                return Err(Error::Invalid(
                    "the whole-tree root '.' cannot be combined with scoped roots".into(),
                ));
            }
            return Ok(None);
        }
        Self::from_effective(paths.to_vec()).map(Some)
    }

    pub(super) fn from_effective(roots: Vec<String>) -> Result<Self> {
        if roots.is_empty() {
            return Err(Error::Invalid("workspace scope has no path roots".into()));
        }
        if roots.len() > MAX_SCOPE_ROOTS {
            return Err(Error::Limit(format!(
                "workspace scope has more than {MAX_SCOPE_ROOTS} roots"
            )));
        }
        let total_bytes = roots.iter().try_fold(0usize, |total, root| {
            total
                .checked_add(root.len())
                .ok_or_else(|| Error::Limit("workspace scope byte count overflowed".into()))
        })?;
        if total_bytes > MAX_SCOPE_BYTES {
            return Err(Error::Limit(format!(
                "workspace scope exceeds {MAX_SCOPE_BYTES} path bytes"
            )));
        }
        let mut unique = BTreeSet::new();
        for root in roots {
            super::validate_workspace_path(&root)?;
            unique.insert(root);
        }
        let roots: Vec<String> = unique.into_iter().collect();
        for (index, left) in roots.iter().enumerate() {
            let left_folded = left.to_ascii_lowercase();
            for right in roots.iter().skip(index + 1) {
                let right_folded = right.to_ascii_lowercase();
                if contains_folded(&left_folded, &right_folded)
                    || contains_folded(&right_folded, &left_folded)
                {
                    if contains(left, right) || contains(right, left) {
                        continue;
                    }
                    return Err(Error::Conflict(format!(
                        "workspace scope roots collide on portable filesystems: {left} and {right}"
                    )));
                }
            }
        }
        let mut canonical: Vec<String> = Vec::new();
        for root in roots {
            if canonical.iter().any(|parent| contains(parent, &root)) {
                continue;
            }
            canonical.push(root);
        }
        Ok(Self { roots: canonical })
    }

    pub(super) fn roots(&self) -> &[String] {
        &self.roots
    }

    pub(super) fn includes(&self, path: &str) -> bool {
        self.roots.iter().any(|root| contains(root, path))
    }

    pub(super) fn traverses(&self, path: &str) -> bool {
        self.includes(path) || self.roots.iter().any(|root| contains(path, root))
    }

    pub(super) fn expanded(&self, additions: &[String]) -> Result<Option<Self>> {
        if additions.is_empty() {
            return Err(Error::Invalid("scope expansion has no path roots".into()));
        }
        if additions.iter().any(|path| path == ".") {
            if additions.len() != 1 {
                return Err(Error::Invalid(
                    "the whole-tree root '.' cannot be combined with scoped roots".into(),
                ));
            }
            return Ok(None);
        }
        let mut roots = self.roots.clone();
        roots.extend_from_slice(additions);
        Self::from_effective(roots).map(Some)
    }

    pub(super) fn with_selection_dependencies<'a>(
        &self,
        base_paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self> {
        let available: BTreeSet<&str> = base_paths.into_iter().collect();
        let mut roots = self.roots.clone();
        for candidate in [".gitignore", ".agentscmignore"] {
            if available.contains(candidate) {
                roots.push(candidate.to_owned());
            }
        }
        for root in &self.roots {
            let mut prefix = root.as_str();
            while let Some((parent, _)) = prefix.rsplit_once('/') {
                let candidate = format!("{parent}/.gitignore");
                if available.contains(candidate.as_str()) {
                    roots.push(candidate);
                }
                prefix = parent;
            }
        }
        Self::from_effective(roots)
    }
}

fn contains(root: &str, path: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn contains_folded(root: &str, path: &str) -> bool {
    contains(root, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_match_exact_paths_and_descendants_only() {
        let scope = Scope::from_effective(vec!["src/a".into()]).unwrap();
        assert!(scope.includes("src/a"));
        assert!(scope.includes("src/a/file"));
        assert!(!scope.includes("src/ab"));
        assert!(scope.traverses("src"));
        assert!(!scope.traverses("other"));
    }

    #[test]
    fn canonical_roots_collapse_descendants() {
        let scope = Scope::from_effective(vec!["src/a/file".into(), "src/a".into()]).unwrap();
        assert_eq!(scope.roots(), &["src/a"]);
    }
}
