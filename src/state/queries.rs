use super::*;

impl AppState {
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
        let headroom_state = if state.config.bypass_headroom || !state.config.manage_upstream {
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
}
