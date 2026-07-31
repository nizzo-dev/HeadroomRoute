use crate::{
    config, runtime,
    state::{AppState, should_stop},
};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    fs::OpenOptions,
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

const PROBE_INTERVAL: Duration = Duration::from_secs(60);

pub fn start(app: Arc<AppState>) -> Vec<thread::JoinHandle<()>> {
    vec![
        probe_loop(app.clone()),
        headroom_loop(app.clone()),
        status_loop(app),
    ]
}

fn probe_loop(app: Arc<AppState>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let runtime_config = app.inner.lock().unwrap().config.clone();
        let mut client_builder = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8));
        if let Ok(Some(proxy)) = config::reqwest_outbound_proxy(&runtime_config) {
            client_builder = client_builder.proxy(proxy);
        }
        let client = client_builder.build().unwrap();
        while !should_stop(&app) {
            app.refresh_routes();
            let routes = app.inner.lock().unwrap().routes.clone();
            for route in routes {
                if should_stop(&app) {
                    break;
                }
                let started = Instant::now();
                match client
                    .head(&route.base_url)
                    .header("User-Agent", "HeadroomRoute/0.1 health-probe")
                    .send()
                {
                    Ok(response) => app.record_route_result(
                        route.protocol,
                        &route.provider,
                        true,
                        started.elapsed().as_millis() as u64,
                        Some(response.status().as_u16()),
                        None,
                        false,
                    ),
                    Err(error) => app.record_route_result(
                        route.protocol,
                        &route.provider,
                        false,
                        started.elapsed().as_millis() as u64,
                        None,
                        Some(error.to_string()),
                        false,
                    ),
                }
            }
            wait_for_next_probe(&app);
        }
    })
}

fn wait_for_next_probe(app: &AppState) {
    let chunks = (PROBE_INTERVAL.as_millis() / 100) as usize;
    for _ in 0..chunks {
        if should_stop(app) || app.force_probe.swap(false, Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn status_loop(app: Arc<AppState>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let runtime_available = {
            let config = app.inner.lock().unwrap().config.clone();
            runtime::find_valid_python(&config).is_some()
        };
        while !should_stop(&app) {
            if runtime_available && !app.sync_in_progress.load(Ordering::Acquire) {
                let cfg = app.inner.lock().unwrap().config.clone();
                if config::routing_drifted(&cfg) {
                    let preferred = app.active_url();
                    if let Err(error) = config::sync_all(&cfg, preferred.as_deref()) {
                        app.inner.lock().unwrap().last_error =
                            Some(format!("CLI 路由守护失败: {error}"));
                    }
                }
            }
            let _ = app.write_status();
            sleep_interruptible(&app, Duration::from_secs(2));
        }
    })
}

fn headroom_loop(app: Arc<AppState>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let initial_config = app.inner.lock().unwrap().config.clone();
        let Some(python) = runtime::find_valid_python(&initial_config) else {
            let configured = initial_config
                .headroom_python
                .as_deref()
                .map_or_else(|| "未配置".into(), |path| path.display().to_string());
            let message = format!(
                "未找到可用的 Headroom 环境（Python：{configured}）。HeadroomRoute 不会自动安装 Python 或 Headroom；请按 README 准备环境后重新启动。"
            );
            let mut state = app.inner.lock().unwrap();
            state.headroom_state = "runtime-unavailable".into();
            state.last_error = Some(message.clone());
            drop(state);
            *app.runtime_result.lock().unwrap() = Some((false, message));
            return;
        };
        let (saved, path) = {
            let mut state = app.inner.lock().unwrap();
            state.config.headroom_python = Some(python);
            state.headroom_state = "运行环境就绪".into();
            (
                state.config.clone(),
                state.config.state_dir.join("config.json"),
            )
        };
        if let Err(error) = config::save(&path, &saved) {
            app.inner.lock().unwrap().last_error = Some(format!("保存运行环境配置失败: {error}"));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut child: Option<Child> = None;
        let mut restart_deadline: Option<Instant> = None;
        let mut spawn_failures: u32 = 0;
        while !should_stop(&app) {
            let restart = app.restart_headroom.swap(false, Ordering::Relaxed);
            let (port, manage, state_dir) = {
                let state = app.inner.lock().unwrap();
                (
                    state.config.headroom_port,
                    state.config.manage_headroom,
                    state.config.state_dir.clone(),
                )
            };
            let health = client
                .get(format!("http://127.0.0.1:{port}/livez"))
                .send()
                .ok()
                .and_then(|r| r.json::<Value>().ok());
            let verified = health
                .as_ref()
                .and_then(|v| v.get("service"))
                .and_then(Value::as_str)
                == Some("headroom-proxy");
            if restart {
                if !app.restart_in_progress.load(Ordering::Acquire) {
                    let _ = app.begin_restart();
                }
                restart_deadline = Some(Instant::now() + Duration::from_secs(45));
                spawn_failures = 0;
                app.inner.lock().unwrap().headroom_state = "restarting".into();
                if let Some(mut running) = child.take() {
                    let _ = running.kill();
                    let _ = running.wait();
                } else if verified && let Some(pid) = listener_pid(port) {
                    let _ = terminate_pid(pid);
                    for _ in 0..20 {
                        if client
                            .get(format!("http://127.0.0.1:{port}/livez"))
                            .send()
                            .is_err()
                        {
                            break;
                        }
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            let child_dead = child
                .as_mut()
                .is_some_and(|value| value.try_wait().ok().flatten().is_some());
            if child_dead {
                child = None;
                spawn_failures = spawn_failures.saturating_add(1);
                let detail = recent_headroom_error(&state_dir)
                    .unwrap_or_else(|| "Headroom 进程已退出".into());
                let message = format!("Headroom 异常退出: {detail}");
                app.inner.lock().unwrap().last_error = Some(message.clone());
                if restart_deadline.take().is_some() {
                    app.finish_restart(false, message);
                }
            }
            if verified && !restart {
                spawn_failures = 0;
                let mut state = app.inner.lock().unwrap();
                state.headroom_state = if child.is_some() {
                    "healthy"
                } else {
                    "external"
                }
                .into();
                state.headroom_pid = listener_pid(port);
                drop(state);
                if restart_deadline.take().is_some() {
                    app.finish_restart(true, "Headroom 健康检查已恢复".into());
                }
            } else if manage && child.is_none() && spawn_failures < 3 {
                match spawn_headroom(&app) {
                    Ok(process) => {
                        let pid = process.id();
                        child = Some(process);
                        let mut state = app.inner.lock().unwrap();
                        state.headroom_state = "starting".into();
                        state.headroom_pid = Some(pid);
                    }
                    Err(error) => {
                        spawn_failures = spawn_failures.saturating_add(1);
                        let message = format!("Headroom 启动失败: {error}");
                        let mut state = app.inner.lock().unwrap();
                        state.headroom_state = "unavailable".into();
                        state.headroom_pid = None;
                        state.last_error = Some(message.clone());
                        drop(state);
                        if restart_deadline.take().is_some() {
                            app.finish_restart(false, message);
                        }
                    }
                }
            } else if !verified {
                app.inner.lock().unwrap().headroom_state = "unavailable".into();
            }
            if restart_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                restart_deadline = None;
                let detail =
                    recent_headroom_error(&state_dir).unwrap_or_else(|| "未通过健康检查".into());
                app.finish_restart(false, format!("45 秒内未恢复 Headroom: {detail}"));
            }
            sleep_interruptible(&app, Duration::from_secs(3));
        }
        if let Some(mut running) = child {
            let _ = running.kill();
            let _ = running.wait();
        }
    })
}

fn spawn_headroom(app: &AppState) -> anyhow::Result<Child> {
    let state = app.inner.lock().unwrap();
    let python = state
        .config
        .headroom_python
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("未找到 Headroom Python 运行时"))?;
    if !python.exists() {
        anyhow::bail!("运行时不存在: {}", python.display());
    }
    std::fs::create_dir_all(&state.config.state_dir)?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.config.state_dir.join("headroom.stdout.log"))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.config.state_dir.join("headroom.stderr.log"))?;
    let mut command = Command::new(python);
    let agent_url = format!("http://127.0.0.1:{}", state.config.agent_port);
    let port = state.config.headroom_port.to_string();
    let log_file = state.config.state_dir.join("headroom-proxy.jsonl");
    command.args([
        "-m",
        "headroom.cli",
        "proxy",
        "--host",
        "127.0.0.1",
        "--port",
        &port,
        "--mode",
        "token",
        "--code-aware",
        "--intercept-tool-results",
        "--target-ratio",
        "0.35",
        "--no-ccr-proactive-expansion",
        "--read-maturation",
        "--read-maturation-quiesce-turns",
        "2",
        "--read-maturation-max-hold-turns",
        "8",
        "--read-maturation-min-size-bytes",
        "1024",
        "--openai-api-url",
        &agent_url,
        "--anthropic-api-url",
        &agent_url,
        "--log-file",
        log_file.to_string_lossy().as_ref(),
        "--no-telemetry",
        "--no-http2",
    ]);
    if state.config.no_subscription_tracking {
        command.arg("--no-subscription-tracking");
    }
    command
        .env("OPENAI_TARGET_API_URL", &agent_url)
        .env("ANTHROPIC_TARGET_API_URL", &agent_url)
        .env("HEADROOM_OUTPUT_SHAPER", "1")
        .env("HEADROOM_TELEMETRY", "off")
        .env("HEADROOM_VERBOSITY_LEVEL", "4")
        .env("HEADROOM_INTERCEPT_READ_MIN_CHARS", "200")
        .env("HEADROOM_UPDATE_CHECK", "off")
        .env("HEADROOM_SKIP_UPSTREAM_CHECK", "1")
        .env("HEADROOM_HTTP2", "0")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    Ok(command.spawn()?)
}

fn recent_headroom_error(state_dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(state_dir.join("headroom.stderr.log")).ok()?;
    let interesting = text.lines().rev().find(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("importerror")
            || lower.contains("modulenotfounderror")
            || lower.contains("error:")
            || lower.contains("traceback")
            || lower.contains("address already in use")
            || lower.contains("oserror")
    })?;
    Some(interesting.chars().take(240).collect())
}

fn sleep_interruptible(app: &AppState, duration: Duration) {
    let chunks = (duration.as_millis() / 100).max(1);
    for _ in 0..chunks {
        if should_stop(app) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let output = Command::new("netstat.exe")
        .args(["-ano", "-p", "tcp"])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    let suffix = format!(":{port}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.len() == 5
                && fields[0].eq_ignore_ascii_case("TCP")
                && fields[1].ends_with(&suffix)
                && fields[3].eq_ignore_ascii_case("LISTENING"))
            .then(|| fields[4].parse().ok())
            .flatten()
        })
}

fn terminate_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let status = Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x08000000)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("taskkill failed"))
    }
}
