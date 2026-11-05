//! The optional `checks.json` manifest: several independent local-validation checks that share
//! one registry. Absent, the config directory holds exactly one legacy check. The manifest is
//! trusted configuration read from the verified default-branch checkout; identity comes only
//! from each check's policy, never from the manifest's names or directories.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use super::{ValidationPolicy, ValidationRecipe};
use crate::{Error, Result};

pub const MANIFEST_FILE: &str = "checks.json";
const MAX_CHECKS: usize = 20;
const MAX_NAME_CHARS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    checks: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    dir: String,
}

/// One check of the run, fully validated.
#[derive(Debug)]
pub struct LoadedCheck {
    /// Display name for status descriptions; `None` for the legacy single check.
    pub name: Option<String>,
    pub policy: ValidationPolicy,
    pub recipe: ValidationRecipe,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn check_dir(dir: &str) -> Result<PathBuf> {
    let path = Path::new(dir);
    let relative = !dir.is_empty()
        && (dir == "." || path.components().all(|c| matches!(c, Component::Normal(_))));
    if relative {
        Ok(path.to_path_buf())
    } else {
        Err(invalid(format!(
            "check directory must be \".\" or a relative path without \"..\": {dir:?}"
        )))
    }
}

fn check_policy_recipe(policy: &ValidationPolicy, recipe: &ValidationRecipe) -> Result<()> {
    if recipe.constituents.is_some() && policy.check_id.is_none() {
        return Err(invalid(
            "a recipe with constituents needs a policy with a check ID",
        ));
    }
    Ok(())
}

/// With a manifest, no path the load depends on may be a symbolic link: `registry.json`, each
/// component of a check directory under `root`, and its `policy.json` and `recipe.json`.
fn refuse_symlinks(root: &Path, dir: &Path) -> Result<()> {
    let refuse_link = |path: PathBuf| -> Result<()> {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(invalid(format!(
                "{} must not be a symbolic link",
                path.strip_prefix(root).unwrap_or(&path).display()
            ))),
            Ok(_) => Ok(()),
            // A missing file is reported by the read that follows.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    };
    if dir.as_os_str().is_empty() {
        return refuse_link(root.join("registry.json"));
    }
    let mut path = root.to_path_buf();
    for component in dir.components() {
        path.push(component);
        refuse_link(path.clone())?;
    }
    refuse_link(path.join("policy.json"))?;
    refuse_link(path.join("recipe.json"))
}

/// With a manifest in use, no component of `config_dir` below `repo` may be a symbolic link.
/// The components of `repo` itself are not examined: a checkout may legitimately sit under one.
pub fn refuse_config_dir_symlinks(repo: &Path, config_dir: &Path) -> Result<()> {
    let mut path = repo.to_path_buf();
    for component in config_dir.components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "--config-dir component {} must not be a symbolic link",
                    path.strip_prefix(repo).unwrap_or(&path).display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Loads every check under `root`. `read` returns a file's bytes, bounded; a missing manifest is
/// the only read failure that is not an error. Any problem fails the whole load.
pub fn load(root: &Path, read: &dyn Fn(&Path) -> Result<Vec<u8>>) -> Result<Vec<LoadedCheck>> {
    // A dangling `checks.json` link reads as absent, so it is refused before any read.
    match std::fs::symlink_metadata(root.join(MANIFEST_FILE)) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(invalid("checks.json must not be a symbolic link"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let manifest = match read(&root.join(MANIFEST_FILE)) {
        Ok(bytes) => Some(bytes),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let manifest_in_use = manifest.is_some();
    let load_one = |dir: &Path| -> Result<(ValidationPolicy, ValidationRecipe)> {
        if manifest_in_use {
            refuse_symlinks(root, dir)?;
        }
        let policy = ValidationPolicy::from_slice(&read(&root.join(dir).join("policy.json"))?)?;
        let recipe = ValidationRecipe::from_slice(&read(&root.join(dir).join("recipe.json"))?)?;
        check_policy_recipe(&policy, &recipe)?;
        Ok((policy, recipe))
    };
    let Some(bytes) = manifest else {
        let (policy, recipe) = load_one(Path::new(""))?;
        return Ok(vec![LoadedCheck {
            name: None,
            policy,
            recipe,
        }]);
    };
    refuse_symlinks(root, Path::new(""))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.schema_version != 1 {
        return Err(invalid("checks.json schema_version must be 1"));
    }
    if manifest.checks.is_empty() || manifest.checks.len() > MAX_CHECKS {
        return Err(invalid(format!(
            "checks.json must list 1 to {MAX_CHECKS} checks"
        )));
    }
    let mut seen = HashSet::new();
    let mut names = HashSet::new();
    let mut checks = Vec::new();
    for entry in manifest.checks {
        let length = entry.name.chars().count();
        if length == 0 || length > MAX_NAME_CHARS || entry.name.chars().any(char::is_control) {
            return Err(invalid(format!(
                "check name must be 1 to {MAX_NAME_CHARS} characters without control characters"
            )));
        }
        let dir = check_dir(&entry.dir)?;
        let (policy, recipe) = load_one(&dir)?;
        if policy.recipe_sha256 != recipe.sha256()? {
            return Err(invalid(format!(
                "check {:?}: policy recipe digest does not match its recipe",
                entry.name
            )));
        }
        // A second legacy policy has the same (legacy) context as the first.
        if !seen.insert(policy.status_context()) || !names.insert(entry.name.clone()) {
            return Err(invalid(format!(
                "check {:?}: duplicate check identity or name",
                entry.name
            )));
        }
        checks.push(LoadedCheck {
            name: Some(entry.name),
            policy,
            recipe,
        });
    }
    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_validation::{RecipeCommand, Services};
    use std::fs;

    fn recipe(constituents: bool) -> ValidationRecipe {
        ValidationRecipe {
            recipe_id: "r".into(),
            commands: vec![RecipeCommand {
                id: "gate".into(),
                argv: vec!["true".into()],
                timeout_secs: 30,
                paths: None,
            }],
            dependency_files: None,
            toolchain_probes: None,
            constituents: constituents.then(|| {
                vec![crate::local_validation::Constituent {
                    id: "g".into(),
                    name: "G".into(),
                    commands: vec!["gate".into()],
                }]
            }),
        }
    }

    fn write(root: &Path, dir: &str, id: Option<&str>, constituents: bool) {
        let recipe = recipe(constituents);
        let policy = ValidationPolicy {
            repository: "github.com/o/r".into(),
            recipe_sha256: recipe.sha256().unwrap(),
            required_services: Services {
                postgres: false,
                s3: false,
            },
            max_age_seconds: 60,
            epoch: 1,
            exempt_paths: None,
            check_id: id.map(str::to_owned),
        };
        let dir = root.join(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("policy.json"),
            serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("recipe.json"),
            serde_json::to_vec(&recipe).unwrap(),
        )
        .unwrap();
    }

    fn load_dir(root: &Path) -> Result<Vec<LoadedCheck>> {
        load(root, &|p| Ok(fs::read(p)?))
    }

    fn manifest(root: &Path, text: &str) {
        fs::write(root.join(MANIFEST_FILE), text).unwrap();
    }

    const TWO: &str = r#"{"schema_version":1,"checks":[{"name":"Unit","dir":"."},{"name":"Static","dir":"static"}]}"#;

    #[cfg(unix)]
    #[test]
    fn manifest_refuses_symbolic_links() {
        use std::os::unix::fs::symlink;
        let fresh = || {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".", Some("unit"), false);
            write(dir.path(), "static", Some("static"), true);
            fs::write(dir.path().join("registry.json"), "{}").unwrap();
            manifest(dir.path(), TWO);
            dir
        };
        let refused = |dir: &tempfile::TempDir, what: &str| {
            let error = load_dir(dir.path()).unwrap_err().to_string();
            assert!(error.contains("symbolic link"), "{what}: {error}");
        };
        assert!(load_dir(fresh().path()).is_ok());

        let dir = fresh();
        fs::remove_file(dir.path().join(MANIFEST_FILE)).unwrap();
        symlink("missing.json", dir.path().join(MANIFEST_FILE)).unwrap();
        refused(&dir, "dangling checks.json");

        let dir = fresh();
        fs::rename(dir.path().join(MANIFEST_FILE), dir.path().join("real.json")).unwrap();
        symlink("real.json", dir.path().join(MANIFEST_FILE)).unwrap();
        refused(&dir, "checks.json");

        let dir = fresh();
        fs::rename(dir.path().join("static"), dir.path().join("elsewhere")).unwrap();
        symlink("elsewhere", dir.path().join("static")).unwrap();
        refused(&dir, "check directory");

        for file in ["policy.json", "recipe.json"] {
            let dir = fresh();
            let path = dir.path().join("static").join(file);
            fs::rename(&path, dir.path().join("static").join("real")).unwrap();
            symlink("real", &path).unwrap();
            refused(&dir, file);
        }

        let dir = fresh();
        let path = dir.path().join("registry.json");
        fs::rename(&path, dir.path().join("real-registry")).unwrap();
        symlink("real-registry", &path).unwrap();
        refused(&dir, "registry.json");
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_components_below_the_repo_are_not_symbolic_links() {
        use std::os::unix::fs::symlink;
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join("real/lv")).unwrap();
        assert!(refuse_config_dir_symlinks(repo.path(), Path::new("real/lv")).is_ok());
        symlink("real", repo.path().join("linked")).unwrap();
        let error = refuse_config_dir_symlinks(repo.path(), Path::new("linked/lv"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic link"), "{error}");
        // The repository path itself may be reached through a link.
        let outer = tempfile::tempdir().unwrap();
        symlink(repo.path(), outer.path().join("repo-link")).unwrap();
        assert!(
            refuse_config_dir_symlinks(&outer.path().join("repo-link"), Path::new("real/lv"))
                .is_ok()
        );
    }

    #[test]
    fn absent_manifest_is_one_legacy_check() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".", None, false);
        let checks = load_dir(dir.path()).unwrap();
        assert_eq!(checks.len(), 1);
        assert!(checks[0].name.is_none());
    }

    #[test]
    fn manifest_loads_checks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".", Some("unit"), false);
        write(dir.path(), "static", Some("static"), true);
        manifest(dir.path(), TWO);
        let checks = load_dir(dir.path()).unwrap();
        let names: Vec<_> = checks.iter().map(|c| c.name.as_deref()).collect();
        assert_eq!(names, [Some("Unit"), Some("Static")]);
    }

    #[test]
    fn invalid_manifests_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".", Some("unit"), false);
        write(dir.path(), "static", Some("static"), false);
        write(dir.path(), "dup", Some("unit"), false);
        write(dir.path(), "legacy1", None, false);
        write(dir.path(), "legacy2", None, false);
        write(dir.path(), "bare", None, true);
        let check = |name: &str, dir: &str| format!(r#"{{"name":"{name}","dir":"{dir}"}}"#);
        let wrap = |checks: &[String]| {
            format!(r#"{{"schema_version":1,"checks":[{}]}}"#, checks.join(","))
        };
        let too_many: Vec<String> = (0..21).map(|i| check(&format!("c{i}"), "static")).collect();
        let cases = [
            ("duplicate id", wrap(&[check("A", "."), check("B", "dup")])),
            (
                "two legacy",
                wrap(&[check("A", "legacy1"), check("B", "legacy2")]),
            ),
            ("traversal", wrap(&[check("A", "../x")])),
            ("absolute", wrap(&[check("A", "/etc")])),
            ("empty dir", wrap(&[check("A", "")])),
            ("bad name", wrap(&[check("", "."), check("B", "static")])),
            ("empty list", wrap(&[])),
            ("too many", wrap(&too_many)),
            ("constituents without id", wrap(&[check("A", "bare")])),
            ("missing dir", wrap(&[check("A", "nope")])),
            (
                "unknown field",
                r#"{"schema_version":1,"extra":1,"checks":[{"name":"A","dir":"."}]}"#.into(),
            ),
            (
                "unknown entry field",
                r#"{"schema_version":1,"checks":[{"name":"A","dir":".","id":"x"}]}"#.into(),
            ),
            (
                "schema",
                r#"{"schema_version":2,"checks":[{"name":"A","dir":"."}]}"#.into(),
            ),
            ("malformed", "{".into()),
        ];
        for (what, text) in cases {
            manifest(dir.path(), &text);
            assert!(load_dir(dir.path()).is_err(), "{what}");
        }
    }

    #[test]
    fn recipe_digest_mismatch_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".", Some("unit"), false);
        write(dir.path(), "static", Some("static"), false);
        let mut other = recipe(false);
        other.recipe_id = "changed".into();
        fs::write(
            dir.path().join("static/recipe.json"),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();
        manifest(dir.path(), TWO);
        assert!(load_dir(dir.path()).is_err());
    }
}
