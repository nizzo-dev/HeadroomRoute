use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

use super::{OperationRecord, UndoTicket};

#[derive(Serialize, Deserialize)]
pub(super) struct HistoryFile {
    #[serde(default)]
    pub(super) schema_version: u32,
    #[serde(default)]
    pub(super) next_seq: u64,
    #[serde(default)]
    pub(super) entries: Vec<OperationRecord>,
    #[serde(default)]
    pub(super) undo_tickets: Vec<UndoTicket>,
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.{}.{}.headroom-route.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("tmp"),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn replace_file(temp: &Path, path: &Path) -> Result<()> {
    #[cfg(windows)]
    if path.exists() {
        let temp_wide: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let replaced = unsafe {
            ReplaceFileW(
                path_wide.as_ptr(),
                temp_wide.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced != 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to replace operation history: {}", path.display()));
    }
    fs::rename(temp, path)
        .with_context(|| format!("failed to replace operation history: {}", path.display()))
}

pub(super) fn quarantine_corrupt(path: &Path) -> Option<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = path.file_name()?.to_str()?;
    let quarantined = path.with_file_name(format!("{name}.corrupt-{stamp}"));
    fs::rename(path, &quarantined).ok().map(|_| quarantined)
}
