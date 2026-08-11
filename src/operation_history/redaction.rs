use anyhow::{Result, bail};

pub(super) const REDACTED: &str = "[REDACTED]";

pub fn sanitize_reason(raw: &str) -> String {
    redact_tokens(raw)
}

fn redact_tokens(raw: &str) -> String {
    let mut redact_next = false;
    raw.split_inclusive(char::is_whitespace)
        .map(|part| {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[trimmed.len()..];
            let unquoted = trimmed.trim_matches(&['"', '\\', '`'][..]);
            let lower = unquoted.to_ascii_lowercase();
            let separator = lower.ends_with([':', '=']);
            let key_name = if separator {
                lower.trim_end_matches([':', '='])
            } else {
                lower.as_str()
            };
            let key_is_secret = is_secret_key(key_name);
            let obvious_secret = contains_obvious_secret(trimmed);
            let marker = SECRET_VALUE_MARKERS
                .iter()
                .any(|marker| lower.contains(marker));
            let should_redact = redact_next || key_is_secret || obvious_secret || marker;
            redact_next = lower == "bearer"
                || lower.ends_with("authorization:")
                || key_is_secret
                || (redact_next && matches!(lower.as_str(), "=" | ":"));
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

pub fn require_clean(raw: &str) -> Result<String> {
    let cleaned = sanitize_reason(raw);
    let lower = cleaned.to_ascii_lowercase();
    if lower.contains("bearer")
        || lower.contains("authorization")
        || lower.contains("password")
        || lower.contains("secret")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("eyjhb")
    {
        bail!("operation reason contains sensitive material");
    }
    Ok(cleaned)
}

pub fn ensure_identifier_clean(provider: &str) -> Result<()> {
    if contains_obvious_secret(provider)
        || looks_like_jwt(provider)
        || is_secret_key(provider.trim_matches(&['"', '\\', '`'][..]))
    {
        bail!("Provider identifier contains sensitive material");
    }
    Ok(())
}

pub(super) fn sanitize_identifier_for_load(value: &str) -> String {
    if contains_obvious_secret(value) || looks_like_jwt(value) {
        REDACTED.to_owned()
    } else {
        value.to_owned()
    }
}

fn contains_obvious_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    (lower.contains("sk-") || lower.contains("sk_")) && value.len() >= 12
}

fn looks_like_jwt(value: &str) -> bool {
    let value = value.trim_matches(&['"', '\\', '`'][..]);
    value.starts_with("eyJ") && value.split('.').count() == 3
}

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

fn is_secret_key(key: &str) -> bool {
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
