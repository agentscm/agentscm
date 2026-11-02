//! Bounded immutable Git object reads. No checkout, hooks, filters, network or candidate code runs.
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::execution::Cancellation;
use crate::execution::process::capture_with_input;
use crate::{Error, Result};

use super::{SourceIdentity, SourceReader, path};

/// The controller supplies the authorized checkout and repository identity. This reader does
/// not infer forge ownership from a mutable remote URL or grant trust to a configuration revision.
pub struct GitSourceReader {
    repository: String,
    checkout: PathBuf,
    private: tempfile::TempDir,
}

impl GitSourceReader {
    pub fn new(checkout: &Path, repository: String) -> Result<Self> {
        crate::evidence::validate_identifier("repository", &repository)?;
        let checkout = checkout.canonicalize()?;
        let private = tempfile::tempdir()?;
        Ok(Self {
            repository,
            checkout,
            private,
        })
    }

    fn run(&self, args: &[&str], limit: usize) -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.private.path())
            .env("XDG_CONFIG_HOME", self.private.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env("LC_ALL", "C")
            .args([
                "--no-replace-objects",
                "--no-optional-locks",
                "--literal-pathspecs",
                "-C",
            ])
            .arg(&self.checkout)
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .process_group(0);
        let output = capture_with_input(
            &mut command,
            None,
            Duration::from_secs(30),
            limit as u64,
            &Cancellation::default(),
        )?;
        if output.stop.is_some() || output.code != Some(0) || output.incomplete {
            return Err(Error::Invalid(
                "immutable Git object read failed or was incomplete".into(),
            ));
        }
        Ok(output.stdout)
    }

    fn subject<'a>(
        &self,
        source: &'a SourceIdentity,
    ) -> Result<&'a crate::local_validation::GitValidationSubject> {
        source.validate()?;
        let SourceIdentity::Git { subject } = source else {
            return Err(Error::Invalid(
                "Git reader cannot read a native source".into(),
            ));
        };
        if subject.repository != self.repository {
            return Err(Error::Invalid(
                "Git source belongs to another repository".into(),
            ));
        }
        Ok(subject)
    }
}

impl SourceReader for GitSourceReader {
    fn verify_source(&self, source: &SourceIdentity) -> Result<()> {
        let subject = self.subject(source)?;
        if self.run(&["cat-file", "-t", &subject.commit], 1024)? != b"commit\n" {
            return Err(Error::Invalid("Git source identity is not a commit".into()));
        }
        let commit = self.run(&["cat-file", "commit", &subject.commit], 64 * 1024)?;
        let expected = format!("tree {}\n", subject.tree);
        if !commit.starts_with(expected.as_bytes()) {
            return Err(Error::Invalid(
                "Git commit/tree binding does not match".into(),
            ));
        }
        // Check availability/type of the bound tree without relying on replacement refs.
        let kind = self.run(&["cat-file", "-t", &subject.tree], 1024)?;
        if kind != b"tree\n" {
            return Err(Error::Invalid("Git source tree is unavailable".into()));
        }
        Ok(())
    }

    fn read_file(&self, source: &SourceIdentity, file: &str, limit: usize) -> Result<Vec<u8>> {
        path("source file", file, false)?;
        if limit == 0 || limit > super::MAX_DOCUMENT_BYTES {
            return Err(Error::Limit("invalid source file byte limit".into()));
        }
        self.verify_source(source)?;
        let subject = self.subject(source)?;
        let entry = self.run(&["ls-tree", "-z", &subject.tree, "--", file], 4096)?;
        let entry = entry
            .strip_suffix(&[0])
            .ok_or_else(|| Error::Invalid("source file is absent".into()))?;
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| Error::Invalid("invalid Git tree entry".into()))?;
        let (metadata, actual_path) = (&entry[..tab], &entry[tab + 1..]);
        if actual_path != file.as_bytes() {
            return Err(Error::Invalid(
                "source path is not an exact regular file".into(),
            ));
        }
        let metadata = std::str::from_utf8(metadata)
            .map_err(|_| Error::Invalid("invalid Git tree metadata".into()))?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" {
            return Err(Error::Invalid(
                "source path is not a regular or executable file".into(),
            ));
        }
        crate::evidence::validate_lower_hex(
            "Git blob",
            fields[2],
            if fields[2].len() == 40 { 20 } else { 32 },
        )?;
        self.run(&["cat-file", "blob", fields[2]], limit)
    }
}
