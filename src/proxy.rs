use crate::{
    config,
    model::AuthStyle,
    notification,
    state::{AppState, should_stop},
};
use anyhow::{Context, Result, anyhow};
use rand::{Rng, distr::Alphanumeric};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn load_or_create_token(state_dir: &Path, legacy_dir: &Path) -> Result<String> {
    let path = state_dir.join("control.token");
    if let Ok(token) = fs::read_to_string(&path)
        && token.trim().len() >= 32
    {
        mirror_token(legacy_dir, token.trim())?;
        return Ok(token.trim().to_owned());
    }
    fs::create_dir_all(state_dir)?;
    let token: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    fs::write(&path, &token)?;
    mirror_token(legacy_dir, &token)?;
    Ok(token)
}

fn mirror_token(dir: &Path, token: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("control.token"), token)?;
    Ok(())
}

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

fn configure_client_stream(stream: &TcpStream) -> std::io::Result<()> {
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

struct Incoming {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Incoming> {
    let mut data = Vec::with_capacity(8192);
    let mut buffer = [0u8; 8192];
    let header_end;
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(anyhow!("连接在请求头完成前关闭"));
        }
        data.extend_from_slice(&buffer[..count]);
        if let Some(index) = data.windows(4).position(|part| part == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        if data.len() > MAX_HEADER {
            return Err(anyhow!("请求头过大"));
        }
    }
    let mut parsed_headers = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut parsed_headers);
    request
        .parse(&data[..header_end])
        .context("HTTP 请求头无法解析")?;
    let method = request
        .method
        .ok_or_else(|| anyhow!("缺少 HTTP 方法"))?
        .to_owned();
    let target = request
        .path
        .ok_or_else(|| anyhow!("缺少请求路径"))?
        .to_owned();
    let headers: HashMap<String, String> = request
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err(anyhow!("请求体超过 32 MiB"));
    }
    let mut body = data[header_end..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            if let Some(decoded) = decode_chunked(&body)? {
                body = decoded;
                break;
            }
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                return Err(anyhow!("分块请求体未完成"));
            }
            body.extend_from_slice(&buffer[..count]);
            if body.len() > MAX_BODY + MAX_HEADER {
                return Err(anyhow!("请求体超过 32 MiB"));
            }
        }
    } else {
        while body.len() < length {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..count]);
        }
        body.truncate(length);
    }
    Ok(Incoming {
        method,
        target,
        headers,
        body,
    })
}

fn handle(mut stream: TcpStream, app: &Arc<AppState>, token: &str, client: &Client) -> Result<()> {
    let request = match read_request(&mut stream) {
        Ok(value) => value,
        Err(error) => {
            write_json(
                &mut stream,
                400,
                serde_json::json!({"error": error.to_string()}),
            )?;
            return Ok(());
        }
    };
    let path = request.target.split('?').next().unwrap_or("/").to_owned();
    if path.starts_with("/_headroom_route_agent/") || path == "/livez" {
        return control(&mut stream, app, token, request, &path);
    }
    let model = top_level_model(&request.body);
    if let Err(error) = app.route_for_request(&path, model.as_deref()) {
        app.inner.lock().unwrap().last_error =
            Some(format!("路由策略决策失败，沿用当前 Provider: {error}"));
    }
    let Some(route) = app.active_route_for_path(&path) else {
        notify_ai_error(&path, "HeadroomRoute", "没有可用的上游路由");
        write_json(
            &mut stream,
            503,
            serde_json::json!({"error":"没有可用上游"}),
        )?;
        return Ok(());
    };
    let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
    let url = join_url(&route.base_url, &request.target)?;
    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        if should_forward_request_header(name, route.api_key.is_some()) {
            builder = builder.header(name, value);
        }
    }
    if let Some(key) = &route.api_key {
        builder = match route.auth_style {
            AuthStyle::Bearer => builder.bearer_auth(key),
            AuthStyle::XApiKey => builder.header("x-api-key", key),
            AuthStyle::PassThrough => builder,
        };
    }
    let started = Instant::now();
    let response = match builder.body(request.body).send() {
        Ok(value) => {
            let status = value.status().as_u16();
            let ok = !is_route_failure(status);
            app.record_route_result(
                route.protocol,
                &route.provider,
                ok,
                started.elapsed().as_millis() as u64,
                Some(status),
                (!ok).then(|| format!("HTTP {status}")),
                true,
            );
            value
        }
        Err(error) => {
            notify_ai_error(&path, &route.provider, &error.to_string());
            app.record_route_result(
                route.protocol,
                &route.provider,
                false,
                started.elapsed().as_millis() as u64,
                None,
                Some(error.to_string()),
                true,
            );
            write_json(
                &mut stream,
                502,
                serde_json::json!({"error":"上游连接失败","route":route.host()}),
            )?;
            return Ok(());
        }
    };
    let status = response.status();
    let is_success = status.is_success();
    if !is_success {
        notify_ai_error(
            &path,
            &route.provider,
            &format!("返回 HTTP {}", status.as_u16()),
        );
    }
    stream.write_all(
        format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Response")
        )
        .as_bytes(),
    )?;
    for (name, value) in response.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if !is_hop_header(&lower) && lower != "content-length" {
            stream.write_all(name.as_str().as_bytes())?;
            stream.write_all(b": ")?;
            stream.write_all(value.as_bytes())?;
            stream.write_all(b"\r\n")?;
        }
    }
    stream.write_all(b"Connection: close\r\n\r\n")?;
    let mut body = response;
    if let Err(error) = std::io::copy(&mut body, &mut stream) {
        if is_success {
            notify_ai_error(&path, &route.provider, &error.to_string());
        }
        return Err(error.into());
    }
    if is_success {
        notify_ai_completed(&path, &route.provider, started.elapsed().as_millis());
    }
    Ok(())
}

fn is_ai_conversation_path(path: &str) -> bool {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');

    matches!(
        path,
        "/v1/chat/completions" | "/v1/completions" | "/v1/responses" | "/v1/messages"
    )
}

fn notify_ai_completed(path: &str, provider: &str, elapsed_ms: u128) {
    if is_ai_conversation_path(path) {
        notification::success("AI 回复完成", format!("{} · {} ms", provider, elapsed_ms));
    }
}

fn notify_ai_error(path: &str, provider: &str, detail: &str) {
    if is_ai_conversation_path(path) {
        notification::error(
            "AI 接口错误",
            format!(
                "{}：{}",
                provider,
                config::portability::redact_sensitive_text(detail)
            ),
        );
    }
}

fn top_level_model(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let model = object.get("model")?.as_str()?.trim();
    (!model.is_empty()).then(|| model.chars().take(160).collect())
}

fn control(
    stream: &mut TcpStream,
    app: &Arc<AppState>,
    token: &str,
    request: Incoming,
    path: &str,
) -> Result<()> {
    if path == "/livez" {
        return write_json(
            stream,
            200,
            serde_json::json!({"service":"headroom-route","status":"healthy"}),
        );
    }
    // Compatibility endpoint used only to identify an older local instance.
    // Returns only stable, non-sensitive identity fields: prepare_port needs
    // the `service` value to tell a HeadroomRoute instance apart from the
    // legacy RouteAgent, and nothing else (no last_error, routes, active_url).
    if path == "/_headroom_route_agent/status" && request.method == "GET" {
        return write_json(stream, 200, compat_status(app));
    }
    if !valid_control_token(&request.headers, token) {
        return write_json(
            stream,
            401,
            serde_json::json!({"detail":"invalid control token"}),
        );
    }
    if path == "/_headroom_route_agent/v1/status" {
        if request.method != "GET" {
            return write_json(
                stream,
                405,
                serde_json::json!({"detail":"method not allowed"}),
            );
        }
        return write_json(stream, 200, stable_status(app));
    }
    if let Some(ticket_id) = path.strip_prefix("/_headroom_route_agent/v1/undo/") {
        if request.method != "POST" {
            return write_json(
                stream,
                405,
                serde_json::json!({"detail":"method not allowed"}),
            );
        }
        let confirmation = request
            .headers
            .get("x-route-agent-confirmation")
            .map(String::as_str)
            .unwrap_or_default();
        return match app.undo_switch(ticket_id, confirmation) {
            Ok(()) => write_json(stream, 200, serde_json::to_value(app.snapshot())?),
            Err(error) => write_json(
                stream,
                409,
                serde_json::json!({"detail": error.to_string()}),
            ),
        };
    }
    let mut ok = true;
    if path.ends_with("/check") {
        app.force_probe
            .store(true, std::sync::atomic::Ordering::Relaxed);
    } else if path.ends_with("/toggle-auto") {
        ok = app.toggle_auto_failover().is_ok();
    } else if path.ends_with("/sync-codex") {
        let cfg = app.inner.lock().unwrap().config.clone();
        let active_url = app.active_url();
        if let Err(error) = config::sync_codex(&cfg, active_url.as_deref()) {
            app.inner.lock().unwrap().last_error = Some(error.to_string());
            ok = false;
        }
        app.refresh_routes();
    } else if path.ends_with("/switch-next") {
        ok = app.switch_next();
    } else if path.ends_with("/restart-headroom") {
        app.restart_headroom
            .store(true, std::sync::atomic::Ordering::Relaxed);
    } else if let Some(value) = path
        .rsplit('/')
        .next()
        .filter(|_| path.contains("/switch-index/"))
    {
        ok = value
            .parse::<usize>()
            .ok()
            .is_some_and(|index| app.switch_index(index, "外部菜单手动切换"));
    } else {
        ok = false;
    }
    let status = if ok { 200 } else { 409 };
    write_json(stream, status, serde_json::to_value(app.snapshot())?)
}

fn valid_control_token(headers: &HashMap<String, String>, token: &str) -> bool {
    !token.is_empty() && headers.get("x-route-agent-token").map(String::as_str) == Some(token)
}

/// Minimal, non-sensitive identity for the legacy `/status` endpoint. Only the
/// `service` field is consumed (by `prepare_port` to distinguish a running
/// HeadroomRoute instance from the legacy RouteAgent); the version is included
/// for logging. Sensitive snapshot fields such as `last_error`, `routes` and
/// `active_url` are deliberately omitted.
fn compat_status(app: &AppState) -> serde_json::Value {
    let snapshot = app.snapshot();
    serde_json::json!({
        "service": snapshot.service,
        "version": snapshot.version,
    })
}

fn stable_status(app: &AppState) -> serde_json::Value {
    let snapshot = app.snapshot();
    let history_entries = app.operation_history().len();
    let pending_undo = app.pending_undo_ticket().map(|ticket| {
        serde_json::json!({
            "id": ticket.id,
            "protocol": ticket.protocol,
            "restore_provider": ticket.restore_provider,
            "expires_at": ticket.expires_at,
        })
    });
    serde_json::json!({
        "schema_version": config::portability::LOCAL_STATUS_API_VERSION,
        "service": snapshot.service,
        "version": snapshot.version,
        "health": snapshot.state,
        "protocols": {
            "codex": {
                "availability": snapshot.codex_availability,
                "mode": protocol_mode(snapshot.direct_codex, snapshot.bypass_headroom),
                "active_name": snapshot.active_name,
                "active_host": snapshot.active_host,
                "latency_ms": snapshot.latency_ms,
            },
            "claude": {
                "availability": snapshot.claude_availability,
                "mode": protocol_mode(snapshot.direct_claude, snapshot.bypass_headroom),
                "active_name": snapshot.active_anthropic_name,
                "active_host": snapshot.active_anthropic_host,
                "latency_ms": snapshot.anthropic_latency_ms,
            }
        },
        "automation": {
            "auto_failover": snapshot.auto_enabled,
        },
        "runtime": {
            "headroom_state": snapshot.headroom_state,
            "headroom_pid": snapshot.headroom_pid,
        },
        "operations": {
            "sync": snapshot.sync_status,
            "restart": snapshot.restart_status,
            "history_entries": history_entries,
            "pending_undo": pending_undo,
        },
        "last_error": snapshot
            .last_error
            .as_deref()
            .map(config::portability::redact_sensitive_text),
    })
}

fn protocol_mode(direct: bool, bypass: bool) -> &'static str {
    if direct {
        "direct"
    } else if bypass {
        "bypass"
    } else {
        "managed"
    }
}

fn join_url(base: &str, target: &str) -> Result<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut url = url::Url::parse(base)?;
    let base_path = url.path().trim_end_matches('/');
    let joined = if (path == "/v1" || path.starts_with("/v1/")) && base_path.ends_with("/v1") {
        format!("{}{}", base_path, &path[3..])
    } else {
        format!("{}{}", base_path, path)
    };
    url.set_path(&joined);
    url.set_query((!query.is_empty()).then_some(query));
    Ok(url.to_string())
}

fn is_hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn should_forward_request_header(name: &str, override_authorization: bool) -> bool {
    !is_hop_header(name)
        && !matches!(name, "host" | "content-length" | "x-headroom-base-url")
        && !(override_authorization && matches!(name, "authorization" | "x-api-key"))
}

fn is_route_failure(status: u16) -> bool {
    matches!(status, 401 | 403 | 408 | 429) || status >= 500
}

fn write_json(stream: &mut TcpStream, status: u16, value: serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(&value)?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        409 => "Conflict",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Response",
    };
    stream.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())?;
    stream.write_all(&body)?;
    Ok(())
}

fn decode_chunked(raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut offset = 0usize;
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = raw[offset..]
            .windows(2)
            .position(|part| part == b"\r\n")
            .map(|index| offset + index)
        else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&raw[offset..line_end])?
            .split(';')
            .next()
            .unwrap_or_default();
        let size = usize::from_str_radix(size_text.trim(), 16).context("无效分块长度")?;
        offset = line_end + 2;
        if size == 0 {
            if raw.len() < offset + 2 {
                return Ok(None);
            }
            return Ok(Some(decoded));
        }
        if decoded.len() + size > MAX_BODY {
            return Err(anyhow!("请求体超过 32 MiB"));
        }
        if raw.len() < offset + size + 2 {
            return Ok(None);
        }
        decoded.extend_from_slice(&raw[offset..offset + size]);
        offset += size;
        if &raw[offset..offset + 2] != b"\r\n" {
            return Err(anyhow!("无效分块边界"));
        }
        offset += 2;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        compat_status, configure_client_stream, is_route_failure, join_url, read_request,
        should_forward_request_header, stable_status, top_level_model, valid_control_token,
    };
    use std::{
        collections::HashMap,
        io::Write,
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    #[test]
    fn versioned_status_api_requires_exact_control_token() {
        let mut headers = HashMap::new();
        assert!(!valid_control_token(&headers, "control-secret"));
        headers.insert("x-route-agent-token".into(), "wrong".into());
        assert!(!valid_control_token(&headers, "control-secret"));
        headers.insert("x-route-agent-token".into(), "control-secret".into());
        assert!(valid_control_token(&headers, "control-secret"));
        assert!(!valid_control_token(&headers, ""));
    }

    #[test]
    fn stable_status_exposes_stable_redacted_fields() {
        use crate::model::AppConfig;
        use crate::state::AppState;
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-status-api-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("codex.toml");
        std::fs::write(
            &codex,
            "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.join("state");
        config.codex_config = codex;
        config.claude_settings = dir.join("missing-claude.json");
        config.cc_switch_db = dir.join("missing.db");
        config.enable_claude = false;
        let app = AppState::new(config);
        let secret = "sk-status-secret-0123456789abcdef";
        app.inner.lock().unwrap().last_error = Some(format!("Bearer {secret}"));
        let value = stable_status(&app);
        let schema = value.get("schema_version").unwrap();
        assert_eq!(
            schema.as_u64(),
            Some(crate::config::portability::LOCAL_STATUS_API_VERSION as u64)
        );
        assert_eq!(value["service"], "headroom-route");
        assert!(value["health"].is_string());
        for protocol in ["codex", "claude"] {
            let fields = &value["protocols"][protocol];
            for field in [
                "availability",
                "mode",
                "active_name",
                "active_host",
                "latency_ms",
            ] {
                assert!(fields.get(field).is_some(), "{protocol}.{field}");
            }
        }
        assert!(value["automation"]["auto_failover"].is_boolean());
        assert!(value["runtime"]["headroom_state"].is_string());
        assert!(value["operations"]["sync"].is_string());
        assert!(value["operations"]["restart"].is_string());
        assert_eq!(value["last_error"], "Bearer [REDACTED]");
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("api_key"));
        assert!(!serialized.contains("token"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compat_status_exposes_only_service_and_version() {
        use crate::model::AppConfig;
        use crate::state::AppState;
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-compat-status-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.join("state");
        config.codex_config = dir.join("missing-codex.toml");
        config.claude_settings = dir.join("missing-claude.json");
        config.cc_switch_db = dir.join("missing.db");
        let app = AppState::new(config);
        app.inner.lock().unwrap().last_error = Some("Bearer sk-status-secret-0123456789".into());
        let value = compat_status(&app);
        assert_eq!(value["service"], "headroom-route");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        for sensitive in [
            "last_error",
            "routes",
            "active_url",
            "active_provider",
            "active_name",
            "health",
            "headroom_state",
        ] {
            assert!(value.get(sensitive).is_none(), "should omit {sensitive}");
        }
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(!serialized.contains("sk-status-secret"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn joins_openai_paths_without_duplicate_v1() {
        assert_eq!(
            join_url("https://example.com/v1", "/v1/responses?x=1").unwrap(),
            "https://example.com/v1/responses?x=1"
        );
        assert_eq!(
            join_url("https://example.com", "/v1/models").unwrap(),
            "https://example.com/v1/models"
        );
    }

    #[test]
    fn model_selection_reads_only_the_json_top_level() {
        assert_eq!(
            top_level_model(br#"{"model":"gpt-4o","input":[{"model":"nested"}]}"#),
            Some("gpt-4o".into())
        );
        assert_eq!(top_level_model(br#"{"input":{"model":"nested"}}"#), None);
        assert_eq!(top_level_model(b"not-json"), None);
    }

    #[test]
    fn decodes_chunked_request_body() {
        assert_eq!(
            super::decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(),
            Some(b"Wikipedia".to_vec())
        );
        assert_eq!(super::decode_chunked(b"4\r\nWi").unwrap(), None);
    }

    #[test]
    fn replaces_incoming_authorization_when_route_has_key() {
        assert!(!should_forward_request_header("authorization", true));
        assert!(!should_forward_request_header("x-api-key", true));
        assert!(should_forward_request_header("authorization", false));
        assert!(!should_forward_request_header("x-headroom-base-url", false));
        assert!(should_forward_request_header("content-type", true));
    }

    #[test]
    fn only_route_failures_count_toward_failover() {
        assert!(!is_route_failure(400));
        assert!(!is_route_failure(404));
        assert!(is_route_failure(401));
        assert!(is_route_failure(429));
        assert!(is_route_failure(502));
    }

    #[test]
    fn recognizes_ai_conversation_paths() {
        for path in [
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/responses",
            "/v1/messages/",
            "/v1/responses?stream=true",
        ] {
            assert!(super::is_ai_conversation_path(path), "{path}");
        }

        for path in [
            "/v1/models",
            "/v1/embeddings",
            "/v1/responses/status",
            "/healthz",
        ] {
            assert!(!super::is_ai_conversation_path(path), "{path}");
        }
    }

    #[test]
    fn accepted_client_waits_for_delayed_request_bytes() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut client = TcpStream::connect(address).unwrap();
            thread::sleep(Duration::from_millis(75));
            client.write_all(b"POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        });

        let mut accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        // Force the Windows failure mode even on platforms where accept does
        // not inherit it, then verify our accepted-socket setup repairs it.
        accepted.set_nonblocking(true).unwrap();
        configure_client_stream(&accepted).unwrap();
        let request = read_request(&mut accepted).unwrap();
        assert_eq!(request.target, "/v1/responses");
        assert_eq!(request.body, b"{}");
        writer.join().unwrap();
    }
}
