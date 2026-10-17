//! Which `agentscm` the hooks call. Hooks must keep working after quickstart exits, so a
//! program that lives in a cache or temp directory is copied to `~/.agentscm/bin` first,
//! in the layout the installer uses: `agentscm-<version>` plus an `agentscm` symlink.
use std::path::{Path, PathBuf};

// Where symlinks need privileges (Windows) this is a copy of the versioned file instead.
pub(crate) const LINK: &str = if cfg!(windows) {
    "agentscm.exe"
} else {
    "agentscm"
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Program {
    /// The running program is in the install directory: hooks use its symlink.
    InstallDir(PathBuf),
    /// `agentscm` on `PATH` is the running program (and not in a cache).
    Bare,
    /// Transient program: copy it to the install directory first.
    Copy,
}

pub(crate) fn bin_dir(home: &Path) -> PathBuf {
    home.join(".agentscm").join("bin")
}

pub(crate) fn is_cache_path(exe: &Path, temp: &Path) -> bool {
    let text = exe
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    ["/_npx/", "/.npm/", "/node_modules/.cache/"]
        .iter()
        .any(|marker| text.contains(marker))
        || exe.starts_with(temp)
}

/// `exe`, `on_path` and `temp` are canonical paths; `on_path` is what `agentscm` on `PATH` is.
pub(crate) fn plan(home: &Path, exe: &Path, on_path: Option<&Path>, temp: &Path) -> Program {
    let dir = bin_dir(home);
    if dir.canonicalize().is_ok_and(|dir| exe.starts_with(dir)) {
        return Program::InstallDir(dir.join(LINK));
    }
    if on_path == Some(exe) && !is_cache_path(exe, temp) {
        return Program::Bare;
    }
    Program::Copy
}

/// A path as it must appear in a shell command.
pub(crate) fn quote(path: &Path) -> String {
    // Forward slashes work in every Windows shell hooks run in; this is a no-op elsewhere.
    let text = path
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Whether two files have the same bytes, compared a block at a time.
fn same_contents(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (Ok(mut a), Ok(mut b)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let len = |f: &std::fs::File| f.metadata().ok().map(|m| m.len());
    if len(&a) != len(&b) {
        return false;
    }
    let (mut x, mut y) = (vec![0_u8; 1 << 16], vec![0_u8; 1 << 16]);
    loop {
        match a.read(&mut x) {
            Ok(0) => return true,
            Ok(n) => {
                if b.read_exact(&mut y[..n]).is_err() || x[..n] != y[..n] {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Copies `exe` to `<dir>/agentscm-<version>` (temp file then rename, 0755) and points
/// `<dir>/agentscm` at it. An existing `agentscm-<version>` is never overwritten: it may be
/// a verified release. It is used as it is when it is the running program or an identical
/// copy. Returns the symlink's path.
pub(crate) fn install(dir: &Path, exe: &Path, version: &str) -> Result<PathBuf, String> {
    let fail = |e: std::io::Error| e.to_string();
    super::private_dir(dir).map_err(fail)?;
    let link = dir.join(LINK);
    if link.symlink_metadata().is_ok_and(|m| {
        if cfg!(unix) {
            !m.file_type().is_symlink()
        } else {
            m.is_dir()
        }
    }) {
        return Err(format!(
            "{} exists and is not {}; I won't replace it",
            link.display(),
            if cfg!(unix) { "a symlink" } else { "a file" }
        ));
    }
    let name = format!("agentscm-{version}{}", std::env::consts::EXE_SUFFIX);
    let versioned = dir.join(&name);
    if versioned.symlink_metadata().is_ok() {
        let running = versioned.canonicalize().ok().as_deref() == Some(exe);
        if !running && !same_contents(&versioned, exe) {
            return Err(format!(
                "{} already exists with different contents (it may be a verified release); I won't overwrite it",
                versioned.display()
            ));
        }
    } else {
        let temp = dir.join(format!("{name}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        std::fs::copy(exe, &temp).map_err(fail)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))
                .map_err(fail)?;
        }
        std::fs::rename(&temp, &versioned).map_err(fail)?;
    }
    #[cfg(unix)]
    if std::fs::read_link(&link).ok().as_deref() != Some(Path::new(&name)) {
        let temp = dir.join(format!("{LINK}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        std::os::unix::fs::symlink(&name, &temp).map_err(fail)?;
        std::fs::rename(&temp, &link).map_err(fail)?;
    }
    #[cfg(not(unix))]
    if !same_contents(&link, &versioned) {
        let temp = dir.join(format!("{LINK}.tmp-{}", std::process::id()));
        let _ = std::fs::remove_file(&temp);
        std::fs::copy(&versioned, &temp).map_err(fail)?;
        std::fs::rename(&temp, &link).map_err(fail)?;
    }
    Ok(link)
}

/// The installer's files directly in `dir`: `agentscm` and `agentscm-*` (never directories).
pub(crate) fn installed(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name == LINK || name.starts_with("agentscm-"))
                && e.path()
                    .symlink_metadata()
                    .is_ok_and(|m| !m.file_type().is_dir())
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_program_the_hooks_call() {
        let home = tempfile::tempdir().unwrap();
        let dir = bin_dir(home.path());
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.canonicalize().unwrap().join("agentscm-1.2.3");
        let temp = Path::new("/private/tmp");
        assert_eq!(
            plan(home.path(), &exe, None, temp),
            Program::InstallDir(dir.join(LINK))
        );
        let elsewhere = Path::new("/opt/homebrew/bin/agentscm");
        assert_eq!(
            plan(home.path(), elsewhere, Some(elsewhere), temp),
            Program::Bare
        );
        // Under npx, `agentscm` on PATH is the cached copy: not stable.
        let npx = Path::new("/home/u/.npm/_npx/ab/node_modules/.bin/agentscm");
        assert_eq!(plan(home.path(), npx, Some(npx), temp), Program::Copy);
        let scratch = Path::new("/private/tmp/x/agentscm");
        assert_eq!(
            plan(home.path(), scratch, Some(scratch), temp),
            Program::Copy
        );
        assert_eq!(plan(home.path(), elsewhere, None, temp), Program::Copy);
        assert!(is_cache_path(Path::new("/a/node_modules/.cache/x"), temp));
    }

    #[cfg(unix)]
    #[test]
    fn installs_the_installer_layout_and_updates_it() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = bin_dir(home.path());
        let exe = home.path().join("built");
        std::fs::write(&exe, b"one").unwrap();
        let link = install(&dir, &exe, "9.9.9").unwrap();
        assert_eq!(link, dir.join("agentscm"));
        assert_eq!(std::fs::read(&link).unwrap(), b"one");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("agentscm-9.9.9")
        );
        let versioned = dir.join("agentscm-9.9.9");
        assert_eq!(
            versioned.metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(dir.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        // The same version with other bytes is never overwritten (it may be a release).
        std::fs::write(&exe, b"two").unwrap();
        let refused = install(&dir, &exe, "9.9.9").unwrap_err();
        assert!(refused.contains("won't overwrite"), "{refused}");
        assert_eq!(std::fs::read(&versioned).unwrap(), b"one");
        // Unless it is the running program itself, or an identical copy.
        install(&dir, &versioned.canonicalize().unwrap(), "9.9.9").unwrap();
        std::fs::write(&exe, b"one").unwrap();
        install(&dir, &exe, "9.9.9").unwrap();
        std::fs::write(&exe, b"two").unwrap();
        install(&dir, &exe, "10.0.0").unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("agentscm-10.0.0")
        );
        assert_eq!(installed(&dir).len(), 3);
        // A regular file named agentscm is never replaced.
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"mine").unwrap();
        assert!(install(&dir, &exe, "10.0.0").is_err());
        assert_eq!(std::fs::read(&link).unwrap(), b"mine");
    }

    #[cfg(windows)]
    #[test]
    fn installs_copies_where_symlinks_need_privileges() {
        let home = tempfile::tempdir().unwrap();
        let dir = bin_dir(home.path());
        let exe = home.path().join("built.exe");
        std::fs::write(&exe, b"one").unwrap();
        let link = install(&dir, &exe, "9.9.9").unwrap();
        assert_eq!(link, dir.join("agentscm.exe"));
        assert_eq!(std::fs::read(&link).unwrap(), b"one");
        assert_eq!(
            std::fs::read(dir.join("agentscm-9.9.9.exe")).unwrap(),
            b"one"
        );
        std::fs::write(&exe, b"two").unwrap();
        install(&dir, &exe, "10.0.0").unwrap();
        assert_eq!(std::fs::read(&link).unwrap(), b"two");
        assert_eq!(installed(&dir).len(), 3);
        // Another version's file is never overwritten.
        assert!(install(&dir, &exe, "9.9.9").is_err());
    }

    #[test]
    fn quotes_paths_with_spaces() {
        assert_eq!(quote(Path::new("/a b/agentscm")), "'/a b/agentscm'");
        assert_eq!(quote(Path::new("/opt/x/agentscm")), "/opt/x/agentscm");
    }
}
