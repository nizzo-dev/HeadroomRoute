#![cfg(windows)]
use crate::{
    approval::{self, ApprovalChoice, ApprovalRequest},
    config,
    model::{FailoverPolicy, Protocol, Route, Snapshot},
    notification, runtime,
    state::AppState,
    updater,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::size_of,
    process::Command,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    thread,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    },
    Graphics::Gdi::{
        BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW,
        CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateRoundRectRgn,
        CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_PITCH, DT_CENTER,
        DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_TOP, DT_VCENTER, DT_WORDBREAK,
        DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD,
        FillRect, GetMonitorInfoW, GetStockObject, GetSysColorBrush, InvalidateRect,
        MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
        PS_SOLID, RoundRect, SRCCOPY, ScreenToClient, SelectObject, SetBkMode, SetTextColor,
        SetWindowRgn, TRANSPARENT,
    },
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        LibraryLoader::GetModuleHandleW,
        Ole::CF_UNICODETEXT,
        Registry::{
            HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteValueW, RegSetValueExW,
        },
        Threading::{
            CreateMutexW, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        Controls::{BST_CHECKED, BST_UNCHECKED, WM_MOUSELEAVE},
        Input::KeyboardAndMouse::{EnableWindow, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        Shell::{
            NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::GUID;

const WM_TRAY: u32 = WM_APP + 1;
const SS_CENTERIMAGE_STYLE: u32 = 0x0000_0200;
const SS_NOPREFIX_STYLE: u32 = 0x0000_0080;
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
const ID_SHOW_API_KEY: usize = 116;
const ID_RESET_METRICS: usize = 117;
const ID_AUTO_UPDATE: usize = 118;
const ID_PROVIDER_IDS: usize = 119;
const ID_RELOAD_FAILOVER: usize = 120;
const ID_FAILOVER_EDITOR: usize = 121;
const ID_APPROVAL_DEMO: usize = 122;
const ID_DIRECT_CODEX: usize = 123;
const ID_DIRECT_CLAUDE: usize = 124;
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
const TRAY_ICON_GUID: GUID = GUID::from_u128(0x5bdb64d1_1bb9_4d6d_9cb3_496b8e5a6d53);
const APPROVAL_HOST_MUTEX_NAME: &str = "Local\\HeadroomRouteApprovalHost-v1";
static APP: OnceLock<Arc<AppState>> = OnceLock::new();
static APPROVAL_HOST_PARENT_PID: AtomicU32 = AtomicU32::new(0);
thread_local! { static URL_POPUP: Cell<HWND> = const { Cell::new(ptr::null_mut()) }; }
thread_local! { static APPROVAL_POPUP: Cell<HWND> = const { Cell::new(ptr::null_mut()) }; }
thread_local! { static APPROVAL_REQUEST: RefCell<Option<ApprovalRequest>> = const { RefCell::new(None) }; }
thread_local! { static APPROVAL_VISUAL: RefCell<Option<ApprovalVisual>> = const { RefCell::new(None) }; }

const APPROVAL_ANIMATION_TIMER: usize = 3;
const APPROVAL_OPEN_MS: u128 = 220;
const APPROVAL_CLOSE_MS: u128 = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalPhase {
    Opening,
    Open,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalHit {
    None,
    Deny,
    Rule,
    Allow,
}

struct ApprovalVisual {
    phase: ApprovalPhase,
    started_at: std::time::Instant,
    dpi: u32,
    anchor_center: i32,
    anchor_top: i32,
    compact_width: i32,
    compact_height: i32,
    expanded_width: i32,
    expanded_height: i32,
    current_width: i32,
    current_height: i32,
    current_alpha: u8,
    animation_from_width: i32,
    animation_from_height: i32,
    animation_from_alpha: u8,
    hover: ApprovalHit,
    title_font: usize,
    body_font: usize,
    small_font: usize,
}

pub fn run(app: Arc<AppState>) -> anyhow::Result<()> {
    let _ = APP.set(app);
    approval::start_server();
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
        let approval_class_name = wide("HeadroomRouteApprovalWindow");
        let approval_class = WNDCLASSW {
            lpfnWndProc: Some(approval_window_proc),
            hInstance: instance,
            lpszClassName: approval_class_name.as_ptr(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&approval_class) == 0 {
            anyhow::bail!("无法注册确认悬浮窗");
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
        approval::set_tray_hwnd(hwnd);
        add_icon(hwnd);
        SetTimer(hwnd, 1, 500, None);
        if std::env::args().any(|arg| arg == "--approval-demo") {
            let _ = approval::enqueue_demo();
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        hide_approval_popup();
        approval::clear_tray_hwnd(hwnd);
        remove_icon(hwnd);
    }
    Ok(())
}

pub fn run_approval_host() -> anyhow::Result<()> {
    let parent_pid = std::env::args().find_map(|arg| {
        arg.strip_prefix("--parent-pid=")
            .and_then(|value| value.parse::<u32>().ok())
    });
    APPROVAL_HOST_PARENT_PID.store(parent_pid.unwrap_or_default(), Ordering::Release);
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, wide(APPROVAL_HOST_MUTEX_NAME).as_ptr()) };
    if mutex.is_null() {
        anyhow::bail!("无法创建确认悬浮窗单实例锁");
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return Ok(());
    }

    approval::start_server();
    let result = unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("HeadroomRouteApprovalHostWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(approval_host_window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("无法注册确认悬浮窗宿主窗口");
        }
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("HeadroomRoute Approval Host").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            anyhow::bail!("无法创建确认悬浮窗宿主窗口");
        }
        approval::set_tray_hwnd(hwnd);
        SetTimer(hwnd, 1, 100, None);
        if std::env::args().any(|arg| arg == "--approval-demo") {
            let _ = approval::enqueue_demo();
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        hide_approval_popup();
        approval::clear_tray_hwnd(hwnd);
        Ok(())
    };
    unsafe { CloseHandle(mutex) };
    result
}

fn approval_host_parent_alive() -> bool {
    let pid = APPROVAL_HOST_PARENT_PID.load(Ordering::Acquire);
    if pid == 0 {
        return true;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let alive = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0 && exit_code == 259;
    unsafe { CloseHandle(handle) };
    alive
}

unsafe extern "system" fn approval_host_window_proc(
    hwnd: HWND,
    message: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER => {
            if !approval_host_parent_alive() {
                unsafe { DestroyWindow(hwnd) };
                return 0;
            }
            unsafe { refresh_approval_popup() };
            0
        }
        approval::WM_APPROVAL => {
            unsafe { refresh_approval_popup() };
            0
        }
        WM_DESTROY => {
            unsafe { hide_approval_popup() };
            approval::clear_tray_hwnd(hwnd);
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, _wparam, _lparam) },
    }
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
            unsafe { refresh_approval_popup() };
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
            unsafe { hide_approval_popup() };
            approval::clear_tray_hwnd(hwnd);
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
    let service = format!(
        "Codex：{}  ·  Claude：{}  ·  路由{}",
        mode_cn(snapshot.direct_codex, snapshot.bypass_headroom),
        mode_cn(snapshot.direct_claude, snapshot.bypass_headroom),
        health_cn(snapshot.state)
    );
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
            snapshot.bypass_headroom || (snapshot.direct_codex && snapshot.direct_claude),
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
        ID_FAILOVER_EDITOR => unsafe { show_failover_editor(hwnd) },
        ID_DIRECT_CODEX => match app.toggle_direct(Protocol::OpenAi) {
            Ok(true) => notify(
                hwnd,
                "Codex 已直连上游",
                "已应用当前 Provider 的地址、模型和凭据；切换 Provider 后重启 Codex，退出时可交还 CC-Switch",
            ),
            Ok(false) => notify(
                hwnd,
                "Codex 已恢复路由",
                "Codex 将重新使用当前 Headroom 模式",
            ),
            Err(error) => notify(hwnd, "Codex 直连切换失败", &error.to_string()),
        },
        ID_DIRECT_CLAUDE => match app.toggle_direct(Protocol::Anthropic) {
            Ok(true) => notify(
                hwnd,
                "Claude 已直连上游",
                "已应用当前 Provider 的地址、模型和凭据；切换 Provider 后重启 Claude Code，退出时可交还 CC-Switch",
            ),
            Ok(false) => notify(
                hwnd,
                "Claude 已恢复路由",
                "Claude Code 将重新使用当前 Headroom 模式",
            ),
            Err(error) => notify(hwnd, "Claude 直连切换失败", &error.to_string()),
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
            let enabled = {
                let _config_guard = app.config_write_guard();
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
                        notification::error("故障转移配置", format!("保存失败：{error}"));
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
    let mode = format!(
        "Codex {}；Claude {}",
        mode_cn(s.direct_codex, s.bypass_headroom),
        mode_cn(s.direct_claude, s.bypass_headroom)
    );
    let text = format!(
        "【当前路由】\r\n模式：{}\r\nCodex：{} · {}\r\nClaude：{} · {}\r\n\r\n【服务状态】\r\n整体路由：{}\r\n自动切换：{}\r\nHeadroom：{}\r\n配置同步：{}\r\n重启任务：{}\r\n\r\n【Headroom 指标】\r\n统计范围：{}\r\n压缩 Token：{} → {}\r\n节省 Token：{}（{:.1}%）\r\n完成请求：{}\r\n失败请求：{}（{:.1}%）\r\n\r\n【最近活动】\r\n可用路由：{}\r\n最近切换：{}\r\n最近错误：{}\r\n\r\n【恢复建议】\r\n{}",
        mode,
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
    let _ = hwnd;
    notification::info("Headroom Route 状态", text);
}

const HOVER_KEY_LINE_WIDTH: usize = 64;

fn wrap_hover_value(value: &str, width: usize) -> String {
    let value: Vec<char> = value
        .chars()
        .filter(|character| !matches!(character, '\0' | '\r' | '\n'))
        .collect();
    value
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\r\n")
}

fn route_hover_text(route: &Route, show_key: bool) -> String {
    if !show_key {
        return route.base_url.clone();
    }
    let key = route
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .map(|key| wrap_hover_value(key, HOVER_KEY_LINE_WIDTH))
        .unwrap_or_else(|| "未配置".into());
    format!("{}\r\nAPI Key：{key}", route.base_url)
}

fn hover_popup_size(text: &str) -> (i32, i32) {
    let max_chars = text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let line_count = text.lines().count().max(1);
    let width = (i32::try_from(max_chars).unwrap_or(100) * 8 + 32).clamp(300, 900);
    let height = (i32::try_from(line_count).unwrap_or(1) * 18 + 16).max(30);
    (width, height)
}

unsafe fn show_hovered_route_url(hwnd: HWND, wparam: WPARAM) {
    let id = wparam & 0xffff;
    let Some((route, show_key)) = APP.get().and_then(|app| {
        let snapshot = app.snapshot();
        snapshot
            .routes
            .get(id.wrapping_sub(ID_ROUTE_BASE))
            .cloned()
            .map(|route| (route, snapshot.show_api_key_on_hover))
    }) else {
        unsafe { hide_route_url() };
        return;
    };
    let hover_text = route_hover_text(&route, show_key);
    URL_POPUP.with(|slot| unsafe {
        let mut popup = slot.get();
        if popup.is_null() {
            popup = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(),
                ptr::null(),
                WS_POPUP | WS_BORDER | SS_CENTERIMAGE_STYLE | SS_NOPREFIX_STYLE,
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
        SetWindowTextW(popup, wide(&hover_text).as_ptr());
        let mut point = POINT::default();
        GetCursorPos(&mut point);
        let (width, height) = hover_popup_size(&hover_text);
        SetWindowPos(
            popup,
            HWND_TOPMOST,
            point.x + 18,
            point.y + 18,
            width,
            height,
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
    data.uFlags = NIF_GUID | NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.guidItem = TRAY_ICON_GUID;
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
    let _ = hwnd;
    if title.contains("失败") || title.contains("错误") || message.contains("失败") {
        notification::error(title, message);
    } else if title.contains("警告") || message.contains("不可用") || message.contains("已删除")
    {
        notification::warning(title, message);
    } else if title.contains("完成") || title.contains("成功") || message.contains("已恢复")
    {
        notification::success(title, message);
    } else {
        notification::info(title, message);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn refresh_approval_popup() {
    if APPROVAL_VISUAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|visual| visual.phase == ApprovalPhase::Closing)
    }) {
        return;
    }
    let current_id =
        APPROVAL_REQUEST.with(|request| request.borrow().as_ref().map(|request| request.id));
    if let Some(id) = current_id {
        if !approval::is_pending(id) || !approval::should_show(id) {
            unsafe { begin_approval_close() };
        } else {
            unsafe { update_approval_countdown() };
            return;
        }
        return;
    }
    let Some(request) = approval::next_request() else {
        return;
    };
    APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = Some(request));
    let work_area = unsafe { approval_work_area() };
    let dpi = unsafe { approval_dpi() };
    let scale = |value: i32| value.saturating_mul(dpi as i32) / 96;
    let compact_width = scale(280);
    let compact_height = scale(58);
    let expanded_width =
        scale(520).min((work_area.right - work_area.left - scale(24)).max(compact_width));
    let expanded_height = scale(286);
    let animate = unsafe { approval_animation_enabled() };
    let anchor_center = work_area.left + (work_area.right - work_area.left) / 2;
    let anchor_top = work_area.top + scale(18);
    APPROVAL_VISUAL.with(|slot| {
        *slot.borrow_mut() = Some(ApprovalVisual {
            phase: if animate {
                ApprovalPhase::Opening
            } else {
                ApprovalPhase::Open
            },
            started_at: std::time::Instant::now(),
            dpi,
            anchor_center,
            anchor_top,
            compact_width,
            compact_height,
            expanded_width,
            expanded_height,
            current_width: if animate {
                compact_width
            } else {
                expanded_width
            },
            current_height: if animate {
                compact_height
            } else {
                expanded_height
            },
            current_alpha: if animate { 175 } else { 255 },
            animation_from_width: compact_width,
            animation_from_height: compact_height,
            animation_from_alpha: 175,
            hover: ApprovalHit::None,
            title_font: 0,
            body_font: 0,
            small_font: 0,
        })
    });
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let popup = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            wide("HeadroomRouteApprovalWindow").as_ptr(),
            wide("HeadroomRoute 确认").as_ptr(),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            if animate {
                compact_width
            } else {
                expanded_width
            },
            if animate {
                compact_height
            } else {
                expanded_height
            },
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if popup.is_null() {
        let id = APPROVAL_REQUEST.with(|slot| slot.borrow().as_ref().map(|request| request.id));
        if let Some(id) = id {
            approval::resolve(id, ApprovalChoice::Deny);
        }
        APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = None);
        APPROVAL_VISUAL.with(|slot| *slot.borrow_mut() = None);
        return;
    }
    APPROVAL_POPUP.with(|slot| slot.set(popup));
    unsafe {
        let initial_width = if animate {
            compact_width
        } else {
            expanded_width
        };
        let initial_height = if animate {
            compact_height
        } else {
            expanded_height
        };
        apply_approval_frame(
            popup,
            initial_width,
            initial_height,
            if animate { 175 } else { 255 },
        );
        if animate {
            SetTimer(popup, APPROVAL_ANIMATION_TIMER, 16, None);
        }
        ShowWindow(popup, SW_SHOWNOACTIVATE);
        InvalidateRect(popup, ptr::null(), 0);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_work_area() -> RECT {
    let foreground = GetForegroundWindow();
    let monitor = MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST);
    if !monitor.is_null() {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            rcMonitor: unsafe { std::mem::zeroed() },
            rcWork: unsafe { std::mem::zeroed() },
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            return info.rcWork;
        }
    }
    let mut work_area = RECT {
        left: 0,
        top: 0,
        right: GetSystemMetrics(SM_CXSCREEN),
        bottom: GetSystemMetrics(SM_CYSCREEN),
    };
    SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        &mut work_area as *mut RECT as *mut c_void,
        0,
    );
    work_area
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_dpi() -> u32 {
    let foreground = GetForegroundWindow();
    if !foreground.is_null() {
        let dpi = GetDpiForWindow(foreground);
        if dpi != 0 {
            return dpi;
        }
    }
    GetDpiForSystem().max(96)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_animation_enabled() -> bool {
    let mut enabled = 1i32;
    if SystemParametersInfoW(
        SPI_GETCLIENTAREAANIMATION,
        0,
        &mut enabled as *mut i32 as *mut c_void,
        0,
    ) == 0
    {
        true
    } else {
        enabled != 0
    }
}

fn approval_scale(value: i32, dpi: u32) -> i32 {
    value.saturating_mul(dpi as i32) / 96
}

fn approval_lerp(start: i32, end: i32, progress: f32) -> i32 {
    start + ((end - start) as f32 * progress).round() as i32
}

fn approval_ease(progress: f32) -> f32 {
    1.0 - (1.0 - progress).powi(3)
}

fn approval_rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn apply_approval_frame(hwnd: HWND, width: i32, height: i32, alpha: u8) {
    let Some((center, top, dpi)) = APPROVAL_VISUAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|visual| (visual.anchor_center, visual.anchor_top, visual.dpi))
    }) else {
        return;
    };
    let radius = height.min(approval_scale(24, dpi)).max(10);
    let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2);
    if !region.is_null() && SetWindowRgn(hwnd, region, 1) == 0 {
        DeleteObject(region as _);
    }
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        center - width / 2,
        top,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn begin_approval_close() {
    let popup = APPROVAL_POPUP.with(Cell::get);
    if popup.is_null() {
        return;
    }
    if !approval_animation_enabled() {
        DestroyWindow(popup);
        APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = None);
        unsafe { refresh_approval_popup() };
        return;
    }
    let should_start = APPROVAL_VISUAL.with(|slot| {
        let mut visual = slot.borrow_mut();
        let Some(visual) = visual.as_mut() else {
            return false;
        };
        if visual.phase == ApprovalPhase::Closing {
            return false;
        }
        visual.phase = ApprovalPhase::Closing;
        visual.started_at = std::time::Instant::now();
        visual.animation_from_width = visual.current_width;
        visual.animation_from_height = visual.current_height;
        visual.animation_from_alpha = visual.current_alpha;
        true
    });
    if should_start {
        SetTimer(popup, APPROVAL_ANIMATION_TIMER, 16, None);
        InvalidateRect(popup, ptr::null(), 0);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn advance_approval_animation(hwnd: HWND) {
    let frame = APPROVAL_VISUAL.with(|slot| {
        let mut visual = slot.borrow_mut();
        let visual = visual.as_mut()?;
        if visual.phase == ApprovalPhase::Open {
            return None;
        }
        let closing = visual.phase == ApprovalPhase::Closing;
        let duration = if closing {
            APPROVAL_CLOSE_MS
        } else {
            APPROVAL_OPEN_MS
        };
        let progress = (visual.started_at.elapsed().as_millis() as f32 / duration as f32).min(1.0);
        let eased = approval_ease(progress);
        let (start_width, start_height, start_alpha, end_width, end_height, end_alpha) = if closing
        {
            (
                visual.animation_from_width,
                visual.animation_from_height,
                visual.animation_from_alpha,
                visual.compact_width,
                visual.compact_height,
                0,
            )
        } else {
            (
                visual.compact_width,
                visual.compact_height,
                175,
                visual.expanded_width,
                visual.expanded_height,
                255,
            )
        };
        let width = approval_lerp(start_width, end_width, eased);
        let height = approval_lerp(start_height, end_height, eased);
        let alpha = approval_lerp(start_alpha as i32, end_alpha, eased).clamp(0, 255) as u8;
        visual.current_width = width;
        visual.current_height = height;
        visual.current_alpha = alpha;
        if progress >= 1.0 && !closing {
            visual.phase = ApprovalPhase::Open;
        }
        Some((width, height, alpha, closing, progress >= 1.0))
    });
    let Some((width, height, alpha, closing, finished)) = frame else {
        return;
    };
    apply_approval_frame(hwnd, width, height, alpha);
    InvalidateRect(hwnd, ptr::null(), 0);
    if finished {
        KillTimer(hwnd, APPROVAL_ANIMATION_TIMER);
        if closing {
            DestroyWindow(hwnd);
            APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = None);
            unsafe { refresh_approval_popup() };
        }
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn update_approval_countdown() {}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn hide_approval_popup() {
    let popup = APPROVAL_POPUP.with(|slot| {
        let popup = slot.get();
        slot.set(ptr::null_mut());
        popup
    });
    if !popup.is_null() {
        unsafe { DestroyWindow(popup) };
    } else {
        APPROVAL_VISUAL.with(|slot| *slot.borrow_mut() = None);
    }
    APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = None);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn resolve_approval_popup(hwnd: HWND, choice: ApprovalChoice) {
    let id = APPROVAL_REQUEST.with(|slot| slot.borrow().as_ref().map(|request| request.id));
    if let Some(id) = id {
        approval::resolve(id, choice);
    }
    unsafe { begin_approval_close() };
    let _ = hwnd;
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "system" fn approval_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().as_mut() {
                    visual.title_font = create_approval_font(16, FW_SEMIBOLD as i32, visual.dpi);
                    visual.body_font = create_approval_font(14, FW_NORMAL as i32, visual.dpi);
                    visual.small_font = create_approval_font(12, FW_NORMAL as i32, visual.dpi);
                }
            });
            0
        }
        WM_PAINT => {
            unsafe { paint_approval_popup(hwnd) };
            0
        }
        WM_TIMER if wparam == APPROVAL_ANIMATION_TIMER => {
            unsafe { advance_approval_animation(hwnd) };
            0
        }
        WM_MOUSEMOVE => {
            let hit = approval_hit_test(hwnd, lparam);
            let changed = APPROVAL_VISUAL.with(|slot| {
                let mut visual = slot.borrow_mut();
                let Some(visual) = visual.as_mut() else {
                    return false;
                };
                if visual.hover == hit {
                    false
                } else {
                    visual.hover = hit;
                    true
                }
            });
            if changed {
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut tracking);
            0
        }
        WM_MOUSELEAVE => {
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().as_mut() {
                    visual.hover = ApprovalHit::None;
                }
            });
            InvalidateRect(hwnd, ptr::null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let hit = approval_hit_test(hwnd, lparam);
            if matches!(
                hit,
                ApprovalHit::Allow | ApprovalHit::Rule | ApprovalHit::Deny
            ) {
                let choice = match hit {
                    ApprovalHit::Allow => ApprovalChoice::AllowOnce,
                    ApprovalHit::Rule => ApprovalChoice::AllowRule,
                    ApprovalHit::Deny => APPROVAL_REQUEST.with(|slot| {
                        if slot
                            .borrow()
                            .as_ref()
                            .is_some_and(|request| request.feedback)
                        {
                            ApprovalChoice::Feedback
                        } else {
                            ApprovalChoice::Deny
                        }
                    }),
                    ApprovalHit::None => ApprovalChoice::Deny,
                };
                unsafe { resolve_approval_popup(hwnd, choice) };
            }
            0
        }
        WM_CLOSE => {
            unsafe { resolve_approval_popup(hwnd, ApprovalChoice::Deny) };
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_SETCURSOR => {
            let hit = approval_hit_test_screen(hwnd);
            if matches!(
                hit,
                ApprovalHit::Allow | ApprovalHit::Rule | ApprovalHit::Deny
            ) {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_HAND));
            } else {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW));
            }
            1
        }
        WM_DESTROY => {
            KillTimer(hwnd, APPROVAL_ANIMATION_TIMER);
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().take() {
                    if visual.title_font != 0 {
                        DeleteObject(visual.title_font as _);
                    }
                    if visual.body_font != 0 {
                        DeleteObject(visual.body_font as _);
                    }
                    if visual.small_font != 0 {
                        DeleteObject(visual.small_font as _);
                    }
                }
            });
            APPROVAL_POPUP.with(|slot| {
                if slot.get() == hwnd {
                    slot.set(ptr::null_mut());
                }
            });
            0
        }
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn create_approval_font(height: i32, weight: i32, dpi: u32) -> usize {
    unsafe {
        CreateFontW(
            -approval_scale(height, dpi),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32 | FF_DONTCARE as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_hit_test(hwnd: HWND, lparam: LPARAM) -> ApprovalHit {
    let x = (lparam as i16) as i32;
    let y = ((lparam >> 16) as i16) as i32;
    approval_hit_test_point(hwnd, x, y)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_hit_test_screen(hwnd: HWND) -> ApprovalHit {
    let mut point = POINT::default();
    if GetCursorPos(&mut point) == 0 || ScreenToClient(hwnd, &mut point) == 0 {
        return ApprovalHit::None;
    }
    approval_hit_test_point(hwnd, point.x, point.y)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn approval_hit_test_point(hwnd: HWND, x: i32, y: i32) -> ApprovalHit {
    let Some((width, height, phase)) = APPROVAL_VISUAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|visual| (visual.current_width, visual.current_height, visual.phase))
    }) else {
        return ApprovalHit::None;
    };
    if phase != ApprovalPhase::Open || hwnd.is_null() {
        return ApprovalHit::None;
    }
    let dpi = APPROVAL_VISUAL.with(|slot| slot.borrow().as_ref().map_or(96, |visual| visual.dpi));
    let deny = approval_deny_rect(width, height, dpi);
    let allow = approval_allow_rect(width, height, dpi);
    let rule = approval_rule_rect(width, height, dpi);
    if point_in_rect(allow, x, y) {
        ApprovalHit::Allow
    } else if point_in_rect(rule, x, y)
        && APPROVAL_REQUEST.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|request| request.allow_rule)
        })
    {
        ApprovalHit::Rule
    } else if point_in_rect(deny, x, y) {
        ApprovalHit::Deny
    } else {
        ApprovalHit::None
    }
}

fn point_in_rect(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn approval_deny_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    let three = APPROVAL_REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|request| request.allow_rule)
    });
    RECT {
        left: if three { scale(18) } else { width - scale(218) },
        top: height - scale(56),
        right: if three {
            scale(160)
        } else {
            width - scale(122)
        },
        bottom: height - scale(18),
    }
}

fn approval_rule_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    RECT {
        left: scale(166),
        top: height - scale(56),
        right: width - scale(166),
        bottom: height - scale(18),
    }
}

fn approval_allow_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    let three = APPROVAL_REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|request| request.allow_rule)
    });
    RECT {
        left: if three {
            width - scale(160)
        } else {
            width - scale(112)
        },
        top: height - scale(56),
        right: width - scale(18),
        bottom: height - scale(18),
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn paint_approval_popup(hwnd: HWND) {
    let mut paint = std::mem::zeroed::<PAINTSTRUCT>();
    let dc = BeginPaint(hwnd, &mut paint);
    if dc.is_null() {
        return;
    }
    let mut client = std::mem::zeroed::<RECT>();
    GetClientRect(hwnd, &mut client);
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    if width <= 0 || height <= 0 {
        EndPaint(hwnd, &paint);
        return;
    }
    let memory = CreateCompatibleDC(dc);
    let bitmap = if !memory.is_null() {
        CreateCompatibleBitmap(dc, width, height)
    } else {
        ptr::null_mut()
    };
    if memory.is_null() || bitmap.is_null() {
        EndPaint(hwnd, &paint);
        if !memory.is_null() {
            DeleteDC(memory);
        }
        return;
    }
    let previous_bitmap = SelectObject(memory, bitmap as _);
    let background = CreateSolidBrush(approval_rgb(8, 11, 15));
    FillRect(memory, &client, background);
    DeleteObject(background as _);
    draw_approval_contents(memory, width, height);
    BitBlt(dc, 0, 0, width, height, memory, 0, 0, SRCCOPY);
    SelectObject(memory, previous_bitmap);
    DeleteObject(bitmap as _);
    DeleteDC(memory);
    EndPaint(hwnd, &paint);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_approval_contents(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    width: i32,
    height: i32,
) {
    let Some((request, visual)) = APPROVAL_REQUEST.with(|request_slot| {
        APPROVAL_VISUAL.with(|visual_slot| {
            Some((
                request_slot.borrow().clone()?,
                visual_slot.borrow().as_ref()?.clone_visual(),
            ))
        })
    }) else {
        return;
    };
    let scale = |value: i32| approval_scale(value, visual.dpi);
    let radius = height.min(scale(24)).max(scale(18));
    let border = if visual.hover != ApprovalHit::None {
        approval_rgb(55, 71, 85)
    } else {
        approval_rgb(35, 43, 53)
    };
    draw_round_box(
        dc,
        scale(1),
        scale(1),
        width - scale(1),
        height - scale(1),
        radius,
        approval_rgb(14, 18, 24),
        border,
    );
    let accent = if request.cli.eq_ignore_ascii_case("codex") {
        approval_rgb(90, 164, 255)
    } else {
        approval_rgb(255, 165, 87)
    };
    draw_circle(dc, scale(22), scale(22), scale(7), accent);
    let (position, total) = approval::request_position(request.id);
    let title = format!(
        "{} · 会话 {:04} · 请求 {}/{}",
        request.cli.to_uppercase(),
        request.pid % 10_000,
        position,
        total
    );
    draw_approval_text(
        dc,
        &title,
        RECT {
            left: scale(38),
            top: scale(12),
            right: width - scale(20),
            bottom: scale(36),
        },
        visual.title_font,
        approval_rgb(245, 247, 250),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
    if height < visual.expanded_height - scale(35) {
        return;
    }
    draw_approval_text(
        dc,
        "执行请求",
        RECT {
            left: scale(20),
            top: scale(58),
            right: width - scale(20),
            bottom: scale(78),
        },
        visual.small_font,
        approval_rgb(142, 153, 168),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
    let wrap_width = ((width - scale(40)) / scale(14).max(1)).clamp(28, 90) as usize;
    let action = wrap_approval_text(&request.action, wrap_width, 2);
    draw_approval_text(
        dc,
        &action,
        RECT {
            left: scale(20),
            top: scale(80),
            right: width - scale(20),
            bottom: scale(122),
        },
        visual.body_font,
        approval_rgb(241, 244, 248),
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
    );
    let cwd = format!("目录  {}", wrap_approval_text(&request.cwd, wrap_width, 1));
    draw_approval_text(
        dc,
        &cwd,
        RECT {
            left: scale(20),
            top: scale(132),
            right: width - scale(20),
            bottom: scale(153),
        },
        visual.small_font,
        approval_rgb(154, 164, 178),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    let summary = wrap_approval_text(&request.summary, wrap_width, 2);
    draw_approval_text(
        dc,
        &summary,
        RECT {
            left: scale(20),
            top: scale(162),
            right: width - scale(20),
            bottom: height - scale(72),
        },
        visual.small_font,
        approval_rgb(116, 127, 142),
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
    );
    let deny = approval_deny_rect(width, height, visual.dpi);
    let rule = approval_rule_rect(width, height, visual.dpi);
    let allow = approval_allow_rect(width, height, visual.dpi);
    let deny_color = if visual.hover == ApprovalHit::Deny {
        approval_rgb(112, 44, 52)
    } else {
        approval_rgb(60, 34, 41)
    };
    let allow_color = if visual.hover == ApprovalHit::Allow {
        approval_rgb(61, 151, 103)
    } else {
        approval_rgb(42, 116, 80)
    };
    let rule_color = if visual.hover == ApprovalHit::Rule {
        approval_rgb(55, 105, 170)
    } else {
        approval_rgb(37, 72, 118)
    };
    draw_round_box(
        dc,
        deny.left,
        deny.top,
        deny.right,
        deny.bottom,
        scale(12),
        deny_color,
        deny_color,
    );
    draw_round_box(
        dc,
        allow.left,
        allow.top,
        allow.right,
        allow.bottom,
        scale(12),
        allow_color,
        allow_color,
    );
    if request.allow_rule {
        draw_round_box(
            dc,
            rule.left,
            rule.top,
            rule.right,
            rule.bottom,
            scale(12),
            rule_color,
            rule_color,
        );
        draw_approval_text(
            dc,
            "允许此类命令",
            rule,
            visual.small_font,
            approval_rgb(232, 242, 255),
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    draw_approval_text(
        dc,
        if request.feedback {
            "拒绝并反馈"
        } else {
            "拒绝"
        },
        deny,
        visual.body_font,
        approval_rgb(255, 224, 226),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    draw_approval_text(
        dc,
        "仅允许一次",
        allow,
        visual.body_font,
        approval_rgb(235, 255, 244),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

#[derive(Clone, Copy)]
struct ApprovalVisualSnapshot {
    dpi: u32,
    expanded_height: i32,
    hover: ApprovalHit,
    title_font: usize,
    body_font: usize,
    small_font: usize,
}

impl ApprovalVisual {
    fn clone_visual(&self) -> ApprovalVisualSnapshot {
        ApprovalVisualSnapshot {
            dpi: self.dpi,
            expanded_height: self.expanded_height,
            hover: self.hover,
            title_font: self.title_font,
            body_font: self.body_font,
            small_font: self.small_font,
        }
    }
}

#[allow(clippy::too_many_arguments, unsafe_op_in_unsafe_fn)]
unsafe fn draw_round_box(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    fill: u32,
    border: u32,
) {
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, border);
    let old_brush = SelectObject(dc, brush as _);
    let old_pen = SelectObject(dc, pen as _);
    RoundRect(dc, left, top, right, bottom, radius * 2, radius * 2);
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(brush as _);
    DeleteObject(pen as _);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_circle(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    left: i32,
    top: i32,
    radius: i32,
    color: u32,
) {
    let brush = CreateSolidBrush(color);
    let old = SelectObject(dc, brush as _);
    Ellipse(dc, left - radius, top - radius, left + radius, top + radius);
    SelectObject(dc, old);
    DeleteObject(brush as _);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_approval_text(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    text: &str,
    mut rect: RECT,
    font: usize,
    color: u32,
    flags: u32,
) {
    let selected_font = if font != 0 {
        font
    } else {
        GetStockObject(DEFAULT_GUI_FONT) as usize
    };
    let old_font = SelectObject(dc, selected_font as _);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    DrawTextW(dc, wide(text).as_ptr(), -1, &mut rect, flags);
    SelectObject(dc, old_font);
}

fn wrap_approval_text(text: &str, width: usize, max_lines: usize) -> String {
    let limit = width.saturating_mul(max_lines);
    let characters = text.chars().collect::<Vec<_>>();
    let truncated = characters.len() > limit;
    let mut lines = Vec::new();
    let mut current = String::new();
    for character in characters.into_iter().take(limit) {
        if current.chars().count() >= width {
            lines.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        return "--".into();
    }
    if truncated {
        let last = lines.last_mut().unwrap();
        last.pop();
        last.push('…');
    }
    lines.join("\r\n")
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
fn mode_cn(direct: bool, bypass: bool) -> &'static str {
    if direct {
        "直连上游"
    } else if bypass {
        "旁路 Headroom"
    } else {
        "经过 Headroom"
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
        ID_RESTART, ID_SELECT_RUNTIME, approval_allow_rect, approval_deny_rect, approval_ease,
        approval_lerp, approval_scale, compact_number, failover_sources, hover_popup_size,
        move_target, recommended_action, route_hover_text, route_is_selected,
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
    fn wraps_long_api_keys_without_losing_characters() {
        let key = "a".repeat(130);
        let route = Route::new(
            Protocol::OpenAi,
            "provider".into(),
            "Provider".into(),
            "https://example.com/v1".into(),
            Some(key.clone()),
            AuthStyle::Bearer,
            "test",
        );
        let text = route_hover_text(&route, true);
        let displayed = text
            .lines()
            .skip(1)
            .collect::<String>()
            .strip_prefix("API Key：")
            .unwrap()
            .to_owned();
        assert_eq!(displayed, key);
        assert!(hover_popup_size(&text).1 > 48);
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
    fn approval_animation_uses_bounded_easing() {
        assert_eq!(approval_ease(0.0), 0.0);
        assert_eq!(approval_ease(1.0), 1.0);
        assert!(approval_ease(0.5) > 0.5);
        assert_eq!(approval_lerp(280, 520, 0.5), 400);
        assert_eq!(approval_scale(520, 144), 780);
    }

    #[test]
    fn approval_buttons_keep_dpi_scaled_margins() {
        let normal_allow = approval_allow_rect(520, 286, 96);
        let large_allow = approval_allow_rect(780, 429, 144);
        assert_eq!(normal_allow.right, 502);
        assert_eq!(large_allow.right, 753);
        assert_eq!(normal_allow.bottom, 268);
        assert_eq!(large_allow.bottom, 402);
        assert_eq!(
            approval_deny_rect(520, 286, 96).right,
            normal_allow.left - 10
        );
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
