use super::*;

pub(super) unsafe fn show_hovered_route_url(hwnd: HWND, wparam: WPARAM) {
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

pub(super) unsafe fn hide_route_url() {
    URL_POPUP.with(|slot| {
        let popup = slot.get();
        if !popup.is_null() {
            unsafe {
                ShowWindow(popup, SW_HIDE);
            }
        }
    });
}

pub(super) unsafe fn destroy_route_url() {
    URL_POPUP.with(|slot| {
        let popup = slot.replace(ptr::null_mut());
        if !popup.is_null() {
            unsafe {
                DestroyWindow(popup);
            }
        }
    });
}

pub(super) unsafe fn add_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &data);
        DestroyIcon(data.hIcon);
    }
}
pub(super) unsafe fn remove_icon(hwnd: HWND) {
    let data = notify_data(hwnd);
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
        DestroyIcon(data.hIcon);
    }
}
pub(super) unsafe fn update_icon(hwnd: HWND) {
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
                    "Headroom Route：{}\r\nCodex：{} · {codex}\r\nClaude：{} · {claude}\r\nHeadroom：{}",
                    s.runtime_status.mode.label(),
                    s.runtime_status.codex.state.label(),
                    s.runtime_status.claude.state.label(),
                    s.runtime_status.headroom.state.label()
                ),
                s.runtime_status.mode.health_key(),
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
