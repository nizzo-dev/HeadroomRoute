use super::{REDACTED, contains_obvious_secret};
use serde_json::Value;

pub(super) fn redact_json_in_place(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_secret_key(key) {
                    *value = Value::String(REDACTED.into());
                } else {
                    redact_json_in_place(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_json_in_place),
        Value::String(text) => *text = redact_sensitive_text(text),
        _ => {}
    }
}

pub(super) fn redacted_json(mut value: Value) -> Value {
    redact_json_in_place(&mut value);
    value
}

pub(super) fn redacted_value(mut value: Value) -> Value {
    redact_json_in_place(&mut value);
    value
}

pub(super) fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("apikey")
        || normalized.contains("authtoken")
        || normalized.contains("accesstoken")
        || normalized.contains("refreshtoken")
        || normalized.contains("clientsecret")
        || normalized.contains("controltoken")
        || normalized.contains("sessiontoken")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || normalized.contains("password")
        || normalized.contains("privatekey")
        || normalized == "token"
        || normalized == "secret"
        || normalized == "cookie"
}

/// Value tokens that carry a credential marker inline, e.g. `api_key=...`,
/// `client_secret: ...`. Kept explicit so stat keys such as `token_count` or
/// `input_tokens` are never treated as secrets.
const SECRET_VALUE_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "auth_token=",
    "access_token=",
    "refresh_token=",
    "client_secret=",
    "control_token=",
    "session_token=",
    "authorization:",
    "password=",
    "secret=",
    "api_key:",
    "client_secret:",
    "control_token:",
    "session_token:",
    "auth_token:",
    "access_token:",
    "refresh_token:",
    "password:",
    "secret:",
];

pub(crate) fn redact_sensitive_text(text: &str) -> String {
    let mut redact_next = false;
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[trimmed.len()..];
            let unquoted = trimmed.trim_matches(['"', '\'', '`']);
            let lower = unquoted.to_ascii_lowercase();
            let separator = lower.ends_with([':', '=']);
            let key_name = if separator {
                lower.trim_end_matches([':', '='])
            } else {
                lower.as_str()
            };
            // `api_key:` / `api_key =` mark the *next* token as the secret
            // value even when it is opaque (no `sk-` prefix, e.g. a JWT).
            let key_is_secret = is_secret_key(key_name);
            let should_redact = redact_next
                || contains_obvious_secret(trimmed)
                || key_is_secret
                || SECRET_VALUE_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker));
            // A lone `=`/`:` between the key and its value keeps the value
            // flagged for redaction rather than resetting it.
            redact_next = lower == "bearer"
                || lower.ends_with("authorization:")
                || key_is_secret
                || (redact_next && (lower == "=" || lower == ":"));
            if should_redact {
                format!("{REDACTED}{suffix}")
            } else if let Some(url) = redact_url_userinfo(trimmed) {
                format!("{url}{suffix}")
            } else {
                part.to_owned()
            }
        })
        .collect()
}

/// Replace the userinfo portion of an http(s) URL so credentials embedded in
/// an address never reach a diagnostic bundle or preview text.
fn redact_url_userinfo(part: &str) -> Option<String> {
    let scheme_end = part.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = part[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(part.len());
    let at = part[authority_start..authority_end].find('@')?;
    let host_start = authority_start + at + 1;
    Some(format!(
        "{}[REDACTED]@{}",
        &part[..authority_start],
        &part[host_start..]
    ))
}
