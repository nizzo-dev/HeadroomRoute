#![allow(unsafe_op_in_unsafe_fn)]

use super::*;
use windows_sys::Win32::UI::Controls::{
    ICC_TAB_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, NMHDR, TCM_GETCURSEL,
    TCN_SELCHANGE,
};

mod native_pages;
use native_pages::{
    activate_selected_route, apply_tab_visibility, create_controls, layout_controls,
    refresh_main_window,
};

const ID_MAIN_TAB: i32 = 300;
const ID_MAIN_STATUS_BODY: i32 = 301;
const ID_MAIN_RECOMMENDED: i32 = 302;
const ID_MAIN_ROUTE_LIST: i32 = 310;
const ID_MAIN_ROUTE_HINT: i32 = 311;
const ID_MAIN_OPS_HINT: i32 = 320;
const ID_MAIN_SETTINGS_HINT: i32 = 330;

const MAIN_CLIENT_WIDTH: i32 = 760;
const MAIN_CLIENT_HEIGHT: i32 = 560;
const MAIN_REFRESH_TIMER: usize = 2;

thread_local! {
    static MAIN_HWND: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
    static TRAY_HOST_HWND: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
}

struct MainWindowState {
    body_font: usize,
    title_font: usize,
    tab: i32,
    page_controls: [Vec<HWND>; 4],
    recommended_command: usize,
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
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_TAB_CLASSES,
    };
    InitCommonControlsEx(&icc);
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
        body_font: 0,
        title_font: 0,
        tab: 0,
        page_controls: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        recommended_command: 0,
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
    apply_window_icons(hwnd);
    if crate::edition::show_window_on_start() {
        show_main_window();
    }
    Ok(hwnd)
}

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
    if IsIconic(hwnd) != 0 {
        ShowWindow(hwnd, SW_RESTORE);
    } else {
        ShowWindow(hwnd, SW_SHOW);
    }
    SetForegroundWindow(hwnd);
    refresh_main_window(hwnd);
}

#[allow(dead_code)]
pub(super) unsafe fn hide_main_window() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        ShowWindow(hwnd, SW_HIDE);
    }
}

pub(super) unsafe fn destroy_main_window() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        DestroyWindow(hwnd);
    }
    MAIN_HWND.with(|slot| slot.set(ptr::null_mut()));
}

pub(super) unsafe fn refresh_main_window_if_visible() {
    let hwnd = main_hwnd();
    if !hwnd.is_null() && IsWindow(hwnd) != 0 && IsWindowVisible(hwnd) != 0 {
        refresh_main_window(hwnd);
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
            create_controls(hwnd, &mut *state_ptr);
            SetTimer(hwnd, MAIN_REFRESH_TIMER, 1000, None);
            0
        }
        WM_SIZE => {
            layout_controls(hwnd, &*state_ptr);
            0
        }
        WM_DPICHANGED => {
            apply_window_icons(hwnd);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_TIMER if wparam == MAIN_REFRESH_TIMER => {
            if IsWindowVisible(hwnd) != 0 {
                refresh_main_window(hwnd);
            }
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lparam as *const NMHDR);
            if hdr.idFrom == ID_MAIN_TAB as usize && hdr.code == TCN_SELCHANGE {
                let tab = SendMessageW(GetDlgItem(hwnd, ID_MAIN_TAB), TCM_GETCURSEL, 0, 0) as i32;
                (*state_ptr).tab = tab.max(0);
                apply_tab_visibility(hwnd, &*state_ptr);
                refresh_main_window(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xffff) as i32;
            let code = ((wparam >> 16) & 0xffff) as u32;
            if id == ID_MAIN_RECOMMENDED && code == BN_CLICKED {
                let command = (*state_ptr).recommended_command;
                if command != 0 {
                    handle_command_for_ui(hwnd, command);
                }
                return 0;
            }
            if id == ID_MAIN_ROUTE_LIST && code == LBN_DBLCLK {
                activate_selected_route(hwnd);
                return 0;
            }
            if (code == BN_CLICKED || code == 0) && is_main_command(id as usize) {
                handle_command_for_ui(hwnd, id as usize);
                refresh_main_window(hwnd);
                return 0;
            }
            0
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, MAIN_REFRESH_TIMER);
            if (*state_ptr).body_font != 0 {
                DeleteObject((*state_ptr).body_font as _);
            }
            if (*state_ptr).title_font != 0 {
                DeleteObject((*state_ptr).title_font as _);
            }
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

fn is_main_command(id: usize) -> bool {
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
    )
}

unsafe fn handle_command_for_ui(ui_hwnd: HWND, id: usize) {
    let host = tray_host_hwnd(ui_hwnd);
    if [ID_EXIT, ID_RESTORE, ID_REPAIR_RUNTIME, ID_UNINSTALL].contains(&id) {
        handle_command(host, id);
    } else {
        handle_command(ui_hwnd, id);
    }
}
