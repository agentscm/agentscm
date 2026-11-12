//! `agentscm deployed`: a deploy script tells the Hub that a commit is live in an environment.
//! The Hub records the deployment on GitHub or GitLab with the credential it already holds for
//! the repository (its GitHub App installation or its GitLab connection), so no forge token is
//! needed here: the command signs in to the Hub as every other client command does.
use std::process::Command;
use std::time::Duration;

use agentscm::{Error, Result};
use clap::{Args, ValueEnum};
use reqwest::Method;
use serde_json::json;

use crate::intent_cli::{Hub, valid_repo};
use crate::quickstart::detect::parse_remote;

const LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Args)]
pub struct DeployedArgs {
    /// Environment the commit now runs in, such as `production` or `staging`.
    #[arg(long = "env", value_name = "NAME")]
    environment: String,
    /// The commit deployed.
    #[arg(long, default_value = "HEAD")]
    commit: String,
    #[arg(long, value_enum, default_value_t = Status::Success)]
    status: Status,
    /// Where the deployed app is reached (GitHub keeps it; GitLab deployments carry no URL).
    #[arg(long)]
    url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Status {
    Success,
    Failure,
    #[value(name = "in_progress", alias = "in-progress")]
    InProgress,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::InProgress => "in_progress",
        }
    }

    fn words(self) -> &'static str {
        match self {
            Self::Success => "live",
            Self::Failure => "failed",
            Self::InProgress => "deploying",
        }
    }
}

pub async fn run(args: DeployedArgs) -> Result<()> {
    let sha = git(&[
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{}^{{commit}}", args.commit),
    ])
    .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
    .ok_or_else(|| Error::Invalid(format!("{} is not a commit in this checkout", args.commit)))?
    .to_ascii_lowercase();
    let hub = Hub::connect(LIMIT)?;
    let whoami = hub.call(Method::GET, "/v1/hub/whoami", None).await?;
    let repo = whoami["token"]["repository"]
        .as_str()
        .filter(|repo| valid_repo(repo))
        .ok_or_else(|| Error::Invalid("hub error: bad repository".into()))?
        .to_owned();
    if let Some(remote) = git(&["remote", "get-url", "origin"]).and_then(|u| parse_remote(&u))
        && !remote.repo.eq_ignore_ascii_case(&repo)
    {
        return Err(Error::Invalid(format!(
            "the Hub token is for {repo}, but this checkout's origin is {}",
            remote.repo
        )));
    }
    let mut body = json!({"commit": sha, "environment": args.environment.trim(),
        "status": args.status.as_str()});
    if let Some(url) = &args.url {
        body["url"] = json!(url);
    }
    let path = format!("{}/deployments", Hub::repo_path(&repo));
    let reply = hub.call(Method::POST, &path, Some(body)).await?;
    let forge = match reply["provider"].as_str() {
        Some("gitlab") => "GitLab",
        _ => "GitHub",
    };
    if forge == "GitLab" && args.url.is_some() {
        eprintln!("GitLab deployments carry no URL; --url was not recorded.");
    }
    let what = format!(
        "{} {} in {} for {repo} on {forge}",
        &sha[..12],
        args.status.words(),
        reply["environment"].as_str().unwrap_or_default(),
    );
    if reply["recorded"] == true {
        println!("Recorded {what}.");
    } else {
        println!("Already recorded {what}; nothing changed.");
    }
    Ok(())
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
}
