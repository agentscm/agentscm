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

/// Uninstall removes only the exact startup configuration owned by this client.
pub(crate) fn remove_login_setting() -> Result<()> {
    #[cfg(target_os = "macos")]
    login::set_enabled(false, &std::env::current_exe()?)?;
    Ok(())
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
}
