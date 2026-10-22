//! Discovers `src/hub/migrations/named/*.sqlite.sql` and the matching
//! `*.pg.sql` pair. Adding a file is enough; the integer migration list stays
//! untouched. A `-- requires: <name>` comment records a dependency.
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = generate() {
        eprintln!("hub named migrations: {error}");
        std::process::exit(1);
    }
}

fn generate() -> Result<(), String> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let directory = manifest.join("src/hub/migrations/named");
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut sqlite = Vec::new();
    let mut postgres = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
        if let Some(stem) = name.strip_suffix(".sqlite.sql") {
            sqlite.push((stem.to_owned(), path));
        } else if let Some(stem) = name.strip_suffix(".pg.sql") {
            postgres.push((stem.to_owned(), path));
        } else {
            return Err(format!(
                "{} is not <name>.sqlite.sql or <name>.pg.sql",
                path.display()
            ));
        }
    }
    sqlite.sort();
    postgres.sort();
    if sqlite.iter().map(|(name, _)| name).collect::<Vec<_>>()
        != postgres.iter().map(|(name, _)| name).collect::<Vec<_>>()
    {
        return Err("each named migration needs both a .sqlite.sql and a .pg.sql file".into());
    }
    let mut migrations = Vec::new();
    for ((name, sqlite_path), (_, postgres_path)) in sqlite.iter().zip(&postgres) {
        validate_name(name)?;
        let sqlite_sql = fs::read_to_string(sqlite_path).map_err(|e| e.to_string())?;
        let postgres_sql = fs::read_to_string(postgres_path).map_err(|e| e.to_string())?;
        if sqlite_sql.trim().is_empty() || postgres_sql.trim().is_empty() {
            return Err(format!("{name} is empty"));
        }
        let sqlite_requires = requirements(&sqlite_sql)?;
        let postgres_requires = requirements(&postgres_sql)?;
        if sqlite_requires != postgres_requires {
            return Err(format!("{name} sqlite and postgres requirements differ"));
        }
        if sqlite_requires
            .iter()
            .any(|requirement| requirement == name)
        {
            return Err(format!("{name} requires itself"));
        }
        migrations.push((name.clone(), sqlite_requires, sqlite_path, postgres_path));
    }
    let known: Vec<&str> = migrations.iter().map(|(name, ..)| name.as_str()).collect();
    for (name, requires, _, _) in &migrations {
        for requirement in requires {
            if !known.contains(&requirement.as_str()) {
                return Err(format!("{name} requires unknown migration {requirement}"));
            }
        }
    }
    ensure_acyclic(&migrations)?;
    let mut source = String::from("pub(super) static NAMED_MIGRATIONS: &[NamedMigration] = &[\n");
    for (name, requires, sqlite_path, postgres_path) in &migrations {
        source.push_str("    NamedMigration {\n");
        source.push_str(&format!("        name: \"{name}\",\n"));
        source.push_str("        requires: &[");
        for requirement in requires {
            source.push_str(&format!("\"{requirement}\", "));
        }
        source.push_str("],\n");
        source.push_str(&format!(
            "        sqlite: include_str!(\"{}\"),\n",
            include_path(sqlite_path)?
        ));
        source.push_str(&format!(
            "        postgres: include_str!(\"{}\"),\n",
            include_path(postgres_path)?
        ));
        source.push_str("    },\n");
    }
    source.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?)
        .join("hub_named_migrations.rs");
    fs::write(out, source).map_err(|e| e.to_string())
}

fn validate_name(name: &str) -> Result<(), String> {
    let ok = (1..64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && name.as_bytes()[0].is_ascii_lowercase();
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name} must be 1-63 bytes of [a-z0-9_] starting with a letter"
        ))
    }
}

fn requirements(sql: &str) -> Result<Vec<String>, String> {
    let mut requirements = Vec::new();
    for (index, line) in sql.lines().enumerate() {
        let Some(comment) = line.trim().strip_prefix("--") else {
            continue;
        };
        let Some(name) = comment.trim().strip_prefix("requires:") else {
            continue;
        };
        let name = name.trim();
        validate_name(name).map_err(|error| format!("line {}: {error}", index + 1))?;
        if requirements
            .iter()
            .any(|existing: &String| existing == name)
        {
            return Err(format!("{} is required twice", name));
        }
        requirements.push(name.to_owned());
    }
    Ok(requirements)
}

fn ensure_acyclic(migrations: &[(String, Vec<String>, &PathBuf, &PathBuf)]) -> Result<(), String> {
    let mut pending: Vec<&str> = migrations.iter().map(|(name, ..)| name.as_str()).collect();
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let ready = pending.iter().position(|name| {
            let requires = &migrations
                .iter()
                .find(|(candidate, ..)| candidate == name)
                .unwrap()
                .1;
            requires
                .iter()
                .all(|requirement| ordered.iter().any(|done| done == requirement))
        });
        let Some(index) = ready else {
            return Err("named migrations have a cycle".into());
        };
        ordered.push(pending.remove(index));
    }
    Ok(())
}

fn include_path(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not utf-8", path.display()))?
        .replace('\\', "/");
    if text.contains('"') {
        return Err(format!("{} contains a quote", path.display()));
    }
    Ok(text)
}
