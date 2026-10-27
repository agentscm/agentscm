//! `~/Applications/AgentSCM.app`: a minimal bundle that opens the menu bar app from the
//! installed link, so Spotlight, Launchpad and the Dock can reopen it after a restart. It is
//! written locally (so it carries no quarantine attribute) and removed only while it is
//! exactly what this module writes.
use std::path::{Path, PathBuf};

const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>AgentSCM</string>
	<key>CFBundleIdentifier</key>
	<string>com.counterbranch.agentscm.launcher</string>
	<key>CFBundleName</key>
	<string>AgentSCM</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>LSUIElement</key>
	<true/>
</dict>
</plist>
"#;

pub(crate) fn app(home: &Path) -> PathBuf {
    home.join("Applications").join("AgentSCM.app")
}

fn files(app: &Path, program: &Path) -> [(PathBuf, String); 2] {
    [
        (app.join("Contents/Info.plist"), PLIST.to_owned()),
        (
            app.join("Contents/MacOS/AgentSCM"),
            format!(
                "#!/bin/sh\nexec {} --no-telemetry tray\n",
                super::binary::quote(program)
            ),
        ),
    ]
}

fn ours(app: &Path, program: &Path) -> bool {
    !app.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
        && files(app, program)
            .iter()
            .all(|(path, text)| std::fs::read_to_string(path).is_ok_and(|found| found == *text))
}

/// Writes the bundle; `Ok(false)` when it was already there. Someone else's app of that name is
/// left alone and reported.
pub(crate) fn ensure(app: &Path, program: &Path) -> std::io::Result<bool> {
    if ours(app, program) {
        return Ok(false);
    }
    if app.symlink_metadata().is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and was not made by AgentSCM", app.display()),
        ));
    }
    for (path, text) in files(app, program) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = app.join("Contents/MacOS/AgentSCM");
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(true)
}

/// Removes the bundle only while it is exactly ours; `Ok(false)` when there was nothing to remove.
pub(crate) fn remove(app: &Path, program: &Path) -> std::io::Result<bool> {
    if !ours(app, program) {
        return Ok(false);
    }
    for (path, _) in files(app, program) {
        std::fs::remove_file(path)?;
    }
    // Only the directories this module made, and only once empty.
    for dir in ["Contents/MacOS", "Contents", ""] {
        std::fs::remove_dir(app.join(dir))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_is_written_once_and_removed_only_while_it_is_ours() {
        let home = tempfile::tempdir().unwrap();
        let app = app(home.path());
        let program = home.path().join(".agentscm/bin/agentscm");
        assert!(ensure(&app, &program).unwrap());
        assert!(!ensure(&app, &program).unwrap());
        let script = std::fs::read_to_string(app.join("Contents/MacOS/AgentSCM")).unwrap();
        assert!(
            script.contains("--no-telemetry tray") && script.contains(".agentscm/bin/agentscm")
        );
        assert!(remove(&app, &program).unwrap());
        assert!(!app.exists());
        assert!(home.path().join("Applications").exists());

        // An app the person changed, or someone else's, stays.
        ensure(&app, &program).unwrap();
        std::fs::write(app.join("Contents/MacOS/AgentSCM"), "#!/bin/sh\n").unwrap();
        assert!(!remove(&app, &program).unwrap());
        assert!(ensure(&app, &program).is_err());
        assert!(app.join("Contents/Info.plist").exists());
    }
}
