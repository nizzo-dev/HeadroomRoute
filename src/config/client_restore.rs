use super::*;

pub fn capture_baseline(config: &AppConfig) -> Result<()> {
    let manifest_path = config.state_dir.join("install-manifest.json");
    if manifest_path.exists() {
        let Ok(mut manifest) =
            serde_json::from_str::<InstallManifest>(&fs::read_to_string(&manifest_path)?)
        else {
            return Ok(());
        };
        if manifest.codex_auth_baseline.is_none() {
            let baseline_dir = config.state_dir.join("baseline");
            fs::create_dir_all(&baseline_dir)?;
            manifest.codex_auth_baseline = capture_original(
                &codex_auth_path(config).unwrap_or_else(|| baseline_dir.join("auth.json")),
                &baseline_dir.join("codex-auth.json"),
            )?;
            atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
        }
        return Ok(());
    }
    let baseline_dir = config.state_dir.join("baseline");
    fs::create_dir_all(&baseline_dir)?;
    let codex_baseline = capture_original(&config.codex_config, &baseline_dir.join("codex.toml"))?;
    let claude_baseline =
        capture_original(&config.claude_settings, &baseline_dir.join("claude.json"))?;
    let codex_auth_baseline = capture_original(
        &codex_auth_path(config).unwrap_or_else(|| baseline_dir.join("auth.json")),
        &baseline_dir.join("codex-auth.json"),
    )?;
    let manifest = InstallManifest {
        version: env!("CARGO_PKG_VERSION").into(),
        codex_baseline,
        claude_baseline,
        codex_auth_baseline,
        codex_auth_managed: false,
        claude_provider_env_managed: false,
    };
    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
}

pub(super) fn mark_direct_managed(config: &AppConfig, protocol: Protocol) -> Result<()> {
    let manifest_path = config.state_dir.join("install-manifest.json");
    if !manifest_path.exists() {
        capture_baseline(config)?;
    }
    let mut manifest: InstallManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    match protocol {
        Protocol::OpenAi => manifest.codex_auth_managed = true,
        Protocol::Anthropic => manifest.claude_provider_env_managed = true,
    }
    atomic_write(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)
}

pub fn restore_clients(config: &AppConfig) -> Result<()> {
    let manifest_path = config.state_dir.join("install-manifest.json");
    if !manifest_path.exists() {
        return Ok(());
    }
    let manifest: InstallManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    restore_codex(
        config,
        manifest.codex_baseline.as_deref(),
        manifest.codex_auth_baseline.as_deref(),
        manifest.codex_auth_managed,
    )?;
    restore_claude(
        config,
        manifest.claude_baseline.as_deref(),
        manifest.claude_provider_env_managed,
    )?;
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

fn restore_codex(
    config: &AppConfig,
    baseline_path: Option<&str>,
    auth_baseline_path: Option<&str>,
    auth_managed: bool,
) -> Result<()> {
    if !config.codex_config.exists() {
        if let Some(path) = baseline_path {
            fs::copy(path, &config.codex_config)?;
        }
    } else {
        let original = fs::read_to_string(&config.codex_config)?;
        let mut current = original
            .parse::<DocumentMut>()
            .context("恢复时 Codex TOML 无法解析")?;
        let baseline = baseline_path
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| text.parse::<DocumentMut>().ok());
        if auth_managed
            || matches!(
                current.get("model_provider").and_then(Item::as_str),
                Some("headroom" | DIRECT_CODEX_PROVIDER)
            )
        {
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
            .is_some_and(|url| is_local_service(url, config))
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
            providers.remove(DIRECT_CODEX_PROVIDER);
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
    }
    if auth_managed
        && let (Some(source), Some(target)) = (auth_baseline_path, codex_auth_path(config))
        && Path::new(source).exists()
    {
        let original = fs::read(&target).ok();
        let baseline = fs::read(source)?;
        if original.as_deref() != Some(baseline.as_slice()) {
            if let Some(bytes) = original.as_ref() {
                backup(&target, &String::from_utf8_lossy(bytes))?;
            }
            atomic_write(&target, &baseline)?;
        }
    }
    Ok(())
}

fn restore_claude(
    config: &AppConfig,
    baseline_path: Option<&str>,
    provider_env_managed: bool,
) -> Result<()> {
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
    let current_url = current
        .pointer("/env/ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let local = current_url
        .as_deref()
        .is_some_and(|url| is_local_service(url, config));
    let direct =
        current_url.as_deref() == selected_route_url(config, Protocol::Anthropic).as_deref();
    if local || direct || provider_env_managed {
        let env = current
            .as_object_mut()
            .and_then(|root| root.get_mut("env"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("Claude env 配置无效"))?;
        if provider_env_managed {
            env.retain(|key, _| !is_claude_provider_key(key));
            if let Some(baseline_env) = baseline
                .as_ref()
                .and_then(|root| root.get("env"))
                .and_then(Value::as_object)
            {
                for (key, value) in baseline_env {
                    if is_claude_provider_key(key) {
                        env.insert(key.clone(), value.clone());
                    }
                }
            }
        } else if let Some(value) = baseline
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

fn selected_route_url(config: &AppConfig, protocol: Protocol) -> Option<String> {
    let selected = match protocol {
        Protocol::OpenAi => config.selected_openai_provider.as_deref(),
        Protocol::Anthropic => config.selected_anthropic_provider.as_deref(),
    }?;
    discover_routes(config)
        .ok()?
        .routes
        .into_iter()
        .find(|route| route.protocol == protocol && route.provider == selected)
        .map(|route| route.base_url)
}

pub fn routing_drifted_with_targets(
    config: &AppConfig,
    _preferred_openai: Option<&str>,
    _preferred_anthropic: Option<&str>,
) -> bool {
    let codex_local = format!("http://127.0.0.1:{}/v1", client_port(config));
    let codex_drifted = config.manage_upstream
        && config.enable_codex
        && config.codex_config.exists()
        && fs::read_to_string(&config.codex_config)
            .ok()
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .is_none_or(|doc| {
                doc.get("model_provider").and_then(Item::as_str) != Some("headroom")
                    || doc
                        .get("model_providers")
                        .and_then(Item::as_table)
                        .and_then(|providers| providers.get("headroom"))
                        .and_then(Item::as_table)
                        .and_then(|provider| provider.get("base_url"))
                        .and_then(Item::as_str)
                        != Some(codex_local.as_str())
            });
    let local = format!("http://127.0.0.1:{}", client_port(config));
    let claude_drifted = config.manage_upstream
        && config.enable_claude
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
