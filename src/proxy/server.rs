use super::*;

pub fn run(app: Arc<AppState>, token: String) -> Result<thread::JoinHandle<()>> {
    let runtime_config = app.inner.lock().unwrap().config.clone();
    let port = runtime_config.agent_port;
    if let Err(error) = prepare_port(port) {
        return Err(report_port_conflict(&app, port, error));
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|error| {
        report_port_conflict(
            &app,
            port,
            anyhow!(error).context(format!("无法监听 127.0.0.1:{port}")),
        )
    })?;
    listener.set_nonblocking(true)?;
    let mut client_builder = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(None)
        .pool_max_idle_per_host(4);
    if let Some(proxy) = config::reqwest_outbound_proxy(&runtime_config)? {
        client_builder = client_builder.proxy(proxy);
    }
    let client = client_builder.build()?;
    Ok(thread::spawn(move || {
        while !should_stop(&app) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = configure_client_stream(&stream) {
                        app.inner.lock().unwrap().last_error =
                            Some(format!("代理客户端连接配置失败: {error}"));
                        continue;
                    }
                    let app = app.clone();
                    let token = token.clone();
                    let client = client.clone();
                    thread::spawn(move || {
                        let _ = handle(stream, &app, &token, &client);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(error) => {
                    app.inner.lock().unwrap().last_error = Some(format!("代理监听失败: {error}"));
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }))
}

fn report_port_conflict(app: &AppState, port: u16, error: anyhow::Error) -> anyhow::Error {
    let occupant = listener_occupant(port);
    let detail = occupant.map_or_else(
        || format!("本地代理端口 {port} 被占用，未能确定监听进程；请关闭占用程序后重试"),
        |occupant| {
            format!(
                "本地代理端口 {port} 被 PID {}（{}，{}）占用；为避免结束用户进程，已停止启动",
                occupant.pid,
                occupant.name,
                if occupant.is_headroom {
                    "Headroom 进程"
                } else {
                    "其他进程"
                }
            )
        },
    );
    app.inner.lock().unwrap().last_error = Some(detail.clone());
    app.process_recovery_event(crate::environment_recovery::EnvironmentEvent::PortConflict);
    app.process_recovery_event(crate::environment_recovery::EnvironmentEvent::RecoveryFailed);
    error.context(detail)
}

fn listener_occupant(port: u16) -> Option<crate::environment_recovery::PortOccupant> {
    use std::os::windows::process::CommandExt;

    let pid = listener_pid(port)?;
    let filter = format!("PID eq {pid}");
    let process_name = std::process::Command::new("tasklist.exe")
        .args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
        .creation_flags(0x08000000)
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    if !line.starts_with('"') {
                        return None;
                    }
                    line.split(',')
                        .next()
                        .map(|name| name.trim_matches('"').to_owned())
                })
        })
        .unwrap_or_else(|| format!("proc-{pid}"));
    crate::environment_recovery::classify_port_with_process(
        pid,
        process_name,
        Some(std::process::id()),
    )
}

pub(super) fn configure_client_stream(stream: &TcpStream) -> std::io::Result<()> {
    // A socket accepted by a non-blocking listener can inherit non-blocking mode
    // on Windows. Request parsing runs on a dedicated worker, so it must block
    // until Codex/Headroom has actually sent the request bytes.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))
}

fn prepare_port(port: u16) -> Result<()> {
    if TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse()?,
        Duration::from_millis(300),
    )
    .is_err()
    {
        return Ok(());
    }
    let client = Client::builder().timeout(Duration::from_secs(2)).build()?;
    let status: serde_json::Value = client
        .get(format!(
            "http://127.0.0.1:{port}/_headroom_route_agent/status"
        ))
        .send()
        .context("控制端口已占用，且无法验证旧 RouteAgent")?
        .json()
        .context("控制端口已占用，但响应不是 RouteAgent 状态")?;
    let service = status
        .get("service")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if service == "headroom-route" {
        return Err(anyhow!("另一个 Headroom Route 实例已经运行"));
    }
    if service != "headroom-route-agent" {
        return Err(anyhow!("端口 {port} 被其他程序占用，拒绝自动结束"));
    }
    let pid =
        listener_pid(port).ok_or_else(|| anyhow!("已验证旧 RouteAgent，但无法确定监听进程"))?;
    terminate_pid(pid).context("无法结束已验证的旧 RouteAgent")?;
    for _ in 0..40 {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse()?,
            Duration::from_millis(100),
        )
        .is_err()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("旧 RouteAgent 未能释放端口 {port}"))
}

fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("netstat.exe")
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
    let status = std::process::Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x08000000)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("taskkill failed"))
    }
}
