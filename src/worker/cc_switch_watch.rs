use crate::{
    model::AppConfig,
    sqlite::{self, ProviderRow},
};
use serde_json::{Map, Value};

pub(super) type ProviderApiSnapshot = (Vec<String>, Vec<String>);

pub(super) fn cc_switch_provider_snapshot(
    config: &AppConfig,
) -> anyhow::Result<ProviderApiSnapshot> {
    Ok((
        fingerprint_list(if config.enable_codex {
            sqlite::providers(&config.cc_switch_db, "codex")?
        } else {
            Vec::new()
        }),
        fingerprint_list(if config.enable_claude {
            sqlite::providers(&config.cc_switch_db, "claude")?
        } else {
            Vec::new()
        }),
    ))
}

pub(super) fn provider_snapshot_changed(
    previous: &mut Option<ProviderApiSnapshot>,
    current: ProviderApiSnapshot,
) -> bool {
    let changed = previous
        .as_ref()
        .is_some_and(|previous| previous != &current);
    *previous = Some(current);
    changed
}

fn fingerprint_list(rows: Vec<ProviderRow>) -> Vec<String> {
    let mut items: Vec<String> = rows.iter().map(provider_api_fingerprint).collect();
    items.sort();
    items
}

fn provider_api_fingerprint(row: &ProviderRow) -> String {
    let api = serde_json::from_str::<Value>(&row.settings)
        .ok()
        .map(|value| api_relevant_settings(&value))
        .unwrap_or(Value::Null);
    serde_json::to_string(&(
        row.id.as_str(),
        row.name.as_str(),
        row.website_url.as_str(),
        api,
    ))
    .unwrap_or_default()
}

fn api_relevant_settings(settings: &Value) -> Value {
    let Some(obj) = settings.as_object() else {
        return settings.clone();
    };
    let mut out = Map::new();
    for key in ["auth", "config", "env", "model"] {
        if let Some(value) = obj.get(key) {
            out.insert(
                key.to_owned(),
                if key == "env" {
                    api_relevant_env(value)
                } else {
                    value.clone()
                },
            );
        }
    }
    Value::Object(out)
}

fn api_relevant_env(value: &Value) -> Value {
    let Some(env) = value.as_object() else {
        return value.clone();
    };
    env.iter()
        .filter(|(key, _)| is_api_env_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn is_api_env_key(key: &str) -> bool {
    key.starts_with("ANTHROPIC_") || key.starts_with("CLAUDE_CODE_") || key.starts_with("OPENAI_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, settings: &str) -> ProviderRow {
        ProviderRow {
            id: "provider".into(),
            name: name.into(),
            settings: settings.into(),
            website_url: String::new(),
        }
    }

    fn snapshot(rows: Vec<ProviderRow>) -> ProviderApiSnapshot {
        (fingerprint_list(rows), Vec::new())
    }

    #[test]
    fn only_reports_api_relevant_provider_changes() {
        let mut previous = None;
        let before = snapshot(vec![row(
            "Before",
            r#"{"config":"model_provider=\"a\"","updatedAt":1}"#,
        )]);
        assert!(!provider_snapshot_changed(&mut previous, before.clone()));
        assert!(!provider_snapshot_changed(&mut previous, before));
        assert!(provider_snapshot_changed(
            &mut previous,
            snapshot(vec![row(
                "After",
                r#"{"config":"model_provider=\"a\"","updatedAt":1}"#,
            )])
        ));
    }

    #[test]
    fn ignores_metadata_reorder_and_non_api_env() {
        let mut previous = None;
        let first = snapshot(vec![
            ProviderRow {
                id: "b".into(),
                name: "B".into(),
                settings:
                    r#"{"config":"x","icon":"old","env":{"ANTHROPIC_API_KEY":"k","FOO":"1"}}"#
                        .into(),
                website_url: String::new(),
            },
            ProviderRow {
                id: "a".into(),
                name: "A".into(),
                settings: r#"{"config":"x","updatedAt":9}"#.into(),
                website_url: String::new(),
            },
        ]);
        assert!(!provider_snapshot_changed(&mut previous, first));
        let shuffled = snapshot(vec![
            ProviderRow {
                id: "a".into(),
                name: "A".into(),
                settings: r#"{"updatedAt":10,"config":"x"}"#.into(),
                website_url: String::new(),
            },
            ProviderRow {
                id: "b".into(),
                name: "B".into(),
                settings:
                    r#"{"icon":"new","config":"x","env":{"FOO":"2","ANTHROPIC_API_KEY":"k"}}"#
                        .into(),
                website_url: String::new(),
            },
        ]);
        assert!(!provider_snapshot_changed(&mut previous, shuffled));
        assert!(provider_snapshot_changed(
            &mut previous,
            snapshot(vec![row(
                "B",
                r#"{"config":"x","env":{"ANTHROPIC_API_KEY":"changed"}}"#,
            )])
        ));
    }
}
