#![cfg(windows)]
use crate::{
    config,
    model::{FailoverPolicy, Protocol, Route, Snapshot},
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
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET,
        DEFAULT_GUI_FONT, DEFAULT_PITCH, DeleteObject, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD,
        GetStockObject, GetSysColorBrush, OUT_DEFAULT_PRECIS, SetBkMode, TRANSPARENT,
    },
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
        Controls::{BST_CHECKED, BST_UNCHECKED},
        Input::KeyboardAndMouse::EnableWindow,
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
            Shell_NotifyIconW,
        },
        WindowsAndMessaging::*,
    },
};

const WM_TRAY: u32 = WM_APP + 1;
const SS_CENTERIMAGE_STYLE: u32 = 0x0000_0200;
const SS_ETCHEDHORZ_STYLE: u32 = 0x0000_0010;
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
const ID_PROVIDER_IDS: usize = 119;
const ID_RELOAD_FAILOVER: usize = 120;
const ID_FAILOVER_EDITOR: usize = 121;
const ID_ROUTE_BASE: usize = 1000;
const ID_EDITOR_AUTO: usize = 200;
const ID_EDITOR_PROTOCOL: usize = 201;
const ID_EDITOR_SOURCE: usize = 202;
const ID_EDITOR_CUSTOM: usize = 203;
const ID_EDITOR_AVAILABLE: usize = 204;
const ID_EDITOR_TARGETS: usize = 205;
const ID_EDITOR_ADD: usize = 206;
const ID_EDITOR_REMOVE: usize = 207;
const ID_EDITOR_UP: usize = 208;
const ID_EDITOR_DOWN: usize = 209;
const ID_EDITOR_SAVE: usize = 210;
const ID_EDITOR_CANCEL: usize = 211;
const ID_EDITOR_STATUS: usize = 212;
const ID_EDITOR_SOURCE_DETAIL: usize = 213;
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
        let editor_class_name = wide("HeadroomRouteFailoverEditor");
        let editor_class = WNDCLASSW {
            lpfnWndProc: Some(failover_window_proc),
            hInstance: instance,
            lpszClassName: editor_class_name.as_ptr(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&editor_class) == 0 {
            anyhow::bail!("无法注册故障转移配置窗口");
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
        AppendMenuW(settings_menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            settings_menu,
            MF_STRING,
            ID_CONFIG,
            wide("打开 config.json（高级配置）").as_ptr(),
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
        ID_FAILOVER_EDITOR => unsafe { show_failover_editor(hwnd) },
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

struct FailoverEditor {
    parent: HWND,
    app: Arc<AppState>,
    routes: Vec<Route>,
    policy: FailoverPolicy,
    auto_failover: bool,
    protocol: Protocol,
    sources: Vec<String>,
    source_provider: Option<String>,
    available: Vec<String>,
    dirty: bool,
    body_font: usize,
    title_font: usize,
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn show_failover_editor(parent: HWND) {
    let Some(app) = APP.get().cloned() else {
        return;
    };
    let snapshot = app.snapshot();
    let (policy, auto_failover) = {
        let state = app.inner.lock().unwrap();
        (
            state.config.failover_policy.clone(),
            state.config.auto_failover,
        )
    };
    let protocol = if snapshot
        .routes
        .iter()
        .any(|route| route.protocol == Protocol::OpenAi)
    {
        Protocol::OpenAi
    } else {
        Protocol::Anthropic
    };
    let editor = Box::new(FailoverEditor {
        parent,
        app,
        routes: snapshot.routes,
        policy,
        auto_failover,
        protocol,
        sources: Vec::new(),
        source_provider: None,
        available: Vec::new(),
        dirty: false,
        body_font: 0,
        title_font: 0,
    });
    EnableWindow(parent, 0);
    let raw = Box::into_raw(editor);
    let instance = GetModuleHandleW(ptr::null());
    let class_name = wide("HeadroomRouteFailoverEditor");
    let title = wide("故障转移策略");
    let ex_style = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE;
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: 800,
        bottom: 640,
    };
    AdjustWindowRectEx(&mut bounds, style, 0, ex_style);
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let window = CreateWindowExW(
        ex_style,
        class_name.as_ptr(),
        title.as_ptr(),
        style,
        (GetSystemMetrics(SM_CXSCREEN) - width) / 2,
        (GetSystemMetrics(SM_CYSCREEN) - height) / 2,
        width,
        height,
        parent,
        ptr::null_mut(),
        instance,
        raw.cast(),
    );
    if window.is_null() {
        drop(Box::from_raw(raw));
        EnableWindow(parent, 1);
        return;
    }
    let mut message: MSG = std::mem::zeroed();
    while IsWindow(window) != 0 && GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
        if IsDialogMessageW(window, &message) == 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

// The editor Box is installed during WM_NCCREATE and released exactly once at WM_NCDESTROY.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn failover_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let editor = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FailoverEditor;
    match message {
        WM_CREATE => {
            (*editor).create_controls(hwnd);
            0
        }
        WM_CTLCOLORSTATIC => {
            SetBkMode(wparam as _, TRANSPARENT as i32);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            let code = (wparam >> 16) & 0xffff;
            if id == ID_EDITOR_PROTOCOL && code == CBN_SELCHANGE as usize {
                (*editor).protocol = if SendMessageW(
                    GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
                    CB_GETCURSEL,
                    0,
                    0,
                ) == 0
                {
                    Protocol::OpenAi
                } else {
                    Protocol::Anthropic
                };
                (*editor).source_provider = None;
                (*editor).refresh_sources(hwnd);
            } else if id == ID_EDITOR_SOURCE && code == CBN_SELCHANGE as usize {
                let index = SendMessageW(
                    GetDlgItem(hwnd, ID_EDITOR_SOURCE as i32),
                    CB_GETCURSEL,
                    0,
                    0,
                );
                let sources: &Vec<String> = &(*editor).sources;
                (*editor).source_provider = sources.get(index.max(0) as usize).cloned();
                (*editor).refresh_targets(hwnd);
            } else if id == ID_EDITOR_CUSTOM && code == BN_CLICKED as usize {
                if let Some(source) = (*editor).source_provider.clone() {
                    let checked =
                        SendMessageW(GetDlgItem(hwnd, ID_EDITOR_CUSTOM as i32), BM_GETCHECK, 0, 0)
                            == BST_CHECKED as isize;
                    if checked {
                        (*editor)
                            .policy
                            .rules_mut((*editor).protocol)
                            .entry(source)
                            .or_default();
                    } else {
                        (*editor)
                            .policy
                            .rules_mut((*editor).protocol)
                            .remove(&source);
                    }
                    (*editor).dirty = true;
                    (*editor).refresh_targets(hwnd);
                }
            } else if id == ID_EDITOR_AUTO && code == BN_CLICKED as usize {
                (*editor).dirty = true;
            } else if code == LBN_SELCHANGE as usize
                && matches!(id, ID_EDITOR_AVAILABLE | ID_EDITOR_TARGETS)
            {
                (*editor).update_action_buttons(hwnd);
            } else if code == LBN_DBLCLK as usize && id == ID_EDITOR_AVAILABLE {
                (*editor).add_selected(hwnd);
            } else if code == LBN_DBLCLK as usize && id == ID_EDITOR_TARGETS {
                (*editor).remove_selected(hwnd);
            } else if id == ID_EDITOR_ADD && code == BN_CLICKED as usize {
                (*editor).add_selected(hwnd);
            } else if id == ID_EDITOR_REMOVE && code == BN_CLICKED as usize {
                (*editor).remove_selected(hwnd);
            } else if id == ID_EDITOR_UP && code == BN_CLICKED as usize {
                (*editor).move_selected(hwnd, -1);
            } else if id == ID_EDITOR_DOWN && code == BN_CLICKED as usize {
                (*editor).move_selected(hwnd, 1);
            } else if id == ID_EDITOR_SAVE && code == BN_CLICKED as usize {
                let auto = SendMessageW(GetDlgItem(hwnd, ID_EDITOR_AUTO as i32), BM_GETCHECK, 0, 0)
                    == BST_CHECKED as isize;
                match (*editor)
                    .app
                    .save_failover_settings((*editor).policy.clone(), auto)
                {
                    Ok((sources, targets)) => {
                        let parent = (*editor).parent;
                        (*editor).dirty = false;
                        DestroyWindow(hwnd);
                        notify(
                            parent,
                            "故障转移配置已保存",
                            &format!("已应用 {sources} 个源 Provider、{targets} 个有序目标"),
                        );
                    }
                    Err(error) => {
                        MessageBoxW(
                            hwnd,
                            wide(&format!("保存失败：{error}")).as_ptr(),
                            wide("故障转移配置").as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
            } else if id == ID_EDITOR_CANCEL && code == BN_CLICKED as usize {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_CLOSE => {
            if (*editor).dirty
                && MessageBoxW(
                    hwnd,
                    wide("有未保存的故障转移修改，确定放弃吗？").as_ptr(),
                    wide("故障转移配置").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                ) != IDYES
            {
                return 0;
            }
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            EnableWindow((*editor).parent, 1);
            SetForegroundWindow((*editor).parent);
            0
        }
        WM_NCDESTROY => {
            if (*editor).body_font != 0 {
                DeleteObject((*editor).body_font as _);
            }
            if (*editor).title_font != 0 {
                DeleteObject((*editor).title_font as _);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(editor));
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
impl FailoverEditor {
    unsafe fn create_controls(&mut self, hwnd: HWND) {
        let stock_font = GetStockObject(DEFAULT_GUI_FONT) as usize;
        let instance = GetModuleHandleW(ptr::null());
        self.body_font = CreateFontW(
            -15,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let font = if self.body_font == 0 {
            stock_font
        } else {
            self.body_font
        };
        self.title_font = CreateFontW(
            -22,
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let title_font = if self.title_font == 0 {
            font
        } else {
            self.title_font
        };
        editor_control(
            hwnd,
            "STATIC",
            "故障转移策略",
            24,
            16,
            420,
            30,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            title_font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "为每个 Provider 指定允许转移的目标，并按优先级从上到下依次尝试。",
            24,
            48,
            720,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "启用自动故障切换",
            590,
            18,
            185,
            26,
            ID_EDITOR_AUTO,
            WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "路由范围",
            20,
            82,
            760,
            108,
            0,
            WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "协议",
            38,
            110,
            80,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "COMBOBOX",
            "",
            38,
            131,
            160,
            180,
            ID_EDITOR_PROTOCOL,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "源 Provider",
            218,
            110,
            120,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "COMBOBOX",
            "",
            218,
            131,
            532,
            220,
            ID_EDITOR_SOURCE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            218,
            161,
            532,
            18,
            ID_EDITOR_SOURCE_DETAIL,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "为此 Provider 使用自定义转移顺序",
            24,
            210,
            390,
            26,
            ID_EDITOR_CUSTOM,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "目标与优先级",
            20,
            246,
            760,
            276,
            0,
            WS_CHILD | WS_VISIBLE | BS_GROUPBOX as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "可选 Provider",
            36,
            272,
            300,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "故障转移优先级",
            444,
            272,
            300,
            20,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "LISTBOX",
            "",
            36,
            294,
            300,
            204,
            ID_EDITOR_AVAILABLE,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "LISTBOX",
            "",
            444,
            294,
            320,
            204,
            ID_EDITOR_TARGETS,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "添加  >",
            354,
            330,
            72,
            30,
            ID_EDITOR_ADD,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "<  移除",
            354,
            368,
            72,
            30,
            ID_EDITOR_REMOVE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "上移",
            354,
            422,
            72,
            30,
            ID_EDITOR_UP,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "下移",
            354,
            460,
            72,
            30,
            ID_EDITOR_DOWN,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            24,
            534,
            750,
            22,
            ID_EDITOR_STATUS,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "",
            20,
            562,
            760,
            2,
            0,
            WS_CHILD | WS_VISIBLE | SS_ETCHEDHORZ_STYLE,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "保存并应用",
            552,
            578,
            120,
            34,
            ID_EDITOR_SAVE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "取消",
            684,
            578,
            90,
            34,
            ID_EDITOR_CANCEL,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_AUTO as i32),
            BM_SETCHECK,
            if self.auto_failover {
                BST_CHECKED as usize
            } else {
                BST_UNCHECKED as usize
            },
            0,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_ADDSTRING,
            0,
            wide("Codex").as_ptr() as LPARAM,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_ADDSTRING,
            0,
            wide("Claude").as_ptr() as LPARAM,
        );
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_PROTOCOL as i32),
            CB_SETCURSEL,
            if self.protocol == Protocol::OpenAi {
                0
            } else {
                1
            },
            0,
        );
        self.refresh_sources(hwnd);
    }

    unsafe fn refresh_sources(&mut self, hwnd: HWND) {
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

    unsafe fn refresh_targets(&mut self, hwnd: HWND) {
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

    unsafe fn add_selected(&mut self, hwnd: HWND) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        if !self.policy.rules(self.protocol).contains_key(&source) {
            return;
        }
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_AVAILABLE as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let Some(provider) = self.available.get(index as usize).cloned() else {
            return;
        };
        let selected = {
            let targets = self
                .policy
                .rules_mut(self.protocol)
                .entry(source)
                .or_default();
            targets.push(provider);
            targets.len() - 1
        };
        self.dirty = true;
        self.refresh_targets(hwnd);
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_SETCURSEL,
            selected,
            0,
        );
        self.update_action_buttons(hwnd);
    }

    unsafe fn remove_selected(&mut self, hwnd: HWND) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let next = self
            .policy
            .rules_mut(self.protocol)
            .get_mut(&source)
            .and_then(|targets| {
                targets.remove(index as usize);
                (!targets.is_empty()).then_some((index as usize).min(targets.len() - 1))
            });
        self.dirty = true;
        self.refresh_targets(hwnd);
        if let Some(next) = next {
            SendMessageW(
                GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
                LB_SETCURSEL,
                next,
                0,
            );
            self.update_action_buttons(hwnd);
        }
    }

    unsafe fn move_selected(&mut self, hwnd: HWND, direction: isize) {
        let Some(source) = self.source_provider.clone() else {
            return;
        };
        let index = SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_GETCURSEL,
            0,
            0,
        );
        if index < 0 {
            return;
        }
        let Some(targets) = self.policy.rules_mut(self.protocol).get_mut(&source) else {
            return;
        };
        let Some(next) = move_target(targets, index as usize, direction) else {
            return;
        };
        self.dirty = true;
        self.refresh_targets(hwnd);
        SendMessageW(
            GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32),
            LB_SETCURSEL,
            next,
            0,
        );
        self.update_action_buttons(hwnd);
    }

    unsafe fn update_action_buttons(&self, hwnd: HWND) {
        let custom = self
            .source_provider
            .as_ref()
            .is_some_and(|source| self.policy.rules(self.protocol).contains_key(source));
        let available = GetDlgItem(hwnd, ID_EDITOR_AVAILABLE as i32);
        let targets = GetDlgItem(hwnd, ID_EDITOR_TARGETS as i32);
        let available_selected = SendMessageW(available, LB_GETCURSEL, 0, 0);
        let target_selected = SendMessageW(targets, LB_GETCURSEL, 0, 0);
        let target_count = SendMessageW(targets, LB_GETCOUNT, 0, 0);
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_ADD as i32),
            (custom && available_selected >= 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_REMOVE as i32),
            (custom && target_selected >= 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_UP as i32),
            (custom && target_selected > 0) as i32,
        );
        EnableWindow(
            GetDlgItem(hwnd, ID_EDITOR_DOWN as i32),
            (custom && target_selected >= 0 && target_selected + 1 < target_count) as i32,
        );
    }

    fn route(&self, provider: &str) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| route.provider == provider && route.protocol == self.protocol)
    }

    fn display(&self, provider: &str, ordered: bool) -> String {
        let Some(route) = self.route(provider) else {
            return provider.into();
        };
        if ordered {
            format!("{}  ·  {}", route.name, route.evidence_label())
        } else {
            format!(
                "{}  ·  {}  ·  {}",
                route.name,
                route.evidence_label(),
                route.host()
            )
        }
    }
}

#[allow(clippy::too_many_arguments, unsafe_op_in_unsafe_fn)]
unsafe fn editor_control(
    parent: HWND,
    class: &str,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: usize,
    style: u32,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    let control = CreateWindowExW(
        if class == "LISTBOX" {
            WS_EX_CLIENTEDGE
        } else {
            0
        },
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        style,
        x,
        y,
        width,
        height,
        parent,
        id as _,
        instance,
        ptr::null(),
    );
    SendMessageW(control, WM_SETFONT, font, 1);
    control
}

fn move_target(targets: &mut [String], selected: usize, direction: isize) -> Option<usize> {
    let next = selected.checked_add_signed(direction)?;
    if next >= targets.len() {
        return None;
    }
    targets.swap(selected, next);
    Some(next)
}

fn failover_sources(routes: &[Route], policy: &FailoverPolicy, protocol: Protocol) -> Vec<String> {
    let mut sources: Vec<String> = routes
        .iter()
        .filter(|route| route.protocol == protocol)
        .map(|route| route.provider.clone())
        .collect();
    for provider in policy.rules(protocol).keys() {
        if !sources.contains(provider) {
            sources.push(provider.clone());
        }
    }
    sources
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
        ID_RESTART, ID_SELECT_RUNTIME, compact_number, failover_sources, move_target,
        recommended_action, route_is_selected,
    };
    use crate::model::{AuthStyle, FailoverPolicy, Protocol, Route};

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

    #[test]
    fn moves_failover_targets_without_crossing_list_bounds() {
        let mut targets = vec!["one".into(), "two".into(), "three".into()];
        assert_eq!(move_target(&mut targets, 1, -1), Some(0));
        assert_eq!(targets, ["two", "one", "three"]);
        assert_eq!(move_target(&mut targets, 0, -1), None);
        assert_eq!(move_target(&mut targets, 2, 1), None);
    }

    #[test]
    fn editor_keeps_stale_configured_sources_visible() {
        let route = Route::new(
            Protocol::OpenAi,
            "active".into(),
            "Active".into(),
            "https://active.example.com/v1".into(),
            None,
            AuthStyle::PassThrough,
            "test",
        );
        let mut policy = FailoverPolicy::default();
        policy.openai.insert("deleted".into(), Vec::new());
        assert_eq!(
            failover_sources(&[route], &policy, Protocol::OpenAi),
            ["active", "deleted"]
        );
    }
}
