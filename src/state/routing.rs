use super::*;

impl AppState {
    pub fn switch_index(&self, index: usize, reason: &str) -> bool {
        self.switch_index_impl(index, reason, Some(OperationKind::ManualSwitch))
    }

    pub(super) fn switch_index_impl(
        &self,
        index: usize,
        reason: &str,
        history_kind: Option<OperationKind>,
    ) -> bool {
        let _config_guard = self.config_write_guard();
        let (protocol, provider, from_provider, managing, app_config) = {
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
                    Protocol::OpenAi => state.config.manage_codex,
                    Protocol::Anthropic => state.config.manage_claude,
                },
                state.config.clone(),
            )
        };
        if !managing {
            self.inner.lock().unwrap().last_error =
                Some("当前为观测模式：请在 CC-Switch 切换上游，或打开该协议的「接管配置」".into());
            return false;
        }
        // Managing: point clients at local Headroom and sync model metadata.
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
                let manage_sync = saved.manage_upstream;
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
                // Only rewrite clients when managing; observe mode leaves CLI alone.
                if manage_sync
                    && let Err(error) = config::sync_all_with_targets(
                        &saved,
                        preferred_openai.as_deref(),
                        preferred_anthropic.as_deref(),
                    )
                {
                    self.inner.lock().unwrap().last_error =
                        Some(format!("同步接管上游失败: {error}"));
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
            // Auto-failover only while managing upstream (own the switch).
            let managing = match protocol {
                Protocol::OpenAi => state.config.manage_codex,
                Protocol::Anthropic => state.config.manage_claude,
            };
            if !managing
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
}
