use super::animation::{advance_approval_animation, resolve_approval_popup};
use super::hit_test::{approval_hit_test, approval_hit_test_screen, create_approval_font};
use super::paint::paint_approval_popup;
use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
pub(crate) unsafe extern "system" fn approval_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().as_mut() {
                    visual.title_font = create_approval_font(16, FW_SEMIBOLD as i32, visual.dpi);
                    visual.body_font = create_approval_font(14, FW_NORMAL as i32, visual.dpi);
                    visual.small_font = create_approval_font(12, FW_NORMAL as i32, visual.dpi);
                }
            });
            0
        }
        WM_PAINT => {
            unsafe { paint_approval_popup(hwnd) };
            0
        }
        WM_TIMER if wparam == APPROVAL_ANIMATION_TIMER => {
            unsafe { advance_approval_animation(hwnd) };
            0
        }
        WM_MOUSEMOVE => {
            let hit = approval_hit_test(hwnd, lparam);
            let changed = APPROVAL_VISUAL.with(|slot| {
                let mut visual = slot.borrow_mut();
                let Some(visual) = visual.as_mut() else {
                    return false;
                };
                if visual.hover == hit {
                    false
                } else {
                    visual.hover = hit;
                    true
                }
            });
            if changed {
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut tracking);
            0
        }
        WM_MOUSELEAVE => {
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().as_mut() {
                    visual.hover = ApprovalHit::None;
                }
            });
            InvalidateRect(hwnd, ptr::null(), 0);
            0
        }
        WM_LBUTTONUP => {
            let informational = APPROVAL_REQUEST.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .is_some_and(|request| request.popup_kind != PopupKind::Confirmation)
            });
            if informational {
                unsafe { resolve_approval_popup(hwnd, ApprovalChoice::Deny) };
                return 0;
            }
            let hit = approval_hit_test(hwnd, lparam);
            if matches!(
                hit,
                ApprovalHit::Allow | ApprovalHit::Rule | ApprovalHit::Deny
            ) {
                let choice = match hit {
                    ApprovalHit::Allow => ApprovalChoice::AllowOnce,
                    ApprovalHit::Rule => ApprovalChoice::AllowRule,
                    ApprovalHit::Deny => APPROVAL_REQUEST.with(|slot| {
                        if slot
                            .borrow()
                            .as_ref()
                            .is_some_and(|request| request.feedback)
                        {
                            ApprovalChoice::Feedback
                        } else {
                            ApprovalChoice::Deny
                        }
                    }),
                    ApprovalHit::None => ApprovalChoice::Deny,
                };
                unsafe { resolve_approval_popup(hwnd, choice) };
            }
            0
        }
        WM_CLOSE => {
            unsafe { resolve_approval_popup(hwnd, ApprovalChoice::Deny) };
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_SETCURSOR => {
            let hit = approval_hit_test_screen(hwnd);
            if matches!(
                hit,
                ApprovalHit::Allow | ApprovalHit::Rule | ApprovalHit::Deny
            ) {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_HAND));
            } else {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW));
            }
            1
        }
        WM_DESTROY => {
            KillTimer(hwnd, APPROVAL_ANIMATION_TIMER);
            APPROVAL_VISUAL.with(|slot| {
                if let Some(visual) = slot.borrow_mut().take() {
                    if visual.title_font != 0 {
                        DeleteObject(visual.title_font as _);
                    }
                    if visual.body_font != 0 {
                        DeleteObject(visual.body_font as _);
                    }
                    if visual.small_font != 0 {
                        DeleteObject(visual.small_font as _);
                    }
                }
            });
            APPROVAL_POPUP.with(|slot| {
                if slot.get() == hwnd {
                    slot.set(ptr::null_mut());
                }
            });
            0
        }
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
