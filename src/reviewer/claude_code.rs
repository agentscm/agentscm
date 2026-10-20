use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::contracts::*;
use crate::execution::Cancellation;
use crate::execution::process::{Stop, capture_with_input};
use crate::{Error, Result};

const MINIMUM_VERSION: (u64, u64, u64) = (2, 1, 300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_OUTPUT_BYTES: u64 = 16 * 1024;
const CANCELLATION_POLL: Duration = Duration::from_millis(5);
const SCOPE_PROFILE: &str = "restricted-safe-mode-no-tools/v2";

// Claude's tool input schema rejects top-level union combinators, so this declares
// one flat object. The typed decoder below enforces each action's required and
// exclusive fields; the controller then validates coverage and every citation.
const DECISION_SCHEMA: &str = r#"{
  "type":"object",
  "properties":{
    "action":{"enum":["request_rows","finish"]},
    "row_ids":{"type":"array","maxItems":128,"uniqueItems":true,"items":{"type":"string","pattern":"^[0-9a-f]{64}$"}},
    "findings":{
      "type":"array","maxItems":64,
      "items":{
        "type":"object",
        "properties":{
          "title":{"type":"string","minLength":1,"maxLength":512},
          "detail":{"type":"string","minLength":1,"maxLength":16384},
          "severity":{"enum":["note","warning","error"]},
          "citations":{
            "type":"array","minItems":1,"maxItems":16,
            "items":{
              "type":"object",
              "properties":{
                "row_id":{"type":"string","pattern":"^[0-9a-f]{64}$"},
                "start_line":{"type":"integer","minimum":1},
                "end_line":{"type":"integer","minimum":1}
              },
              "required":["row_id","start_line","end_line"],
              "additionalProperties":false
            }
          }
        },
        "required":["title","detail","severity","citations"],
        "additionalProperties":false
      }
    },
    "unresolved":{"type":"array","maxItems":128,"items":{"type":"string","minLength":1,"maxLength":2048}}
  },
  "required":["action","row_ids","findings","unresolved"],
  "additionalProperties":false
}"#;

#[derive(Clone)]
pub struct ClaudeCodeTransport {
    executable: PathBuf,
    work_dir: PathBuf,
    scope: String,
    executable_sha256: String,
}

#[derive(Serialize)]
struct Scope<'a> {
    profile: &'a str,
    executable: &'a str,
    version: &'a str,
    auth_method: &'a str,
    decision_schema_sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedChatRequest {
    model: String,
    messages: Vec<PreparedMessage>,
    stream: bool,
    tools: Vec<serde_json::Value>,
    tool_choice: String,
    max_completion_tokens: u32,
    n: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    logged_in: bool,
    auth_method: String,
}

#[derive(Deserialize)]
struct ClaudeResult {
    #[serde(rename = "type")]
    kind: String,
    subtype: String,
    is_error: bool,
    session_id: String,
    num_turns: u64,
    structured_output: Option<serde_json::Value>,
    usage: ClaudeUsage,
    #[serde(rename = "modelUsage")]
    model_usage: BTreeMap<String, ClaudeModelUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeModelUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl ClaudeCodeTransport {
    /// Selects one installed Claude Code executable and an empty private working directory.
    /// The executable is trusted controller software and uses only an existing Claude
    /// subscription login; API-key and third-party-provider authentication are rejected.
    pub fn new(executable: &Path, work_dir: &Path) -> Result<Self> {
        let executable = validate_executable(executable)?;
        let executable_sha256 = executable_digest(&executable)?;
        let work_dir = validate_work_dir(work_dir)?;
        let version = probe_version(&executable, &work_dir)?;
        let auth_method = probe_auth(&executable, &work_dir)?;
        let executable_text = executable
            .to_str()
            .ok_or_else(|| Error::Invalid("Claude Code executable path must be UTF-8".into()))?;
        let scope = scope_digest(
            executable_text,
            &version,
            &auth_method,
            SCOPE_PROFILE,
            DECISION_SCHEMA,
        )?;
        Ok(Self {
            executable,
            work_dir,
            scope,
            executable_sha256,
        })
    }

    pub fn verified_executable_sha256(&self) -> Result<String> {
        if executable_digest(&self.executable)? != self.executable_sha256 {
            return Err(Error::Conflict("Claude Code executable changed".into()));
        }
        Ok(self.executable_sha256.clone())
    }

    fn invoke_blocking(
        &self,
        call: PreparedModelCall,
        review_cancel: ReviewCancellation,
    ) -> ModelCallObservation {
        let mut observation = ModelCallObservation::not_started(
            TransportBasis::ClaudeCode,
            ModelFailure::InvalidResponse,
        );
        if self.verified_executable_sha256().is_err() {
            return observation;
        }
        if review_cancel.is_cancelled() {
            observation.failure = Some(ModelFailure::Cancelled);
            return observation;
        }
        match validate_work_dir(&self.work_dir) {
            Ok(current) if current == self.work_dir.as_path() => {}
            _ => {
                observation.failure = Some(ModelFailure::InvalidResponse);
                return observation;
            }
        }
        let prepared = match decode_prepared_call(&call) {
            Ok(prepared) => prepared,
            Err(failure) => {
                observation.failure = Some(failure);
                return observation;
            }
        };
        let mut command = self.review_command(
            &call.expected_model,
            &prepared.messages[0].content,
            call.max_output_tokens,
        );
        let input = prepared.messages[1].content.clone().into_bytes();
        let cancellation = Cancellation::default();
        let watcher_done = Arc::new(AtomicBool::new(false));
        let capture = thread::scope(|scope| {
            let watcher_done_for_thread = Arc::clone(&watcher_done);
            let watcher_cancellation = cancellation.clone();
            scope.spawn(move || {
                while !watcher_done_for_thread.load(Ordering::Acquire) {
                    if review_cancel.is_cancelled() {
                        watcher_cancellation.cancel();
                        break;
                    }
                    thread::sleep(CANCELLATION_POLL);
                }
            });
            let capture = capture_with_input(
                &mut command,
                Some(input),
                Duration::from_millis(u64::from(call.timeout_ms)),
                call.max_response_bytes as u64,
                &cancellation,
            );
            watcher_done.store(true, Ordering::Release);
            capture
        });
        let capture = match capture {
            Ok(capture) => capture,
            Err(_) => {
                observation.failure = Some(ModelFailure::Connection);
                return observation;
            }
        };
        observation.state = CallState::Uncertain;
        if self.verified_executable_sha256().is_err() {
            return observation;
        }
        observation.captured_response_bytes = capture.stdout.len() as u64;
        observation.captured_response_sha256 = hash_bytes(&capture.stdout);
        observation.captured_response = capture.stdout;
        observation.response_capture_complete = capture.stop.is_none();
        if let Some(stop) = capture.stop {
            observation.failure = Some(match stop {
                Stop::Cancelled => ModelFailure::Cancelled,
                Stop::Timeout => ModelFailure::Deadline,
                Stop::Output => ModelFailure::ResponseLimit,
                Stop::Io => ModelFailure::Connection,
            });
            return observation;
        }
        if capture.code != Some(0) || !capture.stderr.is_empty() {
            observation.failure = Some(ModelFailure::InvalidResponse);
            return observation;
        }
        match decode_reply(
            &observation.captured_response,
            &call.expected_model,
            call.max_output_tokens,
        ) {
            Ok(reply) => {
                observation.state = CallState::Acknowledged;
                observation.failure = None;
                observation.reply = Some(reply);
            }
            Err(failure) => observation.failure = Some(failure),
        }
        observation
    }

    fn review_command(&self, model: &str, system_prompt: &str, max_output_tokens: u32) -> Command {
        let mut command = Command::new(&self.executable);
        configure_command(&mut command, &self.work_dir);
        command.env(
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            max_output_tokens.to_string(),
        );
        command.args([
            "--print",
            "--restricted",
            "--safe-mode",
            "--tools",
            "",
            "--disallowedTools",
            "mcp__*",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--permission-mode",
            "dontAsk",
            "--permission-prompts",
            "none",
            "--no-session-persistence",
            "--no-chrome",
            "--output-format",
            "json",
            "--max-turns",
            "1",
            "--json-schema",
            DECISION_SCHEMA,
            "--model",
            model,
            "--system-prompt",
            system_prompt,
        ]);
        command
    }
}

fn scope_digest(
    executable: &str,
    version: &str,
    auth_method: &str,
    profile: &str,
    decision_schema: &str,
) -> Result<String> {
    let decision_schema_sha256 = hash_bytes(decision_schema.as_bytes());
    domain_digest(
        "agentscm.claude-code-subscription/v1",
        &Scope {
            profile,
            executable,
            version,
            auth_method,
            decision_schema_sha256: &decision_schema_sha256,
        },
    )
}

impl ModelTransport for ClaudeCodeTransport {
    fn scope_sha256(&self) -> String {
        self.scope.clone()
    }

    fn basis(&self) -> TransportBasis {
        TransportBasis::ClaudeCode
    }

    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> Pin<Box<dyn Future<Output = ModelCallObservation> + Send + 'a>> {
        let transport = self.clone();
        let call = call.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || transport.invoke_blocking(call, cancel)).await
            {
                Ok(observation) => observation,
                Err(_) => ModelCallObservation::not_started(
                    TransportBasis::ClaudeCode,
                    ModelFailure::Connection,
                ),
            }
        })
    }
}

/// Returns the model identity reported by Claude Code's retained `modelUsage` map.
/// The requested model remains in the review recipe and call reservation. More than
/// one observed identity is rejected so provider fallback cannot pass silently.
pub fn observed_models(observation: &ModelCallObservation) -> Result<Vec<String>> {
    if observation.basis != TransportBasis::ClaudeCode {
        return Err(Error::Invalid(
            "observed Claude models require a Claude Code call".into(),
        ));
    }
    if observation.state != CallState::Acknowledged
        || observation.failure.is_some()
        || observation.reply.is_none()
        || !observation.response_capture_complete
    {
        return Err(Error::Invalid(
            "observed Claude models require a complete acknowledged call".into(),
        ));
    }
    let result = decode_result(&observation.captured_response)
        .map_err(|_| Error::Invalid("retained Claude Code result is invalid".into()))?;
    validate_observed_models(&result.model_usage)
        .map_err(|_| Error::Invalid("retained Claude Code model identities are invalid".into()))?;
    Ok(result.model_usage.into_keys().collect())
}

pub(super) fn decode_reply(
    bytes: &[u8],
    expected_model: &str,
    max_output_tokens: u32,
) -> std::result::Result<ModelReply, ModelFailure> {
    let result = decode_result(bytes)?;
    if result.kind != "result"
        || result.subtype != "success"
        || result.is_error
        // Claude Code 2.1.300 reports two turns when `--json-schema` performs
        // its built-in structured-output finalization, even with max-turns 1
        // and schema retries disabled. The durable journal unit is therefore
        // one bounded CLI dispatch, not a claim about provider request count.
        || !(1..=2).contains(&result.num_turns)
        || validate_identifier(&result.session_id).is_err()
    {
        return Err(ModelFailure::InvalidResponse);
    }
    validate_observed_models(&result.model_usage)?;
    let (observed_model, observed_usage) = result
        .model_usage
        .iter()
        .next()
        .ok_or(ModelFailure::Identity)?;
    validate_requested_model(expected_model, observed_model)?;
    if observed_usage.input_tokens != result.usage.input_tokens
        || observed_usage.output_tokens != result.usage.output_tokens
        || observed_usage.cache_creation_input_tokens != result.usage.cache_creation_input_tokens
        || observed_usage.cache_read_input_tokens != result.usage.cache_read_input_tokens
    {
        return Err(ModelFailure::Usage);
    }
    let mut decision = result
        .structured_output
        .ok_or(ModelFailure::InvalidResponse)?;
    // Claude requires one flat schema. Unused variant fields must be empty;
    // remove only those empty arrays before the strict tagged-enum decoder.
    let action = decision
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or(ModelFailure::InvalidResponse)?;
    let unused: &[&str] = match action {
        "request_rows" => &["findings", "unresolved"],
        "finish" => &["row_ids"],
        _ => return Err(ModelFailure::InvalidResponse),
    };
    for field in unused {
        if let Some(value) = decision.get(*field) {
            if value.as_array().is_none_or(|values| !values.is_empty()) {
                return Err(ModelFailure::InvalidResponse);
            }
            decision
                .as_object_mut()
                .ok_or(ModelFailure::InvalidResponse)?
                .remove(*field);
        }
    }
    let decision: ReviewDecision =
        serde_json::from_value(decision).map_err(|_| ModelFailure::InvalidResponse)?;
    let cache_tokens = result
        .usage
        .cache_creation_input_tokens
        .checked_add(result.usage.cache_read_input_tokens)
        .ok_or(ModelFailure::Usage)?;
    let input_tokens = result
        .usage
        .input_tokens
        .checked_add(cache_tokens)
        .ok_or(ModelFailure::Usage)?;
    let total_tokens = input_tokens
        .checked_add(result.usage.output_tokens)
        .ok_or(ModelFailure::Usage)?;
    let usage = ReviewUsage {
        basis: UsageBasis::ClaudeCodeResultV1,
        input_tokens,
        output_tokens: result.usage.output_tokens,
        total_tokens,
        prompt_details: Some(PromptTokenUsage {
            cached_tokens: Some(cache_tokens),
            audio_tokens: None,
        }),
        completion_details: None,
    };
    usage.validate().map_err(|_| ModelFailure::Usage)?;
    if usage.output_tokens > u64::from(max_output_tokens) {
        return Err(ModelFailure::OutputLimit);
    }
    let decision_bytes =
        serde_json::to_vec(&decision).map_err(|_| ModelFailure::InvalidResponse)?;
    Ok(ModelReply {
        response_id: result.session_id,
        // The exact requested alias is retained here for the generic controller's
        // request/reply check. `observed_models` exposes the resolved identities.
        model: expected_model.into(),
        usage,
        decision,
        content_sha256: hash_bytes(&decision_bytes),
        system_fingerprint: None,
        service_tier: None,
    })
}

fn decode_result(bytes: &[u8]) -> std::result::Result<ClaudeResult, ModelFailure> {
    serde_json::from_slice(bytes).map_err(|_| ModelFailure::InvalidResponse)
}

fn validate_observed_models(
    models: &BTreeMap<String, ClaudeModelUsage>,
) -> std::result::Result<(), ModelFailure> {
    if models.len() != 1 {
        return Err(ModelFailure::Identity);
    }
    for (model, usage) in models {
        if validate_identifier(model).is_err() {
            return Err(ModelFailure::Identity);
        }
        usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .and_then(|total| total.checked_add(usage.cache_creation_input_tokens))
            .and_then(|total| total.checked_add(usage.cache_read_input_tokens))
            .ok_or(ModelFailure::Usage)?;
    }
    Ok(())
}

fn validate_requested_model(
    requested: &str,
    observed: &str,
) -> std::result::Result<(), ModelFailure> {
    super::validate_requested_model_selector(requested).map_err(|_| ModelFailure::Identity)?;
    match requested {
        "sonnet" | "haiku" | "opus" | "fable" => {
            let mut prefix = String::with_capacity(requested.len() + "claude--".len());
            prefix.push_str("claude-");
            prefix.push_str(requested);
            prefix.push('-');
            if observed.starts_with(&prefix) {
                Ok(())
            } else {
                Err(ModelFailure::Identity)
            }
        }
        full if full == observed => Ok(()),
        _ => Err(ModelFailure::Identity),
    }
}

fn decode_prepared_call(
    call: &PreparedModelCall,
) -> std::result::Result<PreparedChatRequest, ModelFailure> {
    let prepared: PreparedChatRequest =
        serde_json::from_slice(&call.body).map_err(|_| ModelFailure::InvalidResponse)?;
    if prepared.model != call.expected_model
        || prepared.messages.len() != 2
        || prepared.messages[0].role != "system"
        || prepared.messages[1].role != "user"
        || prepared.stream
        || !prepared.tools.is_empty()
        || prepared.tool_choice != "none"
        || prepared.n != 1
        || prepared.max_completion_tokens != call.max_output_tokens
    {
        return Err(ModelFailure::InvalidResponse);
    }
    Ok(prepared)
}

fn validate_executable(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "Claude Code executable path must be absolute".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path)?;
    let metadata = std::fs::metadata(&canonical)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::Invalid(
            "Claude Code executable must be an executable regular file".into(),
        ));
    }
    Ok(canonical)
}

fn validate_work_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "Claude Code working directory must be absolute".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path)?;
    let metadata = std::fs::metadata(&canonical)?;
    // SAFETY: `geteuid` has no preconditions and only reads the process identity.
    let effective_user = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != effective_user
        || std::fs::read_dir(&canonical)?.next().is_some()
    {
        return Err(Error::Invalid(
            "Claude Code working directory must be empty, private and owned by the current user"
                .into(),
        ));
    }
    if canonical
        .ancestors()
        .any(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
    {
        return Err(Error::Invalid(
            "Claude Code working directory must be outside a Git checkout".into(),
        ));
    }
    Ok(canonical)
}

fn probe_version(executable: &Path, work_dir: &Path) -> Result<String> {
    let output = run_probe(executable, work_dir, &["--version"])?;
    let text = std::str::from_utf8(&output)
        .map_err(|_| Error::Invalid("Claude Code version output must be UTF-8".into()))?
        .trim();
    let version = text
        .strip_suffix(" (Claude Code)")
        .ok_or_else(|| Error::Invalid("unsupported Claude Code version output".into()))?;
    let numbers = version
        .split('.')
        .map(|part| part.parse::<u64>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::Invalid("unsupported Claude Code version output".into()))?;
    if numbers.len() != 3 || (numbers[0], numbers[1], numbers[2]) < MINIMUM_VERSION {
        return Err(Error::Invalid(format!(
            "Claude Code {version} does not support the required restricted unattended profile"
        )));
    }
    Ok(version.into())
}

fn probe_auth(executable: &Path, work_dir: &Path) -> Result<String> {
    let output = run_probe(executable, work_dir, &["auth", "status"])?;
    let status: AuthStatus = serde_json::from_slice(&output)
        .map_err(|_| Error::Invalid("Claude Code auth status is invalid".into()))?;
    if !status.logged_in || !matches!(status.auth_method.as_str(), "claude.ai" | "oauth_token") {
        return Err(Error::Invalid(
            "Claude Code must use an existing Claude subscription login".into(),
        ));
    }
    Ok(status.auth_method)
}

fn run_probe(executable: &Path, work_dir: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new(executable);
    configure_command(&mut command, work_dir);
    command.args(arguments);
    let capture = capture_with_input(
        &mut command,
        None,
        PROBE_TIMEOUT,
        PROBE_OUTPUT_BYTES,
        &Cancellation::default(),
    )?;
    if capture.stop.is_some() || capture.code != Some(0) || !capture.stderr.is_empty() {
        return Err(Error::Invalid("Claude Code probe failed".into()));
    }
    Ok(capture.stdout)
}

fn configure_command(command: &mut Command, work_dir: &Path) {
    command.env_clear().current_dir(work_dir).process_group(0);
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "CLAUDE_CONFIG_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs([
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("DISABLE_UPDATES", "1"),
        ("DISABLE_EXTRA_USAGE_COMMAND", "1"),
        ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ("CLAUDE_CODE_DISABLE_POLICY_SKILLS", "1"),
        ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
        ("CLAUDE_CODE_AUTO_CONNECT_IDE", "false"),
        ("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1"),
        ("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS", "1"),
        ("CLAUDE_CODE_MAX_RETRIES", "0"),
        ("MAX_STRUCTURED_OUTPUT_RETRIES", "0"),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_scope_binds_prompt_profile_and_decision_schema() {
        let current = scope_digest(
            "/controller/claude",
            "2.1.300",
            "claude.ai",
            SCOPE_PROFILE,
            DECISION_SCHEMA,
        )
        .unwrap();
        let old_profile = scope_digest(
            "/controller/claude",
            "2.1.300",
            "claude.ai",
            "restricted-safe-mode-no-tools/v1",
            DECISION_SCHEMA,
        )
        .unwrap();
        let changed_schema = scope_digest(
            "/controller/claude",
            "2.1.300",
            "claude.ai",
            SCOPE_PROFILE,
            r#"{"type":"object","properties":{}}"#,
        )
        .unwrap();

        assert_ne!(current, old_profile);
        assert_ne!(current, changed_schema);
    }
}

fn executable_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(validate_executable(path)?)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod executable_identity_tests {
    use super::*;

    #[test]
    fn concrete_transport_hashes_bytes_and_rejects_replaced_executable() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.path().join("claude-fixture");
        let script = b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.300 (Claude Code)'; else echo '{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}'; fi\n";
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let transport = ClaudeCodeTransport::new(&executable, &work).unwrap();
        assert_eq!(
            transport.verified_executable_sha256().unwrap(),
            hash_bytes(script)
        );
        std::fs::write(&executable, b"#!/bin/sh\nexit 1\n").unwrap();
        assert!(transport.verified_executable_sha256().is_err());
    }
}
