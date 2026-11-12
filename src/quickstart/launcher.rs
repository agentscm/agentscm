//! `~/Applications/AgentSCM.app`: the bundle earlier versions wrote to open the removed menu bar
//! app. Nothing writes it any more; setup and uninstall remove it. The bundle id in its
//! Info.plist marks it as ours; an app without it is left alone.
use std::path::{Path, PathBuf};

const BUNDLE_ID: &str = "<string>com.counterbranch.agentscm.launcher</string>";

pub(crate) fn app(home: &Path) -> PathBuf {
    home.join("Applications").join("AgentSCM.app")
}

fn ours(app: &Path) -> bool {
    !app.symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
        && std::fs::read_to_string(app.join("Contents/Info.plist"))
            .is_ok_and(|plist| plist.contains(BUNDLE_ID))
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

    /// A bundle as an earlier version wrote it.
    fn write(app: &Path, info: &str) {
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), info).unwrap();
        std::fs::write(
            app.join("Contents/MacOS/AgentSCM"),
            "#!/bin/sh\nexec /old/agentscm tray\n",
        )
        .unwrap();
    }

    #[test]
    fn our_launcher_is_removed_and_another_app_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let app = app(home.path());
        let ours = format!("<plist>{BUNDLE_ID}</plist>");
        assert!(!remove(&app).unwrap());

        write(&app, &ours);
        assert!(remove(&app).unwrap());
        assert!(!app.exists());
        assert!(home.path().join("Applications").exists());

        // Something the person put inside ours stays, with the folders that hold it.
        write(&app, &ours);
        std::fs::write(app.join("Contents/notes.txt"), "mine").unwrap();
        assert!(remove(&app).unwrap());
        assert!(app.join("Contents/notes.txt").exists());
        std::fs::remove_dir_all(&app).unwrap();

        // Another app of that name is not removed.
        write(&app, "<plist>someone else</plist>");
        assert!(!remove(&app).unwrap());
        assert!(app.join("Contents/MacOS/AgentSCM").exists());
    }
}
