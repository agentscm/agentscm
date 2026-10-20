use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::execution::Cancellation;
use crate::execution::process::capture_with_input;
use crate::{Error, Result};

pub(super) struct Git {
    executable: PathBuf,
    destination: PathBuf,
    #[cfg(test)]
    fixture_launcher: bool,
}

pub(super) const MAX_SEED_OBJECTS: usize = 100_000;
pub(super) const MAX_CLOSURE_OBJECTS: usize = 200_001;
pub(super) const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_SEED_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct ObjectDescriptor {
    pub oid: String,
    pub kind: String,
    pub size: u64,
}

impl Git {
    pub(super) fn new(executable: &Path, destination: PathBuf) -> Result<Self> {
        if !executable.is_absolute() {
            return Err(Error::Invalid(
                "git executable path must be absolute".into(),
            ));
        }
        Ok(Self {
            executable: executable.to_path_buf(),
            destination,
            #[cfg(test)]
            fixture_launcher: false,
        })
    }

    #[cfg(test)]
    fn fixture_launcher(mut self) -> Self {
        self.fixture_launcher = true;
        self
    }

    pub(super) fn executable(&self) -> PathBuf {
        self.executable.clone()
    }

    pub(super) fn run(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        budget: u64,
    ) -> Result<Vec<u8>> {
        self.run_code(args, input, budget, false)
            .map(|(_, bytes)| bytes)
    }

    fn run_code(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        budget: u64,
        one_ok: bool,
    ) -> Result<(i32, Vec<u8>)> {
        self.run_code_with_timeout(args, input, budget, one_ok, Duration::from_secs(60))
    }

    fn run_code_with_timeout(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        budget: u64,
        one_ok: bool,
        timeout: Duration,
    ) -> Result<(i32, Vec<u8>)> {
        let mut command = self.command();
        command.args(args);
        let output = capture_with_input(
            &mut command,
            input,
            timeout,
            budget,
            &Cancellation::default(),
        )?;
        if output.stop.is_some() || !(output.code == Some(0) || (one_ok && output.code == Some(1)))
        {
            return Err(Error::Invalid(format!(
                "Git {} failed ({:?}, {:?}): {}",
                args.first().unwrap_or(&""),
                output.code,
                output.stop,
                String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(512)])
            )));
        }
        Ok((output.code.unwrap_or_default(), output.stdout))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .process_group(0)
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", "/nonexistent")
            .env("LC_ALL", "C");
        #[cfg(test)]
        if self.fixture_launcher {
            command.current_dir(&self.destination);
            return command;
        }
        command.arg("-C").arg(&self.destination).args([
            "--no-replace-objects",
            "--no-lazy-fetch",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "core.fsync=loose-object,reference",
            "-c",
            "core.fsyncMethod=fsync",
        ]);
        command
    }

    // Network peers can echo credentials in arbitrary diagnostics. Never return
    // their stderr or raw command text as a public error or journal field.
    pub(super) fn transport(
        &self,
        args: &[&str],
        config: &[(String, String)],
        environment: &[(String, String)],
        budget: u64,
    ) -> Result<Vec<u8>> {
        let mut command = self.command();
        for (key, value) in config {
            command.args(["-c", &format!("{key}={value}")]);
        }
        command
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .args(args);
        let output = capture_with_input(
            &mut command,
            None,
            Duration::from_secs(60),
            budget,
            &Cancellation::default(),
        )?;
        if output.stop.is_some() || output.code != Some(0) {
            return Err(Error::SourceIndeterminate);
        }
        Ok(output.stdout)
    }

    pub(super) fn text(&self, args: &[&str]) -> Result<String> {
        String::from_utf8(self.run(args, None, 8192)?)
            .map(|text| text.trim().to_owned())
            .map_err(|_| Error::Invalid("Git response is not UTF-8".into()))
    }

    pub(super) fn object_format(&self) -> Result<String> {
        self.text(&["rev-parse", "--show-object-format"])
    }

    pub(super) fn reject_incomplete_history_metadata(&self) -> Result<()> {
        for name in ["shallow", "info/grafts"] {
            let path = PathBuf::from(self.text(&["rev-parse", "--git-path", name])?);
            let path = if path.is_absolute() {
                path
            } else {
                self.destination.join(path)
            };
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(Error::Invalid(format!(
                    "seed repository has unsupported {name} history metadata"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn object_type(&self, oid: &str) -> Result<String> {
        validate_oid(oid)?;
        self.text(&["cat-file", "-t", oid])
    }

    pub(super) fn tree(&self, commit: &str) -> Result<String> {
        validate_oid(commit)?;
        let tree = self.text(&["rev-parse", "--verify", &format!("{commit}^{{tree}}")])?;
        validate_oid(&tree)?;
        Ok(tree)
    }

    pub(super) fn closure(
        &self,
        commit: &str,
        object_limit: usize,
    ) -> Result<Vec<ObjectDescriptor>> {
        validate_oid(commit)?;
        let objects = self.run(
            &[
                "rev-list",
                "--objects",
                "--missing=error",
                "--no-object-names",
                commit,
            ],
            None,
            8 * 1024 * 1024,
        )?;
        let text = std::str::from_utf8(&objects)
            .map_err(|_| Error::Invalid("invalid Git closure listing".into()))?;
        let count = validate_closure_listing(text, object_limit)?;
        let checked = self.run(
            &["cat-file", "--batch-check"],
            Some(objects),
            16 * 1024 * 1024,
        )?;
        let checked = std::str::from_utf8(&checked)
            .map_err(|_| Error::Invalid("invalid Git object check".into()))?;
        if checked.lines().count() != count {
            return Err(Error::Conflict("incomplete Git object check".into()));
        }
        let mut descriptors = Vec::with_capacity(count);
        for line in checked.lines() {
            let fields: Vec<_> = line.split(' ').collect();
            if fields.len() != 3 || !matches!(fields[1], "commit" | "tree" | "blob") {
                return Err(Error::Conflict(
                    "Git mirror has missing or unsupported reachable objects".into(),
                ));
            }
            validate_oid(fields[0])?;
            let size = fields[2]
                .parse::<u64>()
                .map_err(|_| Error::Invalid("invalid Git object size".into()))?;
            descriptors.push(ObjectDescriptor {
                oid: fields[0].into(),
                kind: fields[1].into(),
                size,
            });
        }
        Ok(descriptors)
    }

    pub(super) fn read_object(&self, object: &ObjectDescriptor) -> Result<Vec<u8>> {
        if object.size > MAX_OBJECT_BYTES {
            return Err(Error::Limit(format!(
                "Git object exceeds {MAX_OBJECT_BYTES} bytes"
            )));
        }
        let budget = object
            .size
            .checked_add(8192)
            .ok_or_else(|| Error::Limit("Git object output budget overflow".into()))?;
        let bytes = self.run(&["cat-file", &object.kind, &object.oid], None, budget)?;
        if bytes.len() as u64 != object.size {
            return Err(Error::Conflict(
                "Git object size changed while reading".into(),
            ));
        }
        Ok(bytes)
    }

    pub(super) fn current(&self, reference: &str) -> Result<Option<String>> {
        // Enumeration can omit dangling symbolic refs. Reject them explicitly
        // rather than confusing an occupied symbolic ref with an absent ref.
        if self
            .run_code(&["symbolic-ref", "--quiet", reference], None, 8192, true)?
            .0
            == 0
        {
            return Err(Error::Conflict("owned ref must not be symbolic".into()));
        }
        // for-each-ref returns success with empty output for an absent direct ref.
        let output = self.text(&[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
            reference,
        ])?;
        if output.is_empty() {
            return Ok(None);
        }
        let fields: Vec<_> = output.split(' ').collect();
        if fields.len() != 2 || fields[0] != reference {
            return Err(Error::Conflict("owned ref is symbolic or ambiguous".into()));
        }
        validate_oid(fields[1])?;
        Ok(Some(fields[1].into()))
    }

    /// Check that the retained commit/tree mapping and every reachable object
    /// still exist. This is bounded connectivity, not fresh native byte evidence.
    pub(super) fn verify_closure(
        &self,
        commit: &str,
        expected_tree: Option<&str>,
        object_limit: usize,
    ) -> Result<()> {
        validate_oid(commit)?;
        let actual_tree = self.tree(commit)?;
        if expected_tree.is_some_and(|tree| tree != actual_tree) {
            return Err(Error::Conflict(
                "retained commit/tree mapping differs from Git".into(),
            ));
        }
        let _ = self.closure(commit, object_limit)?;
        Ok(())
    }

    pub(super) fn write_object(&self, kind: &str, bytes: &[u8]) -> Result<String> {
        let result = self.run(
            &["hash-object", "--no-filters", "-t", kind, "-w", "--stdin"],
            Some(bytes.to_vec()),
            8192,
        )?;
        let oid =
            String::from_utf8(result).map_err(|_| Error::Invalid("invalid Git OID".into()))?;
        let oid = oid.trim().to_owned();
        validate_oid(&oid)?;
        Ok(oid)
    }
}

fn validate_closure_listing(text: &str, object_limit: usize) -> Result<usize> {
    let mut count = 0usize;
    for oid in text.lines() {
        count += 1;
        if count > object_limit {
            return Err(Error::Limit(format!(
                "Git closure exceeds {object_limit} objects"
            )));
        }
        validate_oid(oid)?;
    }
    if count == 0 {
        return Err(Error::Invalid("empty Git closure".into()));
    }
    Ok(count)
}

pub(super) fn validate_oid(oid: &str) -> Result<()> {
    if oid.len() != 40
        || !oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid(
            "expected full lowercase SHA-1 Git OID".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_closure_listing;

    fn descendants_stop_with_git(output_limit: bool) {
        use std::fs;
        use std::path::Path;
        use std::time::Duration;

        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("late-write");
        let script = root.path().join("git-fixture");
        let quote =
            |path: &std::path::Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        let tail = if output_limit {
            "while :; do printf '0123456789012345678901234567890123456789\\n'; done"
        } else {
            "/bin/sleep 2"
        };
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n(/bin/sleep 0.4; /usr/bin/touch {}) &\n{tail}\n",
                quote(&marker)
            ),
        )
        .unwrap();
        let git = super::Git::new(Path::new("/bin/sh"), root.path().to_path_buf())
            .unwrap()
            .fixture_launcher();
        let script = script.to_str().unwrap();
        let result = git.run_code_with_timeout(
            &[script],
            None,
            if output_limit { 64 } else { 8192 },
            false,
            if output_limit {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(100)
            },
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(if output_limit { "Output" } else { "Timeout" }),
            "{error}"
        );
        std::thread::sleep(Duration::from_millis(650));
        assert!(!marker.exists(), "Git descendant survived termination");
    }

    #[test]
    fn timeout_terminates_git_descendants_before_they_can_write() {
        descendants_stop_with_git(false);
    }

    #[test]
    fn output_limit_terminates_git_descendants_before_they_can_write() {
        descendants_stop_with_git(true);
    }

    #[test]
    fn closure_listing_honors_the_selected_inclusive_limit() {
        let oid = "0123456789012345678901234567890123456789";
        let listing = format!("{oid}\n{oid}\n{oid}\n");
        assert_eq!(validate_closure_listing(&listing, 3).unwrap(), 3);
        assert!(validate_closure_listing(&listing, 2).is_err());
        assert!(validate_closure_listing("", 3).is_err());
    }
}
