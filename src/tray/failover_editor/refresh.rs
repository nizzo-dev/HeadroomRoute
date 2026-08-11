use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
impl FailoverEditor {
    pub(super) unsafe fn refresh_sources(&mut self, hwnd: HWND) {
        self.sources = failover_sources(&self.routes, &self.policy, self.protocol);
        let combo = GetDlgItem(hwnd, ID_EDITOR_SOURCE as i32);
        SendMessageW(combo, CB_RESETCONTENT, 0, 0);
        for provider in &self.sources {
            let text = self.route(provider).map_or_else(
                || format!("{provider}（已失效）"),
                |route| format!("{}  ·  {}", route.name, route.evidence_label()),
            );
            SendMessageW(combo, CB_ADDSTRING, 0, wide(&text).as_ptr() as LPARAM);
        }
        if self
            .source_provider
            .as_ref()
            .is_none_or(|id| !self.sources.contains(id))
        {
            self.source_provider = self.sources.first().cloned();
        }
        if let Some(index) = self
            .source_provider
            .as_ref()
            .and_then(|id| self.sources.iter().position(|value| value == id))
        {
            SendMessageW(combo, CB_SETCURSEL, index, 0);
        }
        self.refresh_targets(hwnd);
    }

    pub(super) unsafe fn refresh_targets(&mut self, hwnd: HWND) {
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_CUSTOM as i32),
            self.source_provider.is_some() as i32,
        );
        let custom = self
            .source_provider
            .as_ref()
            .is_some_and(|source| self.policy.rules(self.protocol).contains_key(source));
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_CUSTOM as i32),
            BM_SETCHECK,
            if custom {
                BST_CHECKED as usize
            } else {
                BST_UNCHECKED as usize
            },
            0,
        );
        let targets = self
            .source_provider
            .as_ref()
            .and_then(|source| self.policy.targets(self.protocol, source))
            .unwrap_or_default()
            .to_vec();
        let source_detail = self.source_provider.as_ref().map_or_else(
            || "请选择一个源 Provider。".into(),
            |provider| {
                self.route(provider).map_or_else(
                    || format!("Provider ID：{provider}  ·  当前已失效，可关闭自定义规则后清理"),
                    |route| format!("Provider ID：{}  ·  上游：{}", route.provider, route.host()),
                )
            },
        );
        SetWindowTextW(
            GetDlgItem(hwnd, ID_EDITOR_SOURCE_DETAIL as i32),
            wide(&source_detail).as_ptr(),
        );
        self.available = self
            .routes
            .iter()
            .filter(|route| {
                route.protocol == self.protocol
                    && self.source_provider.as_deref() != Some(route.provider.as_str())
                    && !targets.contains(&route.provider)
            })
            .map(|route| route.provider.clone())
            .collect();
        let available = GetDlgItem(hwnd, ID_EDITOR_AVAILABLE as i32);
        let target_list = GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32);
        SendMessageW(available, LB_RESETCONTENT, 0, 0);
        for provider in &self.available {
            SendMessageW(
                available,
                LB_ADDSTRING,
                0,
                wide(&self.display(provider, false)).as_ptr() as LPARAM,
            );
        }
        SendMessageW(target_list, LB_RESETCONTENT, 0, 0);
        for (index, provider) in targets.iter().enumerate() {
            let text = format!("{}. {}", index + 1, self.display(provider, true));
            SendMessageW(target_list, LB_ADDSTRING, 0, wide(&text).as_ptr() as LPARAM);
        }
        for id in [ID_EDITOR_AVAILABLE, ID_EDITOR_TARGETS] {
            EnableWindow(GetDlgItem(hwnd, id as i32), custom as i32);
        }
        let status = if self.source_provider.is_none() {
            "当前协议没有可配置的 Provider。".into()
        } else if custom {
            format!(
                "已允许 {} 个目标，故障时将严格按列表顺序尝试。",
                targets.len()
            )
        } else {
            "未启用自定义顺序，将使用健康 Provider 中评分最高的线路。".into()
        };
        SetWindowTextW(
            GetDlgItem(hwnd, ID_EDITOR_STATUS as i32),
            wide(&status).as_ptr(),
        );
        self.update_action_buttons(hwnd);
    }
}
