use super::super::{CHILD_SCREEN_HEIGHT, CHILD_SCREEN_WIDTH, WireRequest, connect_pipe};
use super::sink::InputSink;
use anyhow::Result;
use std::{
    io::{self, Read, Write},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::WriteFile,
    System::{
        Console::{
            CONSOLE_SCREEN_BUFFER_INFO, COORD, GetConsoleScreenBufferInfo, GetStdHandle, HPCON,
            ResizePseudoConsole, STD_OUTPUT_HANDLE,
        },
        Pipes::CreatePipe,
    },
    UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetForegroundWindow},
};

pub(super) fn normalize_console_size(size: COORD) -> COORD {
    COORD {
        X: size.X.max(1),
        Y: size.Y.max(1),
    }
}

pub(super) fn resize_pseudo_console_loop(console: HPCON, stop: Arc<AtomicBool>) {
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

pub(super) fn write_cli_output(bytes: &[u8]) -> io::Result<()> {
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

pub(super) fn forward_stdin(input: Arc<InputSink>) {
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
pub(super) fn parent_console_size() -> COORD {
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

pub(super) fn create_pipe() -> Result<(HANDLE, HANDLE)> {
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

pub fn build_command_line(cli: &str, args: &[String]) -> String {
    let mut command = quote_cmd_arg(cli);
    for arg in args {
        command.push(' ');
        command.push_str(&quote_cmd_arg(arg));
    }
    format!("cmd.exe /d /s /c \"{command}\"")
}
pub fn quote_cmd_arg(value: &str) -> String {
    if value
        .chars()
        .all(|character| !character.is_whitespace() && !matches!(character, '"' | '^' | '&' | '|'))
    {
        return value.into();
    }
    format!("\"{}\"", value.replace('"', "\\\""))
}

pub(super) fn update_remote_focus(pid: u32, focused: bool) {
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

pub(super) fn update_remote_session(
    kind: &str,
    pid: u32,
    source_window: u64,
    focus_known: bool,
    focused: bool,
) {
    let Ok(mut stream) = connect_pipe() else {
        return;
    };
    let payload = WireRequest {
        kind: kind.into(),
        cli: String::new(),
        pid,
        cwd: String::new(),
        action: String::new(),
        summary: String::new(),
        allow_rule: false,
        feedback: false,
        source_window,
        focus_known,
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

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub(super) fn foreground_root_window() -> u64 {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        0
    } else {
        (unsafe { GetAncestor(foreground, GA_ROOT) }) as usize as u64
    }
}
