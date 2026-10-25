//! Best-effort secret redaction for conversation bodies that may leave the machine.
//!
//! This scrubber recognizes well-known credential shapes (cloud and forge tokens, JWTs, bearer
//! credentials, PEM private keys), values of secret-named JSON fields and `NAME=value` style
//! assignments in free text. It is heuristic: it cannot guarantee that every secret is removed,
//! and it cannot recognize a secret that has no distinctive shape. Replacements are the fixed
//! string `[REDACTED:<kind>]`, so output is deterministic and byte-stable. Scanning is a single
//! forward pass over bytes with only ASCII match boundaries, so it is linear-ish, UTF-8 safe and
//! panic-free on arbitrary input.

use serde_json::Value;

/// Normalized (lowercase, no `_`/`-`) name suffixes that mark a field or variable as secret.
/// Suffix matching keeps counters such as `max_tokens` and `input_tokens` readable.
const SECRET_WORDS: [&str; 15] = [
    "password",
    "passwords",
    "passwd",
    "secret",
    "secrets",
    "token",
    "apikey",
    "accesskey",
    "privatekey",
    "authorization",
    "cookie",
    "cookies",
    "sessionkey",
    "credential",
    "credentials",
];

const FIELD_REPLACEMENT: &str = "[REDACTED:field]";
const REDACTED_PREFIX: &str = "[REDACTED:";

/// Redacts secrets in place and returns the number of replacements made.
pub fn redact_json(value: &mut Value) -> usize {
    match value {
        Value::String(text) => match redact_text(text) {
            Some((redacted, count)) => {
                *text = redacted;
                count
            }
            None => 0,
        },
        Value::Array(items) => items.iter_mut().map(redact_json).sum(),
        Value::Object(map) => {
            let mut count = 0;
            for (key, item) in map.iter_mut() {
                let secret_field = is_secret_name(key.as_bytes())
                    && match item {
                        Value::String(text) => text != FIELD_REPLACEMENT,
                        Value::Number(_) => true,
                        _ => false,
                    };
                if secret_field {
                    *item = Value::String(FIELD_REPLACEMENT.to_owned());
                    count += 1;
                } else {
                    count += redact_json(item);
                }
            }
            count
        }
        _ => 0,
    }
}

/// Returns the redacted text and replacement count, or `None` when nothing matched.
fn redact_text(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut count = 0;
    let mut replace = |out: &mut String, copied: &mut usize, from: usize, to: usize, kind: &str| {
        out.push_str(&text[*copied..from]);
        out.push_str(REDACTED_PREFIX);
        out.push_str(kind);
        out.push(']');
        *copied = to;
        count += 1;
    };
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-'
            && bytes[i..].starts_with(b"-----BEGIN ")
            && let Some(end) = pem_block_end(bytes, i)
        {
            replace(&mut out, &mut copied, i, end, "private_key");
            i = end;
            continue;
        }
        if is_ident(bytes[i]) && (i == 0 || !is_ident(bytes[i - 1])) {
            let name_end = run_end(bytes, i, is_ident);
            if let Some((from, end, kind)) = token_at(bytes, i) {
                replace(&mut out, &mut copied, from, end, kind);
                i = end;
            } else if let Some((from, end)) = assignment_value(bytes, i, name_end) {
                replace(&mut out, &mut copied, from, end, "assignment");
                i = end;
            } else {
                i = name_end;
            }
            continue;
        }
        i += 1;
    }
    if count == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some((out, count))
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn run_end(bytes: &[u8], from: usize, class: fn(u8) -> bool) -> usize {
    bytes[from..]
        .iter()
        .position(|byte| !class(*byte))
        .map_or(bytes.len(), |offset| from + offset)
}

/// Length of the run of `class` bytes starting at `from`.
fn run_len(bytes: &[u8], from: usize, class: fn(u8) -> bool) -> usize {
    run_end(bytes, from, class) - from
}

fn is_alnum_underscore(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_alnum(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

fn is_upper_alnum(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

fn is_bearer_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

fn is_slack(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

/// Matches a known token shape that starts at `i`. Returns the replaced range and its kind.
fn token_at(bytes: &[u8], i: usize) -> Option<(usize, usize, &'static str)> {
    let rest = &bytes[i..];
    let prefixed = |prefix: &[u8], class: fn(u8) -> bool, min: usize| {
        if !rest.starts_with(prefix) {
            return None;
        }
        let tail = run_len(bytes, i + prefix.len(), class);
        (tail >= min).then_some(i + prefix.len() + tail)
    };
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        let tail = run_len(bytes, i + 4, is_alnum);
        let id = &bytes[i + 4..i + 4 + tail];
        return (tail == 16 && id.iter().all(|byte| is_upper_alnum(*byte))).then_some((
            i,
            i + 20,
            "aws_access_key_id",
        ));
    }
    for prefix in [
        &b"ghp_"[..],
        b"gho_",
        b"ghu_",
        b"ghs_",
        b"ghr_",
        b"github_pat_",
    ] {
        if let Some(end) = prefixed(prefix, is_alnum_underscore, 20) {
            return Some((i, end, "github_token"));
        }
    }
    if let Some(end) = prefixed(b"glpat-", is_ident, 20) {
        return Some((i, end, "gitlab_token"));
    }
    if rest.len() > 5
        && rest.starts_with(b"xox")
        && matches!(rest[3], b'a' | b'b' | b'p' | b'r' | b's')
        && rest[4] == b'-'
        && let Some(end) = prefixed(&rest[..5], is_slack, 10)
    {
        return Some((i, end, "slack_token"));
    }
    if let Some(end) = prefixed(b"sk-ant-", is_ident, 20) {
        return Some((i, end, "anthropic_key"));
    }
    if let Some(end) = prefixed(b"sk-", is_ident, 20) {
        return Some((i, end, "openai_key"));
    }
    for prefix in [&b"sk_live_"[..], b"rk_live_"] {
        if let Some(end) = prefixed(prefix, is_alnum, 10) {
            return Some((i, end, "stripe_key"));
        }
    }
    if let Some(end) = prefixed(b"AIza", is_ident, 35) {
        return Some((i, end, "google_api_key"));
    }
    if rest.starts_with(b"eyJ")
        && let Some(end) = jwt_end(bytes, i)
    {
        return Some((i, end, "jwt"));
    }
    if rest.len() > 7 && rest[..6].eq_ignore_ascii_case(b"bearer") && rest[6] == b' ' {
        let token = &bytes[i + 7..i + 7 + run_len(bytes, i + 7, is_bearer_token)];
        // A trailing dot is sentence punctuation, not part of the credential.
        let tail = token.len() - token.iter().rev().take_while(|byte| **byte == b'.').count();
        if tail >= 16 {
            return Some((i + 7, i + 7 + tail, "bearer"));
        }
    }
    None
}

/// Three dot-separated base64url segments, each at least ten bytes.
fn jwt_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    for segment in 0..3 {
        let len = run_len(bytes, pos, is_ident);
        if len < 10 {
            return None;
        }
        pos += len;
        if segment < 2 {
            if bytes.get(pos) != Some(&b'.') {
                return None;
            }
            pos += 1;
        }
    }
    Some(pos)
}

/// End of a `-----BEGIN ... PRIVATE KEY-----` block, or the end of input when it is truncated.
fn pem_block_end(bytes: &[u8], start: usize) -> Option<usize> {
    let label_start = start + b"-----BEGIN ".len();
    let label_end = find(bytes, label_start, b"-----")?;
    if !bytes[label_start..label_end].ends_with(b"PRIVATE KEY") || label_end - label_start > 64 {
        return None;
    }
    let after_begin = label_end + 5;
    Some(
        find(bytes, after_begin, b"-----END ")
            .and_then(|end| find(bytes, end + 9, b"-----").map(|close| close + 5))
            .unwrap_or(bytes.len()),
    )
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// For a secret-named identifier run `[start, name_end)` followed by `=` or `:`, returns the
/// value range to replace.
fn assignment_value(bytes: &[u8], start: usize, name_end: usize) -> Option<(usize, usize)> {
    if !is_secret_name(&bytes[start..name_end]) {
        return None;
    }
    let blank = |pos: usize| matches!(bytes.get(pos), Some(b' ' | b'\t'));
    let mut pos = name_end;
    if matches!(bytes.get(pos), Some(b'"' | b'\'')) {
        pos += 1;
    }
    while blank(pos) {
        pos += 1;
    }
    if !matches!(bytes.get(pos), Some(b'=' | b':')) {
        return None;
    }
    pos += 1;
    while blank(pos) {
        pos += 1;
    }
    let quote = bytes
        .get(pos)
        .copied()
        .filter(|byte| matches!(byte, b'"' | b'\''));
    let value_start = pos + usize::from(quote.is_some());
    let end = bytes[value_start..]
        .iter()
        .position(|byte| match quote {
            Some(quote) => *byte == quote || *byte == b'\n',
            None => byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'`'),
        })
        .map_or(bytes.len(), |offset| value_start + offset);
    if end == value_start || bytes[value_start..end].starts_with(REDACTED_PREFIX.as_bytes()) {
        return None;
    }
    let value = &bytes[value_start..end];
    if quote.is_none()
        && (value.eq_ignore_ascii_case(b"bearer") || value.eq_ignore_ascii_case(b"basic"))
        && blank(end)
    {
        let mut credential = end;
        while blank(credential) {
            credential += 1;
        }
        let tail = run_len(bytes, credential, is_bearer_token);
        if tail > 0 {
            return Some((value_start, credential + tail));
        }
    }
    Some((value_start, end))
}

fn is_secret_name(name: &[u8]) -> bool {
    let normalized: Vec<u8> = name
        .iter()
        .filter(|byte| !matches!(byte, b'_' | b'-'))
        .map(u8::to_ascii_lowercase)
        .collect();
    SECRET_WORDS
        .iter()
        .any(|word| normalized.ends_with(word.as_bytes()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn redact(input: &str) -> (String, usize) {
        let mut value = Value::String(input.to_owned());
        let count = redact_json(&mut value);
        (value.as_str().unwrap().to_owned(), count)
    }

    fn repeat(ch: char, count: usize) -> String {
        ch.to_string().repeat(count)
    }

    #[test]
    fn redacts_each_known_token_shape() {
        let cases = [
            (
                ["AK", "IA"].concat() + &repeat('A', 16),
                "aws_access_key_id",
            ),
            (
                ["AS", "IA"].concat() + &repeat('Z', 16),
                "aws_access_key_id",
            ),
            (["gh", "p_"].concat() + &repeat('a', 36), "github_token"),
            (["gh", "o_"].concat() + &repeat('b', 30), "github_token"),
            (
                ["github", "_pat_"].concat() + &repeat('c', 40),
                "github_token",
            ),
            (["gl", "pat-"].concat() + &repeat('d', 20), "gitlab_token"),
            (["xo", "xb-"].concat() + &repeat('1', 24), "slack_token"),
            (
                ["sk", "-ant-api03-"].concat() + &repeat('e', 30),
                "anthropic_key",
            ),
            (["sk", "-proj-"].concat() + &repeat('f', 30), "openai_key"),
            (["sk", "-"].concat() + &repeat('g', 32), "openai_key"),
            (["sk", "_live_"].concat() + &repeat('h', 24), "stripe_key"),
            (["rk", "_live_"].concat() + &repeat('i', 24), "stripe_key"),
            (["AI", "za"].concat() + &repeat('j', 35), "google_api_key"),
            (
                [
                    "ey",
                    "J",
                    &repeat('k', 12),
                    ".",
                    &repeat('l', 20),
                    ".",
                    &repeat('m', 20),
                ]
                .concat(),
                "jwt",
            ),
        ];
        for (secret, kind) in cases {
            let (out, count) = redact(&format!("before {secret} after"));
            assert_eq!(out, format!("before [REDACTED:{kind}] after"), "{kind}");
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn keeps_bearer_prefix_and_redacts_pem_blocks() {
        let token = repeat('t', 30);
        assert_eq!(
            redact(&format!("Authorization-ish Bearer {token}.")),
            ("Authorization-ish Bearer [REDACTED:bearer].".to_owned(), 1)
        );
        let begin = ["-----BEGIN ", "RSA PRIVATE", " KEY-----"].concat();
        let end = ["-----END ", "RSA PRIVATE", " KEY-----"].concat();
        let (out, count) = redact(&format!("a\n{begin}\nQUJD\nREVG\n{end}\nb"));
        assert_eq!((out.as_str(), count), ("a\n[REDACTED:private_key]\nb", 1));
        let (out, _) = redact(&format!("a {begin}\nQUJD"));
        assert_eq!(out, "a [REDACTED:private_key]");
        assert_eq!(redact("-----BEGIN CERTIFICATE-----\nabc").1, 0);
    }

    #[test]
    fn redacts_assignment_values_only() {
        let name = ["pass", "word"].concat();
        let (out, count) = redact(&format!(
            "run {name}=hunter2 now and {name}: swordfish\nnext"
        ));
        assert_eq!(
            out,
            "run password=[REDACTED:assignment] now and password: [REDACTED:assignment]\nnext"
        );
        assert_eq!(count, 2);
        let (out, _) = redact(r#"{"api_key": "abc def", "x": 1}"#);
        assert_eq!(out, r#"{"api_key": "[REDACTED:assignment]", "x": 1}"#);
        let (out, _) = redact("Authorization: Bearer abc123");
        assert_eq!(out, "Authorization: [REDACTED:assignment]");
        let (out, _) = redact("export AWS_SECRET_ACCESS_KEY='abc/def+ghi'");
        assert_eq!(out, "export AWS_SECRET_ACCESS_KEY='[REDACTED:assignment]'");
        // Idempotent on its own output.
        assert_eq!(redact("password=[REDACTED:assignment]").1, 0);
    }

    #[test]
    fn redacts_secret_named_json_fields() {
        let mut value = json!({
            "password": "hunter2",
            "nested": [{"Client_Secret": 12345, "apiKey": "x", "flag": true, "token": null}],
            "private-key": {"inner": "kept"},
            "note": "plain",
        });
        let count = redact_json(&mut value);
        assert_eq!(count, 3);
        assert_eq!(value["password"], FIELD_REPLACEMENT);
        assert_eq!(value["nested"][0]["Client_Secret"], FIELD_REPLACEMENT);
        assert_eq!(value["nested"][0]["apiKey"], FIELD_REPLACEMENT);
        assert_eq!(value["nested"][0]["flag"], true);
        assert_eq!(value["nested"][0]["token"], Value::Null);
        assert_eq!(value["private-key"]["inner"], "kept");
        assert_eq!(redact_json(&mut value), 0);
        let mut counters = json!({"max_tokens": 4096, "input_tokens": 12, "token_count": 3});
        assert_eq!(redact_json(&mut counters), 0);
        assert_eq!(
            redact("max_tokens: 4096 and tokenizer=bpe"),
            ("max_tokens: 4096 and tokenizer=bpe".to_owned(), 0)
        );
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for input in [
            "Please add a README and run cargo test.",
            "sk-short and task-description-of-something-quite-long-here",
            "commit 16a6be7c2bd0a8d1e3f45a7b9c0d1e2f3a4b5c6d fixed it",
            "AKIAshort and AKIA1234 and xoxb-1 and ghp_tooshort",
            "Bearer tokens are explained in the docs",
            "-----BEGIN CERTIFICATE----- is public",
            "eyJhbGciOi is a prefix only",
            "",
        ] {
            assert_eq!(redact(input), (input.to_owned(), 0), "{input}");
        }
    }

    #[test]
    fn output_is_deterministic_and_utf8_safe() {
        let secret = ["gh", "p_"].concat() + &repeat('x', 30);
        let input = format!("héllo 日本語 {secret}日本 password=パス語 \u{1F600}{secret}\u{1F600}");
        let first = redact(&input);
        assert_eq!(first, redact(&input));
        assert_eq!(
            first.0,
            "héllo 日本語 [REDACTED:github_token]日本 password=[REDACTED:assignment] \u{1F600}[REDACTED:github_token]\u{1F600}"
        );
        assert_eq!(first.1, 3);
    }

    #[test]
    fn never_panics_on_generated_input() {
        let pieces = [
            "AKIA",
            "ghp_",
            "sk-",
            "sk-ant-",
            "eyJ",
            ".",
            "Bearer ",
            "password",
            "=",
            ":",
            "\"",
            "'",
            " ",
            "\n",
            "-----BEGIN ",
            "PRIVATE KEY",
            "-----",
            "-----END ",
            "日",
            "é",
            "\u{1F600}",
            "a",
            "Z",
            "9",
            "_",
            "-",
            "xoxb-",
            "glpat-",
            "AIza",
            "[REDACTED:",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..400 {
            let mut input = String::new();
            for _ in 0..(seed % 40) + 1 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                input.push_str(pieces[(seed % pieces.len() as u64) as usize]);
            }
            let first = redact(&input);
            assert_eq!(first, redact(&input));
        }
    }
}
