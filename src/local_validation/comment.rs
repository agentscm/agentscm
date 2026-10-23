//! Forge-neutral Markdown carrier for an attestation. The signed blocks decide;
//! the summary is display only. Bytes travel as base64 so Markdown cannot alter them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

use super::{FIXED_OMISSIONS, MAX_RECORD_BYTES, ValidationRecord};
use crate::evidence::validate_lower_hex;
use crate::{Error, Result};

pub const MAX_COMMENT_BYTES: usize = 60 * 1024;
const MARKER_PREFIX: &str = "<!-- agentscm-local-validation:v1 ";
const ENVELOPE_FENCE: &str = "```agentscm-envelope-v1";
const RECORD_FENCE: &str = "```agentscm-record-v1";

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

/// Deterministic comment body for the exact envelope and record bytes.
pub fn render(envelope: &[u8], record: &[u8]) -> Result<String> {
    let parsed = ValidationRecord::from_slice(record)?;
    let envelope_json: serde_json::Value = serde_json::from_slice(envelope)?;
    let outcome = envelope_json["payload"]["outcome"]
        .as_str()
        .unwrap_or("unknown");
    let mut omissions: Vec<&str> = FIXED_OMISSIONS.to_vec();
    if !parsed.services.postgres {
        omissions.push("postgres tests");
    }
    if !parsed.services.s3 {
        omissions.push("s3 tests");
    }
    let mut body = format!(
        "{MARKER_PREFIX}{} -->\n\n**Local validation: {outcome}**\n\n- commit: `{}`\n- tree: `{}`\n- base: `{}` (merge-base `{}`)\n- services: postgres={}, s3={}\n- omissions: {}\n",
        envelope_sha256(envelope),
        parsed.subject.commit,
        parsed.subject.tree,
        parsed.context.base_ref,
        parsed.context.merge_base,
        parsed.services.postgres,
        parsed.services.s3,
        omissions.join(", "),
    );
    for r in &parsed.results {
        body.push_str(&format!(
            "- `{}`: {:?}, exit {:?}, {} ms\n",
            r.id, r.stop, r.exit_code, r.duration_ms
        ));
    }
    body.push_str("\nThis summary is display only; the signed blocks below decide.\n\n");
    body.push_str(&block(ENVELOPE_FENCE, envelope));
    body.push('\n');
    body.push_str(&block(RECORD_FENCE, record));
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
