use super::*;

pub(super) fn create_approval_font(height: i32, weight: i32, dpi: u32) -> usize {
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
pub(super) unsafe fn approval_hit_test(hwnd: HWND, lparam: LPARAM) -> ApprovalHit {
    let x = (lparam as i16) as i32;
    let y = ((lparam >> 16) as i16) as i32;
    approval_hit_test_point(hwnd, x, y)
}

#[allow(unsafe_op_in_unsafe_fn)]
pub(super) unsafe fn approval_hit_test_screen(hwnd: HWND) -> ApprovalHit {
    let mut point = POINT::default();
    if GetCursorPos(&mut point) == 0 || ScreenToClient(hwnd, &mut point) == 0 {
        return ApprovalHit::None;
    }
    approval_hit_test_point(hwnd, point.x, point.y)
}

#[allow(unsafe_op_in_unsafe_fn)]
pub(super) unsafe fn approval_hit_test_point(hwnd: HWND, x: i32, y: i32) -> ApprovalHit {
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
    if APPROVAL_REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|request| request.popup_kind != PopupKind::Confirmation)
    }) {
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
    approval_layout::point_in_rect(rect, x, y)
}

pub(crate) fn approval_deny_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let allow_rule = APPROVAL_REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|request| request.allow_rule)
    });
    approval_layout::approval_deny_rect(width, height, dpi, allow_rule)
}

pub(super) fn approval_rule_rect(width: i32, height: i32, dpi: u32) -> RECT {
    approval_layout::approval_rule_rect(width, height, dpi)
}

pub(crate) fn approval_allow_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let allow_rule = APPROVAL_REQUEST.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|request| request.allow_rule)
    });
    approval_layout::approval_allow_rect(width, height, dpi, allow_rule)
}
