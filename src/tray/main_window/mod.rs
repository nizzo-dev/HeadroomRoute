#![allow(unsafe_op_in_unsafe_fn)]

mod theme;
mod ui_model;

use theme::{HostTheme, apply_host_theme, parse_host_theme, refresh_system_theme};

#[allow(unused_imports)]
pub(super) use ui_model::{UiInbound, UiSnapshot, build_ui_snapshot, parse_ui_message};

use super::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use std::num::NonZeroIsize;
use wry::Rect;
use wry::WebViewBuilder;
use wry::dpi::{LogicalPosition, LogicalSize, Position, Size};

/// HWND wrapper so wry can attach a WebView to the existing shell window.
pub(super) struct ShellWindow(pub HWND);

impl HasWindowHandle for ShellWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?;
        let handle = Win32WindowHandle::new(hwnd);
        // SAFETY: handle is only borrowed while the shell HWND is alive on the UI thread.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}

impl HasDisplayHandle for ShellWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let handle = WindowsDisplayHandle::new();
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Windows(handle)) })
    }
}

const MAIN_CLIENT_WIDTH: i32 = 760;
const MAIN_CLIENT_HEIGHT: i32 = 560;
const MAIN_REFRESH_TIMER: usize = 2;
/// Custom message: lparam owns `Box<String>` IPC body from the WebView thread.
const WM_UI_IPC: u32 = WM_APP + 40;

const UI_HTML: &str = include_str!("../../../ui/main/app.html");

thread_local! {
    static MAIN_HWND: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
    static TRAY_HOST_HWND: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
}

struct MainWindowState {
    webview: Option<wry::WebView>,
    /// Last theme choice reported by the console; `None` means "system"
    /// until the WebView reports in.
    theme: Option<HostTheme>,
}

pub(super) fn set_tray_host_hwnd(hwnd: HWND) {
    TRAY_HOST_HWND.with(|slot| slot.set(hwnd));
}

pub(super) fn tray_host_hwnd(fallback: HWND) -> HWND {
    TRAY_HOST_HWND.with(|slot| {
        let host = slot.get();
        if host.is_null() { fallback } else { host }
    })
}

pub(super) fn main_hwnd() -> HWND {
    MAIN_HWND.with(|slot| slot.get())
}

pub(super) fn dialog_owner(fallback: HWND) -> HWND {
    let main = main_hwnd();
    if !main.is_null() && unsafe { IsWindow(main) != 0 && IsWindowVisible(main) != 0 } {
        main
    } else {
        fallback
    }
}

pub(super) unsafe fn register_main_window_class(
    instance: windows_sys::Win32::Foundation::HINSTANCE,
) -> anyhow::Result<()> {
    let class_name = wide("HeadroomRouteMainWindow");
    let class = WNDCLASSW {
        lpfnWndProc: Some(main_window_proc),
        hInstance: instance,
        lpszClassName: class_name.as_ptr(),
        hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as _,
        hIcon: crate::branding::window_icon_big(),
        style: CS_HREDRAW | CS_VREDRAW,
        ..std::mem::zeroed()
    };
    if RegisterClassW(&class) == 0 {
        anyhow::bail!("无法注册主控制台窗口");
    }
    Ok(())
}

pub(super) unsafe fn create_main_window(tray_hwnd: HWND) -> anyhow::Result<HWND> {
    let instance = GetModuleHandleW(ptr::null());
    let class_name = wide("HeadroomRouteMainWindow");
    let title = wide("Headroom Route");
    let style = WS_OVERLAPPEDWINDOW;
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: MAIN_CLIENT_WIDTH,
        bottom: MAIN_CLIENT_HEIGHT,
    };
    AdjustWindowRectEx(&mut bounds, style, 0, 0);
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let _ = tray_hwnd;
    let state = Box::new(MainWindowState {
        webview: None,
        theme: None,
    });
    let raw = Box::into_raw(state);
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        title.as_ptr(),
        style,
        (GetSystemMetrics(SM_CXSCREEN) - width) / 2,
        (GetSystemMetrics(SM_CYSCREEN) - height) / 2,
        width,
        height,
        ptr::null_mut(),
        ptr::null_mut(),
        instance,
        raw.cast(),
    );
    if hwnd.is_null() {
        drop(Box::from_raw(raw));
        anyhow::bail!("无法创建主控制台窗口");
    }
    MAIN_HWND.with(|slot| slot.set(hwnd));
    // Follow the system frame theme until the console reports its choice.
    apply_host_theme(hwnd, HostTheme::System);
    apply_window_icons(hwnd);
    if crate::edition::show_window_on_start() {
        show_main_window();
    }
    Ok(hwnd)
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn apply_window_icons(hwnd: HWND) {
    let dpi = GetDpiForWindow(hwnd).max(96);
    let small = crate::branding::window_icon_small_for_dpi(dpi);
    if !small.is_null() {
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as LPARAM);
    }
    let big = crate::branding::window_icon_big_for_dpi(dpi);
    if !big.is_null() {
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as LPARAM);
    }
}

pub(super) unsafe fn show_main_window() {
    let hwnd = main_hwnd();
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return;
    }
    ensure_webview(hwnd);
    if IsIconic(hwnd) != 0 {
        ShowWindow(hwnd, SW_RESTORE);
    } else {
        ShowWindow(hwnd, SW_SHOW);
    }
    SetForegroundWindow(hwnd);
    push_snapshot(hwnd);
}

#[allow(dead_code)]
pub(super) unsafe fn hide_main_window() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        teardown_webview(hwnd);
        ShowWindow(hwnd, SW_HIDE);
    }
}

pub(super) unsafe fn destroy_main_window() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        teardown_webview(hwnd);
        DestroyWindow(hwnd);
    }
    MAIN_HWND.with(|slot| slot.set(ptr::null_mut()));
}

pub(super) unsafe fn refresh_main_window_if_visible() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0 {
        push_snapshot(hwnd);
    }
}

unsafe extern "system" fn main_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    match message {
        WM_CREATE => {
            SetTimer(hwnd, MAIN_REFRESH_TIMER, 1000, None);
            0
        }
        WM_SIZE => {
            resize_webview(hwnd, &*state_ptr);
            0
        }
        WM_DPICHANGED => {
            apply_window_icons(hwnd);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_TIMER if wparam == MAIN_REFRESH_TIMER => {
            if IsWindowVisible(hwnd) != 0 {
                push_snapshot(hwnd);
            }
            0
        }
        WM_UI_IPC => {
            if lparam != 0 {
                let body = *Box::from_raw(lparam as *mut String);
                dispatch_ui_message(hwnd, &body);
            }
            0
        }
        WM_SETTINGCHANGE => {
            // Windows reports app color-scheme changes this way; re-apply the
            // system-following frame theme when the console is following the
            // system (or has not yet reported a choice).
            let state = &*state_ptr;
            if state.theme != Some(HostTheme::Dark) && state.theme != Some(HostTheme::Light) {
                refresh_system_theme(hwnd, lparam as *const u16);
            }
            0
        }
        WM_CLOSE => {
            teardown_webview(hwnd);
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, MAIN_REFRESH_TIMER);
            teardown_webview(hwnd);
            MAIN_HWND.with(|slot| {
                if slot.get() == hwnd {
                    slot.set(ptr::null_mut());
                }
            });
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state_ptr));
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn post_ui_ipc(body: String) {
    let hwnd = main_hwnd();
    if hwnd.is_null() {
        return;
    }
    let raw = Box::into_raw(Box::new(body));
    let ok = unsafe { PostMessageW(hwnd, WM_UI_IPC, 0, raw as LPARAM) };
    if ok == 0 {
        // Drop the box if post failed so we don't leak.
        drop(unsafe { Box::from_raw(raw) });
    }
}

fn is_allowed_ui_command(id: usize) -> bool {
    matches!(
        id,
        ID_CHECK
            | ID_SYNC
            | ID_RESTART
            | ID_AUTO
            | ID_BYPASS
            | ID_MANAGE_UPSTREAM
            | ID_FAILOVER_EDITOR
            | ID_STARTUP
            | ID_AUTO_UPDATE
            | ID_SHOW_API_KEY
            | ID_CONFIG
            | ID_LOGS
            | ID_DIAG
            | ID_PRECHECK
            | ID_RESET_METRICS
            | ID_TAKEOVER
            | ID_CREATE_BACKUP
            | ID_RESTORE_BACKUP
            | ID_EXPORT_PORTABLE
            | ID_IMPORT_PORTABLE
            | ID_DIAGNOSTIC_ZIP
            | ID_PROVIDER_IDS
            | ID_RELOAD_FAILOVER
            | ID_UPDATE
            | ID_REPAIR_RUNTIME
            | ID_SELECT_RUNTIME
            | ID_RESTORE
            | ID_UNINSTALL
            | ID_APPROVAL_DEMO
            | ID_OPEN_STATUS
            | ID_EXIT
    )
}

unsafe fn handle_command_for_ui(ui_hwnd: HWND, id: usize) {
    let host = tray_host_hwnd(ui_hwnd);
    let destroy_ids = [ID_EXIT, ID_RESTORE, ID_REPAIR_RUNTIME, ID_UNINSTALL];
    if destroy_ids.contains(&id) {
        handle_command(host, id);
    } else {
        handle_command(ui_hwnd, id);
    }
}

fn dispatch_ui_message(hwnd: HWND, body: &str) {
    match parse_ui_message(body) {
        Some(UiInbound::Ready) => unsafe { push_snapshot(main_hwnd()) },
        Some(UiInbound::Theme { mode }) => {
            let Some(theme) = parse_host_theme(&mode) else {
                return;
            };
            unsafe {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
                if !state_ptr.is_null() {
                    (*state_ptr).theme = Some(theme);
                    apply_host_theme(hwnd, theme);
                }
            }
        }
        Some(UiInbound::Command { id }) => {
            if !is_allowed_ui_command(id) {
                return;
            }
            unsafe {
                handle_command_for_ui(main_hwnd(), id);
                push_snapshot(main_hwnd());
            }
        }
        Some(UiInbound::SwitchRoute { index }) => {
            if let Some(app) = APP.get()
                && app.switch_index(index, "主窗口手动切换")
            {
                let _ = app.write_status();
            }
            unsafe { push_snapshot(main_hwnd()) };
        }
        None => {}
    }
}

unsafe fn ensure_webview(hwnd: HWND) {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return;
    }
    if (*state_ptr).webview.is_some() {
        resize_webview(hwnd, &*state_ptr);
        return;
    }

    let mut client = RECT::default();
    GetClientRect(hwnd, &mut client);
    let width = (client.right - client.left).max(1) as f64;
    let height = (client.bottom - client.top).max(1) as f64;
    let shell = ShellWindow(hwnd);
    let builder = WebViewBuilder::new()
        .with_html(UI_HTML)
        .with_bounds(Rect {
            position: Position::Logical(LogicalPosition::new(0.0, 0.0)),
            size: Size::Logical(LogicalSize::new(width, height)),
        })
        .with_ipc_handler(|req| {
            let body = req.body().clone();
            post_ui_ipc(body);
        });

    match builder.build(&shell) {
        Ok(webview) => {
            (*state_ptr).webview = Some(webview);
        }
        Err(error) => {
            notification::error(
                "无法打开主窗口",
                format!("需要已安装的 Microsoft Edge WebView2 Runtime（Evergreen）。\n{error}"),
            );
        }
    }
}

unsafe fn teardown_webview(hwnd: HWND) {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return;
    }
    (*state_ptr).webview.take();
}

unsafe fn resize_webview(hwnd: HWND, state: &MainWindowState) {
    let Some(webview) = state.webview.as_ref() else {
        return;
    };
    let mut client = RECT::default();
    GetClientRect(hwnd, &mut client);
    let width = (client.right - client.left).max(1) as f64;
    let height = (client.bottom - client.top).max(1) as f64;
    let _ = webview.set_bounds(Rect {
        position: Position::Logical(LogicalPosition::new(0.0, 0.0)),
        size: Size::Logical(LogicalSize::new(width, height)),
    });
}

unsafe fn push_snapshot(hwnd: HWND) {
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return;
    }
    let Some(app) = APP.get() else {
        return;
    };
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return;
    }
    let Some(webview) = (*state_ptr).webview.as_ref() else {
        return;
    };
    let snap = build_ui_snapshot(app);
    let Ok(json) = serde_json::to_string(&snap) else {
        return;
    };
    // json is a JS object literal when inserted raw.
    let script = format!("window.__hr && window.__hr.applySnapshot({json});");
    let _ = webview.evaluate_script(&script);
}
