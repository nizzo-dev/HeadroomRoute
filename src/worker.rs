use crate::{
    config,
    model::HeadroomMetrics,
    runtime,
    state::{AppState, should_stop},
    updater,
};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
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
        status_loop(app.clone()),
        update_loop(app),
    ]
}

fn update_loop(app: Arc<AppState>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        sleep_interruptible(&app, Duration::from_secs(10));
        while !should_stop(&app) {
            if let Ok(Some(config)) = app.begin_daily_update_check(chrono::Utc::now())
                && let Ok(Some(message)) = updater::check_background(&config)
            {
                *app.update_notice.lock().unwrap() = Some(message);
            }
            sleep_interruptible(&app, Duration::from_secs(60 * 60));
        }
    })
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
        let mut log_offset = app.inner.lock().unwrap().config.metrics_log_offset;
        let mut pending_log = Vec::new();
        let mut metrics = HeadroomMetrics::default();
        let routing_available = {
            let config = app.inner.lock().unwrap().config.clone();
            config.bypass_headroom || runtime::find_valid_python(&config).is_some()
        };
        let mut cc_switch_modified = app
            .inner
            .lock()
            .unwrap()
            .config
            .cc_switch_db
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        let mut routing_drift_active = false;
        while !should_stop(&app) {
            if app.reset_metrics.swap(false, Ordering::Acquire) {
                let state = app.inner.lock().unwrap();
                log_offset = state.config.metrics_log_offset;
                metrics = HeadroomMetrics::default();
                pending_log.clear();
            }
            let log_file = app
                .inner
                .lock()
                .unwrap()
                .config
                .state_dir
                .join("headroom-proxy.jsonl");
            if update_headroom_metrics(&log_file, &mut log_offset, &mut pending_log, &mut metrics)
                .is_ok()
            {
                app.inner.lock().unwrap().headroom_metrics = metrics;
            }
            if routing_available && !app.sync_in_progress.load(Ordering::Acquire) {
                let cfg = app.inner.lock().unwrap().config.clone();
                let drifted = config::routing_drifted(&cfg);
                if should_repair_drift(&mut routing_drift_active, drifted) {
                    let preferred = app.active_url();
                    let result = config::sync_all(&cfg, preferred.as_deref());
                    let (ok, message) = match result {
                        Ok(_) => (
                            true,
                            "检测到 CLI 路由配置被外部修改；HeadroomRoute 接管配置已恢复".into(),
                        ),
                        Err(error) => {
                            let message =
                                format!("检测到 CLI 路由配置被外部修改，但恢复失败: {error}");
                            app.inner.lock().unwrap().last_error = Some(message.clone());
                            (false, message)
                        }
                    };
                    *app.routing_notice.lock().unwrap() = Some((ok, message));
                }
            }
            let cc_switch_db = app.inner.lock().unwrap().config.cc_switch_db.clone();
            let current_modified = cc_switch_db
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok();
            if database_changed(&mut cc_switch_modified, current_modified) {
                *app.config_change_notice.lock().unwrap() = Some(
                    "CC-Switch Provider 配置已变化；请从托盘执行“同步 Codex + Claude / CC-Switch”"
                        .into(),
                );
            }
            let _ = app.write_status();
            sleep_interruptible(&app, Duration::from_secs(2));
        }
    })
}

fn should_repair_drift(active: &mut bool, drifted: bool) -> bool {
    if !drifted {
        *active = false;
        return false;
    }
    if *active {
        return false;
    }
    *active = true;
    true
}

fn database_changed(
    previous: &mut Option<std::time::SystemTime>,
    current: Option<std::time::SystemTime>,
) -> bool {
    let Some(current) = current else { return false };
    let changed = previous.is_none_or(|previous| previous != current);
    *previous = Some(current);
    changed
}

fn update_headroom_metrics(
    path: &Path,
    offset: &mut u64,
    pending: &mut Vec<u8>,
    metrics: &mut HeadroomMetrics,
) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() < *offset {
        *offset = 0;
        pending.clear();
        *metrics = HeadroomMetrics::default();
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    *offset += bytes.len() as u64;
    pending.extend(bytes);
    let Some(end) = pending.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(());
    };
    let incomplete = pending.split_off(end + 1);
    let complete = std::mem::replace(pending, incomplete);
    for line in complete.split(|byte| *byte == b'\n') {
        aggregate_log_line(line, metrics);
    }
    Ok(())
}

fn aggregate_log_line(line: &[u8], metrics: &mut HeadroomMetrics) {
    let Ok(serde_json::Value::Object(entry)) = serde_json::from_slice(line) else {
        return;
    };
    let original = entry.get("input_tokens_original").and_then(|v| v.as_u64());
    let optimized = entry.get("input_tokens_optimized").and_then(|v| v.as_u64());
    if original.is_none() && !entry.contains_key("error") {
        return;
    }
    metrics.completed_requests = metrics.completed_requests.saturating_add(1);
    if entry.get("error").is_some_and(|error| !error.is_null()) {
        metrics.failed_requests = metrics.failed_requests.saturating_add(1);
    }
    if let (Some(original), Some(optimized)) = (original, optimized) {
        metrics.input_tokens_original = metrics.input_tokens_original.saturating_add(original);
        metrics.input_tokens_optimized = metrics.input_tokens_optimized.saturating_add(optimized);
        metrics.tokens_saved = metrics
            .tokens_saved
            .saturating_add(original.saturating_sub(optimized));
    }
}

fn headroom_loop(app: Arc<AppState>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let initial_config = app.inner.lock().unwrap().config.clone();
        let Some(python) = runtime::find_valid_python(&initial_config) else {
            let message = runtime::setup_instructions(&initial_config);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_completed_headroom_requests() {
        let mut metrics = HeadroomMetrics::default();
        aggregate_log_line(
            br#"{"input_tokens_original":100,"input_tokens_optimized":70,"error":null}"#,
            &mut metrics,
        );
        aggregate_log_line(br#"{"error":"upstream failed"}"#, &mut metrics);
        assert_eq!(metrics.completed_requests, 2);
        assert_eq!(metrics.failed_requests, 1);
        assert_eq!(metrics.tokens_saved, 30);
        assert_eq!(metrics.compression_percent(), 30.0);
        assert_eq!(metrics.failure_percent(), 50.0);
    }

    #[test]
    fn persisted_offset_excludes_cleared_metrics() {
        let path = std::env::temp_dir().join(format!(
            "headroom-route-metrics-offset-{}.jsonl",
            std::process::id()
        ));
        let old = b"{\"input_tokens_original\":100,\"input_tokens_optimized\":70,\"error\":null}\n";
        let new = b"{\"input_tokens_original\":50,\"input_tokens_optimized\":30,\"error\":null}\n";
        let mut contents = old.to_vec();
        contents.extend(new);
        std::fs::write(&path, contents).unwrap();
        let mut offset = old.len() as u64;
        let mut pending = Vec::new();
        let mut metrics = HeadroomMetrics::default();
        update_headroom_metrics(&path, &mut offset, &mut pending, &mut metrics).unwrap();
        assert_eq!(metrics.completed_requests, 1);
        assert_eq!(metrics.tokens_saved, 20);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reports_each_database_version_once() {
        let first = std::time::SystemTime::UNIX_EPOCH;
        let second = first + Duration::from_secs(1);
        let mut previous = Some(first);
        assert!(!database_changed(&mut previous, Some(first)));
        assert!(database_changed(&mut previous, Some(second)));
        assert!(!database_changed(&mut previous, Some(second)));
        assert!(!database_changed(&mut previous, None));
    }

    #[test]
    fn repairs_each_drift_episode_once() {
        let mut active = false;
        assert!(should_repair_drift(&mut active, true));
        assert!(!should_repair_drift(&mut active, true));
        assert!(!should_repair_drift(&mut active, false));
        assert!(should_repair_drift(&mut active, true));
    }
}
