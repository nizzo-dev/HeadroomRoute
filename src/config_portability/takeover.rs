use std::fs;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use super::{
    AppConfig, ConfigFileKind, PendingFile, TAKEOVER_FORMAT, TAKEOVER_PREVIEW_VERSION,
    commit_files, hex, redacted_value, sha256_bytes, sha256_hex,
};

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
    if !config.manage_codex && !config.manage_claude {
        bail!("观测模式（未接管上游）不属于本地路由接管预览");
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
                    && *name != super::super::DIRECT_CODEX_PROVIDER
                    && super::super::table_string(item, "base_url").as_deref() == Some(url)
            })
            .map(|(name, _)| name.to_owned())
    });
    let upstream = if let Some(provider) = preferred_provider {
        provider
    } else if before_provider.as_deref().is_some_and(|selected| {
        selected != "headroom"
            && selected != super::super::DIRECT_CODEX_PROVIDER
            && providers.contains_key(selected)
    }) {
        before_provider.clone().unwrap_or_default()
    } else {
        providers
            .iter()
            .find(|(name, item)| {
                *name != "headroom"
                    && *name != super::super::DIRECT_CODEX_PROVIDER
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
    let target_url = format!("http://127.0.0.1:{}/v1", super::super::client_port(config));
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
    let target = format!("http://127.0.0.1:{}", super::super::client_port(config));
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
