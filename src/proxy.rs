use crate::{config, model::AuthStyle, state::{AppState, should_stop}};
use anyhow::{Context, Result, anyhow};
use rand::{Rng, distr::Alphanumeric};
use reqwest::blocking::Client;
use std::{collections::HashMap, fs, io::{Read, Write}, net::{TcpListener, TcpStream}, path::Path, sync::Arc, thread, time::{Duration, Instant}};

const MAX_HEADER: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn load_or_create_token(state_dir: &Path, legacy_dir: &Path) -> Result<String> {
    let path = state_dir.join("control.token");
    if let Ok(token) = fs::read_to_string(&path) {
        if token.trim().len() >= 32 { mirror_token(legacy_dir, token.trim())?; return Ok(token.trim().to_owned()); }
    }
    fs::create_dir_all(state_dir)?;
    let token: String = rand::rng().sample_iter(Alphanumeric).take(48).map(char::from).collect();
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
    prepare_port(port)?;
    let listener = TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("无法监听 127.0.0.1:{port}"))?;
    listener.set_nonblocking(true)?;
    let mut client_builder = Client::builder().connect_timeout(Duration::from_secs(10)).timeout(None).pool_max_idle_per_host(4);
    if let Some(proxy) = config::reqwest_outbound_proxy(&runtime_config)? { client_builder = client_builder.proxy(proxy); }
    let client = client_builder.build()?;
    Ok(thread::spawn(move || {
        while !should_stop(&app) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = configure_client_stream(&stream) {
                        app.inner.lock().unwrap().last_error = Some(format!("代理客户端连接配置失败: {error}"));
                        continue;
                    }
                    let app = app.clone(); let token = token.clone(); let client = client.clone();
                    thread::spawn(move || { let _ = handle(stream, &app, &token, &client); });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
                Err(error) => { app.inner.lock().unwrap().last_error = Some(format!("代理监听失败: {error}")); thread::sleep(Duration::from_secs(1)); }
            }
        }
    }))
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
    if TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse()?, Duration::from_millis(300)).is_err() {
        return Ok(());
    }
    let client = Client::builder().timeout(Duration::from_secs(2)).build()?;
    let status: serde_json::Value = client.get(format!("http://127.0.0.1:{port}/_headroom_route_agent/status"))
        .send().context("控制端口已占用，且无法验证旧 RouteAgent")?
        .json().context("控制端口已占用，但响应不是 RouteAgent 状态")?;
    let service = status.get("service").and_then(serde_json::Value::as_str).unwrap_or_default();
    if service == "headroom-route" { return Err(anyhow!("另一个 Headroom Route 实例已经运行")); }
    if service != "headroom-route-agent" { return Err(anyhow!("端口 {port} 被其他程序占用，拒绝自动结束")); }
    let pid = listener_pid(port).ok_or_else(|| anyhow!("已验证旧 RouteAgent，但无法确定监听进程"))?;
    terminate_pid(pid).context("无法结束已验证的旧 RouteAgent")?;
    for _ in 0..40 {
        if TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse()?, Duration::from_millis(100)).is_err() { return Ok(()); }
        thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("旧 RouteAgent 未能释放端口 {port}"))
}

fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(windows)] use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("netstat.exe").args(["-ano", "-p", "tcp"]).creation_flags(0x08000000).output().ok()?;
    let suffix = format!(":{port}");
    String::from_utf8_lossy(&output.stdout).lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        (fields.len() == 5 && fields[0].eq_ignore_ascii_case("TCP") && fields[1].ends_with(&suffix) && fields[3].eq_ignore_ascii_case("LISTENING")).then(|| fields[4].parse().ok()).flatten()
    })
}

fn terminate_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)] use std::os::windows::process::CommandExt;
    let status = std::process::Command::new("taskkill.exe").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(0x08000000).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other("taskkill failed")) }
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
        if count == 0 { return Err(anyhow!("连接在请求头完成前关闭")); }
        data.extend_from_slice(&buffer[..count]);
        if let Some(index) = data.windows(4).position(|part| part == b"\r\n\r\n") { header_end = index + 4; break; }
        if data.len() > MAX_HEADER { return Err(anyhow!("请求头过大")); }
    }
    let mut parsed_headers = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut parsed_headers);
    request.parse(&data[..header_end]).context("HTTP 请求头无法解析")?;
    let method = request.method.ok_or_else(|| anyhow!("缺少 HTTP 方法"))?.to_owned();
    let target = request.path.ok_or_else(|| anyhow!("缺少请求路径"))?.to_owned();
    let headers: HashMap<String, String> = request.headers.iter().map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).into_owned())).collect();
    let length = headers.get("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if length > MAX_BODY { return Err(anyhow!("请求体超过 32 MiB")); }
    let mut body = data[header_end..].to_vec();
    if headers.get("transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked")) {
        loop {
            if let Some(decoded) = decode_chunked(&body)? { body = decoded; break; }
            let count = stream.read(&mut buffer)?;
            if count == 0 { return Err(anyhow!("分块请求体未完成")); }
            body.extend_from_slice(&buffer[..count]);
            if body.len() > MAX_BODY + MAX_HEADER { return Err(anyhow!("请求体超过 32 MiB")); }
        }
    } else {
        while body.len() < length {
            let count = stream.read(&mut buffer)?;
            if count == 0 { break; }
            body.extend_from_slice(&buffer[..count]);
        }
        body.truncate(length);
    }
    Ok(Incoming { method, target, headers, body })
}

fn handle(mut stream: TcpStream, app: &Arc<AppState>, token: &str, client: &Client) -> Result<()> {
    let request = match read_request(&mut stream) {
        Ok(value) => value,
        Err(error) => { write_json(&mut stream, 400, serde_json::json!({"error": error.to_string()}))?; return Ok(()); }
    };
    let path = request.target.split('?').next().unwrap_or("/").to_owned();
    if path.starts_with("/_headroom_route_agent/") || path == "/livez" {
        return control(&mut stream, app, token, request, &path);
    }
    let Some(route) = app.active_route_for_path(&path) else {
        write_json(&mut stream, 503, serde_json::json!({"error":"没有可用上游"}))?;
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
            app.record_route_result(route.protocol, &route.base_url, status < 400, started.elapsed().as_millis() as u64, Some(status), (status >= 400).then(|| format!("HTTP {status}")), true);
            value
        }
        Err(error) => {
            app.record_route_result(route.protocol, &route.base_url, false, started.elapsed().as_millis() as u64, None, Some(error.to_string()), true);
            write_json(&mut stream, 502, serde_json::json!({"error":"上游连接失败","route":route.host()}))?;
            return Ok(());
        }
    };
    let status = response.status();
    stream.write_all(format!("HTTP/1.1 {} {}\r\n", status.as_u16(), status.canonical_reason().unwrap_or("Response")).as_bytes())?;
    for (name, value) in response.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if !is_hop_header(&lower) && lower != "content-length" {
            stream.write_all(name.as_str().as_bytes())?; stream.write_all(b": ")?; stream.write_all(value.as_bytes())?; stream.write_all(b"\r\n")?;
        }
    }
    stream.write_all(b"Connection: close\r\n\r\n")?;
    let mut body = response;
    std::io::copy(&mut body, &mut stream)?;
    Ok(())
}

fn control(stream: &mut TcpStream, app: &Arc<AppState>, token: &str, request: Incoming, path: &str) -> Result<()> {
    if path == "/livez" {
        return write_json(stream, 200, serde_json::json!({"service":"headroom-route","status":"healthy"}));
    }
    if path.ends_with("/status") && request.method == "GET" { return write_json(stream, 200, serde_json::to_value(app.snapshot())?); }
    if request.headers.get("x-route-agent-token").map(String::as_str) != Some(token) {
        return write_json(stream, 401, serde_json::json!({"detail":"invalid control token"}));
    }
    let mut ok = true;
    if path.ends_with("/check") { app.force_probe.store(true, std::sync::atomic::Ordering::Relaxed); }
    else if path.ends_with("/toggle-auto") { ok = false; }
    else if path.ends_with("/sync-codex") {
        let cfg = app.inner.lock().unwrap().config.clone();
        let active_url = app.active_url();
        if let Err(error) = config::sync_codex(&cfg, active_url.as_deref()) { app.inner.lock().unwrap().last_error = Some(error.to_string()); ok = false; }
        app.refresh_routes();
    }
    else if path.ends_with("/switch-next") { ok = app.switch_next(); }
    else if path.ends_with("/restart-headroom") { app.restart_headroom.store(true, std::sync::atomic::Ordering::Relaxed); }
    else if let Some(value) = path.rsplit('/').next().filter(|_| path.contains("/switch-index/")) { ok = value.parse::<usize>().ok().is_some_and(|index| app.switch_index(index, "外部菜单手动切换")); }
    else { ok = false; }
    let status = if ok { 200 } else { 409 };
    write_json(stream, status, serde_json::to_value(app.snapshot())?)
}

fn join_url(base: &str, target: &str) -> Result<String> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut url = url::Url::parse(base)?;
    let base_path = url.path().trim_end_matches('/');
    let joined = if (path == "/v1" || path.starts_with("/v1/")) && base_path.ends_with("/v1") {
        format!("{}{}", base_path, &path[3..])
    } else { format!("{}{}", base_path, path) };
    url.set_path(&joined);
    url.set_query((!query.is_empty()).then_some(query));
    Ok(url.to_string())
}

fn is_hop_header(name: &str) -> bool {
    matches!(name, "connection"|"keep-alive"|"proxy-authenticate"|"proxy-authorization"|"te"|"trailer"|"transfer-encoding"|"upgrade")
}

fn should_forward_request_header(name: &str, override_authorization: bool) -> bool {
    !is_hop_header(name)
        && !matches!(name, "host" | "content-length" | "x-headroom-base-url")
        && !(override_authorization && matches!(name, "authorization" | "x-api-key"))
}

fn write_json(stream: &mut TcpStream, status: u16, value: serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(&value)?;
    let reason = match status { 200=>"OK",400=>"Bad Request",401=>"Unauthorized",409=>"Conflict",413=>"Payload Too Large",502=>"Bad Gateway",503=>"Service Unavailable",_=>"Response" };
    stream.write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())?;
    stream.write_all(&body)?;
    Ok(())
}

fn decode_chunked(raw: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut offset = 0usize;
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = raw[offset..].windows(2).position(|part| part == b"\r\n").map(|index| offset + index) else { return Ok(None) };
        let size_text = std::str::from_utf8(&raw[offset..line_end])?.split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_text.trim(), 16).context("无效分块长度")?;
        offset = line_end + 2;
        if size == 0 {
            if raw.len() < offset + 2 { return Ok(None); }
            return Ok(Some(decoded));
        }
        if decoded.len() + size > MAX_BODY { return Err(anyhow!("请求体超过 32 MiB")); }
        if raw.len() < offset + size + 2 { return Ok(None); }
        decoded.extend_from_slice(&raw[offset..offset + size]);
        offset += size;
        if &raw[offset..offset + 2] != b"\r\n" { return Err(anyhow!("无效分块边界")); }
        offset += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::{configure_client_stream, join_url, read_request, should_forward_request_header};
    use std::{io::Write, net::{TcpListener, TcpStream}, thread, time::Duration};

    #[test]
    fn joins_openai_paths_without_duplicate_v1() {
        assert_eq!(join_url("https://example.com/v1", "/v1/responses?x=1").unwrap(), "https://example.com/v1/responses?x=1");
        assert_eq!(join_url("https://example.com", "/v1/models").unwrap(), "https://example.com/v1/models");
    }

    #[test]
    fn decodes_chunked_request_body() {
        assert_eq!(super::decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), Some(b"Wikipedia".to_vec()));
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
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
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
