use super::*;

fn sync_codex_direct_provider(
    config: &AppConfig,
    provider: Option<&str>,
    preferred: Option<&str>,
) -> Result<String> {
    capture_baseline(config)?;
    if !config.enable_codex || !config.codex_config.exists() {
        return Ok("未启用".into());
    }
    let path = &config.codex_config;
    let original =
        fs::read_to_string(path).with_context(|| format!("读取失败: {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .context("Codex TOML 无法解析")?;
    if let Some(provider) = provider
        && let Some(settings) = cc_switch_settings(config, Protocol::OpenAi, provider)?
    {
        let source_text = settings
            .get("config")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("CC-Switch Codex Provider 缺少 config"))?;
        let source = source_text
            .parse::<DocumentMut>()
            .context("CC-Switch Codex Provider TOML 无法解析")?;
        let (_, target) = apply_codex_provider_document(config, &mut doc, &source)?;
        let mut updates = vec![PendingFile {
            path: path.clone(),
            original: Some(original.as_bytes().to_vec()),
            updated: Some(doc.to_string().into_bytes()),
        }];
        if let Some(auth) = pending_codex_auth(config, &settings)? {
            updates.push(auth);
        }
        commit_files(updates)?;
        mark_direct_managed(config, Protocol::OpenAi)?;
        return Ok(target);
    }
    sync_codex_direct_legacy(config, preferred, path, &original, doc)
}

fn sync_codex_direct_legacy(
    config: &AppConfig,
    preferred: Option<&str>,
    path: &Path,
    original: &str,
    mut doc: DocumentMut,
) -> Result<String> {
    let selected = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .unwrap_or_default()
        .to_owned();
    let providers = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("缺少 model_providers"))?;

    let preferred_normalized = preferred.and_then(|url| normalize_url(url).ok());
    let source_name = preferred
        .and_then(|_| {
            providers
                .iter()
                .find(|(name, item)| {
                    name != &"headroom"
                        && name != &DIRECT_CODEX_PROVIDER
                        && table_string(item, "base_url")
                            .and_then(|value| normalize_url(&value).ok())
                            .as_ref()
                            == preferred_normalized.as_ref()
                })
                .map(|(name, _)| name.to_owned())
        })
        .or_else(|| {
            preferred
                .is_none()
                .then(|| {
                    (selected != "headroom"
                        && selected != DIRECT_CODEX_PROVIDER
                        && providers.contains_key(&selected))
                    .then_some(selected.clone())
                })
                .flatten()
        });
    let target = preferred
        .map(normalize_url)
        .transpose()?
        .or_else(|| {
            source_name.as_deref().and_then(|name| {
                providers
                    .get(name)
                    .and_then(Item::as_table)
                    .and_then(|item| item.get("base_url"))
                    .and_then(Item::as_str)
                    .and_then(|url| normalize_url(url).ok())
            })
        })
        .or_else(|| {
            providers.iter().find_map(|(name, item)| {
                if name == "headroom" || name == DIRECT_CODEX_PROVIDER {
                    return None;
                }
                table_string(item, "base_url").and_then(|url| normalize_url(&url).ok())
            })
        })
        .ok_or_else(|| anyhow!("无法确定 Codex 直连上游 Provider"))?;
    if is_local_service(&target, config) {
        return Err(anyhow!("Codex 直连上游不能是本地 HeadroomRoute 地址"));
    }

    let canonical_source = source_name.clone();
    if let Some(_source_name) = source_name {
        providers.remove(DIRECT_CODEX_PROVIDER);
    } else {
        let mut direct = Table::new();
        direct.insert("name", value("HeadroomRoute Direct"));
        direct.insert("base_url", value(target.clone()));
        providers.insert(DIRECT_CODEX_PROVIDER, Item::Table(direct));
    }
    let _ = providers;
    doc["model_provider"] = value(canonical_source.unwrap_or_else(|| DIRECT_CODEX_PROVIDER.into()));
    let updated = doc.to_string();
    if updated != original {
        backup(path, original)?;
        atomic_write(path, updated.as_bytes())?;
    }
    Ok(target)
}

pub fn sync_codex(config: &AppConfig, preferred: Option<&str>) -> Result<String> {
    if !config.enable_codex || !config.codex_config.exists() {
        return Ok("未启用".into());
    }
    if !config.manage_codex {
        return sync_codex_direct_provider(
            config,
            config.selected_openai_provider.as_deref(),
            preferred,
        );
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
                *name != "headroom"
                    && *name != DIRECT_CODEX_PROVIDER
                    && table_string(item, "base_url").as_deref() == Some(url)
            })
            .map(|(name, _)| name.to_owned())
    });
    let upstream = if let Some(provider) = preferred_provider {
        provider
    } else if selected != "headroom"
        && selected != DIRECT_CODEX_PROVIDER
        && providers.contains_key(&selected)
    {
        selected
    } else {
        providers
            .iter()
            .find(|(name, item)| {
                *name != "headroom" && *name != DIRECT_CODEX_PROVIDER && item.as_table().is_some()
            })
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
        value(format!("http://127.0.0.1:{}/v1", client_port(config))),
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

fn sync_claude_direct_provider(
    config: &AppConfig,
    provider: Option<&str>,
    preferred: Option<&str>,
) -> Result<String> {
    capture_baseline(config)?;
    if !config.enable_claude {
        return Ok("未启用".into());
    }
    let path = &config.claude_settings;
    let original = if path.exists() {
        fs::read(path).with_context(|| format!("读取失败: {}", path.display()))?
    } else {
        b"{}".to_vec()
    };
    let original_text =
        String::from_utf8(original.clone()).context("Claude settings.json 不是 UTF-8")?;
    let mut root: Value =
        serde_json::from_slice(&original).context("Claude settings.json 无法解析")?;
    if let Some(provider) = provider
        && let Some(settings) = cc_switch_settings(config, Protocol::Anthropic, provider)?
    {
        let target = apply_claude_provider_settings(config, &mut root, &settings, preferred)?;
        let updated = serde_json::to_vec_pretty(&root)?;
        commit_files(vec![PendingFile {
            path: path.clone(),
            original: Some(original),
            updated: Some(updated),
        }])?;
        mark_direct_managed(config, Protocol::Anthropic)?;
        return Ok(target);
    }

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("Claude settings.json 根节点必须是对象"))?;
    if !root_obj.get("env").is_some_and(Value::is_object) {
        root_obj.insert("env".into(), Value::Object(Map::new()));
    }
    let env = root_obj
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .unwrap();
    let target = preferred
        .map(normalize_url)
        .transpose()?
        .or_else(|| {
            env.get("ANTHROPIC_BASE_URL")
                .and_then(Value::as_str)
                .and_then(|value| normalize_url(value).ok())
        })
        .ok_or_else(|| anyhow!("无法确定 Claude 直连上游 Provider"))?;
    if is_local_service(&target, config) {
        return Err(anyhow!("Claude 直连上游不能是本地 HeadroomRoute 地址"));
    }
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(target.clone()));
    let updated = serde_json::to_vec_pretty(&root)?;
    commit_files(vec![PendingFile {
        path: path.clone(),
        original: Some(original_text.into_bytes()),
        updated: Some(updated),
    }])?;
    mark_direct_managed(config, Protocol::Anthropic)?;
    Ok(target)
}

pub fn sync_direct_provider(
    config: &AppConfig,
    protocol: Protocol,
    provider: &str,
) -> Result<String> {
    capture_baseline(config)?;
    let preferred = discover_routes(config)
        .ok()
        .and_then(|found| {
            found
                .routes
                .into_iter()
                .find(|route| route.protocol == protocol && route.provider == provider)
        })
        .map(|route| route.base_url);
    match protocol {
        Protocol::OpenAi => {
            sync_codex_direct_provider(config, Some(provider), preferred.as_deref())
        }
        Protocol::Anthropic => {
            sync_claude_direct_provider(config, Some(provider), preferred.as_deref())
        }
    }
}

/// Release enabled clients to the CC-Switch current upstream (observe mode).
/// Used when turning manage_upstream off, on clean exit after manage, and to
/// heal sticky local URLs after an unclean shutdown.
pub fn release_to_cc_switch(config: &AppConfig) -> Result<String> {
    if !config.cc_switch_db.exists() {
        return Ok("未发现 CC-Switch 数据库，保留当前客户端配置".into());
    }
    let mut updated = config.clone();
    let mut handed_off = Vec::new();
    if config.enable_codex
        && let Some(provider) = sqlite::current_provider(&config.cc_switch_db, "codex")?
    {
        sync_direct_provider(config, Protocol::OpenAi, &provider.id)?;
        updated.selected_openai_provider = Some(provider.id.clone());
        handed_off.push(format!("Codex={}", provider.name));
    }
    if config.enable_claude
        && let Some(provider) = sqlite::current_provider(&config.cc_switch_db, "claude")?
    {
        sync_direct_provider(config, Protocol::Anthropic, &provider.id)?;
        updated.selected_anthropic_provider = Some(provider.id.clone());
        handed_off.push(format!("Claude={}", provider.name));
    }
    if updated.selected_openai_provider != config.selected_openai_provider
        || updated.selected_anthropic_provider != config.selected_anthropic_provider
    {
        // Persist selection only; manage_upstream flag is owned by caller.
        let mut to_save = updated.clone();
        to_save.manage_codex = config.manage_codex;
        to_save.manage_claude = config.manage_claude;
        to_save.sync_deprecated_direct_flags();
        save(&config.state_dir.join("config.json"), &to_save)?;
    }
    if handed_off.is_empty() {
        Ok("CC-Switch 没有当前 Provider，保留当前客户端配置".into())
    } else {
        Ok(handed_off.join(", "))
    }
}

/// Backward-compatible alias used by older call sites.
pub fn handoff_direct_to_cc_switch(config: &AppConfig) -> Result<String> {
    release_to_cc_switch(config)
}

pub fn sync_claude_with_target(config: &AppConfig, preferred: Option<&str>) -> Result<String> {
    if !config.enable_claude {
        return Ok("未启用".into());
    }
    if !config.manage_claude {
        return sync_claude_direct_provider(
            config,
            config.selected_anthropic_provider.as_deref(),
            preferred,
        );
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
    let target = format!("http://127.0.0.1:{}", client_port(config));
    let already = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) == Some(target.as_str());
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(target));
    if !already {
        if path.exists() {
            backup(path, &original)?;
        }
        atomic_write(path, &serde_json::to_vec_pretty(&root)?)?;
    }
    Ok("Claude Code".into())
}

pub fn sync_all_with_targets(
    config: &AppConfig,
    preferred_openai: Option<&str>,
    preferred_anthropic: Option<&str>,
) -> Result<String> {
    capture_baseline(config)?;
    if !config.manage_codex && !config.manage_claude {
        return release_to_cc_switch(config);
    }
    Ok(format!(
        "Codex={}, Claude={}",
        sync_codex(config, preferred_openai)?,
        sync_claude_with_target(config, preferred_anthropic)?
    ))
}

pub fn sync_protocol_with_target(
    config: &AppConfig,
    protocol: Protocol,
    preferred: Option<&str>,
) -> Result<String> {
    capture_baseline(config)?;
    match protocol {
        Protocol::OpenAi => sync_codex(config, preferred),
        Protocol::Anthropic => sync_claude_with_target(config, preferred),
    }
}

#[cfg(test)]
mod independent_manage_tests {
    use super::*;
    use crate::model::AppConfig;
    use std::fs;

    #[test]
    fn migrate_splits_legacy_flag_and_keeps_single_protocol() {
        let mut both = AppConfig {
            manage_upstream: true,
            ..AppConfig::default()
        };
        both.migrate_manage_upstream();
        assert!(both.manage_codex && both.manage_claude && both.manage_upstream);

        let mut codex_only = AppConfig {
            manage_codex: true,
            ..AppConfig::default()
        };
        codex_only.migrate_manage_upstream();
        assert!(codex_only.manage_codex && !codex_only.manage_claude);
        assert!(codex_only.manage_upstream && !codex_only.direct_codex && codex_only.direct_claude);
    }

    #[test]
    fn sync_all_rewrites_only_managed_protocol() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-manage-split-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig {
            state_dir: dir.clone(),
            codex_config: dir.join("config.toml"),
            claude_settings: dir.join("settings.json"),
            cc_switch_db: dir.join("missing.db"),
            manage_codex: true,
            manage_claude: false,
            ..AppConfig::default()
        };
        config.sync_deprecated_direct_flags();
        fs::write(
            &config.codex_config,
            "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        fs::write(
            &config.claude_settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://claude.example.com"}}"#,
        )
        .unwrap();
        sync_all_with_targets(&config, None, None).unwrap();
        let codex = fs::read_to_string(&config.codex_config).unwrap();
        let claude = fs::read_to_string(&config.claude_settings).unwrap();
        assert!(codex.contains("model_provider = \"headroom\""));
        assert!(claude.contains("https://claude.example.com"));
        assert!(!claude.contains("127.0.0.1"));
        let _ = fs::remove_dir_all(dir);
    }
}
