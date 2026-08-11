use super::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct PortableSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headroom_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_codex: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_claude: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_failover: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failover_policy: Option<FailoverPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manage_headroom: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_with_windows: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_subscription_tracking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_system_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bypass_headroom: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_codex: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_claude: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_check_updates: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_api_key_on_hover: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_strategy: Option<RoutingStrategyConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PortableEnvelope {
    format: String,
    format_version: u32,
    #[serde(default = "portable_reader_version")]
    minimum_reader_version: u32,
    exported_at: DateTime<Utc>,
    #[serde(default)]
    settings: PortableSettings,
}

fn portable_reader_version() -> u32 {
    PORTABLE_CONFIG_VERSION
}

pub fn export_portable_config(config: &AppConfig, destination: &Path) -> Result<()> {
    config.routing_strategy.validate()?;
    let envelope = PortableEnvelope {
        format: PORTABLE_FORMAT.into(),
        format_version: PORTABLE_CONFIG_VERSION,
        minimum_reader_version: PORTABLE_CONFIG_VERSION,
        exported_at: Utc::now(),
        settings: PortableSettings::from_config(config),
    };
    let bytes = serde_json::to_vec_pretty(&envelope)?;
    let text = String::from_utf8_lossy(&bytes);
    if contains_obvious_secret(&text) {
        bail!("便携配置意外包含敏感内容，导出已中止");
    }
    atomic_write(destination, &bytes)
}

/// Parse and validate an import without touching disk. Unknown fields and a
/// newer additive format are accepted when its minimum reader remains v1.
pub fn decode_portable_config(bytes: &[u8], current: &AppConfig) -> Result<AppConfig> {
    if bytes.len() > MAX_CONFIG_BYTES {
        bail!("导入配置超过 8 MiB 限制");
    }
    let envelope: PortableEnvelope = serde_json::from_slice(bytes).context("便携配置无法解析")?;
    if envelope.format != PORTABLE_FORMAT {
        bail!("不是 HeadroomRoute 便携配置");
    }
    if envelope.format_version == 0 || envelope.minimum_reader_version > PORTABLE_CONFIG_VERSION {
        bail!("便携配置需要读取器版本 {}", envelope.minimum_reader_version);
    }
    let mut updated = current.clone();
    envelope.settings.apply(&mut updated);
    validate_portable_result(&updated)?;
    Ok(updated)
}

pub fn import_portable_config(
    source: &Path,
    destination: &Path,
    current: &AppConfig,
) -> Result<AppConfig> {
    let bytes = read_limited(source)?;
    let updated = decode_portable_config(&bytes, current)?;
    let encoded = serde_json::to_vec_pretty(&updated)?;
    let original = if destination.exists() {
        Some(read_limited(destination)?)
    } else {
        None
    };
    commit_files(vec![PendingFile {
        path: destination.to_owned(),
        original,
        updated: Some(encoded),
    }])?;
    Ok(updated)
}

impl PortableSettings {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            agent_port: Some(config.agent_port),
            headroom_port: Some(config.headroom_port),
            enable_codex: Some(config.enable_codex),
            enable_claude: Some(config.enable_claude),
            auto_failover: Some(config.auto_failover),
            failover_policy: Some(config.failover_policy.clone()),
            manage_headroom: Some(config.manage_headroom),
            start_with_windows: Some(config.start_with_windows),
            no_subscription_tracking: Some(config.no_subscription_tracking),
            use_system_proxy: Some(config.use_system_proxy),
            bypass_headroom: Some(config.bypass_headroom),
            direct_codex: Some(config.direct_codex),
            direct_claude: Some(config.direct_claude),
            auto_check_updates: Some(config.auto_check_updates),
            show_api_key_on_hover: Some(config.show_api_key_on_hover),
            routing_strategy: Some(config.routing_strategy.clone()),
        }
    }

    fn apply(self, config: &mut AppConfig) {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    config.$field = value;
                }
            };
        }
        apply!(agent_port);
        apply!(headroom_port);
        apply!(enable_codex);
        apply!(enable_claude);
        apply!(auto_failover);
        apply!(failover_policy);
        apply!(manage_headroom);
        apply!(start_with_windows);
        apply!(no_subscription_tracking);
        apply!(use_system_proxy);
        apply!(bypass_headroom);
        apply!(direct_codex);
        apply!(direct_claude);
        apply!(auto_check_updates);
        apply!(show_api_key_on_hover);
        apply!(routing_strategy);
    }
}

fn validate_portable_result(config: &AppConfig) -> Result<()> {
    config.routing_strategy.validate()?;
    if config.agent_port == 0 || config.headroom_port == 0 {
        bail!("代理端口必须在 1 到 65535 之间");
    }
    for rules in [
        &config.failover_policy.openai,
        &config.failover_policy.anthropic,
    ] {
        if rules.iter().any(|(source, targets)| {
            source.trim().is_empty() || targets.iter().any(|v| v.trim().is_empty())
        }) {
            bail!("故障转移规则不能包含空 Provider ID");
        }
    }
    Ok(())
}

pub(super) fn contains_obvious_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    (lower.contains("sk-") || lower.contains("sk_")) && value.len() >= 12
}
