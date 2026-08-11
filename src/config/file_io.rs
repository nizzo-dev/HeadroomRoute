use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    fs,
    io::Write,
    os::windows::ffi::OsStrExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

pub(super) fn backup(path: &Path, original: &str) -> Result<()> {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S%3f");
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("config");
    fs::write(
        path.with_file_name(format!("{name}.pre-headroom-route-{stamp}")),
        original,
    )?;
    Ok(())
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.{}.{}.headroom-route.tmp",
        path.extension().and_then(|v| v.to_str()).unwrap_or("tmp"),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
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
            .with_context(|| format!("无法安全替换配置文件: {}", path.display()));
    }

    fs::rename(temp, path).with_context(|| format!("无法替换配置文件: {}", path.display()))
}
