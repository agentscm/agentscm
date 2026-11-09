//! The digest of stale unfinished Work and repeated collisions, and its delivery switches,
//! through the Hub API. The Hub computes the digest; this only prints it.
use std::time::Duration;

use agentscm::Result;
use agentscm::hub::digest::{DEFAULT_DAYS, WorkDigestV1, render_text};
use agentscm::hub::digest_http::DigestSettings;
use clap::Subcommand;
use reqwest::Method;
use serde_json::json;

use crate::intent_cli::Hub;

const LIMIT: Duration = Duration::from_secs(10);
const DIGEST: &str = "/v1/hub/standup/digest";
const SETTINGS: &str = "/v1/hub/standup/digest/settings";

#[derive(Debug, Subcommand)]
pub enum DigestCommand {
    /// Print your unfinished Work that has gone quiet and where agents in this repository keep colliding.
    Show {
        /// The period, in days up to now.
        #[arg(long, default_value_t = DEFAULT_DAYS)]
        days: u32,
        /// Print the versioned JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Print whether the weekly digest is delivered for this repository and for you.
    Settings {
        /// Print the JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Switch weekly delivery on for this repository; needs a repository admin or Hub admin.
    Enable,
    /// Switch weekly delivery off for this repository; needs a repository admin or Hub admin.
    Disable,
    /// Stop your own weekly digest for this repository.
    OptOut,
    /// Receive your own weekly digest for this repository again.
    OptIn,
}

/// The switches in one sentence each.
fn settings_text(settings: &DigestSettings) -> String {
    let repository = if settings.enabled {
        "on"
    } else {
        "off (a repository admin or Hub admin switches it on with `agentscm digest enable`)"
    };
    let you = if settings.opted_out {
        "off (switch it back on with `agentscm digest opt-in`)"
    } else {
        "on"
    };
    let outcome = if settings.delivering {
        "The Hub sends you the digest each week where you have a Slack binding."
    } else {
        "The Hub sends you no digest."
    };
    format!(
        "Weekly digest for {}: {repository}.\nFor you: {you}.\n{outcome}",
        settings.repository
    )
}

pub async fn run(command: DigestCommand) -> Result<()> {
    let hub = Hub::connect(LIMIT)?;
    let (reply, as_json) = match command {
        DigestCommand::Show { days, json } => {
            let reply = hub
                .call(Method::GET, &format!("{DIGEST}?days={days}"), None)
                .await?;
            if json {
                println!("{reply}");
            } else {
                let digest: WorkDigestV1 = serde_json::from_value(reply)?;
                println!("{}", render_text(&digest));
            }
            return Ok(());
        }
        DigestCommand::Settings { json } => (hub.call(Method::GET, SETTINGS, None).await?, json),
        DigestCommand::Enable | DigestCommand::Disable => {
            let enabled = matches!(command, DigestCommand::Enable);
            let body = json!({"enabled": enabled});
            (hub.call(Method::PUT, SETTINGS, Some(body)).await?, false)
        }
        DigestCommand::OptOut | DigestCommand::OptIn => {
            let opted_out = matches!(command, DigestCommand::OptOut);
            let body = json!({"opted_out": opted_out});
            let path = format!("{SETTINGS}/me");
            (hub.call(Method::PUT, &path, Some(body)).await?, false)
        }
    };
    if as_json {
        println!("{reply}");
    } else {
        let settings: DigestSettings = serde_json::from_value(reply)?;
        println!("{}", settings_text(&settings));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_text_says_who_switched_what_and_the_outcome() {
        let mut settings = DigestSettings {
            repository: "acme/widgets".into(),
            enabled: false,
            opted_out: false,
            delivering: false,
        };
        let off = settings_text(&settings);
        assert!(off.contains("acme/widgets: off"), "{off}");
        assert!(off.contains("agentscm digest enable"), "{off}");
        assert!(off.ends_with("The Hub sends you no digest."), "{off}");
        settings.enabled = true;
        settings.delivering = true;
        let on = settings_text(&settings);
        assert!(on.contains("acme/widgets: on.\nFor you: on."), "{on}");
        assert!(
            on.contains("each week where you have a Slack binding"),
            "{on}"
        );
        settings.opted_out = true;
        settings.delivering = false;
        let out = settings_text(&settings);
        assert!(out.contains("agentscm digest opt-in"), "{out}");
        assert!(out.ends_with("The Hub sends you no digest."), "{out}");
    }
}
