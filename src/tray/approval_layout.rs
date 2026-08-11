use super::RECT;

pub(super) fn approval_scale(value: i32, dpi: u32) -> i32 {
    value.saturating_mul(dpi as i32) / 96
}

pub(super) fn approval_lerp(start: i32, end: i32, progress: f32) -> i32 {
    start + ((end - start) as f32 * progress).round() as i32
}

pub(super) fn approval_ease(progress: f32) -> f32 {
    1.0 - (1.0 - progress).powi(3)
}

pub(super) fn approval_rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

pub(super) fn point_in_rect(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

pub(super) fn approval_deny_rect(width: i32, height: i32, dpi: u32, allow_rule: bool) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    RECT {
        left: if allow_rule {
            scale(18)
        } else {
            width - scale(218)
        },
        top: height - scale(56),
        right: if allow_rule {
            scale(160)
        } else {
            width - scale(122)
        },
        bottom: height - scale(18),
    }
}

pub(super) fn approval_rule_rect(width: i32, height: i32, dpi: u32) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    RECT {
        left: scale(166),
        top: height - scale(56),
        right: width - scale(166),
        bottom: height - scale(18),
    }
}

pub(super) fn approval_allow_rect(width: i32, height: i32, dpi: u32, allow_rule: bool) -> RECT {
    let scale = |value: i32| approval_scale(value, dpi);
    RECT {
        left: if allow_rule {
            width - scale(160)
        } else {
            width - scale(112)
        },
        top: height - scale(56),
        right: width - scale(18),
        bottom: height - scale(18),
    }
}

pub(super) fn wrap_approval_text(text: &str, width: usize, max_lines: usize) -> String {
    let limit = width.saturating_mul(max_lines);
    let characters = text.chars().collect::<Vec<_>>();
    let truncated = characters.len() > limit;
    let mut lines = Vec::new();
    let mut current = String::new();
    for character in characters.into_iter().take(limit) {
        if current.chars().count() >= width {
            lines.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        return "--".into();
    }
    if truncated {
        let last = lines.last_mut().unwrap();
        last.pop();
        last.push('…');
    }
    lines.join("\r\n")
}
