use crate::{
    model::{AppConfig, AuthStyle, Protocol, Route},
    sqlite,
};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs, path::Path};
use toml_edit::{DocumentMut, Item, Table, value};
#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_DWORD, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};

pub struct DiscoveredRoutes {
    pub routes: Vec<Route>,
    pub selected_openai: Option<String>,
    pub selected_anthropic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InstallManifest {
    version: String,
    codex_baseline: Option<String>,
    claude_baseline: Option<String>,
}

pub fn load_or_create(path: &Path) -> Result<AppConfig> {
    if path.exists() {
        return serde_json::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("配置文件无法解析: {}", path.display()));
    }
    let config = AppConfig::default();
    save(path, &config)?;
    Ok(config)
}

pub fn save(path: &Path, config: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(path, &serde_json::to_vec_pretty(config)?)
}

fn table_string(item: &Item, key: &str) -> Option<String> {
    item.get(key)?.as_str().map(str::to_owned)
}

fn parse_codex_text(text: &str, source: &str) -> Result<(Vec<Route>, Option<String>)> {
    let doc = text.parse::<DocumentMut>().context("Codex TOML 无法解析")?;
    let selected = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .map(str::to_owned);
    let mut routes = Vec::new();
    if let Some(providers) = doc.get("model_providers").and_then(Item::as_table_like) {
        for (name, item) in providers.iter() {
            if name == "headroom" {
                continue;
            }
            let Some(base_url) = table_string(item, "base_url") else {
                continue;
            };
            let Ok(base_url) = normalize_url(&base_url) else {
                continue;
            };
            routes.push(Route::new(
                Protocol::OpenAi,
                name.to_owned(),
                name.to_owned(),
                base_url,
                None,
                AuthStyle::Bearer,
                source,
            ));
        }
    }
    Ok((routes, selected))
}

pub fn discover_routes(config: &AppConfig) -> Result<DiscoveredRoutes> {
    let mut routes = Vec::new();
    let selected_openai = config.selected_openai_provider.clone();
    let mut selected_anthropic = config.selected_anthropic_provider.clone();

    if config.cc_switch_db.exists() {
        if config.enable_codex {
            for row in sqlite::providers(&config.cc_switch_db, "codex")? {
                let Ok(settings) = serde_json::from_str::<Value>(&row.settings) else {
                    continue;
                };
                let Some(toml) = settings.get("config").and_then(Value::as_str) else {
                    continue;
                };
                let Ok((mut parsed, _)) = parse_codex_text(toml, "cc-switch") else {
                    continue;
                };
                let api_key = settings
                    .pointer("/auth/OPENAI_API_KEY")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                for route in &mut parsed {
                    route.provider = row.id.clone();
                    route.name = row.name.clone();
                    route.api_key = api_key.clone();
                }
                for route in parsed {
                    push_unique(&mut routes, route);
                }
            }
        }
        if config.enable_claude {
            for row in sqlite::providers(&config.cc_switch_db, "claude")? {
                let Ok(settings) = serde_json::from_str::<Value>(&row.settings) else {
                    continue;
                };
                let Some(env) = settings.get("env").and_then(Value::as_object) else {
                    continue;
                };
                let Some(base) = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) else {
                    continue;
                };
                let Some(base_url) =
                    effective_claude_base_url(base, &row.website_url, config.headroom_port)
                else {
                    continue;
                };
                let (api_key, auth_style) = if let Some(key) =
                    env.get("ANTHROPIC_API_KEY").and_then(Value::as_str)
                {
                    (Some(key.to_owned()), AuthStyle::XApiKey)
                } else if let Some(key) = env.get("ANTHROPIC_AUTH_TOKEN").and_then(Value::as_str) {
                    (Some(key.to_owned()), AuthStyle::Bearer)
                } else {
                    (None, AuthStyle::PassThrough)
                };
                let route = Route::new(
                    Protocol::Anthropic,
                    row.id.clone(),
                    row.name,
                    base_url,
                    api_key,
                    auth_style,
                    "cc-switch",
                );
                push_unique(&mut routes, route);
            }
        }
    }

    if config.enable_codex && config.codex_config.exists() {
        let (parsed, _) = parse_codex_text(&fs::read_to_string(&config.codex_config)?, "codex")?;
        for route in parsed {
            push_fallback(&mut routes, route);
        }
    }

    if config.enable_claude
        && config.claude_settings.exists()
        && let Ok(settings) =
            serde_json::from_str::<Value>(&fs::read_to_string(&config.claude_settings)?)
        && let Some(env) = settings.get("env").and_then(Value::as_object)
        && let Some(base) = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str)
        && let Ok(base_url) = normalize_url(base)
        && !is_local_headroom(&base_url, config.headroom_port)
    {
        let (key, style) = claude_auth(env);
        let id = "claude-settings".to_owned();
        if let Some(existing) = routes
            .iter()
            .find(|route| route.protocol == Protocol::Anthropic && route.base_url == base_url)
        {
            if selected_anthropic.is_none() {
                selected_anthropic = Some(existing.provider.clone());
            }
        } else {
            push_fallback(
                &mut routes,
                Route::new(
                    Protocol::Anthropic,
                    id.clone(),
                    "Claude settings".into(),
                    base_url,
                    key,
                    style,
                    "claude",
                ),
            );
            if selected_anthropic.is_none() {
                selected_anthropic = Some(id);
            }
        }
    }

    if config.enable_claude && !routes.iter().any(|r| r.protocol == Protocol::Anthropic) {
        let base = config
            .claude_upstream_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into());
        routes.push(Route::new(
            Protocol::Anthropic,
            "anthropic-default".into(),
            "Anthropic（沿用 Claude 登录）".into(),
            normalize_url(&base)?,
            None,
            AuthStyle::PassThrough,
            "default",
        ));
        selected_anthropic = Some("anthropic-default".into());
    }

    routes.sort_by(|a, b| {
        (a.protocol.label(), a.name.to_lowercase())
            .cmp(&(b.protocol.label(), b.name.to_lowercase()))
    });
    if routes.is_empty() {
        return Err(anyhow!(
            "没有发现可用上游，请先配置 Codex、Claude Code 或 CC-Switch Provider"
        ));
    }
    Ok(DiscoveredRoutes {
        routes,
        selected_openai,
        selected_anthropic,
    })
}

fn claude_auth(env: &Map<String, Value>) -> (Option<String>, AuthStyle) {
    if let Some(key) = env.get("ANTHROPIC_API_KEY").and_then(Value::as_str) {
        (Some(key.to_owned()), AuthStyle::XApiKey)
    } else if let Some(key) = env.get("ANTHROPIC_AUTH_TOKEN").and_then(Value::as_str) {
        (Some(key.to_owned()), AuthStyle::Bearer)
    } else {
        (None, AuthStyle::PassThrough)
    }
}

fn push_unique(routes: &mut Vec<Route>, route: Route) {
    if routes.iter().any(|old| {
        old.protocol == route.protocol
            && old.provider == route.provider
            && old.base_url == route.base_url
    }) {
        return;
    }
    routes.push(route);
}

fn push_fallback(routes: &mut Vec<Route>, route: Route) {
    if routes
        .iter()
        .any(|old| old.protocol == route.protocol && old.base_url == route.base_url)
    {
        return;
    }
    routes.push(route);
}

fn effective_claude_base_url(base: &str, website_url: &str, headroom_port: u16) -> Option<String> {
    let base_url = normalize_url(base).ok()?;
    if !is_local_headroom(&base_url, headroom_port) {
        return Some(base_url);
    }
    let recovered = normalize_url(website_url).ok()?;
    (!is_local_headroom(&recovered, headroom_port)).then_some(recovered)
}

pub fn normalize_url(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let url = url::Url::parse(trimmed)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(anyhow!("无效 HTTP(S) URL"));
    }
    if url.scheme() == "http" && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        return Err(anyhow!("非本机上游必须使用 HTTPS"));
    }
    Ok(trimmed.to_owned())
}

fn is_local_headroom(value: &str, port: u16) -> bool {
    url::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
            && url.port_or_known_default() == Some(port)
    })
}

pub fn sync_codex(config: &AppConfig, preferred: Option<&str>) -> Result<String> {
    if !config.enable_codex || !config.codex_config.exists() {
        return Ok("未启用".into());
    }
    let path = &config.codex_config;
    let original =
        fs::read_to_string(path).with_context(|| format!("读取失败: {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .context("Codex TOML 无法解析")?;
    let selected = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let providers = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("缺少 model_providers"))?;
    let preferred_provider = preferred.and_then(|url| {
        providers
            .iter()
            .find(|(name, item)| {
                *name != "headroom" && table_string(item, "base_url").as_deref() == Some(url)
            })
            .map(|(name, _)| name.to_owned())
    });
    let upstream = if let Some(provider) = preferred_provider {
        provider
    } else if selected != "headroom" && providers.contains_key(&selected) {
        selected
    } else {
        providers
            .iter()
            .find(|(name, item)| *name != "headroom" && item.as_table().is_some())
            .map(|(name, _)| name.to_owned())
            .unwrap_or_default()
    };
    if upstream.is_empty() {
        return Err(anyhow!("无法确定 Codex 上游 Provider"));
    }
    let source = providers
        .get(&upstream)
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("找不到上游 Provider: {upstream}"))?
        .clone();
    let mut headroom = Table::new();
    headroom.insert("name", value("Headroom Route"));
    headroom.insert(
        "base_url",
        value(format!("http://127.0.0.1:{}/v1", config.headroom_port)),
    );
    if let Some(item) = source.get("requires_openai_auth") {
        headroom.insert("requires_openai_auth", item.clone());
    }
    if let Some(item) = source.get("wire_api") {
        headroom.insert("wire_api", item.clone());
    }
    let mut env_headers = toml_edit::InlineTable::new();
    env_headers.insert(
        "X-Headroom-Project",
        toml_edit::Value::from("HEADROOM_PROJECT"),
    );
    headroom.insert(
        "env_http_headers",
        Item::Value(toml_edit::Value::InlineTable(env_headers)),
    );
    providers.insert("headroom", Item::Table(headroom));
    doc["model_provider"] = value("headroom");
    let updated = doc.to_string();
    if updated != original {
        backup(path, &original)?;
        atomic_write(path, updated.as_bytes())?;
    }
    Ok(upstream)
}

pub fn sync_claude(config: &AppConfig) -> Result<String> {
    if !config.enable_claude {
        return Ok("未启用".into());
    }
    let path = &config.claude_settings;
    let original = if path.exists() {
        fs::read_to_string(path)?
    } else {
        "{}".into()
    };
    let mut root: Value =
        serde_json::from_str(&original).context("Claude settings.json 无法解析")?;
    if !root.is_object() {
        return Err(anyhow!("Claude settings.json 根节点必须是对象"));
    }
    let root_obj = root.as_object_mut().unwrap();
    if !root_obj.get("env").is_some_and(Value::is_object) {
        root_obj.insert("env".into(), Value::Object(Map::new()));
    }
    let env = root_obj
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .unwrap();
    let local = format!("http://127.0.0.1:{}", config.headroom_port);
    let already = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) == Some(local.as_str());
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(local));
    if !already {
        if path.exists() {
            backup(path, &original)?;
        }
        atomic_write(path, &serde_json::to_vec_pretty(&root)?)?;
    }
    Ok("Claude Code".into())
}

pub fn sync_all(config: &AppConfig, preferred_openai: Option<&str>) -> Result<String> {
    capture_baseline(config)?;
    let codex = sync_codex(config, preferred_openai)?;
    let claude = sync_claude(config)?;
    Ok(format!("Codex={codex}, Claude={claude}"))
}

pub fn sync_provider_models(
    config: &AppConfig,
    protocol: Protocol,
    provider: &str,
) -> Result<Option<String>> {
    if !config.cc_switch_db.exists() {
        return Ok(None);
    }
    let app_type = if protocol == Protocol::OpenAi {
        "codex"
    } else {
        "claude"
    };
    let Some(row) = sqlite::providers(&config.cc_switch_db, app_type)?
        .into_iter()
        .find(|row| row.id == provider)
    else {
        return Ok(None);
    };
    let settings: Value = serde_json::from_str(&row.settings)
        .with_context(|| format!("CC-Switch Provider {} 配置无法解析", row.name))?;
    if protocol == Protocol::OpenAi {
        sync_codex_model(config, &settings)
    } else {
        sync_claude_models(config, &settings)
    }
}

fn sync_codex_model(config: &AppConfig, settings: &Value) -> Result<Option<String>> {
    let provider_text = settings
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("CC-Switch Codex Provider 缺少 config"))?;
    let provider_doc = provider_text
        .parse::<DocumentMut>()
        .context("CC-Switch Codex TOML 无法解析")?;
    let target = provider_doc
        .get("model")
        .and_then(Item::as_str)
        .map(str::to_owned);

    let original = fs::read_to_string(&config.codex_config)
        .with_context(|| format!("无法读取 {}", config.codex_config.display()))?;
    let mut current = original
        .parse::<DocumentMut>()
        .context("Codex TOML 无法解析")?;
    let previous = current
        .get("model")
        .and_then(Item::as_str)
        .map(str::to_owned);
    if previous == target {
        return Ok(None);
    }
    match target.as_deref() {
        Some(model) => current["model"] = value(model),
        None => {
            current.remove("model");
        }
    }
    atomic_write(&config.codex_config, current.to_string().as_bytes())?;
    Ok(Some(format!(
        "Codex 模型已从 {} 切换为 {}，请重启 Codex 生效",
        previous.as_deref().unwrap_or("默认模型"),
        target.as_deref().unwrap_or("默认模型")
    )))
}

fn sync_claude_models(config: &AppConfig, settings: &Value) -> Result<Option<String>> {
    let target = settings
        .get("env")
        .and_then(Value::as_object)
        .map(claude_model_values)
        .unwrap_or_default();
    let original = if config.claude_settings.exists() {
        fs::read_to_string(&config.claude_settings)?
    } else {
        "{}".into()
    };
    let mut root: Value =
        serde_json::from_str(&original).context("Claude settings.json 无法解析")?;
    if !root.is_object() {
        return Err(anyhow!("Claude settings.json 根节点必须是对象"));
    }
    let root_obj = root.as_object_mut().unwrap();
    if !root_obj.get("env").is_some_and(Value::is_object) {
        root_obj.insert("env".into(), Value::Object(Map::new()));
    }
    let env = root_obj
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .unwrap();
    let previous = claude_model_values(env);
    if previous == target {
        return Ok(None);
    }
    env.retain(|key, _| !is_claude_model_key(key));
    env.extend(target);
    atomic_write(&config.claude_settings, &serde_json::to_vec_pretty(&root)?)?;
    Ok(Some(
        "Claude Code 模型及角色模型已更新，请重启 Claude Code 生效".into(),
    ))
}

fn claude_model_values(env: &Map<String, Value>) -> BTreeMap<String, Value> {
    env.iter()
        .filter(|(key, value)| is_claude_model_key(key) && value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn is_claude_model_key(key: &str) -> bool {
    key == "ANTHROPIC_MODEL"
        || key == "CLAUDE_CODE_SUBAGENT_MODEL"
        || (key.starts_with("ANTHROPIC_DEFAULT_")
            && (key.ends_with("_MODEL") || key.ends_with("_MODEL_NAME")))
}

pub fn capture_baseline(config: &AppConfig) -> Result<()> {
    let manifest_path = config.state_dir.join("install-manifest.json");
    if manifest_path.exists() {
        return Ok(());
    }
    let baseline_dir = config.state_dir.join("baseline");
    fs::create_dir_all(&baseline_dir)?;
    let codex_baseline = capture_original(&config.codex_config, &baseline_dir.join("codex.toml"))?;
    let claude_baseline =
        capture_original(&config.claude_settings, &baseline_dir.join("claude.json"))?;
    let manifest = InstallManifest {
        version: env!("CARGO_PKG_VERSION").into(),
        codex_baseline,
        claude_baseline,
    };
    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
}

pub fn restore_clients(config: &AppConfig) -> Result<()> {
    let manifest_path = config.state_dir.join("install-manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    let manifest: InstallManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    restore_codex(config, manifest.codex_baseline.as_deref())?;
    restore_claude(config, manifest.claude_baseline.as_deref())?;
    Ok(())
}

fn capture_original(source: &Path, target: &Path) -> Result<Option<String>> {
    let candidate = earliest_pre_route_backup(source).unwrap_or_else(|| source.to_path_buf());
    if !candidate.exists() {
        return Ok(None);
    }
    fs::copy(candidate, target)?;
    Ok(Some(target.to_string_lossy().into_owned()))
}

fn earliest_pre_route_backup(path: &Path) -> Option<std::path::PathBuf> {
    let parent = path.parent()?;
    let filename = path.file_name()?.to_string_lossy();
    let prefix = format!("{filename}.pre-headroom-route-");
    let mut matches: Vec<_> = fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

fn restore_codex(config: &AppConfig, baseline_path: Option<&str>) -> Result<()> {
    if !config.codex_config.exists() {
        if let Some(path) = baseline_path {
            fs::copy(path, &config.codex_config)?;
        }
        return Ok(());
    }
    let original = fs::read_to_string(&config.codex_config)?;
    let mut current = original
        .parse::<DocumentMut>()
        .context("恢复时 Codex TOML 无法解析")?;
    let baseline = baseline_path
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.parse::<DocumentMut>().ok());
    if current.get("model_provider").and_then(Item::as_str) == Some("headroom") {
        if let Some(item) = baseline
            .as_ref()
            .and_then(|doc| doc.get("model_provider"))
            .cloned()
        {
            current["model_provider"] = item;
        } else {
            current.remove("model_provider");
        }
    }
    if current
        .get("openai_base_url")
        .and_then(Item::as_str)
        .is_some_and(|url| is_local_headroom(url, config.headroom_port))
    {
        if let Some(item) = baseline
            .as_ref()
            .and_then(|doc| doc.get("openai_base_url"))
            .cloned()
        {
            current["openai_base_url"] = item;
        } else {
            current.remove("openai_base_url");
        }
    }
    if let Some(providers) = current
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
    {
        providers.remove("headroom");
    }
    if let Some(baseline) = baseline.as_ref() {
        if let Some(item) = baseline.get("model").cloned() {
            current["model"] = item;
        } else {
            current.remove("model");
        }
    }
    let restored = current.to_string();
    if restored != original {
        backup(&config.codex_config, &original)?;
        atomic_write(&config.codex_config, restored.as_bytes())?;
    }
    Ok(())
}

fn restore_claude(config: &AppConfig, baseline_path: Option<&str>) -> Result<()> {
    if !config.claude_settings.exists() {
        if let Some(path) = baseline_path {
            fs::copy(path, &config.claude_settings)?;
        }
        return Ok(());
    }
    let original = fs::read_to_string(&config.claude_settings)?;
    let mut current: Value =
        serde_json::from_str(&original).context("恢复时 Claude settings.json 无法解析")?;
    let baseline = baseline_path
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let local = current
        .pointer("/env/ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .is_some_and(|url| is_local_headroom(url, config.headroom_port));
    if local {
        let env = current
            .as_object_mut()
            .and_then(|root| root.get_mut("env"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("Claude env 配置无效"))?;
        if let Some(value) = baseline
            .as_ref()
            .and_then(|root| root.pointer("/env/ANTHROPIC_BASE_URL"))
            .cloned()
        {
            env.insert("ANTHROPIC_BASE_URL".into(), value);
        } else {
            env.remove("ANTHROPIC_BASE_URL");
        }
    }
    if let Some(baseline) = baseline.as_ref()
        && let Some(env) = current
            .as_object_mut()
            .and_then(|root| root.get_mut("env"))
            .and_then(Value::as_object_mut)
    {
        env.retain(|key, _| !is_claude_model_key(key));
        if let Some(baseline_env) = baseline.get("env").and_then(Value::as_object) {
            env.extend(claude_model_values(baseline_env));
        }
    }
    let restored = serde_json::to_vec_pretty(&current)?;
    if restored != original.as_bytes() {
        backup(&config.claude_settings, &original)?;
        atomic_write(&config.claude_settings, &restored)?;
    }
    Ok(())
}

pub fn routing_drifted(config: &AppConfig) -> bool {
    let codex_drifted = config.enable_codex
        && config.codex_config.exists()
        && fs::read_to_string(&config.codex_config)
            .ok()
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .and_then(|doc| {
                doc.get("model_provider")
                    .and_then(Item::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            != Some("headroom");
    let local = format!("http://127.0.0.1:{}", config.headroom_port);
    let claude_drifted = config.enable_claude
        && fs::read_to_string(&config.claude_settings)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|root| {
                root.pointer("/env/ANTHROPIC_BASE_URL")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            != Some(local.as_str());
    codex_drifted || claude_drifted
}

pub fn outbound_proxy_url(config: &AppConfig) -> Option<String> {
    if !config.use_system_proxy {
        return None;
    }
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(value) = std::env::var(name)
            && let Some(url) = parse_proxy_server(&value)
        {
            return Some(url);
        }
    }
    windows_proxy_server().and_then(|value| parse_proxy_server(&value))
}

pub fn reqwest_outbound_proxy(config: &AppConfig) -> Result<Option<reqwest::Proxy>> {
    let Some(proxy_url) = outbound_proxy_url(config) else {
        return Ok(None);
    };
    let target = url::Url::parse(&proxy_url)?;
    Ok(Some(reqwest::Proxy::custom(move |request| {
        if matches!(request.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
            None
        } else {
            Some(target.clone())
        }
    })))
}

fn parse_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let endpoint = if value.contains('=') {
        let mut https = None;
        let mut http = None;
        for part in value.split(';') {
            let Some((kind, address)) = part.split_once('=') else {
                continue;
            };
            match kind.trim().to_ascii_lowercase().as_str() {
                "https" => https = Some(address.trim()),
                "http" => http = Some(address.trim()),
                _ => {}
            }
        }
        https.or(http)?
    } else {
        value
    };
    let candidate = if endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("http://{endpoint}")
    };
    let url = url::Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(candidate.trim_end_matches('/').to_owned())
}

#[cfg(windows)]
fn windows_proxy_server() -> Option<String> {
    let mut key = std::ptr::null_mut();
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    } != 0
    {
        return None;
    }
    let enabled = query_registry_dword(key, "ProxyEnable") == Some(1);
    let server = enabled
        .then(|| query_registry_string(key, "ProxyServer"))
        .flatten();
    unsafe {
        RegCloseKey(key);
    }
    server
}

#[cfg(not(windows))]
fn windows_proxy_server() -> Option<String> {
    None
}

#[cfg(windows)]
fn query_registry_dword(
    key: windows_sys::Win32::System::Registry::HKEY,
    name: &str,
) -> Option<u32> {
    let name = wide(name);
    let mut value = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    let mut kind = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            &mut value as *mut u32 as *mut u8,
            &mut bytes,
        )
    };
    (status == 0 && kind == REG_DWORD).then_some(value)
}

#[cfg(windows)]
fn query_registry_string(
    key: windows_sys::Win32::System::Registry::HKEY,
    name: &str,
) -> Option<String> {
    let name = wide(name);
    let mut bytes = 0u32;
    let mut kind = 0u32;
    if unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        )
    } != 0
        || kind != REG_SZ
        || bytes < 2
    {
        return None;
    }
    let mut value = vec![0u16; bytes as usize / 2];
    if unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            value.as_mut_ptr() as *mut u8,
            &mut bytes,
        )
    } != 0
    {
        return None;
    }
    Some(
        String::from_utf16_lossy(&value)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn backup(path: &Path, original: &str) -> Result<()> {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S%3f");
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("config");
    fs::write(
        path.with_file_name(format!("{name}.pre-headroom-route-{stamp}")),
        original,
    )?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.headroom-route.tmp",
        path.extension().and_then(|v| v.to_str()).unwrap_or("tmp")
    ));
    fs::write(&temp, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod model_sync_tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    #[test]
    fn syncs_codex_model_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-codex-model-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.codex_config = dir.join("config.toml");
        fs::write(
            &config.codex_config,
            "model = \"old-model\"\nmodel_provider = \"headroom\"\n",
        )
        .unwrap();
        let settings = serde_json::json!({"config": "model = \"new-model\"\n"});

        assert!(sync_codex_model(&config, &settings).unwrap().is_some());
        let doc = fs::read_to_string(&config.codex_config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc.get("model").and_then(Item::as_str), Some("new-model"));
        assert!(sync_codex_model(&config, &settings).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn syncs_all_claude_model_roles_and_preserves_unrelated_env() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-claude-model-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.claude_settings = dir.join("settings.json");
        fs::write(&config.claude_settings, r#"{"env":{"KEEP":"yes","ANTHROPIC_MODEL":"old","ANTHROPIC_DEFAULT_HAIKU_MODEL":"old-haiku"}}"#).unwrap();
        let settings = serde_json::json!({"env": {
            "ANTHROPIC_MODEL": "new",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "new-opus",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME": "fable-name",
            "CLAUDE_CODE_SUBAGENT_MODEL": "subagent"
        }});

        assert!(sync_claude_models(&config, &settings).unwrap().is_some());
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(&config.claude_settings).unwrap()).unwrap();
        assert_eq!(saved["env"]["KEEP"], "yes");
        assert_eq!(saved["env"]["ANTHROPIC_MODEL"], "new");
        assert_eq!(saved["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "new-opus");
        assert_eq!(
            saved["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL_NAME"],
            "fable-name"
        );
        assert_eq!(saved["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "subagent");
        assert!(saved["env"].get("ANTHROPIC_DEFAULT_HAIKU_MODEL").is_none());
        assert!(sync_claude_models(&config, &settings).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        effective_claude_base_url, parse_proxy_server, push_fallback, push_unique, routing_drifted,
        sync_claude,
    };
    use crate::model::{AppConfig, AuthStyle, Protocol, Route};
    use serde_json::Value;
    use std::fs;

    #[test]
    fn sync_claude_preserves_unknown_fields_and_secrets() {
        let dir = std::env::temp_dir().join(format!("headroom-route-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"theme":"dark","env":{"ANTHROPIC_AUTH_TOKEN":"secret","CUSTOM":"yes","ANTHROPIC_BASE_URL":"https://example.com"}}"#).unwrap();
        let mut config = AppConfig::default();
        config.claude_settings = path.clone();
        config.headroom_port = 8787;
        config.enable_codex = false;
        sync_claude(&config).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        assert_eq!(value["env"]["CUSTOM"], "yes");
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert!(!routing_drifted(&config));
        assert!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("pre-headroom-route"))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_windows_proxy_server_formats() {
        assert_eq!(
            parse_proxy_server("127.0.0.1:7897").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(
            parse_proxy_server("http=127.0.0.1:8080;https=127.0.0.1:7897").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        assert_eq!(parse_proxy_server("socks=127.0.0.1:1080"), None);
    }

    #[test]
    fn recovers_cc_switch_provider_url_from_website_metadata() {
        assert_eq!(
            effective_claude_base_url("http://127.0.0.1:8787", "https://api.example.com", 8787)
                .as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(
            effective_claude_base_url(
                "https://direct.example.com",
                "https://ignored.example.com",
                8787
            )
            .as_deref(),
            Some("https://direct.example.com")
        );
        assert_eq!(
            effective_claude_base_url("http://127.0.0.1:8787", "", 8787),
            None
        );
    }

    #[test]
    fn keeps_distinct_cc_switch_providers_that_share_an_endpoint() {
        let mut routes = Vec::new();
        push_unique(
            &mut routes,
            Route::new(
                Protocol::Anthropic,
                "one".into(),
                "One".into(),
                "https://same.example.com".into(),
                Some("a".into()),
                AuthStyle::Bearer,
                "cc-switch",
            ),
        );
        push_unique(
            &mut routes,
            Route::new(
                Protocol::Anthropic,
                "two".into(),
                "Two".into(),
                "https://same.example.com".into(),
                Some("b".into()),
                AuthStyle::Bearer,
                "cc-switch",
            ),
        );
        push_fallback(
            &mut routes,
            Route::new(
                Protocol::Anthropic,
                "settings".into(),
                "Settings".into(),
                "https://same.example.com".into(),
                None,
                AuthStyle::PassThrough,
                "claude",
            ),
        );
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn restore_removes_only_managed_routing_changes() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-restore-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("codex.toml");
        let claude = dir.join("claude.json");
        fs::write(&codex, "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\n").unwrap();
        fs::write(&claude, r#"{"theme":"dark","env":{"ANTHROPIC_BASE_URL":"https://claude.example.com","ANTHROPIC_AUTH_TOKEN":"secret"}}"#).unwrap();
        let mut config = AppConfig::default();
        config.state_dir = dir.join("state");
        config.codex_config = codex.clone();
        config.claude_settings = claude.clone();
        config.cc_switch_db = dir.join("missing.db");
        super::sync_all(&config, Some("https://api.example.com/v1")).unwrap();
        let mut current: Value =
            serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        current["later_user_setting"] = Value::Bool(true);
        fs::write(&claude, serde_json::to_vec_pretty(&current).unwrap()).unwrap();
        super::restore_clients(&config).unwrap();
        let codex_text = fs::read_to_string(&codex).unwrap();
        assert!(codex_text.contains("model_provider = \"upstream\""));
        assert!(!codex_text.contains("model_providers.headroom"));
        let restored: Value = serde_json::from_str(&fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(
            restored["env"]["ANTHROPIC_BASE_URL"],
            "https://claude.example.com"
        );
        assert_eq!(restored["later_user_setting"], true);
        assert_eq!(restored["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        let _ = fs::remove_dir_all(dir);
    }
}
