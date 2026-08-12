use crate::routing_policy::RoutingStrategyConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

mod runtime_status;

#[allow(unused_imports)]
pub use runtime_status::{
    ClientPath, ClientRuntimeStatus, ComponentState, HeadroomRuntimeStatus, RuntimeMode,
    RuntimeStatus, RuntimeStatusInput, evaluate_runtime_status,
};

pub const DEFAULT_AGENT_PORT: u16 = 8790;
pub const DEFAULT_HEADROOM_PORT: u16 = 8787;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub codex_config: PathBuf,
    pub claude_settings: PathBuf,
    pub cc_switch_db: PathBuf,
    pub state_dir: PathBuf,
    pub legacy_state_dir: PathBuf,
    pub agent_port: u16,
    pub headroom_port: u16,
    pub enable_codex: bool,
    pub enable_claude: bool,
    pub selected_openai_provider: Option<String>,
    pub selected_anthropic_provider: Option<String>,
    pub claude_upstream_url: Option<String>,
    pub auto_failover: bool,
    pub failover_policy: FailoverPolicy,
    pub manage_headroom: bool,
    pub start_with_windows: bool,
    pub no_subscription_tracking: bool,
    pub use_system_proxy: bool,
    pub bypass_headroom: bool,
    /// When true, rewrite Codex/Claude client configs to the local HeadroomRoute
    /// agent and own provider switching. When false (default), only observe:
    /// clients stay on the CC-Switch current upstream.
    #[serde(default)]
    pub manage_upstream: bool,
    /// Deprecated: folded into `manage_upstream`. Kept for config migration.
    #[serde(default)]
    pub direct_codex: bool,
    /// Deprecated: folded into `manage_upstream`. Kept for config migration.
    #[serde(default)]
    pub direct_claude: bool,
    pub metrics_log_offset: u64,
    pub metrics_since: Option<DateTime<Utc>>,
    pub auto_check_updates: bool,
    pub last_update_check: Option<DateTime<Utc>>,
    pub show_api_key_on_hover: bool,
    pub headroom_python: Option<PathBuf>,
    /// Policy defaults keep existing routing unchanged until explicitly enabled.
    pub routing_strategy: RoutingStrategyConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        let state_dir = std::env::var_os("HEADROOM_ROUTE_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| local.join("HeadroomRoute"));
        Self {
            codex_config: home.join(".codex/config.toml"),
            claude_settings: home.join(".claude/settings.json"),
            cc_switch_db: home.join(".cc-switch/cc-switch.db"),
            state_dir,
            legacy_state_dir: home.join(".headroom/route-agent"),
            agent_port: DEFAULT_AGENT_PORT,
            headroom_port: DEFAULT_HEADROOM_PORT,
            enable_codex: true,
            enable_claude: true,
            selected_openai_provider: None,
            selected_anthropic_provider: None,
            claude_upstream_url: None,
            auto_failover: false,
            failover_policy: FailoverPolicy::default(),
            manage_headroom: true,
            start_with_windows: false,
            no_subscription_tracking: true,
            use_system_proxy: true,
            bypass_headroom: false,
            manage_upstream: false,
            direct_codex: false,
            direct_claude: false,
            metrics_log_offset: 0,
            metrics_since: None,
            auto_check_updates: true,
            last_update_check: None,
            show_api_key_on_hover: false,
            headroom_python: Some(home.join(".headroom/venv/Scripts/python.exe")),
            routing_strategy: RoutingStrategyConfig::default(),
        }
    }
}

impl AppConfig {
    /// Normalize after deserialize.
    /// - `manage_upstream` defaults to false (observe-by-default).
    /// - Legacy `direct_*` mean "do not manage"; if either was on, force observe.
    /// - Keep `direct_*` as derived mirrors of `!manage_upstream` so existing
    ///   call sites that branch on direct keep working during the transition.
    pub fn migrate_manage_upstream(&mut self) {
        if self.direct_codex || self.direct_claude {
            self.manage_upstream = false;
        }
        self.sync_deprecated_direct_flags();
    }

    pub fn sync_deprecated_direct_flags(&mut self) {
        let observing = !self.manage_upstream;
        self.direct_codex = observing;
        self.direct_claude = observing;
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FailoverPolicy {
    pub openai: BTreeMap<String, Vec<String>>,
    pub anthropic: BTreeMap<String, Vec<String>>,
}

impl FailoverPolicy {
    pub fn rules(&self, protocol: Protocol) -> &BTreeMap<String, Vec<String>> {
        match protocol {
            Protocol::OpenAi => &self.openai,
            Protocol::Anthropic => &self.anthropic,
        }
    }

    pub fn rules_mut(&mut self, protocol: Protocol) -> &mut BTreeMap<String, Vec<String>> {
        match protocol {
            Protocol::OpenAi => &mut self.openai,
            Protocol::Anthropic => &mut self.anthropic,
        }
    }

    pub fn targets(&self, protocol: Protocol, provider: &str) -> Option<&[String]> {
        self.rules(protocol).get(provider).map(Vec::as_slice)
    }

    pub fn counts(&self) -> (usize, usize) {
        let rules = self.openai.values().chain(self.anthropic.values());
        (rules.clone().count(), rules.map(Vec::len).sum())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    OpenAi,
    Anthropic,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "Codex",
            Self::Anthropic => "Claude",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    Bearer,
    XApiKey,
    PassThrough,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub protocol: Protocol,
    pub provider: String,
    pub name: String,
    pub base_url: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub auth_style: AuthStyle,
    pub source: String,
    pub state: RouteHealth,
    pub score: i32,
    pub latency_ms: Option<u64>,
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
    pub verified_by_request: bool,
    pub last_error: Option<String>,
    pub last_status_code: Option<u16>,
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing)]
    pub failover_blocked_until: Option<DateTime<Utc>>,
}

impl Route {
    pub fn new(
        protocol: Protocol,
        provider: String,
        name: String,
        base_url: String,
        api_key: Option<String>,
        auth_style: AuthStyle,
        source: &str,
    ) -> Self {
        Self {
            protocol,
            provider,
            name,
            base_url,
            api_key,
            auth_style,
            source: source.to_owned(),
            state: RouteHealth::Unknown,
            score: 0,
            latency_ms: None,
            consecutive_successes: 0,
            consecutive_failures: 0,
            verified_by_request: false,
            last_error: None,
            last_status_code: None,
            last_success_at: None,
            failover_blocked_until: None,
        }
    }
    pub fn host(&self) -> String {
        url::Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| self.base_url.clone())
    }
    pub fn evidence_label(&self) -> &'static str {
        if matches!(self.last_status_code, Some(401 | 403)) {
            "鉴权失败"
        } else if self.verified_by_request && self.state == RouteHealth::Healthy {
            "真实请求验证"
        } else if self.state == RouteHealth::Unknown && self.latency_ms.is_some() {
            "探测可达"
        } else if self.state == RouteHealth::Unknown {
            "尚未验证"
        } else {
            self.state.label()
        }
    }
    pub fn record(
        &mut self,
        ok: bool,
        latency_ms: u64,
        status: Option<u16>,
        error: Option<String>,
        request: bool,
    ) {
        self.latency_ms = Some(latency_ms);
        self.last_status_code = status;
        if ok {
            self.consecutive_successes += 1;
            self.consecutive_failures = 0;
            self.last_error = None;
            self.last_success_at = Some(Utc::now());
            if request {
                self.verified_by_request = true;
            }
            let authenticated = request || status.is_some_and(|value| value < 400);
            self.state = if authenticated {
                RouteHealth::Healthy
            } else {
                RouteHealth::Unknown
            };
            self.score = (if authenticated { 65 } else { 35 })
                + if self.verified_by_request { 20 } else { 0 }
                + if latency_ms < 300 {
                    15
                } else if latency_ms < 1000 {
                    10
                } else {
                    0
                };
        } else {
            self.consecutive_failures += 1;
            self.consecutive_successes = 0;
            self.last_error = error.map(|value| value.chars().take(300).collect());
            self.state = if self.consecutive_failures >= 3 {
                RouteHealth::Unavailable
            } else {
                RouteHealth::Degraded
            };
            self.score = (40 - self.consecutive_failures as i32 * 15).max(0);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteHealth {
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
}

impl RouteHealth {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "未知",
            Self::Healthy => "健康",
            Self::Degraded => "降级",
            Self::Unavailable => "不可用",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct HeadroomMetrics {
    pub completed_requests: u64,
    pub failed_requests: u64,
    pub input_tokens_original: u64,
    pub input_tokens_optimized: u64,
    pub tokens_saved: u64,
}

impl HeadroomMetrics {
    pub fn compression_percent(self) -> f64 {
        percent(self.tokens_saved, self.input_tokens_original)
    }
    pub fn failure_percent(self) -> f64 {
        percent(self.failed_requests, self.completed_requests)
    }
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub cli_compatibility: crate::cli_identity::CliCompatibility,
    pub service: &'static str,
    pub version: &'static str,
    pub state: &'static str,
    pub runtime_status: RuntimeStatus,
    pub active_provider: Option<String>,
    pub active_name: Option<String>,
    pub active_url: Option<String>,
    pub active_host: Option<String>,
    pub active_score: i32,
    pub latency_ms: Option<u64>,
    pub codex_availability: &'static str,
    pub active_anthropic_provider: Option<String>,
    pub active_anthropic_name: Option<String>,
    pub active_anthropic_url: Option<String>,
    pub active_anthropic_host: Option<String>,
    pub active_anthropic_score: i32,
    pub anthropic_latency_ms: Option<u64>,
    pub claude_availability: &'static str,
    pub auto_enabled: bool,
    pub bypass_headroom: bool,
    pub manage_upstream: bool,
    pub direct_codex: bool,
    pub direct_claude: bool,
    pub headroom_state: String,
    pub headroom_pid: Option<u32>,
    pub headroom_metrics: HeadroomMetrics,
    pub headroom_metrics_since: Option<DateTime<Utc>>,
    pub auto_update_check: bool,
    pub show_api_key_on_hover: bool,
    pub sync_status: String,
    pub restart_status: String,
    pub routes: Vec<Route>,
    pub last_switch_reason: Option<String>,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod runtime_status_tests {
    use super::{
        AppConfig, ClientPath, ComponentState, RouteHealth, RuntimeMode, RuntimeStatusInput,
        evaluate_runtime_status,
    };

    #[test]
    fn old_config_without_routing_strategy_uses_disabled_defaults() {
        let config: AppConfig = serde_json::from_str(r#"{"enable_codex":true}"#).unwrap();
        assert!(!config.routing_strategy.enabled);
        assert!(config.routing_strategy.observe_only);
        assert!(config.routing_strategy.provider_costs.is_empty());
    }

    #[test]
    fn routing_strategy_costs_are_provider_id_mappings() {
        let config: AppConfig =
            serde_json::from_str(r#"{"routing_strategy":{"provider_costs":{"provider-a":0.25}}}"#)
                .unwrap();
        assert_eq!(
            config.routing_strategy.provider_cost("provider-a"),
            Some(0.25)
        );
        assert!(
            serde_json::from_str::<AppConfig>(
                r#"{"routing_strategy":{"provider_costs":{"provider-a":-1.0}}}"#
            )
            .is_err()
        );
    }

    fn healthy_input() -> RuntimeStatusInput<'static> {
        RuntimeStatusInput {
            codex_enabled: true,
            claude_enabled: true,
            direct_codex: false,
            direct_claude: false,
            bypass_headroom: false,
            codex_route_health: Some(RouteHealth::Healthy),
            claude_route_health: Some(RouteHealth::Healthy),
            headroom_state: "healthy",
            sync_in_progress: false,
            restart_in_progress: false,
            recovery_in_progress: false,
        }
    }

    #[test]
    fn healthy_headroom_routes_are_normal() {
        let status = evaluate_runtime_status(healthy_input());
        assert_eq!(status.mode, RuntimeMode::Normal);
        assert_eq!(status.codex.path, ClientPath::Headroom);
        assert_eq!(status.claude.state, ComponentState::Ready);
        assert_eq!(status.headroom.state, ComponentState::Ready);
    }

    #[test]
    fn unhealthy_required_component_is_degraded() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            headroom_state: "runtime-unavailable",
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Degraded);
        assert_eq!(status.codex.state, ComponentState::Unavailable);
        assert!(status.reason.contains("Headroom 运行环境不可用"));
    }

    #[test]
    fn bypass_and_direct_are_explicit_operating_modes() {
        let bypass = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            headroom_state: "unavailable",
            ..healthy_input()
        });
        assert_eq!(bypass.mode, RuntimeMode::Bypass);
        assert_eq!(bypass.codex.path, ClientPath::Bypass);
        assert_eq!(bypass.headroom.state, ComponentState::NotRequired);

        let direct = evaluate_runtime_status(RuntimeStatusInput {
            direct_codex: true,
            direct_claude: true,
            headroom_state: "unavailable",
            ..healthy_input()
        });
        assert_eq!(direct.mode, RuntimeMode::Direct);
        assert_eq!(direct.codex.path, ClientPath::Direct);
        assert_eq!(direct.headroom.state, ComponentState::NotRequired);
    }

    #[test]
    fn active_recovery_is_observable_without_changing_client_paths() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            restart_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Recovering);
        assert_eq!(status.codex.path, ClientPath::Headroom);
        assert_eq!(status.claude.path, ClientPath::Headroom);
        assert!(status.reason.contains("重启"));
    }

    #[test]
    fn missing_route_degrades_only_the_affected_client() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            codex_route_health: None,
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Degraded);
        assert_eq!(status.codex.state, ComponentState::Unavailable);
        assert_eq!(status.claude.state, ComponentState::Ready);
        assert!(status.reason.starts_with("Codex"));
    }

    #[test]
    fn bypass_mode_is_stable_before_routes_are_verified() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            codex_route_health: Some(RouteHealth::Unknown),
            claude_route_health: Some(RouteHealth::Unknown),
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Bypass);
        assert_eq!(status.codex.path, ClientPath::Bypass);
        assert_eq!(status.codex.state, ComponentState::Checking);
    }

    #[test]
    fn direct_mode_is_stable_before_routes_are_verified() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            direct_codex: true,
            direct_claude: true,
            codex_route_health: Some(RouteHealth::Unknown),
            claude_route_health: Some(RouteHealth::Unknown),
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Direct);
        assert_eq!(status.codex.path, ClientPath::Direct);
        assert_eq!(status.codex.state, ComponentState::Checking);
    }

    #[test]
    fn real_route_failure_outranks_explicit_bypass_mode() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            codex_route_health: Some(RouteHealth::Degraded),
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Degraded);
        assert_eq!(status.codex.state, ComponentState::Degraded);
    }

    #[test]
    fn mixed_direct_and_bypass_clients_show_direct_with_partial_reason() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            direct_codex: true,
            codex_route_health: Some(RouteHealth::Unknown),
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Direct);
        assert_eq!(status.codex.path, ClientPath::Direct);
        assert_eq!(status.claude.path, ClientPath::Bypass);
        assert!(status.reason.contains("部分客户端直连上游"));
    }

    #[test]
    fn sync_recovery_is_observable_without_changing_client_paths() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            sync_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Recovering);
        assert_eq!(status.codex.path, ClientPath::Headroom);
        assert_eq!(status.claude.path, ClientPath::Headroom);
        assert!(status.reason.contains("同步"));
    }

    #[test]
    fn recovery_exits_once_restart_and_sync_finish() {
        let input = healthy_input();
        assert_eq!(
            evaluate_runtime_status(RuntimeStatusInput {
                restart_in_progress: true,
                ..input
            })
            .mode,
            RuntimeMode::Recovering
        );
        assert_eq!(evaluate_runtime_status(input).mode, RuntimeMode::Normal);
    }

    #[test]
    fn restart_and_sync_show_recovering_even_in_direct_mode() {
        let restarting = evaluate_runtime_status(RuntimeStatusInput {
            direct_codex: true,
            direct_claude: true,
            restart_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(restarting.mode, RuntimeMode::Recovering);
        assert!(restarting.reason.contains("重启"));
        assert_eq!(restarting.codex.path, ClientPath::Direct);

        let syncing = evaluate_runtime_status(RuntimeStatusInput {
            direct_codex: true,
            direct_claude: true,
            sync_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(syncing.mode, RuntimeMode::Recovering);
        assert!(syncing.reason.contains("同步"));
        assert_eq!(syncing.codex.path, ClientPath::Direct);
    }

    #[test]
    fn restart_and_sync_show_recovering_even_in_bypass_mode() {
        let restarting = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            restart_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(restarting.mode, RuntimeMode::Recovering);
        assert!(restarting.reason.contains("重启"));
        assert_eq!(restarting.codex.path, ClientPath::Bypass);

        let syncing = evaluate_runtime_status(RuntimeStatusInput {
            bypass_headroom: true,
            sync_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(syncing.mode, RuntimeMode::Recovering);
        assert!(syncing.reason.contains("同步"));
        assert_eq!(syncing.codex.path, ClientPath::Bypass);
    }

    #[test]
    fn real_failure_still_outranks_explicit_recovery_operations() {
        // 真实路由失败在显式重启期间仍保持最高优先级。
        let status = evaluate_runtime_status(RuntimeStatusInput {
            direct_codex: true,
            direct_claude: true,
            codex_route_health: Some(RouteHealth::Degraded),
            restart_in_progress: true,
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Degraded);
        assert_eq!(status.codex.state, ComponentState::Degraded);
    }

    #[test]
    fn headroom_starting_is_recovering_until_ready() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            headroom_state: "starting",
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Recovering);
        assert_eq!(status.headroom.state, ComponentState::Checking);
    }

    #[test]
    fn disabled_clients_are_normal_with_clear_reason() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            codex_enabled: false,
            claude_enabled: false,
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Normal);
        assert!(status.reason.contains("均未启用"));
    }

    #[test]
    fn checking_route_in_normal_mode_is_recovering() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            codex_route_health: Some(RouteHealth::Unknown),
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Recovering);
        assert_eq!(status.codex.state, ComponentState::Checking);
    }

    #[test]
    fn headroom_unavailability_is_reasoned_by_headroom_not_the_client() {
        let status = evaluate_runtime_status(RuntimeStatusInput {
            headroom_state: "unavailable",
            ..healthy_input()
        });
        assert_eq!(status.mode, RuntimeMode::Degraded);
        assert_eq!(status.headroom.state, ComponentState::Unavailable);
        assert!(status.reason.contains("Headroom"));
        assert!(!status.reason.starts_with("Codex"));
    }
}
