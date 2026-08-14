use super::{
    ApprovalChoice, ApprovalDecision, ApprovalRequest, BACKGROUND_REMINDER_DELAY,
    MAX_PENDING_REQUESTS, PopupKind, WM_APPROVAL, clamp_text,
};
use std::{
    collections::{HashMap, VecDeque},
    process::{self, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, PostMessageW,
};

pub(super) struct Waiter {
    result: Mutex<Option<ApprovalDecision>>,
    ready: Condvar,
}

impl Waiter {
    pub(super) fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    pub(super) fn resolve(&self, decision: ApprovalDecision) {
        *self.result.lock().unwrap() = Some(decision);
        self.ready.notify_one();
    }

    pub(super) fn wait(&self) -> ApprovalDecision {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.ready.wait(result).unwrap();
        }
        result.expect("approval waiter must be resolved")
    }
}

pub(super) struct PendingRequest {
    request: ApprovalRequest,
    waiter: Option<Arc<Waiter>>,
    background_since: Option<Instant>,
}

pub(super) struct BrokerState {
    pub(super) pending: HashMap<u64, PendingRequest>,
    queue: VecDeque<u64>,
    sessions: HashMap<u32, CliSession>,
}

#[derive(Clone, Copy)]
struct CliSession {
    source_window: u64,
    focus_known: bool,
    focused: bool,
}

pub(super) struct Broker {
    next_id: AtomicU64,
    tray_hwnd: AtomicIsize,
    server_started: AtomicBool,
    pub(super) state: Mutex<BrokerState>,
}

static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();

pub(super) fn broker() -> &'static Arc<Broker> {
    BROKER.get_or_init(|| {
        Arc::new(Broker {
            next_id: AtomicU64::new(1),
            tray_hwnd: AtomicIsize::new(0),
            server_started: AtomicBool::new(false),
            state: Mutex::new(BrokerState {
                pending: HashMap::new(),
                queue: VecDeque::new(),
                sessions: HashMap::new(),
            }),
        })
    })
}

pub fn start_server() {
    if broker().server_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = thread::Builder::new()
        .name("headroom-approval-pipe".into())
        .spawn(super::pipe::pipe_server);
}

pub fn ensure_server() -> bool {
    if super::pipe::approval_pipe_available() {
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
        if super::pipe::approval_pipe_available() {
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

pub(super) fn is_cli_executable_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("HeadroomRouteCLI")
        || name
            .get(.."HeadroomRouteCLI-".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("HeadroomRouteCLI-"))
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

#[allow(dead_code)]
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

pub(super) fn cancel_pid(pid: u32) -> usize {
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

pub(super) fn update_pid_focus(pid: u32, focused: bool) -> usize {
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
    if let Some(session) = state.sessions.get_mut(&pid) {
        session.focus_known = true;
        session.focused = focused;
    }
    drop(state);
    if count > 0 {
        broker.notify_ui();
    }
    count
}

pub(super) fn register_cli_session(pid: u32, source_window: u64, focus_known: bool, focused: bool) {
    broker().state.lock().unwrap().sessions.insert(
        pid,
        CliSession {
            source_window,
            focus_known,
            focused,
        },
    );
}

pub(super) fn close_cli_session(pid: u32) {
    broker().state.lock().unwrap().sessions.remove(&pid);
}

pub(super) fn should_show_turn_notice(pid: u32) -> bool {
    let session = broker().state.lock().unwrap().sessions.get(&pid).copied();
    let Some(session) = session else {
        return true;
    };
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_root = if foreground.is_null() {
        0
    } else {
        unsafe { GetAncestor(foreground, GA_ROOT) as u64 }
    };
    turn_notice_visible(session, foreground_root)
}

fn turn_notice_visible(session: CliSession, foreground_root: u64) -> bool {
    session.source_window == 0
        || foreground_root != session.source_window
        || !session.focus_known
        || !session.focused
}

pub(super) fn enqueue_notice(
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
#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue(
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
    pub(super) fn notify_ui(&self) {
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

#[cfg(test)]
mod turn_notice_tests {
    use super::{CliSession, turn_notice_visible};

    #[test]
    fn suppresses_notice_while_cli_terminal_is_foreground() {
        let session = CliSession {
            source_window: 42,
            focus_known: true,
            focused: true,
        };
        assert!(!turn_notice_visible(session, 42));
    }

    #[test]
    fn shows_notice_after_cli_terminal_moves_to_background() {
        let session = CliSession {
            source_window: 42,
            focus_known: true,
            focused: false,
        };
        assert!(turn_notice_visible(session, 99));
    }
}
