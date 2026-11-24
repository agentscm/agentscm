use std::path::Path;
use std::process::Command;

use agentscm::execution::executable::{self, ExecutableIdentity};
use semver::Version;

pub(crate) fn pinned_git(excluded_roots: &[&Path]) -> Option<ExecutableIdentity> {
    let candidates = [
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/usr/bin/git",
    ];
    let git = candidates.iter().find_map(|candidate| {
        let identity = executable::capture(Path::new(candidate), excluded_roots).ok()?;
        let output = Command::new(&identity.path)
            .env_clear()
            .env("LC_ALL", "C")
            .arg("--version")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let report = std::str::from_utf8(&output.stdout).ok()?;
        let version = report
            .strip_prefix("git version ")?
            .split_whitespace()
            .next()?;
        let version = Version::parse(version).ok()?;
        (version >= Version::new(2, 45, 0) && version.pre.is_empty() && version.build.is_empty())
            .then_some(identity)
    });
    if git.is_none() {
        eprintln!(
            "skipping pipeline fixture: none of {candidates:?} is Git 2.45.0 or newer and can be pinned"
        );
    }
    git
}
