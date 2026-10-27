//! `~/Applications/AgentSCM.app`: a minimal bundle that opens the menu bar app from the
//! installed link, so Spotlight, Launchpad and the Dock can reopen it after a restart. It is
//! written locally (so it carries no quarantine attribute). The bundle id in its Info.plist marks
//! it as ours, so any later version can update or remove it; an app without it is left alone.
use std::path::{Path, PathBuf};

const BUNDLE_ID: &str = "<string>com.counterbranch.agentscm.launcher</string>";

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

fn ours(app: &Path) -> bool {
    !app.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
        && std::fs::read_to_string(app.join("Contents/Info.plist"))
            .is_ok_and(|plist| plist.contains(BUNDLE_ID))
}

/// Writes the bundle, or brings ours up to date; `Ok(false)` when it already matched. An app of
/// that name without our bundle id is left alone and reported.
pub(crate) fn ensure(app: &Path, program: &Path) -> std::io::Result<bool> {
    let files = files(app, program);
    if files
        .iter()
        .all(|(path, text)| std::fs::read_to_string(path).is_ok_and(|found| found == *text))
        && ours(app)
    {
        return Ok(false);
    }
    if app.symlink_metadata().is_ok() && !ours(app) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and was not made by AgentSCM", app.display()),
        ));
    }
    for (path, text) in files {
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

/// Removes our bundle, whatever version wrote it; `Ok(false)` when there was none.
pub(crate) fn remove(app: &Path) -> std::io::Result<bool> {
    if !ours(app) {
        return Ok(false);
    }
    for file in ["Contents/Info.plist", "Contents/MacOS/AgentSCM"] {
        match std::fs::remove_file(app.join(file)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    // Only directories left empty: anything the person added inside stays.
    for dir in ["Contents/MacOS", "Contents", ""] {
        let _ = std::fs::remove_dir(app.join(dir));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_is_written_once_updated_by_marker_and_never_takes_another_app() {
        let home = tempfile::tempdir().unwrap();
        let app = app(home.path());
        let program = home.path().join(".agentscm/bin/agentscm");
        assert!(ensure(&app, &program).unwrap());
        assert!(!ensure(&app, &program).unwrap());
        let script = app.join("Contents/MacOS/AgentSCM");
        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .contains("--no-telemetry tray")
        );

        // A launcher an older version wrote (different script) is still ours: updated, then removed.
        std::fs::write(&script, "#!/bin/sh\nexec /old/agentscm tray\n").unwrap();
        assert!(ensure(&app, &program).unwrap());
        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .contains(".agentscm/bin/agentscm")
        );
        std::fs::write(&script, "#!/bin/sh\nexec /old/agentscm tray\n").unwrap();
        assert!(remove(&app).unwrap());
        assert!(!app.exists());
        assert!(home.path().join("Applications").exists());

        // Something the person put inside ours stays, with the folders that hold it.
        ensure(&app, &program).unwrap();
        std::fs::write(app.join("Contents/notes.txt"), "mine").unwrap();
        assert!(remove(&app).unwrap());
        assert!(app.join("Contents/notes.txt").exists());
        std::fs::remove_dir_all(&app).unwrap();

        // Another app of that name is neither replaced nor removed.
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            "<plist>someone else</plist>",
        )
        .unwrap();
        assert!(ensure(&app, &program).is_err());
        assert!(!remove(&app).unwrap());
        assert!(app.join("Contents/Info.plist").exists());
    }
}
