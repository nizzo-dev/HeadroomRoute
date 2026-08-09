use crate::routing_policy::RoutingStrategyConfig;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

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
    /// Apply the selected provider to Codex and bypass the local route agent.
    pub direct_codex: bool,
    /// Apply the selected provider to Claude Code and bypass the local agent.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    Normal,
    Degraded,
    Bypass,
    Direct,
    Recovering,
}

impl RuntimeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "正常",
            Self::Degraded => "降级",
            Self::Bypass => "旁路",
            Self::Direct => "直连",
            Self::Recovering => "恢复中",
        }
    }

    pub fn health_key(self) -> &'static str {
        match self {
            Self::Normal | Self::Bypass | Self::Direct => "healthy",
            Self::Degraded => "degraded",
            Self::Recovering => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientPath {
    Disabled,
    Headroom,
    Bypass,
    Direct,
}

impl ClientPath {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "未启用",
            Self::Headroom => "经 Headroom",
            Self::Bypass => "旁路 Headroom",
            Self::Direct => "直连上游",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentState {
    Disabled,
    NotRequired,
    Ready,
    Checking,
    Degraded,
    Unavailable,
}

impl ComponentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "未启用",
            Self::NotRequired => "不需要",
            Self::Ready => "可用",
            Self::Checking => "检测中",
            Self::Degraded => "降级",
            Self::Unavailable => "不可用",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRuntimeStatus {
    pub path: ClientPath,
    pub state: ComponentState,
    pub reason: String,
}

impl ClientRuntimeStatus {
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {}",
            self.path.label(),
            self.state.label(),
            self.reason
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HeadroomRuntimeStatus {
    pub state: ComponentState,
    pub reason: String,
}

impl HeadroomRuntimeStatus {
    pub fn summary(&self) -> String {
        format!("{} · {}", self.state.label(), self.reason)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeStatus {
    pub mode: RuntimeMode,
    pub reason: String,
    pub codex: ClientRuntimeStatus,
    pub claude: ClientRuntimeStatus,
    pub headroom: HeadroomRuntimeStatus,
}

impl RuntimeStatus {
    pub fn summary(&self) -> String {
        format!("{} · {}", self.mode.label(), self.reason)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeStatusInput<'a> {
    pub codex_enabled: bool,
    pub claude_enabled: bool,
    pub direct_codex: bool,
    pub direct_claude: bool,
    pub bypass_headroom: bool,
    pub codex_route_health: Option<RouteHealth>,
    pub claude_route_health: Option<RouteHealth>,
    pub headroom_state: &'a str,
    pub sync_in_progress: bool,
    pub restart_in_progress: bool,
    pub recovery_in_progress: bool,
}

/// 五种运行模式的统一判定，是托盘、完整状态、预检与诊断报告共用的单一强类型来源。
///
/// 优先级从高到低（首次命中的分支即当前模式；条件变化后重新求值即进入/退出，
/// 无需手动迁移，`reason` 即为可读的迁移原因）：
///
/// 1. `Degraded`（降级）：必需且正在使用的组件失败 —— 需要 Headroom 但 Headroom
///    不可用，或任一启用客户端的当前路由失败/不可用/未配置。真实请求已受影响，
///    故优先级最高；退出条件是失败组件恢复为可用/待验证。
/// 2. `Recovering`（恢复中，显式操作）：重启或同步进行中。这是用户可见的进行中
///    操作，即使在直连/旁路拓扑下也应在顶层显示；退出条件是操作完成。
/// 3. `Direct`（直连）：任一启用客户端直连上游（不经本地代理，也必然不经 Headroom）。
///    显式拓扑，优先于校验态；退出条件是全部直连客户端恢复经代理路由。
/// 4. `Bypass`（旁路）：`bypass_headroom` 开启且存在启用但未直连的客户端。显式拓扑，
///    优先于校验态；退出条件是旁路开关关闭。
/// 5. `Recovering`（恢复中，校验态）：必需组件仍在检测（Headroom 启动、Headroom
///    路径路由等待验证）。仅 `RouteHealth::Unknown` 会进入此态，不打断稳定的
///    Direct/Bypass；仅作观察层：不改变实际请求路径，也不阻塞真实请求；退出条件是
///    操作完成或组件进入就绪/失败态。
/// 6. `Normal`（正常）：所有启用客户端与所需组件均可用，且无进行中操作。
pub fn evaluate_runtime_status(input: RuntimeStatusInput<'_>) -> RuntimeStatus {
    let codex_path = client_path(
        input.codex_enabled,
        input.direct_codex,
        input.bypass_headroom,
    );
    let claude_path = client_path(
        input.claude_enabled,
        input.direct_claude,
        input.bypass_headroom,
    );
    let headroom_required =
        matches!(codex_path, ClientPath::Headroom) || matches!(claude_path, ClientPath::Headroom);
    let headroom = headroom_status(headroom_required, input.headroom_state);
    let codex = client_status(codex_path, input.codex_route_health, &headroom);
    let claude = client_status(claude_path, input.claude_route_health, &headroom);

    let headroom_failed = headroom_required && headroom.state == ComponentState::Unavailable;
    let client_failed = [(&codex, "Codex"), (&claude, "Claude")]
        .into_iter()
        .find(|(status, _)| {
            matches!(
                status.state,
                ComponentState::Degraded | ComponentState::Unavailable
            )
        });
    let bypass = input.bypass_headroom
        && [codex_path, claude_path]
            .into_iter()
            .any(|path| path == ClientPath::Bypass);
    let direct = [codex_path, claude_path]
        .into_iter()
        .any(|path| path == ClientPath::Direct);

    let (mode, reason) = if headroom_failed {
        (RuntimeMode::Degraded, headroom.reason.clone())
    } else if let Some((status, name)) = client_failed {
        (RuntimeMode::Degraded, format!("{name}：{}", status.reason))
    } else if input.restart_in_progress {
        (RuntimeMode::Recovering, "正在重启 Headroom".into())
    } else if input.sync_in_progress {
        (RuntimeMode::Recovering, "正在同步客户端路由配置".into())
    } else if input.recovery_in_progress {
        (RuntimeMode::Recovering, "正在恢复本地运行环境".into())
    } else if direct {
        let all_enabled_direct = [codex_path, claude_path]
            .into_iter()
            .filter(|path| *path != ClientPath::Disabled)
            .all(|path| path == ClientPath::Direct);
        (
            RuntimeMode::Direct,
            if all_enabled_direct {
                "所有启用的客户端均直连上游".into()
            } else {
                "部分客户端直连上游，其余客户端保持当前路径".into()
            },
        )
    } else if bypass {
        (
            RuntimeMode::Bypass,
            "启用的非直连客户端已旁路 Headroom".into(),
        )
    } else if headroom.state == ComponentState::Checking {
        (RuntimeMode::Recovering, headroom.reason.clone())
    } else if let Some((status, name)) = [(&codex, "Codex"), (&claude, "Claude")]
        .into_iter()
        .find(|(status, _)| status.state == ComponentState::Checking)
    {
        (
            RuntimeMode::Recovering,
            format!("{name}：{}", status.reason),
        )
    } else {
        let any_enabled = [codex_path, claude_path]
            .into_iter()
            .any(|path| path != ClientPath::Disabled);
        (
            RuntimeMode::Normal,
            if any_enabled {
                "所有启用的客户端与所需组件均可用".into()
            } else {
                "Codex 与 Claude 均未启用".into()
            },
        )
    };

    RuntimeStatus {
        mode,
        reason,
        codex,
        claude,
        headroom,
    }
}

fn client_path(enabled: bool, direct: bool, bypass_headroom: bool) -> ClientPath {
    if !enabled {
        ClientPath::Disabled
    } else if direct {
        ClientPath::Direct
    } else if bypass_headroom {
        ClientPath::Bypass
    } else {
        ClientPath::Headroom
    }
}

fn headroom_status(required: bool, state: &str) -> HeadroomRuntimeStatus {
    if !required {
        return HeadroomRuntimeStatus {
            state: ComponentState::NotRequired,
            reason: "当前客户端路径不经过 Headroom".into(),
        };
    }
    let (state, reason) = match state {
        "healthy" => (ComponentState::Ready, "本地 Headroom 正常"),
        "external" => (ComponentState::Ready, "已连接外部 Headroom"),
        "检测中" | "运行环境就绪" | "starting" | "restarting" => {
            (ComponentState::Checking, "Headroom 正在启动或恢复")
        }
        "runtime-unavailable" => (ComponentState::Unavailable, "Headroom 运行环境不可用"),
        _ => (ComponentState::Unavailable, "Headroom 服务不可用"),
    };
    HeadroomRuntimeStatus {
        state,
        reason: reason.into(),
    }
}

fn client_status(
    path: ClientPath,
    route_health: Option<RouteHealth>,
    headroom: &HeadroomRuntimeStatus,
) -> ClientRuntimeStatus {
    if path == ClientPath::Disabled {
        return ClientRuntimeStatus {
            path,
            state: ComponentState::Disabled,
            reason: "客户端未启用".into(),
        };
    }
    let Some(route_health) = route_health else {
        return ClientRuntimeStatus {
            path,
            state: ComponentState::Unavailable,
            reason: "未配置可用路由".into(),
        };
    };
    if path == ClientPath::Headroom && headroom.state != ComponentState::Ready {
        return ClientRuntimeStatus {
            path,
            state: headroom.state,
            reason: headroom.reason.clone(),
        };
    }
    let (state, reason) = match route_health {
        RouteHealth::Healthy => (ComponentState::Ready, "当前路由已验证"),
        RouteHealth::Unknown => (ComponentState::Checking, "当前路由等待验证"),
        RouteHealth::Degraded => (ComponentState::Degraded, "当前路由出现失败"),
        RouteHealth::Unavailable => (ComponentState::Unavailable, "当前路由不可用"),
    };
    ClientRuntimeStatus {
        path,
        state,
        reason: reason.into(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
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
