#![allow(unsafe_op_in_unsafe_fn)]

use super::super::*;
use super::{
    ID_MAIN_OPS_HINT, ID_MAIN_RECOMMENDED, ID_MAIN_ROUTE_HINT, ID_MAIN_ROUTE_LIST,
    ID_MAIN_SETTINGS_HINT, ID_MAIN_STATUS_BODY, ID_MAIN_TAB, MainWindowState,
};
use windows_sys::Win32::UI::Controls::{
    TCIF_TEXT, TCITEMW, TCM_ADJUSTRECT, TCM_INSERTITEMW, TCS_FIXEDWIDTH, WC_TABCONTROL,
};

fn make_font(height: i32, weight: i32) -> usize {
    unsafe {
        CreateFontW(
            -height,
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
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize
    }
}

pub(super) unsafe fn create_controls(hwnd: HWND, state: &mut MainWindowState) {
    let instance = GetModuleHandleW(ptr::null());
    let stock = GetStockObject(DEFAULT_GUI_FONT) as usize;
    state.body_font = make_font(15, FW_NORMAL as i32);
    state.title_font = make_font(18, FW_SEMIBOLD as i32);
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
    let recommended = button(
        hwnd,
        "建议操作",
        ID_MAIN_RECOMMENDED as usize,
        instance,
        font,
    );
    state.page_controls[0].extend([status_body, recommended]);

    let route_hint = static_text(
        hwnd,
        "双击列表项切换上游。勾选对应协议的「接管配置」后才会写入客户端。",
        ID_MAIN_ROUTE_HINT as usize,
        instance,
        font,
    );
    let manage_codex = checkbox(hwnd, "接管 Codex", ID_MANAGE_UPSTREAM, instance, font);
    let manage_claude = checkbox(hwnd, "接管 Claude", ID_MANAGE_CLAUDE, instance, font);
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
    state.page_controls[1].extend([route_hint, manage_codex, manage_claude, route_list]);

    let ops_hint = static_text(
        hwnd,
        "常用运维开关与动作。进行中的同步/重启会暂时禁用对应按钮。",
        ID_MAIN_OPS_HINT as usize,
        instance,
        title_font,
    );
    state.page_controls[2].push(ops_hint);
    for (id, label, check) in [
        (ID_AUTO, "自动故障切换", true),
        (ID_BYPASS, "旁路 Headroom（保留路由）", true),
        (ID_CHECK, "立即检查上游", false),
        (ID_SYNC, "同步 Codex + Claude", false),
        (ID_RESTART, "重启 Headroom", false),
        (ID_FAILOVER_EDITOR, "配置故障转移策略...", false),
    ] {
        let control = if check {
            checkbox(hwnd, label, id, instance, font)
        } else {
            button(hwnd, label, id, instance, font)
        };
        state.page_controls[2].push(control);
    }

    let settings_hint = static_text(
        hwnd,
        "日常选项在上：更新验证、环境、备份、诊断，危险操作在最下。",
        ID_MAIN_SETTINGS_HINT as usize,
        instance,
        title_font,
    );
    state.page_controls[3].push(settings_hint);
    for (id, label) in [
        (ID_STARTUP, "随 Windows 启动"),
        (ID_AUTO_UPDATE, "每日检查软件更新"),
        (ID_SHOW_API_KEY, "悬浮显示上游 API Key"),
    ] {
        state.page_controls[3].push(checkbox(hwnd, label, id, instance, font));
    }
    for (id, label) in [
        (ID_UPDATE, "检查软件更新..."),
        (ID_VERIFY_INSTALL, "验证当前安装..."),
        (ID_PRECHECK, "运行启动预检..."),
        (ID_SELECT_RUNTIME, "选择 Headroom Python..."),
        (ID_REPAIR_RUNTIME, "重新检测 Headroom 环境..."),
        (ID_CONFIG, "打开 config.json（高级配置）"),
        (ID_LOGS, "打开数据与日志目录"),
        (ID_CREATE_BACKUP, "创建配置备份"),
        (ID_RESTORE_BACKUP, "恢复配置备份..."),
        (ID_EXPORT_PORTABLE, "导出可移植配置..."),
        (ID_IMPORT_PORTABLE, "导入可移植配置..."),
        (ID_TAKEOVER, "预览并应用 CLI 接管..."),
        (ID_DIAG, "复制脱敏诊断报告"),
        (ID_DIAGNOSTIC_ZIP, "创建脱敏诊断 ZIP..."),
        (ID_PROVIDER_IDS, "复制 Provider ID 清单"),
        (ID_RELOAD_FAILOVER, "重新加载故障转移规则"),
        (ID_RESET_METRICS, "清零 Headroom 统计..."),
        (ID_RESTORE, "恢复 Codex / Claude 原始配置..."),
        (ID_UNINSTALL, "完全卸载并还原..."),
        (ID_APPROVAL_DEMO, "演示确认悬浮窗"),
    ] {
        state.page_controls[3].push(button(hwnd, label, id, instance, font));
    }
}

unsafe fn static_text(
    hwnd: HWND,
    text: &str,
    id: usize,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    editor_control(
        hwnd,
        "STATIC",
        text,
        0,
        0,
        0,
        0,
        id,
        WS_CHILD | WS_VISIBLE,
        instance,
        font,
    )
}

unsafe fn button(
    hwnd: HWND,
    text: &str,
    id: usize,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    editor_control(
        hwnd,
        "BUTTON",
        text,
        0,
        0,
        0,
        0,
        id,
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32 | WS_TABSTOP,
        instance,
        font,
    )
}

unsafe fn checkbox(
    hwnd: HWND,
    text: &str,
    id: usize,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    editor_control(
        hwnd,
        "BUTTON",
        text,
        0,
        0,
        0,
        0,
        id,
        WS_CHILD | WS_VISIBLE | BS_AUTOCHECKBOX as u32 | WS_TABSTOP,
        instance,
        font,
    )
}

pub(super) unsafe fn layout_controls(hwnd: HWND, state: &MainWindowState) {
    let mut client = RECT::default();
    GetClientRect(hwnd, &mut client);
    let tab = GetDlgItem(hwnd, ID_MAIN_TAB);
    SetWindowPos(
        tab,
        ptr::null_mut(),
        8,
        8,
        client.right - 16,
        client.bottom - 16,
        SWP_NOZORDER,
    );
    let mut inner = RECT {
        left: 8,
        top: 8,
        right: client.right - 8,
        bottom: client.bottom - 8,
    };
    SendMessageW(tab, TCM_ADJUSTRECT, 0, &mut inner as *mut RECT as LPARAM);
    let x = inner.left + 8;
    let y = inner.top + 8;
    let w = (inner.right - inner.left - 16).max(40);
    let h = (inner.bottom - inner.top - 16).max(40);
    place_page(state, 0, x, y, w, h, 36);
    place_page(state, 1, x, y, w, h, 28);
    place_page(state, 2, x, y, w, h, 32);
    place_page(state, 3, x, y, w, h, 28);
    apply_tab_visibility(hwnd, state);
}

unsafe fn place_page(
    state: &MainWindowState,
    page: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    row: i32,
) {
    let controls = &state.page_controls[page];
    if controls.is_empty() {
        return;
    }
    match page {
        0 => {
            let button_h = 32;
            SetWindowPos(
                controls[0],
                ptr::null_mut(),
                x,
                y,
                w,
                (h - button_h - 8).max(40),
                SWP_NOZORDER,
            );
            if let Some(&button) = controls.get(1) {
                SetWindowPos(
                    button,
                    ptr::null_mut(),
                    x,
                    y + h - button_h,
                    w,
                    button_h,
                    SWP_NOZORDER,
                );
            }
        }
        1 => {
            SetWindowPos(controls[0], ptr::null_mut(), x, y, w, 36, SWP_NOZORDER);
            let half = (w / 2).max(80);
            SetWindowPos(
                controls[1],
                ptr::null_mut(),
                x,
                y + 40,
                half - 8,
                24,
                SWP_NOZORDER,
            );
            SetWindowPos(
                controls[2],
                ptr::null_mut(),
                x + half,
                y + 40,
                w - half,
                24,
                SWP_NOZORDER,
            );
            SetWindowPos(
                controls[3],
                ptr::null_mut(),
                x,
                y + 72,
                w,
                (h - 72).max(40),
                SWP_NOZORDER,
            );
        }
        _ => {
            for (index, &control) in controls.iter().enumerate() {
                SetWindowPos(
                    control,
                    ptr::null_mut(),
                    x,
                    y + (index as i32) * row,
                    w,
                    row - 4,
                    SWP_NOZORDER,
                );
            }
        }
    }
}

pub(super) unsafe fn apply_tab_visibility(hwnd: HWND, state: &MainWindowState) {
    let _ = hwnd;
    for (index, controls) in state.page_controls.iter().enumerate() {
        let show = if index as i32 == state.tab {
            SW_SHOW
        } else {
            SW_HIDE
        };
        for &control in controls {
            ShowWindow(control, show);
        }
    }
}

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

pub(super) unsafe fn activate_selected_route(hwnd: HWND) {
    let index = SendMessageW(GetDlgItem(hwnd, ID_MAIN_ROUTE_LIST), LB_GETCURSEL, 0, 0);
    if index < 0 {
        return;
    }
    if let Some(app) = APP.get()
        && app.switch_index(index as usize, "主窗口手动切换")
    {
        let _ = app.write_status();
        refresh_main_window(hwnd);
    }
}

pub(super) unsafe fn refresh_main_window(hwnd: HWND) {
    let Some(app) = APP.get() else {
        return;
    };
    let snapshot = app.snapshot();
    let status = format!(
        "{}\r\n\r\nCodex：{}\r\n  上游：{}\r\n\r\nClaude：{}\r\n  上游：{}\r\n\r\nHeadroom：{}\r\n同步：{}\r\n重启：{}\r\n{}",
        snapshot.runtime_status.summary(),
        snapshot.runtime_status.codex.summary(),
        app.route_summary(Protocol::OpenAi),
        snapshot.runtime_status.claude.summary(),
        app.route_summary(Protocol::Anthropic),
        snapshot.runtime_status.headroom.summary(),
        snapshot.sync_status,
        snapshot.restart_status,
        app.recovery_hint()
    );
    SetWindowTextW(
        GetDlgItem(hwnd, ID_MAIN_STATUS_BODY),
        wide(&status).as_ptr(),
    );

    let list = GetDlgItem(hwnd, ID_MAIN_ROUTE_LIST);
    SendMessageW(list, LB_RESETCONTENT, 0, 0);
    for route in &snapshot.routes {
        let selected = match route.protocol {
            Protocol::OpenAi => route_is_selected(route, snapshot.active_provider.as_deref()),
            Protocol::Anthropic => {
                route_is_selected(route, snapshot.active_anthropic_provider.as_deref())
            }
        };
        let marker = if selected { "●" } else { "○" };
        let line = format!(
            "{marker} {} · {} · {} · {}",
            route.name,
            route.state.label(),
            route.evidence_label(),
            latency_text(route.latency_ms)
        );
        SendMessageW(list, LB_ADDSTRING, 0, wide(&line).as_ptr() as LPARAM);
    }

    set_check(hwnd, ID_MANAGE_UPSTREAM, snapshot.manage_codex);
    set_check(hwnd, ID_MANAGE_CLAUDE, snapshot.manage_claude);
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
        i32::from(!app.sync_in_progress.load(Ordering::Acquire)),
    );
    EnableWindow(
        GetDlgItem(hwnd, ID_RESTART as i32),
        i32::from(!app.restart_in_progress.load(Ordering::Acquire)),
    );
    EnableWindow(
        GetDlgItem(hwnd, ID_UPDATE as i32),
        i32::from(!updater::is_running()),
    );

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainWindowState;
    if state_ptr.is_null() {
        return;
    }
    match recommended_action(
        &snapshot.runtime_status,
        &snapshot.headroom_state,
        snapshot.last_error.as_deref(),
    ) {
        Some((id, label)) => {
            (*state_ptr).recommended_command = id;
            SetWindowTextW(GetDlgItem(hwnd, ID_MAIN_RECOMMENDED), wide(label).as_ptr());
            EnableWindow(GetDlgItem(hwnd, ID_MAIN_RECOMMENDED), 1);
        }
        None => {
            (*state_ptr).recommended_command = 0;
            SetWindowTextW(
                GetDlgItem(hwnd, ID_MAIN_RECOMMENDED),
                wide("暂无建议操作").as_ptr(),
            );
            EnableWindow(GetDlgItem(hwnd, ID_MAIN_RECOMMENDED), 0);
        }
    }
}
