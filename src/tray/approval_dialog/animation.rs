use super::*;

pub(crate) unsafe fn refresh_approval_popup() {
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
        let notice_expired = APPROVAL_REQUEST.with(|request| {
            request.borrow().as_ref().is_some_and(|request| {
                let timeout = match request.popup_kind {
                    PopupKind::Success => Some(std::time::Duration::from_secs(4)),
                    PopupKind::Error => Some(std::time::Duration::from_secs(7)),
                    PopupKind::Confirmation => None,
                };
                timeout.is_some_and(|timeout| {
                    APPROVAL_VISUAL.with(|visual| {
                        visual
                            .borrow()
                            .as_ref()
                            .is_some_and(|visual| visual.started_at.elapsed() >= timeout)
                    })
                })
            })
        });
        if notice_expired {
            approval::resolve(id, ApprovalChoice::Deny);
            unsafe { begin_approval_close() };
            return;
        }
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
    let informational = request.popup_kind != PopupKind::Confirmation;
    APPROVAL_REQUEST.with(|slot| *slot.borrow_mut() = Some(request));
    let work_area = unsafe { approval_work_area() };
    let dpi = unsafe { approval_dpi() };
    let scale = |value: i32| value.saturating_mul(dpi as i32) / 96;
    let compact_width = scale(280);
    let compact_height = scale(58);
    let expanded_width =
        scale(520).min((work_area.right - work_area.left - scale(24)).max(compact_width));
    let expanded_height = scale(if informational { 190 } else { 286 });
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
pub(super) unsafe fn approval_dpi() -> u32 {
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

pub(crate) fn approval_scale(value: i32, dpi: u32) -> i32 {
    approval_layout::approval_scale(value, dpi)
}

pub(crate) fn approval_lerp(start: i32, end: i32, progress: f32) -> i32 {
    approval_layout::approval_lerp(start, end, progress)
}

pub(crate) fn approval_ease(progress: f32) -> f32 {
    approval_layout::approval_ease(progress)
}

pub(super) fn approval_rgb(red: u8, green: u8, blue: u8) -> u32 {
    approval_layout::approval_rgb(red, green, blue)
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
pub(super) unsafe fn begin_approval_close() {
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
pub(super) unsafe fn advance_approval_animation(hwnd: HWND) {
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
pub(super) unsafe fn update_approval_countdown() {}

#[allow(unsafe_op_in_unsafe_fn)]
pub(crate) unsafe fn hide_approval_popup() {
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
pub(super) unsafe fn resolve_approval_popup(hwnd: HWND, choice: ApprovalChoice) {
    let id = APPROVAL_REQUEST.with(|slot| slot.borrow().as_ref().map(|request| request.id));
    if let Some(id) = id {
        approval::resolve(id, choice);
    }
    unsafe { begin_approval_close() };
    let _ = hwnd;
}
