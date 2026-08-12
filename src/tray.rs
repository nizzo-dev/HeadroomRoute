#![cfg(windows)]

mod approval_dialog;
mod approval_layout;
mod commands;
#[path = "tray/failover_editor.rs"]
mod failover_editor;
use failover_editor::*;
mod icon_bitmap;
mod main_window;
mod menu;
mod portability_actions;
mod precheck_dialog;
mod precheck_layout;
#[cfg(test)]
mod tests;
mod tray_window;

use approval_dialog::*;
use commands::*;
use main_window::{
    create_main_window, destroy_main_window, dialog_owner, refresh_main_window_if_visible,
    register_main_window_class, set_tray_host_hwnd, show_main_window, tray_host_hwnd,
};
use menu::*;
use portability_actions::*;
#[cfg(test)]
use precheck_dialog::{
    precheck_action_command, precheck_action_compact_label, precheck_action_label,
};
use precheck_dialog::{precheck_window_proc, show_precheck};
#[cfg(test)]
use precheck_layout::{precheck_layout, precheck_scale};
use tray_window::{
    add_icon, destroy_route_url, hide_route_url, remove_icon, show_hovered_route_url, update_icon,
};
mod route_text;

#[cfg(test)]
use self::route_text::compact_number;
use self::route_text::{hover_popup_size, latency_text, route_hover_text};
use crate::config::portability::{
    BackupDescriptor, TakeoverPlan, apply_takeover_plan, create_config_backup,
    create_diagnostic_bundle, decode_portable_config, export_portable_config,
    import_portable_config, list_config_backups, prepare_takeover, restore_config_backup,
};
use crate::{
    approval::{self, ApprovalChoice, ApprovalRequest, PopupKind},
    config,
    model::{ComponentState, FailoverPolicy, Protocol, Route, RuntimeStatus, Snapshot},
    notification, precheck, runtime,
    state::AppState,
    updater,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    fs,
    mem::size_of,
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
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
        SetWindowRgn, TRANSPARENT, UpdateWindow,
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
            NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIN_SELECT, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::GUID;

const WM_TRAY: u32 = WM_APP + 1;
/// Keyboard activation of the tray icon (NIN_SELECT | 1). Not always exported by windows-sys.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
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
const ID_MANAGE_UPSTREAM: usize = 123;
#[allow(dead_code)]
const ID_DIRECT_CLAUDE: usize = 124; // reserved (legacy dual-direct removed)
const ID_PRECHECK: usize = 125;
const ID_TAKEOVER: usize = 126;
const ID_CREATE_BACKUP: usize = 127;
const ID_RESTORE_BACKUP: usize = 128;
const ID_EXPORT_PORTABLE: usize = 129;
const ID_IMPORT_PORTABLE: usize = 130;
const ID_DIAGNOSTIC_ZIP: usize = 131;
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
/// 预检窗口布局基线（96 DPI 客户区像素）。所有尺寸、间距与字体高度按 DPI 缩放。
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

pub fn run(app: Arc<AppState>, auto_open_precheck: bool) -> anyhow::Result<()> {
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
        let precheck_class_name = wide("HeadroomRoutePrecheck");
        let precheck_class = WNDCLASSW {
            lpfnWndProc: Some(precheck_window_proc),
            hInstance: instance,
            lpszClassName: precheck_class_name.as_ptr(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            ..std::mem::zeroed()
        };
        if RegisterClassW(&precheck_class) == 0 {
            anyhow::bail!("无法注册启动预检窗口");
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
        register_main_window_class(instance)?;
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
        set_tray_host_hwnd(hwnd);
        approval::set_tray_hwnd(hwnd);
        create_main_window(hwnd)?;
        add_icon(hwnd);
        SetTimer(hwnd, 1, 500, None);
        if auto_open_precheck {
            show_precheck(dialog_owner(hwnd));
        }
        if std::env::args().any(|arg| arg == "--approval-demo") {
            let _ = approval::enqueue_demo();
        }
        if std::env::args().any(|arg| arg == "--notification-demo") {
            let _ = approval::enqueue_notice_demo();
        }
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        hide_approval_popup();
        destroy_main_window();
        approval::clear_tray_hwnd(hwnd);
        remove_icon(hwnd);
        set_tray_host_hwnd(ptr::null_mut());
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
        WM_TRAY
            if lparam as u32 == WM_LBUTTONUP
                || lparam as u32 == WM_LBUTTONDBLCLK
                || lparam as u32 == NIN_SELECT
                || lparam as u32 == NIN_KEYSELECT =>
        {
            unsafe { show_main_window() };
            0
        }
        WM_POWERBROADCAST
            if matches!(
                wparam as u32,
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL
            ) =>
        {
            if let Some(app) = APP.get() {
                crate::worker::trigger_environment_event(
                    app,
                    crate::environment_recovery::EnvironmentEvent::Resume,
                );
            }
            1
        }
        WM_SETTINGCHANGE | WM_DEVICECHANGE => {
            if let Some(app) = APP.get() {
                crate::worker::trigger_environment_event(
                    app,
                    crate::environment_recovery::EnvironmentEvent::NetworkOrProxyChanged,
                );
            }
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
            unsafe { refresh_main_window_if_visible() };
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
                if let Some(message) = app.take_operation_notice() {
                    notify(hwnd, "Provider 操作已记录", &message);
                }
                if let Some(message) = app.take_recovery_notice() {
                    notify(hwnd, "环境恢复状态更新", &message);
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
            unsafe { destroy_main_window() };
            approval::clear_tray_hwnd(hwnd);
            set_tray_host_hwnd(ptr::null_mut());
            if let Some(app) = APP.get() {
                app.stop.store(true, Ordering::Relaxed);
            }
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[allow(dead_code)]
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

pub(super) fn route_is_selected(route: &Route, active_provider: Option<&str>) -> bool {
    active_provider == Some(route.provider.as_str())
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

fn make_icon(_health: &str) -> *mut c_void {
    // Prefer branded logo; fall back to procedural glyph if ICO decode fails.
    let branded = crate::branding::tray_icon();
    if !branded.is_null() {
        return branded;
    }
    let color = match _health {
        "healthy" => 0x00_3c_b3_71u32,
        "degraded" => 0x00_00_a5_ff,
        "unavailable" => 0x00_43_43_dc,
        _ => 0x00_80_80_80,
    };
    let (and_mask, xor) = icon_bitmap::render(color);
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
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
