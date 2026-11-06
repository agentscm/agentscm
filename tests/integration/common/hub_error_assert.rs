use serde_json::Value;

pub fn assert_legacy_fields(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_legacy_fields(actual, expected);
            }
        }
        (Value::Object(actual), Value::Object(expected))
            if expected.get("error").is_some_and(Value::is_string) =>
        {
            for (key, value) in expected {
                assert_eq!(actual.get(key), Some(value), "legacy field {key}");
            }
            for key in actual.keys() {
                assert!(
                    expected.contains_key(key)
                        || matches!(
                            key.as_str(),
                            "message"
                                | "retryable"
                                | "retry_after_seconds"
                                | "recovery"
                                | "fields"
                                | "correlation_id"
                        ),
                    "unexpected field {key}"
                );
            }
            assert!(actual["retryable"].is_boolean());
            assert!(actual["message"].is_string());
            assert!(actual["recovery"].is_object());
            assert!(agentscm::hub::error_envelope::valid_correlation_id(
                actual["correlation_id"].as_str().unwrap()
            ));
        }
        _ => assert_eq!(actual, expected),
    }
}

#[macro_export]
macro_rules! assert_hub_error {
    ($actual:expr, $expected:expr $(, $($context:tt)*)?) => {
        $crate::hub_error_assert::assert_legacy_fields(
            &serde_json::to_value($actual).unwrap(),
            &serde_json::to_value($expected).unwrap(),
        )
    };
}

/// The text with every `cor_<32 hex>` error reference blanked, so two answers to the same
/// refused request can be compared apart from the reference each carries.
pub fn blank_references(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("cor_") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 4..];
        let hex = tail.bytes().take_while(u8::is_ascii_hexdigit).count();
        out.push_str("cor_<ref>");
        rest = &tail[hex..];
    }
    out.push_str(rest);
    out
}
