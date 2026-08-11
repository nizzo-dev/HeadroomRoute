use super::*;

pub(super) struct InputSink {
    pub(super) file: Mutex<Option<File>>,
    pub(super) next_approval_token: AtomicU64,
    pub(super) active_approval_token: AtomicU64,
    pub(super) pid: u32,
    pub(super) source_window: u64,
    pub(super) focus_known: AtomicBool,
    pub(super) focused: AtomicBool,
    pub(super) turn_pending: AtomicBool,
    pub(super) turn_activity_seen: AtomicBool,
    /// After submit, ignore a stale `• 已完成` until it leaves the region once
    /// (or non-prompt activity is seen). Prevents instant false completes.
    pub(super) turn_completion_armed: AtomicBool,
    pub(super) turn_input_has_text: AtomicBool,
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

    pub(super) fn begin_approval(&self) -> u64 {
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

    pub(super) fn finish_approval(
        &self,
        token: u64,
        decision: ApprovalDecision,
        prompt: &ConfirmationPrompt,
    ) {
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
        self.mark_turn_submitted(bytes);
        self.write(bytes)
    }

    fn mark_turn_submitted(&self, bytes: &[u8]) {
        let submitted = bytes.iter().any(|byte| *byte == b'\r' || *byte == b'\n');
        let has_text = bytes.iter().any(|byte| *byte >= b' ');
        if has_text {
            self.turn_input_has_text.store(true, Ordering::Release);
        }
        if submitted && (has_text || self.turn_input_has_text.swap(false, Ordering::AcqRel)) {
            self.turn_activity_seen.store(false, Ordering::Release);
            self.turn_completion_armed.store(false, Ordering::Release);
            self.turn_pending.store(true, Ordering::Release);
        }
    }

    fn take_completed_turn(&self) -> bool {
        if !self.turn_pending.load(Ordering::Acquire)
            || !self.turn_activity_seen.load(Ordering::Acquire)
        {
            return false;
        }
        self.clear_turn();
        true
    }

    fn clear_turn(&self) {
        self.turn_pending.store(false, Ordering::Release);
        self.turn_activity_seen.store(false, Ordering::Release);
        self.turn_completion_armed.store(false, Ordering::Release);
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
        turn_pending: AtomicBool::new(false),
        turn_activity_seen: AtomicBool::new(false),
        turn_completion_armed: AtomicBool::new(false),
        turn_input_has_text: AtomicBool::new(false),
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
    if input.take_completed_turn() {
        notify_turn_result(&cli, child_pid, TurnResult::Completed);
    }
    if let Some(error) = output_error {
        let _ = write_cli_output(b"\x1b[?1004l");
        return Err(error.context("转发 CLI 输出失败"));
    }
    let _ = write_cli_output(b"\x1b[?1004l");
    Ok((exit_code & 0xff) as i32)
}

pub(super) fn read_cli_output(
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
        // Codex keeps the caret on the model/status row under the › prompt, so
        // completion detection must look at the cursor neighborhood—not only the
        // single cursor line—otherwise idle turns never fire turn_completed.
        let prompt_region = terminal.prompt_region_text();
        // End-of-turn chrome ("Worked for", "tokens used") often sits a few rows
        // above the caret; scan the bottom of the screen as well as the region.
        let completion_scan = terminal.completion_scan_text();
        if input.turn_pending.load(Ordering::Acquire) {
            let prompt_ready = cli_input_prompt_ready(&cli, &prompt_region);
            let completion_bullet =
                cli == "codex" && completion_bullet_visible(&completion_scan);
            // Arm bullet fallback only after the stale completion marker leaves
            // (or after non-prompt activity). Avoids firing on the previous turn's •.
            if !completion_bullet
                && !input
                    .turn_completion_armed
                    .swap(true, Ordering::AcqRel)
            {
                approval_trace(&format!(
                    "turn completion armed ({cli}); region={}",
                    clamp_trace(&prompt_region)
                ));
            }
            if prompt_ready {
                // Preferred: left prompt then returned. Codex often keeps › near
                // the caret all turn, so also accept a freshly armed completion bullet.
                let completed = input.take_completed_turn()
                    || (completion_bullet
                        && input.turn_completion_armed.load(Ordering::Acquire)
                        && {
                            input.clear_turn();
                            true
                        });
                if completed {
                    let result = classify_turn_result(&cli, &screen);
                    approval_trace(&format!(
                        "turn complete detected ({cli}): {result:?}; scan={}",
                        clamp_trace(&completion_scan)
                    ));
                    notify_turn_result(&cli, pid, result);
                }
            } else if !input.turn_activity_seen.swap(true, Ordering::AcqRel) {
                input.turn_completion_armed.store(true, Ordering::Release);
                approval_trace(&format!(
                    "turn activity seen ({cli}); region={}",
                    clamp_trace(&prompt_region)
                ));
            }
        }
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

pub(super) fn approval_trace(message: &str) {
    if std::env::var_os("HEADROOM_ROUTE_APPROVAL_TRACE").is_some() {
        eprintln!("HeadroomRoute approval trace: {message}");
    }
}

fn clamp_trace(value: &str) -> String {
    let flat: String = value
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    let trimmed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    trimmed.chars().take(160).collect()
}

pub(super) struct TerminalScreen {
    parser: vt100::Parser,
    size: COORD,
    last_prompt_key: Option<String>,
}

impl TerminalScreen {
    pub(super) fn new(size: COORD) -> Self {
        let size = normalize_console_size(size);
        Self {
            parser: vt100::Parser::new(size.Y as u16, size.X as u16, 0),
            size,
            last_prompt_key: None,
        }
    }

    pub(super) fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub(super) fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Text around the caret used for input-prompt detection.
    ///
    /// Codex often parks the cursor on the model line directly under `›` /
    /// `❯`, so a single cursor line misses the prompt glyph entirely.
    fn prompt_region_text(&self) -> String {
        const RADIUS: usize = 2;
        let screen = self.parser.screen();
        let (row, _) = screen.cursor_position();
        let width = self.size.X.max(1) as u16;
        let row = row as usize;
        let start = row.saturating_sub(RADIUS);
        let end = row.saturating_add(RADIUS);
        screen
            .rows(0, width)
            .enumerate()
            .filter_map(|(index, line)| (index >= start && index <= end).then_some(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Bottom-of-screen text used to spot Codex end-of-turn chrome.
    ///
    /// `Worked for` / `tokens used` can sit several rows above the caret while
    /// the prompt stays near the bottom; the narrow prompt region misses them.
    fn completion_scan_text(&self) -> String {
        const BOTTOM_ROWS: usize = 12;
        let screen = self.parser.screen();
        let width = self.size.X.max(1) as u16;
        let rows: Vec<String> = screen.rows(0, width).collect();
        let start = rows.len().saturating_sub(BOTTOM_ROWS);
        rows[start..].join("\n")
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

pub(super) fn build_command_line(cli: &str, args: &[String]) -> String {
    let mut command = quote_cmd_arg(cli);
    for arg in args {
        command.push(' ');
        command.push_str(&quote_cmd_arg(arg));
    }
    format!("cmd.exe /d /s /c \"{command}\"")
}

pub(super) fn quote_cmd_arg(value: &str) -> String {
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

pub(super) fn wide(value: &str) -> Vec<u16> {
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
