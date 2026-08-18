use std::collections::HashMap;

use crate::{config, state::AppState};

pub(super) fn valid_control_token(headers: &HashMap<String, String>, token: &str) -> bool {
    !token.is_empty() && headers.get("x-route-agent-token").map(String::as_str) == Some(token)
}
/// Minimal, non-sensitive identity for the legacy `/status` endpoint. Only the
/// `service` field is consumed (by `prepare_port` to distinguish a running
/// HeadroomRoute instance from the legacy RouteAgent); the version is included
/// for logging. Sensitive snapshot fields such as `last_error`, `routes` and
/// `active_url` are deliberately omitted.
pub(super) fn compat_status(app: &AppState) -> serde_json::Value {
    let snapshot = app.snapshot();
    serde_json::json!({
        "service": snapshot.service,
        "version": snapshot.version,
    })
}

pub(super) fn stable_status(app: &AppState) -> serde_json::Value {
    let snapshot = app.snapshot();
    let history_entries = app.operation_history().len();
    let pending_undo = app.pending_undo_ticket().map(|ticket| {
        serde_json::json!({
            "id": ticket.id,
            "protocol": ticket.protocol,
            "restore_provider": ticket.restore_provider,
            "expires_at": ticket.expires_at,
        })
    });
    serde_json::json!({
        "schema_version": config::portability::LOCAL_STATUS_API_VERSION,
        "service": snapshot.service,
        "version": snapshot.version,
        "health": snapshot.state,
        "protocols": {
            "codex": {
                "availability": snapshot.codex_availability,
                "manage_codex": snapshot.manage_codex,
                "mode": protocol_mode(!snapshot.manage_codex, snapshot.bypass_headroom),
                "active_name": snapshot.active_name,
                "active_host": snapshot.active_host,
                "latency_ms": snapshot.latency_ms,
            },
            "claude": {
                "availability": snapshot.claude_availability,
                "manage_claude": snapshot.manage_claude,
                "mode": protocol_mode(!snapshot.manage_claude, snapshot.bypass_headroom),
                "active_name": snapshot.active_anthropic_name,
                "active_host": snapshot.active_anthropic_host,
                "latency_ms": snapshot.anthropic_latency_ms,
            }
        },
        "automation": {
            "auto_failover": snapshot.auto_enabled,
        },
        "runtime": {
            "headroom_state": snapshot.headroom_state,
            "headroom_pid": snapshot.headroom_pid,
        },
        "operations": {
            "sync": snapshot.sync_status,
            "restart": snapshot.restart_status,
            "history_entries": history_entries,
            "pending_undo": pending_undo,
        },
        "last_error": snapshot
            .last_error
            .as_deref()
            .map(config::portability::redact_sensitive_text),
    })
}

fn protocol_mode(direct: bool, bypass: bool) -> &'static str {
    if direct {
        "direct"
    } else if bypass {
        "bypass"
    } else {
        "managed"
    }
}
