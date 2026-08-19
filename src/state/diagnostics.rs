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
                snapshot.manage_codex || snapshot.manage_claude,
                !snapshot.manage_codex,
                !snapshot.manage_claude,
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
        let body = format!(
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
        );
        format!("{body}\r\n\r\n{}", current_install_verification())
    }

    pub fn install_verification_text() -> String {
        current_install_verification()
    }

    pub fn current_exe_sha256() -> Option<String> {
        hash_current_exe()
    }
}

fn sha256_path(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_current_exe() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|path| sha256_path(&path).ok())
}

fn current_install_verification() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let edition = crate::edition::EDITION;
    let path = std::env::current_exe()
        .map(|value| value.display().to_string())
        .unwrap_or_else(|_| "--".into());
    let hash = hash_current_exe().unwrap_or_else(|| "无法计算".into());
    let expected_name = match edition {
        "desktop" => format!("HeadroomRoute-{version}-desktop.exe"),
        _ => format!("HeadroomRoute-{version}.exe"),
    };
    format!(
        "安装验证\r\n版本: {version}\r\n版本形态: {edition}\r\n路径: {path}\r\nSHA-256: {hash}\r\n\r\n当前正式版默认未做 Authenticode 签名。请把上述哈希对照 GitHub Release 同版本 HeadroomRoute-{version}-SHA256SUMS.txt 中的 {expected_name}。\r\n清单: https://github.com/nizzo-dev/HeadroomRoute/releases/download/v{version}/HeadroomRoute-{version}-SHA256SUMS.txt"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn sha256_path_matches_known_abc_digest() {
        let path =
            std::env::temp_dir().join(format!("headroom-route-sha256-test-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        let hash = sha256_path(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(hash, format!("{:x}", sha2::Sha256::digest(b"abc")));
    }

    #[test]
    fn install_verification_text_names_sums_file_and_edition() {
        let text = current_install_verification();
        let version = env!("CARGO_PKG_VERSION");
        assert!(text.contains("SHA-256"));
        assert!(text.contains(&format!("HeadroomRoute-{version}-SHA256SUMS.txt")));
        assert!(text.contains(crate::edition::EDITION));
        assert!(text.contains("未做 Authenticode 签名"));
    }
}
