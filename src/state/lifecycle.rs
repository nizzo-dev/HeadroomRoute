use super::*;

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

    pub(super) fn save_operation_history(&self, history: &OperationHistory) -> Result<()> {
        let path = self
            .inner
            .lock()
            .unwrap()
            .config
            .state_dir
            .join("operation-history.json");
        history.save(&path)
    }
}
