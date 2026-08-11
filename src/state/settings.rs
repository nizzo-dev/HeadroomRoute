use super::*;

impl AppState {
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
}
