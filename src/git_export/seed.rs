use std::path::Path;

use serde::{Deserialize, Serialize};

use super::git::{
    Git, MAX_OBJECT_BYTES, MAX_SEED_BYTES, MAX_SEED_OBJECTS, ObjectDescriptor, validate_oid,
};
use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SeedMetadata {
    pub object_format: String,
    pub commit: String,
    pub tree: String,
}

pub(super) struct QualifiedSeed {
    pub metadata: SeedMetadata,
    source: Git,
    objects: Vec<ObjectDescriptor>,
}

impl QualifiedSeed {
    pub(super) fn qualify(executable: &Path, repository: &Path, commit: &str) -> Result<Self> {
        validate_oid(commit)?;
        let repository = repository.canonicalize()?;
        let source = Git::new(executable, repository)?;
        let object_format = source.object_format()?;
        if object_format != "sha1" {
            return Err(Error::Invalid(
                "seed repository must use SHA-1 objects".into(),
            ));
        }
        source.reject_incomplete_history_metadata()?;
        if source.object_type(commit)? != "commit" {
            return Err(Error::Invalid("seed OID must name a commit object".into()));
        }
        let tree = source.tree(commit)?;
        let objects = source.closure(commit, MAX_SEED_OBJECTS)?;
        let mut total = 0u64;
        for object in &objects {
            if object.size > MAX_OBJECT_BYTES {
                return Err(Error::Limit(format!(
                    "seed object exceeds {MAX_OBJECT_BYTES} bytes"
                )));
            }
            total = total
                .checked_add(object.size)
                .ok_or_else(|| Error::Limit("seed closure size overflow".into()))?;
            if total > MAX_SEED_BYTES {
                return Err(Error::Limit(format!(
                    "seed closure exceeds {MAX_SEED_BYTES} raw bytes"
                )));
            }
        }
        Ok(Self {
            metadata: SeedMetadata {
                object_format,
                commit: commit.into(),
                tree,
            },
            source,
            objects,
        })
    }

    pub(super) fn copy_to(&self, destination: &Git) -> Result<()> {
        for object in &self.objects {
            let bytes = self.source.read_object(object)?;
            let copied = destination.write_object(&object.kind, &bytes)?;
            if copied != object.oid {
                return Err(Error::Conflict(
                    "copied seed object identity differs from source".into(),
                ));
            }
        }
        destination.verify_closure(
            &self.metadata.commit,
            Some(&self.metadata.tree),
            MAX_SEED_OBJECTS,
        )
    }
}
