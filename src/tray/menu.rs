use super::*;

pub(super) unsafe fn show_menu(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let snapshot = app.snapshot();
    let menu = unsafe { CreatePopupMenu() };
    let service = format!("状态：{}", snapshot.runtime_status.summary());
    unsafe {
        // Disabled native menu items with ID 0 stay inert but render normal text.
        AppendMenuW(menu, MF_STRING, 0, wide(&service).as_ptr());
        AppendMenuW(
            menu,
            MF_STRING,
            ID_OPEN_STATUS,
            wide("打开主窗口").as_ptr(),
        );
        if let Some((command, label)) = recommended_action(
            &snapshot.runtime_status,
            &snapshot.headroom_state,
            snapshot.last_error.as_deref(),
        ) {
            AppendMenuW(menu, MF_STRING, command, wide(label).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_CHECK, wide("立即检查上游").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(
            menu,
            MF_STRING,
            ID_EXIT,
            wide(if snapshot.manage_upstream {
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

pub(super) fn recommended_action(
    status: &RuntimeStatus,
    headroom_state: &str,
    error: Option<&str>,
) -> Option<(usize, &'static str)> {
    if status.headroom.state == ComponentState::Unavailable
        && headroom_state == "runtime-unavailable"
    {
        return Some((ID_SELECT_RUNTIME, "建议操作：选择 Headroom Python..."));
    }
    if status.headroom.state == ComponentState::Checking {
        return None;
    }
    if status.headroom.state == ComponentState::Unavailable {
        return Some((ID_RESTART, "建议操作：重启 Headroom"));
    }
    let error = error.unwrap_or_default().to_ascii_lowercase();
    if ["同步", "配置", "routing", "route guard"]
        .iter()
        .any(|word| error.contains(word))
    {
        return Some((ID_SYNC, "建议操作：重新同步配置"));
    }
    if [status.codex.state, status.claude.state]
        .into_iter()
        .any(|state| {
            matches!(
                state,
                ComponentState::Degraded | ComponentState::Unavailable
            )
        })
    {
        return Some((ID_CHECK, "建议操作：立即检查上游"));
    }
    (!error.is_empty()).then_some((ID_DIAG, "建议操作：复制脱敏诊断报告"))
}
