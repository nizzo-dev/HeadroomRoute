use super::{
    ApprovalDecision, ConfirmationPrompt, GENERIC_READ_FLAG, GENERIC_WRITE_FLAG, MAX_MESSAGE_BYTES,
    MAX_PENDING_REQUESTS, PIPE_ACCESS_DUPLEX_FLAG, PIPE_NAME, PopupKind, WireRequest, WireResponse,
    approval_trace, clamp_text, valid_wire_request, wide, write_reason, write_response,
};
use anyhow::Result;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::windows::io::{FromRawHandle, RawHandle},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

static PIPE_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

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
    Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING},
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_MESSAGE, PIPE_WAIT, WaitNamedPipeW,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

pub(super) fn approval_pipe_available() -> bool {
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
pub(super) fn request_approval(
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
pub(super) fn pipe_server() {
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
        super::broker::cancel_pid(request.pid);
        let _ = write_reason(&mut stream, false, "cancelled");
        return;
    }
    if request.kind == "focus_update" {
        super::broker::update_pid_focus(request.pid, request.focused);
        let _ = write_reason(&mut stream, false, "focus_updated");
        return;
    }
    if request.kind == "session_register" {
        super::broker::register_cli_session(
            request.pid,
            request.source_window,
            request.focus_known,
            request.focused,
        );
        let _ = write_reason(&mut stream, false, "session_registered");
        return;
    }
    if request.kind == "session_close" {
        super::broker::close_cli_session(request.pid);
        let _ = write_reason(&mut stream, false, "session_closed");
        return;
    }
    if request.kind == "turn_completed" || request.kind == "turn_failed" {
        if !super::broker::should_show_turn_notice(request.pid) {
            let _ = write_reason(&mut stream, false, &request.kind);
            return;
        }
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
        let queued = super::broker::enqueue_notice(
            request.cli,
            request.pid,
            popup_kind,
            action.into(),
            summary,
        );
        let reason = if queued { &request.kind } else { "queue_full" };
        let _ = write_reason(&mut stream, false, reason);
        return;
    }
    let waiter = Arc::new(super::broker::Waiter::new());
    let Some(enqueued) = super::broker::enqueue(
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
    let removed = super::broker::broker()
        .state
        .lock()
        .unwrap()
        .pending
        .remove(&enqueued.id);
    if removed.is_some() {
        super::broker::broker().notify_ui();
    }
    let _ = write_response(&mut stream, decision);
}

pub(super) fn cancel_remote_requests(pid: u32) {
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

pub(super) fn connect_pipe() -> Result<File> {
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
