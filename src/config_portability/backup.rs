use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{self, Value};
use toml_edit::DocumentMut;

use super::{
    AppConfig, BACKUP_FORMAT, BACKUP_FORMAT_VERSION, ConfigFileKind, PendingFile, atomic_write,
    commit_files, read_limited, sha256_hex, unique_id,
};

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
        match super::super::codex_auth_path(config) {
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

pub(super) fn validate_backup_descriptor(descriptor: &BackupDescriptor) -> Result<()> {
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
        ConfigFileKind::CodexAuth => super::super::codex_auth_path(config)
            .ok_or_else(|| anyhow!("无法确定 Codex auth.json 路径")),
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
