//! Boot-time Run key should target the installed exe, not a portable unzip copy.

use anyhow::Result;
use std::path::{Path, PathBuf};

pub const INSTALLED_EXE_NAME: &str = "HeadroomRoute.exe";
const RUN_VALUE_NAME: &str = "HeadroomRoute";

pub fn installed_executable(state_dir: &Path) -> PathBuf {
    state_dir.join(INSTALLED_EXE_NAME)
}

pub fn run_command(executable: &Path) -> String {
    format!(
        "\"{}\" {}",
        executable.display(),
        crate::edition::AUTOSTART_ARG
    )
}

/// Prefer `%LOCALAPPDATA%\HeadroomRoute\HeadroomRoute.exe` when that file exists.
pub fn autostart_executable(state_dir: &Path, current_exe: &Path) -> PathBuf {
    let installed = installed_executable(state_dir);
    if installed.is_file() {
        installed
    } else {
        current_exe.to_path_buf()
    }
}

pub fn sync_from_config(start_with_windows: bool, state_dir: &Path) -> Result<()> {
    if !start_with_windows {
        return Ok(());
    }
    let current = std::env::current_exe()?;
    set_enabled(true, &autostart_executable(state_dir, &current))
}

pub fn set_enabled(enabled: bool, executable: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        registry_set(enabled, executable)
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, executable);
        Ok(())
    }
}

pub fn disable() -> Result<()> {
    set_enabled(false, Path::new(""))
}

#[cfg(windows)]
fn registry_set(enabled: bool, executable: &Path) -> Result<()> {
    use std::ptr;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
        RegSetValueExW,
    };

    unsafe {
        let mut key = ptr::null_mut();
        let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            ptr::null_mut(),
            0,
            KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        ) != 0
        {
            anyhow::bail!("无法打开启动项注册表")
        }
        let name = wide(RUN_VALUE_NAME);
        let result = if enabled {
            let value = wide(&run_command(executable));
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(key, name.as_ptr())
        };
        RegCloseKey(key);
        if result != 0 && enabled {
            anyhow::bail!("注册表写入失败: {result}")
        }
        Ok(())
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn run_executable_from_value(value: &str) -> String {
        let value = value.trim();
        if let Some(rest) = value.strip_prefix('"')
            && let Some(end) = rest.find('"')
        {
            return rest[..end].to_string();
        }
        value.split_whitespace().next().unwrap_or("").to_string()
    }

    fn normalize_path_value(value: &str) -> String {
        run_executable_from_value(value)
            .trim_end_matches(['\\', '/'])
            .to_string()
    }

    fn paths_equal(left: &str, right: &str) -> bool {
        left.eq_ignore_ascii_case(right)
    }

    /// Mirrors `Test-ShouldRewriteAutostart` in Install.ps1.
    fn should_rewrite_run_key(
        existing_run_value: &str,
        installed_exe: &Path,
        install_dir: &Path,
        updating_process_path: &str,
    ) -> bool {
        let existing = normalize_path_value(existing_run_value);
        let installed = normalize_path_value(&installed_exe.to_string_lossy());
        if existing.is_empty() || installed.is_empty() || paths_equal(&existing, &installed) {
            return false;
        }
        let Some(leaf) = Path::new(&existing)
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return false;
        };
        let leaf = leaf.to_ascii_lowercase();
        if !leaf.starts_with("headroomroute") || !leaf.ends_with(".exe") {
            return false;
        }
        let existing_dir = Path::new(&existing)
            .parent()
            .map(|path| normalize_path_value(&path.to_string_lossy()))
            .unwrap_or_default();
        let install = normalize_path_value(&install_dir.to_string_lossy());
        if paths_equal(&existing_dir, &install) {
            return true;
        }
        let updating = normalize_path_value(updating_process_path);
        !updating.is_empty() && paths_equal(&updating, &existing)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "headroom-startup-{label}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn prefers_installed_exe_when_present() {
        let dir = temp_dir("installed");
        let installed = dir.join(INSTALLED_EXE_NAME);
        fs::write(&installed, b"installed").unwrap();
        let portable = dir.join("HeadroomRoute-0.9.0.exe");
        let chosen = autostart_executable(&dir, &portable);
        assert_eq!(chosen, installed);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn falls_back_to_current_exe_before_install() {
        let dir = temp_dir("portable");
        let portable = dir.join("HeadroomRoute-0.9.0.exe");
        fs::write(&portable, b"portable").unwrap();
        let chosen = autostart_executable(&dir, &portable);
        assert_eq!(chosen, portable);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn skips_registry_when_autostart_disabled() {
        let dir = temp_dir("disabled");
        assert!(sync_from_config(false, &dir).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn run_command_passes_autostart_flag() {
        let exe = PathBuf::from(r"C:\HeadroomRoute\HeadroomRoute.exe");
        let command = run_command(&exe);
        assert_eq!(
            command,
            r#""C:\HeadroomRoute\HeadroomRoute.exe" --autostart"#
        );
        assert_eq!(run_executable_from_value(&command), exe.to_string_lossy());
    }

    #[test]
    fn rewrites_run_key_for_portable_updater_not_unrelated_installs() {
        let install_dir = PathBuf::from(r"C:\Users\me\AppData\Local\HeadroomRoute");
        let installed = install_dir.join(INSTALLED_EXE_NAME);
        let portable = PathBuf::from(r"D:\Downloads\HeadroomRoute-0.9.0.exe");
        assert!(should_rewrite_run_key(
            &format!("\"{}\"", portable.display()),
            &installed,
            &install_dir,
            &portable.to_string_lossy(),
        ));
        assert!(!should_rewrite_run_key(
            r#""C:\Users\me\AppData\Local\HeadroomRoute\HeadroomRoute.exe""#,
            Path::new(r"C:\Temp\headroom-test\HeadroomRoute.exe"),
            Path::new(r"C:\Temp\headroom-test"),
            "",
        ));
        assert!(!should_rewrite_run_key(
            "",
            &installed,
            &install_dir,
            &portable.to_string_lossy()
        ));
    }
}
