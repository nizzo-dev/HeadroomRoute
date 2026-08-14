use super::super::{
    TurnResult, cancel_remote_requests, ensure_server, notify_turn_result,
    write_claude_stop_hook_settings,
};
use super::io::{
    build_command_line, create_pipe, foreground_root_window, forward_stdin, parent_console_size,
    resize_pseudo_console_loop, update_remote_session, wide, write_cli_output,
};
use super::output::read_cli_output;
use super::sink::InputSink;
use anyhow::{Context, Result};
use std::{
    ffi::c_void,
    fs::File,
    os::windows::io::{FromRawHandle, RawHandle},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE},
    System::{
        Console::{ClosePseudoConsole, CreatePseudoConsole, HPCON},
        Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
            EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

pub(super) struct SpawnedConsole {
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
    let session_pid = std::process::id();
    let cwd = std::env::current_dir().context("无法读取当前工作目录")?;
    let mut forwarded_args = args[1..].to_vec();
    let executable = std::env::current_exe().context("无法定位 HeadroomRouteCLI 可执行文件")?;
    // Codex `notify` and Claude `Stop` are process-local hooks. When they are
    // installed, skip screen-scraped turn completion — Enter echo looks too
    // much like a finished prompt cycle.
    let turn_notify_hook = if cli == "codex" {
        forwarded_args.insert(0, codex_notify_config(&executable, session_pid));
        forwarded_args.insert(0, "-c".into());
        true
    } else if cli == "claude" {
        match write_claude_stop_hook_settings(&executable, session_pid) {
            Ok(path) => {
                forwarded_args.insert(0, path.to_string_lossy().into_owned());
                forwarded_args.insert(0, "--settings".into());
                true
            }
            Err(error) => {
                eprintln!(
                    "HeadroomRoute：无法注入 Claude Stop 钩子（{error:#}），回复完成通知不可用"
                );
                false
            }
        }
    } else {
        false
    };
    let command_line = build_command_line(&cli, &forwarded_args);
    let spawned = SpawnedConsole::spawn(&command_line, cwd.to_string_lossy().as_ref(), &cli)?;
    let child_pid = spawned.pid;
    let input = Arc::new(InputSink {
        file: Mutex::new(Some(spawned.input)),
        next_approval_token: AtomicU64::new(1),
        active_approval_token: AtomicU64::new(0),
        pid: child_pid,
        session_pid,
        source_window,
        focus_known: AtomicBool::new(source_window != 0),
        focused: AtomicBool::new(source_window != 0),
        turn_pending: AtomicBool::new(false),
        turn_activity_seen: AtomicBool::new(false),
        turn_prompt_left: AtomicBool::new(false),
        turn_prompt_returned: AtomicBool::new(false),
        turn_completion_armed: AtomicBool::new(false),
        turn_input_has_text: AtomicBool::new(false),
    });
    update_remote_session(
        "session_register",
        session_pid,
        source_window,
        source_window != 0,
        source_window != 0,
    );
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
                turn_notify_hook,
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
    update_remote_session("session_close", session_pid, 0, false, false);
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
    if !turn_notify_hook && input.take_completed_turn() {
        notify_turn_result(&cli, session_pid, TurnResult::Completed);
    }
    if let Some(error) = output_error {
        let _ = write_cli_output(b"\x1b[?1004l");
        return Err(error.context("转发 CLI 输出失败"));
    }
    let _ = write_cli_output(b"\x1b[?1004l");
    Ok((exit_code & 0xff) as i32)
}
impl SpawnedConsole {
    fn spawn(command_line: &str, cwd: &str, cli: &str) -> Result<Self> {
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
        // Explicit NULL standard handles via STARTF_USESTDHANDLES: with the
        // ConPTY attribute this skips the kernel's console-app handle
        // duplication hack (microsoft/terminal#15814) so the pseudoconsole
        // connection fills the child's std slots with the pty console handles.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let mut command = wide(command_line);
        let mut process_info = unsafe { std::mem::zeroed() };
        let directory = wide(cwd);
        let environment = super::super::headroom_project::child_unicode_environment(
            cli,
            std::path::Path::new(cwd),
        );
        let created = unsafe {
            CreateProcessW(
                ptr::null(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
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
fn codex_notify_config(executable: &std::path::Path, pid: u32) -> String {
    let executable = executable.to_string_lossy();
    // Use TOML basic strings so an apostrophe in a Windows user/profile path
    // is harmless.  Backslashes must be escaped for TOML, then Codex receives
    // the original Windows path after parsing the `-c` value.
    let executable = executable.replace('\\', "\\\\").replace('"', "\\\"");
    format!("notify=[\"{executable}\",\"--codex-notify\",\"{pid}\"]")
}

#[cfg(test)]
mod codex_notify_config_tests {
    use super::codex_notify_config;
    use std::path::Path;

    #[test]
    fn builds_a_process_local_codex_notify_command() {
        assert_eq!(
            codex_notify_config(Path::new(r"C:\Apps\HeadroomRouteCLI.exe"), 42),
            String::from(
                "notify=[\"C:\\\\Apps\\\\HeadroomRouteCLI.exe\",\"--codex-notify\",\"42\"]"
            )
        );
    }

    #[test]
    fn escapes_paths_that_contain_an_apostrophe() {
        assert_eq!(
            codex_notify_config(Path::new(r"C:\O'Brien\cli.exe"), 42),
            String::from("notify=[\"C:\\\\O'Brien\\\\cli.exe\",\"--codex-notify\",\"42\"]")
        );
    }
}
