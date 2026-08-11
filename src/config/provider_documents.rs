use super::DIRECT_CODEX_PROVIDER;
use crate::{
    config::discovery::{is_local_service, normalize_url},
    model::AppConfig,
};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table, value};

pub(super) fn apply_codex_provider_document(
    config: &AppConfig,
    current: &mut DocumentMut,
    source: &DocumentMut,
) -> Result<(String, String)> {
    let selected = source
        .get("model_provider")
        .and_then(Item::as_str)
        .ok_or_else(|| anyhow!("CC-Switch Codex Provider 缺少 model_provider"))?
        .to_owned();
    if matches!(selected.as_str(), "headroom" | DIRECT_CODEX_PROVIDER) {
        return Err(anyhow!("CC-Switch Codex Provider 使用了保留名称"));
    }
    let source_provider = source
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(&selected))
        .and_then(Item::as_table)
        .ok_or_else(|| anyhow!("CC-Switch Codex Provider 缺少上游配置"))?
        .clone();
    let target = source_provider
        .get("base_url")
        .and_then(Item::as_str)
        .map(normalize_url)
        .transpose()?
        .ok_or_else(|| anyhow!("CC-Switch Codex Provider 缺少有效 base_url"))?;
    if is_local_service(&target, config) {
        return Err(anyhow!("Codex 直连上游不能是本地 HeadroomRoute 地址"));
    }
    if current
        .get("model_providers")
        .and_then(Item::as_table)
        .is_none()
    {
        current["model_providers"] = Item::Table(Table::new());
    }
    let providers = current
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow!("Codex 配置缺少 model_providers"))?;
    providers.remove(DIRECT_CODEX_PROVIDER);
    providers.insert(&selected, Item::Table(source_provider));
    current["model_provider"] = value(selected.clone());
    if let Some(item) = source.get("model") {
        current["model"] = item.clone();
    } else {
        current.remove("model");
    }
    if let Some(item) = source.get("openai_base_url") {
        current["openai_base_url"] = item.clone();
    } else if current
        .get("openai_base_url")
        .and_then(Item::as_str)
        .is_some_and(|url| is_local_service(url, config))
    {
        current.remove("openai_base_url");
    }
    Ok((selected, target))
}

pub(super) fn is_claude_provider_key(key: &str) -> bool {
    key == "ANTHROPIC_BASE_URL"
        || key == "ANTHROPIC_API_KEY"
        || key == "ANTHROPIC_AUTH_TOKEN"
        || key.starts_with("ANTHROPIC_")
        || key.starts_with("CLAUDE_CODE_")
}

pub(super) fn apply_claude_provider_settings(
    config: &AppConfig,
    root: &mut Value,
    settings: &Value,
    preferred: Option<&str>,
) -> Result<String> {
    let source_env = settings
        .get("env")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("CC-Switch Claude Provider 缺少 env"))?;
    let source_target = source_env
        .get("ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .map(normalize_url)
        .transpose()?;
    let target = preferred
        .and_then(|value| normalize_url(value).ok())
        .or(source_target)
        .ok_or_else(|| anyhow!("无法确定 Claude 直连上游 Provider"))?;
    if is_local_service(&target, config) {
        return Err(anyhow!("Claude 直连上游不能是本地 HeadroomRoute 地址"));
    }
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
    // Provider-scoped variables must not leak from the previous account.
    env.retain(|key, _| !is_claude_provider_key(key));
    for (key, value) in source_env {
        env.insert(key.clone(), value.clone());
    }
    env.insert("ANTHROPIC_BASE_URL".into(), Value::String(target.clone()));
    if let Some(model) = settings.get("model") {
        root_obj.insert("model".into(), model.clone());
    }
    Ok(target)
}
