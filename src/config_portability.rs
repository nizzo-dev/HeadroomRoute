#[path = "config_portability/diagnostic.rs"]
mod diagnostic;
#[path = "config_portability/portable.rs"]
mod portable;
#[path = "config_portability/redaction.rs"]
mod redaction;
pub use diagnostic::{DiagnosticBundleDescriptor, create_diagnostic_bundle};
#[path = "config_portability/takeover.rs"]
mod takeover;
pub use portable::{decode_portable_config, export_portable_config, import_portable_config};
#[cfg(test)]
use redaction::is_secret_key;
pub(crate) use redaction::redact_sensitive_text;
use redaction::{redacted_json, redacted_value};
pub use takeover::{
    FieldChangePreview, FileChangePreview, TakeoverPlan, TakeoverPreview, apply_takeover_plan,
    prepare_takeover,
};

use super::{PendingFile, atomic_write, commit_files};
use crate::{
    model::{AppConfig, FailoverPolicy},
    routing_policy::RoutingStrategyConfig,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    fs,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn contains_obvious_secret(value: &str) -> bool {
    portable::contains_obvious_secret(value)
}

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

#[path = "config_portability/backup.rs"]
mod backup;
#[cfg(test)]
use backup::validate_backup_descriptor;
pub use backup::{
    BackupDescriptor, BackupFileDescriptor, create_config_backup, list_config_backups,
    restore_config_backup,
};
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
            manage_upstream: false,
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
        let mut config = fixture_config(&dir);
        config.manage_codex = true;
        config.manage_claude = true;
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
