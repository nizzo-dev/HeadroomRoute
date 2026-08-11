use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
pub(crate) unsafe fn show_failover_editor(parent: HWND) {
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
pub(crate) unsafe extern "system" fn failover_window_proc(
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
