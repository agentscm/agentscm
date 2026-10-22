//! Operator-side Slack setup over the same local service used by installation flows.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use agentscm::hub::forge_http::{check_base_url, send};
use agentscm::hub::slack::{BindingConfig, SlackConfig, setup};
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use ring::hmac;
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum SlackCommand {
    /// Print the least-privilege manifest; omit --hub-url for initial app creation.
    Manifest {
        #[arg(long)]
        hub_url: Option<String>,
    },
    /// Prepare a private Slack CLI project without scaffolding another application.
    Project {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        hub_url: Option<String>,
    },
    /// Run the official Slack CLI installer in a validated private project.
    Install {
        #[arg(long)]
        directory: PathBuf,
        /// Explicit workspace ID, never an inferred current workspace.
        #[arg(long)]
        team_id: String,
        #[arg(long)]
        hub_url: Option<String>,
        #[arg(long)]
        slack_cli: Option<PathBuf>,
    },
    /// Derive the repository binding from its read-only Hub credential and save config.
    Setup(SetupArgs),
    /// Inspect local setup; --verify also checks Hub identity, Slack bot and public callback.
    Doctor {
        #[arg(long)]
        config_file: PathBuf,
        #[arg(long)]
        verify: bool,
        /// Optional local control endpoint; the public callback is still checked separately.
        #[arg(long)]
        api_url: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    #[arg(long)]
    directory: PathBuf,
    #[arg(long)]
    hub_url: String,
    #[arg(long)]
    api_url: Option<String>,
    #[arg(long)]
    repo: String,
    #[arg(long)]
    app_id: String,
    /// Copy from the intended app installation, independently of the supplied token.
    #[arg(long)]
    bot_id: String,
    #[arg(long)]
    team_id: String,
    #[arg(long)]
    slack_user: String,
    #[arg(long)]
    signing_secret_file: PathBuf,
    #[arg(long)]
    bot_token_file: PathBuf,
    #[arg(long)]
    agent_token_file: PathBuf,
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

pub async fn run(command: SlackCommand) -> Result<()> {
    let output = match command {
        SlackCommand::Manifest { hub_url } => {
            setup::manifest(hub_url.as_deref()).map_err(crate::hub_cli::map_hub_error)?
        }
        SlackCommand::Project { directory, hub_url } => {
            json!(
                setup::project(&directory, hub_url.as_deref())
                    .map_err(crate::hub_cli::map_hub_error)?
            )
        }
        SlackCommand::Install {
            directory,
            team_id,
            hub_url,
            slack_cli,
        } => install(directory, team_id, hub_url, slack_cli).await?,
        SlackCommand::Setup(args) => configure(args).await?,
        SlackCommand::Doctor {
            config_file,
            verify,
            api_url,
        } => doctor(&config_file, verify, api_url.as_deref()).await?,
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| invalid("Slack setup HTTP client unavailable"))
}

async fn request(request: reqwest::RequestBuilder, error: &str) -> Result<Value> {
    let (status, body) = send(request, 64 * 1024).await.map_err(|_| invalid(error))?;
    if status != 200 {
        return Err(invalid(error));
    }
    Ok(body)
}

fn api_origin(public: &str, api_url: Option<&str>) -> Result<String> {
    // Validate the public origin through the shared manifest authority before sending a token.
    setup::manifest(Some(public)).map_err(crate::hub_cli::map_hub_error)?;
    check_base_url(api_url.unwrap_or(public)).map_err(crate::hub_cli::map_hub_error)
}

async fn identity(client: &reqwest::Client, origin: &str, token: &str) -> Result<Value> {
    request(
        client
            .get(format!("{origin}/v1/hub/whoami"))
            .bearer_auth(token),
        "Hub identity check failed; verify the endpoint and dedicated read-only credential",
    )
    .await
}

fn binding(
    who: &Value,
    repo: &str,
    slack_user: String,
    token_file: PathBuf,
) -> Result<BindingConfig> {
    let token = &who["token"];
    let owner = &who["owner"];
    if token["read_only"] != true
        || token["repository"].as_str() != Some(repo)
        || token["expires_at"]
            .as_i64()
            .is_none_or(|at| at <= unix_now())
    {
        return Err(invalid(
            "Slack setup requires an unexpired read-only credential for the requested repository",
        ));
    }
    let provider = owner["provider"].as_str().unwrap_or_default();
    let host = owner["host"].as_str().unwrap_or_default();
    let user = owner["user_id"].as_i64().filter(|id| *id > 0);
    let repository = token["repository_id"].as_i64().filter(|id| *id > 0);
    if !matches!(provider, "github" | "gitlab")
        || host.is_empty()
        || user.is_none()
        || repository.is_none()
    {
        return Err(invalid(
            "Hub did not return a stable forge/repository identity; use a matching Hub build and repository-bound credential",
        ));
    }
    Ok(BindingConfig {
        slack_user,
        provider: provider.into(),
        host: host.into(),
        forge_user_id: user.unwrap(),
        repository: repo.into(),
        repository_id: repository.unwrap(),
        agent_token_file: token_file,
    })
}

async fn configure(args: SetupArgs) -> Result<Value> {
    let origin = api_origin(&args.hub_url, args.api_url.as_deref())?;
    let credential =
        setup::read_agent_token(&args.agent_token_file).map_err(crate::hub_cli::map_hub_error)?;
    let who = identity(&client()?, &origin, &credential).await?;
    let binding = binding(
        &who,
        &args.repo,
        args.slack_user,
        args.agent_token_file,
    )?;
    let config = SlackConfig {
        version: 1,
        app_id: args.app_id,
        bot_id: args.bot_id,
        team_id: args.team_id,
        hub_url: args.hub_url,
        signing_secret_file: args.signing_secret_file,
        bot_token_file: args.bot_token_file,
        database: args.directory.join("install.sqlite"),
        bindings: vec![binding],
    };
    let report = setup::prepare(&args.directory, config).map_err(crate::hub_cli::map_hub_error)?;
    Ok(
        json!({"ok":true,"configuration":report,"hub_identity_verified":true,"live_verified":false,
        "next_action":"Install this config with quickstart --hub local --slack-config-file CONFIG_FILE, or configure the existing server's AGENTSCM_SLACK_FILE and restart it. Then run hub slack doctor --verify."}),
    )
}

async fn doctor(path: &Path, verify: bool, api_url: Option<&str>) -> Result<Value> {
    let report = setup::check(path).map_err(crate::hub_cli::map_hub_error)?;
    if !verify {
        return Ok(
            json!({"ok":true,"configuration":report,"live_verified":false,
            "next_action":"Run hub slack doctor --config-file CONFIG_FILE --verify after installing the app and starting the configured Hub."}),
        );
    }
    let config = setup::read_config(path).map_err(crate::hub_cli::map_hub_error)?;
    let origin = api_origin(&config.hub_url, api_url)?;
    let client = client()?;
    for expected in &config.bindings {
        let token = setup::read_agent_token(&expected.agent_token_file)
            .map_err(crate::hub_cli::map_hub_error)?;
        let who = identity(&client, &origin, &token).await?;
        let actual = binding(
            &who,
            &expected.repository,
            expected.slack_user.clone(),
            expected.agent_token_file.clone(),
        )?;
        if actual != *expected {
            return Err(invalid(
                "Slack binding no longer matches current Hub authorization",
            ));
        }
    }
    let bundle =
        setup::bundle(path, Path::new("/slack-check")).map_err(crate::hub_cli::map_hub_error)?;
    let file = |name: &str| -> Result<&str> {
        bundle
            .files()
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
            .map(str::trim)
            .ok_or_else(|| invalid("Slack credential unavailable"))
    };
    let auth = request(
        client
            .post("https://slack.com/api/auth.test")
            .bearer_auth(file("bot-token")?),
        "Slack bot identity check failed",
    )
    .await?;
    if auth["ok"] != true
        || auth["team_id"].as_str() != Some(&config.team_id)
        || auth["bot_id"].as_str() != Some(&config.bot_id)
        || auth["app_id"]
            .as_str()
            .is_some_and(|id| id != config.app_id)
    {
        return Err(invalid(
            "Slack bot does not match the intended installation",
        ));
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| invalid("Slack verification challenge unavailable"))?;
    let challenge = hex(&random);
    let body = json!({"type":"url_verification","challenge":challenge}).to_string();
    let stamp = unix_now().to_string();
    let key = hmac::Key::new(hmac::HMAC_SHA256, file("signing-secret")?.as_bytes());
    let signature = format!(
        "v0={}",
        hex(hmac::sign(&key, format!("v0:{stamp}:{body}").as_bytes()).as_ref())
    );
    let response = request(client.post(&report.callback_url)
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", stamp)
        .header("x-slack-signature", signature).body(body),
        "Public HTTPS callback verification failed; check ingress, active config and signing secret").await?;
    if response["challenge"].as_str() != Some(&challenge) {
        return Err(invalid(
            "Public callback returned the wrong verification challenge",
        ));
    }
    Ok(
        json!({"ok":true,"configuration":report,"hub_identity_verified":true,
        "slack_bot_verified":true,"public_callback_verified":false,"callback_challenge_echoed":true,"live_verified":false,
        "direct_message_verified":false,"next_action":"Make one agent edit a file another agent holds, twice, and check that both people get a direct message and the holder's buttons answer the handoff. The callback echo proves reachability, not signature enforcement, intended server identity, Slack-origin delivery, rendering or human approval."}),
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn install(
    directory: PathBuf,
    team_id: String,
    hub_url: Option<String>,
    slack_cli: Option<PathBuf>,
) -> Result<Value> {
    if !team_id.starts_with('T')
        || !(2..=32).contains(&team_id.len())
        || !team_id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Err(invalid(
            "Select the intended Slack workspace with its explicit T… ID",
        ));
    }
    let project =
        setup::project(&directory, hub_url.as_deref()).map_err(crate::hub_cli::map_hub_error)?;
    let icon = directory.join("icon.png");
    let expected_icon_report = format!("Updated app icon: {}", icon.display());
    let installed_directory = directory.clone();
    let installed_team = team_id.clone();
    // Provider tooling receives its own login/config environment, never Hub/forge secrets.
    let environment: Vec<_> = [
        "HOME",
        "PATH",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "USER",
        "LOGNAME",
        "SystemRoot",
        "WINDIR",
    ]
    .into_iter()
    .filter_map(|key| std::env::var_os(key).map(|value| (key, value)))
    .collect();
    let program = slack_cli.unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".slack/bin/slack"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("slack"))
    });
    let status = tokio::task::spawn_blocking(move || {
        crate::quickstart::process::run(
            Command::new(program)
                .current_dir(directory)
                .env_clear()
                .envs(environment)
                .env("SLACK_CLI_APP_ICON_PATH", icon)
                .args([
                    "app",
                    "install",
                    "--environment",
                    "deployed",
                    "--app",
                    "deployed",
                    "--team",
                    &team_id,
                    "--skip-update",
                    "--no-color",
                ]),
            Duration::from_secs(120),
        )
    })
    .await
    .map_err(|_| invalid("Slack CLI process unavailable"))?
    .map_err(|_| {
        invalid(
            "Slack CLI is unavailable; install it from the official Slack distribution and sign in",
        )
    })?;
    if !status.success {
        return Err(invalid(
            "Slack CLI installation did not complete; resolve its reported login or workspace approval and resume in the same project",
        ));
    }
    // The provider CLI can exit zero after pending approval or an icon-upload failure.
    // Retain only the expected acknowledgement; never print captured provider output.
    if !status
        .stdout
        .lines()
        .any(|line| line.trim() == expected_icon_report)
    {
        return Err(invalid(
            "Slack CLI did not confirm the app icon upload; resolve workspace approval or icon upload and resume in the same project",
        ));
    }
    let app_record = setup::app_record(&installed_directory, &installed_team)
        .map_err(crate::hub_cli::map_hub_error)?
        .ok_or_else(|| {
            invalid("Slack CLI did not record this workspace app; resume in the same project")
        })?;
    Ok(
        json!({"ok":true,"project":project,"app_record":app_record,"local_app_recorded":true,"installation_verified":false,"cli_exit_success":true,
        "icon_upload_reported":true,"live_verified":false,
        "next_action":"Prepare Hub configuration from the intended app's private credentials, then use doctor --verify to check bot identity. A local app record and icon acknowledgement do not prove installation approval or live readiness."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Value {
        json!({"token":{"repository":"acme/widgets","repository_id":101,"read_only":true,"expires_at":unix_now()+3600},"owner":{"provider":"github","host":"github.com","user_id":42}})
    }

    #[test]
    fn binding_is_derived_from_live_read_only_stable_identity() {
        let actual = binding(
            &who(),
            "acme/widgets",
            "U123".into(),
            "/private/token".into(),
        )
        .unwrap();
        assert_eq!(actual.repository_id, 101);
        assert_eq!(actual.forge_user_id, 42);
        for (field, value) in [
            ("read_only", json!(false)),
            ("repository", json!("other/repo")),
            ("repository_id", Value::Null),
            ("expires_at", json!(unix_now() - 1)),
        ] {
            let mut invalid = who();
            invalid["token"][field] = value;
            assert!(
                binding(
                    &invalid,
                    "acme/widgets",
                    "U123".into(),
                    "/private/token".into(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn public_origin_is_validated_even_when_control_endpoint_is_local() {
        assert_eq!(
            api_origin("https://hub.example.test/", Some("http://127.0.0.1:7332")).unwrap(),
            "http://127.0.0.1:7332"
        );
        for bad in [
            "http://hub.example.test",
            "https://hub.example.test/path",
            "https://user:secret@hub.example.test",
        ] {
            assert!(api_origin(bad, Some("http://127.0.0.1:7332")).is_err());
        }
    }
}
