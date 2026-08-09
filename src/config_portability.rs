use super::{PendingFile, atomic_write, commit_files};
use crate::{
    model::{AppConfig, FailoverPolicy},
    routing_policy::RoutingStrategyConfig,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use toml_edit::{DocumentMut, Item, Table, value};
use zip::{ZipWriter, write::SimpleFileOptions};

pub const TAKEOVER_PREVIEW_VERSION: u32 = 1;
pub const BACKUP_FORMAT_VERSION: u32 = 1;
pub const PORTABLE_CONFIG_VERSION: u32 = 1;
pub const LOCAL_STATUS_API_VERSION: u32 = 1;

const TAKEOVER_FORMAT: &str = "headroom-route-takeover-preview";
const BACKUP_FORMAT: &str = "headroom-route-config-backup";
const PORTABLE_FORMAT: &str = "headroom-route-portable-config";
const REDACTED: &str = "[REDACTED]";
const MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFileKind {
    MainConfig,
    CodexConfig,
    CodexAuth,
    ClaudeSettings,
}

impl ConfigFileKind {
    fn payload_name(self) -> &'static str {
        match self {
            Self::MainConfig => "main-config.json",
            Self::CodexConfig => "codex-config.toml",
            Self::CodexAuth => "codex-auth.json",
            Self::ClaudeSettings => "claude-settings.json",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::MainConfig => "HeadroomRoute config.json",
            Self::CodexConfig => "Codex config.toml",
            Self::CodexAuth => "Codex auth.json",
            Self::ClaudeSettings => "Claude settings.json",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FieldChangePreview {
    pub field: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileChangePreview {
    pub kind: ConfigFileKind,
    pub path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub fields: Vec<FieldChangePreview>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TakeoverPreview {
    pub format: &'static str,
    pub format_version: u32,
    pub generated_at: DateTime<Utc>,
    pub confirmation_token: String,
    pub changes: Vec<FileChangePreview>,
}

/// Prepared bytes stay private so callers can render or serialize `preview`
/// without accidentally exposing credentials preserved from existing files.
pub struct TakeoverPlan {
    pub preview: TakeoverPreview,
    updates: Vec<PendingFile>,
}

struct StagedChange {
    kind: ConfigFileKind,
    pending: PendingFile,
    fields: Vec<FieldChangePreview>,
}

/// Build the managed Codex/Claude takeover without writing any file. Direct
/// provider mode has its own account handoff flow and is intentionally rejected
/// here so a preview can never describe a different operation from the write.
pub fn prepare_takeover(
    config: &AppConfig,
    preferred_openai: Option<&str>,
    preferred_anthropic: Option<&str>,
) -> Result<TakeoverPlan> {
    if config.direct_codex || config.direct_claude {
        bail!("直连 Provider 切换不属于本地路由接管预览");
    }
    let mut staged = Vec::new();
    if let Some(change) = prepare_codex_takeover(config, preferred_openai)? {
        staged.push(change);
    }
    if let Some(change) = prepare_claude_takeover(config, preferred_anthropic)? {
        staged.push(change);
    }

    let confirmation_token = takeover_token(&staged);
    let changes = staged
        .iter()
        .map(|change| FileChangePreview {
            kind: change.kind,
            path: change.pending.path.to_string_lossy().into_owned(),
            before_sha256: change.pending.original.as_deref().map(sha256_hex),
            after_sha256: change.pending.updated.as_deref().map(sha256_hex),
            fields: change.fields.clone(),
        })
        .collect();
    Ok(TakeoverPlan {
        preview: TakeoverPreview {
            format: TAKEOVER_FORMAT,
            format_version: TAKEOVER_PREVIEW_VERSION,
            generated_at: Utc::now(),
            confirmation_token,
            changes,
        },
        updates: staged.into_iter().map(|change| change.pending).collect(),
    })
}

/// Apply only the exact bytes described by a previously confirmed preview.
/// A stale plan is rejected before the first write.
pub fn apply_takeover_plan(plan: TakeoverPlan, confirmation_token: &str) -> Result<()> {
    if confirmation_token != plan.preview.confirmation_token {
        bail!("接管配置确认令牌不匹配");
    }
    for update in &plan.updates {
        let current = if update.path.exists() {
            Some(
                fs::read(&update.path)
                    .with_context(|| format!("无法复核 {}", update.path.display()))?,
            )
        } else {
            None
        };
        if current != update.original {
            bail!(
                "配置已在预览后变化，请重新生成预览: {}",
                update.path.display()
            );
        }
    }
    commit_files(plan.updates)
}

fn prepare_codex_takeover(
    config: &AppConfig,
    preferred: Option<&str>,
) -> Result<Option<StagedChange>> {
    if !config.enable_codex || !config.codex_config.exists() {
        return Ok(None);
    }
    let path = &config.codex_config;
    let original =
        fs::read_to_string(path).with_context(|| format!("读取失败: {}", path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .context("Codex TOML 无法解析")?;
    let before_provider = doc
        .get("model_provider")
        .and_then(Item::as_str)
        .map(str::to_owned);
    let providers = doc
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("缺少 model_providers"))?;
    let preferred_provider = preferred.and_then(|url| {
        providers
            .iter()
            .find(|(name, item)| {
                *name != "headroom"
                    && *name != super::DIRECT_CODEX_PROVIDER
                    && super::table_string(item, "base_url").as_deref() == Some(url)
            })
            .map(|(name, _)| name.to_owned())
    });
    let upstream = if let Some(provider) = preferred_provider {
        provider
    } else if before_provider.as_deref().is_some_and(|selected| {
        selected != "headroom"
            && selected != super::DIRECT_CODEX_PROVIDER
            && providers.contains_key(selected)
    }) {
        before_provider.clone().unwrap_or_default()
    } else {
        providers
            .iter()
            .find(|(name, item)| {
                *name != "headroom"
                    && *name != super::DIRECT_CODEX_PROVIDER
                    && item.as_table().is_some()
            })
            .map(|(name, _)| name.to_owned())
            .unwrap_or_default()
    };
    if upstream.is_empty() {
        bail!("无法确定 Codex 上游 Provider");
    }
    let source = providers
        .get(&upstream)
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("找不到上游 Provider: {upstream}"))?
        .clone();
    let before_url = providers
        .get("headroom")
        .and_then(Item::as_table)
        .and_then(|table| table.get("base_url"))
        .and_then(Item::as_str)
        .map(str::to_owned);
    let target_url = format!("http://127.0.0.1:{}/v1", super::client_port(config));
    let mut headroom = Table::new();
    headroom.insert("name", value("Headroom Route"));
    headroom.insert("base_url", value(target_url.clone()));
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
    let updated = doc.to_string().into_bytes();
    if updated == original.as_bytes() {
        return Ok(None);
    }
    Ok(Some(StagedChange {
        kind: ConfigFileKind::CodexConfig,
        pending: PendingFile {
            path: path.clone(),
            original: Some(original.into_bytes()),
            updated: Some(updated),
        },
        fields: vec![
            field_change(
                "model_provider",
                before_provider.map(Value::String),
                Some(Value::String("headroom".into())),
            ),
            field_change(
                "model_providers.headroom.base_url",
                before_url.map(Value::String),
                Some(Value::String(target_url)),
            ),
        ],
    }))
}

fn prepare_claude_takeover(
    config: &AppConfig,
    _preferred: Option<&str>,
) -> Result<Option<StagedChange>> {
    if !config.enable_claude {
        return Ok(None);
    }
    let path = &config.claude_settings;
    let original = if path.exists() {
        Some(fs::read(path).with_context(|| format!("读取失败: {}", path.display()))?)
    } else {
        None
    };
    let mut root: Value = match original.as_deref() {
        Some(bytes) => serde_json::from_slice(bytes).context("Claude settings.json 无法解析")?,
        None => Value::Object(Map::new()),
    };
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
    let before = env
        .get("ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let target = format!("http://127.0.0.1:{}", super::client_port(config));
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(target.clone()));
    let updated = serde_json::to_vec_pretty(&root)?;
    if original.as_deref() == Some(updated.as_slice()) {
        return Ok(None);
    }
    Ok(Some(StagedChange {
        kind: ConfigFileKind::ClaudeSettings,
        pending: PendingFile {
            path: path.clone(),
            original,
            updated: Some(updated),
        },
        fields: vec![field_change(
            "env.ANTHROPIC_BASE_URL",
            before.map(Value::String),
            Some(Value::String(target)),
        )],
    }))
}

fn field_change(field: &str, before: Option<Value>, after: Option<Value>) -> FieldChangePreview {
    FieldChangePreview {
        field: field.into(),
        before: before.map(redacted_value),
        after: after.map(redacted_value),
    }
}

fn takeover_token(changes: &[StagedChange]) -> String {
    let mut digest = Sha256::new();
    digest.update(TAKEOVER_FORMAT.as_bytes());
    digest.update(TAKEOVER_PREVIEW_VERSION.to_le_bytes());
    for change in changes {
        digest.update(change.kind.payload_name().as_bytes());
        digest.update(change.pending.path.to_string_lossy().as_bytes());
        match change.pending.original.as_deref() {
            Some(bytes) => digest.update(sha256_bytes(bytes)),
            None => digest.update([0; 32]),
        }
        match change.pending.updated.as_deref() {
            Some(bytes) => digest.update(sha256_bytes(bytes)),
            None => digest.update([0; 32]),
        }
    }
    hex(&digest.finalize())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackupFileDescriptor {
    pub kind: ConfigFileKind,
    pub label: String,
    pub payload: String,
    pub size: u64,
    pub sha256: String,
    /// Whether the file existed when the backup was created. Older manifests
    /// without this field are treated as present, keeping backups restorable.
    #[serde(default = "backup_file_present_default")]
    pub present: bool,
}

fn backup_file_present_default() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackupDescriptor {
    pub format: String,
    pub format_version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub app_version: String,
    pub files: Vec<BackupFileDescriptor>,
}

pub fn create_config_backup(config: &AppConfig) -> Result<BackupDescriptor> {
    let root = config.state_dir.join("backups");
    fs::create_dir_all(&root)?;
    let id = unique_id();
    let temporary = root.join(format!(".{id}.tmp"));
    let destination = root.join(&id);
    fs::create_dir(&temporary)?;
    let result = (|| -> Result<BackupDescriptor> {
        let mut files = Vec::new();
        let main_path = config.state_dir.join("config.json");
        let main = if main_path.exists() {
            read_limited(&main_path)?
        } else {
            serde_json::to_vec_pretty(config)?
        };
        push_backup_payload(&temporary, ConfigFileKind::MainConfig, &main, &mut files)?;
        for (kind, path) in [
            (ConfigFileKind::CodexConfig, config.codex_config.as_path()),
            (
                ConfigFileKind::ClaudeSettings,
                config.claude_settings.as_path(),
            ),
        ] {
            if path.exists() {
                let bytes = read_limited(path)?;
                push_backup_payload(&temporary, kind, &bytes, &mut files)?;
            } else {
                push_backup_absent(kind, &mut files);
            }
        }
        match super::codex_auth_path(config) {
            Some(path) if path.exists() => {
                let bytes = read_limited(&path)?;
                push_backup_payload(&temporary, ConfigFileKind::CodexAuth, &bytes, &mut files)?;
            }
            Some(_) | None => push_backup_absent(ConfigFileKind::CodexAuth, &mut files),
        }
        let descriptor = BackupDescriptor {
            format: BACKUP_FORMAT.into(),
            format_version: BACKUP_FORMAT_VERSION,
            id: id.clone(),
            created_at: Utc::now(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            files,
        };
        atomic_write(
            &temporary.join("manifest.json"),
            &serde_json::to_vec_pretty(&descriptor)?,
        )?;
        fs::rename(&temporary, &destination)?;
        Ok(descriptor)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

pub fn list_config_backups(config: &AppConfig) -> Result<Vec<BackupDescriptor>> {
    let root = config.state_dir.join("backups");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut backups = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path().join("manifest.json");
        let Ok(bytes) = read_limited(&path) else {
            continue;
        };
        let Ok(descriptor) = serde_json::from_slice::<BackupDescriptor>(&bytes) else {
            continue;
        };
        if validate_backup_descriptor(&descriptor).is_ok()
            && descriptor.id == entry.file_name().to_string_lossy()
        {
            backups.push(descriptor);
        }
    }
    backups.sort_by_key(|backup| std::cmp::Reverse(backup.created_at));
    Ok(backups)
}

pub fn restore_config_backup(config: &AppConfig, backup_id: &str) -> Result<BackupDescriptor> {
    validate_backup_id(backup_id)?;
    let directory = config.state_dir.join("backups").join(backup_id);
    let descriptor: BackupDescriptor =
        serde_json::from_slice(&read_limited(&directory.join("manifest.json"))?)?;
    validate_backup_descriptor(&descriptor)?;
    if descriptor.id != backup_id {
        bail!("备份目录与清单 ID 不一致");
    }
    let mut updates = Vec::new();
    for file in &descriptor.files {
        if file.payload != file.kind.payload_name() {
            bail!("备份清单包含无效载荷名称");
        }
        let target = restore_target(config, file.kind)?;
        let exists_now = target.exists();
        if file.present {
            let bytes = read_limited(&directory.join(&file.payload))?;
            if bytes.len() as u64 != file.size || sha256_hex(&bytes) != file.sha256 {
                bail!("备份载荷校验失败: {}", file.label);
            }
            validate_config_payload(file.kind, &bytes)?;
            let original = if exists_now {
                Some(read_limited(&target)?)
            } else {
                None
            };
            updates.push(PendingFile {
                path: target,
                original,
                updated: Some(bytes),
            });
        } else if exists_now {
            // The file was absent when the backup was created but has appeared
            // since. Remove it within the transaction; its current content is
            // kept as the rollback original so a later failure restores it.
            let original = Some(read_limited(&target)?);
            updates.push(PendingFile {
                path: target,
                original,
                updated: None,
            });
        }
    }
    commit_files(updates)?;
    Ok(descriptor)
}

fn push_backup_payload(
    directory: &Path,
    kind: ConfigFileKind,
    bytes: &[u8],
    files: &mut Vec<BackupFileDescriptor>,
) -> Result<()> {
    validate_config_payload(kind, bytes)?;
    let payload = kind.payload_name();
    atomic_write(&directory.join(payload), bytes)?;
    files.push(BackupFileDescriptor {
        kind,
        label: kind.label().into(),
        payload: payload.into(),
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
        present: true,
    });
    Ok(())
}

fn push_backup_absent(kind: ConfigFileKind, files: &mut Vec<BackupFileDescriptor>) {
    files.push(BackupFileDescriptor {
        kind,
        label: kind.label().into(),
        payload: kind.payload_name().into(),
        size: 0,
        sha256: String::new(),
        present: false,
    });
}

fn validate_backup_descriptor(descriptor: &BackupDescriptor) -> Result<()> {
    if descriptor.format != BACKUP_FORMAT {
        bail!("不是 HeadroomRoute 配置备份");
    }
    if descriptor.format_version != BACKUP_FORMAT_VERSION {
        bail!("不支持的配置备份版本: {}", descriptor.format_version);
    }
    validate_backup_id(&descriptor.id)?;
    if descriptor.files.is_empty() {
        bail!("配置备份为空");
    }
    let mut seen = Vec::new();
    for file in &descriptor.files {
        if file.payload != file.kind.payload_name() {
            bail!("备份清单包含无效载荷名称: {}", file.label);
        }
        if seen.contains(&file.kind) {
            bail!("配置备份包含重复文件类型: {}", file.label);
        }
        seen.push(file.kind);
        // An absent file must never carry payload metadata; a present file
        // may legitimately be empty (size 0 with the empty-content hash).
        if !file.present && (file.size != 0 || !file.sha256.is_empty()) {
            bail!("配置备份中缺失文件不应带有载荷信息: {}", file.label);
        }
    }
    Ok(())
}

fn validate_backup_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("无效备份 ID");
    }
    Ok(())
}

fn restore_target(config: &AppConfig, kind: ConfigFileKind) -> Result<PathBuf> {
    match kind {
        ConfigFileKind::MainConfig => Ok(config.state_dir.join("config.json")),
        ConfigFileKind::CodexConfig => Ok(config.codex_config.clone()),
        ConfigFileKind::CodexAuth => {
            super::codex_auth_path(config).ok_or_else(|| anyhow!("无法确定 Codex auth.json 路径"))
        }
        ConfigFileKind::ClaudeSettings => Ok(config.claude_settings.clone()),
    }
}

fn validate_config_payload(kind: ConfigFileKind, bytes: &[u8]) -> Result<()> {
    match kind {
        ConfigFileKind::MainConfig => {
            serde_json::from_slice::<AppConfig>(bytes)
                .context("HeadroomRoute config.json 无法解析")?;
        }
        ConfigFileKind::CodexConfig => {
            std::str::from_utf8(bytes)
                .context("Codex config.toml 不是 UTF-8")?
                .parse::<DocumentMut>()
                .context("Codex config.toml 无法解析")?;
        }
        ConfigFileKind::CodexAuth | ConfigFileKind::ClaudeSettings => {
            let value: Value = serde_json::from_slice(bytes).context("JSON 配置无法解析")?;
            if !value.is_object() {
                bail!("JSON 配置根节点必须是对象");
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct DiagnosticBundleDescriptor {
    pub format: &'static str,
    pub format_version: u32,
    pub created_at: DateTime<Utc>,
    pub app_version: &'static str,
    pub entries: Vec<String>,
    pub exclusions: Vec<&'static str>,
}

/// Create a redacted support archive. Deliberately excludes auth.json, the
/// proxy metrics log and every request/response body source.
pub fn create_diagnostic_bundle(
    config: &AppConfig,
    destination: &Path,
    precheck_report: Option<&str>,
) -> Result<DiagnosticBundleDescriptor> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let app_config = redacted_json(serde_json::to_value(config)?);
    entries.push((
        "config/headroom-route.json".into(),
        serde_json::to_vec_pretty(&app_config)?,
    ));
    if config.codex_config.exists() {
        let text = std::str::from_utf8(&read_limited(&config.codex_config)?)?.to_owned();
        match toml_edit::de::from_str::<Value>(&text) {
            Ok(value) => entries.push((
                "config/codex-redacted.json".into(),
                serde_json::to_vec_pretty(&redacted_json(value))?,
            )),
            Err(_) => entries.push((
                "config/codex-read-error.txt".into(),
                b"Codex config.toml could not be parsed; contents omitted.\n".to_vec(),
            )),
        }
    }
    if config.claude_settings.exists() {
        let value: Value = serde_json::from_slice(&read_limited(&config.claude_settings)?)
            .context("Claude settings.json 无法解析")?;
        entries.push((
            "config/claude-redacted.json".into(),
            serde_json::to_vec_pretty(&redacted_json(value))?,
        ));
    }
    for (source, name) in [
        (config.state_dir.join("status.json"), "state/status.json"),
        (config.state_dir.join("runtime.json"), "state/runtime.json"),
    ] {
        if source.exists() {
            let value: Value = serde_json::from_slice(&read_limited(&source)?)
                .with_context(|| format!("无法解析诊断状态: {}", source.display()))?;
            entries.push((
                name.into(),
                serde_json::to_vec_pretty(&redacted_json(value))?,
            ));
        }
    }
    if let Some(report) = precheck_report {
        entries.push((
            "precheck.txt".into(),
            redact_sensitive_text(report).into_bytes(),
        ));
    }
    let descriptor = DiagnosticBundleDescriptor {
        format: "headroom-route-diagnostic-bundle",
        format_version: 1,
        created_at: Utc::now(),
        app_version: env!("CARGO_PKG_VERSION"),
        entries: entries.iter().map(|(name, _)| name.clone()).collect(),
        exclusions: vec![
            "API keys and credentials",
            "Codex auth.json",
            "request and response bodies",
            "proxy traffic logs",
        ],
    };
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    archive.start_file("manifest.json", options)?;
    archive.write_all(&serde_json::to_vec_pretty(&descriptor)?)?;
    for (name, bytes) in &entries {
        archive.start_file(name, options)?;
        archive.write_all(bytes)?;
    }
    let bytes = archive.finish()?.into_inner();
    atomic_write(destination, &bytes)?;
    Ok(descriptor)
}

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

fn redacted_json(mut value: Value) -> Value {
    redact_json_in_place(&mut value);
    value
}

fn redacted_value(mut value: Value) -> Value {
    redact_json_in_place(&mut value);
    value
}

fn redact_json_in_place(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_secret_key(key) {
                    *value = Value::String(REDACTED.into());
                } else {
                    redact_json_in_place(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_json_in_place),
        Value::String(text) => *text = redact_sensitive_text(text),
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("apikey")
        || normalized.contains("authtoken")
        || normalized.contains("accesstoken")
        || normalized.contains("refreshtoken")
        || normalized.contains("clientsecret")
        || normalized.contains("controltoken")
        || normalized.contains("sessiontoken")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || normalized.contains("password")
        || normalized.contains("privatekey")
        || normalized == "token"
        || normalized == "secret"
        || normalized == "cookie"
}

/// Value tokens that carry a credential marker inline, e.g. `api_key=...`,
/// `client_secret: ...`. Kept explicit so stat keys such as `token_count` or
/// `input_tokens` are never treated as secrets.
const SECRET_VALUE_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "auth_token=",
    "access_token=",
    "refresh_token=",
    "client_secret=",
    "control_token=",
    "session_token=",
    "authorization:",
    "password=",
    "secret=",
    "api_key:",
    "client_secret:",
    "control_token:",
    "session_token:",
    "auth_token:",
    "access_token:",
    "refresh_token:",
    "password:",
    "secret:",
];

pub(crate) fn redact_sensitive_text(text: &str) -> String {
    let mut redact_next = false;
    text.split_inclusive(char::is_whitespace)
        .map(|part| {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[trimmed.len()..];
            let unquoted = trimmed.trim_matches(['"', '\'', '`']);
            let lower = unquoted.to_ascii_lowercase();
            let separator = lower.ends_with([':', '=']);
            let key_name = if separator {
                lower.trim_end_matches([':', '='])
            } else {
                lower.as_str()
            };
            // `api_key:` / `api_key =` mark the *next* token as the secret
            // value even when it is opaque (no `sk-` prefix, e.g. a JWT).
            let key_is_secret = is_secret_key(key_name);
            let should_redact = redact_next
                || contains_obvious_secret(trimmed)
                || key_is_secret
                || SECRET_VALUE_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker));
            // A lone `=`/`:` between the key and its value keeps the value
            // flagged for redaction rather than resetting it.
            redact_next = lower == "bearer"
                || lower.ends_with("authorization:")
                || key_is_secret
                || (redact_next && (lower == "=" || lower == ":"));
            if should_redact {
                format!("{REDACTED}{suffix}")
            } else if let Some(url) = redact_url_userinfo(trimmed) {
                format!("{url}{suffix}")
            } else {
                part.to_owned()
            }
        })
        .collect()
}

/// Replace the userinfo portion of an http(s) URL so credentials embedded in
/// an address never reach a diagnostic bundle or preview text.
fn redact_url_userinfo(part: &str) -> Option<String> {
    let scheme_end = part.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = part[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(part.len());
    let at = part[authority_start..authority_end].find('@')?;
    let host_start = authority_start + at + 1;
    Some(format!(
        "{}[REDACTED]@{}",
        &part[..authority_start],
        &part[host_start..]
    ))
}

fn contains_obvious_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    (lower.contains("sk-") || lower.contains("sk_")) && value.len() >= 12
}

fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::metadata(path).with_context(|| format!("无法读取文件元数据: {}", path.display()))?;
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        bail!("配置文件超过 8 MiB 限制: {}", path.display());
    }
    let file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        bail!("配置文件超过 8 MiB 限制: {}", path.display());
    }
    Ok(bytes)
}

fn unique_id() -> String {
    let now = Utc::now().format("%Y%m%d-%H%M%S%3f");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    format!("{now}-{}-{nanos:09}", std::process::id())
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256_bytes(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use zip::ZipArchive;

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "headroom-route-portability-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn fixture_config(dir: &Path) -> AppConfig {
        AppConfig {
            state_dir: dir.join("state"),
            legacy_state_dir: dir.join("legacy"),
            codex_config: dir.join("codex/config.toml"),
            claude_settings: dir.join("claude/settings.json"),
            cc_switch_db: dir.join("missing.db"),
            enable_codex: true,
            enable_claude: true,
            direct_codex: false,
            direct_claude: false,
            ..AppConfig::default()
        }
    }

    fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn write_fixture(config: &AppConfig, secret: &str) {
        fs::create_dir_all(config.codex_config.parent().unwrap()).unwrap();
        fs::create_dir_all(config.claude_settings.parent().unwrap()).unwrap();
        fs::create_dir_all(&config.state_dir).unwrap();
        fs::write(
            &config.codex_config,
            format!(
                "model_provider = \"upstream\"\n\n[model_providers.upstream]\nbase_url = \"https://api.example.com/v1\"\napi_key = \"{secret}\"\n"
            ),
        )
        .unwrap();
        fs::write(
            &config.claude_settings,
            format!(
                r#"{{"theme":"dark","env":{{"ANTHROPIC_AUTH_TOKEN":"{secret}","ANTHROPIC_BASE_URL":"https://claude.example.com"}}}}"#
            ),
        )
        .unwrap();
        fs::write(
            config.codex_config.parent().unwrap().join("auth.json"),
            format!(r#"{{"OPENAI_API_KEY":"{secret}"}}"#),
        )
        .unwrap();
        fs::write(
            config.state_dir.join("config.json"),
            serde_json::to_vec_pretty(config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn takeover_preview_is_redacted_and_requires_fresh_confirmation() {
        let dir = test_dir("preview");
        let config = fixture_config(&dir);
        let secret = "sk-test-preview-secret-0123456789";
        write_fixture(&config, secret);

        let plan = prepare_takeover(&config, None, None).unwrap();
        let serialized = serde_json::to_string_pretty(&plan.preview).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("ANTHROPIC_AUTH_TOKEN"));
        assert_eq!(plan.preview.format_version, TAKEOVER_PREVIEW_VERSION);
        let token = plan.preview.confirmation_token.clone();
        assert!(apply_takeover_plan(plan, "wrong-token").is_err());
        assert!(
            fs::read_to_string(&config.codex_config)
                .unwrap()
                .contains("model_provider = \"upstream\"")
        );

        let stale = prepare_takeover(&config, None, None).unwrap();
        fs::write(&config.codex_config, "model_provider = \"changed\"\n").unwrap();
        assert!(apply_takeover_plan(stale, &token).is_err());

        write_fixture(&config, secret);
        let plan = prepare_takeover(&config, None, None).unwrap();
        let token = plan.preview.confirmation_token.clone();
        apply_takeover_plan(plan, &token).unwrap();
        let claude = fs::read_to_string(&config.claude_settings).unwrap();
        assert!(claude.contains(secret));
        assert!(claude.contains("127.0.0.1"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_restore_rolls_back_every_file_when_a_later_target_fails() {
        let dir = test_dir("backup-rollback");
        let config = fixture_config(&dir);
        write_fixture(&config, "sk-test-backup-secret-0123456789");
        let backup = create_config_backup(&config).unwrap();
        assert_eq!(backup.format_version, BACKUP_FORMAT_VERSION);
        assert_eq!(list_config_backups(&config).unwrap().len(), 1);

        let changed_codex = b"model_provider = \"changed\"\n\n[model_providers.changed]\nbase_url = \"https://changed.example.com\"\n";
        fs::write(&config.codex_config, changed_codex).unwrap();
        let mut changed_config = config.clone();
        changed_config.auto_failover = true;
        let changed_main = serde_json::to_vec_pretty(&changed_config).unwrap();
        fs::write(config.state_dir.join("config.json"), &changed_main).unwrap();

        let blocked = dir.join("blocked");
        fs::write(&blocked, b"not-a-directory").unwrap();
        let restore_config = AppConfig {
            claude_settings: blocked.join("settings.json"),
            ..config.clone()
        };
        assert!(restore_config_backup(&restore_config, &backup.id).is_err());
        assert_eq!(fs::read(&config.codex_config).unwrap(), changed_codex);
        assert_eq!(
            fs::read(config.state_dir.join("config.json")).unwrap(),
            changed_main
        );
        assert!(restore_config_backup(&config, "../escape").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_records_absent_files_and_restore_removes_them() {
        let dir = test_dir("backup-absent");
        let config = fixture_config(&dir);
        write_fixture(&config, "sk-test-backup-absent-0123456789");
        let auth_path = config.codex_config.parent().unwrap().join("auth.json");
        fs::remove_file(&auth_path).unwrap();

        let backup = create_config_backup(&config).unwrap();
        let auth_entry = backup
            .files
            .iter()
            .find(|file| file.kind == ConfigFileKind::CodexAuth)
            .expect("备份清单应包含 CodexAuth");
        assert!(!auth_entry.present);

        let reintroduced = r#"{"OPENAI_API_KEY":"sk-test-reintroduced-0123456789"}"#;
        fs::write(&auth_path, reintroduced).unwrap();
        assert!(auth_path.exists());

        let changed_codex = b"model_provider = \"changed\"\n";
        fs::write(&config.codex_config, changed_codex).unwrap();

        restore_config_backup(&config, &backup.id).unwrap();
        assert!(!auth_path.exists(), "还原应删除备份时本不存在的 auth.json");
        assert_eq!(
            fs::read_to_string(&config.codex_config).unwrap(),
            "model_provider = \"upstream\"\n\n[model_providers.upstream]\nbase_url = \"https://api.example.com/v1\"\napi_key = \"sk-test-backup-absent-0123456789\"\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_manifest_without_present_field_defaults_to_present() {
        let descriptor: BackupDescriptor = serde_json::from_str(
            r#"{
                "format": "headroom-route-config-backup",
                "format_version": 1,
                "minimum_reader_version": 1,
                "created_at": "2026-01-01T00:00:00Z",
                "id": "legacy",
                "app_version": "0.6.0",
                "files": [
                    {
                        "kind": "codex_config",
                        "label": "Codex 配置",
                        "payload": "codex-config.toml",
                        "size": 3,
                        "sha256": "4b2b0bd29f9a10f1d60b3f043d3e1e46c1e06fdb7c50be6b14e3a29488fcdd60"
                    }
                ]
            }"#,
        )
        .unwrap();
        assert!(descriptor.files[0].present);
        assert!(validate_backup_descriptor(&descriptor).is_ok());
    }

    #[test]
    fn backup_validation_rejects_duplicate_kinds_and_contradictory_flags() {
        let dir = test_dir("backup-validate");
        let config = fixture_config(&dir);
        write_fixture(&config, "sk-test-backup-validate-0123456789");
        let base = create_config_backup(&config).unwrap();

        let mut duplicate = base.clone();
        duplicate.files.push(duplicate.files[0].clone());
        assert!(validate_backup_descriptor(&duplicate).is_err());

        let mut absent_with_metadata = base.clone();
        for file in absent_with_metadata.files.iter_mut() {
            file.present = false;
        }
        absent_with_metadata.files[0].size = 7;
        assert!(validate_backup_descriptor(&absent_with_metadata).is_err());

        let mut absent_with_hash = base.clone();
        for file in absent_with_hash.files.iter_mut() {
            file.present = false;
        }
        absent_with_hash.files[0].sha256 = "ab12cd34".into();
        assert!(validate_backup_descriptor(&absent_with_hash).is_err());

        let mut absent_clean = base.clone();
        for file in absent_clean.files.iter_mut() {
            file.present = false;
            file.size = 0;
            file.sha256.clear();
        }
        assert!(validate_backup_descriptor(&absent_clean).is_ok());

        let mut empty_present = base.clone();
        empty_present.files[0].size = 0;
        empty_present.files[0].sha256 =
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into();
        assert!(validate_backup_descriptor(&empty_present).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn portable_export_omits_credentials_and_import_is_forward_compatible() {
        let dir = test_dir("portable");
        let mut config = fixture_config(&dir);
        config.auto_failover = true;
        write_fixture(&config, "sk-test-portable-secret-0123456789");
        let export = dir.join("portable.json");
        export_portable_config(&config, &export).unwrap();
        let text = fs::read_to_string(&export).unwrap();
        assert!(!text.contains("sk-test-portable"));
        assert!(!text.contains("OPENAI_API_KEY"));
        assert!(!text.contains(&config.codex_config.to_string_lossy().into_owned()));

        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["format_version"] = Value::from(2);
        value["minimum_reader_version"] = Value::from(1);
        value["future_field"] = Value::String("ignored".into());
        let decoded =
            decode_portable_config(&serde_json::to_vec(&value).unwrap(), &config).unwrap();
        assert!(decoded.auto_failover);
        assert_eq!(decoded.codex_config, config.codex_config);

        let destination = config.state_dir.join("config.json");
        let before = fs::read(&destination).unwrap();
        value["settings"]["agent_port"] = Value::from(0);
        fs::write(&export, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(import_portable_config(&export, &destination, &config).is_err());
        assert_eq!(fs::read(&destination).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn diagnostic_bundle_excludes_secrets_auth_and_traffic_logs() {
        let dir = test_dir("diagnostic");
        let config = fixture_config(&dir);
        let secret = "sk-test-diagnostic-secret-0123456789";
        let opaque = "opaque-credential-eyJhbGciOiJIUzI1NiJ9.value";
        write_fixture(&config, secret);
        fs::write(
            &config.claude_settings,
            format!(
                r#"{{"client_secret":"{opaque}","env":{{"ANTHROPIC_AUTH_TOKEN":"{secret}","ANTHROPIC_BASE_URL":"https://claude.example.com"}}}}"#
            ),
        )
        .unwrap();
        fs::write(
            config.state_dir.join("status.json"),
            format!(r#"{{"service":"headroom-route","last_error":"Bearer {secret}"}}"#),
        )
        .unwrap();
        fs::write(
            config.state_dir.join("headroom-proxy.jsonl"),
            format!(r#"{{"request_body":"{secret}"}}"#),
        )
        .unwrap();
        let archive_path = dir.join("diagnostic.zip");
        create_diagnostic_bundle(
            &config,
            &archive_path,
            Some(&format!("authorization: Bearer {secret}")),
        )
        .unwrap();

        let mut archive = ZipArchive::new(fs::File::open(&archive_path).unwrap()).unwrap();
        let mut combined = String::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            assert!(!entry.name().contains("auth.json"));
            assert!(!entry.name().contains("proxy.jsonl"));
            let mut raw = Vec::new();
            entry.read_to_end(&mut raw).unwrap();
            assert!(
                !contains_slice(&raw, secret.as_bytes()),
                "raw bytes leaked {secret} in {}",
                entry.name()
            );
            assert!(
                !contains_slice(&raw, opaque.as_bytes()),
                "raw bytes leaked opaque credential in {}",
                entry.name()
            );
            combined.push_str(&String::from_utf8_lossy(&raw));
        }
        assert!(!combined.contains(secret));
        assert!(!combined.contains(opaque));
        assert!(!combined.contains("request_body"));
        assert!(combined.contains(REDACTED));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn redact_sensitive_text_removes_url_credentials() {
        assert_eq!(
            redact_sensitive_text("upstream https://user:pass@host.example.com/path?x=1"),
            "upstream https://[REDACTED]@host.example.com/path?x=1"
        );
        assert_eq!(
            redact_sensitive_text("base_url = \"https://user:pass@example.com/v1\""),
            "base_url = \"https://[REDACTED]@example.com/v1\""
        );
        assert_eq!(
            redact_sensitive_text("https://token@example.com"),
            "https://[REDACTED]@example.com"
        );
        assert_eq!(
            redact_sensitive_text("https://plain.example.com/v1"),
            "https://plain.example.com/v1"
        );
        assert_eq!(redact_sensitive_text("no url here"), "no url here");
        assert_eq!(
            redact_sensitive_text("Bearer sk-secret-token-0123456789"),
            "Bearer [REDACTED]"
        );
    }

    #[test]
    fn redact_sensitive_text_redacts_colon_separated_credentials() {
        assert_eq!(
            redact_sensitive_text("api_key: 0123456789abcdef"),
            "[REDACTED] [REDACTED]"
        );
        assert_eq!(
            redact_sensitive_text("client_secret: eyJhbGciOiJIUzI1NiJ9.abc"),
            "[REDACTED] [REDACTED]"
        );
        assert_eq!(
            redact_sensitive_text("control_token = \"opaque-value-123\""),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
        assert_eq!(
            redact_sensitive_text("session_token=\"opaque-value-456\""),
            "[REDACTED]"
        );
        assert_eq!(
            redact_sensitive_text("authorization: Bearer abcdef0123456789"),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn redact_sensitive_text_leaves_stat_keys_untouched() {
        assert_eq!(
            redact_sensitive_text(r#"token_count: 12, input_tokens: 34"#),
            r#"token_count: 12, input_tokens: 34"#
        );
        assert_eq!(
            redact_sensitive_text(r#"{"token_count":12,"input_tokens":34}"#),
            r#"{"token_count":12,"input_tokens":34}"#
        );
        assert_eq!(
            redact_sensitive_text("no secrets, tokens here: 7"),
            "no secrets, tokens here: 7"
        );
    }

    #[test]
    fn secret_key_recognition_covers_client_secret_and_tokens() {
        for key in [
            "client_secret",
            "CLIENT_SECRET",
            "control_token",
            "session_token",
            "api_key",
            "access_token",
            "auth_token",
            "refresh_token",
            "authorization",
            "password",
            "secret",
        ] {
            assert!(is_secret_key(key), "{key} should be secret");
        }
        for key in [
            "token_count",
            "input_tokens",
            "output_tokens",
            "completed_requests",
            "latency_ms",
            "usage",
        ] {
            assert!(!is_secret_key(key), "{key} should not be secret");
        }
    }

    #[test]
    fn backup_restore_rejects_tampered_payload_without_touching_targets() {
        let dir = test_dir("backup-tamper");
        let config = fixture_config(&dir);
        write_fixture(&config, "sk-test-backup-tamper-secret-0123456789");
        let backup = create_config_backup(&config).unwrap();
        let backup_dir = config.state_dir.join("backups").join(&backup.id);
        fs::write(
            backup_dir.join("codex-config.toml"),
            b"model_provider = \"tampered\"\n",
        )
        .unwrap();
        assert!(restore_config_backup(&config, &backup.id).is_err());
        assert!(
            fs::read_to_string(&config.codex_config)
                .unwrap()
                .contains("model_provider = \"upstream\"")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_listing_skips_incomplete_directories_and_rejects_bad_ids() {
        let dir = test_dir("backup-listing");
        let config = fixture_config(&dir);
        write_fixture(&config, "sk-test-backup-listing-secret-0123456789");
        let backup = create_config_backup(&config).unwrap();
        let root = config.state_dir.join("backups");
        fs::create_dir_all(root.join(".partial")).unwrap();
        fs::create_dir_all(root.join("missing-manifest")).unwrap();
        let listed = list_config_backups(&config).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, backup.id);
        for bad in ["..", "a/b", "..\\..", "", "id with space", "ümlaut", "."] {
            assert!(restore_config_backup(&config, bad).is_err(), "{bad}");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn portable_import_applies_settings_and_writes_atomically() {
        let dir = test_dir("portable-roundtrip");
        let mut config = fixture_config(&dir);
        config.auto_failover = true;
        write_fixture(&config, "sk-test-portable-roundtrip-secret-0123456789");
        let export = dir.join("portable.json");
        export_portable_config(&config, &export).unwrap();
        let mut fresh = config.clone();
        fresh.auto_failover = false;
        fresh.agent_port = 9999;

        let destination = config.state_dir.join("config.json");
        fs::write(&destination, serde_json::to_vec_pretty(&fresh).unwrap()).unwrap();
        let before = fs::read(&destination).unwrap();
        let updated = import_portable_config(&export, &destination, &fresh).unwrap();
        assert!(updated.auto_failover);
        assert_eq!(updated.agent_port, config.agent_port);
        let on_disk: AppConfig = serde_json::from_slice(&fs::read(&destination).unwrap()).unwrap();
        assert!(on_disk.auto_failover);
        assert_ne!(before, fs::read(&destination).unwrap());

        let new_destination = dir.join("fresh-config.json");
        assert!(!new_destination.exists());
        import_portable_config(&export, &new_destination, &fresh).unwrap();
        assert!(new_destination.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
