use crate::{
    config,
    model::{AppConfig, FailoverPolicy, HeadroomMetrics, Protocol, Route, RouteHealth, Snapshot},
};
use anyhow::Result;
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
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
    pub maintenance_action: Mutex<Option<String>>,
}

impl AppState {
    pub fn new(mut config: AppConfig) -> Arc<Self> {
        let (routes, configured_openai, configured_anthropic, mut error) =
            match config::discover_routes(&config) {
                Ok(found) => (
                    found.routes,
                    found.selected_openai,
                    found.selected_anthropic,
                    None,
                ),
                Err(error) => (Vec::new(), None, None, Some(error.to_string())),
            };
        let openai = valid_provider(&routes, Protocol::OpenAi, configured_openai.as_deref())
            .or_else(|| {
                previous_provider(&config, "active_provider")
                    .filter(|id| provider_exists(&routes, Protocol::OpenAi, id))
            });
        let anthropic = valid_provider(
            &routes,
            Protocol::Anthropic,
            configured_anthropic.as_deref(),
        )
        .or_else(|| {
            previous_provider(&config, "active_anthropic_provider")
                .filter(|id| provider_exists(&routes, Protocol::Anthropic, id))
        });
        let active = select_index(&routes, Protocol::OpenAi, openai.as_deref());
        let active_anthropic = select_index(&routes, Protocol::Anthropic, anthropic.as_deref());
        let actual_openai = active
            .and_then(|index| routes.get(index))
            .map(|route| route.provider.clone());
        let actual_anthropic = active_anthropic
            .and_then(|index| routes.get(index))
            .map(|route| route.provider.clone());
        if config.selected_openai_provider != actual_openai
            || config.selected_anthropic_provider != actual_anthropic
        {
            config.selected_openai_provider = actual_openai.clone();
            config.selected_anthropic_provider = actual_anthropic.clone();
            let path = config.state_dir.join("config.json");
            if let Err(save_error) = config::save(&path, &config) {
                error = Some(format!("修复上游选择失败: {save_error}"));
            }
        }
        Arc::new(Self {
            inner: Mutex::new(RuntimeState {
                config,
                routes,
                active,
                active_anthropic,
                selected_provider: actual_openai,
                selected_anthropic_provider: actual_anthropic,
                headroom_state: "检测中".into(),
                headroom_pid: None,
                headroom_metrics: HeadroomMetrics::default(),
                last_switch_reason: None,
                last_error: error,
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
            maintenance_action: Mutex::new(None),
        })
    }

    pub fn begin_sync(&self) -> bool {
        if self
            .sync_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        *self.sync_status.lock().unwrap() = "同步中".into();
        true
    }

    pub fn finish_sync(&self, ok: bool, message: String) {
        *self.sync_status.lock().unwrap() = if ok {
            "同步完成".into()
        } else {
            "同步失败".into()
        };
        *self.sync_result.lock().unwrap() = Some((ok, message));
        self.sync_in_progress.store(false, Ordering::Release);
    }

    pub fn take_sync_result(&self) -> Option<(bool, String)> {
        self.sync_result.lock().unwrap().take()
    }

    pub fn begin_restart(&self) -> bool {
        if self
            .restart_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        *self.restart_status.lock().unwrap() = "重启中".into();
        true
    }

    pub fn finish_restart(&self, ok: bool, message: String) {
        *self.restart_status.lock().unwrap() = if ok {
            "重启完成".into()
        } else {
            "重启失败".into()
        };
        *self.restart_result.lock().unwrap() = Some((ok, message));
        self.restart_in_progress.store(false, Ordering::Release);
    }

    pub fn take_restart_result(&self) -> Option<(bool, String)> {
        self.restart_result.lock().unwrap().take()
    }

    pub fn take_model_change_notice(&self) -> Option<String> {
        self.model_change_notice.lock().unwrap().take()
    }

    pub fn take_auto_switch_notice(&self) -> Option<String> {
        self.auto_switch_notice.lock().unwrap().take()
    }
    pub fn take_runtime_result(&self) -> Option<(bool, String)> {
        self.runtime_result.lock().unwrap().take()
    }

    pub fn take_config_change_notice(&self) -> Option<String> {
        self.config_change_notice.lock().unwrap().take()
    }

    pub fn take_routing_notice(&self) -> Option<(bool, String)> {
        self.routing_notice.lock().unwrap().take()
    }

    pub fn take_update_notice(&self) -> Option<String> {
        self.update_notice.lock().unwrap().take()
    }

    pub fn toggle_auto_failover(&self) -> Result<bool> {
        let (enabled, path, saved) = {
            let mut state = self.inner.lock().unwrap();
            state.config.auto_failover = !state.config.auto_failover;
            (
                state.config.auto_failover,
                state.config.state_dir.join("config.json"),
                state.config.clone(),
            )
        };
        if let Err(error) = config::save(&path, &saved) {
            let mut state = self.inner.lock().unwrap();
            state.config.auto_failover = !enabled;
            state.last_error = Some(format!("保存自动切换设置失败: {error}"));
            return Err(error);
        }
        Ok(enabled)
    }

    pub fn reload_failover_policy(&self) -> Result<(usize, usize)> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .config
            .state_dir
            .join("config.json");
        let policy = config::load_or_create(&path)?.failover_policy;
        let counts = policy.counts();
        self.inner.lock().unwrap().config.failover_policy = policy;
        Ok(counts)
    }

    pub fn save_failover_settings(
        &self,
        policy: FailoverPolicy,
        auto_failover: bool,
    ) -> Result<(usize, usize)> {
        let (path, mut saved) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.state_dir.join("config.json"),
                state.config.clone(),
            )
        };
        saved.failover_policy = policy.clone();
        saved.auto_failover = auto_failover;
        config::save(&path, &saved)?;
        let counts = policy.counts();
        let mut state = self.inner.lock().unwrap();
        state.config.failover_policy = policy;
        state.config.auto_failover = auto_failover;
        Ok(counts)
    }

    pub fn toggle_headroom_bypass(&self) -> Result<bool> {
        let (current, mut updated, path, preferred) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.clone(),
                state.config.clone(),
                state.config.state_dir.join("config.json"),
                active_route(&state, Protocol::OpenAi).map(|route| route.base_url.clone()),
            )
        };
        updated.bypass_headroom = !updated.bypass_headroom;
        if let Err(error) = config::sync_all(&updated, preferred.as_deref())
            .and_then(|_| config::save(&path, &updated))
        {
            let _ = config::sync_all(&current, preferred.as_deref());
            self.inner.lock().unwrap().last_error =
                Some(format!("切换 Headroom 模式失败: {error}"));
            return Err(error);
        }
        let enabled = updated.bypass_headroom;
        self.inner.lock().unwrap().config = updated;
        Ok(enabled)
    }

    pub fn reset_headroom_metrics(&self) -> Result<()> {
        let (mut updated, path, log_file) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.clone(),
                state.config.state_dir.join("config.json"),
                state.config.state_dir.join("headroom-proxy.jsonl"),
            )
        };
        updated.metrics_log_offset = fs::metadata(log_file)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        updated.metrics_since = Some(chrono::Utc::now());
        config::save(&path, &updated)?;
        let mut state = self.inner.lock().unwrap();
        state.config = updated;
        state.headroom_metrics = HeadroomMetrics::default();
        drop(state);
        self.reset_metrics.store(true, Ordering::Release);
        Ok(())
    }

    pub fn toggle_auto_update_check(&self) -> Result<bool> {
        let (enabled, path, saved) = {
            let mut state = self.inner.lock().unwrap();
            state.config.auto_check_updates = !state.config.auto_check_updates;
            (
                state.config.auto_check_updates,
                state.config.state_dir.join("config.json"),
                state.config.clone(),
            )
        };
        if let Err(error) = config::save(&path, &saved) {
            self.inner.lock().unwrap().config.auto_check_updates = !enabled;
            return Err(error);
        }
        Ok(enabled)
    }

    pub fn begin_daily_update_check(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<AppConfig>> {
        let (path, saved) = {
            let mut state = self.inner.lock().unwrap();
            if !update_check_due(
                state.config.auto_check_updates,
                state.config.last_update_check,
                now,
            ) {
                return Ok(None);
            }
            state.config.last_update_check = Some(now);
            (
                state.config.state_dir.join("config.json"),
                state.config.clone(),
            )
        };
        config::save(&path, &saved)?;
        Ok(Some(saved))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_unlocked(&self.inner.lock().unwrap())
    }
    pub fn active_route(&self) -> Option<Route> {
        self.active_route_for(Protocol::OpenAi)
    }
    pub fn active_route_for(&self, protocol: Protocol) -> Option<Route> {
        let state = self.inner.lock().unwrap();
        let index = if protocol == Protocol::OpenAi {
            state.active
        } else {
            state.active_anthropic
        };
        index.and_then(|value| state.routes.get(value).cloned())
    }
    pub fn active_route_for_path(&self, path: &str) -> Option<Route> {
        self.active_route_for(protocol_for_path(path))
    }
    pub fn active_url(&self) -> Option<String> {
        self.active_route().map(|route| route.base_url)
    }
    pub fn route_summary(&self, protocol: Protocol) -> String {
        let state = self.inner.lock().unwrap();
        route_summary(active_route(&state, protocol))
    }
    pub fn recovery_hint(&self) -> &'static str {
        let state = self.inner.lock().unwrap();
        recovery_hint(
            active_route(&state, Protocol::OpenAi),
            active_route(&state, Protocol::Anthropic),
            &state.headroom_state,
            state.last_error.as_deref(),
        )
    }

    pub fn switch_index(&self, index: usize, reason: &str) -> bool {
        let (protocol, provider, app_config) = {
            let state = self.inner.lock().unwrap();
            let Some(route) = state.routes.get(index) else {
                return false;
            };
            (route.protocol, route.provider.clone(), state.config.clone())
        };
        let model_notice = match config::sync_provider_models(&app_config, protocol, &provider) {
            Ok(notice) => notice,
            Err(error) => {
                self.inner.lock().unwrap().last_error =
                    Some(format!("同步目标模型配置失败: {error}"));
                return false;
            }
        };
        let mut state = self.inner.lock().unwrap();
        if !state
            .routes
            .get(index)
            .is_some_and(|route| route.protocol == protocol && route.provider == provider)
        {
            return false;
        }
        if protocol == Protocol::OpenAi {
            state.active = Some(index);
            state.selected_provider = Some(provider.clone());
            state.config.selected_openai_provider = Some(provider);
        } else {
            state.active_anthropic = Some(index);
            state.selected_anthropic_provider = Some(provider.clone());
            state.config.selected_anthropic_provider = Some(provider);
        }
        state.last_switch_reason = Some(format!("{}：{}", protocol.label(), reason));
        let path = state.config.state_dir.join("config.json");
        let saved = state.config.clone();
        drop(state);
        if let Err(error) = config::save(&path, &saved) {
            self.inner.lock().unwrap().last_error = Some(format!("保存上游选择失败: {error}"));
        }
        if let Some(notice) = model_notice {
            *self.model_change_notice.lock().unwrap() = Some(notice);
        }
        true
    }

    pub fn switch_next(&self) -> bool {
        let state = self.inner.lock().unwrap();
        let indices: Vec<usize> = state
            .routes
            .iter()
            .enumerate()
            .filter_map(|(i, r)| (r.protocol == Protocol::OpenAi).then_some(i))
            .collect();
        if indices.is_empty() {
            return false;
        }
        let current = state
            .active
            .and_then(|active| indices.iter().position(|value| *value == active))
            .unwrap_or(0);
        let next = indices[(current + 1) % indices.len()];
        drop(state);
        self.switch_index(next, "托盘手动切换")
    }

    pub fn refresh_routes(&self) {
        let config = self.inner.lock().unwrap().config.clone();
        match config::discover_routes(&config) {
            Ok(found) => {
                let mut state = self.inner.lock().unwrap();
                let old_openai = state
                    .active
                    .and_then(|i| state.routes.get(i))
                    .map(|r| r.provider.clone());
                let old_anthropic = state
                    .active_anthropic
                    .and_then(|i| state.routes.get(i))
                    .map(|r| r.provider.clone());
                let mut routes = found.routes;
                for route in &mut routes {
                    if let Some(old) = state.routes.iter().find(|old| {
                        old.protocol == route.protocol && old.provider == route.provider
                    }) {
                        route.state = old.state;
                        route.score = old.score;
                        route.latency_ms = old.latency_ms;
                        route.consecutive_successes = old.consecutive_successes;
                        route.consecutive_failures = old.consecutive_failures;
                        route.verified_by_request = old.verified_by_request;
                        route.last_error = old.last_error.clone();
                        route.last_status_code = old.last_status_code;
                        route.last_success_at = old.last_success_at;
                        route.failover_blocked_until = old.failover_blocked_until;
                    }
                }
                let active = preserve_index(
                    &routes,
                    Protocol::OpenAi,
                    old_openai,
                    found.selected_openai.as_deref(),
                );
                let active_anthropic = preserve_index(
                    &routes,
                    Protocol::Anthropic,
                    old_anthropic,
                    found.selected_anthropic.as_deref(),
                );
                let actual_openai = active
                    .and_then(|index| routes.get(index))
                    .map(|route| route.provider.clone());
                let actual_anthropic = active_anthropic
                    .and_then(|index| routes.get(index))
                    .map(|route| route.provider.clone());
                let selection_changed = state.config.selected_openai_provider != actual_openai
                    || state.config.selected_anthropic_provider != actual_anthropic;
                state.active = active;
                state.active_anthropic = active_anthropic;
                state.selected_provider = actual_openai.clone();
                state.selected_anthropic_provider = actual_anthropic.clone();
                state.config.selected_openai_provider = actual_openai;
                state.config.selected_anthropic_provider = actual_anthropic;
                state.routes = routes;
                state.last_error = None;
                let path = state.config.state_dir.join("config.json");
                let saved = state.config.clone();
                drop(state);
                if selection_changed && let Err(error) = config::save(&path, &saved) {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("修复上游选择失败: {error}"));
                }
            }
            Err(error) => self.inner.lock().unwrap().last_error = Some(error.to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_route_result(
        &self,
        protocol: Protocol,
        provider: &str,
        ok: bool,
        latency: u64,
        status: Option<u16>,
        error: Option<String>,
        request: bool,
    ) {
        let failover = {
            let mut state = self.inner.lock().unwrap();
            let Some(failed) = state
                .routes
                .iter()
                .position(|r| r.protocol == protocol && r.provider == provider)
            else {
                return;
            };
            state.routes[failed].record(ok, latency, status, error, request);
            let active = if protocol == Protocol::OpenAi {
                state.active
            } else {
                state.active_anthropic
            };
            if !state.config.auto_failover
                || active != Some(failed)
                || state.routes[failed].state != RouteHealth::Unavailable
            {
                None
            } else {
                failover_candidate(
                    &state.routes,
                    protocol,
                    failed,
                    &state.config.failover_policy,
                )
                .map(|next| {
                    (
                        next,
                        failed,
                        state.routes[failed].name.clone(),
                        state.routes[next].name.clone(),
                    )
                })
            }
        };
        if let Some((next, failed_index, failed, replacement)) = failover
            && self.switch_index(next, "自动故障切换（连续 3 次失败）")
        {
            self.inner.lock().unwrap().routes[failed_index].failover_blocked_until =
                Some(chrono::Utc::now() + chrono::Duration::minutes(5));
            *self.auto_switch_notice.lock().unwrap() = Some(format!(
                "{}：{} 连续 3 次失败，已切换到 {}；故障线路将冷却 5 分钟",
                protocol.label(),
                failed,
                replacement
            ));
        }
    }

    pub fn write_status(&self) -> Result<()> {
        let snapshot = self.snapshot();
        let (state_dir, legacy, port) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.state_dir.clone(),
                state.config.legacy_state_dir.clone(),
                state.config.agent_port,
            )
        };
        for dir in [&state_dir, &legacy] {
            fs::create_dir_all(dir)?;
            atomic_write(
                &dir.join("status.json"),
                &serde_json::to_vec_pretty(&snapshot)?,
            )?;
            let ini = format!(
                "[status]\r\nstate={}\r\nactive_provider={}\r\nactive_host={}\r\nclaude_provider={}\r\nclaude_host={}\r\nlatency_ms={}\r\nscore={}\r\nauto_enabled={}\r\nheadroom_state={}\r\ninflight=0\r\nroute_count={}\r\nlast_error={}\r\n",
                snapshot.state,
                snapshot.active_name.as_deref().unwrap_or("--"),
                snapshot.active_host.as_deref().unwrap_or("--"),
                snapshot.active_anthropic_name.as_deref().unwrap_or("--"),
                snapshot.active_anthropic_host.as_deref().unwrap_or("--"),
                snapshot
                    .latency_ms
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                snapshot.active_score,
                snapshot.auto_enabled,
                snapshot.headroom_state,
                snapshot.routes.len().min(32),
                snapshot.last_error.as_deref().unwrap_or("")
            );
            let mut utf16 = vec![0xff, 0xfe];
            utf16.extend(ini.encode_utf16().flat_map(u16::to_le_bytes));
            atomic_write(&dir.join("status.ini"), &utf16)?;
            atomic_write(
                &dir.join("runtime.json"),
                &serde_json::to_vec_pretty(&json!({"service":"headroom-route","port":port}))?,
            )?;
        }
        Ok(())
    }

    pub fn diagnostic_text(&self) -> String {
        let state = self.inner.lock().unwrap();
        let openai = active_route(&state, Protocol::OpenAi);
        let anthropic = active_route(&state, Protocol::Anthropic);
        format!(
            "Headroom Route {}\r\n模式: {}\r\nCodex: {} [{}]\r\nClaude: {} [{}]\r\nCC-Switch: {} [{}]\r\nAgent: 127.0.0.1:{}\r\nHeadroom: 127.0.0.1:{} ({}, PID={})\r\n统计范围: {}\r\n压缩 Token: {} -> {}，节省 {} ({:.1}%)\r\n完成请求: {}，失败 {} ({:.1}%)\r\nCodex 上游: {}\r\nClaude 上游: {}\r\n路由数: {}\r\n自动切换: {}\r\n最近错误: {}\r\n恢复建议: {}",
            env!("CARGO_PKG_VERSION"),
            if state.config.bypass_headroom {
                "旁路 Headroom"
            } else {
                "经过 Headroom"
            },
            state.config.codex_config.display(),
            availability(openai, transport_ready(&state)),
            state.config.claude_settings.display(),
            availability(anthropic, transport_ready(&state)),
            state.config.cc_switch_db.display(),
            yes(state.config.cc_switch_db.exists()),
            state.config.agent_port,
            state.config.headroom_port,
            state.headroom_state,
            state
                .headroom_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "--".into()),
            metrics_scope(state.config.metrics_since),
            state.headroom_metrics.input_tokens_original,
            state.headroom_metrics.input_tokens_optimized,
            state.headroom_metrics.tokens_saved,
            state.headroom_metrics.compression_percent(),
            state.headroom_metrics.completed_requests,
            state.headroom_metrics.failed_requests,
            state.headroom_metrics.failure_percent(),
            route_summary(openai),
            route_summary(anthropic),
            state.routes.len(),
            yes(state.config.auto_failover),
            state.last_error.as_deref().unwrap_or("无"),
            recovery_hint(
                openai,
                anthropic,
                &state.headroom_state,
                state.last_error.as_deref()
            )
        )
    }

    fn snapshot_unlocked(&self, state: &RuntimeState) -> Snapshot {
        let openai = state.active.and_then(|i| state.routes.get(i));
        let anthropic = state.active_anthropic.and_then(|i| state.routes.get(i));
        let health = combined_health(openai.map(|r| r.state), anthropic.map(|r| r.state));
        Snapshot {
            service: "headroom-route",
            version: env!("CARGO_PKG_VERSION"),
            state: match health {
                RouteHealth::Healthy => "healthy",
                RouteHealth::Degraded => "degraded",
                RouteHealth::Unknown => "unknown",
                RouteHealth::Unavailable => "unavailable",
            },
            active_provider: openai.map(|r| r.provider.clone()),
            active_name: openai.map(|r| r.name.clone()),
            active_url: openai.map(|r| r.base_url.clone()),
            active_host: openai.map(Route::host),
            active_score: openai.map(|r| r.score).unwrap_or(0),
            latency_ms: openai.and_then(|r| r.latency_ms),
            codex_availability: availability(openai, transport_ready(state)),
            active_anthropic_provider: anthropic.map(|r| r.provider.clone()),
            active_anthropic_name: anthropic.map(|r| r.name.clone()),
            active_anthropic_url: anthropic.map(|r| r.base_url.clone()),
            active_anthropic_host: anthropic.map(Route::host),
            active_anthropic_score: anthropic.map(|r| r.score).unwrap_or(0),
            anthropic_latency_ms: anthropic.and_then(|r| r.latency_ms),
            claude_availability: availability(anthropic, transport_ready(state)),
            auto_enabled: state.config.auto_failover,
            bypass_headroom: state.config.bypass_headroom,
            headroom_state: state.headroom_state.clone(),
            headroom_pid: state.headroom_pid,
            headroom_metrics: state.headroom_metrics,
            headroom_metrics_since: state.config.metrics_since,
            auto_update_check: state.config.auto_check_updates,
            sync_status: self.sync_status.lock().unwrap().clone(),
            restart_status: self.restart_status.lock().unwrap().clone(),
            routes: state.routes.clone(),
            last_switch_reason: state.last_switch_reason.clone(),
            last_error: state.last_error.clone(),
        }
    }
}

fn transport_ready(state: &RuntimeState) -> bool {
    state.config.bypass_headroom || matches!(state.headroom_state.as_str(), "healthy" | "external")
}

fn availability(route: Option<&Route>, transport_ready: bool) -> &'static str {
    let Some(route) = route else {
        return "未配置";
    };
    if !transport_ready {
        return "不可用";
    }
    match route.state {
        RouteHealth::Healthy => "可用",
        RouteHealth::Degraded => "降级",
        RouteHealth::Unavailable => "不可用",
        RouteHealth::Unknown => "待验证",
    }
}

fn metrics_scope(since: Option<chrono::DateTime<chrono::Utc>>) -> String {
    since.map_or_else(
        || "当前日志文件累计".into(),
        |since| format!("自 {} UTC", since.format("%Y-%m-%d %H:%M:%S")),
    )
}

fn update_check_due(
    enabled: bool,
    last: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    enabled && last.is_none_or(|last| now.signed_duration_since(last) >= chrono::Duration::days(1))
}

fn select_index(routes: &[Route], protocol: Protocol, selected: Option<&str>) -> Option<usize> {
    selected
        .and_then(|id| {
            routes
                .iter()
                .position(|r| r.protocol == protocol && r.provider == id)
        })
        .or_else(|| routes.iter().position(|r| r.protocol == protocol))
}
fn provider_exists(routes: &[Route], protocol: Protocol, provider: &str) -> bool {
    routes
        .iter()
        .any(|route| route.protocol == protocol && route.provider == provider)
}
fn valid_provider(routes: &[Route], protocol: Protocol, provider: Option<&str>) -> Option<String> {
    provider
        .filter(|id| provider_exists(routes, protocol, id))
        .map(str::to_owned)
}
fn previous_provider(config: &AppConfig, key: &str) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(config.state_dir.join("status.json")).ok()?).ok()?;
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}
fn preserve_index(
    routes: &[Route],
    protocol: Protocol,
    old: Option<String>,
    selected: Option<&str>,
) -> Option<usize> {
    old.as_deref()
        .and_then(|provider| {
            routes
                .iter()
                .position(|r| r.protocol == protocol && r.provider == provider)
        })
        .or_else(|| select_index(routes, protocol, selected))
}
fn failover_candidate(
    routes: &[Route],
    protocol: Protocol,
    failed: usize,
    policy: &FailoverPolicy,
) -> Option<usize> {
    let eligible = |index: usize, route: &Route| {
        index != failed
            && route.protocol == protocol
            && route.state == RouteHealth::Healthy
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
fn route_summary(route: Option<&Route>) -> String {
    route
        .map(|route| {
            format!(
                "{} · {} · {} ms · HTTP {} · {}",
                route.name,
                route.evidence_label(),
                route
                    .latency_ms
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "--".into()),
                route
                    .last_status_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "--".into()),
                route.last_error.as_deref().unwrap_or("无错误")
            )
        })
        .unwrap_or_else(|| "未配置".into())
}
fn recovery_hint(
    openai: Option<&Route>,
    anthropic: Option<&Route>,
    headroom: &str,
    last_error: Option<&str>,
) -> &'static str {
    if matches!(headroom, "unavailable" | "runtime-unavailable") {
        return "从托盘重启 Headroom；仍失败时按 README 检查外部运行环境";
    }
    let routes = [openai, anthropic];
    if routes
        .iter()
        .flatten()
        .any(|route| matches!(route.last_status_code, Some(401 | 403)))
    {
        return "检查当前 Provider 的 API Key 或登录状态";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| route.last_status_code == Some(429))
    {
        return "上游正在限流；启用自动切换或稍后重试";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| route.state == RouteHealth::Unavailable)
    {
        return "立即检查上游与系统代理，并启用自动切换";
    }
    if routes.iter().any(Option::is_none) {
        return "同步配置并确认 Codex、Claude 或 CC-Switch Provider 已配置";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| matches!(route.state, RouteHealth::Unknown | RouteHealth::Degraded))
    {
        return "从托盘立即检查上游，等待健康状态确认";
    }
    if last_error.is_some() {
        return "复制脱敏诊断报告并打开日志目录查看详情";
    }
    "当前无需操作"
}
fn combined_health(a: Option<RouteHealth>, b: Option<RouteHealth>) -> RouteHealth {
    let values = [a, b];
    if values.iter().flatten().any(|v| *v == RouteHealth::Healthy) {
        RouteHealth::Healthy
    } else if values.iter().flatten().any(|v| *v == RouteHealth::Degraded) {
        RouteHealth::Degraded
    } else if values.iter().flatten().any(|v| *v == RouteHealth::Unknown) {
        RouteHealth::Unknown
    } else {
        RouteHealth::Unavailable
    }
}
fn protocol_for_path(path: &str) -> Protocol {
    if path == "/api/oauth/usage" || path.starts_with("/v1/messages") {
        Protocol::Anthropic
    } else {
        Protocol::OpenAi
    }
}
fn yes(value: bool) -> &'static str {
    if value { "是" } else { "否" }
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
        AppState, RuntimeState, availability, failover_candidate, preserve_index,
        protocol_for_path, recovery_hint, route_summary, update_check_due,
    };
    use crate::model::{
        AppConfig, AuthStyle, FailoverPolicy, HeadroomMetrics, Protocol, Route, RouteHealth,
    };
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

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
            failover_candidate(&routes, Protocol::OpenAi, 0, &policy),
            Some(1)
        );

        policy
            .openai
            .insert("primary".into(), vec!["missing".into()]);
        assert_eq!(
            failover_candidate(&routes, Protocol::OpenAi, 0, &policy),
            None
        );
        assert_eq!(
            failover_candidate(&routes, Protocol::OpenAi, 0, &FailoverPolicy::default()),
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
