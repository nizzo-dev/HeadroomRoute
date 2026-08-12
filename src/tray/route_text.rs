use crate::model::Route;

const HOVER_KEY_LINE_WIDTH: usize = 64;

fn wrap_hover_value(value: &str, width: usize) -> String {
    let value: Vec<char> = value
        .chars()
        .filter(|character| !matches!(character, '\0' | '\r' | '\n'))
        .collect();
    value
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub(super) fn route_hover_text(route: &Route, show_key: bool) -> String {
    if !show_key {
        return route.base_url.clone();
    }
    let key = route
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .map(|key| wrap_hover_value(key, HOVER_KEY_LINE_WIDTH))
        .unwrap_or_else(|| "未配置".into());
    format!("{}\r\nAPI Key：{key}", route.base_url)
}

pub(super) fn hover_popup_size(text: &str) -> (i32, i32) {
    let max_chars = text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let line_count = text.lines().count().max(1);
    let width = (i32::try_from(max_chars).unwrap_or(100) * 8 + 32).clamp(300, 900);
    let height = (i32::try_from(line_count).unwrap_or(1) * 18 + 16).max(30);
    (width, height)
}

pub(super) fn latency_text(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "--".into())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn compact_number(value: u64) -> String {
    let (divisor, suffix) = if value >= 1_000_000_000 {
        (1_000_000_000, "B")
    } else if value >= 1_000_000 {
        (1_000_000, "M")
    } else if value >= 1_000 {
        (1_000, "K")
    } else {
        return value.to_string();
    };
    let number = format!("{:.1}", value as f64 / divisor as f64);
    format!("{}{suffix}", number.trim_end_matches(".0"))
}
