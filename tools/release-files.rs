//! Synchronize release metadata from the root Cargo package without updating dependencies.
//! Compile with `rustc --edition=2024 tools/release-files.rs -o /tmp/release-files`.
use std::{env, fs, process};

fn version(manifest: &str) -> Result<String, String> {
    let mut package = false;
    let mut found = Vec::new();
    for line in manifest.lines() {
        if line.starts_with('[') {
            package = line == "[package]";
        }
        if package && let Some(value) = line.strip_prefix("version = \"") {
            found.push(value.strip_suffix('"').ok_or("invalid Cargo version")?);
        }
    }
    let [value] = found.as_slice() else {
        return Err("expected one root package version".into());
    };
    let components: Vec<_> = value.split('.').collect();
    if components.len() != 3
        || components.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|c| c.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
                || part.parse::<u64>().is_err()
        })
    {
        return Err("release version must be a plain major.minor.patch".into());
    }
    Ok((*value).into())
}

fn npm(input: &str, version: &str) -> Result<String, String> {
    let matches = input
        .lines()
        .filter(|l| l.starts_with("  \"version\": \""))
        .count();
    if matches != 1 {
        return Err("expected one npm version in the repository format".into());
    }
    let mut output = String::new();
    for line in input.split_inclusive('\n') {
        if line.starts_with("  \"version\": \"") {
            if !line.trim_end().ends_with("\",") {
                return Err("invalid npm version field".into());
            }
            output.push_str(&format!("  \"version\": \"{version}\",\n"));
        } else {
            output.push_str(line);
        }
    }
    Ok(output)
}

fn lock(input: &str, version: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut packages = 0;
    for block in input.split("[[package]]") {
        let target = block.lines().any(|l| l == "name = \"agentscm\"");
        if target {
            packages += 1;
            let mut versions = 0;
            for line in block.split_inclusive('\n') {
                if line.starts_with("version = \"") {
                    versions += 1;
                    if !line.trim_end().ends_with('"') {
                        return Err("invalid lockfile version".into());
                    }
                    output.push_str(&format!("version = \"{version}\"\n"));
                } else {
                    output.push_str(line);
                }
            }
            if versions != 1 {
                return Err("expected one agentscm version per lockfile entry".into());
            }
        } else {
            output.push_str(block);
        }
        output.push_str("[[package]]");
    }
    output.truncate(output.len() - "[[package]]".len());
    if packages != 1 {
        return Err("expected one agentscm package in lockfile".into());
    }
    Ok(output)
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args != ["--check"] && args != ["--sync"] {
        return Err("usage: release-files --check|--sync (from repository root)".into());
    }
    let read = |path| fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    let version = version(&read("Cargo.toml")?)?;
    // Validate every input before writing any output. A failed write remains an error.
    let mut updates = Vec::new();
    for path in [
        "Cargo.lock",
        "ui/dev-seed/mint/Cargo.lock",
        "packages/npm/package.json",
    ] {
        let old = read(path)?;
        let new = if path.ends_with(".json") {
            npm(&old, &version)?
        } else {
            lock(&old, &version)?
        };
        if old != new {
            updates.push((path, new));
        }
    }
    if args == ["--check"] && !updates.is_empty() {
        return Err("release files disagree with Cargo.toml; run release-files --sync".into());
    }
    for (path, data) in updates {
        fs::write(path, data).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_versions_only() {
        assert_eq!(
            version("[package]\nversion = \"2.3.4\"\n").unwrap(),
            "2.3.4"
        );
        for v in ["2.3.4-alpha.9", "2.3", "02.3.4", "2.3.4+build", "../x"] {
            assert!(version(&format!("[package]\nversion = \"{v}\"\n")).is_err());
        }
        assert!(version("[dependencies]\nversion = \"2.3.4\"\n").is_err());
    }
    #[test]
    fn lock_changes_only_our_package_and_is_idempotent() {
        let old = "version = 4\n\n[[package]]\nname = \"other\"\nversion = \"9.8.7\"\n\n[[package]]\nname = \"agentscm\"\nversion = \"1.0.0-alpha.9\"\ndependencies = [\"other\"]\n";
        let expected = old.replace("1.0.0-alpha.9", "2.3.4");
        assert_eq!(lock(old, "2.3.4").unwrap(), expected);
        assert_eq!(lock(&expected, "2.3.4").unwrap(), expected);
        assert!(lock("[[package]]\nname = \"other\"\n", "2.3.4").is_err());
        assert!(lock(&format!("{old}{old}"), "2.3.4").is_err());
    }
    #[test]
    fn npm_preserves_other_fields_and_refuses_ambiguous_input() {
        let old =
            "{\n  \"name\": \"agentscm\",\n  \"version\": \"1.0.0-alpha.9\",\n  \"bin\": {}\n}\n";
        let expected = old.replace("1.0.0-alpha.9", "2.3.4");
        assert_eq!(npm(old, "2.3.4").unwrap(), expected);
        assert_eq!(npm(&expected, "2.3.4").unwrap(), expected);
        assert!(npm("{}", "2.3.4").is_err());
        assert!(npm(&format!("{old}{old}"), "2.3.4").is_err());
    }
}
