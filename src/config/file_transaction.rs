use anyhow::{Context, Result, anyhow};
use std::{fs, path::PathBuf};

use super::{atomic_write, backup};

pub(super) struct PendingFile {
    pub(super) path: PathBuf,
    pub(super) original: Option<Vec<u8>>,
    /// `None` removes the file instead of writing it.
    pub(super) updated: Option<Vec<u8>>,
}

/// Best-effort transaction rollback. Returns paths whose rollback itself failed.
pub(crate) fn rollback_files(committed: Vec<(PathBuf, Option<Vec<u8>>)>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, original) in committed.into_iter().rev() {
        let result = match original {
            Some(bytes) => atomic_write(&path, &bytes),
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(anyhow::Error::from(error)),
            },
        };
        if let Err(error) = result {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    failures
}

pub(super) fn commit_files(updates: Vec<PendingFile>) -> Result<()> {
    let mut committed: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for update in updates {
        let remove = update.updated.is_none();
        let unchanged = match (&update.original, &update.updated) {
            (Some(original), Some(updated)) => original == updated,
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            continue;
        }

        let outcome = if remove {
            let Some(original) = update.original.as_ref() else {
                continue;
            };
            match backup(&update.path, &String::from_utf8_lossy(original)) {
                Ok(()) => fs::remove_file(&update.path)
                    .with_context(|| format!("无法删除文件: {}", update.path.display())),
                Err(error) => Err(error),
            }
        } else {
            let updated = update.updated.as_deref().unwrap_or_default();
            match update.original.as_ref() {
                Some(original) => match backup(&update.path, &String::from_utf8_lossy(original)) {
                    Ok(()) => atomic_write(&update.path, updated),
                    Err(error) => Err(error),
                },
                None => atomic_write(&update.path, updated),
            }
        };

        match outcome {
            Ok(()) => committed.push((update.path, update.original)),
            Err(error) => {
                let rollback_failures = rollback_files(committed);
                return if rollback_failures.is_empty() {
                    Err(error)
                } else {
                    Err(anyhow!(
                        "{error}；事务回滚也未完全成功: {}",
                        rollback_failures.join("；")
                    ))
                };
            }
        }
    }
    Ok(())
}
