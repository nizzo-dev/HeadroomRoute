use std::{
    io::{Cursor, Write},
    path::Path,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use zip::{ZipWriter, write::SimpleFileOptions};

use super::{AppConfig, atomic_write, read_limited, redact_sensitive_text, redacted_json};

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
