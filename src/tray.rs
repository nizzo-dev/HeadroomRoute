#![cfg(windows)]
use crate::{
    config,
    model::{Protocol, Route, Snapshot},
    runtime,
    state::AppState,
    updater,
};
use std::{
    cell::Cell,
    ffi::c_void,
    mem::size_of,
    process::Command,
    ptr,
    sync::{Arc, OnceLock, atomic::Ordering},
    thread,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        LibraryLoader::GetModuleHandleW,
        Ole::CF_UNICODETEXT,
        Registry::{
            HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteValueW, RegSetValueExW,
        },
    },
    UI::{
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
            Shell_NotifyIconW,
        },
        WindowsAndMessaging::*,
    },
};

const WM_TRAY: u32 = WM_APP + 1;
const SS_CENTERIMAGE_STYLE: u32 = 0x0000_0200;
const ID_OPEN_STATUS: usize = 100;
const ID_SYNC: usize = 101;
const ID_CHECK: usize = 102;
const ID_RESTART: usize = 103;
const ID_AUTO: usize = 104;
const ID_STARTUP: usize = 105;
const ID_DIAG: usize = 106;
const ID_CONFIG: usize = 107;
const ID_LOGS: usize = 108;
const ID_EXIT: usize = 109;
const ID_RESTORE: usize = 110;
const ID_REPAIR_RUNTIME: usize = 111;
const ID_UNINSTALL: usize = 112;
const ID_UPDATE: usize = 113;
const ID_BYPASS: usize = 114;
const ID_SELECT_RUNTIME: usize = 115;
const ID_RESET_METRICS: usize = 117;
const ID_AUTO_UPDATE: usize = 118;
const ID_ROUTE_BASE: usize = 1000;
static APP: OnceLock<Arc<AppState>> = OnceLock::new();
thread_local! { static URL_POPUP: Cell<HWND> = const { Cell::new(ptr::null_mut()) }; }

pub fn run(app: Arc<AppState>) -> anyhow::Result<()> {
    let _ = APP.set(app);
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("HeadroomRouteTrayWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("无法注册托盘窗口");
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Headroom Route").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            anyhow::bail!("无法创建托盘窗口");
        }
        add_icon(hwnd);
        SetTimer(hwnd, 1, 500, None);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        remove_icon(hwnd);
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY if lparam as u32 == WM_RBUTTONUP || lparam as u32 == WM_CONTEXTMENU => {
            unsafe { show_menu(hwnd) };
            0
        }
        WM_TRAY if lparam as u32 == WM_LBUTTONDBLCLK => {
            unsafe { show_status(hwnd) };
            0
        }
        WM_COMMAND => {
            unsafe { handle_command(hwnd, wparam & 0xffff) };
            0
        }
        WM_MENUSELECT => {
            unsafe { show_hovered_route_url(hwnd, wparam) };
            0
        }
        WM_EXITMENULOOP => {
            unsafe { hide_route_url() };
            0
        }
        WM_TIMER => {
            unsafe { update_icon(hwnd) };
            if let Some(app) = APP.get() {
                if let Some((ok, message)) = app.take_sync_result() {
                    if ok {
                        notify(hwnd, "同步完成", &message);
                    } else {
                        notify(hwnd, "同步失败", &message);
                    }
                }
                if let Some(message) = app.take_model_change_notice() {
                    notify(hwnd, "模型配置已更新", &message);
                }
                if let Some(message) = app.take_auto_switch_notice() {
                    notify(hwnd, "已自动切换上游", &message);
                }
                if let Some((ok, message)) = app.take_runtime_result() {
                    notify(
                        hwnd,
                        if ok {
                            "Headroom 环境就绪"
                        } else {
                            "Headroom 环境不可用"
                        },
                        &message,
                    );
                }
                if let Some(message) = app.take_config_change_notice() {
                    notify(hwnd, "CC-Switch 配置已变化", &message);
                }
                if let Some((ok, message)) = app.take_routing_notice() {
                    notify(
                        hwnd,
                        if ok {
                            "配置接管已恢复"
                        } else {
                            "配置接管恢复失败"
                        },
                        &message,
                    );
                }
                if let Some(message) = app.take_update_notice() {
                    notify(hwnd, "发现软件更新", &message);
                }
                if let Some((ok, message)) = app.take_restart_result() {
                    if ok {
                        notify(hwnd, "Headroom 重启完成", &message);
                    } else {
                        notify(hwnd, "Headroom 重启失败", &message);
                    }
                }
            }
            0
        }
        WM_DESTROY => {
            unsafe { destroy_route_url() };
            if let Some(app) = APP.get() {
                app.stop.store(true, Ordering::Relaxed);
            }
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn show_menu(hwnd: HWND) {
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
    let service = if snapshot.bypass_headroom {
        format!("旁路 Headroom  ·  路由{}", health_cn(snapshot.state))
    } else {
        format!(
            "Headroom：{}  ·  路由{}",
            headroom_cn(&snapshot.headroom_state),
            health_cn(snapshot.state)
        )
    };
    let codex = format!(
        "Codex：{}  ·  {}  ·  {} ms",
        snapshot.codex_availability,
        snapshot.active_name.as_deref().unwrap_or("未配置"),
        latency_text(snapshot.latency_ms)
    );
    let claude = format!(
        "Claude：{}  ·  {}  ·  {} ms",
        snapshot.claude_availability,
        snapshot
            .active_anthropic_name
            .as_deref()
            .unwrap_or("未配置"),
        latency_text(snapshot.anthropic_latency_ms)
    );
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
            snapshot.bypass_headroom,
            &snapshot.headroom_state,
            snapshot.codex_availability,
            snapshot.claude_availability,
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
    }
    unsafe {
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_CHECK, wide("立即检查上游").as_ptr());
        AppendMenuW(
            menu,
            MF_STRING | if snapshot.auto_enabled { MF_CHECKED } else { 0 },
            ID_AUTO,
            wide("自动故障切换").as_ptr(),
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
        AppendMenuW(settings_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_CONFIG,
            wide("打开 config.json").as_ptr(),
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
            wide("退出 HeadroomRoute").as_ptr(),
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

fn recommended_action(
    bypass_headroom: bool,
    headroom_state: &str,
    codex: &str,
    claude: &str,
    error: Option<&str>,
) -> Option<(usize, &'static str)> {
    if !bypass_headroom && headroom_state == "runtime-unavailable" {
        return Some((ID_SELECT_RUNTIME, "建议操作：选择 Headroom Python..."));
    }
    if matches!(
        headroom_state,
        "检测中" | "运行环境就绪" | "starting" | "restarting"
    ) {
        return None;
    }
    if !bypass_headroom && !matches!(headroom_state, "healthy" | "external") {
        return Some((ID_RESTART, "建议操作：重启 Headroom"));
    }
    let error = error.unwrap_or_default().to_ascii_lowercase();
    if ["同步", "配置", "routing", "route guard"]
        .iter()
        .any(|word| error.contains(word))
    {
        return Some((ID_SYNC, "建议操作：重新同步配置"));
    }
    if matches!(codex, "降级" | "不可用") || matches!(claude, "降级" | "不可用") {
        return Some((ID_CHECK, "建议操作：立即检查上游"));
    }
    (!error.is_empty()).then_some((ID_DIAG, "建议操作：复制脱敏诊断报告"))
}

unsafe fn route_menu(
    snapshot: &Snapshot,
    protocol: Protocol,
    active_provider: Option<&str>,
) -> HMENU {
    let menu = unsafe { CreatePopupMenu() };
    let mut count = 0;
    for (index, route) in snapshot.routes.iter().take(32).enumerate() {
        if route.protocol != protocol {
            continue;
        }
        count += 1;
        let flags = MF_STRING
            | if route_is_selected(route, active_provider) {
                MF_CHECKED
            } else {
                0
            };
        let text = format!(
            "{}  ·  {}  ·  {} ms",
            route.name,
            route.evidence_label(),
            latency_text(route.latency_ms)
        );
        unsafe { AppendMenuW(menu, flags, ID_ROUTE_BASE + index, wide(&text).as_ptr()) };
    }
    if count == 0 {
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING | MF_DISABLED | MF_GRAYED,
                0,
                wide("未发现可用上游").as_ptr(),
            )
        };
    }
    menu
}

fn route_is_selected(route: &Route, active_provider: Option<&str>) -> bool {
    active_provider == Some(route.provider.as_str())
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    let Some(app) = APP.get() else { return };
    match id {
        ID_OPEN_STATUS => unsafe { show_status(hwnd) },
        ID_CHECK => {
            app.force_probe.store(true, Ordering::Relaxed);
            notify(hwnd, "正在检查上游", "检查结果会自动更新到托盘状态");
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
                match config::sync_all(&cfg, active_url.as_deref()) {
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
            let enabled = {
                let mut state = app.inner.lock().unwrap();
                state.config.start_with_windows = !state.config.start_with_windows;
                let path = state.config.state_dir.join("config.json");
                let _ = config::save(&path, &state.config);
                state.config.start_with_windows
            };
            if let Err(e) = set_startup(enabled) {
                notify(hwnd, "开机启动设置失败", &e.to_string())
            }
        }
        ID_AUTO_UPDATE => match app.toggle_auto_update_check() {
            Ok(true) => notify(hwnd, "自动更新提醒已启用", "每天最多检查一次，只提醒不安装"),
            Ok(false) => notify(hwnd, "自动更新提醒已关闭", "仍可随时手动检查更新"),
            Err(error) => notify(hwnd, "自动更新提醒设置失败", &error.to_string()),
        },
        ID_DIAG => {
            let text = app.diagnostic_text();
            if copy_clipboard(hwnd, &text).is_ok() {
                notify(hwnd, "诊断报告已复制", "报告不包含 API Key")
            };
        }
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
                    DestroyWindow(hwnd);
                }
            }
        }
        ID_REPAIR_RUNTIME => {
            if unsafe {
                MessageBoxW(
                    hwnd,
                    wide("将退出程序并重新检测 config.json 中配置的 Headroom 环境，是否继续？")
                        .as_ptr(),
                    wide("检测 Headroom 环境").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                )
            } == IDYES
            {
                *app.maintenance_action.lock().unwrap() = Some("check-runtime".into());
                unsafe {
                    DestroyWindow(hwnd);
                }
            }
        }
        ID_SELECT_RUNTIME => match runtime::select_python() {
            Ok(Some(path)) => {
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
                    DestroyWindow(hwnd);
                }
            }
        }
        ID_EXIT => unsafe {
            DestroyWindow(hwnd);
        },
        value
            if value >= ID_ROUTE_BASE
                && app.switch_index(value - ID_ROUTE_BASE, "托盘手动切换") =>
        {
            let _ = app.write_status();
        }
        _ => {}
    }
}

unsafe fn show_status(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let s = app.snapshot();
    let text = format!(
        "【当前路由】\r\n模式：{}\r\nCodex：{} · {}\r\nClaude：{} · {}\r\n\r\n【服务状态】\r\n整体路由：{}\r\n自动切换：{}\r\nHeadroom：{}\r\n配置同步：{}\r\n重启任务：{}\r\n\r\n【Headroom 指标】\r\n统计范围：{}\r\n压缩 Token：{} → {}\r\n节省 Token：{}（{:.1}%）\r\n完成请求：{}\r\n失败请求：{}（{:.1}%）\r\n\r\n【最近活动】\r\n可用路由：{}\r\n最近切换：{}\r\n最近错误：{}\r\n\r\n【恢复建议】\r\n{}",
        if s.bypass_headroom {
            "旁路 Headroom"
        } else {
            "经过 Headroom"
        },
        s.codex_availability,
        app.route_summary(Protocol::OpenAi),
        s.claude_availability,
        app.route_summary(Protocol::Anthropic),
        health_cn(s.state),
        if s.auto_enabled {
            "已启用"
        } else {
            "未启用"
        },
        headroom_cn(&s.headroom_state),
        s.sync_status,
        s.restart_status,
        s.headroom_metrics_since.map_or_else(
            || "当前日志文件累计".into(),
            |since| format!("自 {} UTC", since.format("%Y-%m-%d %H:%M:%S")),
        ),
        s.headroom_metrics.input_tokens_original,
        s.headroom_metrics.input_tokens_optimized,
        s.headroom_metrics.tokens_saved,
        s.headroom_metrics.compression_percent(),
        s.headroom_metrics.completed_requests,
        s.headroom_metrics.failed_requests,
        s.headroom_metrics.failure_percent(),
        s.routes.len(),
        s.last_switch_reason.as_deref().unwrap_or("无"),
        s.last_error.as_deref().unwrap_or("无"),
        app.recovery_hint()
    );
    unsafe {
        MessageBoxW(
            hwnd,
            wide(&text).as_ptr(),
            wide("Headroom Route 状态").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

unsafe fn show_hovered_route_url(hwnd: HWND, wparam: WPARAM) {
    let id = wparam & 0xffff;
    let Some(route) = APP.get().and_then(|app| {
        app.snapshot()
            .routes
            .get(id.wrapping_sub(ID_ROUTE_BASE))
            .cloned()
    }) else {
        unsafe { hide_route_url() };
        return;
    };
    URL_POPUP.with(|slot| unsafe {
        let mut popup = slot.get();
        if popup.is_null() {
            popup = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(),
                ptr::null(),
                WS_POPUP | WS_BORDER | SS_CENTERIMAGE_STYLE,
                0,
                0,
                0,
                0,
                hwnd,
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            );
            slot.set(popup);
        }
        if popup.is_null() {
            return;
        }
        SetWindowTextW(popup, wide(&route.base_url).as_ptr());
        let mut point = POINT::default();
        GetCursorPos(&mut point);
        let width = (route.base_url.chars().count() as i32 * 7 + 28).clamp(300, 720);
        SetWindowPos(
            popup,
            HWND_TOPMOST,
            point.x + 18,
            point.y + 18,
            width,
            30,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    });
}

unsafe fn hide_route_url() {
    URL_POPUP.with(|slot| {
        let popup = slot.get();
        if !popup.is_null() {
            unsafe {
                ShowWindow(popup, SW_HIDE);
            }
        }
    });
}

unsafe fn destroy_route_url() {
    URL_POPUP.with(|slot| {
        let popup = slot.replace(ptr::null_mut());
        if !popup.is_null() {
            unsafe {
                DestroyWindow(popup);
            }
        }
    });
}

unsafe fn add_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &data);
        DestroyIcon(data.hIcon);
    }
}
unsafe fn remove_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
        DestroyIcon(data.hIcon);
    }
}
unsafe fn update_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_MODIFY, &data);
        DestroyIcon(data.hIcon);
    }
}
fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let (tip, health) = APP
        .get()
        .map(|app| {
            let s = app.snapshot();
            let codex = s
                .active_name
                .as_deref()
                .unwrap_or("未配置")
                .chars()
                .take(24)
                .collect::<String>();
            let claude = s
                .active_anthropic_name
                .as_deref()
                .unwrap_or("未配置")
                .chars()
                .take(24)
                .collect::<String>();
            (
                format!(
                    "Headroom Route\r\nCodex：{codex}\r\nClaude：{claude}\r\n{} · {} · {}",
                    health_cn(s.state),
                    s.sync_status,
                    s.restart_status
                ),
                s.state,
            )
        })
        .unwrap_or(("Headroom Route".into(), "unknown"));
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = 1;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = make_icon(health);
    let chars = wide(&tip);
    for (dst, src) in data.szTip.iter_mut().zip(chars) {
        *dst = src;
    }
    data
}
fn make_icon(health: &str) -> *mut c_void {
    let color = match health {
        "healthy" => 0x00_3c_b3_71u32,
        "degraded" => 0x00_00_a5_ff,
        "unavailable" => 0x00_43_43_dc,
        _ => 0x00_80_80_80,
    };
    let mut and_mask = [0xffu8; 32];
    let mut xor = [0u8; 1024];
    draw_icon_line(&mut and_mask, &mut xor, 3, 12, 7, 8, color);
    draw_icon_line(&mut and_mask, &mut xor, 7, 8, 7, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 7, 4, 13, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 11, 2, 13, 4, color);
    draw_icon_line(&mut and_mask, &mut xor, 11, 6, 13, 4, color);
    for (x, y) in [(3, 12), (7, 8), (7, 4)] {
        draw_icon_node(&mut and_mask, &mut xor, x, y, color);
    }
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::CreateIcon(
            ptr::null_mut(),
            16,
            16,
            1,
            32,
            and_mask.as_ptr(),
            xor.as_ptr(),
        )
    }
}
fn set_icon_pixel(and_mask: &mut [u8; 32], xor: &mut [u8; 1024], x: i32, y: i32, color: u32) {
    if !(0..16).contains(&x) || !(0..16).contains(&y) {
        return;
    }
    let row = (15 - y) as usize;
    let x = x as usize;
    and_mask[row * 2 + x / 8] &= !(0x80 >> (x % 8));
    let offset = (row * 16 + x) * 4;
    xor[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
}
fn draw_icon_line(
    and_mask: &mut [u8; 32],
    xor: &mut [u8; 1024],
    mut x0: i32,
    mut y0: i32,
    x1: i32,
    y1: i32,
    color: u32,
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        set_icon_pixel(and_mask, xor, x0, y0, color);
        set_icon_pixel(and_mask, xor, x0, y0 + 1, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x0 += sx
        }
        if twice <= dx {
            error += dx;
            y0 += sy
        }
    }
}
fn draw_icon_node(and_mask: &mut [u8; 32], xor: &mut [u8; 1024], x: i32, y: i32, color: u32) {
    for (dx, dy) in [
        (0, -2),
        (-1, -1),
        (0, -1),
        (1, -1),
        (-2, 0),
        (-1, 0),
        (0, 0),
        (1, 0),
        (2, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
        (0, 2),
    ] {
        set_icon_pixel(and_mask, xor, x + dx, y + dy, 0x00_ff_ff_ff)
    }
    set_icon_pixel(and_mask, xor, x, y, color);
}
fn notify(hwnd: HWND, title: &str, message: &str) {
    let mut data = notify_data(hwnd);
    data.uFlags |= windows_sys::Win32::UI::Shell::NIF_INFO;
    for (d, s) in data.szInfoTitle.iter_mut().zip(wide(title)) {
        *d = s;
    }
    for (d, s) in data.szInfo.iter_mut().zip(wide(message)) {
        *d = s;
    }
    unsafe {
        Shell_NotifyIconW(NIM_MODIFY, &data);
        DestroyIcon(data.hIcon);
    };
}

fn set_startup(enabled: bool) -> anyhow::Result<()> {
    unsafe {
        let mut key = ptr::null_mut();
        let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            ptr::null_mut(),
            0,
            KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        ) != 0
        {
            anyhow::bail!("无法打开启动项注册表")
        };
        let name = wide("HeadroomRoute");
        let result = if enabled {
            let exe = std::env::current_exe()?;
            let value = wide(&format!("\"{}\"", exe.display()));
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(key, name.as_ptr())
        };
        RegCloseKey(key);
        if result != 0 && enabled {
            anyhow::bail!("注册表写入失败: {result}")
        };
        Ok(())
    }
}
fn copy_clipboard(hwnd: HWND, text: &str) -> anyhow::Result<()> {
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            anyhow::bail!("无法打开剪贴板")
        };
        EmptyClipboard();
        let value = wide(text);
        let bytes = value.len() * 2;
        let memory = windows_sys::Win32::System::Memory::GlobalAlloc(
            windows_sys::Win32::System::Memory::GMEM_MOVEABLE,
            bytes,
        );
        if memory.is_null() {
            CloseClipboard();
            anyhow::bail!("内存分配失败")
        };
        let target = windows_sys::Win32::System::Memory::GlobalLock(memory) as *mut u16;
        ptr::copy_nonoverlapping(value.as_ptr(), target, value.len());
        windows_sys::Win32::System::Memory::GlobalUnlock(memory);
        SetClipboardData(CF_UNICODETEXT.into(), memory as _);
        CloseClipboard();
        Ok(())
    }
}
fn health_cn(state: &str) -> &'static str {
    match state {
        "healthy" => "健康",
        "degraded" => "降级",
        "unavailable" => "不可用",
        _ => "检测中",
    }
}
fn headroom_cn(state: &str) -> &str {
    match state {
        "healthy" => "运行正常",
        "external" => "外部实例",
        "starting" => "正在启动",
        "restarting" => "正在重启",
        "unavailable" | "runtime-unavailable" => "不可用",
        _ => state,
    }
}
fn latency_text(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "--".into())
}
fn compact_number(value: u64) -> String {
    let (divisor, suffix) = if value >= 1_000_000_000 {
        (1_000_000_000, "B")
    } else if value >= 1_000_000 {
        (1_000_000, "M")
    } else if value >= 1_000 {
        (1_000, "K")
    } else {
        return value.to_string();
    };
    let number = format!("{:.1}", value as f64 / divisor as f64);
    format!("{}{suffix}", number.trim_end_matches(".0"))
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ID_RESTART, ID_SELECT_RUNTIME, compact_number, recommended_action, route_is_selected,
    };
    use crate::model::{AuthStyle, Protocol, Route};

    #[test]
    fn duplicate_urls_select_only_the_active_provider() {
        let first = Route::new(
            Protocol::OpenAi,
            "first".into(),
            "First".into(),
            "https://same.example.com/v1".into(),
            Some("key-a".into()),
            AuthStyle::Bearer,
            "test",
        );
        let second = Route::new(
            Protocol::OpenAi,
            "second".into(),
            "Second".into(),
            "https://same.example.com/v1".into(),
            Some("key-b".into()),
            AuthStyle::Bearer,
            "test",
        );
        assert!(!route_is_selected(&first, Some("second")));
        assert!(route_is_selected(&second, Some("second")));
    }

    #[test]
    fn compacts_large_status_numbers() {
        assert_eq!(compact_number(999), "999");
        assert_eq!(compact_number(1_000), "1K");
        assert_eq!(compact_number(12_345), "12.3K");
        assert_eq!(compact_number(1_000_000), "1M");
    }

    #[test]
    fn recommends_the_action_closest_to_the_fault() {
        assert_eq!(
            recommended_action(false, "runtime-unavailable", "不可用", "不可用", None)
                .map(|action| action.0),
            Some(ID_SELECT_RUNTIME)
        );
        assert_eq!(
            recommended_action(false, "unavailable", "不可用", "不可用", None)
                .map(|action| action.0),
            Some(ID_RESTART)
        );
    }
}
