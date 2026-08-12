#![allow(unsafe_op_in_unsafe_fn)]

use super::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use std::num::NonZeroIsize;
use windows_sys::Win32::UI::Controls::{
    ICC_TAB_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, NMHDR, TCIF_TEXT, TCITEMW,
    TCM_ADJUSTRECT, TCM_GETCURSEL, TCM_INSERTITEMW, TCN_SELCHANGE, TCS_FIXEDWIDTH, WC_TABCONTROL,
};

/// HWND wrapper so wry can attach a WebView to the existing shell window.
#[allow(dead_code)]
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
    /// Controls belonging to each tab (0..3). Tab control itself is always visible.
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
    let mut icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_TAB_CLASSES,
    };
    InitCommonControlsEx(&mut icc);

    let class_name = wide("HeadroomRouteMainWindow");
    let class = WNDCLASSW {
        lpfnWndProc: Some(main_window_proc),
        hInstance: instance,
        lpszClassName: class_name.as_ptr(),
        hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as _,
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
    // Created without WS_VISIBLE — stays off the taskbar until shown.
    Ok(hwnd)
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

#[allow(unsafe_op_in_unsafe_fn)]
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
            if code == BN_CLICKED || code == 0 {
                // Checkbox toggles and menu-equivalent buttons share command IDs.
                if matches!(
                    id as usize,
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
                ) {
                    handle_command_for_ui(hwnd, id as usize);
                    refresh_main_window(hwnd);
                    return 0;
                }
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

/// Route destroy/exit commands to the tray host; other commands keep the UI hwnd as parent.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn handle_command_for_ui(ui_hwnd: HWND, id: usize) {
    let host = tray_host_hwnd(ui_hwnd);
    let destroy_ids = [ID_EXIT, ID_RESTORE, ID_REPAIR_RUNTIME, ID_UNINSTALL];
    if destroy_ids.contains(&id) {
        handle_command(host, id);
    } else {
        handle_command(ui_hwnd, id);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn create_controls(hwnd: HWND, state: &mut MainWindowState) {
    let instance = GetModuleHandleW(ptr::null());
    let stock = GetStockObject(DEFAULT_GUI_FONT) as usize;
    state.body_font = CreateFontW(
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
    state.title_font = CreateFontW(
        -18,
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
    let font = if state.body_font == 0 {
        stock
    } else {
        state.body_font
    };
    let title_font = if state.title_font == 0 {
        font
    } else {
        state.title_font
    };

    let tab = CreateWindowExW(
        0,
        WC_TABCONTROL,
        wide("").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | TCS_FIXEDWIDTH | WS_TABSTOP,
        0,
        0,
        0,
        0,
        hwnd,
        ID_MAIN_TAB as _,
        instance,
        ptr::null(),
    );
    SendMessageW(tab, WM_SETFONT, font, 1);
    for (index, label) in ["状态", "上游", "运维", "设置"].into_iter().enumerate() {
        let mut text = wide(label);
        let mut item = TCITEMW {
            mask: TCIF_TEXT,
            pszText: text.as_mut_ptr(),
            ..std::mem::zeroed()
        };
        SendMessageW(
            tab,
            TCM_INSERTITEMW,
            index,
            &mut item as *mut TCITEMW as LPARAM,
        );
    }

    // --- Status page ---
    let status_body = precheck_report_edit(
        hwnd,
        0,
        0,
        0,
        0,
        ID_MAIN_STATUS_BODY as usize,
        WS_CHILD
            | WS_VISIBLE
            | WS_VSCROLL
            | ES_MULTILINE as u32
            | ES_READONLY as u32
            | ES_AUTOVSCROLL as u32
            | WS_TABSTOP,
        instance,
        font,
    );
    let recommended = editor_control(
        hwnd,
        "BUTTON",
        "建议操作",
        0,
        0,
        0,
        0,
        ID_MAIN_RECOMMENDED as usize,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    );
    state.page_controls[0].extend([status_body, recommended]);

    // --- Upstream page ---
    let route_hint = editor_control(
        hwnd,
        "STATIC",
        "双击列表项切换上游。勾选「接管上游」后，托盘与本页切换才会写入客户端配置。",
        0,
        0,
        0,
        0,
        ID_MAIN_ROUTE_HINT as usize,
        WS_CHILD | WS_VISIBLE,
        instance,
        font,
    );
    let manage = editor_control(
        hwnd,
        "BUTTON",
        "接管上游",
        0,
        0,
        0,
        0,
        ID_MANAGE_UPSTREAM,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let route_list = editor_control(
        hwnd,
        "LISTBOX",
        "",
        0,
        0,
        0,
        0,
        ID_MAIN_ROUTE_LIST as usize,
        WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY as u32 | WS_TABSTOP,
        instance,
        font,
    );
    state.page_controls[1].extend([route_hint, manage, route_list]);

    // --- Ops page ---
    let ops_hint = editor_control(
        hwnd,
        "STATIC",
        "常用运维开关与动作。进行中的同步/重启会暂时禁用对应按钮。",
        0,
        0,
        0,
        0,
        ID_MAIN_OPS_HINT as usize,
        WS_CHILD | WS_VISIBLE,
        instance,
        title_font,
    );
    let auto = editor_control(
        hwnd,
        "BUTTON",
        "自动故障切换",
        0,
        0,
        0,
        0,
        ID_AUTO,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let bypass = editor_control(
        hwnd,
        "BUTTON",
        "旁路 Headroom（保留路由）",
        0,
        0,
        0,
        0,
        ID_BYPASS,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let check = editor_control(
        hwnd,
        "BUTTON",
        "立即检查上游",
        0,
        0,
        0,
        0,
        ID_CHECK,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let sync = editor_control(
        hwnd,
        "BUTTON",
        "同步 Codex + Claude",
        0,
        0,
        0,
        0,
        ID_SYNC,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let restart = editor_control(
        hwnd,
        "BUTTON",
        "重启 Headroom",
        0,
        0,
        0,
        0,
        ID_RESTART,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let failover = editor_control(
        hwnd,
        "BUTTON",
        "配置故障转移策略...",
        0,
        0,
        0,
        0,
        ID_FAILOVER_EDITOR,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    );
    state.page_controls[2].extend([ops_hint, auto, bypass, check, sync, restart, failover]);

    // --- Settings page ---
    let settings_hint = editor_control(
        hwnd,
        "STATIC",
        "设置、诊断与维护。危险操作仍会弹出确认框。",
        0,
        0,
        0,
        0,
        ID_MAIN_SETTINGS_HINT as usize,
        WS_CHILD | WS_VISIBLE,
        instance,
        title_font,
    );
    let startup = editor_control(
        hwnd,
        "BUTTON",
        "随 Windows 启动",
        0,
        0,
        0,
        0,
        ID_STARTUP,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let auto_update = editor_control(
        hwnd,
        "BUTTON",
        "每日检查软件更新",
        0,
        0,
        0,
        0,
        ID_AUTO_UPDATE,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let show_key = editor_control(
        hwnd,
        "BUTTON",
        "悬浮显示上游 API Key",
        0,
        0,
        0,
        0,
        ID_SHOW_API_KEY,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    );
    let settings_buttons: &[(usize, &str)] = &[
        (ID_CONFIG, "打开 config.json（高级配置）"),
        (ID_LOGS, "打开数据与日志目录"),
        (ID_PRECHECK, "运行启动预检..."),
        (ID_DIAG, "复制脱敏诊断报告"),
        (ID_DIAGNOSTIC_ZIP, "创建脱敏诊断 ZIP..."),
        (ID_PROVIDER_IDS, "复制 Provider ID 清单"),
        (ID_RELOAD_FAILOVER, "重新加载故障转移规则"),
        (ID_RESET_METRICS, "清零 Headroom 统计..."),
        (ID_TAKEOVER, "预览并应用 CLI 接管..."),
        (ID_CREATE_BACKUP, "创建配置备份"),
        (ID_RESTORE_BACKUP, "恢复配置备份..."),
        (ID_EXPORT_PORTABLE, "导出可移植配置..."),
        (ID_IMPORT_PORTABLE, "导入可移植配置..."),
        (ID_UPDATE, "检查软件更新..."),
        (ID_REPAIR_RUNTIME, "重新检测 Headroom 环境..."),
        (ID_SELECT_RUNTIME, "选择 Headroom Python..."),
        (ID_RESTORE, "恢复 Codex / Claude 原始配置..."),
        (ID_UNINSTALL, "完全卸载并还原..."),
        (ID_APPROVAL_DEMO, "测试确认悬浮窗"),
    ];
    state.page_controls[3].extend([settings_hint, startup, auto_update, show_key]);
    for &(id, label) in settings_buttons {
        let button = editor_control(
            hwnd,
            "BUTTON",
            label,
            0,
            0,
            0,
            0,
            id,
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
            instance,
            font,
        );
        state.page_controls[3].push(button);
    }

    layout_controls(hwnd, state);
    apply_tab_visibility(hwnd, state);
    refresh_main_window(hwnd);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn layout_controls(hwnd: HWND, state: &MainWindowState) {
    let mut client = RECT::default();
    GetClientRect(hwnd, &mut client);
    let tab = GetDlgItem(hwnd, ID_MAIN_TAB);
    let margin = 12;
    let width = (client.right - client.left).max(100);
    let height = (client.bottom - client.top).max(100);
    SetWindowPos(tab, ptr::null_mut(), 0, 0, width, height, SWP_NOZORDER);

    let mut content = RECT {
        left: margin,
        top: margin,
        right: width - margin,
        bottom: height - margin,
    };
    SendMessageW(
        tab,
        TCM_ADJUSTRECT,
        0,
        &mut content as *mut RECT as LPARAM,
    );
    let cx = content.left;
    let cy = content.top;
    let cw = (content.right - content.left).max(40);
    let ch = (content.bottom - content.top).max(40);
    let row = 28;
    let gap = 8;

    // Status
    MoveWindow(
        GetDlgItem(hwnd, ID_MAIN_STATUS_BODY),
        cx,
        cy,
        cw,
        (ch - row - gap).max(40),
        1,
    );
    MoveWindow(
        GetDlgItem(hwnd, ID_MAIN_RECOMMENDED),
        cx,
        content.bottom - row,
        cw.min(280),
        row,
        1,
    );

    // Upstream
    MoveWindow(GetDlgItem(hwnd, ID_MAIN_ROUTE_HINT), cx, cy, cw, 36, 1);
    MoveWindow(
        GetDlgItem(hwnd, ID_MANAGE_UPSTREAM as i32),
        cx,
        cy + 40,
        160,
        row,
        1,
    );
    MoveWindow(
        GetDlgItem(hwnd, ID_MAIN_ROUTE_LIST),
        cx,
        cy + 40 + row + gap,
        cw,
        (ch - 40 - row - gap - 4).max(40),
        1,
    );

    // Ops
    let mut y = cy;
    MoveWindow(GetDlgItem(hwnd, ID_MAIN_OPS_HINT), cx, y, cw, row, 1);
    y += row + gap;
    for id in [ID_AUTO, ID_BYPASS] {
        MoveWindow(GetDlgItem(hwnd, id as i32), cx, y, cw.min(320), row, 1);
        y += row + gap;
    }
    for id in [ID_CHECK, ID_SYNC, ID_RESTART, ID_FAILOVER_EDITOR] {
        MoveWindow(GetDlgItem(hwnd, id as i32), cx, y, 220, row, 1);
        y += row + gap;
    }

    // Settings — two columns of buttons under checkboxes
    y = cy;
    MoveWindow(GetDlgItem(hwnd, ID_MAIN_SETTINGS_HINT), cx, y, cw, row, 1);
    y += row + gap;
    for id in [ID_STARTUP, ID_AUTO_UPDATE, ID_SHOW_API_KEY] {
        MoveWindow(GetDlgItem(hwnd, id as i32), cx, y, cw.min(280), row, 1);
        y += row + 4;
    }
    y += gap;
    let col_w = ((cw - gap) / 2).max(160);
    let mut col = 0;
    let start_y = y;
    let settings_ids = [
        ID_CONFIG,
        ID_LOGS,
        ID_PRECHECK,
        ID_DIAG,
        ID_DIAGNOSTIC_ZIP,
        ID_PROVIDER_IDS,
        ID_RELOAD_FAILOVER,
        ID_RESET_METRICS,
        ID_TAKEOVER,
        ID_CREATE_BACKUP,
        ID_RESTORE_BACKUP,
        ID_EXPORT_PORTABLE,
        ID_IMPORT_PORTABLE,
        ID_UPDATE,
        ID_REPAIR_RUNTIME,
        ID_SELECT_RUNTIME,
        ID_RESTORE,
        ID_UNINSTALL,
        ID_APPROVAL_DEMO,
    ];
    for id in settings_ids {
        let x = cx + col * (col_w + gap);
        MoveWindow(GetDlgItem(hwnd, id as i32), x, y, col_w, row, 1);
        col += 1;
        if col == 2 {
            col = 0;
            y += row + 4;
            if y + row > content.bottom {
                // keep packing; scroll not available — still usable on large screens
                let _ = start_y;
            }
        }
    }

    let _ = state;
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn apply_tab_visibility(hwnd: HWND, state: &MainWindowState) {
    for (index, controls) in state.page_controls.iter().enumerate() {
        let show = if index as i32 == state.tab {
            SW_SHOW
        } else {
            SW_HIDE
        };
        for control in controls {
            ShowWindow(*control, show);
        }
    }
    let _ = hwnd;
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn refresh_main_window(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return;
    }
    let snapshot = app.snapshot();
    let status_text = format!(
        "【状态中心】\r\n当前模式：{}\r\n原因：{}\r\nCodex：{}\r\nClaude：{}\r\nHeadroom：{}\r\n\r\n【当前路由】\r\nCodex：{} · {}\r\nClaude：{} · {}\r\n\r\n【服务状态】\r\n自动切换：{}\r\n配置同步：{}\r\n重启任务：{}\r\n接管上游：{}\r\n\r\n【Headroom 指标】\r\n统计范围：{}\r\n压缩 Token：{} → {}\r\n节省 Token：{}（{:.1}%）\r\n完成请求：{}\r\n失败请求：{}（{:.1}%）\r\n\r\n【最近活动】\r\n可用路由：{}\r\n最近切换：{}\r\n最近错误：{}\r\n\r\n【恢复建议】\r\n{}",
        snapshot.runtime_status.mode.label(),
        snapshot.runtime_status.reason,
        snapshot.runtime_status.codex.summary(),
        snapshot.runtime_status.claude.summary(),
        snapshot.runtime_status.headroom.summary(),
        snapshot.codex_availability,
        app.route_summary(Protocol::OpenAi),
        snapshot.claude_availability,
        app.route_summary(Protocol::Anthropic),
        if snapshot.auto_enabled {
            "已启用"
        } else {
            "未启用"
        },
        snapshot.sync_status,
        snapshot.restart_status,
        if snapshot.manage_upstream {
            "已接管"
        } else {
            "交还 CC-Switch"
        },
        snapshot.headroom_metrics_since.map_or_else(
            || "当前日志文件累计".into(),
            |since| format!("自 {} UTC", since.format("%Y-%m-%d %H:%M:%S")),
        ),
        compact_number(snapshot.headroom_metrics.input_tokens_original),
        compact_number(snapshot.headroom_metrics.input_tokens_optimized),
        compact_number(snapshot.headroom_metrics.tokens_saved),
        snapshot.headroom_metrics.compression_percent(),
        compact_number(snapshot.headroom_metrics.completed_requests),
        compact_number(snapshot.headroom_metrics.failed_requests),
        snapshot.headroom_metrics.failure_percent(),
        snapshot.routes.len(),
        snapshot.last_switch_reason.as_deref().unwrap_or("无"),
        snapshot.last_error.as_deref().unwrap_or("无"),
        app.recovery_hint()
    );
    SetWindowTextW(
        GetDlgItem(hwnd, ID_MAIN_STATUS_BODY),
        wide(&status_text).as_ptr(),
    );

    if let Some((command, label)) = recommended_action(
        &snapshot.runtime_status,
        &snapshot.headroom_state,
        snapshot.last_error.as_deref(),
    ) {
        (*state_ptr).recommended_command = command;
        let button = GetDlgItem(hwnd, ID_MAIN_RECOMMENDED);
        SetWindowTextW(button, wide(label).as_ptr());
        EnableWindow(button, 1);
    } else {
        (*state_ptr).recommended_command = 0;
        let button = GetDlgItem(hwnd, ID_MAIN_RECOMMENDED);
        SetWindowTextW(button, wide("暂无建议操作").as_ptr());
        EnableWindow(button, 0);
    }

    set_check(hwnd, ID_MANAGE_UPSTREAM, snapshot.manage_upstream);
    set_check(hwnd, ID_AUTO, snapshot.auto_enabled);
    set_check(hwnd, ID_BYPASS, snapshot.bypass_headroom);
    set_check(
        hwnd,
        ID_STARTUP,
        app.inner.lock().unwrap().config.start_with_windows,
    );
    set_check(hwnd, ID_AUTO_UPDATE, snapshot.auto_update_check);
    set_check(hwnd, ID_SHOW_API_KEY, snapshot.show_api_key_on_hover);

    EnableWindow(
        GetDlgItem(hwnd, ID_SYNC as i32),
        if app.sync_in_progress.load(Ordering::Acquire) {
            0
        } else {
            1
        },
    );
    EnableWindow(
        GetDlgItem(hwnd, ID_RESTART as i32),
        if app.restart_in_progress.load(Ordering::Acquire) {
            0
        } else {
            1
        },
    );
    EnableWindow(
        GetDlgItem(hwnd, ID_UPDATE as i32),
        if updater::is_running() { 0 } else { 1 },
    );

    let list = GetDlgItem(hwnd, ID_MAIN_ROUTE_LIST);
    let previous = SendMessageW(list, LB_GETCURSEL, 0, 0);
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    for (index, route) in snapshot.routes.iter().enumerate() {
        let selected = match route.protocol {
            Protocol::OpenAi => route_is_selected(route, snapshot.active_provider.as_deref()),
            Protocol::Anthropic => {
                route_is_selected(route, snapshot.active_anthropic_provider.as_deref())
            }
        };
        let mark = if selected { "● " } else { "○ " };
        let line = format!(
            "{mark}[{}] {}  ·  {}  ·  {} ms",
            match route.protocol {
                Protocol::OpenAi => "Codex",
                Protocol::Anthropic => "Claude",
            },
            route.name,
            route.evidence_label(),
            latency_text(route.latency_ms)
        );
        let row = SendMessageW(list, LB_ADDSTRING, 0, wide(&line).as_ptr() as LPARAM);
        if row >= 0 {
            SendMessageW(list, LB_SETITEMDATA, row as usize, index as LPARAM);
        }
    }
    if previous >= 0 {
        SendMessageW(list, LB_SETCURSEL, previous as usize, 0);
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn set_check(hwnd: HWND, id: usize, checked: bool) {
    SendMessageW(
        GetDlgItem(hwnd, id as i32),
        BM_SETCHECK,
        if checked {
            BST_CHECKED as usize
        } else {
            BST_UNCHECKED as usize
        },
        0,
    );
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn activate_selected_route(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let list = GetDlgItem(hwnd, ID_MAIN_ROUTE_LIST);
    let row = SendMessageW(list, LB_GETCURSEL, 0, 0);
    if row < 0 {
        return;
    }
    let index = SendMessageW(list, LB_GETITEMDATA, row as usize, 0);
    if index < 0 {
        return;
    }
    if app.switch_index(index as usize, "主窗口手动切换") {
        let _ = app.write_status();
        refresh_main_window(hwnd);
    }
}
