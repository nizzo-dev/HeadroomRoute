use super::{DIRECT_CODEX_PROVIDER, DiscoveredRoutes, table_string};
use crate::{
    model::{AppConfig, AuthStyle, Protocol, Route},
    sqlite,
};
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::fs;
use toml_edit::{DocumentMut, Item};

pub(super) fn parse_codex_text(text: &str, source: &str) -> Result<(Vec<Route>, Option<String>)> {
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
    let mut selected_openai = config.selected_openai_provider.clone();
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
                let Ok((mut parsed, parsed_selected)) = parse_codex_text(toml, "cc-switch") else {
                    continue;
                };
                parsed.retain(|route| !is_local_service(&route.base_url, config));
                if selected_openai.as_deref() == Some(DIRECT_CODEX_PROVIDER)
                    && parsed_selected.as_deref() == Some(DIRECT_CODEX_PROVIDER)
                    && !parsed.is_empty()
                {
                    selected_openai = Some(row.id.clone());
                }
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
                let Some(base_url) = effective_claude_base_url(base, &row.website_url, config)
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
        for mut route in parsed {
            if is_local_service(&route.base_url, config) {
                continue;
            }
            if route.provider == DIRECT_CODEX_PROVIDER
                && let Some(canonical) = routes.iter().find(|candidate| {
                    candidate.protocol == Protocol::OpenAi
                        && candidate.base_url == route.base_url
                        && candidate.provider != DIRECT_CODEX_PROVIDER
                })
            {
                route.provider = canonical.provider.clone();
                route.name = canonical.name.clone();
            }
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
        && !is_local_service(&base_url, config)
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

pub(super) fn push_unique(routes: &mut Vec<Route>, route: Route) {
    if routes.iter().any(|old| {
        old.protocol == route.protocol
            && old.provider == route.provider
            && old.base_url == route.base_url
    }) {
        return;
    }
    routes.push(route);
}

pub(super) fn push_fallback(routes: &mut Vec<Route>, route: Route) {
    if routes
        .iter()
        .any(|old| old.protocol == route.protocol && old.base_url == route.base_url)
    {
        return;
    }
    routes.push(route);
}

pub(super) fn effective_claude_base_url(
    base: &str,
    website_url: &str,
    config: &AppConfig,
) -> Option<String> {
    let base_url = normalize_url(base).ok()?;
    if !is_local_service(&base_url, config) {
        return Some(base_url);
    }
    let recovered = normalize_url(website_url).ok()?;
    (!is_local_service(&recovered, config)).then_some(recovered)
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

pub(super) fn is_local_headroom(value: &str, port: u16) -> bool {
    url::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
            && url.port_or_known_default() == Some(port)
    })
}

pub(super) fn is_local_service(value: &str, config: &AppConfig) -> bool {
    is_local_headroom(value, config.headroom_port) || is_local_headroom(value, config.agent_port)
}

pub(super) fn client_port(config: &AppConfig) -> u16 {
    if config.bypass_headroom {
        config.agent_port
    } else {
        config.headroom_port
    }
}
