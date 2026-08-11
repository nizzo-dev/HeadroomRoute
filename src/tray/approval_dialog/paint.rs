use super::animation::approval_rgb;
use super::hit_test::approval_rule_rect;
use super::*;
#[allow(unsafe_op_in_unsafe_fn)]
pub(super) unsafe fn paint_approval_popup(hwnd: HWND) {
    let mut paint = std::mem::zeroed::<PAINTSTRUCT>();
    let dc = BeginPaint(hwnd, &mut paint);
    if dc.is_null() {
        return;
    }
    let mut client = std::mem::zeroed::<RECT>();
    GetClientRect(hwnd, &mut client);
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    if width <= 0 || height <= 0 {
        EndPaint(hwnd, &paint);
        return;
    }
    let memory = CreateCompatibleDC(dc);
    let bitmap = if !memory.is_null() {
        CreateCompatibleBitmap(dc, width, height)
    } else {
        ptr::null_mut()
    };
    if memory.is_null() || bitmap.is_null() {
        EndPaint(hwnd, &paint);
        if !memory.is_null() {
            DeleteDC(memory);
        }
        return;
    }
    let previous_bitmap = SelectObject(memory, bitmap as _);
    let background = CreateSolidBrush(approval_rgb(8, 11, 15));
    FillRect(memory, &client, background);
    DeleteObject(background as _);
    draw_approval_contents(memory, width, height);
    BitBlt(dc, 0, 0, width, height, memory, 0, 0, SRCCOPY);
    SelectObject(memory, previous_bitmap);
    DeleteObject(bitmap as _);
    DeleteDC(memory);
    EndPaint(hwnd, &paint);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_approval_contents(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    width: i32,
    height: i32,
) {
    let Some((request, visual)) = APPROVAL_REQUEST.with(|request_slot| {
        APPROVAL_VISUAL.with(|visual_slot| {
            Some((
                request_slot.borrow().clone()?,
                visual_slot.borrow().as_ref()?.clone_visual(),
            ))
        })
    }) else {
        return;
    };
    let scale = |value: i32| approval_scale(value, visual.dpi);
    let radius = height.min(scale(24)).max(scale(18));
    let border = if visual.hover != ApprovalHit::None {
        approval_rgb(55, 71, 85)
    } else {
        approval_rgb(35, 43, 53)
    };
    draw_round_box(
        dc,
        scale(1),
        scale(1),
        width - scale(1),
        height - scale(1),
        radius,
        approval_rgb(14, 18, 24),
        border,
    );
    let accent = match request.popup_kind {
        PopupKind::Success => approval_rgb(67, 214, 142),
        PopupKind::Error => approval_rgb(255, 92, 108),
        PopupKind::Confirmation if request.cli.eq_ignore_ascii_case("codex") => {
            approval_rgb(90, 164, 255)
        }
        PopupKind::Confirmation => approval_rgb(255, 165, 87),
    };
    draw_circle(dc, scale(22), scale(22), scale(7), accent);
    let (position, total) = approval::request_position(request.id);
    let title = match request.popup_kind {
        PopupKind::Confirmation => format!(
            "{} · 会话 {:04} · 请求 {}/{}",
            request.cli.to_uppercase(),
            request.pid % 10_000,
            position,
            total
        ),
        PopupKind::Success => format!(
            "{} · 会话 {:04} · 回复完成",
            request.cli.to_uppercase(),
            request.pid % 10_000
        ),
        PopupKind::Error => format!(
            "{} · 会话 {:04} · 请求失败",
            request.cli.to_uppercase(),
            request.pid % 10_000
        ),
    };
    draw_approval_text(
        dc,
        &title,
        RECT {
            left: scale(38),
            top: scale(12),
            right: width - scale(20),
            bottom: scale(36),
        },
        visual.title_font,
        approval_rgb(245, 247, 250),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
    if height < visual.expanded_height - scale(35) {
        return;
    }
    if request.popup_kind != PopupKind::Confirmation {
        draw_approval_text(
            dc,
            &request.action,
            RECT {
                left: scale(20),
                top: scale(60),
                right: width - scale(20),
                bottom: scale(88),
            },
            visual.body_font,
            accent,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
        );
        let summary = wrap_approval_text(&request.summary, 62, 3);
        draw_approval_text(
            dc,
            &summary,
            RECT {
                left: scale(20),
                top: scale(98),
                right: width - scale(20),
                bottom: height - scale(18),
            },
            visual.small_font,
            approval_rgb(205, 212, 222),
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );
        return;
    }
    draw_approval_text(
        dc,
        "执行请求",
        RECT {
            left: scale(20),
            top: scale(58),
            right: width - scale(20),
            bottom: scale(78),
        },
        visual.small_font,
        approval_rgb(142, 153, 168),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
    );
    let wrap_width = ((width - scale(40)) / scale(14).max(1)).clamp(28, 90) as usize;
    let action = wrap_approval_text(&request.action, wrap_width, 2);
    draw_approval_text(
        dc,
        &action,
        RECT {
            left: scale(20),
            top: scale(80),
            right: width - scale(20),
            bottom: scale(122),
        },
        visual.body_font,
        approval_rgb(241, 244, 248),
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
    );
    let cwd = format!("目录  {}", wrap_approval_text(&request.cwd, wrap_width, 1));
    draw_approval_text(
        dc,
        &cwd,
        RECT {
            left: scale(20),
            top: scale(132),
            right: width - scale(20),
            bottom: scale(153),
        },
        visual.small_font,
        approval_rgb(154, 164, 178),
        DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    let summary = wrap_approval_text(&request.summary, wrap_width, 2);
    draw_approval_text(
        dc,
        &summary,
        RECT {
            left: scale(20),
            top: scale(162),
            right: width - scale(20),
            bottom: height - scale(72),
        },
        visual.small_font,
        approval_rgb(116, 127, 142),
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
    );
    let deny = approval_deny_rect(width, height, visual.dpi);
    let rule = approval_rule_rect(width, height, visual.dpi);
    let allow = approval_allow_rect(width, height, visual.dpi);
    let deny_color = if visual.hover == ApprovalHit::Deny {
        approval_rgb(112, 44, 52)
    } else {
        approval_rgb(60, 34, 41)
    };
    let allow_color = if visual.hover == ApprovalHit::Allow {
        approval_rgb(61, 151, 103)
    } else {
        approval_rgb(42, 116, 80)
    };
    let rule_color = if visual.hover == ApprovalHit::Rule {
        approval_rgb(55, 105, 170)
    } else {
        approval_rgb(37, 72, 118)
    };
    draw_round_box(
        dc,
        deny.left,
        deny.top,
        deny.right,
        deny.bottom,
        scale(12),
        deny_color,
        deny_color,
    );
    draw_round_box(
        dc,
        allow.left,
        allow.top,
        allow.right,
        allow.bottom,
        scale(12),
        allow_color,
        allow_color,
    );
    if request.allow_rule {
        draw_round_box(
            dc,
            rule.left,
            rule.top,
            rule.right,
            rule.bottom,
            scale(12),
            rule_color,
            rule_color,
        );
        draw_approval_text(
            dc,
            "允许此类命令",
            rule,
            visual.small_font,
            approval_rgb(232, 242, 255),
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    draw_approval_text(
        dc,
        if request.feedback {
            "拒绝并反馈"
        } else {
            "拒绝"
        },
        deny,
        visual.body_font,
        approval_rgb(255, 224, 226),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    draw_approval_text(
        dc,
        "仅允许一次",
        allow,
        visual.body_font,
        approval_rgb(235, 255, 244),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

#[derive(Clone, Copy)]
struct ApprovalVisualSnapshot {
    dpi: u32,
    expanded_height: i32,
    hover: ApprovalHit,
    title_font: usize,
    body_font: usize,
    small_font: usize,
}

impl ApprovalVisual {
    fn clone_visual(&self) -> ApprovalVisualSnapshot {
        ApprovalVisualSnapshot {
            dpi: self.dpi,
            expanded_height: self.expanded_height,
            hover: self.hover,
            title_font: self.title_font,
            body_font: self.body_font,
            small_font: self.small_font,
        }
    }
}

#[allow(clippy::too_many_arguments, unsafe_op_in_unsafe_fn)]
unsafe fn draw_round_box(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    fill: u32,
    border: u32,
) {
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, border);
    let old_brush = SelectObject(dc, brush as _);
    let old_pen = SelectObject(dc, pen as _);
    RoundRect(dc, left, top, right, bottom, radius * 2, radius * 2);
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    DeleteObject(brush as _);
    DeleteObject(pen as _);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_circle(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    left: i32,
    top: i32,
    radius: i32,
    color: u32,
) {
    let brush = CreateSolidBrush(color);
    let old = SelectObject(dc, brush as _);
    Ellipse(dc, left - radius, top - radius, left + radius, top + radius);
    SelectObject(dc, old);
    DeleteObject(brush as _);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn draw_approval_text(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    text: &str,
    mut rect: RECT,
    font: usize,
    color: u32,
    flags: u32,
) {
    let selected_font = if font != 0 {
        font
    } else {
        GetStockObject(DEFAULT_GUI_FONT) as usize
    };
    let old_font = SelectObject(dc, selected_font as _);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    DrawTextW(dc, wide(text).as_ptr(), -1, &mut rect, flags);
    SelectObject(dc, old_font);
}

fn wrap_approval_text(text: &str, width: usize, max_lines: usize) -> String {
    approval_layout::wrap_approval_text(text, width, max_lines)
}
