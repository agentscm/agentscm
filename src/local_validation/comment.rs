//! Forge-neutral Markdown carrier for an attestation. The signed blocks decide;
//! the summary is display only. Bytes travel as base64 so Markdown cannot alter them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use super::{MAX_RECORD_BYTES, ValidationRecord};
use crate::evidence::validate_lower_hex;
use crate::{Error, Result};

pub const MAX_COMMENT_BYTES: usize = 60 * 1024;
/// Every attestation comment starts with this.
pub const MARKER_PREFIX: &str = "<!-- agentscm-local-validation:v1 ";
const ENVELOPE_FENCE: &str = "```agentscm-envelope-v1";
const RECORD_FENCE: &str = "```agentscm-record-v1";
const LOGO_URL: &str = "https://raw.githubusercontent.com/counterbranch/agentscm/24b70c0796e8288d1560adda36a1590e613d0ffc/docs/assets/agentscm-icon.png";

pub fn envelope_sha256(envelope: &[u8]) -> String {
    format!("{:x}", Sha256::digest(envelope))
}

fn block(fence: &str, bytes: &[u8]) -> String {
    let encoded = STANDARD.encode(bytes);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    format!("{fence}\n{}\n```\n", lines.join("\n"))
}

/// Escapes display-only text before placing it in Markdown or HTML. Encoding every
/// non-alphanumeric character also prevents Markdown delimiters and newlines from
/// changing the surrounding disclosure structure.
fn display_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == ' ' {
            escaped.push(character);
        } else {
            write!(escaped, "&#x{:x};", character as u32).expect("writing to a string cannot fail");
        }
    }
    escaped
}

fn code(value: &str) -> String {
    format!("<code>{}</code>", display_text(value))
}

/// Deterministic comment body for the exact envelope and record bytes.
pub fn render(envelope: &[u8], record: &[u8]) -> Result<String> {
    let parsed = ValidationRecord::from_slice(record)?;
    let envelope_json: serde_json::Value = serde_json::from_slice(envelope)?;
    let outcome = match envelope_json["payload"]["outcome"].as_str() {
        Some("pass") => "pass",
        Some("fail") => "fail",
        _ => "unknown",
    };
    let omissions: Vec<&str> = envelope_json["payload"]["coverage"]["omissions"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let observed = envelope_json["payload"]["coverage"]["observed_count"]
        .as_u64()
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    let expected = envelope_json["payload"]["coverage"]["expected_count"]
        .as_u64()
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    let omission_count = omissions.len();
    let omission_details = if omissions.is_empty() {
        "None reported.\n".to_owned()
    } else {
        omissions
            .iter()
            .map(|value| format!("- {}\n", code(value)))
            .collect()
    };
    let missing_services = [
        (!parsed.services.postgres).then_some("PostgreSQL"),
        (!parsed.services.s3).then_some("S3"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let missing_services = if missing_services.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n&#x26a0;&#xfe0f; Missing services: {}. Coverage is incomplete.",
            missing_services.join(", ")
        )
    };
    let short_commit: String = parsed.subject.commit.chars().take(12).collect();
    let mut body = format!(
        "{MARKER_PREFIX}{} -->\n\n<img src=\"{LOGO_URL}\" alt=\"AgentSCM icon\" width=\"28\"> **Local validation: reported {outcome}**\n\nSource: {} at {}\n\nWorker-reported; the verifier checks the signature and source bindings, not whether commands ran.\n\nCoverage: {observed}/{expected} checks reported; {omission_count} omissions.{missing_services}\n\n<details>\n<summary>Coverage omissions ({omission_count})</summary>\n\n{omission_details}\n</details>\n\n<details>\n<summary>Source and environment</summary>\n\n- Repository: {}\n- Commit: {}\n- Tree: {}\n- Base ref: {}\n- Base tip: {}\n- Merge base: {}\n- Services: PostgreSQL={}, S3={}\n- Platform: {}\n- Toolchain: {}\n\n</details>\n\n<details>\n<summary>Command results</summary>\n\n",
        envelope_sha256(envelope),
        code(&parsed.subject.repository),
        code(&short_commit),
        code(&parsed.subject.repository),
        code(&parsed.subject.commit),
        code(&parsed.subject.tree),
        code(&parsed.context.base_ref),
        code(&parsed.base_tip),
        code(&parsed.context.merge_base),
        parsed.services.postgres,
        parsed.services.s3,
        code(&parsed.platform),
        parsed
            .toolchain
            .iter()
            .map(|value| code(value))
            .collect::<Vec<_>>()
            .join(", "),
    );
    for r in &parsed.results {
        let exit = r.exit_code.map_or("none".to_owned(), |c| c.to_string());
        body.push_str(&format!(
            "- {}: {:?}, exit {exit}, {} ms\n",
            code(&r.id),
            r.stop,
            r.duration_ms
        ));
    }
    if !parsed.not_required.is_empty() {
        let ids: Vec<String> = parsed.not_required.iter().map(|id| code(id)).collect();
        body.push_str(&format!(
            "- Not required by the changed paths: {}\n",
            ids.join(", ")
        ));
    }
    body.push_str("\n</details>\n\n<details>\n<summary>Signed machine-readable evidence</summary>\n\nThe exact signed bytes are preserved below for verification and audit tooling.\n\n");
    body.push_str(&block(ENVELOPE_FENCE, envelope));
    body.push('\n');
    body.push_str(&block(RECORD_FENCE, record));
    body.push_str("\n</details>\n");
    if body.len() > MAX_COMMENT_BYTES {
        return Err(Error::Limit(format!(
            "comment body exceeds {MAX_COMMENT_BYTES} bytes"
        )));
    }
    Ok(body)
}

/// Strictly extracts `(envelope, record)`: marker first, exactly one block of
/// each kind, and a marker digest equal to the envelope bytes' SHA-256.
pub fn parse(body: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let invalid = |m: &str| Error::Invalid(format!("comment body: {m}"));
    if body.len() > MAX_COMMENT_BYTES {
        return Err(invalid("too large"));
    }
    let mut lines = body.lines();
    let marker = lines
        .next()
        .and_then(|l| l.strip_prefix(MARKER_PREFIX))
        .and_then(|l| l.strip_suffix(" -->"))
        .ok_or_else(|| invalid("missing marker"))?;
    validate_lower_hex("marker digest", marker, 32)?;
    let (mut envelope, mut record): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut open: Option<(bool, String)> = None;
    for line in lines {
        match (&mut open, line) {
            (None, ENVELOPE_FENCE) => open = Some((true, String::new())),
            (None, RECORD_FENCE) => open = Some((false, String::new())),
            (Some((is_envelope, text)), "```") => {
                let bytes = STANDARD.decode(&*text).map_err(|_| invalid("bad base64"))?;
                if bytes.len() > MAX_RECORD_BYTES {
                    return Err(invalid("block too large"));
                }
                let slot = if *is_envelope {
                    &mut envelope
                } else {
                    &mut record
                };
                if slot.replace(bytes).is_some() {
                    return Err(invalid("duplicate block"));
                }
                open = None;
            }
            (Some((_, text)), data) => text.push_str(data),
            (None, _) => {}
        }
    }
    let (Some(envelope), Some(record), None) = (envelope, record, open) else {
        return Err(invalid(
            "expected exactly one envelope and one record block",
        ));
    };
    if envelope_sha256(&envelope) != marker {
        return Err(invalid("marker digest does not match the envelope"));
    }
    Ok((envelope, record))
}
