use crate::{
    model::{AppConfig, Protocol},
    sqlite,
};
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fs};
use toml_edit::{DocumentMut, Item, value};

use super::atomic_write;

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

pub(super) fn sync_codex_model(config: &AppConfig, settings: &Value) -> Result<Option<String>> {
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

pub(super) fn sync_claude_models(config: &AppConfig, settings: &Value) -> Result<Option<String>> {
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

pub(super) fn claude_model_values(env: &Map<String, Value>) -> BTreeMap<String, Value> {
    env.iter()
        .filter(|(key, value)| is_claude_model_key(key) && value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub(super) fn is_claude_model_key(key: &str) -> bool {
    key == "ANTHROPIC_MODEL"
        || key == "CLAUDE_CODE_SUBAGENT_MODEL"
        || (key.starts_with("ANTHROPIC_DEFAULT_")
            && (key.ends_with("_MODEL") || key.ends_with("_MODEL_NAME")))
}
