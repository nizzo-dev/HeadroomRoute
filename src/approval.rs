#![cfg(windows)]

use anyhow::{Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::io::{FromRawHandle, RawHandle},
    process::{self, Command, Stdio},
    ptr,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError,
        HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    },
    Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING, WriteFile},
    System::{
        Console::{
            CONSOLE_SCREEN_BUFFER_INFO, COORD, ClosePseudoConsole, CreatePseudoConsole,
            GetConsoleScreenBufferInfo, GetStdHandle, HPCON, ResizePseudoConsole,
            STD_OUTPUT_HANDLE,
        },
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PIPE_READMODE_MESSAGE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT, WaitNamedPipeW,
        },
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
            OpenProcessToken, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW,
            UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
    UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetForegroundWindow, PostMessageW, WM_APP},
};

#[path = "approval/request_data.rs"]
mod request_data;
#[path = "approval/terminal.rs"]
mod terminal;
#[path = "approval/turn.rs"]
mod turn;
pub use terminal::run_cli_command;
use terminal::*;
use turn::{
    TurnResult, classify_turn_result, cli_input_prompt_ready, completion_bullet_visible,
    notify_turn_result,
};

pub use request_data::{ApprovalChoice, ApprovalRequest, PopupKind};
use request_data::{
    ApprovalDecision, ConfirmationPrompt, WireRequest, WireResponse, clamp_text,
    confirmation_prompt, valid_wire_request, write_reason, write_response,
};
#[cfg(test)]
use request_data::{confirmation_answers, prompt_summary, strip_ansi};

pub const WM_APPROVAL: u32 = WM_APP + 7;

const PIPE_NAME: &str = r"\\.\pipe\HeadroomRouteApproval-v1";
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_PENDING_REQUESTS: usize = 32;
const CHILD_SCREEN_WIDTH: i16 = 120;
const CHILD_SCREEN_HEIGHT: i16 = 40;
const GENERIC_READ_FLAG: u32 = 0x8000_0000;

#[allow(dead_code)] // Used by the CLI binary; this module is also compiled into the tray binary.
pub fn run_codex_notify(args: &[String]) -> Result<()> {
    turn::run_codex_notify(args)
}
const GENERIC_WRITE_FLAG: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX_FLAG: u32 = 0x0000_0003;
// 给终端短暂的焦点切换时间，避免后台请求在瞬时失焦时立刻抢占；不再等待 3 秒。
const BACKGROUND_REMINDER_DELAY: Duration = Duration::from_millis(350);

struct Waiter {
    result: Mutex<Option<ApprovalDecision>>,
    ready: Condvar,
}

impl Waiter {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn resolve(&self, decision: ApprovalDecision) {
        *self.result.lock().unwrap() = Some(decision);
        self.ready.notify_one();
    }

    fn wait(&self) -> ApprovalDecision {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.ready.wait(result).unwrap();
        }
        result.expect("approval waiter must be resolved")
    }
}

struct PendingRequest {
    request: ApprovalRequest,
    waiter: Option<Arc<Waiter>>,
    background_since: Option<Instant>,
}

struct BrokerState {
    pending: HashMap<u64, PendingRequest>,
    queue: VecDeque<u64>,
}

struct Broker {
    next_id: AtomicU64,
    tray_hwnd: AtomicIsize,
    server_started: AtomicBool,
    state: Mutex<BrokerState>,
}

static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();
static PIPE_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

fn broker() -> &'static Arc<Broker> {
    BROKER.get_or_init(|| {
        Arc::new(Broker {
            next_id: AtomicU64::new(1),
            tray_hwnd: AtomicIsize::new(0),
            server_started: AtomicBool::new(false),
            state: Mutex::new(BrokerState {
                pending: HashMap::new(),
                queue: VecDeque::new(),
            }),
        })
    })
}

pub fn start_server() {
    let broker = broker().clone();
    if broker.server_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = thread::Builder::new()
        .name("headroom-approval-pipe".into())
        .spawn(move || pipe_server(broker));
}

pub fn ensure_server() -> bool {
    if approval_pipe_available() {
        return true;
    }
    let Some(executable) = approval_host_executable() else {
        return false;
    };
    let _ = Command::new(executable)
        .arg("--approval-host")
        .arg(format!("--parent-pid={}", process::id()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    for _ in 0..100 {
        if approval_pipe_available() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn approval_host_executable() -> Option<std::path::PathBuf> {
    let current = std::env::current_exe().ok()?;
    let is_cli = current
        .file_stem()
        .and_then(|name| name.to_str())
        .is_some_and(is_cli_executable_name);
    if !is_cli {
        return Some(current);
    }
    let installed = current.with_file_name("HeadroomRoute.exe");
    if installed.is_file() {
        return Some(installed);
    }
    let portable =
        current.with_file_name(format!("HeadroomRoute-{}.exe", env!("CARGO_PKG_VERSION")));
    portable.is_file().then_some(portable)
}

fn is_cli_executable_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("HeadroomRouteCLI")
        || name
            .get(.."HeadroomRouteCLI-".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("HeadroomRouteCLI-"))
}

fn approval_pipe_available() -> bool {
    if unsafe { WaitNamedPipeW(wide(PIPE_NAME).as_ptr(), 0) } == 0 {
        return false;
    }

    let handle = unsafe {
        CreateFileW(
            wide(PIPE_NAME).as_ptr(),
            GENERIC_READ_FLAG | GENERIC_WRITE_FLAG,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    unsafe { CloseHandle(handle) };
    true
}

pub fn set_tray_hwnd(hwnd: windows_sys::Win32::Foundation::HWND) {
    broker().tray_hwnd.store(hwnd as isize, Ordering::Release);
    broker().notify_ui();
}

pub fn clear_tray_hwnd(hwnd: windows_sys::Win32::Foundation::HWND) {
    let _ =
        broker()
            .tray_hwnd
            .compare_exchange(hwnd as isize, 0, Ordering::AcqRel, Ordering::Relaxed);
}

pub fn next_request() -> Option<ApprovalRequest> {
    let broker = broker();
    let mut state = broker.state.lock().unwrap();
    let live = state
        .pending
        .keys()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    state.queue.retain(|id| live.contains(id));
    let ids = state.queue.iter().copied().collect::<Vec<_>>();
    for id in ids {
        if let Some(pending) = state.pending.get_mut(&id)
            && update_request_visibility(pending)
        {
            return Some(pending.request.clone());
        }
    }
    None
}

pub fn should_show(id: u64) -> bool {
    broker()
        .state
        .lock()
        .unwrap()
        .pending
        .get_mut(&id)
        .is_some_and(update_request_visibility)
}

fn update_request_visibility(pending: &mut PendingRequest) -> bool {
    if pending.request.demo || pending.request.popup_kind != PopupKind::Confirmation {
        return true;
    }
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_root = if foreground.is_null() {
        0
    } else {
        (unsafe { GetAncestor(foreground, GA_ROOT) }) as usize as u64
    };
    let background = pending.request.source_window == 0
        || foreground_root != pending.request.source_window
        || !pending.request.focus_known
        || !pending.request.focused;
    if !background {
        pending.background_since = None;
        return false;
    }
    let since = pending.background_since.get_or_insert_with(Instant::now);
    since.elapsed() >= BACKGROUND_REMINDER_DELAY
}

pub fn is_pending(id: u64) -> bool {
    broker().state.lock().unwrap().pending.contains_key(&id)
}

pub fn pending_count() -> usize {
    broker().state.lock().unwrap().pending.len()
}

pub fn request_position(id: u64) -> (usize, usize) {
    let state = broker().state.lock().unwrap();
    let live = state
        .queue
        .iter()
        .filter(|queued| state.pending.contains_key(queued))
        .copied()
        .collect::<Vec<_>>();
    let position = live
        .iter()
        .position(|queued| *queued == id)
        .map_or(1, |index| index + 1);
    (position, live.len().max(1))
}

pub fn resolve(id: u64, choice: ApprovalChoice) -> bool {
    let pending = broker().state.lock().unwrap().pending.remove(&id);
    let Some(pending) = pending else { return false };
    if let Some(waiter) = pending.waiter {
        waiter.resolve(match choice {
            ApprovalChoice::Deny => ApprovalDecision::Denied,
            ApprovalChoice::AllowOnce => ApprovalDecision::Approved,
            ApprovalChoice::AllowRule => ApprovalDecision::ApprovedAlways,
            ApprovalChoice::Feedback => ApprovalDecision::Feedback,
        });
    }
    broker().notify_ui();
    true
}

fn cancel_pid(pid: u32) -> usize {
    let broker = broker();
    let cancelled = {
        let mut state = broker.state.lock().unwrap();
        let ids = state
            .pending
            .iter()
            .filter_map(|(id, pending)| (pending.request.pid == pid).then_some(*id))
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| state.pending.remove(&id))
            .collect::<Vec<_>>()
    };
    let count = cancelled.len();
    for pending in cancelled {
        if let Some(waiter) = pending.waiter {
            waiter.resolve(ApprovalDecision::Cancelled);
        }
    }
    if count > 0 {
        broker.notify_ui();
    }
    count
}

fn update_pid_focus(pid: u32, focused: bool) -> usize {
    let broker = broker();
    let mut state = broker.state.lock().unwrap();
    let mut count = 0;
    for pending in state.pending.values_mut() {
        if pending.request.pid == pid {
            pending.request.focus_known = true;
            pending.request.focused = focused;
            pending.background_since = None;
            count += 1;
        }
    }
    drop(state);
    if count > 0 {
        broker.notify_ui();
    }
    count
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
    let success = enqueue_notice(
        "Codex".into(),
        pid,
        PopupKind::Success,
        "AI 回复完成".into(),
        "本地演示：本轮任务已完成".into(),
    );
    let error = enqueue_notice(
        "Codex".into(),
        pid,
        PopupKind::Error,
        "AI 回复失败".into(),
        "本地演示：模拟 429 Too Many Requests".into(),
    );
    success && error
}

fn enqueue_notice(
    cli: String,
    pid: u32,
    popup_kind: PopupKind,
    action: String,
    summary: String,
) -> bool {
    debug_assert!(popup_kind != PopupKind::Confirmation);
    enqueue(
        cli,
        pid,
        String::new(),
        action,
        summary,
        false,
        false,
        0,
        false,
        false,
        false,
        popup_kind,
        None,
    )
    .is_some()
}

fn request_approval(
    cli: &str,
    pid: u32,
    cwd: &str,
    prompt: &ConfirmationPrompt,
    source_window: u64,
    focus_known: bool,
    focused: bool,
) -> ApprovalDecision {
    approval_trace("requesting popup decision");
    let mut stream = match connect_pipe() {
        Ok(stream) => stream,
        Err(error) => {
            if !PIPE_WARNING_SHOWN.swap(true, Ordering::AcqRel) {
                eprintln!(
                    "HeadroomRoute：确认悬浮窗未连接，已安全取消请求。请先启动 HeadroomRoute（{error:#}）"
                );
            }
            return ApprovalDecision::Cancelled;
        }
    };
    let payload = WireRequest {
        kind: "approval_request".into(),
        cli: clamp_text(cli, 32),
        pid,
        cwd: clamp_text(cwd, 260),
        action: clamp_text(&prompt.action, 300),
        summary: clamp_text(&prompt.summary, 900),
        allow_rule: prompt.allow_rule_answer.is_some(),
        feedback: prompt.feedback_answer.is_some(),
        source_window,
        focus_known,
        focused,
        demo: false,
    };
    let Ok(mut body) = serde_json::to_vec(&payload) else {
        return ApprovalDecision::Cancelled;
    };
    body.push(b'\n');
    if stream.write_all(&body).is_err() || stream.flush().is_err() {
        return ApprovalDecision::Cancelled;
    }
    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    if (&mut reader)
        .take(MAX_MESSAGE_BYTES as u64)
        .read_line(&mut response)
        .is_err()
    {
        return ApprovalDecision::Cancelled;
    }
    let decision = serde_json::from_str::<WireResponse>(&response)
        .map(|response| match response.reason.as_str() {
            "approved" if response.approved => ApprovalDecision::Approved,
            "denied" => ApprovalDecision::Denied,
            "approved_always" if response.approved => ApprovalDecision::ApprovedAlways,
            "feedback" => ApprovalDecision::Feedback,
            _ => ApprovalDecision::Cancelled,
        })
        .unwrap_or(ApprovalDecision::Cancelled);
    approval_trace(&format!("popup decision: {decision:?}"));
    decision
}

#[allow(clippy::too_many_arguments)]
fn enqueue(
    cli: String,
    pid: u32,
    cwd: String,
    action: String,
    summary: String,
    allow_rule: bool,
    feedback: bool,
    source_window: u64,
    focus_known: bool,
    focused: bool,
    demo: bool,
    popup_kind: PopupKind,
    waiter: Option<Arc<Waiter>>,
) -> Option<ApprovalRequest> {
    let broker = broker();
    let mut state = broker.state.lock().unwrap();
    if state.pending.len() >= MAX_PENDING_REQUESTS {
        if let Some(waiter) = waiter {
            waiter.resolve(ApprovalDecision::Cancelled);
        }
        return None;
    }
    let request = ApprovalRequest {
        id: broker.next_id.fetch_add(1, Ordering::Relaxed),
        popup_kind,
        cli: clamp_text(&cli, 32),
        pid,
        cwd: clamp_text(&cwd, 260),
        action: clamp_text(&action, 300),
        summary: clamp_text(&summary, 900),
        allow_rule,
        feedback,
        source_window,
        focus_known,
        focused,
        demo,
    };
    state.queue.push_back(request.id);
    state.pending.insert(
        request.id,
        PendingRequest {
            request: request.clone(),
            waiter,
            background_since: None,
        },
    );
    drop(state);
    broker.notify_ui();
    Some(request)
}

impl Broker {
    fn notify_ui(&self) {
        let hwnd = self.tray_hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe {
                PostMessageW(
                    hwnd as windows_sys::Win32::Foundation::HWND,
                    WM_APPROVAL,
                    0,
                    0,
                );
            }
        }
    }
}

fn pipe_server(_broker: Arc<Broker>) {
    loop {
        let pipe = create_pipe_instance();
        if pipe == INVALID_HANDLE_VALUE {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) } != 0;
        if !connected && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            unsafe { CloseHandle(pipe) };
            continue;
        }
        let pipe_value = pipe as isize;
        let _ = thread::Builder::new()
            .name("headroom-approval-client".into())
            .spawn(move || handle_pipe(pipe_value));
    }
}

fn create_pipe_instance() -> HANDLE {
    let descriptor = pipe_security_descriptor();
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let pipe = unsafe {
        CreateNamedPipeW(
            wide(PIPE_NAME).as_ptr(),
            PIPE_ACCESS_DUPLEX_FLAG,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            MAX_PENDING_REQUESTS as u32,
            MAX_MESSAGE_BYTES as u32,
            MAX_MESSAGE_BYTES as u32,
            0,
            if descriptor.is_null() {
                ptr::null()
            } else {
                &security
            },
        )
    };
    if !descriptor.is_null() {
        unsafe { LocalFree(descriptor as HLOCAL) };
    }
    if pipe != INVALID_HANDLE_VALUE {
        return pipe;
    }

    unsafe {
        CreateNamedPipeW(
            wide(PIPE_NAME).as_ptr(),
            PIPE_ACCESS_DUPLEX_FLAG,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            MAX_PENDING_REQUESTS as u32,
            MAX_MESSAGE_BYTES as u32,
            MAX_MESSAGE_BYTES as u32,
            0,
            ptr::null(),
        )
    }
}

fn pipe_security_descriptor() -> PSECURITY_DESCRIPTOR {
    let sid = current_user_sid().unwrap_or_else(|| "IU".into());
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut size = 0u32;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(&sddl).as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            &mut size,
        )
    } != 0;
    if converted {
        descriptor
    } else {
        ptr::null_mut()
    }
}

fn current_user_sid() -> Option<String> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let mut required = 0u32;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        unsafe { CloseHandle(token) };
        return None;
    }
    let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; word_count];
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } != 0;
    if !read {
        unsafe { CloseHandle(token) };
        return None;
    }
    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_text = ptr::null_mut();
    let converted = unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) } != 0;
    unsafe { CloseHandle(token) };
    if !converted || sid_text.is_null() {
        return None;
    }
    let mut length = 0usize;
    unsafe {
        while *sid_text.add(length) != 0 {
            length += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
    unsafe { LocalFree(sid_text as HLOCAL) };
    Some(sid)
}

fn handle_pipe(pipe: isize) {
    let pipe = pipe as HANDLE;
    let file = unsafe { File::from_raw_handle(pipe as RawHandle) };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let read_result = (&mut reader)
        .take(MAX_MESSAGE_BYTES as u64)
        .read_line(&mut line);
    let mut stream = reader.into_inner();
    let request = read_result
        .ok()
        .filter(|bytes| *bytes > 0)
        .and_then(|_| serde_json::from_str::<WireRequest>(&line).ok());
    let Some(request) = request.filter(valid_wire_request) else {
        let _ = write_reason(&mut stream, false, "invalid_request");
        return;
    };
    if request.kind == "cancel_pid" {
        cancel_pid(request.pid);
        let _ = write_reason(&mut stream, false, "cancelled");
        return;
    }
    if request.kind == "focus_update" {
        update_pid_focus(request.pid, request.focused);
        let _ = write_reason(&mut stream, false, "focus_updated");
        return;
    }
    if request.kind == "turn_completed" || request.kind == "turn_failed" {
        let cli = if request.cli == "codex" {
            "Codex"
        } else {
            "Claude"
        };
        let (popup_kind, action, summary) = if request.kind == "turn_failed" {
            (
                PopupKind::Error,
                "AI 回复失败",
                format!("{cli}：{}", clamp_text(&request.summary, 300)),
            )
        } else {
            (
                PopupKind::Success,
                "AI 回复完成",
                format!("{cli} 本轮任务已完成"),
            )
        };
        let queued = enqueue_notice(request.cli, request.pid, popup_kind, action.into(), summary);
        let reason = if queued { &request.kind } else { "queue_full" };
        let _ = write_reason(&mut stream, false, reason);
        return;
    }
    let waiter = Arc::new(Waiter::new());
    let Some(enqueued) = enqueue(
        request.cli,
        request.pid,
        request.cwd,
        request.action,
        request.summary,
        request.allow_rule,
        request.feedback,
        request.source_window,
        request.focus_known,
        request.focused,
        request.demo,
        PopupKind::Confirmation,
        Some(waiter.clone()),
    ) else {
        let _ = write_reason(&mut stream, false, "queue_full");
        return;
    };
    let decision = waiter.wait();
    let removed = broker().state.lock().unwrap().pending.remove(&enqueued.id);
    if removed.is_some() {
        broker().notify_ui();
    }
    let _ = write_response(&mut stream, decision);
}

fn cancel_remote_requests(pid: u32) {
    let Ok(mut stream) = connect_pipe() else {
        return;
    };
    let payload = WireRequest {
        kind: "cancel_pid".into(),
        cli: String::new(),
        pid,
        cwd: String::new(),
        action: String::new(),
        summary: String::new(),
        allow_rule: false,
        feedback: false,
        source_window: 0,
        focus_known: false,
        focused: false,
        demo: false,
    };
    let Ok(mut body) = serde_json::to_vec(&payload) else {
        return;
    };
    body.push(b'\n');
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn connect_pipe() -> Result<File> {
    for _ in 0..20 {
        let handle = unsafe {
            CreateFileW(
                wide(PIPE_NAME).as_ptr(),
                GENERIC_READ_FLAG | GENERIC_WRITE_FLAG,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_PIPE_BUSY && error != ERROR_FILE_NOT_FOUND {
            break;
        }
        unsafe {
            WaitNamedPipeW(wide(PIPE_NAME).as_ptr(), 50);
        }
    }
    anyhow::bail!("HeadroomRoute 确认管道不可用")
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalDecision, ConfirmationPrompt, InputSink, TerminalScreen, build_command_line,
        confirmation_answers, confirmation_prompt, connect_pipe, is_cli_executable_name,
        prompt_summary, quote_cmd_arg, start_server, strip_ansi,
    };
    use std::{
        io::{BufRead, BufReader, Write},
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64},
        },
        time::Duration,
    };
    use windows_sys::Win32::System::Console::COORD;

    #[test]
    fn detects_confirmation_prompt_but_not_normal_output() {
        assert!(
            confirmation_prompt(
                "codex",
                "Would you like to allow this command to run? Yes / No"
            )
            .is_some()
        );
        assert!(confirmation_prompt("codex", "Build completed successfully").is_none());
    }

    #[test]
    fn ignores_generic_questions_without_an_explicit_permission_marker() {
        assert!(confirmation_prompt("codex", "Should we proceed? Yes / No").is_none());
        assert!(confirmation_prompt("claude", "The answer is yes or no").is_none());
    }

    #[test]
    fn leaves_workspace_trust_prompt_to_native_cli_input() {
        let prompt = "Accessing workspace: C:\\Users\\HD Quick safety check: Is this a project you created or one you trust? Claude Code'll be able to read, edit, and execute files here. Security guide 1. Yes, I trust this folder 2. No, exit";
        assert!(confirmation_prompt("claude", prompt).is_none());
    }

    #[test]
    fn strips_terminal_control_sequences() {
        assert_eq!(strip_ansi("\x1b[32mAllow?\x1b[0m\r\n"), "Allow?\n\n");
    }

    #[test]
    fn bounds_prompt_summary() {
        let prompt = confirmation_prompt(
            "codex",
            &format!(
                "{} Would you like to allow this command? Yes No",
                "x".repeat(600)
            ),
        )
        .unwrap();
        assert!(prompt.summary.chars().count() <= 420);
    }

    #[test]
    fn summarizes_the_visible_prompt_without_working_status() {
        let summary = prompt_summary(
            "Working (12s • esc to interrupt)\r\nWould you like to allow this command?\r\nYes / No\r\n",
        );
        assert!(!summary.contains("Working"));
        assert!(summary.contains("Would you like to allow"));
    }

    #[test]
    fn detects_confirmation_from_the_rendered_terminal_screen() {
        let mut terminal = TerminalScreen::new(COORD { X: 120, Y: 40 });
        terminal.process(
            b"\x1b[?9001h\x1b[?1004hWould you like to allow this command to run?\r\n1. Yes\r\n2. No\r\n",
        );
        let screen = terminal.contents();
        assert!(
            confirmation_prompt("codex", &screen).is_some(),
            "rendered screen: {screen:?}"
        );
    }

    #[test]
    fn terminal_input_cancels_an_active_popup_without_injecting_an_answer() {
        let sink = InputSink {
            file: Mutex::new(None),
            next_approval_token: AtomicU64::new(1),
            active_approval_token: AtomicU64::new(0),
            pid: 1,
            source_window: 0,
            focus_known: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            turn_pending: AtomicBool::new(false),
            turn_activity_seen: AtomicBool::new(false),
            turn_prompt_left: AtomicBool::new(false),
            turn_prompt_returned: AtomicBool::new(false),
            turn_completion_armed: AtomicBool::new(false),
            turn_input_has_text: AtomicBool::new(false),
        };
        let prompt = ConfirmationPrompt {
            action: "git status".into(),
            summary: "Would you like to allow this command? Yes / No".into(),
            approve_answer: "y\n",
            allow_rule_answer: None,
            feedback_answer: None,
            deny_answer: "n\n",
        };
        let token = sink.begin_approval();
        assert_ne!(token, 0);
        sink.finish_approval(token, ApprovalDecision::Cancelled, &prompt);
        assert_eq!(
            sink.active_approval_token
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    #[test]
    fn ignores_confirmation_text_that_is_outside_the_recent_terminal_tail() {
        let output = format!(
            "Would you like to allow this command? Yes / No {}",
            "normal output ".repeat(140)
        );
        assert!(confirmation_prompt("codex", &output).is_none());
    }

    #[test]
    fn selects_first_or_last_numbered_permission_option() {
        let prompt = "1. Yes, allow once 2. Yes always 3. No";
        assert_eq!(
            confirmation_answers(prompt),
            ("1\n", Some("2\n"), None, "3\n")
        );
        assert_eq!(
            confirmation_answers("Proceed? (y/n)"),
            ("y\n", None, None, "n\n")
        );
    }

    #[test]
    fn exposes_native_allow_rule_and_feedback_answers() {
        let answers = confirmation_answers(
            "1. Yes, allow once 2. Yes, and don't ask again 3. No, and tell Codex what to do differently",
        );
        assert_eq!(answers, ("1\n", Some("2\n"), Some("3\n"), "3\n"));
    }

    #[test]
    fn extracts_command_from_confirmation_prompt() {
        let prompt = confirmation_prompt(
            "claude",
            "Claude needs permission\r\n> cargo test\r\nProceed? Yes / No",
        )
        .unwrap();
        assert_eq!(prompt.action, "cargo test");
    }

    #[test]
    fn quotes_cli_arguments_for_cmd() {
        assert_eq!(quote_cmd_arg("codex"), "codex");
        assert_eq!(quote_cmd_arg("hello world"), "\"hello world\"");
        assert!(
            build_command_line("claude", &["--model".into(), "sonnet 4".into()])
                .contains("claude --model \"sonnet 4\"")
        );
    }

    #[test]
    fn recognizes_installed_and_versioned_cli_names() {
        assert!(is_cli_executable_name("HeadroomRouteCLI"));
        assert!(is_cli_executable_name("headroomroutecli-0.6.9"));
        assert!(!is_cli_executable_name("HeadroomRoute"));
    }

    #[test]
    fn local_pipe_accepts_current_user_and_rejects_invalid_payload() {
        start_server();
        std::thread::sleep(Duration::from_millis(100));
        let mut stream = connect_pipe().expect("approval pipe should be available");
        stream.write_all(b"{}\n").unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        assert!(response.contains("invalid_request"));
    }
}
