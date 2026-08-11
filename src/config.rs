use crate::{
    model::{AppConfig, Protocol, Route},
    sqlite,
};
mod client_restore;
mod client_sync;
mod discovery;
mod file_io;
mod file_transaction;
mod model_sync;
mod provider_documents;
mod system_proxy;

pub use client_restore::*;
pub use client_sync::*;
pub use model_sync::sync_provider_models;
use model_sync::{claude_model_values, is_claude_model_key};
#[cfg(test)]
use model_sync::{sync_claude_models, sync_codex_model};

use discovery::{client_port, is_local_service};
pub use discovery::{discover_routes, normalize_url};
use file_io::{atomic_write, backup};
use file_transaction::{PendingFile, commit_files};
use provider_documents::{
    apply_claude_provider_settings, apply_codex_provider_document, is_claude_provider_key,
};
pub use system_proxy::{outbound_proxy_url, reqwest_outbound_proxy};

#[cfg(test)]
use discovery::{effective_claude_base_url, parse_codex_text, push_fallback, push_unique};
#[cfg(test)]
use system_proxy::parse_proxy_server;

#[cfg(test)]
fn rollback_files(committed: Vec<(PathBuf, Option<Vec<u8>>)>) -> Vec<String> {
    file_transaction::rollback_files(committed)
}

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

#[path = "config_portability.rs"]
pub mod portability;

pub const DIRECT_CODEX_PROVIDER: &str = "headroom_route_direct";

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
    #[serde(default)]
    codex_auth_baseline: Option<String>,
    #[serde(default)]
    codex_auth_managed: bool,
    #[serde(default)]
    claude_provider_env_managed: bool,
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
    config
        .routing_strategy
        .validate()
        .context("routing strategy configuration is invalid")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(path, &serde_json::to_vec_pretty(config)?)
}

fn table_string(item: &Item, key: &str) -> Option<String> {
    item.get(key)?.as_str().map(str::to_owned)
}

fn cc_switch_settings(
    config: &AppConfig,
    protocol: Protocol,
    provider: &str,
) -> Result<Option<Value>> {
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
    let settings = serde_json::from_str::<Value>(&row.settings)
        .with_context(|| format!("CC-Switch Provider {} 配置无法解析", row.name))?;
    Ok(Some(settings))
}

fn codex_auth_path(config: &AppConfig) -> Option<PathBuf> {
    config
        .codex_config
        .parent()
        .map(|parent| parent.join("auth.json"))
}

fn pending_codex_auth(config: &AppConfig, settings: &Value) -> Result<Option<PendingFile>> {
    let Some(path) = codex_auth_path(config) else {
        return Ok(None);
    };
    let original = if path.exists() {
        Some(fs::read(&path).with_context(|| format!("无法读取 {}", path.display()))?)
    } else {
        None
    };
    let original_value = original
        .as_deref()
        .map(|bytes| {
            serde_json::from_slice::<Value>(bytes)
                .with_context(|| format!("Codex auth.json 无法解析: {}", path.display()))
        })
        .transpose()?
        .unwrap_or_else(|| Value::Object(Map::new()));
    if !original_value.is_object() {
        return Err(anyhow!("Codex auth.json 根节点必须是对象"));
    }
    let mut updated = original_value;
    let auth = settings.get("auth").and_then(Value::as_object);
    let object = updated.as_object_mut().unwrap();
    if let Some(auth) = auth {
        for (key, value) in auth {
            object.insert(key.clone(), value.clone());
        }
    }
    // CC-Switch uses this key for API-key providers. Remove a previous key
    // when the newly selected provider uses pass-through/OAuth auth instead.
    if auth.is_none_or(|auth| !auth.contains_key("OPENAI_API_KEY")) {
        object.remove("OPENAI_API_KEY");
    }
    let updated = serde_json::to_vec_pretty(&updated)?;
    if original.as_deref() == Some(updated.as_slice()) {
        return Ok(None);
    }
    Ok(Some(PendingFile {
        path,
        original,
        updated: Some(updated),
    }))
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
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
        DocumentMut, Item, effective_claude_base_url, parse_proxy_server, push_fallback,
        push_unique, routing_drifted_with_targets, sync_all_with_targets, sync_claude_with_target,
    };
    use crate::model::{AppConfig, AuthStyle, Protocol, Route};
    use serde_json::Value;
    use std::fs;

    #[test]
    fn legacy_config_defaults_api_key_hover_to_off() {
        let mut value = serde_json::to_value(AppConfig::default()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("show_api_key_on_hover");
        let loaded: AppConfig = serde_json::from_value(value).unwrap();
        assert!(!loaded.show_api_key_on_hover);
    }

    #[test]
    fn rollback_files_ignores_missing_created_files_and_reports_other_failures() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-rollback-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let created = dir.join("created.json");
        assert!(!created.exists());
        let failures = super::rollback_files(vec![(created.clone(), None)]);
        assert!(
            failures.is_empty(),
            "missing created file is already reverted"
        );

        fs::write(&created, b"{}").unwrap();
        let failures = super::rollback_files(vec![(created.clone(), None)]);
        assert!(failures.is_empty());
        assert!(
            !created.exists(),
            "created file should be removed on rollback"
        );

        let restored = dir.join("restored.json");
        fs::write(&restored, b"old").unwrap();
        let failures = super::rollback_files(vec![(restored.clone(), Some(b"new".to_vec()))]);
        assert!(failures.is_empty());
        assert_eq!(fs::read(&restored).unwrap(), b"new".to_vec());

        let blocked = dir.join("blocked");
        fs::write(&blocked, b"not-a-directory").unwrap();
        let failures =
            super::rollback_files(vec![(blocked.join("child.json"), Some(b"x".to_vec()))]);
        assert_eq!(failures.len(), 1, "real rollback failures must be reported");

        let _ = fs::remove_dir_all(dir);
    }

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
        sync_claude_with_target(&config, None).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        assert_eq!(value["env"]["CUSTOM"], "yes");
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert!(!routing_drifted_with_targets(&config, None, None));
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
        let config = AppConfig::default();
        assert_eq!(
            effective_claude_base_url("http://127.0.0.1:8787", "https://api.example.com", &config)
                .as_deref(),
            Some("https://api.example.com")
        );
        assert_eq!(
            effective_claude_base_url(
                "https://direct.example.com",
                "https://ignored.example.com",
                &config
            )
            .as_deref(),
            Some("https://direct.example.com")
        );
        assert_eq!(
            effective_claude_base_url("http://127.0.0.1:8787", "", &config),
            None
        );
    }

    #[test]
    fn bypass_routes_clients_to_agent() {
        let dir =
            std::env::temp_dir().join(format!("headroom-route-bypass-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.codex_config = dir.join("config.toml");
        config.claude_settings = dir.join("settings.json");
        config.bypass_headroom = true;
        fs::write(
            &config.codex_config,
            "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        sync_all_with_targets(&config, None, None).unwrap();
        let codex = fs::read_to_string(&config.codex_config).unwrap();
        let claude: Value =
            serde_json::from_str(&fs::read_to_string(&config.claude_settings).unwrap()).unwrap();
        assert!(codex.contains("http://127.0.0.1:8790/v1"));
        assert_eq!(
            claude
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(Value::as_str),
            Some("http://127.0.0.1:8790")
        );
        assert!(!routing_drifted_with_targets(&config, None, None));
        fs::write(
            &config.codex_config,
            codex.replace("http://127.0.0.1:8790/v1", "http://127.0.0.1:8787/v1"),
        )
        .unwrap();
        assert!(routing_drifted_with_targets(&config, None, None));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_codex_fallback_preserves_cli_authentication() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-direct-codex-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.codex_config = dir.join("config.toml");
        config.state_dir = dir.join("state");
        config.enable_claude = false;
        config.direct_codex = true;
        fs::write(
            &config.codex_config,
            "model_provider = \"upstream\"\n[model_providers.upstream]\nname = \"Upstream\"\nbase_url = \"https://api.example.com/v1\"\nrequires_openai_auth = true\nwire_api = \"responses\"\n[model_providers.upstream.env_http_headers]\nAuthorization = \"SECRET\"\n",
        )
        .unwrap();

        super::sync_codex(&config, Some("https://direct.example.com/v1")).unwrap();
        let text = fs::read_to_string(&config.codex_config).unwrap();
        let doc = text.parse::<DocumentMut>().unwrap();
        assert_eq!(
            doc.get("model_provider").and_then(Item::as_str),
            Some(super::DIRECT_CODEX_PROVIDER)
        );
        assert_eq!(
            doc["model_providers"][super::DIRECT_CODEX_PROVIDER]
                .get("base_url")
                .and_then(Item::as_str),
            Some("https://direct.example.com/v1")
        );
        assert!(
            doc["model_providers"][super::DIRECT_CODEX_PROVIDER]
                .get("env_http_headers")
                .is_none()
        );
        assert!(!super::routing_drifted_with_targets(
            &config,
            Some("https://direct.example.com/v1"),
            None
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_claude_fallback_preserves_cli_authentication() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-direct-claude-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.claude_settings = dir.join("settings.json");
        config.state_dir = dir.join("state");
        config.enable_codex = false;
        config.direct_claude = true;
        fs::write(
            &config.claude_settings,
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"secret","CUSTOM":"yes"}}"#,
        )
        .unwrap();

        sync_claude_with_target(&config, Some("https://anthropic.example.com")).unwrap();
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(&config.claude_settings).unwrap()).unwrap();
        assert_eq!(
            saved["env"]["ANTHROPIC_BASE_URL"],
            "https://anthropic.example.com"
        );
        assert_eq!(saved["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        assert_eq!(saved["env"]["CUSTOM"], "yes");
        super::restore_clients(&config).unwrap();
        let restored: Value =
            serde_json::from_str(&fs::read_to_string(&config.claude_settings).unwrap()).unwrap();
        assert!(restored["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert_eq!(restored["env"]["ANTHROPIC_AUTH_TOKEN"], "secret");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_provider_switch_updates_codex_config_and_auth() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-direct-provider-codex-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.codex_config = dir.join("config.toml");
        config.state_dir = dir.join("state");
        let current = "model = \"model-a\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nname = \"A\"\nbase_url = \"https://a.example.com/v1\"\n[model_providers.headroom_route_direct]\nname = \"old\"\nbase_url = \"https://a.example.com/v1\"\n";
        fs::write(&config.codex_config, current).unwrap();
        let auth_path = dir.join("auth.json");
        fs::write(&auth_path, r#"{"OPENAI_API_KEY":"key-a","other":"keep"}"#).unwrap();
        let source = "model = \"model-b\"\nmodel_provider = \"custom\"\n[model_providers.custom]\nname = \"B\"\nbase_url = \"https://b.example.com/v1\"\nwire_api = \"chat\"\n"
            .parse::<DocumentMut>()
            .unwrap();
        let settings = serde_json::json!({
            "auth": {"OPENAI_API_KEY": "key-b"},
            "config": source.to_string()
        });
        let mut current_doc = current.parse::<DocumentMut>().unwrap();
        let (_, target) =
            super::apply_codex_provider_document(&config, &mut current_doc, &source).unwrap();
        assert_eq!(target, "https://b.example.com/v1");
        let auth = super::pending_codex_auth(&config, &settings)
            .unwrap()
            .unwrap();
        super::commit_files(vec![
            super::PendingFile {
                path: config.codex_config.clone(),
                original: Some(current.as_bytes().to_vec()),
                updated: Some(current_doc.to_string().into_bytes()),
            },
            auth,
        ])
        .unwrap();
        let saved = fs::read_to_string(&config.codex_config).unwrap();
        let saved_doc = saved.parse::<DocumentMut>().unwrap();
        assert_eq!(saved_doc["model_provider"].as_str(), Some("custom"));
        assert_eq!(saved_doc["model"].as_str(), Some("model-b"));
        assert_eq!(
            saved_doc["model_providers"]["custom"]["name"].as_str(),
            Some("B")
        );
        assert!(
            saved_doc["model_providers"]
                .get(super::DIRECT_CODEX_PROVIDER)
                .is_none()
        );
        let auth: Value = serde_json::from_slice(&fs::read(auth_path).unwrap()).unwrap();
        assert_eq!(auth["OPENAI_API_KEY"], "key-b");
        assert_eq!(auth["other"], "keep");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_provider_switch_replaces_claude_credentials() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-direct-provider-claude-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut config = AppConfig::default();
        config.claude_settings = dir.join("settings.json");
        let mut current: Value = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://a.example.com",
                "ANTHROPIC_API_KEY": "key-a",
                "ANTHROPIC_MODEL": "model-a",
                "CUSTOM": "keep"
            }
        });
        fs::write(
            &config.claude_settings,
            serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
        let settings = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://b.example.com",
                "ANTHROPIC_AUTH_TOKEN": "token-b",
                "ANTHROPIC_MODEL": "model-b"
            }
        });
        let target =
            super::apply_claude_provider_settings(&config, &mut current, &settings, None).unwrap();
        assert_eq!(target, "https://b.example.com");
        let env = current["env"].as_object().unwrap();
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").and_then(Value::as_str),
            Some("token-b")
        );
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert_eq!(
            env.get("ANTHROPIC_MODEL").and_then(Value::as_str),
            Some("model-b")
        );
        assert_eq!(env.get("CUSTOM").and_then(Value::as_str), Some("keep"));
        let _ = fs::remove_dir_all(dir);
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
        super::sync_all_with_targets(&config, Some("https://api.example.com/v1"), None).unwrap();
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

    #[test]
    fn legacy_direct_provider_is_parsed_when_upstream_is_real() {
        let text = r#"
model_provider = "headroom_route_direct"
[model_providers.headroom_route_direct]
name = "Legacy direct"
base_url = "https://api.example.com/v1"
"#;
        let (routes, selected) = super::parse_codex_text(text, "legacy").unwrap();
        assert_eq!(selected.as_deref(), Some(super::DIRECT_CODEX_PROVIDER));
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].base_url, "https://api.example.com/v1");
    }

    #[test]
    fn legacy_direct_provider_rejects_headroom_local_address() {
        let dir = std::env::temp_dir().join(format!(
            "headroom-route-legacy-local-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("config.toml");
        fs::write(
            &codex,
            r#"
model_provider = "headroom_route_direct"
[model_providers.headroom_route_direct]
name = "Local loop"
base_url = "http://127.0.0.1:8790/v1"
"#,
        )
        .unwrap();
        let mut config = AppConfig::default();
        config.codex_config = codex;
        config.claude_settings = dir.join("missing-claude.json");
        config.cc_switch_db = dir.join("missing.db");
        config.enable_claude = false;
        let result = super::discover_routes(&config);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
