use super::*;

impl AppState {
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
                "[status]\r\nstate={}\r\nactive_provider={}\r\nactive_host={}\r\nclaude_provider={}\r\nclaude_host={}\r\nlatency_ms={}\r\nscore={}\r\nauto_enabled={}\r\nbypass_headroom={}\r\nmanage_upstream={}\r\ndirect_codex={}\r\ndirect_claude={}\r\nheadroom_state={}\r\ninflight=0\r\nroute_count={}\r\nlast_error={}\r\n",
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
                snapshot.manage_upstream,
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
        let cli = crate::cli_identity::CliCompatibility::inspect_cached(&config.state_dir);
        format!(
            "{existing}\r\n\r\n{}\r\nCLI wrapper: {}\r\nCLI 路径: {}\r\nCLI 版本: {}（期望 {}）\r\n通知协议: {}（期望 {}）",
            precheck.to_text(),
            if cli.compatible {
                "兼容"
            } else {
                "不兼容"
            },
            cli.path.as_deref().unwrap_or("--"),
            cli.detected_version.as_deref().unwrap_or("--"),
            cli.expected_version,
            cli.detected_protocol
                .map(|value| value.to_string())
                .unwrap_or_else(|| "--".into()),
            crate::cli_identity::CLI_PROTOCOL_VERSION,
        )
    }
}
