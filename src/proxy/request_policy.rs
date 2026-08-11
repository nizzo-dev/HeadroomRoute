use anyhow::Result;
use serde_json::Value;

pub(super) fn is_ai_conversation_path(path: &str) -> bool {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');

    matches!(
        path,
        "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/messages"
    )
}
pub(super) fn top_level_model(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let model = object.get("model")?.as_str()?.trim();
    (!model.is_empty()).then(|| model.chars().take(160).collect())
}
pub(super) fn join_url(base: &str, target: &str) -> Result<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut url = url::Url::parse(base)?;
    let base_path = url.path().trim_end_matches('/');
    let joined = if (path == "/v1" || path.starts_with("/v1/")) && base_path.ends_with("/v1") {
        format!("{}{}", base_path, &path[3..])
    } else {
        format!("{}{}", base_path, path)
    };
    url.set_path(&joined);
    url.set_query((!query.is_empty()).then_some(query));
    Ok(url.to_string())
}
pub(super) fn is_hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
pub(super) fn should_forward_request_header(name: &str, override_authorization: bool) -> bool {
    !is_hop_header(name)
        && !matches!(name, "host" | "content-length" | "x-headroom-base-url")
        && !(override_authorization && matches!(name, "authorization" | "x-api-key"))
}
pub(super) fn is_route_failure(status: u16) -> bool {
    matches!(status, 401 | 403 | 408 | 429) || status >= 500
}
