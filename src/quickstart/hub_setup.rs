//! First-run GitHub App setup shared by native and Docker local Hub runtimes.
use std::time::{Duration, Instant};

use serde_json::Value;

use super::detect::{capture, which};
use crate::hub_cli::open_url;
use crate::prompt::Ui;

const QUICK: Duration = Duration::from_secs(20);

pub(crate) struct Started {
    pub(crate) install_url: Option<String>,
    pub(crate) pending: bool,
    pub(crate) url: String,
}

pub(crate) trait Runtime {
    fn url(&self) -> &str;
    fn connection_ids(&self) -> Result<Vec<String>, String>;
    fn read_secret(&self, name: &str) -> Result<String, String>;
    fn restart_fresh(&mut self) -> Result<(), String>;
    async fn configure_github(
        &mut self,
        connection: &str,
        client_id: &str,
        login: &str,
    ) -> Result<(), String>;
}

pub(crate) fn connection_ids(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.trim().rsplit('/').next()?.strip_suffix(".json"))
        .filter(|id| {
            id.starts_with("con_") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .map(str::to_owned)
        .collect()
}

fn login_ok(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn github_login(given: Option<&str>, ui: &Ui) -> Result<String, String> {
    if let Some(login) = given {
        return login_ok(login)
            .then(|| login.to_owned())
            .ok_or_else(|| "--github-login isn't a GitHub login.".into());
    }
    let verified = which("gh")
        .and_then(|gh| capture(&gh, &["api", "user", "-q", ".login"], None, QUICK))
        .filter(|login| login_ok(login));
    let login = if ui.yes {
        verified.unwrap_or_default()
    } else {
        let guess = verified
            .or_else(|| capture(&which("git")?, &["config", "github.user"], None, QUICK))
            .unwrap_or_default();
        ui.ask(
            "Your GitHub login (this account becomes the hub's administrator)",
            &guess,
        )
    };
    login_ok(&login).then_some(login).ok_or_else(|| {
        "I need your GitHub login to make you the hub's administrator; with --yes, pass --github-login LOGIN (or sign in to gh so `gh api user` works).".into()
    })
}

pub(crate) async fn finish(
    runtime: &mut impl Runtime,
    ui: &Ui,
    login: Option<&str>,
    preset: Option<String>,
    tty: bool,
    repo: Option<&str>,
) -> Result<Started, String> {
    let url = runtime.url().to_owned();
    let mut connection = runtime.connection_ids()?;
    if connection.is_empty() {
        println!("Preparing a fresh first-run link ...");
        runtime.restart_fresh()?;
        super::wait_healthy(&url, Duration::from_secs(60))
            .await
            .then_some(())
            .ok_or_else(|| format!("The hub at {url} didn't become healthy within 60 seconds."))?;
    }
    let link = runtime
        .read_secret("bootstrap-setup-url")
        .ok()
        .filter(|link| link.starts_with(&format!("{url}/")) && !link.chars().any(char::is_control));
    connection = runtime.connection_ids()?;
    if let Some(link) = link {
        println!(
            "\nFirst run: the hub needs a GitHub App. Open this link (it works once, for 10 minutes):\n  {link}"
        );
        println!(
            "Create the app on GitHub, then on the success page click Install and choose your repository."
        );
        if tty && !ui.yes {
            open_url(&link);
        }
        if ui.yes {
            println!(
                "Non-interactive setup will not wait for the browser. Finish creating and installing the app, then run quickstart again."
            );
            return Ok(Started {
                install_url: None,
                pending: true,
                url,
            });
        }
        let deadline = Instant::now() + Duration::from_secs(600);
        while connection.is_empty() {
            if Instant::now() >= deadline {
                return Err("GitHub App setup wasn't finished in 10 minutes. Run quickstart again to continue.".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            connection = runtime.connection_ids()?;
        }
        ui.pause(&format!(
            "On the GitHub page, click Install and pick {}, then press Enter.",
            repo.unwrap_or("your repository")
        ));
    }
    let Some(connection) = connection.first() else {
        return Ok(Started {
            install_url: None,
            pending: false,
            url,
        });
    };
    let app: Value = runtime
        .read_secret(&format!("{connection}.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let install_url = app["slug"]
        .as_str()
        .filter(|slug| {
            !slug.is_empty()
                && slug
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
        .map(|slug| format!("https://github.com/apps/{slug}/installations/new"));
    if let Some(client_id) = app["client_id"].as_str().filter(|value| !value.is_empty()) {
        let login = match preset {
            Some(login) => login,
            None => github_login(login, ui)?,
        };
        runtime
            .configure_github(connection, client_id, &login)
            .await?;
        super::wait_healthy(&url, Duration::from_secs(60))
            .await
            .then_some(())
            .ok_or_else(|| format!("The hub at {url} didn't become healthy within 60 seconds."))?;
    }
    Ok(Started {
        install_url,
        pending: false,
        url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_names_and_logins_are_bounded() {
        let listing = "/x/con_ab.json\n/x/con_ab.client_secret\n/x/con_x;y.json\n";
        assert_eq!(connection_ids(listing), ["con_ab"]);
        assert!(login_ok("octo-cat"));
        assert!(!login_ok("") && !login_ok("octo cat"));
    }
}
