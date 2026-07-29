use crate::{config, model::{AppConfig, Protocol, Route, RouteHealth, Snapshot}};
use anyhow::Result;
use serde_json::json;
use std::{fs, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}};

pub struct RuntimeState {
    pub config: AppConfig,
    pub routes: Vec<Route>,
    pub active: Option<usize>,
    pub active_anthropic: Option<usize>,
    pub selected_provider: Option<String>,
    pub selected_anthropic_provider: Option<String>,
    pub headroom_state: String,
    pub headroom_pid: Option<u32>,
    pub last_switch_reason: Option<String>,
    pub last_error: Option<String>,
}

pub struct AppState {
    pub inner: Mutex<RuntimeState>,
    pub stop: AtomicBool,
    pub restart_headroom: AtomicBool,
    pub force_probe: AtomicBool,
    pub sync_in_progress: AtomicBool,
    pub sync_status: Mutex<String>,
    pub sync_result: Mutex<Option<(bool, String)>>,
    pub restart_in_progress: AtomicBool,
    pub restart_status: Mutex<String>,
    pub restart_result: Mutex<Option<(bool, String)>>,
    pub maintenance_action: Mutex<Option<String>>,
}

impl AppState {
    pub fn new(mut config: AppConfig) -> Arc<Self> {
        config.auto_failover = false;
        let (routes, configured_openai, configured_anthropic, mut error) = match config::discover_routes(&config) {
            Ok(found) => (found.routes, found.selected_openai, found.selected_anthropic, None),
            Err(error) => (Vec::new(), None, None, Some(error.to_string())),
        };
        let openai = valid_provider(&routes, Protocol::OpenAi, configured_openai.as_deref())
            .or_else(|| previous_provider(&config, "active_provider").filter(|id| provider_exists(&routes, Protocol::OpenAi, id)));
        let anthropic = valid_provider(&routes, Protocol::Anthropic, configured_anthropic.as_deref())
            .or_else(|| previous_provider(&config, "active_anthropic_provider").filter(|id| provider_exists(&routes, Protocol::Anthropic, id)));
        let active = select_index(&routes, Protocol::OpenAi, openai.as_deref());
        let active_anthropic = select_index(&routes, Protocol::Anthropic, anthropic.as_deref());
        let actual_openai = active.and_then(|index| routes.get(index)).map(|route| route.provider.clone());
        let actual_anthropic = active_anthropic.and_then(|index| routes.get(index)).map(|route| route.provider.clone());
        if config.selected_openai_provider != actual_openai || config.selected_anthropic_provider != actual_anthropic {
            config.selected_openai_provider = actual_openai.clone();
            config.selected_anthropic_provider = actual_anthropic.clone();
            let path = config.state_dir.join("config.json");
            if let Err(save_error) = config::save(&path, &config) { error = Some(format!("修复上游选择失败: {save_error}")); }
        }
        Arc::new(Self {
            inner: Mutex::new(RuntimeState { config, routes, active, active_anthropic, selected_provider: actual_openai, selected_anthropic_provider: actual_anthropic, headroom_state: "检测中".into(), headroom_pid: None, last_switch_reason: None, last_error: error }),
            stop: AtomicBool::new(false), restart_headroom: AtomicBool::new(false), force_probe: AtomicBool::new(false),
            sync_in_progress: AtomicBool::new(false), sync_status: Mutex::new("未同步".into()), sync_result: Mutex::new(None),
            restart_in_progress: AtomicBool::new(false), restart_status: Mutex::new("未重启".into()), restart_result: Mutex::new(None),
            maintenance_action: Mutex::new(None),
        })
    }

    pub fn begin_sync(&self) -> bool {
        if self.sync_in_progress.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() { return false; }
        *self.sync_status.lock().unwrap() = "同步中".into();
        true
    }

    pub fn finish_sync(&self, ok: bool, message: String) {
        *self.sync_status.lock().unwrap() = if ok { "同步完成".into() } else { "同步失败".into() };
        *self.sync_result.lock().unwrap() = Some((ok, message));
        self.sync_in_progress.store(false, Ordering::Release);
    }

    pub fn take_sync_result(&self) -> Option<(bool, String)> { self.sync_result.lock().unwrap().take() }

    pub fn begin_restart(&self) -> bool {
        if self.restart_in_progress.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() { return false; }
        *self.restart_status.lock().unwrap() = "重启中".into();
        true
    }

    pub fn finish_restart(&self, ok: bool, message: String) {
        *self.restart_status.lock().unwrap() = if ok { "重启完成".into() } else { "重启失败".into() };
        *self.restart_result.lock().unwrap() = Some((ok, message));
        self.restart_in_progress.store(false, Ordering::Release);
    }

    pub fn take_restart_result(&self) -> Option<(bool, String)> { self.restart_result.lock().unwrap().take() }

    pub fn snapshot(&self) -> Snapshot { self.snapshot_unlocked(&self.inner.lock().unwrap()) }
    pub fn active_route(&self) -> Option<Route> { self.active_route_for(Protocol::OpenAi) }
    pub fn active_route_for(&self, protocol: Protocol) -> Option<Route> {
        let state = self.inner.lock().unwrap();
        let index = if protocol == Protocol::OpenAi { state.active } else { state.active_anthropic };
        index.and_then(|value| state.routes.get(value).cloned())
    }
    pub fn active_route_for_path(&self, path: &str) -> Option<Route> {
        self.active_route_for(protocol_for_path(path))
    }
    pub fn active_url(&self) -> Option<String> { self.active_route().map(|route| route.base_url) }

    pub fn switch_index(&self, index: usize, reason: &str) -> bool {
        let mut state = self.inner.lock().unwrap();
        let Some(route) = state.routes.get(index) else { return false };
        let protocol = route.protocol; let provider = route.provider.clone();
        if protocol == Protocol::OpenAi {
            state.active = Some(index); state.selected_provider = Some(provider.clone()); state.config.selected_openai_provider = Some(provider);
        } else {
            state.active_anthropic = Some(index); state.selected_anthropic_provider = Some(provider.clone()); state.config.selected_anthropic_provider = Some(provider);
        }
        state.last_switch_reason = Some(format!("{}：{}", protocol.label(), reason));
        let path = state.config.state_dir.join("config.json");
        let saved = state.config.clone();
        drop(state);
        if let Err(error) = config::save(&path, &saved) { self.inner.lock().unwrap().last_error = Some(format!("保存上游选择失败: {error}")); }
        true
    }

    pub fn switch_next(&self) -> bool {
        let state = self.inner.lock().unwrap();
        let indices: Vec<usize> = state.routes.iter().enumerate().filter_map(|(i, r)| (r.protocol == Protocol::OpenAi).then_some(i)).collect();
        if indices.is_empty() { return false; }
        let current = state.active.and_then(|active| indices.iter().position(|value| *value == active)).unwrap_or(0);
        let next = indices[(current + 1) % indices.len()]; drop(state); self.switch_index(next, "托盘手动切换")
    }

    pub fn refresh_routes(&self) {
        let config = self.inner.lock().unwrap().config.clone();
        match config::discover_routes(&config) {
            Ok(found) => {
                let mut state = self.inner.lock().unwrap();
                let old_openai = state.active.and_then(|i| state.routes.get(i)).map(|r| (r.provider.clone(), r.base_url.clone()));
                let old_anthropic = state.active_anthropic.and_then(|i| state.routes.get(i)).map(|r| (r.provider.clone(), r.base_url.clone()));
                let mut routes = found.routes;
                for route in &mut routes {
                    if let Some(old) = state.routes.iter().find(|old| old.protocol == route.protocol && old.base_url == route.base_url) {
                        route.state = old.state; route.score = old.score; route.latency_ms = old.latency_ms; route.consecutive_successes = old.consecutive_successes; route.consecutive_failures = old.consecutive_failures; route.verified_by_request = old.verified_by_request; route.last_error = old.last_error.clone(); route.last_status_code = old.last_status_code; route.last_success_at = old.last_success_at;
                    }
                }
                let active = preserve_index(&routes, Protocol::OpenAi, old_openai, found.selected_openai.as_deref());
                let active_anthropic = preserve_index(&routes, Protocol::Anthropic, old_anthropic, found.selected_anthropic.as_deref());
                let actual_openai = active.and_then(|index| routes.get(index)).map(|route| route.provider.clone());
                let actual_anthropic = active_anthropic.and_then(|index| routes.get(index)).map(|route| route.provider.clone());
                let selection_changed = state.config.selected_openai_provider != actual_openai || state.config.selected_anthropic_provider != actual_anthropic;
                state.active = active; state.active_anthropic = active_anthropic;
                state.selected_provider = actual_openai.clone(); state.selected_anthropic_provider = actual_anthropic.clone();
                state.config.selected_openai_provider = actual_openai; state.config.selected_anthropic_provider = actual_anthropic;
                state.routes = routes; state.last_error = None;
                let path = state.config.state_dir.join("config.json"); let saved = state.config.clone();
                drop(state);
                if selection_changed { if let Err(error) = config::save(&path, &saved) { self.inner.lock().unwrap().last_error = Some(format!("修复上游选择失败: {error}")); } }
            }
            Err(error) => self.inner.lock().unwrap().last_error = Some(error.to_string()),
        }
    }

    pub fn record_route_result(&self, protocol: Protocol, base_url: &str, ok: bool, latency: u64, status: Option<u16>, error: Option<String>, request: bool) {
        let mut state = self.inner.lock().unwrap();
        if let Some(route) = state.routes.iter_mut().find(|r| r.protocol == protocol && r.base_url == base_url) { route.record(ok, latency, status, error, request); }
    }

    pub fn write_status(&self) -> Result<()> {
        let snapshot = self.snapshot();
        let (state_dir, legacy, port) = { let state = self.inner.lock().unwrap(); (state.config.state_dir.clone(), state.config.legacy_state_dir.clone(), state.config.agent_port) };
        for dir in [&state_dir, &legacy] {
            fs::create_dir_all(dir)?;
            atomic_write(&dir.join("status.json"), &serde_json::to_vec_pretty(&snapshot)?)?;
            let ini = format!("[status]\r\nstate={}\r\nactive_provider={}\r\nactive_host={}\r\nclaude_provider={}\r\nclaude_host={}\r\nlatency_ms={}\r\nscore={}\r\nauto_enabled=false\r\nheadroom_state={}\r\ninflight=0\r\nroute_count={}\r\nlast_error={}\r\n",
                snapshot.state, snapshot.active_name.as_deref().unwrap_or("--"), snapshot.active_host.as_deref().unwrap_or("--"), snapshot.active_anthropic_name.as_deref().unwrap_or("--"), snapshot.active_anthropic_host.as_deref().unwrap_or("--"), snapshot.latency_ms.map(|v| v.to_string()).unwrap_or_default(), snapshot.active_score, snapshot.headroom_state, snapshot.routes.len().min(32), snapshot.last_error.as_deref().unwrap_or(""));
            let mut utf16 = vec![0xff, 0xfe]; utf16.extend(ini.encode_utf16().flat_map(u16::to_le_bytes)); atomic_write(&dir.join("status.ini"), &utf16)?;
            atomic_write(&dir.join("runtime.json"), &serde_json::to_vec_pretty(&json!({"service":"headroom-route","port":port}))?)?;
        }
        Ok(())
    }

    pub fn diagnostic_text(&self) -> String {
        let state = self.inner.lock().unwrap(); let snap = self.snapshot_unlocked(&state);
        format!("Headroom Route {}\r\nCodex: {} [{}]\r\nClaude: {} [{}]\r\nCC-Switch: {} [{}]\r\nAgent: 127.0.0.1:{}\r\nHeadroom: 127.0.0.1:{} ({}, PID={})\r\nCodex 上游: {}\r\nClaude 上游: {}\r\n路由数: {}\r\n自动切换: 否\r\n最近错误: {}",
            env!("CARGO_PKG_VERSION"), state.config.codex_config.display(), yes(state.config.codex_config.exists()), state.config.claude_settings.display(), yes(state.config.claude_settings.exists()), state.config.cc_switch_db.display(), yes(state.config.cc_switch_db.exists()), state.config.agent_port, state.config.headroom_port, state.headroom_state, state.headroom_pid.map(|v| v.to_string()).unwrap_or_else(|| "--".into()), snap.active_name.as_deref().unwrap_or("--"), snap.active_anthropic_name.as_deref().unwrap_or("--"), state.routes.len(), state.last_error.as_deref().unwrap_or("无"))
    }

    fn snapshot_unlocked(&self, state: &RuntimeState) -> Snapshot {
        let openai = state.active.and_then(|i| state.routes.get(i)); let anthropic = state.active_anthropic.and_then(|i| state.routes.get(i));
        let health = combined_health(openai.map(|r| r.state), anthropic.map(|r| r.state));
        Snapshot { service: "headroom-route", version: env!("CARGO_PKG_VERSION"), state: match health { RouteHealth::Healthy => "healthy", RouteHealth::Degraded => "degraded", RouteHealth::Unknown => "unknown", RouteHealth::Unavailable => "unavailable" }, active_provider: openai.map(|r| r.provider.clone()), active_name: openai.map(|r| r.name.clone()), active_url: openai.map(|r| r.base_url.clone()), active_host: openai.map(Route::host), active_score: openai.map(|r| r.score).unwrap_or(0), latency_ms: openai.and_then(|r| r.latency_ms), active_anthropic_provider: anthropic.map(|r| r.provider.clone()), active_anthropic_name: anthropic.map(|r| r.name.clone()), active_anthropic_url: anthropic.map(|r| r.base_url.clone()), active_anthropic_host: anthropic.map(Route::host), active_anthropic_score: anthropic.map(|r| r.score).unwrap_or(0), anthropic_latency_ms: anthropic.and_then(|r| r.latency_ms), auto_enabled: false, headroom_state: state.headroom_state.clone(), headroom_pid: state.headroom_pid, sync_status: self.sync_status.lock().unwrap().clone(), restart_status: self.restart_status.lock().unwrap().clone(), routes: state.routes.clone(), last_switch_reason: state.last_switch_reason.clone(), last_error: state.last_error.clone() }
    }
}

fn select_index(routes: &[Route], protocol: Protocol, selected: Option<&str>) -> Option<usize> { selected.and_then(|id| routes.iter().position(|r| r.protocol == protocol && r.provider == id)).or_else(|| routes.iter().position(|r| r.protocol == protocol)) }
fn provider_exists(routes: &[Route], protocol: Protocol, provider: &str) -> bool { routes.iter().any(|route| route.protocol == protocol && route.provider == provider) }
fn valid_provider(routes: &[Route], protocol: Protocol, provider: Option<&str>) -> Option<String> { provider.filter(|id| provider_exists(routes, protocol, id)).map(str::to_owned) }
fn previous_provider(config: &AppConfig, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(config.state_dir.join("status.json")).ok()?).ok()?;
    value.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
}
fn preserve_index(routes: &[Route], protocol: Protocol, old: Option<(String, String)>, selected: Option<&str>) -> Option<usize> { old.and_then(|(provider, url)| routes.iter().position(|r| r.protocol == protocol && (r.provider == provider || r.base_url == url))).or_else(|| select_index(routes, protocol, selected)) }
fn combined_health(a: Option<RouteHealth>, b: Option<RouteHealth>) -> RouteHealth { let values = [a, b]; if values.iter().flatten().any(|v| *v == RouteHealth::Healthy) { RouteHealth::Healthy } else if values.iter().flatten().any(|v| *v == RouteHealth::Degraded) { RouteHealth::Degraded } else if values.iter().flatten().any(|v| *v == RouteHealth::Unknown) { RouteHealth::Unknown } else { RouteHealth::Unavailable } }
fn protocol_for_path(path: &str) -> Protocol { if path == "/api/oauth/usage" || path.starts_with("/v1/messages") { Protocol::Anthropic } else { Protocol::OpenAi } }
fn yes(value: bool) -> &'static str { if value { "是" } else { "否" } }
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> { let temp = path.with_extension("tmp"); fs::write(&temp, bytes)?; if path.exists() { fs::remove_file(path)?; } fs::rename(temp, path)?; Ok(()) }
pub fn should_stop(app: &AppState) -> bool { app.stop.load(Ordering::Relaxed) }

#[cfg(test)]
mod tests {
    use super::{AppState, RuntimeState, protocol_for_path};
    use crate::model::{AppConfig, AuthStyle, Protocol, Route};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    #[test]
    fn classifies_openai_and_anthropic_paths() {
        assert_eq!(protocol_for_path("/v1/responses"), Protocol::OpenAi);
        assert_eq!(protocol_for_path("/v1/messages"), Protocol::Anthropic);
        assert_eq!(protocol_for_path("/v1/messages/count_tokens"), Protocol::Anthropic);
        assert_eq!(protocol_for_path("/api/oauth/usage"), Protocol::Anthropic);
    }

    #[test]
    fn tool_selection_is_persisted_independently_of_cc_switch() {
        let dir = std::env::temp_dir().join(format!("headroom-route-selection-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default(); config.state_dir = dir.clone();
        let routes = vec![
            Route::new(Protocol::OpenAi, "cc-current".into(), "CC current".into(), "https://one.example.com/v1".into(), None, AuthStyle::PassThrough, "cc-switch"),
            Route::new(Protocol::OpenAi, "tool-choice".into(), "Tool choice".into(), "https://two.example.com/v1".into(), None, AuthStyle::PassThrough, "cc-switch"),
        ];
        let app = Arc::new(AppState {
            inner: Mutex::new(RuntimeState { config, routes, active: Some(0), active_anthropic: None, selected_provider: Some("cc-current".into()), selected_anthropic_provider: None, headroom_state: "test".into(), headroom_pid: None, last_switch_reason: None, last_error: None }),
            stop: AtomicBool::new(false), restart_headroom: AtomicBool::new(false), force_probe: AtomicBool::new(false), sync_in_progress: AtomicBool::new(false), sync_status: Mutex::new("未同步".into()), sync_result: Mutex::new(None), restart_in_progress: AtomicBool::new(false), restart_status: Mutex::new("未重启".into()), restart_result: Mutex::new(None), maintenance_action: Mutex::new(None),
        });
        assert!(app.switch_index(1, "test"));
        let saved: AppConfig = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved.selected_openai_provider.as_deref(), Some("tool-choice"));
        assert!(app.begin_restart());
        assert!(!app.begin_restart());
        app.finish_restart(true, "ok".into());
        assert_eq!(app.take_restart_result(), Some((true, "ok".into())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_selection_recovers_from_previous_runtime_status() {
        let dir = std::env::temp_dir().join(format!("headroom-route-repair-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("config.toml");
        std::fs::write(&codex, "model_provider = \"remembered\"\n[model_providers.remembered]\nname = \"Remembered\"\nbase_url = \"https://remembered.example.com/v1\"\n[model_providers.other]\nname = \"Other\"\nbase_url = \"https://other.example.com/v1\"\n").unwrap();
        std::fs::write(dir.join("status.json"), r#"{"active_provider":"remembered"}"#).unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.clone(); config.codex_config = codex; config.cc_switch_db = dir.join("missing.db"); config.enable_claude = false; config.selected_openai_provider = Some("deleted-provider".into());
        let app = AppState::new(config);
        assert_eq!(app.active_route().unwrap().provider, "remembered");
        let saved: AppConfig = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved.selected_openai_provider.as_deref(), Some("remembered"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
