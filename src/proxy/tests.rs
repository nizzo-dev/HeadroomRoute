#![allow(clippy::field_reassign_with_default)]
use super::server::configure_client_stream;
use super::{
    compat_status, is_route_failure, join_url, read_request, should_forward_request_header,
    stable_status, top_level_model, valid_control_token,
};
use std::{
    collections::HashMap,
    io::Write,
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};

#[test]
fn versioned_status_api_requires_exact_control_token() {
    let mut headers = HashMap::new();
    assert!(!valid_control_token(&headers, "control-secret"));
    headers.insert("x-route-agent-token".into(), "wrong".into());
    assert!(!valid_control_token(&headers, "control-secret"));
    headers.insert("x-route-agent-token".into(), "control-secret".into());
    assert!(valid_control_token(&headers, "control-secret"));
    assert!(!valid_control_token(&headers, ""));
}

#[test]
fn stable_status_exposes_stable_redacted_fields() {
    use crate::model::AppConfig;
    use crate::state::AppState;
    let dir = std::env::temp_dir().join(format!(
        "headroom-route-status-api-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let codex = dir.join("codex.toml");
    std::fs::write(
            &codex,
            "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
    let mut config = AppConfig::default();
    config.state_dir = dir.join("state");
    config.codex_config = codex;
    config.claude_settings = dir.join("missing-claude.json");
    config.cc_switch_db = dir.join("missing.db");
    config.enable_claude = false;
    let app = AppState::new(config);
    let secret = "sk-status-secret-0123456789abcdef";
    app.inner.lock().unwrap().last_error = Some(format!("Bearer {secret}"));
    let value = stable_status(&app);
    let schema = value.get("schema_version").unwrap();
    assert_eq!(
        schema.as_u64(),
        Some(crate::config::portability::LOCAL_STATUS_API_VERSION as u64)
    );
    assert_eq!(value["service"], "headroom-route");
    assert!(value["health"].is_string());
    for protocol in ["codex", "claude"] {
        let fields = &value["protocols"][protocol];
        for field in [
            "availability",
            "mode",
            "active_name",
            "active_host",
            "latency_ms",
        ] {
            assert!(fields.get(field).is_some(), "{protocol}.{field}");
        }
    }
    assert!(value["automation"]["auto_failover"].is_boolean());
    assert!(value["runtime"]["headroom_state"].is_string());
    assert!(value["operations"]["sync"].is_string());
    assert!(value["operations"]["restart"].is_string());
    assert_eq!(value["last_error"], "Bearer [REDACTED]");
    let serialized = serde_json::to_string(&value).unwrap();
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("api_key"));
    assert!(!serialized.contains("token"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn compat_status_exposes_only_service_and_version() {
    use crate::model::AppConfig;
    use crate::state::AppState;
    let dir = std::env::temp_dir().join(format!(
        "headroom-route-compat-status-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = AppConfig::default();
    config.state_dir = dir.join("state");
    config.codex_config = dir.join("missing-codex.toml");
    config.claude_settings = dir.join("missing-claude.json");
    config.cc_switch_db = dir.join("missing.db");
    let app = AppState::new(config);
    app.inner.lock().unwrap().last_error = Some("Bearer sk-status-secret-0123456789".into());
    let value = compat_status(&app);
    assert_eq!(value["service"], "headroom-route");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    for sensitive in [
        "last_error",
        "routes",
        "active_url",
        "active_provider",
        "active_name",
        "health",
        "headroom_state",
    ] {
        assert!(value.get(sensitive).is_none(), "should omit {sensitive}");
    }
    let serialized = serde_json::to_string(&value).unwrap();
    assert!(!serialized.contains("sk-status-secret"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn joins_openai_paths_without_duplicate_v1() {
    assert_eq!(
        join_url("https://example.com/v1", "/v1/responses?x=1").unwrap(),
        "https://example.com/v1/responses?x=1"
    );
    assert_eq!(
        join_url("https://example.com", "/v1/models").unwrap(),
        "https://example.com/v1/models"
    );
}

#[test]
fn model_selection_reads_only_the_json_top_level() {
    assert_eq!(
        top_level_model(br#"{"model":"gpt-4o","input":[{"model":"nested"}]}"#),
        Some("gpt-4o".into())
    );
    assert_eq!(top_level_model(br#"{"input":{"model":"nested"}}"#), None);
    assert_eq!(top_level_model(b"not-json"), None);
}

#[test]
fn decodes_chunked_request_body() {
    assert_eq!(
        super::decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(),
        Some(b"Wikipedia".to_vec())
    );
    assert_eq!(super::decode_chunked(b"4\r\nWi").unwrap(), None);
}

#[test]
fn replaces_incoming_authorization_when_route_has_key() {
    assert!(!should_forward_request_header("authorization", true));
    assert!(!should_forward_request_header("x-api-key", true));
    assert!(should_forward_request_header("authorization", false));
    assert!(!should_forward_request_header("x-headroom-base-url", false));
    assert!(should_forward_request_header("content-type", true));
}

#[test]
fn only_route_failures_count_toward_failover() {
    assert!(!is_route_failure(400));
    assert!(!is_route_failure(404));
    assert!(is_route_failure(401));
    assert!(is_route_failure(429));
    assert!(is_route_failure(502));
}

#[test]
fn recognizes_ai_conversation_paths() {
    for path in [
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/responses",
        "/v1/messages/",
        "/v1/responses?stream=true",
    ] {
        assert!(super::is_ai_conversation_path(path), "{path}");
    }

    for path in [
        "/v1/models",
        "/v1/embeddings",
        "/v1/responses/status",
        "/healthz",
    ] {
        assert!(!super::is_ai_conversation_path(path), "{path}");
    }
}

#[test]
fn accepted_client_waits_for_delayed_request_bytes() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let mut client = TcpStream::connect(address).unwrap();
        thread::sleep(Duration::from_millis(75));
        client
            .write_all(
                b"POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
            )
            .unwrap();
    });

    let mut accepted = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    // Force the Windows failure mode even on platforms where accept does
    // not inherit it, then verify our accepted-socket setup repairs it.
    accepted.set_nonblocking(true).unwrap();
    configure_client_stream(&accepted).unwrap();
    let request = read_request(&mut accepted).unwrap();
    assert_eq!(request.target, "/v1/responses");
    assert_eq!(request.body, b"{}");
    writer.join().unwrap();
}
