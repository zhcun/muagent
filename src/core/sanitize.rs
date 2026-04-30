//! PII/secret redaction for audit logs and traces.
//!
//! Walks a `serde_json::Value` and replaces the values of any key whose
//! name looks like a secret. Conservative and cheap — not a substitute for
//! real DLP, but enough to keep obvious API keys / passwords out of a
//! user-visible audit trail.

use serde_json::{Map, Value};

const SENSITIVE_KEY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "id_token",
    "bearer",
    "authorization",
    "auth_token",
    "session_id",
    "cookie",
    "private_key",
    "privatekey",
    "credential",
];

/// In-place redaction. Returns the JSON serialized as a string.
/// Sensitive values become the literal string `"<redacted>"`.
pub fn sanitize_json(mut v: Value) -> String {
    redact_value(&mut v);
    serde_json::to_string(&v).unwrap_or_default()
}

fn redact_value(v: &mut Value) {
    match v {
        Value::Object(map) => redact_object(map),
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_value(item);
            }
        }
        _ => {}
    }
}

fn redact_object(map: &mut Map<String, Value>) {
    for (k, val) in map.iter_mut() {
        if looks_sensitive(k) {
            *val = Value::String("<redacted>".into());
        } else {
            redact_value(val);
        }
    }
}

fn looks_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SENSITIVE_KEY_SUBSTRINGS.iter().any(|s| k.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_top_level_api_key() {
        let out = sanitize_json(json!({"api_key":"sk-123","user":"alice"}));
        assert!(out.contains(r#""api_key":"<redacted>""#));
        assert!(out.contains(r#""user":"alice""#));
    }

    #[test]
    fn redacts_nested_and_arrays() {
        let out = sanitize_json(json!({
            "config": {"Authorization":"Bearer abc"},
            "users": [{"password":"p1"}, {"password":"p2"}]
        }));
        assert!(out.contains(r#""Authorization":"<redacted>""#));
        assert_eq!(out.matches("<redacted>").count(), 3);
    }

    #[test]
    fn case_insensitive_key_match() {
        let out = sanitize_json(json!({"APIKey":"x","my_password":"y"}));
        assert!(!out.contains("\"x\""));
        assert!(!out.contains("\"y\""));
    }

    #[test]
    fn non_sensitive_keys_pass_through() {
        let out = sanitize_json(json!({"name":"alice","count":42}));
        assert!(out.contains(r#""name":"alice""#));
        assert!(out.contains(r#""count":42"#));
    }
}
