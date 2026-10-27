//! Optional native status-bar client. The ordinary CLI remains the launcher and owner.

use agentscm::Result;
use clap::Args;

#[derive(Debug, Args)]
pub(crate) struct TrayArgs {
    /// Run the status-bar event loop in this process (used by the detached launcher).
    #[arg(long, hide = true)]
    foreground: bool,
}

pub(crate) async fn run(args: TrayArgs) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::run(args.foreground).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        Err(agentscm::Error::Invalid(
            "the AgentSCM tray is currently supported only on macOS".into(),
        ))
    }
}

/// Caller holds the shared client lifecycle lease through program cleanup.
pub(crate) fn remove_login_setting_locked() -> Result<()> {
    #[cfg(target_os = "macos")]
    login::set_enabled(false, &std::env::current_exe()?)?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn set_login_enabled(enable: bool, current: &std::path::Path) -> Result<()> {
    with_client_lifecycle_using(crate::client_cli::lifecycle_lock, || {
        login::set_enabled(enable, current)
    })
}

/// Whether Start at Login is already on, so quickstart need not offer it again.
#[cfg(target_os = "macos")]
pub(crate) fn login_enabled() -> bool {
    login::enabled()
}

#[cfg(target_os = "macos")]
fn with_client_lifecycle_using<T>(
    acquire: impl FnOnce() -> Result<std::fs::File>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lifecycle = acquire()?;
    operation()
}

#[cfg(target_os = "macos")]
mod login;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(any(target_os = "macos", test))]
fn clean_label(value: &str) -> String {
    let mut label = value
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>()
        .replace('&', "&&");
    if label.is_empty() {
        label.push_str("Unknown");
    }
    label
}

#[cfg(any(target_os = "macos", test))]
fn open_argv(path: &std::path::Path) -> Vec<std::ffi::OsString> {
    vec!["--".into(), path.as_os_str().to_owned()]
}

#[cfg(any(target_os = "macos", test))]
fn foreground_argv() -> [&'static str; 3] {
    ["--no-telemetry", "tray", "--foreground"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_escape_native_menu_mnemonics_and_controls() {
        assert_eq!(clean_label("A&B\nC"), "A&&BC");
    }

    #[test]
    fn open_arguments_do_not_invoke_a_shell() {
        let path = std::path::PathBuf::from("/tmp/a; touch owned");
        assert_eq!(open_argv(&path), vec!["--".into(), path.into_os_string()]);
    }

    #[test]
    fn detached_tray_disables_invocation_telemetry() {
        assert_eq!(
            foreground_argv(),
            ["--no-telemetry", "tray", "--foreground"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn login_change_waits_for_cleanup_and_rejects_a_deleted_program() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("client-lifecycle.lock");
        let cleanup = crate::client_config::acquire_lock(&lock).unwrap();
        let mut attempted = false;
        let blocked = with_client_lifecycle_using(
            || crate::client_config::acquire_lock(&lock),
            || {
                attempted = true;
                Ok(())
            },
        );
        assert!(blocked.is_err());
        assert!(!attempted);
        drop(cleanup);

        let current = temp.path().join("versioned-agentscm");
        std::fs::write(&current, b"program").unwrap();
        let stable = temp.path().join("bin/agentscm");
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&current, &stable).unwrap();
        let plist = temp.path().join("LaunchAgents/agentscm.plist");
        let config = temp.path().join("config");
        std::fs::remove_file(&current).unwrap();
        let rejected = with_client_lifecycle_using(
            || crate::client_config::acquire_lock(&lock),
            || login::set_enabled_at(true, &current, &plist, &stable, &config),
        );
        assert!(rejected.is_err());
        assert!(!plist.exists());
    }
}
