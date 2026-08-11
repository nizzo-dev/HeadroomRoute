use super::RouteHealth;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    Normal,
    Degraded,
    Bypass,
    Direct,
    Recovering,
}

impl RuntimeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "正常",
            Self::Degraded => "降级",
            Self::Bypass => "旁路",
            Self::Direct => "直连",
            Self::Recovering => "恢复中",
        }
    }

    pub fn health_key(self) -> &'static str {
        match self {
            Self::Normal | Self::Bypass | Self::Direct => "healthy",
            Self::Degraded => "degraded",
            Self::Recovering => "unknown",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientPath {
    Disabled,
    Headroom,
    Bypass,
    Direct,
}

impl ClientPath {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "未启用",
            Self::Headroom => "经 Headroom",
            Self::Bypass => "旁路 Headroom",
            Self::Direct => "直连上游",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentState {
    Disabled,
    NotRequired,
    Ready,
    Checking,
    Degraded,
    Unavailable,
}

impl ComponentState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "未启用",
            Self::NotRequired => "不需要",
            Self::Ready => "可用",
            Self::Checking => "检测中",
            Self::Degraded => "降级",
            Self::Unavailable => "不可用",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClientRuntimeStatus {
    pub path: ClientPath,
    pub state: ComponentState,
    pub reason: String,
}

impl ClientRuntimeStatus {
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {}",
            self.path.label(),
            self.state.label(),
            self.reason
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HeadroomRuntimeStatus {
    pub state: ComponentState,
    pub reason: String,
}

impl HeadroomRuntimeStatus {
    pub fn summary(&self) -> String {
        format!("{} · {}", self.state.label(), self.reason)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeStatus {
    pub mode: RuntimeMode,
    pub reason: String,
    pub codex: ClientRuntimeStatus,
    pub claude: ClientRuntimeStatus,
    pub headroom: HeadroomRuntimeStatus,
}

impl RuntimeStatus {
    pub fn summary(&self) -> String {
        format!("{} · {}", self.mode.label(), self.reason)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeStatusInput<'a> {
    pub codex_enabled: bool,
    pub claude_enabled: bool,
    pub direct_codex: bool,
    pub direct_claude: bool,
    pub bypass_headroom: bool,
    pub codex_route_health: Option<RouteHealth>,
    pub claude_route_health: Option<RouteHealth>,
    pub headroom_state: &'a str,
    pub sync_in_progress: bool,
    pub restart_in_progress: bool,
    pub recovery_in_progress: bool,
}

/// 五种运行模式的统一判定，是托盘、完整状态、预检与诊断报告共用的单一强类型来源。
///
/// 优先级从高到低（首次命中的分支即当前模式；条件变化后重新求值即进入/退出，
/// 无需手动迁移，`reason` 即为可读的迁移原因）：
///
/// 1. `Degraded`（降级）：必需且正在使用的组件失败 —— 需要 Headroom 但 Headroom
///    不可用，或任一启用客户端的当前路由失败/不可用/未配置。真实请求已受影响，
///    故优先级最高；退出条件是失败组件恢复为可用/待验证。
/// 2. `Recovering`（恢复中，显式操作）：重启或同步进行中。这是用户可见的进行中
///    操作，即使在直连/旁路拓扑下也应在顶层显示；退出条件是操作完成。
/// 3. `Direct`（直连）：任一启用客户端直连上游（不经本地代理，也必然不经 Headroom）。
///    显式拓扑，优先于校验态；退出条件是全部直连客户端恢复经代理路由。
/// 4. `Bypass`（旁路）：`bypass_headroom` 开启且存在启用但未直连的客户端。显式拓扑，
///    优先于校验态；退出条件是旁路开关关闭。
/// 5. `Recovering`（恢复中，校验态）：必需组件仍在检测（Headroom 启动、Headroom
///    路径路由等待验证）。仅 `RouteHealth::Unknown` 会进入此态，不打断稳定的
///    Direct/Bypass；仅作观察层：不改变实际请求路径，也不阻塞真实请求；退出条件是
///    操作完成或组件进入就绪/失败态。
/// 6. `Normal`（正常）：所有启用客户端与所需组件均可用，且无进行中操作。
pub fn evaluate_runtime_status(input: RuntimeStatusInput<'_>) -> RuntimeStatus {
    let codex_path = client_path(
        input.codex_enabled,
        input.direct_codex,
        input.bypass_headroom,
    );
    let claude_path = client_path(
        input.claude_enabled,
        input.direct_claude,
        input.bypass_headroom,
    );
    let headroom_required =
        matches!(codex_path, ClientPath::Headroom) || matches!(claude_path, ClientPath::Headroom);
    let headroom = headroom_status(headroom_required, input.headroom_state);
    let codex = client_status(codex_path, input.codex_route_health, &headroom);
    let claude = client_status(claude_path, input.claude_route_health, &headroom);

    let headroom_failed = headroom_required && headroom.state == ComponentState::Unavailable;
    let client_failed = [(&codex, "Codex"), (&claude, "Claude")]
        .into_iter()
        .find(|(status, _)| {
            matches!(
                status.state,
                ComponentState::Degraded | ComponentState::Unavailable
            )
        });
    let bypass = input.bypass_headroom
        && [codex_path, claude_path]
            .into_iter()
            .any(|path| path == ClientPath::Bypass);
    let direct = [codex_path, claude_path]
        .into_iter()
        .any(|path| path == ClientPath::Direct);

    let (mode, reason) = if headroom_failed {
        (RuntimeMode::Degraded, headroom.reason.clone())
    } else if let Some((status, name)) = client_failed {
        (RuntimeMode::Degraded, format!("{name}：{}", status.reason))
    } else if input.restart_in_progress {
        (RuntimeMode::Recovering, "正在重启 Headroom".into())
    } else if input.sync_in_progress {
        (RuntimeMode::Recovering, "正在同步客户端路由配置".into())
    } else if input.recovery_in_progress {
        (RuntimeMode::Recovering, "正在恢复本地运行环境".into())
    } else if direct {
        let all_enabled_direct = [codex_path, claude_path]
            .into_iter()
            .filter(|path| *path != ClientPath::Disabled)
            .all(|path| path == ClientPath::Direct);
        (
            RuntimeMode::Direct,
            if all_enabled_direct {
                "所有启用的客户端均直连上游".into()
            } else {
                "部分客户端直连上游，其余客户端保持当前路径".into()
            },
        )
    } else if bypass {
        (
            RuntimeMode::Bypass,
            "启用的非直连客户端已旁路 Headroom".into(),
        )
    } else if headroom.state == ComponentState::Checking {
        (RuntimeMode::Recovering, headroom.reason.clone())
    } else if let Some((status, name)) = [(&codex, "Codex"), (&claude, "Claude")]
        .into_iter()
        .find(|(status, _)| status.state == ComponentState::Checking)
    {
        (
            RuntimeMode::Recovering,
            format!("{name}：{}", status.reason),
        )
    } else {
        let any_enabled = [codex_path, claude_path]
            .into_iter()
            .any(|path| path != ClientPath::Disabled);
        (
            RuntimeMode::Normal,
            if any_enabled {
                "所有启用的客户端与所需组件均可用".into()
            } else {
                "Codex 与 Claude 均未启用".into()
            },
        )
    };

    RuntimeStatus {
        mode,
        reason,
        codex,
        claude,
        headroom,
    }
}

fn client_path(enabled: bool, direct: bool, bypass_headroom: bool) -> ClientPath {
    if !enabled {
        ClientPath::Disabled
    } else if direct {
        ClientPath::Direct
    } else if bypass_headroom {
        ClientPath::Bypass
    } else {
        ClientPath::Headroom
    }
}

fn headroom_status(required: bool, state: &str) -> HeadroomRuntimeStatus {
    if !required {
        return HeadroomRuntimeStatus {
            state: ComponentState::NotRequired,
            reason: "当前客户端路径不经过 Headroom".into(),
        };
    }
    let (state, reason) = match state {
        "healthy" => (ComponentState::Ready, "本地 Headroom 正常"),
        "external" => (ComponentState::Ready, "已连接外部 Headroom"),
        "检测中" | "运行环境就绪" | "starting" | "restarting" => {
            (ComponentState::Checking, "Headroom 正在启动或恢复")
        }
        "runtime-unavailable" => (ComponentState::Unavailable, "Headroom 运行环境不可用"),
        _ => (ComponentState::Unavailable, "Headroom 服务不可用"),
    };
    HeadroomRuntimeStatus {
        state,
        reason: reason.into(),
    }
}

fn client_status(
    path: ClientPath,
    route_health: Option<RouteHealth>,
    headroom: &HeadroomRuntimeStatus,
) -> ClientRuntimeStatus {
    if path == ClientPath::Disabled {
        return ClientRuntimeStatus {
            path,
            state: ComponentState::Disabled,
            reason: "客户端未启用".into(),
        };
    }
    let Some(route_health) = route_health else {
        return ClientRuntimeStatus {
            path,
            state: ComponentState::Unavailable,
            reason: "未配置可用路由".into(),
        };
    };
    if path == ClientPath::Headroom && headroom.state != ComponentState::Ready {
        return ClientRuntimeStatus {
            path,
            state: headroom.state,
            reason: headroom.reason.clone(),
        };
    }
    let (state, reason) = match route_health {
        RouteHealth::Healthy => (ComponentState::Ready, "当前路由已验证"),
        RouteHealth::Unknown => (ComponentState::Checking, "当前路由等待验证"),
        RouteHealth::Degraded => (ComponentState::Degraded, "当前路由出现失败"),
        RouteHealth::Unavailable => (ComponentState::Unavailable, "当前路由不可用"),
    };
    ClientRuntimeStatus {
        path,
        state,
        reason: reason.into(),
    }
}
