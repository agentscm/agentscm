//! Agent-facing promise reads and advisory inputs through the workspace Hub API.
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentscm::hub::promise::{
    DEFAULT_PROMISE_PAGE, MAX_PROMISE_PAGE, PromiseHistoryPageV1, PromiseInputReceiptV1,
    PromiseInputV1, PromisePageV1, PromiseViewV1,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use reqwest::Method;
use serde::de::DeserializeOwned;

use crate::intent_cli::Hub;

const LIMIT: Duration = Duration::from_secs(10);
const REQUEST_BYTES: u64 = 64 * 1024;
const PATH: &str = "/v1/hub/workspace/promises";

#[derive(Debug, Subcommand)]
pub enum PromiseCommand {
    /// List owner-defined promises visible to this token; affected Work and evidence are not assessed yet.
    List {
        #[arg(long, default_value_t = DEFAULT_PROMISE_PAGE)]
        limit: usize,
        #[arg(long)]
        after: Option<String>,
    },
    /// Read the exact current owner-defined promise version.
    Show { id: String },
    /// Read immutable earlier versions, newest first.
    History {
        id: String,
        #[arg(long, default_value_t = DEFAULT_PROMISE_PAGE)]
        limit: usize,
        #[arg(long)]
        before_version: Option<u64>,
    },
    /// Submit an unadopted, caller-authored proposal as strict PromiseInputV1 JSON.
    Propose {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
    /// Submit a caller-authored report against an exact promise version; this is not evidence.
    Report {
        #[arg(long, default_value = "-")]
        request: PathBuf,
    },
}

pub async fn run(command: PromiseCommand) -> Result<()> {
    let hub = Hub::connect(LIMIT)?;
    let proposal = matches!(&command, PromiseCommand::Propose { .. });
    let reply = match command {
        PromiseCommand::List { limit, after } => {
            if !(1..=MAX_PROMISE_PAGE).contains(&limit)
                || after.as_ref().is_some_and(|value| value.len() > 512)
            {
                return Err(invalid("invalid promise page limit or cursor"));
            }
            let mut url = reqwest::Url::parse("http://hub/v1/hub/workspace/promises")
                .map_err(|_| invalid("invalid promise list URL"))?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &limit.to_string());
                if let Some(after) = after {
                    query.append_pair("after", &after);
                }
            }
            let page: PromisePageV1 = read(
                &hub,
                url.as_str()
                    .strip_prefix("http://hub")
                    .ok_or_else(|| invalid("invalid promise list URL"))?,
            )
            .await?;
            serde_json::to_value(page)?
        }
        PromiseCommand::Show { id } => {
            let view: PromiseViewV1 = read(&hub, &promise_path(&id)?).await?;
            serde_json::to_value(view)?
        }
        PromiseCommand::History {
            id,
            limit,
            before_version,
        } => {
            if !(1..=MAX_PROMISE_PAGE).contains(&limit) {
                return Err(invalid("invalid promise history limit"));
            }
            let mut url = reqwest::Url::parse(&format!("http://hub{}/history", promise_path(&id)?))
                .map_err(|_| invalid("invalid promise history URL"))?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", &limit.to_string());
                if let Some(before) = before_version {
                    query.append_pair("before_version", &before.to_string());
                }
            }
            let page: PromiseHistoryPageV1 = read(
                &hub,
                url.as_str()
                    .strip_prefix("http://hub")
                    .ok_or_else(|| invalid("invalid promise history URL"))?,
            )
            .await?;
            serde_json::to_value(page)?
        }
        PromiseCommand::Propose { request } | PromiseCommand::Report { request } => {
            let input: PromiseInputV1 = read_request(&request)?;
            let who = hub.call(Method::GET, "/v1/hub/whoami", None).await?;
            // Workspace promise grants use the token's registered repository key.
            // whoami.repository_id is an optional numeric forge binding, not that key.
            let repository = who["token"]["repository"]
                .as_str()
                .ok_or_else(|| invalid("hub did not identify the token repository"))?;
            validate_input(&input, proposal, repository)?;
            let receipt: PromiseInputReceiptV1 = serde_json::from_value(
                hub.call(
                    Method::POST,
                    &format!("{PATH}/inputs"),
                    Some(serde_json::to_value(input)?),
                )
                .await?,
            )?;
            serde_json::to_value(receipt)?
        }
    };
    println!("{reply}");
    Ok(())
}

fn validate_input(input: &PromiseInputV1, proposal: bool, repository: &str) -> Result<()> {
    if proposal != matches!(input, PromiseInputV1::Proposal { .. }) {
        return Err(invalid("promise input kind differs from command"));
    }
    input
        .validate()
        .map_err(|_| invalid("invalid promise input"))?;
    if input.repository_id() != repository {
        return Err(invalid(
            "promise input repository differs from this agent token",
        ));
    }
    Ok(())
}

async fn read<T: DeserializeOwned>(hub: &Hub, path: &str) -> Result<T> {
    serde_json::from_value(hub.call(Method::GET, path, None).await?).map_err(Error::from)
}

fn promise_path(id: &str) -> Result<String> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(invalid("invalid promise id"));
    }
    Ok(format!("{PATH}/{id}"))
}

fn read_request(path: &PathBuf) -> Result<PromiseInputV1> {
    let mut bytes = Vec::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .take(REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
    } else {
        File::open(path)?
            .take(REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() as u64 > REQUEST_BYTES {
        return Err(invalid("promise input exceeds 64 KiB"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("promise input is not valid strict JSON"))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn promise_path_rejects_navigation_and_unbounded_ids() {
        for id in ["", ".", "..", "../hidden", "a/b", "a?b"] {
            assert!(promise_path(id).is_err());
        }
        assert!(promise_path(&"a".repeat(129)).is_err());
        assert_eq!(promise_path("prm_12").unwrap(), format!("{PATH}/prm_12"));
    }

    #[test]
    fn input_kind_and_stable_repository_are_checked_before_submission() {
        let input: PromiseInputV1 = serde_json::from_value(json!({
            "kind":"proposal","operation_id":"propose_01","repository_id":"repo_01",
            "original_request":"Preserve settings.","commitment":"Keep settings.",
            "criteria":[{"id":"preserve","text":"Existing settings remain.","evidence_requirements":[]}],
            "constraints":[]
        })).unwrap();
        assert!(validate_input(&input, true, "repo_01").is_ok());
        assert!(validate_input(&input, false, "repo_01").is_err());
        assert!(validate_input(&input, true, "repo_02").is_err());
    }

    #[test]
    fn request_file_is_bounded_and_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.json");
        std::fs::write(&path, b"{\"kind\":\"proposal\",\"unexpected\":true}").unwrap();
        assert!(read_request(&path).is_err());
        std::fs::write(&path, vec![b'x'; REQUEST_BYTES as usize + 1]).unwrap();
        assert!(read_request(&path).is_err());
    }
}
