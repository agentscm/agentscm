use clap::Subcommand;
use serde_json::Value;

use agentscm::Result;

use crate::telemetry::{self, Profile};

#[derive(Debug, Subcommand)]
pub(crate) enum TelemetryCommand {
    /// Show the requested preference and whether sending is currently effective.
    Status,
    /// Inspect local pseudonymous IDs, queued categories and retry state.
    Inspect,
    /// Explicitly allow telemetry and grant consent where the configured profile requires it.
    On,
    /// Persistently disable telemetry and delete queued events and pseudonymous IDs.
    Off,
    /// Set the trusted per-user destination and legal profile.
    Configure {
        /// Absolute HTTPS relay URL ending in /v1/telemetry/batches.
        #[arg(long)]
        endpoint: Option<String>,
        /// Remove the relay URL and discard queued events.
        #[arg(long)]
        clear_endpoint: bool,
        /// Whether default collection is permitted or affirmative consent is required.
        #[arg(long, value_enum)]
        profile: Option<Profile>,
    },
    /// Deliver a bounded queued batch in a separate process.
    #[command(hide = true)]
    Send,
}

pub(crate) enum Output {
    Value(Value),
    Quiet,
}

pub(crate) async fn run(command: TelemetryCommand, invocation_off: bool) -> Result<Output> {
    match command {
        TelemetryCommand::Status => Ok(Output::Value(telemetry::status(invocation_off, false)?)),
        TelemetryCommand::Inspect => Ok(Output::Value(telemetry::status(invocation_off, true)?)),
        TelemetryCommand::On => Ok(Output::Value(telemetry::set_on()?)),
        TelemetryCommand::Off => Ok(Output::Value(telemetry::set_off()?)),
        TelemetryCommand::Configure {
            endpoint,
            clear_endpoint,
            profile,
        } => Ok(Output::Value(telemetry::configure(
            endpoint.as_deref(),
            clear_endpoint,
            profile,
        )?)),
        TelemetryCommand::Send => {
            telemetry::send(invocation_off).await;
            Ok(Output::Quiet)
        }
    }
}
