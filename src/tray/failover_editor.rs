use super::*;

#[path = "failover_editor/actions.rs"]
mod actions;
#[path = "failover_editor/helpers.rs"]
mod helpers;
#[path = "failover_editor/layout.rs"]
mod layout;
#[path = "failover_editor/refresh.rs"]
mod refresh;
#[path = "failover_editor/widgets.rs"]
mod widgets;
#[path = "failover_editor/window.rs"]
mod window;

pub(super) use widgets::{editor_control, precheck_report_edit};
pub(super) use window::{failover_window_proc, show_failover_editor};

pub(super) struct FailoverEditor {
    parent: HWND,
    app: Arc<AppState>,
    routes: Vec<Route>,
    policy: FailoverPolicy,
    auto_failover: bool,
    protocol: Protocol,
    sources: Vec<String>,
    source_provider: Option<String>,
    available: Vec<String>,
    dirty: bool,
    body_font: usize,
    title_font: usize,
}
