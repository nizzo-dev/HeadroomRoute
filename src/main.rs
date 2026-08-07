#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod approval;
mod config;
mod model;
mod notification;
mod progress;
mod proxy;
mod runtime;
mod sqlite;
mod state;
mod tray;
mod updater;
mod worker;

use anyhow::{Context, Result};
use state::AppState;
use std::{path::PathBuf, ptr, sync::atomic::Ordering};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType, OPEN_EXISTING,
    },
    System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleCP,
        GetConsoleMode, GetConsoleOutputCP, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, SetStdHandle,
    },
    System::Threading::CreateMutexW,
};

fn main() {
    enable_dpi_awareness();
    let is_cli_wrapper = std::env::args().nth(1).as_deref() == Some("run");
    if is_cli_wrapper {
        notification::blocking_info(
            "HeadroomRoute CLI",
            "此入口已停用，请在终端使用 HeadroomRouteCLI.exe claude 或 HeadroomRouteCLI.exe codex。",
        );
        std::process::exit(2);
    }
    let console = if is_cli_wrapper {
        match attach_cli_console() {
            Some(console) => Some(console),
            None => {
                eprintln!("HeadroomRoute CLI wrapper 需要从 cmd.exe 或 PowerShell 终端启动");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    if let Err(error) = run() {
        if is_cli_wrapper {
            drop(console);
            eprintln!("Headroom Route CLI wrapper failed: {error:#}");
            std::process::exit(1);
        } else {
            drop(console);
            let message = format!("Headroom Route 启动失败：\r\n\r\n{error:#}");
            notification::blocking_error("Headroom Route", message);
        }
    }
}

struct ConsoleSettings {
    input_handle: windows_sys::Win32::Foundation::HANDLE,
    output_handle: windows_sys::Win32::Foundation::HANDLE,
    input_mode: Option<u32>,
    output_mode: Option<u32>,
    input_code_page: Option<u32>,
    output_code_page: Option<u32>,
}

impl Drop for ConsoleSettings {
    fn drop(&mut self) {
        unsafe {
            if let Some(mode) = self.input_mode {
                let _ = SetConsoleMode(self.input_handle, mode);
            }
            if let Some(mode) = self.output_mode {
                let _ = SetConsoleMode(self.output_handle, mode);
            }
            if let Some(code_page) = self.input_code_page {
                let _ = SetConsoleCP(code_page);
            }
            if let Some(code_page) = self.output_code_page {
                let _ = SetConsoleOutputCP(code_page);
            }
        }
    }
}

fn attach_cli_console() -> Option<ConsoleSettings> {
    unsafe {
        let inherited_input = GetStdHandle(STD_INPUT_HANDLE);
        let inherited_output = GetStdHandle(STD_OUTPUT_HANDLE);
        let inherited_error = GetStdHandle(STD_ERROR_HANDLE);
        if is_usable_std_handle(inherited_output) {
            let mut input = inherited_input;
            if !is_usable_std_handle(input) {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
                let console_input = CreateFileW(
                    wide(r"\\.\CONIN$").as_ptr(),
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    ptr::null_mut(),
                );
                if !console_input.is_null()
                    && console_input != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
                {
                    let _ = SetStdHandle(STD_INPUT_HANDLE, console_input);
                    input = GetStdHandle(STD_INPUT_HANDLE);
                    let _ = SetStdHandle(STD_OUTPUT_HANDLE, inherited_output);
                }
            }
            if !is_usable_std_handle(inherited_error) {
                let _ = SetStdHandle(STD_ERROR_HANDLE, inherited_output);
            }
            return Some(configure_console(input, inherited_output));
        }

        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        let input = CreateFileW(
            wide(r"\\.\CONIN$").as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        );
        if !input.is_null() && input != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            let _ = SetStdHandle(STD_INPUT_HANDLE, input);
        }

        let output = CreateFileW(
            wide(r"\\.\CONOUT$").as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        );
        if !output.is_null() && output != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, output);
            let _ = SetStdHandle(STD_ERROR_HANDLE, output);
        }
        let input = windows_sys::Win32::System::Console::GetStdHandle(STD_INPUT_HANDLE);
        let output = windows_sys::Win32::System::Console::GetStdHandle(STD_OUTPUT_HANDLE);
        let error = windows_sys::Win32::System::Console::GetStdHandle(STD_ERROR_HANDLE);
        if !is_usable_std_handle(input)
            || !is_usable_std_handle(output)
            || !is_usable_std_handle(error)
        {
            return None;
        }

        Some(configure_console(input, output))
    }
}

fn configure_console(
    input: windows_sys::Win32::Foundation::HANDLE,
    output: windows_sys::Win32::Foundation::HANDLE,
) -> ConsoleSettings {
    unsafe {
        let mut settings = ConsoleSettings {
            input_handle: input,
            output_handle: output,
            input_mode: None,
            output_mode: None,
            input_code_page: None,
            output_code_page: None,
        };
        let input_code_page = GetConsoleCP();
        if input_code_page != 0 {
            settings.input_code_page = Some(input_code_page);
            let _ = SetConsoleCP(65001);
        }
        let output_code_page = GetConsoleOutputCP();
        if output_code_page != 0 {
            settings.output_code_page = Some(output_code_page);
            let _ = SetConsoleOutputCP(65001);
        }

        let mut input_mode = 0u32;
        if GetConsoleMode(input, &mut input_mode) != 0 {
            settings.input_mode = Some(input_mode);
            let raw_input_mode = input_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            let _ = SetConsoleMode(input, raw_input_mode);
        }
        let mut output_mode = 0u32;
        if GetConsoleMode(output, &mut output_mode) != 0 {
            settings.output_mode = Some(output_mode);
            let vt_output_mode = output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            let _ = SetConsoleMode(output, vt_output_mode);
        }
        settings
    }
}

fn is_usable_std_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return false;
    }
    let mut console_mode = 0u32;
    unsafe {
        GetConsoleMode(handle, &mut console_mode) != 0
            || matches!(GetFileType(handle), FILE_TYPE_PIPE | FILE_TYPE_DISK)
    }
}

fn enable_dpi_awareness() {
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("run") {
        return approval::run_cli_command(&args[1..]).map(|_| ());
    }
    if args.first().map(String::as_str) == Some("--approval-host") {
        return tray::run_approval_host();
    }
    let mutex_name = wide("Local\\HeadroomRouteTray-v1");
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        anyhow::bail!("无法创建单实例锁");
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(mutex);
        }
        anyhow::bail!("程序已经在运行");
    }

    let defaults = model::AppConfig::default();
    let config_path = defaults.state_dir.join("config.json");
    let first_run = !config_path.exists();
    let mut config = config::load_or_create(&config_path)?;
    adopt_cc_switch_startup_selection(&mut config);
    std::fs::create_dir_all(&config.state_dir)?;
    let app = AppState::new(config);

    if args.iter().any(|arg| arg == "--doctor") {
        println!("{}", app.diagnostic_text());
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--restore") {
        let cfg = app.inner.lock().unwrap().config.clone();
        config::restore_clients(&cfg)?;
        show_info("Codex 与 Claude Code 配置已恢复");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| arg == "--check-runtime" || arg == "--repair-runtime")
    {
        let mut cfg = app.inner.lock().unwrap().config.clone();
        let python = require_runtime(&cfg)?;
        cfg.headroom_python = Some(python);
        config::save(&config_path, &cfg)?;
        show_info("Headroom 环境检测通过");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--uninstall") {
        let cfg = app.inner.lock().unwrap().config.clone();
        runtime::uninstall(&cfg)?;
        show_info("配置已恢复，程序数据已删除；程序文件将在 Windows 重启后清理");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| arg == "--configure" || arg == "--configure-all")
    {
        let config = app.inner.lock().unwrap().config.clone();
        let active_url = app.active_url();
        let active_anthropic_url = app.active_anthropic_url();
        let result = config::sync_all_with_targets(
            &config,
            active_url.as_deref(),
            active_anthropic_url.as_deref(),
        )?;
        println!("Codex 与 Claude Code 路由配置已同步：{result}");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--configure-claude") {
        let config = app.inner.lock().unwrap().config.clone();
        let active_url = app.active_anthropic_url();
        config::sync_claude_with_target(&config, active_url.as_deref())?;
        println!("Claude Code 路由配置已同步");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }

    let startup_config = app.inner.lock().unwrap().config.clone();
    if first_run
        && !args
            .iter()
            .any(|arg| arg.starts_with("--headless-seconds="))
        && !args.iter().any(|arg| arg == "--approval-demo")
    {
        let mut issues = Vec::new();
        let headroom_required = !startup_config.bypass_headroom
            && !(startup_config.direct_codex && startup_config.direct_claude);
        if headroom_required && runtime::find_valid_python(&startup_config).is_none() {
            issues.push(runtime::setup_instructions(&startup_config));
        }
        if app.inner.lock().unwrap().routes.is_empty() {
            issues.push(
                "未发现可用 Provider。请先在 CC-Switch 添加 Codex 或 Claude Provider，再从托盘选择“同步 Codex + Claude / CC-Switch”。"
                    .into(),
            );
        }
        if !issues.is_empty() {
            show_info(&format!(
                "首次运行检查发现以下待处理项：\r\n\r\n{}",
                issues.join("\r\n\r\n")
            ));
        }
    }
    if startup_config.bypass_headroom
        || startup_config.direct_codex
        || startup_config.direct_claude
        || runtime::find_valid_python(&startup_config).is_some()
    {
        let startup_url = app.active_url();
        let startup_anthropic_url = app.active_anthropic_url();
        if let Err(error) = config::sync_all_with_targets(
            &startup_config,
            startup_url.as_deref(),
            startup_anthropic_url.as_deref(),
        ) {
            app.inner.lock().unwrap().last_error = Some(format!("首次路由同步失败: {error}"));
        }
    } else {
        app.inner.lock().unwrap().headroom_state = "runtime-unavailable".into();
    }

    let (state_dir, legacy_dir) = {
        let state = app.inner.lock().unwrap();
        (
            state.config.state_dir.clone(),
            state.config.legacy_state_dir.clone(),
        )
    };
    let token = proxy::load_or_create_token(&state_dir, &legacy_dir)?;
    let proxy_thread = proxy::run(app.clone(), token).context("本地路由代理启动失败")?;
    let workers = worker::start(app.clone());
    let tray_result = if let Some(seconds) = args.iter().find_map(|arg| {
        arg.strip_prefix("--headless-seconds=")
            .and_then(|v| v.parse::<u64>().ok())
    }) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
        Ok(())
    } else {
        tray::run(app.clone())
    };
    app.stop.store(true, Ordering::Relaxed);
    let _ = proxy_thread.join();
    for worker in workers {
        let _ = worker.join();
    }
    let maintenance_action = app.maintenance_action.lock().unwrap().take();
    if !matches!(maintenance_action.as_deref(), Some("restore" | "uninstall")) {
        let cfg = app.inner.lock().unwrap().config.clone();
        if (cfg.direct_codex || cfg.direct_claude)
            && let Err(error) = config::handoff_direct_to_cc_switch(&cfg)
        {
            let message = format!("退出时交还 CC-Switch Provider 失败: {error}");
            app.inner.lock().unwrap().last_error = Some(message.clone());
            notification::blocking_error("CC-Switch 控制权交还失败", message);
        }
        let _ = app.write_status();
    }
    if let Some(action) = maintenance_action {
        let mut cfg = app.inner.lock().unwrap().config.clone();
        match action.as_str() {
            "restore" => {
                config::restore_clients(&cfg)?;
                show_info("Codex 与 Claude Code 配置已恢复");
            }
            "check-runtime" => {
                let python = require_runtime(&cfg)?;
                cfg.headroom_python = Some(python);
                config::save(&config_path, &cfg)?;
                show_info("Headroom 环境检测通过，请重新启动程序");
            }
            "uninstall" => {
                runtime::uninstall(&cfg)?;
                show_info("已恢复配置并删除程序数据；程序文件将在 Windows 重启后清理");
            }
            _ => {}
        }
    }
    unsafe {
        CloseHandle(mutex);
    }
    tray_result
}

fn adopt_cc_switch_startup_selection(config: &mut model::AppConfig) {
    if !config.cc_switch_db.exists() {
        return;
    }
    if config.enable_codex && config.direct_codex {
        match sqlite::current_provider(&config.cc_switch_db, "codex") {
            Ok(Some(provider)) => config.selected_openai_provider = Some(provider.id),
            Ok(None) => {}
            Err(error) => notification::warning(
                "CC-Switch 启动接管",
                format!("无法读取 Codex 当前 Provider，继续使用 HeadroomRoute 历史选择：{error}"),
            ),
        }
    }
    if config.enable_claude && config.direct_claude {
        match sqlite::current_provider(&config.cc_switch_db, "claude") {
            Ok(Some(provider)) => config.selected_anthropic_provider = Some(provider.id),
            Ok(None) => {}
            Err(error) => notification::warning(
                "CC-Switch 启动接管",
                format!("无法读取 Claude 当前 Provider，继续使用 HeadroomRoute 历史选择：{error}"),
            ),
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn show_info(message: &str) {
    notification::blocking_info("Headroom Route", message);
}

fn require_runtime(config: &model::AppConfig) -> Result<PathBuf> {
    runtime::find_valid_python(config)
        .ok_or_else(|| anyhow::anyhow!(runtime::setup_instructions(config)))
}

#[allow(dead_code)]
fn app_data_dir() -> PathBuf {
    model::AppConfig::default().state_dir
}
