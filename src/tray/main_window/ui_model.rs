//! Frontend-facing snapshot DTO and IPC inbound parser for the WebView console.
//! Routes never include API keys.

use super::super::{recommended_action, route_is_selected};
use crate::model::{HeadroomMetrics, Protocol, Route, Snapshot};
use crate::state::AppState;
use crate::updater;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiInbound {
    Ready,
    Command { id: usize },
    SwitchRoute { index: usize },
    Theme { mode: String },
}

pub fn parse_ui_message(body: &str) -> Option<UiInbound> {
    serde_json::from_str(body).ok()
}

#[derive(Debug, Clone, Serialize)]
pub struct UiRecommended {
    pub id: usize,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiRoute {
    pub index: usize,
    pub protocol: String,
    pub name: String,
    pub provider: String,
    pub latency_ms: Option<u64>,
    pub evidence: String,
    pub selected: bool,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiSnapshot {
    pub runtime_mode: String,
    pub runtime_reason: String,
    pub codex_summary: String,
    pub claude_summary: String,
    pub headroom_summary: String,
    pub codex_route: String,
    pub claude_route: String,
    pub codex_availability: String,
    pub claude_availability: String,
    pub auto_enabled: bool,
    pub bypass_headroom: bool,
    pub manage_upstream: bool,
    pub sync_status: String,
    pub restart_status: String,
    pub headroom_metrics: HeadroomMetrics,
    pub metrics_since: Option<String>,
    pub recovery_hint: String,
    pub last_switch_reason: Option<String>,
    pub last_error: Option<String>,
    pub routes: Vec<UiRoute>,
    pub recommended: Option<UiRecommended>,
    pub start_with_windows: bool,
    pub auto_update_check: bool,
    pub show_api_key_on_hover: bool,
    pub sync_in_progress: bool,
    pub restart_in_progress: bool,
    pub update_running: bool,
}

impl UiSnapshot {
    /// Pure mapper used by tests and by [`build_ui_snapshot`].
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        snapshot: &Snapshot,
        recovery_hint: String,
        start_with_windows: bool,
        sync_in_progress: bool,
        restart_in_progress: bool,
        update_running: bool,
        codex_route: String,
        claude_route: String,
    ) -> Self {
        let recommended = recommended_action(
            &snapshot.runtime_status,
            &snapshot.headroom_state,
            snapshot.last_error.as_deref(),
        )
        .map(|(id, label)| UiRecommended {
            id,
            label: label.to_string(),
        });

        let routes = snapshot
            .routes
            .iter()
            .enumerate()
            .map(|(index, route)| ui_route(index, route, snapshot))
            .collect();

        Self {
            runtime_mode: snapshot.runtime_status.mode.label().to_string(),
            runtime_reason: snapshot.runtime_status.reason.clone(),
            codex_summary: snapshot.runtime_status.codex.summary(),
            claude_summary: snapshot.runtime_status.claude.summary(),
            headroom_summary: snapshot.runtime_status.headroom.summary(),
            codex_route,
            claude_route,
            codex_availability: snapshot.codex_availability.to_string(),
            claude_availability: snapshot.claude_availability.to_string(),
            auto_enabled: snapshot.auto_enabled,
            bypass_headroom: snapshot.bypass_headroom,
            manage_upstream: snapshot.manage_upstream,
            sync_status: snapshot.sync_status.clone(),
            restart_status: snapshot.restart_status.clone(),
            headroom_metrics: snapshot.headroom_metrics,
            metrics_since: snapshot
                .headroom_metrics_since
                .map(|since| since.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
            recovery_hint,
            last_switch_reason: snapshot.last_switch_reason.clone(),
            last_error: snapshot.last_error.clone(),
            routes,
            recommended,
            start_with_windows,
            auto_update_check: snapshot.auto_update_check,
            show_api_key_on_hover: snapshot.show_api_key_on_hover,
            sync_in_progress,
            restart_in_progress,
            update_running,
        }
    }
}

fn ui_route(index: usize, route: &Route, snapshot: &Snapshot) -> UiRoute {
    let selected = match route.protocol {
        Protocol::OpenAi => route_is_selected(route, snapshot.active_provider.as_deref()),
        Protocol::Anthropic => {
            route_is_selected(route, snapshot.active_anthropic_provider.as_deref())
        }
    };
    UiRoute {
        index,
        protocol: match route.protocol {
            Protocol::OpenAi => "openai".into(),
            Protocol::Anthropic => "anthropic".into(),
        },
        name: route.name.clone(),
        provider: route.provider.clone(),
        latency_ms: route.latency_ms,
        evidence: route.evidence_label().to_string(),
        selected,
        state: route.state.label().to_string(),
    }
}

#[allow(dead_code)]
pub fn build_ui_snapshot(app: &AppState) -> UiSnapshot {
    let snapshot = app.snapshot();
    let start_with_windows = app.inner.lock().unwrap().config.start_with_windows;
    UiSnapshot::from_parts(
        &snapshot,
        app.recovery_hint().to_string(),
        start_with_windows,
        app.sync_in_progress.load(Ordering::Acquire),
        app.restart_in_progress.load(Ordering::Acquire),
        updater::is_running(),
        app.route_summary(Protocol::OpenAi),
        app.route_summary(Protocol::Anthropic),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AuthStyle, ComponentState, HeadroomRuntimeStatus, RouteHealth, RuntimeMode, RuntimeStatus,
    };
    use crate::model::{ClientPath, ClientRuntimeStatus};

    fn empty_runtime() -> RuntimeStatus {
        RuntimeStatus {
            mode: RuntimeMode::Normal,
            reason: "ok".into(),
            codex: ClientRuntimeStatus {
                path: ClientPath::Headroom,
                state: ComponentState::Ready,
                reason: String::new(),
            },
            claude: ClientRuntimeStatus {
                path: ClientPath::Headroom,
                state: ComponentState::Ready,
                reason: String::new(),
            },
            headroom: HeadroomRuntimeStatus {
                state: ComponentState::Ready,
                reason: String::new(),
            },
        }
    }

    fn sample_snapshot_with_secret() -> Snapshot {
        let mut route = Route::new(
            Protocol::OpenAi,
            "provider-a".into(),
            "Alpha".into(),
            "https://example.test/v1".into(),
            Some("sk-secret-should-not-leak".into()),
            AuthStyle::Bearer,
            "test",
        );
        route.state = RouteHealth::Healthy;
        route.latency_ms = Some(42);
        Snapshot {
            cli_compatibility: crate::cli_identity::CliCompatibility {
                path: None,
                expected_version: "test".into(),
                detected_version: None,
                detected_protocol: None,
                compatible: true,
                reason: "test".into(),
            },
            service: "HeadroomRoute",
            version: "test",
            state: "ok",
            runtime_status: empty_runtime(),
            active_provider: Some("provider-a".into()),
            active_name: Some("Alpha".into()),
            active_url: Some("https://example.test/v1".into()),
            active_host: Some("example.test".into()),
            active_score: 80,
            latency_ms: Some(42),
            codex_availability: "可用",
            active_anthropic_provider: None,
            active_anthropic_name: None,
            active_anthropic_url: None,
            active_anthropic_host: None,
            active_anthropic_score: 0,
            anthropic_latency_ms: None,
            claude_availability: "未配置",
            auto_enabled: false,
            bypass_headroom: false,
            manage_upstream: true,
            direct_codex: false,
            direct_claude: false,
            headroom_state: "ready".into(),
            headroom_pid: Some(1),
            headroom_metrics: HeadroomMetrics::default(),
            headroom_metrics_since: None,
            auto_update_check: true,
            show_api_key_on_hover: true,
            sync_status: "空闲".into(),
            restart_status: "空闲".into(),
            routes: vec![route],
            last_switch_reason: None,
            last_error: None,
        }
    }

    #[test]
    fn parses_command_and_switch_messages() {
        assert!(matches!(
            parse_ui_message(r#"{"type":"command","id":102}"#),
            Some(UiInbound::Command { id: 102 })
        ));
        assert!(matches!(
            parse_ui_message(r#"{"type":"switch_route","index":3}"#),
            Some(UiInbound::SwitchRoute { index: 3 })
        ));
        assert!(matches!(
            parse_ui_message(r#"{"type":"ready"}"#),
            Some(UiInbound::Ready)
        ));
        assert!(matches!(
            parse_ui_message(r#"{"type":"theme","mode":"system"}"#),
            Some(UiInbound::Theme { mode }) if mode == "system"
        ));
        assert!(parse_ui_message("not-json").is_none());
        assert!(parse_ui_message(r#"{"type":"command","id":-1}"#).is_none());
    }

    #[test]
    fn ui_snapshot_omits_api_keys() {
        let snap = sample_snapshot_with_secret();
        let ui = UiSnapshot::from_parts(
            &snap,
            "无".into(),
            false,
            false,
            false,
            false,
            "Alpha".into(),
            "未配置".into(),
        );
        let json = serde_json::to_string(&ui).expect("serialize");
        assert!(
            !json.contains("\"api_key\""),
            "serialized UI snapshot must not contain api_key field: {json}"
        );
        assert!(
            !json.contains("sk-secret"),
            "serialized UI snapshot must not leak API key material: {json}"
        );
        assert_eq!(ui.routes.len(), 1);
        assert_eq!(ui.routes[0].name, "Alpha");
        assert!(ui.routes[0].selected);
    }
}
