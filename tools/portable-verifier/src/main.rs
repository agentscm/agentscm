use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agentscm_portable_verifier::format::{ErrorCode, MAX_JSON_BYTES, MAX_SUPPLIED_ARTIFACT_BYTES};
use agentscm_portable_verifier::{
    Clock, SuppliedArtifact, SuppliedArtifactContent, VerificationReport, VerifyError, verify,
};
use serde::Serialize;

const HELP: &str = "agentscm-portable-verifier verify --envelope PATH --registry PATH (--clock-known UNIX_MS | --clock-uncertain) [--artifact-full ID PATH] [--artifact-partial ID PATH] [--artifact-redacted ID] [--artifact-unavailable ID]\n";

#[derive(Debug)]
struct VerifyArgs {
    envelope: PathBuf,
    registry: PathBuf,
    clock: Clock,
    artifacts: Vec<ArtifactArg>,
}

#[derive(Debug)]
enum ArtifactArg {
    Full { artifact_id: String, path: PathBuf },
    Partial { artifact_id: String, path: PathBuf },
    Redacted { artifact_id: String },
    Unavailable { artifact_id: String },
}

#[derive(Serialize)]
struct CliSummary {
    integrity: agentscm_portable_verifier::format::IntegrityStatus,
    registry_binding: agentscm_portable_verifier::format::RegistryBindingStatus,
    current_issuer_trust: agentscm_portable_verifier::format::IssuerTrustStatus,
    freshness: agentscm_portable_verifier::format::FreshnessStatus,
    execution_claim: agentscm_portable_verifier::format::ExecutionClaim,
    artifacts: Vec<agentscm_portable_verifier::format::ArtifactAssessment>,
    evaluated_clock: Clock,
    evaluated_registry_sha256: String,
    envelope_sha256: String,
    verified_receipt_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_payload_sha256: Option<String>,
    authority: &'static str,
}

#[derive(Serialize)]
struct CliError {
    code: ErrorCode,
    message: &'static str,
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        print_help(false);
        return ExitCode::from(2);
    };
    if command == OsStr::new("--help") || command == OsStr::new("-h") {
        if args.next().is_some() {
            emit_error(VerifyError::new(
                ErrorCode::InvalidCli,
                "help does not accept trailing arguments",
            ));
            return ExitCode::from(2);
        }
        print_help(true);
        return ExitCode::SUCCESS;
    }
    if command == OsStr::new("--version") || command == OsStr::new("-V") {
        if args.next().is_some() {
            emit_error(VerifyError::new(
                ErrorCode::InvalidCli,
                "version does not accept trailing arguments",
            ));
            return ExitCode::from(2);
        }
        println!("agentscm-portable-verifier {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if command != OsStr::new("verify") {
        emit_error(VerifyError::new(ErrorCode::InvalidCli, "unknown command"));
        return ExitCode::from(2);
    }

    match parse_verify_args(args.collect())
        .and_then(run_verify)
        .and_then(|summary| write_json_line(std::io::stdout().lock(), &summary))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            emit_error(error);
            ExitCode::from(1)
        }
    }
}

fn parse_verify_args(raw: Vec<OsString>) -> Result<VerifyArgs, VerifyError> {
    let mut envelope = None;
    let mut registry = None;
    let mut clock = None;
    let mut artifacts = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let flag = raw[index]
            .to_str()
            .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "option name is not UTF-8"))?;
        match flag {
            "--envelope" => {
                if envelope
                    .replace(PathBuf::from(required_arg(&raw, index + 1)?))
                    .is_some()
                {
                    return Err(VerifyError::new(
                        ErrorCode::InvalidCli,
                        "--envelope is specified more than once",
                    ));
                }
                index += 2;
            }
            "--registry" => {
                if registry
                    .replace(PathBuf::from(required_arg(&raw, index + 1)?))
                    .is_some()
                {
                    return Err(VerifyError::new(
                        ErrorCode::InvalidCli,
                        "--registry is specified more than once",
                    ));
                }
                index += 2;
            }
            "--clock-known" => {
                if clock.is_some() {
                    return Err(VerifyError::new(
                        ErrorCode::InvalidCli,
                        "clock is specified more than once",
                    ));
                }
                let text = required_text(&raw, index + 1)?;
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(VerifyError::new(
                        ErrorCode::InvalidCli,
                        "known clock must be unsigned decimal digits",
                    ));
                }
                let unix_ms = text.parse().map_err(|_| {
                    VerifyError::new(
                        ErrorCode::ClockLimit,
                        "known clock is outside the supported range",
                    )
                })?;
                clock = Some(Clock::Known { unix_ms });
                index += 2;
            }
            "--clock-uncertain" => {
                if clock.is_some() {
                    return Err(VerifyError::new(
                        ErrorCode::InvalidCli,
                        "clock is specified more than once",
                    ));
                }
                clock = Some(Clock::Uncertain {});
                index += 1;
            }
            "--artifact-full" | "--artifact-partial" => {
                let artifact_id = required_text(&raw, index + 1)?.to_owned();
                let path = PathBuf::from(required_arg(&raw, index + 2)?);
                if flag == "--artifact-full" {
                    artifacts.push(ArtifactArg::Full { artifact_id, path });
                } else {
                    artifacts.push(ArtifactArg::Partial { artifact_id, path });
                }
                index += 3;
            }
            "--artifact-redacted" | "--artifact-unavailable" => {
                let artifact_id = required_text(&raw, index + 1)?.to_owned();
                if flag == "--artifact-redacted" {
                    artifacts.push(ArtifactArg::Redacted { artifact_id });
                } else {
                    artifacts.push(ArtifactArg::Unavailable { artifact_id });
                }
                index += 2;
            }
            _ => {
                return Err(VerifyError::new(
                    ErrorCode::InvalidCli,
                    "unknown or misplaced argument",
                ));
            }
        }
    }
    if artifacts.len() > 64 {
        return Err(VerifyError::new(
            ErrorCode::CountLimit,
            "supplied artifact count exceeds its limit",
        ));
    }
    Ok(VerifyArgs {
        envelope: envelope
            .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "--envelope is required"))?,
        registry: registry
            .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "--registry is required"))?,
        clock: clock
            .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "one clock mode is required"))?,
        artifacts,
    })
}

fn required_arg(raw: &[OsString], index: usize) -> Result<&OsStr, VerifyError> {
    raw.get(index)
        .map(OsString::as_os_str)
        .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "argument value is missing"))
}

fn required_text(raw: &[OsString], index: usize) -> Result<&str, VerifyError> {
    required_arg(raw, index)?
        .to_str()
        .ok_or_else(|| VerifyError::new(ErrorCode::InvalidCli, "non-path argument is not UTF-8"))
}

fn run_verify(args: VerifyArgs) -> Result<CliSummary, VerifyError> {
    let envelope = read_bounded_file(&args.envelope, MAX_JSON_BYTES, ErrorCode::RawSizeLimit)?;
    let registry = read_bounded_file(&args.registry, MAX_JSON_BYTES, ErrorCode::RawSizeLimit)?;
    let supplied = read_artifacts(args.artifacts)?;
    let report = verify(&envelope, &registry, args.clock, &supplied)?;
    Ok(summary_owned(report))
}

fn read_artifacts(arguments: Vec<ArtifactArg>) -> Result<Vec<SuppliedArtifact>, VerifyError> {
    let mut remaining = MAX_SUPPLIED_ARTIFACT_BYTES;
    let mut supplied = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let (artifact_id, content) = match argument {
            ArtifactArg::Full { artifact_id, path } => {
                let bytes =
                    read_bounded_file(&path, remaining, ErrorCode::SuppliedArtifactSizeLimit)?;
                remaining -= bytes.len();
                (artifact_id, SuppliedArtifactContent::Full(bytes))
            }
            ArtifactArg::Partial { artifact_id, path } => {
                let bytes =
                    read_bounded_file(&path, remaining, ErrorCode::SuppliedArtifactSizeLimit)?;
                remaining -= bytes.len();
                (artifact_id, SuppliedArtifactContent::Partial(bytes))
            }
            ArtifactArg::Redacted { artifact_id } => {
                (artifact_id, SuppliedArtifactContent::Redacted)
            }
            ArtifactArg::Unavailable { artifact_id } => {
                (artifact_id, SuppliedArtifactContent::Unavailable)
            }
        };
        supplied.push(SuppliedArtifact {
            artifact_id,
            content,
        });
    }
    Ok(supplied)
}

fn read_bounded_file(
    path: &Path,
    limit: usize,
    limit_error: ErrorCode,
) -> Result<Vec<u8>, VerifyError> {
    let file = File::open(path)
        .map_err(|_| VerifyError::new(ErrorCode::Io, "unable to open an input file"))?;
    let read_limit = limit.checked_add(1).ok_or_else(|| {
        VerifyError::new(
            ErrorCode::RawSizeLimit,
            "input byte limit cannot be represented",
        )
    })?;
    let mut bytes = Vec::new();
    file.take(read_limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| VerifyError::new(ErrorCode::Io, "unable to read an input file"))?;
    if bytes.len() > limit {
        return Err(VerifyError::new(
            limit_error,
            "input file exceeds its byte limit",
        ));
    }
    Ok(bytes)
}

fn summary_owned(report: VerificationReport) -> CliSummary {
    let payload_hash = report
        .verified_receipt()
        .map(|receipt| receipt.canonical_payload_sha256().to_owned());
    CliSummary {
        integrity: report.integrity(),
        registry_binding: report.registry_binding(),
        current_issuer_trust: report.current_issuer_trust(),
        freshness: report.freshness(),
        execution_claim: report.execution_claim(),
        artifacts: report.artifacts().to_vec(),
        evaluated_clock: report.evaluated_clock().clone(),
        evaluated_registry_sha256: report.evaluated_registry_sha256().to_owned(),
        envelope_sha256: report.envelope_sha256().to_owned(),
        verified_receipt_present: report.verified_receipt().is_some(),
        canonical_payload_sha256: payload_hash,
        authority: "descriptive_only_registry_authentication_is_external",
    }
}

fn write_json_line<W: Write, T: Serialize>(mut writer: W, value: &T) -> Result<(), VerifyError> {
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| VerifyError::new(ErrorCode::Io, "unable to write bounded JSON output"))?;
    writer
        .write_all(b"\n")
        .map_err(|_| VerifyError::new(ErrorCode::Io, "unable to write bounded JSON output"))
}

fn emit_error(error: VerifyError) {
    let value = CliError {
        code: error.code,
        message: error.message,
    };
    let _ = write_json_line(std::io::stderr().lock(), &value);
}

fn print_help(stdout: bool) {
    if stdout {
        print!("{HELP}");
    } else {
        eprint!("{HELP}");
    }
}
