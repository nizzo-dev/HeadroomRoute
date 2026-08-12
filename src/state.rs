mod diagnostics;
mod lifecycle;
mod queries;
mod routing;
mod settings;
mod status;
use status::{
    availability, metrics_scope, preserve_index, provider_exists, recovery_hint, route_summary,
    select_index, transport_ready, valid_provider, yes,
};

use crate::{
    config,
    environment_recovery::{
        EnvironmentEvent, RecoveryAction, RecoveryConfig, RecoveryEngine, RecoveryState,
        SessionStore,
    },
    model::{
        AppConfig, FailoverPolicy, HeadroomMetrics, Protocol, Route, RouteHealth, RuntimeStatus,
        RuntimeStatusInput, Snapshot, evaluate_runtime_status,
    },
    operation_history::{
        DEFAULT_COOLDOWN, OperationHistory, OperationKind, OperationRecord, SwitchCooldown,
        UndoGrant, UndoTicket,
    },
    routing_policy::{self, DecisionMode, RoutingDecision},
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct RuntimeState {
    pub config: AppConfig,
    pub routes: Vec<Route>,
    pub active: Option<usize>,
    pub active_anthropic: Option<usize>,
    pub selected_provider: Option<String>,
    pub selected_anthropic_provider: Option<String>,
    pub headroom_state: String,
    pub headroom_pid: Option<u32>,
    pub headroom_metrics: HeadroomMetrics,
    pub last_switch_reason: Option<String>,
    pub last_error: Option<String>,
}

pub struct AppState {
    pub inner: Mutex<RuntimeState>,
    pub stop: AtomicBool,
    pub restart_headroom: AtomicBool,
    pub force_probe: AtomicBool,
    pub reset_metrics: AtomicBool,
    pub sync_in_progress: AtomicBool,
    pub sync_status: Mutex<String>,
    pub sync_result: Mutex<Option<(bool, String)>>,
    pub restart_in_progress: AtomicBool,
    pub restart_status: Mutex<String>,
    pub restart_result: Mutex<Option<(bool, String)>>,
    pub model_change_notice: Mutex<Option<String>>,
    pub auto_switch_notice: Mutex<Option<String>>,
    pub runtime_result: Mutex<Option<(bool, String)>>,
    pub config_change_notice: Mutex<Option<String>>,
    pub routing_notice: Mutex<Option<(bool, String)>>,
    pub update_notice: Mutex<Option<String>>,
    pub operation_history: Mutex<OperationHistory>,
    pub switch_cooldown: Mutex<SwitchCooldown>,
    pub recovery: Mutex<RecoveryEngine>,
    pub recovery_in_progress: AtomicBool,
    pub recovery_notice: Mutex<Option<String>>,
    pub operation_notice: Mutex<Option<String>>,
    pub pending_undo: Mutex<Option<UndoGrant>>,
    pub routing_log_lock: Mutex<()>,
    pub maintenance_action: Mutex<Option<String>>,
}

fn config_write_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl AppState {
    pub(crate) fn config_write_guard(&self) -> MutexGuard<'static, ()> {
        config_write_mutex().lock().unwrap()
    }

    fn snapshot_unlocked(&self, state: &RuntimeState) -> Snapshot {
        let openai = state.active.and_then(|i| state.routes.get(i));
        let anthropic = state.active_anthropic.and_then(|i| state.routes.get(i));
        let runtime_status = self.runtime_status_unlocked(state);
        Snapshot {
            cli_compatibility: crate::cli_identity::CliCompatibility::inspect_cached(
                &state.config.state_dir,
            ),
            service: "headroom-route",
            version: env!("CARGO_PKG_VERSION"),
            state: runtime_status.mode.health_key(),
            active_provider: openai.map(|r| r.provider.clone()),
            active_name: openai.map(|r| r.name.clone()),
            active_url: openai.map(|r| r.base_url.clone()),
            active_host: openai.map(Route::host),
            active_score: openai.map(|r| r.score).unwrap_or(0),
            latency_ms: openai.and_then(|r| r.latency_ms),
            codex_availability: availability(openai, transport_ready(state, Protocol::OpenAi)),
            active_anthropic_provider: anthropic.map(|r| r.provider.clone()),
            active_anthropic_name: anthropic.map(|r| r.name.clone()),
            active_anthropic_url: anthropic.map(|r| r.base_url.clone()),
            active_anthropic_host: anthropic.map(Route::host),
            active_anthropic_score: anthropic.map(|r| r.score).unwrap_or(0),
            anthropic_latency_ms: anthropic.and_then(|r| r.latency_ms),
            claude_availability: availability(
                anthropic,
                transport_ready(state, Protocol::Anthropic),
            ),
            auto_enabled: state.config.auto_failover,
            bypass_headroom: state.config.bypass_headroom,
            manage_upstream: state.config.manage_upstream,
            direct_codex: state.config.direct_codex,
            direct_claude: state.config.direct_claude,
            headroom_state: state.headroom_state.clone(),
            headroom_pid: state.headroom_pid,
            headroom_metrics: state.headroom_metrics,
            headroom_metrics_since: state.config.metrics_since,
            auto_update_check: state.config.auto_check_updates,
            show_api_key_on_hover: state.config.show_api_key_on_hover,
            sync_status: self.sync_status.lock().unwrap().clone(),
            restart_status: self.restart_status.lock().unwrap().clone(),
            routes: state.routes.clone(),
            last_switch_reason: state.last_switch_reason.clone(),
            last_error: state.last_error.clone(),
            runtime_status,
        }
    }

    fn runtime_status_unlocked(&self, state: &RuntimeState) -> RuntimeStatus {
        evaluate_runtime_status(RuntimeStatusInput {
            codex_enabled: state.config.enable_codex,
            claude_enabled: state.config.enable_claude,
            direct_codex: state.config.direct_codex,
            direct_claude: state.config.direct_claude,
            bypass_headroom: state.config.bypass_headroom,
            codex_route_health: active_route(state, Protocol::OpenAi).map(|route| route.state),
            claude_route_health: active_route(state, Protocol::Anthropic).map(|route| route.state),
            headroom_state: &state.headroom_state,
            sync_in_progress: self.sync_in_progress.load(Ordering::Acquire),
            restart_in_progress: self.restart_in_progress.load(Ordering::Acquire),
            recovery_in_progress: self.recovery_in_progress.load(Ordering::Acquire),
        })
    }
}

fn update_check_due(
    enabled: bool,
    last: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    enabled && last.is_none_or(|last| now.signed_duration_since(last) >= chrono::Duration::days(1))
}

fn previous_provider(config: &AppConfig, key: &str) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(config.state_dir.join("status.json")).ok()?).ok()?;
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}
fn failover_candidate_if<F>(
    routes: &[Route],
    protocol: Protocol,
    failed: usize,
    policy: &FailoverPolicy,
    allowed: F,
) -> Option<usize>
where
    F: Fn(&str) -> bool,
{
    let eligible = |index: usize, route: &Route| {
        index != failed
            && route.protocol == protocol
            && route.state == RouteHealth::Healthy
            && allowed(route.provider.as_str())
            && route
                .failover_blocked_until
                .is_none_or(|until| until <= chrono::Utc::now())
    };
    if let Some(targets) = policy.targets(protocol, &routes[failed].provider) {
        return targets.iter().find_map(|provider| {
            routes.iter().enumerate().find_map(|(index, route)| {
                (route.provider == *provider && eligible(index, route)).then_some(index)
            })
        });
    }
    routes
        .iter()
        .enumerate()
        .filter(|(index, route)| eligible(*index, route))
        .max_by_key(|(_, route)| route.score)
        .map(|(index, _)| index)
}
fn active_route(state: &RuntimeState, protocol: Protocol) -> Option<&Route> {
    let index = if protocol == Protocol::OpenAi {
        state.active
    } else {
        state.active_anthropic
    };
    index.and_then(|value| state.routes.get(value))
}
fn protocol_for_path(path: &str) -> Protocol {
    if path == "/api/oauth/usage" || path.starts_with("/v1/messages") {
        Protocol::Anthropic
    } else {
        Protocol::OpenAi
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?;
    Ok(())
}
pub fn should_stop(app: &AppState) -> bool {
    app.stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        AppState, OperationHistory, RecoveryConfig, RecoveryEngine, RuntimeState, SwitchCooldown,
        availability, failover_candidate_if, preserve_index, protocol_for_path, recovery_hint,
        route_summary, update_check_due,
    };
    use crate::model::{
        AppConfig, AuthStyle, FailoverPolicy, HeadroomMetrics, Protocol, Route, RouteHealth,
        RuntimeMode,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    fn test_app(config: AppConfig, routes: Vec<Route>) -> Arc<AppState> {
        let active = routes
            .iter()
            .position(|route| route.protocol == Protocol::OpenAi);
        let active_anthropic = routes
            .iter()
            .position(|route| route.protocol == Protocol::Anthropic);
        Arc::new(AppState {
            inner: Mutex::new(RuntimeState {
                config,
                routes,
                active,
                active_anthropic,
                selected_provider: None,
                selected_anthropic_provider: None,
                headroom_state: "检测中".into(),
                headroom_pid: None,
                headroom_metrics: HeadroomMetrics::default(),
                last_switch_reason: None,
                last_error: None,
            }),
            stop: AtomicBool::new(false),
            restart_headroom: AtomicBool::new(false),
            force_probe: AtomicBool::new(false),
            reset_metrics: AtomicBool::new(false),
            sync_in_progress: AtomicBool::new(false),
            sync_status: Mutex::new("未同步".into()),
            sync_result: Mutex::new(None),
            restart_in_progress: AtomicBool::new(false),
            restart_status: Mutex::new("未重启".into()),
            restart_result: Mutex::new(None),
            model_change_notice: Mutex::new(None),
            auto_switch_notice: Mutex::new(None),
            runtime_result: Mutex::new(None),
            config_change_notice: Mutex::new(None),
            routing_notice: Mutex::new(None),
            update_notice: Mutex::new(None),
            operation_history: Mutex::new(OperationHistory::new()),
            switch_cooldown: Mutex::new(SwitchCooldown::new()),
            recovery: Mutex::new(RecoveryEngine::new(RecoveryConfig::default())),
            recovery_in_progress: AtomicBool::new(false),
            recovery_notice: Mutex::new(None),
            operation_notice: Mutex::new(None),
            pending_undo: Mutex::new(None),
            routing_log_lock: Mutex::new(()),
            maintenance_action: Mutex::new(None),
        })
    }

    #[test]
    fn snapshot_state_derives_from_runtime_mode() {
        let mut routes = vec![
            Route::new(
                Protocol::OpenAi,
                "codex".into(),
                "Codex".into(),
                "https://api.example.com/v1".into(),
                None,
                AuthStyle::Bearer,
                "test",
            ),
            Route::new(
                Protocol::Anthropic,
                "claude".into(),
                "Claude".into(),
                "https://api.anthropic.com/v1".into(),
                None,
                AuthStyle::Bearer,
                "test",
            ),
        ];
        for route in &mut routes {
            route.state = RouteHealth::Healthy;
        }
        let app = test_app(AppConfig::default(), routes);
        app.inner.lock().unwrap().headroom_state = "runtime-unavailable".into();
        let snapshot = app.snapshot();
        assert_eq!(snapshot.runtime_status.mode, RuntimeMode::Degraded);
        assert_eq!(snapshot.state, "degraded");

        app.inner.lock().unwrap().config.bypass_headroom = true;
        let snapshot = app.snapshot();
        assert_eq!(snapshot.runtime_status.mode, RuntimeMode::Bypass);
        assert_eq!(snapshot.state, "healthy");
    }

    #[test]
    fn runtime_status_observes_sync_and_restart_flags() {
        let mut config = AppConfig::default();
        config.enable_codex = false;
        config.enable_claude = false;
        let app = test_app(config, Vec::new());
        assert_eq!(app.snapshot().runtime_status.mode, RuntimeMode::Normal);

        app.sync_in_progress.store(true, Ordering::Release);
        assert_eq!(app.snapshot().runtime_status.mode, RuntimeMode::Recovering);
        app.sync_in_progress.store(false, Ordering::Release);
        app.restart_in_progress.store(true, Ordering::Release);
        assert_eq!(app.snapshot().runtime_status.mode, RuntimeMode::Recovering);
    }

    #[test]
    fn classifies_openai_and_anthropic_paths() {
        assert_eq!(protocol_for_path("/v1/responses"), Protocol::OpenAi);
        assert_eq!(protocol_for_path("/v1/messages"), Protocol::Anthropic);
        assert_eq!(
            protocol_for_path("/v1/messages/count_tokens"),
            Protocol::Anthropic
        );
        assert_eq!(protocol_for_path("/api/oauth/usage"), Protocol::Anthropic);
    }

    #[test]
    fn automatic_update_check_runs_at_most_daily() {
        let now = chrono::Utc::now();
        assert!(update_check_due(true, None, now));
        assert!(!update_check_due(true, Some(now), now));
        assert!(update_check_due(
            true,
            Some(now - chrono::Duration::days(1)),
            now
        ));
        assert!(!update_check_due(false, None, now));
    }

    #[test]
    fn api_key_hover_setting_is_persisted() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-api-key-hover-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = AppConfig::default();
        config.state_dir = dir.clone();
        config.cc_switch_db = dir.join("missing.db");
        config.codex_config = dir.join("missing.toml");
        config.claude_settings = dir.join("missing.json");
        config.enable_codex = false;
        config.enable_claude = false;
        let app = AppState::new(config);

        assert!(!app.snapshot().show_api_key_on_hover);
        assert!(app.toggle_show_api_key_on_hover().unwrap());
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert!(saved.show_api_key_on_hover);
        assert!(!app.toggle_show_api_key_on_hover().unwrap());
        assert!(!app.snapshot().show_api_key_on_hover);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn api_key_hover_setting_rolls_back_when_persistence_fails() {
        let path = std::env::temp_dir().join(format!(
            "headroom-route-api-key-hover-failure-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not a directory").unwrap();
        let mut config = AppConfig::default();
        config.state_dir = path.clone();
        config.cc_switch_db = path.join("missing.db");
        config.codex_config = path.join("missing.toml");
        config.claude_settings = path.join("missing.json");
        config.enable_codex = false;
        config.enable_claude = false;
        let app = AppState::new(config);

        assert!(app.toggle_show_api_key_on_hover().is_err());
        assert!(!app.snapshot().show_api_key_on_hover);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn availability_requires_transport_and_route_health() {
        let mut route = Route::new(
            Protocol::OpenAi,
            "provider".into(),
            "Provider".into(),
            "https://api.example.com".into(),
            None,
            AuthStyle::Bearer,
            "test",
        );
        assert_eq!(availability(Some(&route), false), "不可用");
        assert_eq!(availability(Some(&route), true), "待验证");
        assert_eq!(route.evidence_label(), "尚未验证");
        route.record(true, 10, Some(200), None, true);
        assert_eq!(availability(Some(&route), true), "可用");
        assert_eq!(route.evidence_label(), "真实请求验证");
        route.record(false, 10, Some(401), Some("unauthorized".into()), true);
        assert_eq!(route.evidence_label(), "鉴权失败");
    }

    #[test]
    fn duplicate_urls_preserve_provider_identity_and_health() {
        let routes = vec![
            Route::new(
                Protocol::OpenAi,
                "first".into(),
                "First".into(),
                "https://same.example.com/v1".into(),
                Some("key-a".into()),
                AuthStyle::Bearer,
                "test",
            ),
            Route::new(
                Protocol::OpenAi,
                "second".into(),
                "Second".into(),
                "https://same.example.com/v1".into(),
                Some("key-b".into()),
                AuthStyle::Bearer,
                "test",
            ),
        ];
        assert_eq!(
            preserve_index(&routes, Protocol::OpenAi, Some("second".into()), None),
            Some(1)
        );
        let app = Arc::new(AppState {
            inner: Mutex::new(RuntimeState {
                config: AppConfig::default(),
                routes,
                active: Some(1),
                active_anthropic: None,
                selected_provider: Some("second".into()),
                selected_anthropic_provider: None,
                headroom_state: "test".into(),
                headroom_pid: None,
                headroom_metrics: HeadroomMetrics::default(),
                last_switch_reason: None,
                last_error: None,
            }),
            stop: AtomicBool::new(false),
            restart_headroom: AtomicBool::new(false),
            force_probe: AtomicBool::new(false),
            reset_metrics: AtomicBool::new(false),
            sync_in_progress: AtomicBool::new(false),
            sync_status: Mutex::new("未同步".into()),
            sync_result: Mutex::new(None),
            restart_in_progress: AtomicBool::new(false),
            restart_status: Mutex::new("未重启".into()),
            restart_result: Mutex::new(None),
            model_change_notice: Mutex::new(None),
            auto_switch_notice: Mutex::new(None),
            runtime_result: Mutex::new(None),
            config_change_notice: Mutex::new(None),
            routing_notice: Mutex::new(None),
            update_notice: Mutex::new(None),
            operation_history: Mutex::new(OperationHistory::new()),
            switch_cooldown: Mutex::new(SwitchCooldown::new()),
            recovery: Mutex::new(RecoveryEngine::new(RecoveryConfig::default())),
            recovery_in_progress: AtomicBool::new(false),
            recovery_notice: Mutex::new(None),
            operation_notice: Mutex::new(None),
            pending_undo: Mutex::new(None),
            routing_log_lock: Mutex::new(()),
            maintenance_action: Mutex::new(None),
        });
        app.record_route_result(Protocol::OpenAi, "second", true, 25, Some(200), None, true);
        let snapshot = app.snapshot();
        assert_eq!(snapshot.routes[0].state, RouteHealth::Unknown);
        assert_eq!(snapshot.routes[1].state, RouteHealth::Healthy);
    }

    #[test]
    fn diagnostics_recommend_fixing_provider_auth() {
        let mut route = Route::new(
            Protocol::OpenAi,
            "broken".into(),
            "Broken".into(),
            "https://example.com/v1".into(),
            None,
            AuthStyle::Bearer,
            "test",
        );
        route.record(false, 20, Some(401), Some("HTTP 401".into()), true);
        assert!(route_summary(Some(&route)).contains("HTTP 401"));
        assert_eq!(
            recovery_hint(Some(&route), None, "healthy", None),
            "检查当前 Provider 的 API Key 或登录状态"
        );
    }

    #[test]
    fn auto_failover_persists_and_switches_after_three_failures() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-failover-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.clone();
        config.cc_switch_db = dir.join("missing.db");
        config.auto_failover = false;
        config.manage_upstream = true;
        config.sync_deprecated_direct_flags();
        let mut routes = vec![
            Route::new(
                Protocol::OpenAi,
                "primary".into(),
                "Primary".into(),
                "https://primary.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "test",
            ),
            Route::new(
                Protocol::OpenAi,
                "backup".into(),
                "Backup".into(),
                "https://backup.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "test",
            ),
        ];
        routes[1].record(true, 10, Some(200), None, true);
        let app = Arc::new(AppState {
            inner: Mutex::new(RuntimeState {
                config,
                routes,
                active: Some(0),
                active_anthropic: None,
                selected_provider: Some("primary".into()),
                selected_anthropic_provider: None,
                headroom_state: "test".into(),
                headroom_pid: None,
                headroom_metrics: HeadroomMetrics::default(),
                last_switch_reason: None,
                last_error: None,
            }),
            stop: AtomicBool::new(false),
            restart_headroom: AtomicBool::new(false),
            force_probe: AtomicBool::new(false),
            reset_metrics: AtomicBool::new(false),
            sync_in_progress: AtomicBool::new(false),
            sync_status: Mutex::new("未同步".into()),
            sync_result: Mutex::new(None),
            restart_in_progress: AtomicBool::new(false),
            restart_status: Mutex::new("未重启".into()),
            restart_result: Mutex::new(None),
            model_change_notice: Mutex::new(None),
            auto_switch_notice: Mutex::new(None),
            runtime_result: Mutex::new(None),
            config_change_notice: Mutex::new(None),
            routing_notice: Mutex::new(None),
            update_notice: Mutex::new(None),
            operation_history: Mutex::new(OperationHistory::new()),
            switch_cooldown: Mutex::new(SwitchCooldown::new()),
            recovery: Mutex::new(RecoveryEngine::new(RecoveryConfig::default())),
            recovery_in_progress: AtomicBool::new(false),
            recovery_notice: Mutex::new(None),
            operation_notice: Mutex::new(None),
            pending_undo: Mutex::new(None),
            routing_log_lock: Mutex::new(()),
            maintenance_action: Mutex::new(None),
        });
        assert!(app.toggle_auto_failover().unwrap());
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert!(saved.auto_failover);
        for _ in 0..2 {
            app.record_route_result(
                Protocol::OpenAi,
                "primary",
                false,
                20,
                None,
                Some("offline".into()),
                true,
            );
        }
        assert_eq!(app.snapshot().active_provider.as_deref(), Some("primary"));
        app.record_route_result(
            Protocol::OpenAi,
            "primary",
            false,
            20,
            None,
            Some("offline".into()),
            true,
        );
        assert_eq!(app.snapshot().active_provider.as_deref(), Some("backup"));
        assert!(app.take_auto_switch_notice().is_some());
        assert!(
            app.inner.lock().unwrap().routes[0]
                .failover_blocked_until
                .is_some()
        );
        let history = app.operation_history();
        assert!(
            history
                .iter()
                .any(|record| record.kind == crate::operation_history::OperationKind::AutoFailover)
        );
        assert!(app.switch_index(0, "manual 3 failure test"));
        assert_eq!(
            app.operation_history().last().map(|record| &record.kind),
            Some(&crate::operation_history::OperationKind::ManualSwitch)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn configured_failover_order_is_strict() {
        let mut routes = vec![
            Route::new(
                Protocol::OpenAi,
                "primary".into(),
                "Primary".into(),
                "https://primary.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "test",
            ),
            Route::new(
                Protocol::OpenAi,
                "first".into(),
                "First".into(),
                "https://first.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "test",
            ),
            Route::new(
                Protocol::OpenAi,
                "second".into(),
                "Second".into(),
                "https://second.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "test",
            ),
        ];
        routes[1].record(true, 900, Some(200), None, true);
        routes[2].record(true, 10, Some(200), None, true);

        let mut policy = FailoverPolicy::default();
        policy
            .openai
            .insert("primary".into(), vec!["first".into(), "second".into()]);
        assert_eq!(
            failover_candidate_if(&routes, Protocol::OpenAi, 0, &policy, |_| true),
            Some(1)
        );

        policy
            .openai
            .insert("primary".into(), vec!["missing".into()]);
        assert_eq!(
            failover_candidate_if(&routes, Protocol::OpenAi, 0, &policy, |_| true),
            None
        );
        assert_eq!(
            failover_candidate_if(
                &routes,
                Protocol::OpenAi,
                0,
                &FailoverPolicy::default(),
                |_| true,
            ),
            Some(2)
        );
    }

    #[test]
    fn tool_selection_is_persisted_independently_of_cc_switch() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-selection-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.clone();
        config.manage_upstream = true;
        config.sync_deprecated_direct_flags();
        let routes = vec![
            Route::new(
                Protocol::OpenAi,
                "cc-current".into(),
                "CC current".into(),
                "https://one.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "cc-switch",
            ),
            Route::new(
                Protocol::OpenAi,
                "tool-choice".into(),
                "Tool choice".into(),
                "https://two.example.com/v1".into(),
                None,
                AuthStyle::PassThrough,
                "cc-switch",
            ),
        ];
        let app = Arc::new(AppState {
            inner: Mutex::new(RuntimeState {
                config,
                routes,
                active: Some(0),
                active_anthropic: None,
                selected_provider: Some("cc-current".into()),
                selected_anthropic_provider: None,
                headroom_state: "test".into(),
                headroom_pid: None,
                headroom_metrics: HeadroomMetrics::default(),
                last_switch_reason: None,
                last_error: None,
            }),
            stop: AtomicBool::new(false),
            restart_headroom: AtomicBool::new(false),
            force_probe: AtomicBool::new(false),
            reset_metrics: AtomicBool::new(false),
            sync_in_progress: AtomicBool::new(false),
            sync_status: Mutex::new("未同步".into()),
            sync_result: Mutex::new(None),
            restart_in_progress: AtomicBool::new(false),
            restart_status: Mutex::new("未重启".into()),
            restart_result: Mutex::new(None),
            model_change_notice: Mutex::new(None),
            auto_switch_notice: Mutex::new(None),
            runtime_result: Mutex::new(None),
            config_change_notice: Mutex::new(None),
            routing_notice: Mutex::new(None),
            update_notice: Mutex::new(None),
            operation_history: Mutex::new(OperationHistory::new()),
            switch_cooldown: Mutex::new(SwitchCooldown::new()),
            recovery: Mutex::new(RecoveryEngine::new(RecoveryConfig::default())),
            recovery_in_progress: AtomicBool::new(false),
            recovery_notice: Mutex::new(None),
            operation_notice: Mutex::new(None),
            pending_undo: Mutex::new(None),
            routing_log_lock: Mutex::new(()),
            maintenance_action: Mutex::new(None),
        });
        assert!(app.switch_index(1, "test"));
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved.selected_openai_provider.as_deref(),
            Some("tool-choice")
        );
        assert!(app.begin_restart());
        assert!(!app.begin_restart());
        app.finish_restart(true, "ok".into());
        assert_eq!(app.take_restart_result(), Some((true, "ok".into())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_selection_recovers_from_previous_runtime_status() {
        let dir =
            std::env::temp_dir().join(format!("headroom-route-repair-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("config.toml");
        std::fs::write(&codex, "model_provider = \"remembered\"\n[model_providers.remembered]\nname = \"Remembered\"\nbase_url = \"https://remembered.example.com/v1\"\n[model_providers.other]\nname = \"Other\"\nbase_url = \"https://other.example.com/v1\"\n").unwrap();
        std::fs::write(
            dir.join("status.json"),
            r#"{"active_provider":"remembered"}"#,
        )
        .unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.clone();
        config.codex_config = codex;
        config.cc_switch_db = dir.join("missing.db");
        config.enable_claude = false;
        config.selected_openai_provider = Some("deleted-provider".into());
        let app = AppState::new(config);
        assert_eq!(app.active_route().unwrap().provider, "remembered");
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(
            saved.selected_openai_provider.as_deref(),
            Some("remembered")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
