#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod model;
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
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW},
};

fn main() {
    enable_dpi_awareness();
    if let Err(error) = run() {
        let message = format!("Headroom Route 启动失败：\r\n\r\n{error:#}");
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                wide(&message).as_ptr(),
                wide("Headroom Route").as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
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
    let config = config::load_or_create(&config_path)?;
    std::fs::create_dir_all(&config.state_dir)?;
    let app = AppState::new(config);

    let args: Vec<String> = std::env::args().skip(1).collect();
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
    if args.iter().any(|arg| arg == "--repair-runtime") {
        let mut cfg = app.inner.lock().unwrap().config.clone();
        let python = repair_runtime_with_progress(&cfg)?;
        cfg.headroom_python = Some(python);
        config::save(&config_path, &cfg)?;
        show_info("Headroom 运行环境修复完成");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--uninstall") {
        let cfg = app.inner.lock().unwrap().config.clone();
        runtime::uninstall(&cfg)?;
        show_info("配置已恢复，托管运行环境已删除；程序文件将在 Windows 重启后清理");
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
        let result = config::sync_all(&config, active_url.as_deref())?;
        println!("Codex 与 Claude Code 已通过 Headroom 路由：{result}");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--configure-claude") {
        let config = app.inner.lock().unwrap().config.clone();
        config::sync_claude(&config)?;
        println!("Claude Code 已通过 Headroom 路由");
        unsafe {
            CloseHandle(mutex);
        }
        return Ok(());
    }

    let startup_config = app.inner.lock().unwrap().config.clone();
    if runtime::find_valid_python(&startup_config).is_some() {
        let startup_url = app.active_url();
        if let Err(error) = config::sync_all(&startup_config, startup_url.as_deref()) {
            app.inner.lock().unwrap().last_error = Some(format!("首次路由同步失败: {error}"));
        }
    } else {
        app.inner.lock().unwrap().headroom_state = "准备运行环境".into();
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
    if let Some(action) = app.maintenance_action.lock().unwrap().take() {
        let mut cfg = app.inner.lock().unwrap().config.clone();
        match action.as_str() {
            "restore" => {
                config::restore_clients(&cfg)?;
                show_info("Codex 与 Claude Code 配置已恢复");
            }
            "repair" => {
                let python = repair_runtime_with_progress(&cfg)?;
                cfg.headroom_python = Some(python);
                config::save(&config_path, &cfg)?;
                show_info("Headroom 运行环境修复完成，请重新启动程序");
            }
            "uninstall" => {
                runtime::uninstall(&cfg)?;
                show_info("已恢复配置并删除托管环境；程序文件将在 Windows 重启后清理");
            }
            _ => {}
        }
    }
    unsafe {
        CloseHandle(mutex);
    }
    tray_result
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn show_info(message: &str) {
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(message).as_ptr(),
            wide("Headroom Route").as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn repair_runtime_with_progress(config: &model::AppConfig) -> Result<PathBuf> {
    let progress = progress::ProgressWindow::open("修复 Headroom 运行环境", "正在准备修复")?;
    let result = runtime::repair_runtime(config, |status| progress.set_status(status));
    progress.close();
    result
}

#[allow(dead_code)]
fn app_data_dir() -> PathBuf {
    model::AppConfig::default().state_dir
}
