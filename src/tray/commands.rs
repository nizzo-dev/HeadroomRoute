use super::*;

pub(super) unsafe fn handle_command(hwnd: HWND, id: usize) {
    let Some(app) = APP.get() else { return };
    // Maintenance/exit must destroy the tray host, never the main console.
    let host = tray_host_hwnd(hwnd);
    match id {
        ID_OPEN_STATUS => unsafe { show_main_window() },
        ID_CHECK => {
            app.force_probe.store(true, Ordering::Relaxed);
            notify(hwnd, "正在检查上游", "检查结果会自动更新到托盘状态");
        }
        ID_APPROVAL_DEMO => {
            if approval::enqueue_demo() {
                notify(
                    hwnd,
                    "确认悬浮窗已打开",
                    "这是演示请求，不会执行命令；可点击允许或取消",
                );
            } else {
                notify(hwnd, "确认请求队列已满", "请先处理现有的 CLI 请求");
            }
        }
        ID_AUTO => match app.toggle_auto_failover() {
            Ok(true) => notify(
                hwnd,
                "自动切换已启用",
                "当前路由连续 3 次失败后，将切换到同协议的健康路由",
            ),
            Ok(false) => notify(hwnd, "自动切换已关闭", "上游故障时将保留当前路由"),
            Err(error) => notify(hwnd, "自动切换设置失败", &error.to_string()),
        },
        ID_FAILOVER_EDITOR => unsafe { show_failover_editor(dialog_owner(hwnd)) },
        ID_MANAGE_UPSTREAM => match app.toggle_manage_codex() {
            Ok(true) => notify(hwnd, "已接管 Codex", "Codex 已指向本地 HeadroomRoute。"),
            Ok(false) => notify(
                hwnd,
                "已交还 Codex",
                "观测模式：请在 CC-Switch 切换 Codex Provider。",
            ),
            Err(error) => notify(hwnd, "切换 Codex 接管失败", &error.to_string()),
        },
        ID_MANAGE_CLAUDE => match app.toggle_manage_claude() {
            Ok(true) => notify(
                hwnd,
                "已接管 Claude Code",
                "Claude Code 已指向本地 HeadroomRoute。",
            ),
            Ok(false) => notify(
                hwnd,
                "已交还 Claude Code",
                "观测模式：请在 CC-Switch 切换 Claude Provider。",
            ),
            Err(error) => notify(hwnd, "切换 Claude 接管失败", &error.to_string()),
        },
        ID_BYPASS => match app.toggle_headroom_bypass() {
            Ok(true) => notify(
                hwnd,
                "已旁路 Headroom",
                "Codex 与 Claude 仍经过 HeadroomRoute，但不再压缩上下文",
            ),
            Ok(false) => notify(
                hwnd,
                "已恢复 Headroom",
                "Codex 与 Claude 已重新经过 Headroom 压缩层",
            ),
            Err(error) => notify(hwnd, "切换 Headroom 模式失败", &error.to_string()),
        },
        ID_SYNC => {
            if !app.begin_sync() {
                notify(hwnd, "正在同步", "请等待当前同步完成");
                return;
            }
            notify(
                hwnd,
                "同步中",
                "正在读取 CC-Switch 并更新 Codex / Claude Code",
            );
            let app = app.clone();
            thread::spawn(move || {
                let cfg = app.inner.lock().unwrap().config.clone();
                let active_url = app.active_url();
                let active_anthropic_url = app.active_anthropic_url();
                match config::sync_all_with_targets(
                    &cfg,
                    active_url.as_deref(),
                    active_anthropic_url.as_deref(),
                ) {
                    Ok(_) => {
                        app.refresh_routes();
                        let _ = app.write_status();
                        app.finish_sync(true, "Codex 与 Claude Code 配置同步完成".into());
                    }
                    Err(error) => app.finish_sync(false, error.to_string()),
                }
            });
        }
        ID_RESTART => {
            if !app.begin_restart() {
                notify(hwnd, "正在重启", "请等待当前 Headroom 重启完成");
                return;
            }
            app.restart_headroom.store(true, Ordering::Release);
            notify(
                hwnd,
                "Headroom 重启中",
                "正在停止并重新启动 Headroom，请稍候",
            );
        }
        ID_STARTUP => {
            let (enabled, state_dir) = {
                let _config_guard = app.config_write_guard();
                let mut state = app.inner.lock().unwrap();
                state.config.start_with_windows = !state.config.start_with_windows;
                let path = state.config.state_dir.join("config.json");
                let _ = config::save(&path, &state.config);
                (
                    state.config.start_with_windows,
                    state.config.state_dir.clone(),
                )
            };
            let exe = match std::env::current_exe() {
                Ok(current) => crate::startup::autostart_executable(&state_dir, &current),
                Err(_) => crate::startup::installed_executable(&state_dir),
            };
            if let Err(e) = crate::startup::set_enabled(enabled, &exe) {
                notify(hwnd, "开机启动设置失败", &e.to_string())
            }
        }
        ID_AUTO_UPDATE => match app.toggle_auto_update_check() {
            Ok(true) => notify(hwnd, "自动更新提醒已启用", "每天最多检查一次，只提醒不安装"),
            Ok(false) => notify(hwnd, "自动更新提醒已关闭", "仍可随时手动检查更新"),
            Err(error) => notify(hwnd, "自动更新提醒设置失败", &error.to_string()),
        },
        ID_SHOW_API_KEY => match app.toggle_show_api_key_on_hover() {
            Ok(true) => notify(hwnd, "已开启", "悬停上游列表时将显示 API Key"),
            Ok(false) => notify(hwnd, "已关闭", "不再显示 API Key"),
            Err(error) => notify(hwnd, "设置失败", &error.to_string()),
        },
        ID_DIAG => {
            let text = app.diagnostic_text();
            if copy_clipboard(hwnd, &text).is_ok() {
                notify(hwnd, "诊断报告已复制", "报告不包含 API Key")
            };
        }
        ID_TAKEOVER => show_takeover_preview(hwnd, app),
        ID_CREATE_BACKUP => create_backup_from_tray(hwnd, app),
        ID_RESTORE_BACKUP => restore_backup_from_tray(hwnd, app),
        ID_EXPORT_PORTABLE => export_portable_from_tray(hwnd, app),
        ID_IMPORT_PORTABLE => import_portable_from_tray(hwnd, app),
        ID_DIAGNOSTIC_ZIP => create_diagnostic_zip_from_tray(hwnd, app),
        ID_PRECHECK => unsafe { show_precheck(dialog_owner(hwnd)) },
        ID_RESET_METRICS => {
            if unsafe {
                MessageBoxW(
                    hwnd,
                    wide("只清零 HeadroomRoute 显示的累计统计，不删除原始日志。是否继续？")
                        .as_ptr(),
                    wide("清零 Headroom 统计").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                )
            } == IDYES
            {
                match app.reset_headroom_metrics() {
                    Ok(()) => notify(hwnd, "Headroom 统计已清零", "新的统计起点已保存"),
                    Err(error) => notify(hwnd, "清零 Headroom 统计失败", &error.to_string()),
                }
            }
        }
        ID_CONFIG => {
            let path = app
                .inner
                .lock()
                .unwrap()
                .config
                .state_dir
                .join("config.json");
            let _ = Command::new("notepad.exe").arg(path).spawn();
        }
        ID_PROVIDER_IDS => {
            let snapshot = app.snapshot();
            let mut text = String::from("Codex Provider：\r\n");
            for route in snapshot
                .routes
                .iter()
                .filter(|route| route.protocol == Protocol::OpenAi)
            {
                text.push_str(&format!("{} = {}\r\n", route.name, route.provider));
            }
            text.push_str("\r\nClaude Provider：\r\n");
            for route in snapshot
                .routes
                .iter()
                .filter(|route| route.protocol == Protocol::Anthropic)
            {
                text.push_str(&format!("{} = {}\r\n", route.name, route.provider));
            }
            match copy_clipboard(hwnd, &text) {
                Ok(()) => notify(
                    hwnd,
                    "Provider ID 已复制",
                    "可用于 config.json 的故障转移规则",
                ),
                Err(error) => notify(hwnd, "复制 Provider ID 失败", &error.to_string()),
            }
        }
        ID_RELOAD_FAILOVER => match app.reload_failover_policy() {
            Ok((sources, targets)) => notify(
                hwnd,
                "故障转移规则已加载",
                &format!("已配置 {sources} 个源 Provider、{targets} 个有序目标"),
            ),
            Err(error) => notify(hwnd, "故障转移规则加载失败", &error.to_string()),
        },
        ID_LOGS => {
            let path = app.inner.lock().unwrap().config.state_dir.clone();
            let _ = Command::new("explorer.exe").arg(path).spawn();
        }
        ID_UPDATE => {
            let config = app.inner.lock().unwrap().config.clone();
            if !updater::start_interactive(hwnd as usize, config) {
                notify(hwnd, "正在检查软件更新", "请等待当前更新操作完成");
            }
        }
        ID_RESTORE => {
            if unsafe {
                MessageBoxW(
                    hwnd,
                    wide("将恢复 HeadroomRoute 接管前的 Codex / Claude 配置并退出程序，是否继续？")
                        .as_ptr(),
                    wide("恢复原始配置").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                )
            } == IDYES
            {
                *app.maintenance_action.lock().unwrap() = Some("restore".into());
                unsafe {
                    DestroyWindow(host);
                }
            }
        }
        ID_REPAIR_RUNTIME => {
            if unsafe {
                MessageBoxW(
                    hwnd,
                    wide("将退出并重新检测 Headroom 环境后自动重启程序，是否继续？").as_ptr(),
                    wide("检测 Headroom 环境").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                )
            } == IDYES
            {
                *app.maintenance_action.lock().unwrap() = Some("check-runtime".into());
                unsafe {
                    DestroyWindow(host);
                }
            }
        }
        ID_SELECT_RUNTIME => match runtime::select_python() {
            Ok(Some(path)) => {
                let _config_guard = app.config_write_guard();
                let current = app.inner.lock().unwrap().config.clone();
                match runtime::config_with_python(&current, path) {
                    Ok(updated) => {
                        let config_path = updated.state_dir.join("config.json");
                        match config::save(&config_path, &updated) {
                            Ok(()) => {
                                app.inner.lock().unwrap().config = updated;
                                notify(
                                    hwnd,
                                    "Headroom 环境已保存",
                                    "验证通过；请退出并重新启动 HeadroomRoute 以使用新环境",
                                );
                            }
                            Err(error) => {
                                notify(hwnd, "保存 Headroom 环境失败", &error.to_string())
                            }
                        }
                    }
                    Err(error) => notify(hwnd, "Headroom 环境不可用", &error.to_string()),
                }
            }
            Ok(None) => {}
            Err(error) => notify(hwnd, "选择 Headroom 环境失败", &error.to_string()),
        },
        ID_UNINSTALL => {
            if unsafe {
                MessageBoxW(
                    hwnd,
                    wide(
                        "将恢复 Codex/Claude 配置、删除 HeadroomRoute 数据并取消开机启动。外部 Python/Headroom 环境不会被删除。是否完全卸载？",
                    )
                    .as_ptr(),
                    wide("完全卸载 HeadroomRoute").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                )
            } == IDYES
            {
                *app.maintenance_action.lock().unwrap() = Some("uninstall".into());
                unsafe {
                    DestroyWindow(host);
                }
            }
        }
        ID_EXIT => unsafe {
            DestroyWindow(host);
        },
        value
            if value >= ID_ROUTE_BASE
                && app.switch_index(value - ID_ROUTE_BASE, "托盘手动切换") =>
        {
            let _ = app.write_status();
            unsafe { refresh_main_window_if_visible() };
        }
        _ => {}
    }
}
