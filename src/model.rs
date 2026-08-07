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

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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
    pub service: &'static str,
    pub version: &'static str,
    pub state: &'static str,
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
