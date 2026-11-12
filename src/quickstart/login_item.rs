//! The Start at Login entry of the removed menu bar app. Nothing writes it any more; this only
//! removes one an earlier version wrote, and never one that was changed or is someone else's.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agentscm::{Error, Result};

const LABEL: &str = "com.counterbranch.agentscm.tray";
const LIMIT: u64 = 16 * 1024;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn paths() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?;
    let home = PathBuf::from(home);
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => home.join(".config"),
    };
    if !home.is_absolute()
        || !config.is_absolute()
        || config.components().any(|part| {
            matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(invalid(
            "the AgentSCM config directory must be an absolute normalized path",
        ));
    }
    Ok((
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
        home.join(".agentscm/bin/agentscm"),
        config,
    ))
}

/// Removes the entry when it is exactly what AgentSCM wrote; `Ok` when there is none.
/// The caller holds the shared client lifecycle lease.
pub(crate) fn remove() -> Result<()> {
    let (plist, stable, config) = paths()?;
    remove_at(&plist, &stable, &config)
}

fn remove_at(plist: &Path, stable: &Path, config: &Path) -> Result<()> {
    let expected = plist_body(stable, config);
    if let Some((identity, contents)) = read_owned(plist)? {
        if contents != expected {
            return Err(invalid(
                "the AgentSCM LaunchAgent was modified; it was not removed",
            ));
        }
        remove_owned_at(plist, identity, &expected)?;
    }
    Ok(())
}

fn read_owned(path: &Path) -> Result<Option<((u64, u64), String)>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(invalid(
                "the AgentSCM LaunchAgent path is not an owned regular file",
            ));
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > LIMIT
    {
        return Err(invalid(
            "the AgentSCM LaunchAgent path is not an owned private regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("the AgentSCM LaunchAgent is too large"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| invalid("the AgentSCM LaunchAgent is not valid UTF-8"))?;
    Ok(Some(((metadata.dev(), metadata.ino()), text)))
}

fn revalidate(path: &Path, identity: (u64, u64), expected: &str) -> Result<()> {
    match read_owned(path)? {
        Some((current, contents)) if current == identity && contents == expected => Ok(()),
        _ => Err(invalid(
            "the AgentSCM LaunchAgent changed concurrently; retry",
        )),
    }
}

/// Move first, then inspect the moved entry. A substituted foreign file is never unlinked.
fn remove_owned_at(path: &Path, identity: (u64, u64), expected: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("bad LaunchAgent path"))?;
    let nonce = getrandom::u64().map_err(|_| invalid("cannot prepare startup cleanup"))?;
    let quarantine = parent.join(format!(".{LABEL}-remove-{nonce:016x}"));
    std::fs::DirBuilder::new().mode(0o700).create(&quarantine)?;
    let moved = quarantine.join("startup.plist");
    if let Err(error) = rename_exclusive(path, &moved) {
        let _ = std::fs::remove_dir(&quarantine);
        return Err(error);
    }
    if revalidate(&moved, identity, expected).is_err() {
        if rename_exclusive(&moved, path).is_ok() {
            let _ = std::fs::remove_dir(&quarantine);
            return Err(invalid(
                "the AgentSCM LaunchAgent changed concurrently; its replacement was preserved",
            ));
        }
        return Err(invalid(format!(
            "the AgentSCM LaunchAgent changed concurrently; the original path was not overwritten and its moved replacement is preserved at {}",
            moved.display()
        )));
    }
    // This private, fresh directory is owned only by this operation. Deleting here cannot
    // unlink a replacement published at the original pathname.
    std::fs::remove_file(&moved)?;
    std::fs::remove_dir(quarantine)?;
    Ok(())
}

fn rename_exclusive(from: &Path, to: &Path) -> Result<()> {
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| invalid("startup path contains a null byte"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| invalid("startup path contains a null byte"))?;
    // SAFETY: both C strings are live and NUL-terminated; RENAME_EXCL atomically refuses to
    // overwrite the destination. No retained pointers or memory ownership cross this call.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// The exact file earlier versions wrote. An entry is removed only when it matches this.
fn plist_body(executable: &Path, config: &Path) -> String {
    let executable = xml_escape(&executable.to_string_lossy());
    let config = xml_escape(&config.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{executable}</string><string>--no-telemetry</string><string>tray</string><string>--foreground</string></array>\n<key>EnvironmentVariables</key><dict><key>XDG_CONFIG_HOME</key><string>{config}</string></dict>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n"
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn the_expected_entry_is_the_one_earlier_versions_wrote() {
        let body = plist_body(
            Path::new("/Users/A&B/.agentscm/bin/agentscm"),
            Path::new("/Users/A&B/config"),
        );
        assert!(body.contains("/Users/A&amp;B/.agentscm/bin/agentscm"));
        assert!(body.contains(
            "<string>--no-telemetry</string><string>tray</string><string>--foreground</string>"
        ));
        assert!(body.contains("<key>XDG_CONFIG_HOME</key><string>/Users/A&amp;B/config</string>"));
    }

    #[test]
    fn removal_takes_our_entry_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join("bin/agentscm");
        let config = temp.path().join("config");
        let plist = temp.path().join("agentscm.plist");
        std::fs::write(&plist, plist_body(&stable, &config)).unwrap();
        remove_at(&plist, &stable, &config).unwrap();
        assert!(!plist.exists());
        remove_at(&plist, &stable, &config).unwrap();
        // Nothing else is left behind in the folder.
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn foreign_changed_and_symlink_entries_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join("stable");
        let config = temp.path().join("config");
        let plist = temp.path().join("foreign.plist");
        std::fs::write(&plist, b"foreign").unwrap();
        assert!(remove_at(&plist, &stable, &config).is_err());
        assert_eq!(std::fs::read(&plist).unwrap(), b"foreign");

        std::fs::remove_file(&plist).unwrap();
        symlink(temp.path().join("missing"), &plist).unwrap();
        assert!(remove_at(&plist, &stable, &config).is_err());
        assert!(
            std::fs::symlink_metadata(&plist)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        std::fs::remove_file(&plist).unwrap();
        std::fs::create_dir(&plist).unwrap();
        assert!(remove_at(&plist, &stable, &config).is_err());
        assert!(plist.is_dir());
    }

    #[test]
    fn removal_preserves_a_replacement_published_after_validation() {
        let temp = tempfile::tempdir().unwrap();
        let plist = temp.path().join("startup.plist");
        std::fs::write(&plist, b"owned").unwrap();
        let (identity, expected) = read_owned(&plist).unwrap().unwrap();
        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, b"foreign").unwrap();
        std::fs::rename(&replacement, &plist).unwrap();
        assert!(remove_owned_at(&plist, identity, &expected).is_err());
        assert_eq!(std::fs::read(&plist).unwrap(), b"foreign");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn exclusive_restore_does_not_overwrite_another_file() {
        let temp = tempfile::tempdir().unwrap();
        let saved = temp.path().join("saved");
        let destination = temp.path().join("destination");
        std::fs::write(&saved, b"preserved").unwrap();
        std::fs::write(&destination, b"new").unwrap();
        assert!(rename_exclusive(&saved, &destination).is_err());
        assert_eq!(std::fs::read(&saved).unwrap(), b"preserved");
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    }
}
