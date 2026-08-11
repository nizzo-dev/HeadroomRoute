use super::{Protocol, Route, RouteHealth, RuntimeState};

pub(super) fn transport_ready(state: &RuntimeState, protocol: Protocol) -> bool {
    let direct = match protocol {
        Protocol::OpenAi => state.config.direct_codex,
        Protocol::Anthropic => state.config.direct_claude,
    };
    direct
        || state.config.bypass_headroom
        || matches!(state.headroom_state.as_str(), "healthy" | "external")
}

pub(super) fn availability(route: Option<&Route>, transport_ready: bool) -> &'static str {
    let Some(route) = route else {
        return "未配置";
    };
    if !transport_ready {
        return "不可用";
    }
    match route.state {
        RouteHealth::Healthy => "可用",
        RouteHealth::Degraded => "降级",
        RouteHealth::Unavailable => "不可用",
        RouteHealth::Unknown => "待验证",
    }
}

pub(super) fn metrics_scope(since: Option<chrono::DateTime<chrono::Utc>>) -> String {
    since.map_or_else(
        || "当前日志文件累计".into(),
        |since| format!("自 {} UTC", since.format("%Y-%m-%d %H:%M:%S")),
    )
}

pub(super) fn select_index(
    routes: &[Route],
    protocol: Protocol,
    selected: Option<&str>,
) -> Option<usize> {
    selected
        .and_then(|id| {
            routes
                .iter()
                .position(|r| r.protocol == protocol && r.provider == id)
        })
        .or_else(|| routes.iter().position(|r| r.protocol == protocol))
}

pub(super) fn provider_exists(routes: &[Route], protocol: Protocol, provider: &str) -> bool {
    routes
        .iter()
        .any(|route| route.protocol == protocol && route.provider == provider)
}

pub(super) fn valid_provider(
    routes: &[Route],
    protocol: Protocol,
    provider: Option<&str>,
) -> Option<String> {
    provider
        .filter(|id| provider_exists(routes, protocol, id))
        .map(str::to_owned)
}

pub(super) fn preserve_index(
    routes: &[Route],
    protocol: Protocol,
    old: Option<String>,
    selected: Option<&str>,
) -> Option<usize> {
    old.as_deref()
        .and_then(|provider| {
            routes
                .iter()
                .position(|r| r.protocol == protocol && r.provider == provider)
        })
        .or_else(|| select_index(routes, protocol, selected))
}

pub(super) fn route_summary(route: Option<&Route>) -> String {
    route
        .map(|route| {
            format!(
                "{} · {} · {} ms · HTTP {} · {}",
                route.name,
                route.evidence_label(),
                route
                    .latency_ms
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "--".into()),
                route
                    .last_status_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "--".into()),
                route.last_error.as_deref().unwrap_or("无错误")
            )
        })
        .unwrap_or_else(|| "未配置".into())
}

pub(super) fn recovery_hint(
    openai: Option<&Route>,
    anthropic: Option<&Route>,
    headroom: &str,
    last_error: Option<&str>,
) -> &'static str {
    if matches!(headroom, "unavailable" | "runtime-unavailable") {
        return "从托盘重启 Headroom；仍失败时按 README 检查外部运行环境";
    }
    let routes = [openai, anthropic];
    if routes
        .iter()
        .flatten()
        .any(|route| matches!(route.last_status_code, Some(401 | 403)))
    {
        return "检查当前 Provider 的 API Key 或登录状态";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| route.last_status_code == Some(429))
    {
        return "上游正在限流；启用自动切换或稍后重试";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| route.state == RouteHealth::Unavailable)
    {
        return "立即检查上游与系统代理，并启用自动切换";
    }
    if routes.iter().any(Option::is_none) {
        return "同步配置并确认 Codex、Claude 或 CC-Switch Provider 已配置";
    }
    if routes
        .iter()
        .flatten()
        .any(|route| matches!(route.state, RouteHealth::Unknown | RouteHealth::Degraded))
    {
        return "从托盘立即检查上游，等待健康状态确认";
    }
    if last_error.is_some() {
        return "复制脱敏诊断报告并打开日志目录查看详情";
    }
    "当前无需操作"
}

pub(super) fn yes(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}
