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
            let _config_guard = config_write_mutex().lock().unwrap();
            if let Err(save_error) = config::save(&path, &config) {
                error = Some(format!("修复上游选择失败: {save_error}"));
            }
        }
        let history_path = config.state_dir.join("operation-history.json");
        let history = match OperationHistory::load(&history_path) {
            Ok(outcome) => {
                if let Some(quarantined) = outcome.quarantined {
                    let message = format!(
                        "操作历史文件已隔离，已从空历史继续: {}",
                        quarantined.display()
                    );
                    error = Some(match error.take() {
                        Some(existing) => format!("{existing}；{message}"),
                        None => message,
                    });
                }
                outcome.history
            }
            Err(load_error) => {
                let message = format!("加载操作历史失败，已从空历史继续: {load_error}");
                error = Some(match error.take() {
                    Some(existing) => format!("{existing}；{message}"),
                    None => message,
                });
                OperationHistory::new()
            }
        };
        let cooldown = SwitchCooldown::from_history(&history, DEFAULT_COOLDOWN);
        let recovery = RecoveryEngine::new(RecoveryConfig::default());
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
            operation_history: Mutex::new(history),
            switch_cooldown: Mutex::new(cooldown),
            recovery: Mutex::new(recovery),
            recovery_in_progress: AtomicBool::new(false),
            recovery_notice: Mutex::new(None),
            operation_notice: Mutex::new(None),
            pending_undo: Mutex::new(None),
            routing_log_lock: Mutex::new(()),
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
        self.process_recovery_event(if ok {
            EnvironmentEvent::RecoverySucceeded
        } else {
            EnvironmentEvent::RecoveryFailed
        });
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

    pub fn take_operation_notice(&self) -> Option<String> {
        self.operation_notice.lock().unwrap().take()
    }

    pub fn take_recovery_notice(&self) -> Option<String> {
        self.recovery_notice.lock().unwrap().take()
    }

    pub fn recovery_state(&self) -> RecoveryState {
        self.recovery.lock().unwrap().state()
    }

    pub fn begin_session(&self) -> Result<bool> {
        let path = self.session_path();
        let store = SessionStore::new(path);
        let previous_unclean = store.begin_session_with_status()?;
        if previous_unclean {
            let _ = self.process_recovery_event(EnvironmentEvent::PreviousSessionUnclean);
        }
        Ok(previous_unclean)
    }

    pub fn heartbeat_session(&self) -> Result<()> {
        let store = SessionStore::new(self.session_path());
        store.heartbeat()
    }

    pub fn finish_session(&self) -> Result<()> {
        let store = SessionStore::new(self.session_path());
        store.finish_session()
    }

    pub fn process_recovery_event(&self, event: EnvironmentEvent) -> RecoveryAction {
        let action = {
            let mut recovery = self.recovery.lock().unwrap();
            let action = recovery.process(event.clone());
            let active = matches!(
                recovery.state(),
                RecoveryState::InProgress | RecoveryState::Failed
            );
            self.recovery_in_progress.store(active, Ordering::Release);
            action
        };
        if !matches!(action, RecoveryAction::NoOp) {
            *self.recovery_notice.lock().unwrap() = Some(format!(
                "环境事件 {:?}：恢复动作 {:?}；重试次数 {}",
                event,
                action,
                self.recovery.lock().unwrap().retry_count()
            ));
        }
        action
    }

    pub fn take_recovery_retry(&self) -> Option<EnvironmentEvent> {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.state() != RecoveryState::Failed || !recovery.can_retry() {
            return None;
        }
        let event = recovery.last_event().cloned()?;
        recovery.attempt_retry().ok()?;
        self.recovery_in_progress.store(true, Ordering::Release);
        Some(event)
    }

    pub fn record_routing_decision(&self, decision: &RoutingDecision) -> Result<()> {
        let record = decision.as_record();
        let mut record = serde_json::to_value(record)?;
        if let Some(model) = record
            .get("model")
            .and_then(Value::as_str)
            .map(|model| model.chars().take(160).collect::<String>())
        {
            let safe_model = model;
            record["model"] = Value::String(safe_model);
        }
        if let Some(rationale) = record
            .get("rationale")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            let safe = config::portability::redact_sensitive_text(&rationale);
            record["rationale"] = Value::String(safe.chars().take(500).collect());
        }
        let line = serde_json::to_string(&record)?;
        let _log_guard = self.routing_log_lock.lock().unwrap();
        let path = {
            let state = self.inner.lock().unwrap();
            state.config.state_dir.join("routing-decisions.jsonl")
        };
        let mut lines = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>();
        lines.push(line);
        let keep_from = lines.len().saturating_sub(200);
        let content = lines[keep_from..].join("\n") + "\n";
        atomic_write(&path, content.as_bytes())
    }

    pub fn operation_history(&self) -> Vec<OperationRecord> {
        self.operation_history.lock().unwrap().entries().to_vec()
    }

    pub fn pending_undo_ticket(&self) -> Option<UndoTicket> {
        self.pending_undo
            .lock()
            .unwrap()
            .as_ref()
            .map(|grant| grant.ticket.clone())
    }

    pub fn undo_switch(&self, ticket_id: &str, confirmation_token: &str) -> Result<()> {
        let (protocol, switched_to, restore_provider) = {
            let history = self.operation_history.lock().unwrap();
            let ticket = history
                .undo_ticket(ticket_id)
                .ok_or_else(|| anyhow!("撤销票据不存在或已使用"))?;
            (
                ticket.protocol,
                ticket.switched_to.clone(),
                ticket.restore_provider.clone(),
            )
        };
        let (current_provider, target_index) = {
            let state = self.inner.lock().unwrap();
            let current = active_route(&state, protocol)
                .map(|route| route.provider.clone())
                .ok_or_else(|| anyhow!("当前协议没有活动 Provider"))?;
            let target = state
                .routes
                .iter()
                .position(|route| route.protocol == protocol && route.provider == restore_provider)
                .ok_or_else(|| anyhow!("撤销目标 Provider 已不存在"))?;
            (current, target)
        };
        if current_provider != switched_to {
            return Err(anyhow!("当前 Provider 已变化，撤销票据不再适用"));
        }
        {
            let history = self.operation_history.lock().unwrap();
            history.verify_undo(ticket_id, confirmation_token, &current_provider)?;
        }
        if !self.switch_index_impl(target_index, "撤销上一次 Provider 切换", None) {
            return Err(anyhow!("撤销切换未能应用"));
        }
        let record = {
            let mut history = self.operation_history.lock().unwrap();
            let record = history.record_undo(ticket_id, confirmation_token, &current_provider)?;
            self.save_operation_history(&history)?;
            record
        };
        self.pending_undo
            .lock()
            .unwrap()
            .take_if(|grant| grant.ticket.id == ticket_id);
        *self.operation_notice.lock().unwrap() = Some(format!(
            "已撤销 Provider 切换：{} -> {}（记录 {}）",
            switched_to, restore_provider, record.id
        ));
        Ok(())
    }

    fn session_path(&self) -> std::path::PathBuf {
        self.inner
            .lock()
            .unwrap()
            .config
            .state_dir
            .join("session-marker.json")
    }

    fn save_operation_history(&self, history: &OperationHistory) -> Result<()> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .config
            .state_dir
            .join("operation-history.json");
        history.save(&path)
    }

    pub fn toggle_auto_failover(&self) -> Result<bool> {
        let _config_guard = self.config_write_guard();
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
        let _config_guard = self.config_write_guard();
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
        let _config_guard = self.config_write_guard();
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
        let _config_guard = self.config_write_guard();
        let (current, mut updated, path, preferred_openai, preferred_anthropic) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.clone(),
                state.config.clone(),
                state.config.state_dir.join("config.json"),
                active_route(&state, Protocol::OpenAi).map(|route| route.base_url.clone()),
                active_route(&state, Protocol::Anthropic).map(|route| route.base_url.clone()),
            )
        };
        updated.bypass_headroom = !updated.bypass_headroom;
        if let Err(error) = config::sync_all_with_targets(
            &updated,
            preferred_openai.as_deref(),
            preferred_anthropic.as_deref(),
        )
        .and_then(|_| config::save(&path, &updated))
        {
            let _ = config::sync_all_with_targets(
                &current,
                preferred_openai.as_deref(),
                preferred_anthropic.as_deref(),
            );
            self.inner.lock().unwrap().last_error =
                Some(format!("切换 Headroom 模式失败: {error}"));
            return Err(error);
        }
        let enabled = updated.bypass_headroom;
        self.inner.lock().unwrap().config = updated;
        Ok(enabled)
    }

    pub fn toggle_direct(&self, protocol: Protocol) -> Result<bool> {
        let _config_guard = self.config_write_guard();
        let (current, mut updated, path, preferred_openai, preferred_anthropic) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.clone(),
                state.config.clone(),
                state.config.state_dir.join("config.json"),
                active_route(&state, Protocol::OpenAi).map(|route| route.base_url.clone()),
                active_route(&state, Protocol::Anthropic).map(|route| route.base_url.clone()),
            )
        };
        let enabled = match protocol {
            Protocol::OpenAi => {
                updated.direct_codex = !updated.direct_codex;
                updated.direct_codex
            }
            Protocol::Anthropic => {
                updated.direct_claude = !updated.direct_claude;
                updated.direct_claude
            }
        };
        let preferred = if protocol == Protocol::OpenAi {
            preferred_openai.as_deref()
        } else {
            preferred_anthropic.as_deref()
        };
        if let Err(error) = config::sync_protocol_with_target(&updated, protocol, preferred)
            .and_then(|_| config::save(&path, &updated))
        {
            let current_preferred = if protocol == Protocol::OpenAi {
                preferred_openai.as_deref()
            } else {
                preferred_anthropic.as_deref()
            };
            let _ = config::sync_protocol_with_target(&current, protocol, current_preferred);
            self.inner.lock().unwrap().last_error =
                Some(format!("切换{}直连模式失败: {error}", protocol.label()));
            return Err(error);
        }
        self.inner.lock().unwrap().config = updated;
        Ok(enabled)
    }

    pub fn reset_headroom_metrics(&self) -> Result<()> {
        let _config_guard = self.config_write_guard();
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
        let _config_guard = self.config_write_guard();
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

    pub fn toggle_show_api_key_on_hover(&self) -> Result<bool> {
        let _config_guard = self.config_write_guard();
        let (enabled, path, saved) = {
            let mut state = self.inner.lock().unwrap();
            state.config.show_api_key_on_hover = !state.config.show_api_key_on_hover;
            (
                state.config.show_api_key_on_hover,
                state.config.state_dir.join("config.json"),
                state.config.clone(),
            )
        };
        if let Err(error) = config::save(&path, &saved) {
            self.inner.lock().unwrap().config.show_api_key_on_hover = !enabled;
            return Err(error);
        }
        Ok(enabled)
    }

    pub fn begin_daily_update_check(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<AppConfig>> {
        let _config_guard = self.config_write_guard();
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

    pub fn route_for_request(&self, path: &str, model: Option<&str>) -> Result<Option<Route>> {
        let protocol = protocol_for_path(path);
        let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(self.active_route_for(protocol));
        };
        let (strategy, candidates, current_provider, active, allowed_targets) = {
            let state = self.inner.lock().unwrap();
            let current_provider =
                active_route(&state, protocol).map(|route| route.provider.clone());
            let allowed_targets = current_provider
                .as_deref()
                .map(|provider| {
                    routing_policy::allowed_targets(
                        &state.config.failover_policy,
                        protocol,
                        provider,
                    )
                })
                .unwrap_or_default();
            let candidates = state
                .routes
                .iter()
                .filter(|route| route.protocol == protocol)
                .map(|route| routing_policy::CandidateFacts::from_route(model, route))
                .collect::<Vec<_>>();
            (
                state.config.routing_strategy.clone(),
                candidates,
                current_provider,
                active_route(&state, protocol).cloned(),
                allowed_targets,
            )
        };
        let decision = routing_policy::decide_with_context(
            &routing_policy::DecisionContext {
                model: model.chars().take(160).collect(),
                protocol,
                allowed_targets,
                provider: current_provider.clone(),
                provider_cost: None,
            },
            &candidates,
            &strategy,
        )?;
        if let Err(error) = self.record_routing_decision(&decision) {
            self.inner.lock().unwrap().last_error = Some(format!("保存路由策略决策失败: {error}"));
        }
        if decision.decision == DecisionMode::Apply
            && let Some(provider) = decision.selected_provider.as_deref()
            && current_provider.as_deref() != Some(provider)
        {
            let target = {
                let state = self.inner.lock().unwrap();
                state
                    .routes
                    .iter()
                    .position(|route| route.protocol == protocol && route.provider == provider)
            };
            if let Some(target) = target {
                let rationale = format!("路由策略 apply：{}", decision.rationale);
                if self.switch_index_impl(target, &rationale, Some(OperationKind::ManualSwitch)) {
                    return Ok(self.active_route_for(protocol));
                }
            }
        }
        Ok(active)
    }

    pub fn active_url(&self) -> Option<String> {
        self.active_route().map(|route| route.base_url)
    }
    pub fn active_anthropic_url(&self) -> Option<String> {
        self.active_route_for(Protocol::Anthropic)
            .map(|route| route.base_url)
    }
    pub fn route_summary(&self, protocol: Protocol) -> String {
        let state = self.inner.lock().unwrap();
        route_summary(active_route(&state, protocol))
    }
    pub fn recovery_hint(&self) -> &'static str {
        let state = self.inner.lock().unwrap();
        let headroom_state = if state.config.bypass_headroom
            || (state.config.direct_codex && state.config.direct_claude)
        {
            "external"
        } else {
            &state.headroom_state
        };
        recovery_hint(
            active_route(&state, Protocol::OpenAi),
            active_route(&state, Protocol::Anthropic),
            headroom_state,
            state.last_error.as_deref(),
        )
    }

    pub fn switch_index(&self, index: usize, reason: &str) -> bool {
        self.switch_index_impl(index, reason, Some(OperationKind::ManualSwitch))
    }

    fn switch_index_impl(
        &self,
        index: usize,
        reason: &str,
        history_kind: Option<OperationKind>,
    ) -> bool {
        let _config_guard = self.config_write_guard();
        let (protocol, provider, from_provider, direct, app_config) = {
            let state = self.inner.lock().unwrap();
            let Some(route) = state.routes.get(index) else {
                return false;
            };
            let from_provider =
                active_route(&state, route.protocol).map(|active| active.provider.clone());
            (
                route.protocol,
                route.provider.clone(),
                from_provider,
                match route.protocol {
                    Protocol::OpenAi => state.config.direct_codex,
                    Protocol::Anthropic => state.config.direct_claude,
                },
                state.config.clone(),
            )
        };
        let model_notice = if direct {
            if let Err(error) = config::sync_direct_provider(&app_config, protocol, &provider) {
                self.inner.lock().unwrap().last_error = Some(format!(
                    "同步{}直连 Provider 失败: {error}",
                    protocol.label()
                ));
                return false;
            }
            None
        } else {
            match config::sync_provider_models(&app_config, protocol, &provider) {
                Ok(notice) => notice,
                Err(error) => {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("同步目标模型配置失败: {error}"));
                    return false;
                }
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
            state.config.selected_openai_provider = Some(provider.clone());
        } else {
            state.active_anthropic = Some(index);
            state.selected_anthropic_provider = Some(provider.clone());
            state.config.selected_anthropic_provider = Some(provider.clone());
        }
        state.last_switch_reason = Some(format!("{}：{}", protocol.label(), reason));
        let selected_openai = state.config.selected_openai_provider.clone();
        let selected_anthropic = state.config.selected_anthropic_provider.clone();
        state.routes.retain(|route| {
            !route.name.ends_with("（已从 CC-Switch 删除，仍在使用）")
                || match route.protocol {
                    Protocol::OpenAi => selected_openai.as_deref() == Some(route.provider.as_str()),
                    Protocol::Anthropic => {
                        selected_anthropic.as_deref() == Some(route.provider.as_str())
                    }
                }
        });
        state.active = selected_openai.as_deref().and_then(|provider| {
            state
                .routes
                .iter()
                .position(|route| route.protocol == Protocol::OpenAi && route.provider == provider)
        });
        state.active_anthropic = selected_anthropic.as_deref().and_then(|provider| {
            state.routes.iter().position(|route| {
                route.protocol == Protocol::Anthropic && route.provider == provider
            })
        });
        if history_kind == Some(OperationKind::AutoFailover)
            && let Some(failed_provider) = from_provider.as_deref()
            && let Some(route) = state
                .routes
                .iter_mut()
                .find(|route| route.protocol == protocol && route.provider == failed_provider)
        {
            route.failover_blocked_until = Some(chrono::Utc::now() + chrono::Duration::minutes(5));
        }
        let path = state.config.state_dir.join("config.json");
        let saved = state.config.clone();
        drop(state);
        if let Err(error) = config::save(&path, &saved) {
            self.inner.lock().unwrap().last_error = Some(format!("保存上游选择失败: {error}"));
        }
        if let Some(notice) = model_notice {
            *self.model_change_notice.lock().unwrap() = Some(notice);
        }
        if let Some(kind) = history_kind {
            let operation = {
                let mut history = self.operation_history.lock().unwrap();
                let result = match kind {
                    OperationKind::ManualSwitch => history.record_manual_switch(
                        protocol,
                        from_provider.as_deref(),
                        &provider,
                        reason,
                    ),
                    OperationKind::AutoFailover => from_provider.as_deref().map_or_else(
                        || Err(anyhow!("自动切换缺少原 Provider")),
                        |from| {
                            history
                                .record_auto_failover(protocol, from, &provider, reason)
                                .map(|(record, grant)| (record, Some(grant)))
                        },
                    ),
                    OperationKind::UndoSwitch | OperationKind::CooldownBlocked => {
                        Err(anyhow!("该操作类型不能由普通切换入口记录"))
                    }
                };
                match result {
                    Ok((record, grant)) => {
                        let save_result = self.save_operation_history(&history);
                        (Ok(record), grant, save_result)
                    }
                    Err(error) => (Err(error), None, Ok(())),
                }
            };
            match operation {
                (Ok(record), grant, save_result) => {
                    if let Err(error) = save_result {
                        self.inner.lock().unwrap().last_error =
                            Some(format!("保存操作历史失败: {error}"));
                    }
                    if let Some(grant) = grant {
                        let token = grant.confirmation_token.clone();
                        let ticket_id = grant.ticket.id.clone();
                        *self.pending_undo.lock().unwrap() = Some(grant);
                        *self.operation_notice.lock().unwrap() = Some(format!(
                            "已记录{}切换 {}；撤销票据 {}，确认码 {}",
                            if kind == OperationKind::AutoFailover {
                                "自动"
                            } else {
                                "手动"
                            },
                            record.id,
                            ticket_id,
                            token
                        ));
                    } else {
                        *self.operation_notice.lock().unwrap() =
                            Some(format!("已记录 Provider 切换操作 {}", record.id));
                    }
                    if kind == OperationKind::AutoFailover
                        && let Some(from) = from_provider.as_deref()
                    {
                        self.switch_cooldown
                            .lock()
                            .unwrap()
                            .apply_auto_failover(protocol, from);
                    }
                }
                (Err(error), _, _) => {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("记录 Provider 切换失败: {error}"));
                }
            }
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
        let _config_guard = self.config_write_guard();
        let config = self.inner.lock().unwrap().config.clone();
        match config::discover_routes(&config) {
            Ok(found) => {
                let mut state = self.inner.lock().unwrap();
                let old_openai_route = state.active.and_then(|i| state.routes.get(i)).cloned();
                let old_anthropic_route = state
                    .active_anthropic
                    .and_then(|i| state.routes.get(i))
                    .cloned();
                let old_openai = old_openai_route.as_ref().map(|r| r.provider.clone());
                let old_anthropic = old_anthropic_route.as_ref().map(|r| r.provider.clone());
                let mut routes = found.routes;
                let mut retained_deleted = Vec::new();
                for old in [old_openai_route, old_anthropic_route]
                    .into_iter()
                    .flatten()
                {
                    if !routes.iter().any(|route| {
                        route.protocol == old.protocol && route.provider == old.provider
                    }) {
                        let mut retained = old;
                        retained.name =
                            format!("{}（已从 CC-Switch 删除，仍在使用）", retained.name);
                        retained_deleted.push(retained.provider.clone());
                        routes.push(retained);
                    }
                }
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
                let preferred_openai = active
                    .and_then(|index| routes.get(index))
                    .map(|route| route.base_url.clone());
                let preferred_anthropic = active_anthropic
                    .and_then(|index| routes.get(index))
                    .map(|route| route.base_url.clone());
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
                let direct_sync = saved.direct_codex || saved.direct_claude;
                drop(state);
                if !retained_deleted.is_empty() {
                    *self.config_change_notice.lock().unwrap() = Some(format!(
                        "活动 Provider {} 已从 CC-Switch 删除；切换前继续使用最后有效配置",
                        retained_deleted.join("、")
                    ));
                }
                if selection_changed && let Err(error) = config::save(&path, &saved) {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("修复上游选择失败: {error}"));
                }
                if direct_sync
                    && let Err(error) = config::sync_all_with_targets(
                        &saved,
                        preferred_openai.as_deref(),
                        preferred_anthropic.as_deref(),
                    )
                {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("同步直连上游失败: {error}"));
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
        let (failover, blocked) = {
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
            let direct = match protocol {
                Protocol::OpenAi => state.config.direct_codex,
                Protocol::Anthropic => state.config.direct_claude,
            };
            if direct
                || !state.config.auto_failover
                || active != Some(failed)
                || state.routes[failed].state != RouteHealth::Unavailable
            {
                (None, None)
            } else {
                let mut cooldown = self.switch_cooldown.lock().unwrap();
                cooldown.prune();
                let failed_provider = state.routes[failed].provider.clone();
                if !cooldown.protocol_allowed(protocol) {
                    (
                        None,
                        Some((
                            failed_provider,
                            None::<String>,
                            format!("{} 协议仍在自动切换冷却期", protocol.label()),
                        )),
                    )
                } else {
                    let candidate = failover_candidate_if(
                        &state.routes,
                        protocol,
                        failed,
                        &state.config.failover_policy,
                        |target| cooldown.provider_allowed(protocol, target),
                    );
                    match candidate {
                        Some(next) => (
                            Some((
                                next,
                                failed,
                                state.routes[failed].name.clone(),
                                state.routes[next].name.clone(),
                            )),
                            None,
                        ),
                        None => (None, None),
                    }
                }
            }
        };
        if let Some((next, failed_index, failed, replacement)) = failover
            && self.switch_index_impl(
                next,
                "自动故障切换（连续 3 次失败）",
                Some(OperationKind::AutoFailover),
            )
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
        if let Some((from, to, reason)) = blocked {
            let record = {
                let mut history = self.operation_history.lock().unwrap();
                let record = history
                    .record_blocked_failover(protocol, &from, to.as_deref(), &reason)
                    .map_err(|error| {
                        self.inner.lock().unwrap().last_error =
                            Some(format!("记录冷却阻止操作失败: {error}"));
                        error
                    });
                match record {
                    Ok(record) => match self.save_operation_history(&history) {
                        Ok(()) => Some(record),
                        Err(error) => {
                            self.inner.lock().unwrap().last_error =
                                Some(format!("保存操作历史失败: {error}"));
                            Some(record)
                        }
                    },
                    Err(_) => None,
                }
            };
            if let Some(record) = record {
                *self.operation_notice.lock().unwrap() =
                    Some(format!("自动切换已被冷却期阻止：{}", record.reason));
            }
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
                "[status]\r\nstate={}\r\nactive_provider={}\r\nactive_host={}\r\nclaude_provider={}\r\nclaude_host={}\r\nlatency_ms={}\r\nscore={}\r\nauto_enabled={}\r\nbypass_headroom={}\r\ndirect_codex={}\r\ndirect_claude={}\r\nheadroom_state={}\r\ninflight=0\r\nroute_count={}\r\nlast_error={}\r\n",
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
                snapshot.bypass_headroom,
                snapshot.direct_codex,
                snapshot.direct_claude,
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
        let (config, headroom_state, runtime_status) = {
            let state = self.inner.lock().unwrap();
            (
                state.config.clone(),
                state.headroom_state.clone(),
                self.runtime_status_unlocked(&state),
            )
        };
        let precheck = crate::precheck::collect(&config);
        // --doctor 在 worker 启动前执行，headroom_state 仍是初始的“检测中”；
        // 运行结论改用与预检一致的探测结果，避免健康环境被误判为“恢复中”。
        let runtime_status = if headroom_state == "检测中" {
            precheck.runtime_status.clone()
        } else {
            runtime_status
        };
        let existing = {
            let state = self.inner.lock().unwrap();
            let openai = active_route(&state, Protocol::OpenAi);
            let anthropic = active_route(&state, Protocol::Anthropic);
            format!(
                "Headroom Route {}\r\n运行结论: {}\r\n结论原因: {}\r\nCodex 状态: {}\r\nClaude 状态: {}\r\nHeadroom 状态: {}\r\nCodex: {} [{}]\r\nClaude: {} [{}]\r\nCC-Switch: {} [{}]\r\nAgent: 127.0.0.1:{}\r\nHeadroom: 127.0.0.1:{} ({}, PID={})\r\n统计范围: {}\r\n压缩 Token: {} -> {}，节省 {} ({:.1}%)\r\n完成请求: {}，失败 {} ({:.1}%)\r\nCodex 上游: {}\r\nClaude 上游: {}\r\n路由数: {}\r\n自动切换: {}\r\n最近错误: {}\r\n恢复建议: {}",
                env!("CARGO_PKG_VERSION"),
                runtime_status.mode.label(),
                runtime_status.reason,
                runtime_status.codex.summary(),
                runtime_status.claude.summary(),
                runtime_status.headroom.summary(),
                state.config.codex_config.display(),
                availability(openai, transport_ready(&state, Protocol::OpenAi)),
                state.config.claude_settings.display(),
                availability(anthropic, transport_ready(&state, Protocol::Anthropic)),
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
        };
        format!("{existing}\r\n\r\n{}", precheck.to_text())
    }

    fn snapshot_unlocked(&self, state: &RuntimeState) -> Snapshot {
        let openai = state.active.and_then(|i| state.routes.get(i));
        let anthropic = state.active_anthropic.and_then(|i| state.routes.get(i));
        let runtime_status = self.runtime_status_unlocked(state);
        Snapshot {
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

fn transport_ready(state: &RuntimeState, protocol: Protocol) -> bool {
    let direct = match protocol {
        Protocol::OpenAi => state.config.direct_codex,
        Protocol::Anthropic => state.config.direct_claude,
    };
    direct
        || state.config.bypass_headroom
        || matches!(state.headroom_state.as_str(), "healthy" | "external")
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
