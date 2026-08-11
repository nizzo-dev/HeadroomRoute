use super::*;

pub(super) unsafe fn show_menu(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let snapshot = app.snapshot();
    let menu = unsafe { CreatePopupMenu() };
    let codex_menu = unsafe {
        route_menu(
            &snapshot,
            Protocol::OpenAi,
            snapshot.active_provider.as_deref(),
        )
    };
    let claude_menu = unsafe {
        route_menu(
            &snapshot,
            Protocol::Anthropic,
            snapshot.active_anthropic_provider.as_deref(),
        )
    };
    let service = format!("状态中心：{}", snapshot.runtime_status.summary());
    let codex = format!(
        "Codex：{} · {} · {} ms",
        snapshot.runtime_status.codex.summary(),
        snapshot.active_name.as_deref().unwrap_or("未配置"),
        latency_text(snapshot.latency_ms)
    );
    let claude = format!(
        "Claude：{} · {} · {} ms",
        snapshot.runtime_status.claude.summary(),
        snapshot
            .active_anthropic_name
            .as_deref()
            .unwrap_or("未配置"),
        latency_text(snapshot.anthropic_latency_ms)
    );
    let headroom = format!("Headroom：{}", snapshot.runtime_status.headroom.summary());
    let compression = format!(
        "Token：原始 {} → 优化 {} · 节省 {}（{:.1}%）",
        compact_number(snapshot.headroom_metrics.input_tokens_original),
        compact_number(snapshot.headroom_metrics.input_tokens_optimized),
        compact_number(snapshot.headroom_metrics.tokens_saved),
        snapshot.headroom_metrics.compression_percent()
    );
    let requests = format!(
        "请求：完成 {} · 失败 {}（{:.1}%）",
        compact_number(snapshot.headroom_metrics.completed_requests),
        compact_number(snapshot.headroom_metrics.failed_requests),
        snapshot.headroom_metrics.failure_percent()
    );
    let metrics_scope = snapshot.headroom_metrics_since.map_or_else(
        || "统计：当前日志文件累计".into(),
        |since| format!("统计：自 {} UTC", since.format("%Y-%m-%d %H:%M:%S")),
    );
    unsafe {
        // Disabled native menu items are always drawn gray. ID 0 keeps these
        // status rows inert while allowing Windows to render normal text.
        AppendMenuW(menu, MF_STRING, 0, wide(&service).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&codex).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&claude).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&headroom).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&metrics_scope).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&compression).as_ptr());
        AppendMenuW(menu, MF_STRING, 0, wide(&requests).as_ptr());
        AppendMenuW(
            menu,
            MF_STRING,
            ID_OPEN_STATUS,
            wide("查看完整状态...").as_ptr(),
        );
        if let Some((command, label)) = recommended_action(
            &snapshot.runtime_status,
            &snapshot.headroom_state,
            snapshot.last_error.as_deref(),
        ) {
            AppendMenuW(menu, MF_STRING, command, wide(label).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        let codex_label = format!(
            "切换 Codex 上游（{}）",
            snapshot.active_name.as_deref().unwrap_or("未配置")
        );
        let claude_label = format!(
            "切换 Claude 上游（{}）",
            snapshot
                .active_anthropic_name
                .as_deref()
                .unwrap_or("未配置")
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            codex_menu as usize,
            wide(&codex_label).as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            claude_menu as usize,
            wide(&claude_label).as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING | if snapshot.direct_codex { MF_CHECKED } else { 0 },
            ID_DIRECT_CODEX,
            wide("Codex 直连当前上游").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING
                | if snapshot.direct_claude {
                    MF_CHECKED
                } else {
                    0
                },
            ID_DIRECT_CLAUDE,
            wide("Claude 直连当前上游").as_ptr(),
        );
    }
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_CHECK, wide("立即检查上游").as_ptr());
        let approval_text = if approval::pending_count() == 0 {
            "测试确认悬浮窗"
        } else {
            "测试确认悬浮窗（有请求等待中）"
        };
        AppendMenuW(
            menu,
            MF_STRING,
            ID_APPROVAL_DEMO,
            wide(approval_text).as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING | if snapshot.auto_enabled { MF_CHECKED } else { 0 },
            ID_AUTO,
            wide("自动故障切换").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            ID_FAILOVER_EDITOR,
            wide("配置故障转移策略...").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING
                | if snapshot.bypass_headroom {
                    MF_CHECKED
                } else {
                    0
                },
            ID_BYPASS,
            wide("旁路 Headroom（保留路由）").as_ptr(),
        );
        let (sync_flags, sync_text) = if app.sync_in_progress.load(Ordering::Acquire) {
            (
                MF_STRING | MF_DISABLED | MF_GRAYED,
                "正在同步 Codex + Claude...",
            )
        } else if snapshot.sync_status == "同步完成" {
            (MF_STRING, "同步配置（上次已完成）")
        } else {
            (MF_STRING, "同步 Codex + Claude / CC-Switch")
        };
        AppendMenuW(menu, sync_flags, ID_SYNC, wide(sync_text).as_ptr());
        let (restart_flags, restart_text) = if app.restart_in_progress.load(Ordering::Acquire) {
            (MF_STRING | MF_DISABLED | MF_GRAYED, "正在重启 Headroom...")
        } else if snapshot.restart_status == "重启完成" {
            (MF_STRING, "重启 Headroom（上次已完成）")
        } else {
            (MF_STRING, "重启 Headroom")
        };
        AppendMenuW(menu, restart_flags, ID_RESTART, wide(restart_text).as_ptr());
        let settings_menu = CreatePopupMenu();
        let startup = app.inner.lock().unwrap().config.start_with_windows;
        AppendMenuW(
            settings_menu,
            MF_STRING | if startup { MF_CHECKED } else { 0 },
            ID_STARTUP,
            wide("随 Windows 启动").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING
                | if snapshot.auto_update_check {
                    MF_CHECKED
                } else {
                    0
                },
            ID_AUTO_UPDATE,
            wide("每日检查软件更新").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING
                | if snapshot.show_api_key_on_hover {
                    MF_CHECKED
                } else {
                    0
                },
            ID_SHOW_API_KEY,
            wide("悬浮显示上游 API Key").as_ptr(),
        );
        AppendMenuW(settings_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_CONFIG,
            wide("打开 config.json（高级配置）").as_ptr(),
        );
        let portability_menu = CreatePopupMenu();
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_TAKEOVER,
            wide("预览并应用 CLI 接管...").as_ptr(),
        );
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_CREATE_BACKUP,
            wide("创建配置备份").as_ptr(),
        );
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_RESTORE_BACKUP,
            wide("恢复配置备份...").as_ptr(),
        );
        AppendMenuW(portability_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_EXPORT_PORTABLE,
            wide("导出可移植配置...").as_ptr(),
        );
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_IMPORT_PORTABLE,
            wide("导入可移植配置...").as_ptr(),
        );
        AppendMenuW(
            portability_menu,
            MF_STRING,
            ID_DIAGNOSTIC_ZIP,
            wide("创建脱敏诊断 ZIP...").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_POPUP,
            portability_menu as usize,
            wide("配置迁移与备份").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_PROVIDER_IDS,
            wide("复制 Provider ID 清单").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_RELOAD_FAILOVER,
            wide("重新加载故障转移规则").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_LOGS,
            wide("打开数据与日志目录").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_DIAG,
            wide("复制脱敏诊断报告").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_PRECHECK,
            wide("运行启动预检...").as_ptr(),
        );
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_RESET_METRICS,
            wide("清零 Headroom 统计...").as_ptr(),
        );
        AppendMenuW(settings_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            settings_menu,
            MF_STRING
                | if updater::is_running() {
                    MF_DISABLED | MF_GRAYED
                } else {
                    0
                },
            ID_UPDATE,
            wide(if updater::is_running() {
                "正在检查软件更新..."
            } else {
                "检查软件更新..."
            })
            .as_ptr(),
        );
        let maintenance_menu = CreatePopupMenu();
        AppendMenuW(
            maintenance_menu,
            MF_STRING,
            ID_REPAIR_RUNTIME,
            wide("重新检测 Headroom 环境...").as_ptr(),
        );
        AppendMenuW(
            maintenance_menu,
            MF_STRING,
            ID_SELECT_RUNTIME,
            wide("选择 Headroom Python...").as_ptr(),
        );
        AppendMenuW(
            maintenance_menu,
            MF_STRING,
            ID_RESTORE,
            wide("恢复 Codex / Claude 原始配置...").as_ptr(),
        );
        AppendMenuW(maintenance_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            maintenance_menu,
            MF_STRING,
            ID_UNINSTALL,
            wide("完全卸载并还原...").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu as usize,
            wide("设置与诊断").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            maintenance_menu as usize,
            wide("维护与还原").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING,
            ID_EXIT,
            wide(if snapshot.direct_codex || snapshot.direct_claude {
                "退出并交还 CC-Switch"
            } else {
                "退出 HeadroomRoute"
            })
            .as_ptr(),
        );
        let mut point = POINT::default();
        GetCursorPos(&mut point);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);
    }
}

pub(super) fn recommended_action(
    status: &RuntimeStatus,
    headroom_state: &str,
    error: Option<&str>,
) -> Option<(usize, &'static str)> {
    if status.headroom.state == ComponentState::Unavailable
        && headroom_state == "runtime-unavailable"
    {
        return Some((ID_SELECT_RUNTIME, "建议操作：选择 Headroom Python..."));
    }
    if status.headroom.state == ComponentState::Checking {
        return None;
    }
    if status.headroom.state == ComponentState::Unavailable {
        return Some((ID_RESTART, "建议操作：重启 Headroom"));
    }
    let error = error.unwrap_or_default().to_ascii_lowercase();
    if ["同步", "配置", "routing", "route guard"]
        .iter()
        .any(|word| error.contains(word))
    {
        return Some((ID_SYNC, "建议操作：重新同步配置"));
    }
    if [status.codex.state, status.claude.state]
        .into_iter()
        .any(|state| {
            matches!(
                state,
                ComponentState::Degraded | ComponentState::Unavailable
            )
        })
    {
        return Some((ID_CHECK, "建议操作：立即检查上游"));
    }
    (!error.is_empty()).then_some((ID_DIAG, "建议操作：复制脱敏诊断报告"))
}
