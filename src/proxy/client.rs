use super::*;

pub(super) fn handle(
    mut stream: TcpStream,
    app: &Arc<AppState>,
    token: &str,
    client: &Client,
) -> Result<()> {
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
    Ok(())
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
