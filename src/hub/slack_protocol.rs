//! Bounded parsing and authentication for Slack HTTP requests.

use std::collections::BTreeMap;
use std::fmt;

use ring::hmac;
use serde_json::{Value, json};

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_FORM_FIELDS: usize = 64;
pub const MAX_FORM_KEY_BYTES: usize = 128;
pub const MAX_FORM_VALUE_BYTES: usize = MAX_BODY_BYTES;
pub const MAX_BLOCK_TEXT_CHARS: usize = 3_000;
pub const REPLAY_WINDOW_SECONDS: u64 = 300;

const SIGNED_VERSION_PREFIX: &str = "v0:";
const SIGNATURE_HEADER_PREFIX: &str = "v0=";
const SHA256_HEX_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    BodyTooLarge,
    DuplicateField,
    FieldTooLarge,
    InvalidSignature,
    InvalidTimestamp,
    InvalidUtf8,
    MalformedForm,
    StaleTimestamp,
    TooManyFields,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BodyTooLarge => "request body is too large",
            Self::DuplicateField => "form contains a duplicate field",
            Self::FieldTooLarge => "form field is too large",
            Self::InvalidSignature => "invalid request signature",
            Self::InvalidTimestamp => "invalid request timestamp",
            Self::InvalidUtf8 => "form field is not valid UTF-8",
            Self::MalformedForm => "malformed form body",
            Self::StaleTimestamp => "request timestamp is outside the allowed window",
            Self::TooManyFields => "form contains too many fields",
        })
    }
}

impl std::error::Error for ProtocolError {}

/// Verifies Slack's `v0` HMAC over the timestamp and the exact request bytes.
///
/// Callers must pass the header values without normalization and retain the raw
/// body until verification completes. Timestamps on either side of `now` are
/// accepted only within the replay window.
pub fn verify_request(
    signing_secret: &str,
    timestamp_header: &str,
    signature_header: &str,
    raw_body: &[u8],
    now: i64,
) -> Result<(), ProtocolError> {
    if raw_body.len() > MAX_BODY_BYTES {
        return Err(ProtocolError::BodyTooLarge);
    }
    if signing_secret.is_empty() {
        return Err(ProtocolError::InvalidSignature);
    }

    let timestamp = parse_timestamp(timestamp_header)?;
    if now < 0 {
        return Err(ProtocolError::InvalidTimestamp);
    }
    if timestamp.abs_diff(now) > REPLAY_WINDOW_SECONDS {
        return Err(ProtocolError::StaleTimestamp);
    }

    let signature = decode_signature(signature_header)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, signing_secret.as_bytes());
    hmac::verify(&key, &signed(timestamp_header, raw_body), &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

/// Signs a request as Slack does: the `x-slack-signature` value `verify_request` accepts.
pub fn sign_request(signing_secret: &str, timestamp: &str, raw_body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, signing_secret.as_bytes());
    let tag = hmac::sign(&key, &signed(timestamp, raw_body));
    format!(
        "{SIGNATURE_HEADER_PREFIX}{}",
        crate::team::auth::lower_hex(tag.as_ref())
    )
}

fn signed(timestamp: &str, raw_body: &[u8]) -> Vec<u8> {
    let mut signed =
        Vec::with_capacity(SIGNED_VERSION_PREFIX.len() + timestamp.len() + 1 + raw_body.len());
    signed.extend_from_slice(SIGNED_VERSION_PREFIX.as_bytes());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b':');
    signed.extend_from_slice(raw_body);
    signed
}

fn parse_timestamp(timestamp: &str) -> Result<i64, ProtocolError> {
    if timestamp.is_empty()
        || timestamp.len() > 19
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ProtocolError::InvalidTimestamp);
    }
    timestamp
        .parse::<i64>()
        .map_err(|_| ProtocolError::InvalidTimestamp)
}

fn decode_signature(signature: &str) -> Result<[u8; 32], ProtocolError> {
    let Some(hex) = signature.strip_prefix(SIGNATURE_HEADER_PREFIX) else {
        return Err(ProtocolError::InvalidSignature);
    };
    if hex.len() != SHA256_HEX_BYTES
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ProtocolError::InvalidSignature);
    }

    let mut decoded = [0_u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_value(pair[0]).ok_or(ProtocolError::InvalidSignature)? << 4)
            | hex_value(pair[1]).ok_or(ProtocolError::InvalidSignature)?;
    }
    Ok(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Parses a bounded `application/x-www-form-urlencoded` Slack body.
///
/// Field names are compared after decoding, so differently encoded duplicate
/// names are rejected. Empty values are accepted; empty names and pairs without
/// `=` are rejected.
pub fn parse_form(body: &[u8]) -> Result<BTreeMap<String, String>, ProtocolError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ProtocolError::BodyTooLarge);
    }
    if body.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut fields = BTreeMap::new();
    for pair in body.split(|byte| *byte == b'&') {
        if fields.len() == MAX_FORM_FIELDS {
            return Err(ProtocolError::TooManyFields);
        }
        let Some(separator) = pair.iter().position(|byte| *byte == b'=') else {
            return Err(ProtocolError::MalformedForm);
        };
        let name = decode_form_component(&pair[..separator], MAX_FORM_KEY_BYTES)?;
        if name.is_empty() {
            return Err(ProtocolError::MalformedForm);
        }
        let value = decode_form_component(&pair[separator + 1..], MAX_FORM_VALUE_BYTES)?;
        if fields.insert(name, value).is_some() {
            return Err(ProtocolError::DuplicateField);
        }
    }
    Ok(fields)
}

fn decode_form_component(encoded: &[u8], limit: usize) -> Result<String, ProtocolError> {
    let mut decoded = Vec::with_capacity(encoded.len().min(limit));
    let mut index = 0;
    while index < encoded.len() {
        if decoded.len() == limit {
            return Err(ProtocolError::FieldTooLarge);
        }
        match encoded[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                let high = encoded
                    .get(index + 1)
                    .and_then(|byte| form_hex_value(*byte))
                    .ok_or(ProtocolError::MalformedForm)?;
                let low = encoded
                    .get(index + 2)
                    .and_then(|byte| form_hex_value(*byte))
                    .ok_or(ProtocolError::MalformedForm)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| ProtocolError::InvalidUtf8)
}

fn form_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Returns a Block Kit `plain_text` object with bounded, control-free text.
///
/// The object type is fixed rather than caller-controlled so Slack does not
/// interpret user text as `mrkdwn`.
pub fn plain_text(input: &str, max_chars: usize) -> Value {
    json!({
        "type": "plain_text",
        "text": clip_plain_text(input, max_chars),
        "emoji": true,
    })
}

pub fn clip_plain_text(input: &str, max_chars: usize) -> String {
    let limit = max_chars.min(MAX_BLOCK_TEXT_CHARS);
    let mut output = String::new();
    let mut chars = input.chars();

    for _ in 0..limit {
        let Some(character) = chars.next() else {
            return output;
        };
        output.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    if chars.next().is_some() && limit > 0 {
        output.pop();
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "synthetic-signing-secret";
    const NOW: i64 = 1_800_000_000;

    fn signature(timestamp: &str, body: &[u8]) -> String {
        let mut signed = Vec::new();
        signed.extend_from_slice(b"v0:");
        signed.extend_from_slice(timestamp.as_bytes());
        signed.push(b':');
        signed.extend_from_slice(body);
        let key = hmac::Key::new(hmac::HMAC_SHA256, SECRET.as_bytes());
        let bytes = hmac::sign(&key, &signed);
        let mut result = String::from("v0=");
        for byte in bytes.as_ref() {
            use std::fmt::Write as _;
            write!(&mut result, "{byte:02x}").unwrap();
        }
        result
    }

    #[test]
    fn signs_requests_as_slack_does() {
        let timestamp = NOW.to_string();
        let body = br#"{"type":"url_verification","challenge":"c"}"#;
        let signed = sign_request(SECRET, &timestamp, body);
        assert_eq!(signed, signature(&timestamp, body));
        assert_eq!(
            verify_request(SECRET, &timestamp, &signed, body, NOW),
            Ok(())
        );
        assert_eq!(
            verify_request("other-secret", &timestamp, &signed, body, NOW),
            Err(ProtocolError::InvalidSignature)
        );
    }

    #[test]
    fn verifies_exact_raw_body() {
        let timestamp = NOW.to_string();
        let body = b"command=%2Fagentscm&text=work+wrk_0123456789abcdef0123456789abcdef";
        let signature = signature(&timestamp, body);

        assert_eq!(
            verify_request(SECRET, &timestamp, &signature, body, NOW),
            Ok(())
        );
        assert_eq!(
            verify_request(
                SECRET,
                &timestamp,
                &signature,
                b"command=/agentscm&text=work+wrk_0123456789abcdef0123456789abcdef",
                NOW,
            ),
            Err(ProtocolError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_replayed_and_future_requests_without_overflow() {
        for signed_at in [NOW - 300, NOW + 300] {
            let timestamp = signed_at.to_string();
            assert_eq!(
                verify_request(SECRET, &timestamp, &signature(&timestamp, b""), b"", NOW),
                Ok(())
            );
        }
        for signed_at in [NOW - 301, NOW + 301, i64::MAX] {
            let timestamp = signed_at.to_string();
            assert_eq!(
                verify_request(SECRET, &timestamp, &signature(&timestamp, b""), b"", NOW),
                Err(ProtocolError::StaleTimestamp)
            );
        }
        assert_eq!(
            verify_request(SECRET, "0", &signature("0", b""), b"", i64::MAX),
            Err(ProtocolError::StaleTimestamp)
        );
    }

    #[test]
    fn rejects_forged_malformed_and_oversized_signed_requests() {
        let timestamp = NOW.to_string();
        let valid = signature(&timestamp, b"body");
        let uppercase = valid.to_ascii_uppercase().replacen("V0=", "v0=", 1);

        assert_eq!(
            verify_request(
                SECRET,
                &timestamp,
                &signature(&timestamp, b"other"),
                b"body",
                NOW
            ),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_request(SECRET, &timestamp, &uppercase, b"body", NOW),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_request(SECRET, "-1", &valid, b"body", NOW),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(
            verify_request(
                SECRET,
                &timestamp,
                &valid,
                &vec![b'x'; MAX_BODY_BYTES + 1],
                NOW,
            ),
            Err(ProtocolError::BodyTooLarge)
        );
    }

    #[test]
    fn parses_strict_form_with_unicode() {
        let fields = parse_form(b"command=%2Fagentscm&text=hello+%E2%9C%93&empty=").unwrap();
        assert_eq!(fields.get("command").map(String::as_str), Some("/agentscm"));
        assert_eq!(fields.get("text").map(String::as_str), Some("hello ✓"));
        assert_eq!(fields.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn rejects_duplicate_malformed_and_invalid_utf8_form_fields() {
        for (body, expected) in [
            (
                b"team_id=one&%74eam_id=two".as_slice(),
                ProtocolError::DuplicateField,
            ),
            (b"text=%".as_slice(), ProtocolError::MalformedForm),
            (b"text=%GG".as_slice(), ProtocolError::MalformedForm),
            (b"text=%FF".as_slice(), ProtocolError::InvalidUtf8),
            (b"missing_equals".as_slice(), ProtocolError::MalformedForm),
            (b"=empty_name".as_slice(), ProtocolError::MalformedForm),
            (b"first=ok&".as_slice(), ProtocolError::MalformedForm),
        ] {
            assert_eq!(parse_form(body), Err(expected));
        }
    }

    #[test]
    fn bounds_form_body_field_count_and_field_size() {
        assert_eq!(
            parse_form(&vec![b'x'; MAX_BODY_BYTES + 1]),
            Err(ProtocolError::BodyTooLarge)
        );
        let too_many = (0..=MAX_FORM_FIELDS)
            .map(|index| format!("f{index}=x"))
            .collect::<Vec<_>>()
            .join("&");
        assert_eq!(
            parse_form(too_many.as_bytes()),
            Err(ProtocolError::TooManyFields)
        );
        let long_key = format!("{}=x", "k".repeat(MAX_FORM_KEY_BYTES + 1));
        assert_eq!(
            parse_form(long_key.as_bytes()),
            Err(ProtocolError::FieldTooLarge)
        );
    }

    #[test]
    fn builds_control_free_unicode_plain_text() {
        assert_eq!(clip_plain_text("a\n🙂bc", 4), "a 🙂…");
        assert_eq!(clip_plain_text("🙂🙃", 2), "🙂🙃");
        assert_eq!(clip_plain_text("abc", 0), "");

        let object = plain_text("*not bold* <@U123>", 100);
        assert_eq!(object["type"], "plain_text");
        assert_eq!(object["text"], "*not bold* <@U123>");
        assert_eq!(object.get("mrkdwn"), None);
    }
}
