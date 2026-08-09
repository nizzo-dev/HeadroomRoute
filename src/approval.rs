#![cfg(windows)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

pub const WM_APPROVAL: u32 = WM_APP + 7;

const PIPE_NAME: &str = r"\\.\pipe\HeadroomRouteApproval-v1";
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_PENDING_REQUESTS: usize = 32;
const CHILD_SCREEN_WIDTH: i16 = 120;
const CHILD_SCREEN_HEIGHT: i16 = 40;
const GENERIC_READ_FLAG: u32 = 0x8000_0000;
const GENERIC_WRITE_FLAG: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX_FLAG: u32 = 0x0000_0003;
// 给终端短暂的焦点切换时间，避免后台请求在瞬时失焦时立刻抢占；不再等待 3 秒。
const BACKGROUND_REMINDER_DELAY: Duration = Duration::from_millis(350);

#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub id: u64,
    pub cli: String,
    pub pid: u32,
    pub cwd: String,
    pub action: String,
    pub summary: String,
    pub allow_rule: bool,
    pub feedback: bool,
    pub source_window: u64,
    pub focus_known: bool,
    pub focused: bool,
    pub demo: bool,
}

impl ConfirmationPrompt {
    fn dedupe_key(&self) -> &str {
        if self.action.ends_with("请求执行一项操作") {
            &self.summary
        } else {
            &self.action
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct WireRequest {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    cli: String,
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    allow_rule: bool,
    #[serde(default)]
    feedback: bool,
    #[serde(default)]
    source_window: u64,
    #[serde(default)]
    focus_known: bool,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    demo: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfirmationPrompt {
    action: String,
    summary: String,
    approve_answer: &'static str,
    allow_rule_answer: Option<&'static str>,
    feedback_answer: Option<&'static str>,
    deny_answer: &'static str,
}

#[derive(Debug, Deserialize, Serialize)]
struct WireResponse {
    approved: bool,
    reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalDecision {
    Approved,
    ApprovedAlways,
    Feedback,
    Denied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalChoice {
    Deny,
    AllowOnce,
    AllowRule,
    Feedback,
}

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
    if pending.request.demo {
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

fn valid_wire_request(request: &WireRequest) -> bool {
    ((request.kind == "cancel_pid" || request.kind == "focus_update") && request.pid > 0)
        || (request.kind == "approval_request"
            && !request.cli.trim().is_empty()
            && request.pid > 0
            && !request.action.trim().is_empty()
            && !request.summary.trim().is_empty())
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

fn write_response(stream: &mut File, decision: ApprovalDecision) -> io::Result<()> {
    let (approved, reason) = match decision {
        ApprovalDecision::Approved => (true, "approved"),
        ApprovalDecision::ApprovedAlways => (true, "approved_always"),
        ApprovalDecision::Feedback => (false, "feedback"),
        ApprovalDecision::Denied => (false, "denied"),
        ApprovalDecision::Cancelled => (false, "cancelled"),
    };
    write_reason(stream, approved, reason)
}

fn write_reason(stream: &mut File, approved: bool, reason: &str) -> io::Result<()> {
    let mut body = serde_json::to_vec(&WireResponse {
        approved,
        reason: reason.into(),
    })
    .map_err(io::Error::other)?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()
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

struct InputSink {
    file: Mutex<Option<File>>,
    next_approval_token: AtomicU64,
    active_approval_token: AtomicU64,
    pid: u32,
    source_window: u64,
    focus_known: AtomicBool,
    focused: AtomicBool,
}

impl InputSink {
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().unwrap();
        let Some(file) = file.as_mut() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        file.flush()
    }

    fn begin_approval(&self) -> u64 {
        let token = self
            .next_approval_token
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        match self.active_approval_token.compare_exchange(
            0,
            token,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => token,
            Err(_) => 0,
        }
    }

    fn finish_approval(&self, token: u64, decision: ApprovalDecision, prompt: &ConfirmationPrompt) {
        if token == 0
            || self
                .active_approval_token
                .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let answer = match decision {
            ApprovalDecision::Approved => Some(prompt.approve_answer),
            ApprovalDecision::ApprovedAlways => prompt.allow_rule_answer,
            ApprovalDecision::Feedback => prompt.feedback_answer,
            ApprovalDecision::Denied => Some(prompt.deny_answer),
            ApprovalDecision::Cancelled => None,
        };
        if let Some(answer) = answer {
            let _ = self.write(answer.as_bytes());
        }
    }

    fn write_user_input(&self, bytes: &[u8]) -> io::Result<()> {
        if self.active_approval_token.swap(0, Ordering::AcqRel) != 0 {
            let pid = self.pid;
            let _ = thread::Builder::new()
                .name("headroom-approval-cancel".into())
                .spawn(move || cancel_remote_requests(pid));
        }
        self.write(bytes)
    }

    fn observe_focus(&self, bytes: &[u8]) -> Option<bool> {
        let focused = match bytes {
            b"\x1b[I" => true,
            b"\x1b[O" => false,
            _ => return None,
        };
        self.focus_known.store(true, Ordering::Release);
        self.focused.store(focused, Ordering::Release);
        let pid = self.pid;
        let _ = thread::Builder::new()
            .name("headroom-focus-update".into())
            .spawn(move || update_remote_focus(pid, focused));
        Some(focused)
    }

    fn close(&self) {
        self.file.lock().unwrap().take();
    }
}

struct SpawnedConsole {
    process: HANDLE,
    console: HPCON,
    input: File,
    output: File,
    pid: u32,
}

pub fn run_cli_command(args: &[String]) -> Result<i32> {
    let cli = args
        .first()
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("用法：HeadroomRouteCLI.exe claude|codex [参数...]"))?;
    if cli != "claude" && cli != "codex" {
        anyhow::bail!("不支持的 CLI：{cli}；可用值为 claude 或 codex");
    }
    if !ensure_server() {
        eprintln!("HeadroomRoute：确认托盘未能启动，确认请求将自动取消");
    }
    let source_window = foreground_root_window();
    let cwd = std::env::current_dir().context("无法读取当前工作目录")?;
    let command_line = build_command_line(&cli, &args[1..]);
    let spawned = SpawnedConsole::spawn(&command_line, cwd.to_string_lossy().as_ref())?;
    let child_pid = spawned.pid;
    let input = Arc::new(InputSink {
        file: Mutex::new(Some(spawned.input)),
        next_approval_token: AtomicU64::new(1),
        active_approval_token: AtomicU64::new(0),
        pid: child_pid,
        source_window,
        focus_known: AtomicBool::new(source_window != 0),
        focused: AtomicBool::new(source_window != 0),
    });
    let reader_input = input.clone();
    let reader_cli = cli.clone();
    let reader_cwd = cwd.to_string_lossy().into_owned();
    let output_thread = thread::Builder::new()
        .name("headroom-cli-output".into())
        .spawn(move || {
            read_cli_output(
                spawned.output,
                reader_input,
                reader_cli,
                child_pid,
                reader_cwd,
            )
        })
        .context("无法启动 CLI 输出线程")?;

    let resize_stop = Arc::new(AtomicBool::new(false));
    let resize_stop_thread = resize_stop.clone();
    let resize_thread = thread::Builder::new()
        .name("headroom-cli-resize".into())
        .spawn(move || resize_pseudo_console_loop(spawned.console, resize_stop_thread))
        .ok();

    let _ = write_cli_output(b"\x1b[?1004h");
    let stdin_input = input.clone();
    let _ = thread::Builder::new()
        .name("headroom-cli-input".into())
        .spawn(move || forward_stdin(stdin_input));

    unsafe {
        WaitForSingleObject(spawned.process, INFINITE);
    }
    resize_stop.store(true, Ordering::Release);
    if let Some(thread) = resize_thread {
        let _ = thread.join();
    }
    cancel_remote_requests(child_pid);
    input.close();
    let mut exit_code = 1u32;
    unsafe {
        let _ = GetExitCodeProcess(spawned.process, &mut exit_code);
        CloseHandle(spawned.process);
        // Keep the reader alive while ConPTY flushes terminal cleanup, then
        // let it observe EOF before returning control to the parent shell.
        ClosePseudoConsole(spawned.console);
    }
    let output_error = match output_thread.join() {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some(anyhow::anyhow!("CLI 输出线程异常退出")),
    };
    if let Some(error) = output_error {
        let _ = write_cli_output(b"\x1b[?1004l");
        return Err(error.context("转发 CLI 输出失败"));
    }
    let _ = write_cli_output(b"\x1b[?1004l");
    Ok((exit_code & 0xff) as i32)
}

fn read_cli_output(
    mut output: File,
    input: Arc<InputSink>,
    cli: String,
    pid: u32,
    cwd: String,
) -> Result<()> {
    let mut buffer = [0u8; 4096];
    let mut terminal = TerminalScreen::new(parent_console_size());
    loop {
        terminal.resize_if_needed();
        let read = output.read(&mut buffer).context("读取 CLI 输出失败")?;
        if read == 0 {
            break;
        }
        write_cli_output(&buffer[..read])?;
        terminal.process(&buffer[..read]);
        let screen = terminal.contents();
        if let Some(prompt) = confirmation_prompt(&cli, &screen) {
            let dedupe_key = prompt.dedupe_key().to_owned();
            let should_request = terminal.last_prompt_key.as_ref() != Some(&dedupe_key);
            if should_request {
                approval_trace(&format!("visible confirmation detected: {dedupe_key}"));
                terminal.last_prompt_key = Some(dedupe_key);
                let token = input.begin_approval();
                if token != 0 {
                    let approval_input = input.clone();
                    let approval_cli = cli.clone();
                    let approval_cwd = cwd.clone();
                    let approval_prompt = prompt.clone();
                    let spawned = thread::Builder::new()
                        .name("headroom-approval-request".into())
                        .spawn(move || {
                            let decision = request_approval(
                                &approval_cli,
                                pid,
                                &approval_cwd,
                                &approval_prompt,
                                approval_input.source_window,
                                approval_input.focus_known.load(Ordering::Acquire),
                                approval_input.focused.load(Ordering::Acquire),
                            );
                            approval_input.finish_approval(token, decision, &approval_prompt);
                        });
                    if spawned.is_err() {
                        let _ = input.active_approval_token.compare_exchange(
                            token,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            }
        } else {
            terminal.last_prompt_key = None;
        }
    }
    Ok(())
}

fn approval_trace(message: &str) {
    if std::env::var_os("HEADROOM_ROUTE_APPROVAL_TRACE").is_some() {
        eprintln!("HeadroomRoute approval trace: {message}");
    }
}

struct TerminalScreen {
    parser: vt100::Parser,
    size: COORD,
    last_prompt_key: Option<String>,
}

impl TerminalScreen {
    fn new(size: COORD) -> Self {
        let size = normalize_console_size(size);
        Self {
            parser: vt100::Parser::new(size.Y as u16, size.X as u16, 0),
            size,
            last_prompt_key: None,
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn resize_if_needed(&mut self) {
        let size = normalize_console_size(parent_console_size());
        if size.X != self.size.X || size.Y != self.size.Y {
            self.parser = vt100::Parser::new(size.Y as u16, size.X as u16, 0);
            self.size = size;
            self.last_prompt_key = None;
        }
    }
}

fn normalize_console_size(size: COORD) -> COORD {
    COORD {
        X: size.X.max(1),
        Y: size.Y.max(1),
    }
}

fn resize_pseudo_console_loop(console: HPCON, stop: Arc<AtomicBool>) {
    let mut previous = parent_console_size();
    while !stop.load(Ordering::Acquire) {
        let current = parent_console_size();
        if (current.X != previous.X || current.Y != previous.Y)
            && unsafe { ResizePseudoConsole(console, current) } >= 0
        {
            previous = current;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn write_cli_output(bytes: &[u8]) -> io::Result<()> {
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut offset = 0usize;
    while offset < bytes.len() {
        let remaining = (bytes.len() - offset).min(u32::MAX as usize) as u32;
        let mut written = 0u32;
        let result = unsafe {
            WriteFile(
                handle,
                bytes[offset..].as_ptr().cast(),
                remaining,
                &mut written,
                ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "CLI 输出句柄未写入数据",
            ));
        }
        offset += written as usize;
    }
    Ok(())
}

fn forward_stdin(input: Arc<InputSink>) {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buffer = [0u8; 1024];
    loop {
        let read = match stdin.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => return,
        };
        if read == 0 {
            return;
        }
        let bytes = &buffer[..read];
        let result = if input.observe_focus(bytes).is_some() {
            input.write(bytes)
        } else {
            input.write_user_input(bytes)
        };
        if result.is_err() {
            return;
        }
    }
}

impl SpawnedConsole {
    fn spawn(command_line: &str, cwd: &str) -> Result<Self> {
        let (input_read, input_write) = create_pipe()?;
        let (output_read, output_write) = match create_pipe() {
            Ok(pipes) => pipes,
            Err(error) => {
                unsafe { CloseHandle(input_read) };
                unsafe { CloseHandle(input_write) };
                return Err(error);
            }
        };
        let mut console: HPCON = 0;
        let size = parent_console_size();
        let result =
            unsafe { CreatePseudoConsole(size, input_read, output_write, 0, &mut console) };
        unsafe {
            CloseHandle(input_read);
            CloseHandle(output_write);
        }
        if result < 0 {
            unsafe {
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            anyhow::bail!("无法创建 Windows ConPTY（HRESULT 0x{result:08x}）");
        }

        let mut attribute_size = 0usize;
        unsafe {
            let _ = InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attribute_size);
        }
        if attribute_size == 0 {
            unsafe {
                ClosePseudoConsole(console);
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            anyhow::bail!("无法准备 ConPTY 进程属性");
        }
        let mut attribute_storage = vec![0u8; attribute_size];
        let attribute_list = attribute_storage.as_mut_ptr().cast();
        let initialized =
            unsafe { InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) }
                != 0;
        if !initialized {
            unsafe {
                ClosePseudoConsole(console);
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            anyhow::bail!("无法初始化 ConPTY 进程属性");
        }
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const c_void,
                std::mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } != 0;
        if !updated {
            unsafe {
                DeleteProcThreadAttributeList(attribute_list);
                ClosePseudoConsole(console);
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            anyhow::bail!("无法绑定 ConPTY 到 CLI 进程");
        }

        let mut startup = unsafe { std::mem::zeroed::<STARTUPINFOEXW>() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attribute_list;
        let mut command = wide(command_line);
        let mut process_info = unsafe { std::mem::zeroed() };
        let directory = wide(cwd);
        let created = unsafe {
            CreateProcessW(
                ptr::null(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                ptr::null(),
                directory.as_ptr(),
                &startup.StartupInfo,
                &mut process_info,
            )
        } != 0;
        unsafe { DeleteProcThreadAttributeList(attribute_list) };
        if !created {
            let error = unsafe { GetLastError() };
            unsafe {
                ClosePseudoConsole(console);
                CloseHandle(input_write);
                CloseHandle(output_read);
            }
            anyhow::bail!("无法启动 CLI（Windows 错误 {error}）");
        }
        unsafe { CloseHandle(process_info.hThread) };
        Ok(Self {
            process: process_info.hProcess,
            console,
            input: unsafe { File::from_raw_handle(input_write as RawHandle) },
            output: unsafe { File::from_raw_handle(output_read as RawHandle) },
            pid: process_info.dwProcessId,
        })
    }
}

fn parent_console_size() -> COORD {
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut info = unsafe { std::mem::zeroed::<CONSOLE_SCREEN_BUFFER_INFO>() };
    if !output.is_null()
        && output != INVALID_HANDLE_VALUE
        && unsafe { GetConsoleScreenBufferInfo(output, &mut info) } != 0
    {
        let width = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
        let height = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
        if width > 0 && height > 0 {
            return COORD {
                X: width.clamp(40, i16::MAX as i32) as i16,
                Y: height.clamp(10, i16::MAX as i32) as i16,
            };
        }
    }
    COORD {
        X: CHILD_SCREEN_WIDTH,
        Y: CHILD_SCREEN_HEIGHT,
    }
}

fn create_pipe() -> Result<(HANDLE, HANDLE)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    if unsafe { CreatePipe(&mut read, &mut write, &security, 0) } == 0 {
        anyhow::bail!("无法创建 ConPTY 管道");
    }
    Ok((read, write))
}

fn build_command_line(cli: &str, args: &[String]) -> String {
    let mut command = quote_cmd_arg(cli);
    for arg in args {
        command.push(' ');
        command.push_str(&quote_cmd_arg(arg));
    }
    format!("cmd.exe /d /s /c \"{command}\"")
}

fn quote_cmd_arg(value: &str) -> String {
    if value
        .chars()
        .all(|character| !character.is_whitespace() && !matches!(character, '"' | '^' | '&' | '|'))
    {
        return value.into();
    }
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn confirmation_prompt(cli: &str, text: &str) -> Option<ConfirmationPrompt> {
    let cleaned = strip_ansi(text);
    let mut normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = normalized.chars().count();
    if char_count > 1400 {
        normalized = normalized
            .chars()
            .skip(char_count - 1400)
            .collect::<String>();
    }
    let lower = normalized.to_ascii_lowercase();
    let onboarding_prompt = [
        "accessing workspace",
        "trust this folder",
        "trust this workspace",
        "quick safety check",
        "security guide",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if onboarding_prompt {
        return None;
    }
    let permission_marker = match cli {
        "codex" => [
            "would you like to run",
            "would you like to allow",
            "do you want to run",
            "allow this command",
            "approve this command",
            "approval required",
        ]
        .iter()
        .any(|marker| lower.contains(marker)),
        "claude" => [
            "do you want to proceed",
            "would you like to proceed",
            "needs your permission",
            "needs permission",
            "requires permission",
            "allow once",
            "allow always",
            "yes, allow",
        ]
        .iter()
        .any(|marker| lower.contains(marker)),
        _ => false,
    };
    let has_choice = [
        "deny",
        "reject",
        "cancel",
        "allow once",
        "yes",
        "y/n",
        "yes/no",
    ]
    .iter()
    .any(|word| lower.contains(word))
        && contains_word(&lower, "no");
    if !(permission_marker && (lower.contains('?') || lower.contains("y/n")) && has_choice) {
        return None;
    }
    let summary = prompt_summary(&cleaned);
    let (approve_answer, allow_rule_answer, feedback_answer, deny_answer) =
        confirmation_answers(&summary);
    Some(ConfirmationPrompt {
        action: extract_prompt_action(text, cli),
        summary,
        approve_answer,
        allow_rule_answer,
        feedback_answer,
        deny_answer,
    })
}

fn prompt_summary(text: &str) -> String {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            let countdown = lower.ends_with('s')
                && lower[..lower.len().saturating_sub(1)]
                    .trim()
                    .chars()
                    .all(|character| character.is_ascii_digit());
            !lower.starts_with("working (") && !lower.ends_with("esc to interrupt") && !countdown
        })
        .collect::<Vec<_>>();
    if lines.len() > 14 {
        lines.drain(..lines.len() - 14);
    }
    let mut summary = lines.join(" | ");
    if summary.chars().count() > 420 {
        summary = summary
            .chars()
            .skip(summary.chars().count() - 420)
            .collect();
    }
    summary
}

fn extract_prompt_action(text: &str, cli: &str) -> String {
    let cleaned = strip_ansi(text);
    let candidate = cleaned
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            (line.starts_with(['$', '>', '›', '❯'])
                || line.to_ascii_lowercase().starts_with("run "))
                && !lower.contains("yes")
                && !lower.contains("no")
        })
        .map(|line| line.trim_start_matches(['$', '>', '›', '❯']).trim())
        .filter(|line| !line.is_empty());
    clamp_text(
        candidate.unwrap_or(match cli {
            "codex" => "Codex 请求执行一项操作",
            "claude" => "Claude Code 请求执行一项操作",
            _ => "CLI 请求执行一项操作",
        }),
        300,
    )
}

fn contains_word(text: &str, word: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| token == word)
}

fn confirmation_answers(
    summary: &str,
) -> (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
) {
    let lower = summary.to_ascii_lowercase();
    if lower.contains("1.") && (lower.contains("yes") || lower.contains("allow")) {
        let allow_rule = (lower.contains("2.")
            && ["always", "don't ask", "do not ask", "again"]
                .iter()
                .any(|word| lower.contains(word)))
        .then_some("2\n");
        let feedback = (lower.contains("3.")
            && lower.contains("tell")
            && ["different", "instead", "feedback"]
                .iter()
                .any(|word| lower.contains(word)))
        .then_some("3\n");
        let deny = if lower.contains("3.") { "3\n" } else { "2\n" };
        ("1\n", allow_rule, feedback, deny)
    } else {
        ("y\n", None, None, "n\n")
    }
}

fn update_remote_focus(pid: u32, focused: bool) {
    let Ok(mut stream) = connect_pipe() else {
        return;
    };
    let payload = WireRequest {
        kind: "focus_update".into(),
        cli: String::new(),
        pid,
        cwd: String::new(),
        action: String::new(),
        summary: String::new(),
        allow_rule: false,
        feedback: false,
        source_window: 0,
        focus_known: true,
        focused,
        demo: false,
    };
    let Ok(mut body) = serde_json::to_vec(&payload) else {
        return;
    };
    body.push(b'\n');
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escape = false;
    for character in value.chars() {
        if escape {
            if character.is_ascii_alphabetic() || character == '\x07' {
                escape = false;
            }
            continue;
        }
        if character == '\x1b' {
            escape = true;
            continue;
        }
        if character == '\r' {
            output.push('\n');
        } else if !character.is_control() || character == '\n' || character == '\t' {
            output.push(character);
        }
    }
    output
}

fn clamp_text(value: &str, max_chars: usize) -> String {
    let mut text = value
        .chars()
        .filter(|character| !matches!(character, '\0' | '\r' | '\n'))
        .collect::<String>();
    if text.chars().count() > max_chars {
        text = text.chars().take(max_chars).collect();
    }
    text
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn foreground_root_window() -> u64 {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        0
    } else {
        (unsafe { GetAncestor(foreground, GA_ROOT) }) as usize as u64
    }
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
