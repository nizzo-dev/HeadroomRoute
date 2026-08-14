#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod approval;
mod branding;
mod cli_identity;
pub mod config;
mod edition;
pub mod environment_recovery;
mod model;
mod notification;
pub mod operation_history;
mod precheck;
mod progress;
mod proxy;
pub mod routing_policy;
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
    System::Threading::{CreateMutexW, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
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
    wait_for_previous_instance(&args)?;

    let defaults = crate::model::AppConfig::default();
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
    // 短路：只有模式确实需要 Headroom 时才启动只读子进程探测；旁路、已启用协议
    // 全部直连或全部禁用时 `&&` 右侧不会求值，不产生任何探测开销。
    let startup_python_found = should_probe_python_at_startup(&startup_config)
        && runtime::find_valid_python(&startup_config).is_some();
    if should_mark_runtime_unavailable(&startup_config, startup_python_found) {
        app.inner.lock().unwrap().headroom_state = "runtime-unavailable".into();
    } else {
        let startup_url = app.active_url();
        let startup_anthropic_url = app.active_anthropic_url();
        if let Err(error) = config::sync_all_with_targets(
            &startup_config,
            startup_url.as_deref(),
            startup_anthropic_url.as_deref(),
        ) {
            app.inner.lock().unwrap().last_error = Some(format!("首次路由同步失败: {error}"));
        }
    }

    let cli_compatibility = app.snapshot().cli_compatibility;
    if !cli_compatibility.compatible {
        notification::warning(
            "CLI wrapper 版本不匹配",
            format!(
                "AI 回复完成通知不可用。期望 v{} / 协议 {}，当前 v{}。请运行当前版本的 Install.ps1 成套安装。",
                cli_compatibility.expected_version,
                crate::cli_identity::CLI_PROTOCOL_VERSION,
                cli_compatibility
                    .detected_version
                    .as_deref()
                    .unwrap_or("未检测到")
            ),
        );
    }
    match app.begin_session() {
        Ok(true) if precheck::mode_needs_headroom(&startup_config) => {
            // The previous process did not mark a clean exit. Let the
            // managed Headroom worker perform a full stop/start recovery.
            app.restart_headroom.store(true, Ordering::Release);
        }
        Ok(true) => {
            // Direct and bypass modes have no managed Headroom child to
            // restart, but the route configuration was already synchronized.
            app.process_recovery_event(
                crate::environment_recovery::EnvironmentEvent::RecoverySucceeded,
            );
        }
        Ok(false) => {}
        Err(error) => {
            app.inner.lock().unwrap().last_error = Some(format!("创建会话标记失败: {error}"));
        }
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
        tray::run(app.clone(), should_auto_open_precheck(first_run, &args))
    };
    app.stop.store(true, Ordering::Relaxed);
    let _ = proxy_thread.join();
    for worker in workers {
        let _ = worker.join();
    }
    if let Err(error) = app.finish_session() {
        app.inner.lock().unwrap().last_error = Some(format!("完成会话标记失败: {error}"));
    }
    let maintenance_action = app.maintenance_action.lock().unwrap().take();
    if !matches!(maintenance_action.as_deref(), Some("restore" | "uninstall")) {
        let cfg = app.inner.lock().unwrap().config.clone();
        // Always release managed clients back to CC-Switch current upstream so
        // Codex/Claude keep working after HeadroomRoute exits.
        if cfg.manage_upstream
            && let Err(error) = config::release_to_cc_switch(&cfg)
        {
            let message = format!("退出时交还 CC-Switch Provider 失败: {error}");
            app.inner.lock().unwrap().last_error = Some(message.clone());
            notification::blocking_error("CC-Switch 控制权交还失败", message);
        }
        let _ = app.write_status();
    }
    let relaunch = should_relaunch(maintenance_action.as_deref());
    if let Some(action) = maintenance_action {
        let mut cfg = app.inner.lock().unwrap().config.clone();
        match action.as_str() {
            "restore" => {
                config::restore_clients(&cfg)?;
                show_info("Codex 与 Claude Code 配置已恢复");
            }
            "check-runtime" => match require_runtime(&cfg) {
                Ok(python) => {
                    cfg.headroom_python = Some(python);
                    match config::save(&config_path, &cfg) {
                        Ok(()) => show_info("Headroom 环境检测通过，正在自动重新启动程序"),
                        Err(error) => {
                            notification::blocking_error(
                                "保存 Headroom 环境失败",
                                format!("{error:#}"),
                            );
                        }
                    }
                }
                Err(error) => {
                    notification::blocking_error("Headroom 环境检测未通过", format!("{error:#}"));
                }
            },
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
    if relaunch && let Err(error) = spawn_relaunch(std::process::id()) {
        notification::blocking_error("HeadroomRoute 重新启动失败", format!("{error:#}"));
    }
    tray_result
}

fn adopt_cc_switch_startup_selection(config: &mut crate::model::AppConfig) {
    if !config.cc_switch_db.exists() {
        return;
    }
    // In observe mode, align selection with CC-Switch current providers.
    if config.manage_upstream {
        return;
    }
    if config.enable_codex {
        match sqlite::current_provider(&config.cc_switch_db, "codex") {
            Ok(Some(provider)) => config.selected_openai_provider = Some(provider.id),
            Ok(None) => {}
            Err(error) => notification::warning(
                "CC-Switch 启动同步",
                format!("无法读取 Codex 当前 Provider，继续使用 HeadroomRoute 历史选择：{error}"),
            ),
        }
    }
    if config.enable_claude {
        match sqlite::current_provider(&config.cc_switch_db, "claude") {
            Ok(Some(provider)) => config.selected_anthropic_provider = Some(provider.id),
            Ok(None) => {}
            Err(error) => notification::warning(
                "CC-Switch 启动同步",
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

/// 首次运行（启动前 config.json 不存在）且未走无界面模式时，托盘窗口初始化完成后
/// 自动打开一次预检向导；测试演示参数不自动打开。
fn should_auto_open_precheck(first_run: bool, args: &[String]) -> bool {
    first_run
        && !args
            .iter()
            .any(|arg| arg.starts_with("--headless-seconds="))
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--approval-demo" | "--notification-demo"))
}

/// 启动门禁：是否把 Headroom 运行环境标记为不可用。只有模式确实需要 Headroom
/// （`precheck::mode_needs_headroom` / 接管上游开启）且找不到可用 Python 时才为真。
fn should_mark_runtime_unavailable(config: &crate::model::AppConfig, python_found: bool) -> bool {
    precheck::mode_needs_headroom(config) && !python_found
}

/// 启动阶段是否需要探测 Headroom 运行环境：仅在接管上游且未旁路时才启动
/// 只读子进程探测。观测模式、旁路或协议全禁用时短路返回 `false`。
fn should_probe_python_at_startup(config: &crate::model::AppConfig) -> bool {
    precheck::mode_needs_headroom(config)
}

fn require_runtime(config: &crate::model::AppConfig) -> Result<PathBuf> {
    runtime::find_valid_python(config)
        .ok_or_else(|| anyhow::anyhow!(runtime::setup_instructions(config)))
}

fn should_relaunch(action: Option<&str>) -> bool {
    action == Some("check-runtime")
}

fn wait_exit_pid(args: &[String]) -> Option<u32> {
    args.iter().find_map(|arg| {
        arg.strip_prefix("--wait-exit=")
            .and_then(|value| value.parse::<u32>().ok())
    })
}

fn wait_for_previous_instance(args: &[String]) -> Result<()> {
    let Some(pid) = wait_exit_pid(args) else {
        return Ok(());
    };
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return Ok(());
    }
    unsafe {
        WaitForSingleObject(process, 15_000);
        CloseHandle(process);
    }
    Ok(())
}

fn spawn_relaunch(old_pid: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let executable = std::env::current_exe().context("无法确定当前程序路径")?;
    std::process::Command::new(executable)
        .arg(format!("--wait-exit={old_pid}"))
        .creation_flags(0x08000000)
        .spawn()
        .context("无法启动新实例")?;
    Ok(())
}

#[allow(dead_code)]
fn app_data_dir() -> PathBuf {
    crate::model::AppConfig::default().state_dir
}

#[cfg(test)]
mod tests {
    use super::{
        should_auto_open_precheck, should_mark_runtime_unavailable, should_probe_python_at_startup,
        should_relaunch, wait_exit_pid,
    };

    #[test]
    fn relaunches_only_after_runtime_redetection() {
        assert!(should_relaunch(Some("check-runtime")));
        assert!(!should_relaunch(Some("restore")));
        assert!(!should_relaunch(Some("uninstall")));
        assert!(!should_relaunch(None));
    }

    #[test]
    fn parses_previous_instance_pid_from_arguments() {
        assert_eq!(
            wait_exit_pid(&["--wait-exit=4321".into(), "other".into()]),
            Some(4321)
        );
        assert_eq!(wait_exit_pid(&["--wait-exit=abc".into()]), None);
        assert_eq!(wait_exit_pid(&["--wait-exit=".into()]), None);
        assert_eq!(wait_exit_pid(&["plain".into()]), None);
        assert_eq!(wait_exit_pid(&[]), None);
    }

    #[test]
    fn auto_opens_precheck_only_on_first_run() {
        assert!(should_auto_open_precheck(true, &[]));
        assert!(!should_auto_open_precheck(false, &[]));
        assert!(!should_auto_open_precheck(
            true,
            &["--headless-seconds=5".into()]
        ));
        assert!(!should_auto_open_precheck(
            true,
            &["--approval-demo".into()]
        ));
        assert!(!should_auto_open_precheck(
            true,
            &["--headless-seconds=5".into(), "--approval-demo".into()]
        ));
        assert!(should_auto_open_precheck(true, &["--doctor".into()]));
    }

    #[test]
    fn startup_probe_short_circuits_when_headroom_not_needed() {
        let defaults = crate::model::AppConfig::default();
        // Default is observe mode: no Headroom required.
        assert!(!should_probe_python_at_startup(&defaults));
        assert!(should_probe_python_at_startup(&crate::model::AppConfig {
            manage_upstream: true,
            ..defaults.clone()
        }));
        assert!(!should_probe_python_at_startup(&crate::model::AppConfig {
            manage_upstream: true,
            bypass_headroom: true,
            ..defaults.clone()
        }));
        assert!(!should_probe_python_at_startup(&crate::model::AppConfig {
            manage_upstream: true,
            enable_codex: false,
            enable_claude: false,
            ..defaults.clone()
        }));
    }

    #[test]
    fn startup_gate_reuses_mode_needs_headroom() {
        let defaults = crate::model::AppConfig::default();
        assert!(!should_mark_runtime_unavailable(&defaults, false));
        assert!(should_mark_runtime_unavailable(
            &crate::model::AppConfig {
                manage_upstream: true,
                ..defaults.clone()
            },
            false
        ));
        assert!(!should_mark_runtime_unavailable(
            &crate::model::AppConfig {
                manage_upstream: true,
                ..defaults.clone()
            },
            true
        ));
        assert!(!should_mark_runtime_unavailable(
            &crate::model::AppConfig {
                manage_upstream: true,
                bypass_headroom: true,
                ..defaults.clone()
            },
            false
        ));
        assert!(!should_mark_runtime_unavailable(
            &crate::model::AppConfig {
                manage_upstream: true,
                enable_codex: false,
                enable_claude: false,
                ..defaults
            },
            false
        ));
    }
}
