#![cfg(windows)]

use anyhow::Result;
use std::process;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_APP;

#[path = "approval/broker.rs"]
mod broker;
#[path = "approval/headroom_project.rs"]
mod headroom_project;
#[path = "approval/pipe.rs"]
mod pipe;
#[path = "approval/request_data.rs"]
mod request_data;
#[path = "approval/terminal/mod.rs"]
mod terminal;
#[path = "approval/turn.rs"]
mod turn;

use broker::enqueue;
use pipe::{cancel_remote_requests, connect_pipe, request_approval};
use request_data::{
    ApprovalDecision, ConfirmationPrompt, WireRequest, WireResponse, clamp_text,
    confirmation_prompt, valid_wire_request, write_reason, write_response,
};
pub use terminal::run_cli_command;
use terminal::{approval_trace, wide};
use turn::{
    TurnResult, classify_turn_result, claude_screen_has_new_reply, cli_input_prompt_ready,
    completion_bullet_visible, notify_turn_result, write_claude_stop_hook_settings,
};

#[cfg(test)]
use broker::is_cli_executable_name;
pub use request_data::{ApprovalChoice, ApprovalRequest, PopupKind};
#[cfg(test)]
use request_data::{confirmation_answers, prompt_summary, strip_ansi};
#[cfg(test)]
use terminal::{InputSink, TerminalScreen, build_command_line, quote_cmd_arg};

pub const WM_APPROVAL: u32 = WM_APP + 7;

pub(super) const PIPE_NAME: &str = r"\\.\pipe\HeadroomRouteApproval-v1";
pub(super) const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub(super) const MAX_PENDING_REQUESTS: usize = 32;
pub(super) const CHILD_SCREEN_WIDTH: i16 = 120;
pub(super) const CHILD_SCREEN_HEIGHT: i16 = 40;
pub(super) const GENERIC_READ_FLAG: u32 = 0x8000_0000;
pub(super) const GENERIC_WRITE_FLAG: u32 = 0x4000_0000;
pub(super) const PIPE_ACCESS_DUPLEX_FLAG: u32 = 0x0000_0003;
pub(super) const BACKGROUND_REMINDER_DELAY: std::time::Duration =
    std::time::Duration::from_millis(350);

#[allow(dead_code)] // Used by the CLI binary; this module is also compiled into the tray binary.
pub fn run_codex_notify(args: &[String]) -> Result<()> {
    turn::run_codex_notify(args)
}

#[allow(dead_code)] // Used by the CLI binary; this module is also compiled into the tray binary.
pub fn run_claude_notify(args: &[String]) -> Result<()> {
    turn::run_claude_notify(args)
}

pub fn start_server() {
    broker::start_server();
}

pub fn ensure_server() -> bool {
    broker::ensure_server()
}

pub fn set_tray_hwnd(hwnd: windows_sys::Win32::Foundation::HWND) {
    broker::set_tray_hwnd(hwnd);
}

pub fn clear_tray_hwnd(hwnd: windows_sys::Win32::Foundation::HWND) {
    broker::clear_tray_hwnd(hwnd);
}

pub fn next_request() -> Option<ApprovalRequest> {
    broker::next_request()
}

pub fn should_show(id: u64) -> bool {
    broker::should_show(id)
}

pub fn is_pending(id: u64) -> bool {
    broker::is_pending(id)
}

#[allow(dead_code)]
pub fn pending_count() -> usize {
    broker::pending_count()
}

pub fn request_position(id: u64) -> (usize, usize) {
    broker::request_position(id)
}

pub fn resolve(id: u64, choice: ApprovalChoice) -> bool {
    broker::resolve(id, choice)
}

pub fn enqueue_demo() -> bool {
    enqueue(
        "演示".into(),
        process::id(),
        std::env::current_dir()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
            .unwrap_or_default(),
        "git status".into(),
        "请求执行：git status（这是演示请求，不会执行任何命令）".into(),
        false,
        false,
        0,
        false,
        false,
        true,
        PopupKind::Confirmation,
        None,
    )
    .is_some()
}

pub fn enqueue_notice_demo() -> bool {
    let pid = process::id();
    let success = broker::enqueue_notice(
        "Codex".into(),
        pid,
        PopupKind::Success,
        "AI 回复完成".into(),
        "本地演示：本轮任务已完成".into(),
    );
    let error = broker::enqueue_notice(
        "Codex".into(),
        pid,
        PopupKind::Error,
        "AI 回复失败".into(),
        "本地演示：模拟 429 Too Many Requests".into(),
    );
    success && error
}

#[cfg(test)]
#[path = "approval/tests.rs"]
mod tests;
