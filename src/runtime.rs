use crate::{config, model::AppConfig};
use anyhow::{Context, Result, anyhow};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
#[cfg(windows)]
use windows_sys::Win32::{
    Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW},
    System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    },
};

const HEADROOM_VERSION: &str = "0.32.1";

pub fn setup_instructions(config: &AppConfig) -> String {
    let configured = config
        .headroom_python
        .as_deref()
        .map_or_else(|| "未配置".into(), |path| path.display().to_string());
    format!(
        "未找到可用的 Headroom 环境（当前 Python：{configured}）。\r\n\r\n请在 PowerShell 运行：\r\npython -m venv \"$env:USERPROFILE\\.headroom\\venv\"\r\n& \"$env:USERPROFILE\\.headroom\\venv\\Scripts\\python.exe\" -m pip install \"headroom-ai[code]=={HEADROOM_VERSION}\"\r\n\r\n完成后从托盘选择“重新检测 Headroom 环境”。"
    )
}

fn managed_python(config: &AppConfig) -> PathBuf {
    config.state_dir.join("runtime/venv/Scripts/python.exe")
}

pub fn find_valid_python(config: &AppConfig) -> Option<PathBuf> {
    config
        .headroom_python
        .as_ref()
        .filter(|path| validate_python(path))
        .cloned()
        .or_else(|| {
            let path = managed_python(config);
            validate_python(&path).then_some(path)
        })
}

pub fn validate_python(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let mut command = Command::new(path);
    let check = format!(
        "import sys,importlib.metadata as m; assert sys.version_info >= (3,10); assert m.version('headroom-ai') == '{HEADROOM_VERSION}'; import headroom.cli"
    );
    hidden(&mut command)
        .args(["-c", &check])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn select_python() -> Result<Option<PathBuf>> {
    let script = r#"Add-Type -AssemblyName System.Windows.Forms; $dialog = New-Object System.Windows.Forms.OpenFileDialog; $dialog.Filter = 'python.exe|python.exe'; $dialog.FileName = 'python.exe'; if ($dialog.ShowDialog() -eq 'OK') { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::Write($dialog.FileName) }"#;
    let mut command = Command::new("powershell.exe");
    let output = hidden(&mut command)
        .args(["-NoProfile", "-STA", "-Command", script])
        .output()
        .context("无法打开 Python 文件选择窗口")?;
    if !output.status.success() {
        return Err(anyhow!("Python 文件选择窗口异常退出"));
    }
    let path = String::from_utf8(output.stdout)
        .context("无法读取所选 Python 路径")?
        .trim_start_matches('\u{feff}')
        .trim()
        .to_owned();
    Ok((!path.is_empty()).then(|| PathBuf::from(path)))
}

pub fn config_with_python(config: &AppConfig, path: PathBuf) -> Result<AppConfig> {
    if !validate_python(&path) {
        return Err(anyhow!(setup_instructions(config)));
    }
    let mut updated = config.clone();
    updated.headroom_python = Some(path);
    Ok(updated)
}

pub fn remove_managed_runtime(config: &AppConfig) -> Result<()> {
    let runtime = config.state_dir.join("runtime");
    if !runtime.join("managed-runtime.json").exists() {
        return Ok(());
    }
    safe_remove_dir(&runtime, &config.state_dir)
}

pub fn uninstall(config: &AppConfig) -> Result<()> {
    config::restore_clients(config)?;
    remove_startup_entry()?;
    remove_managed_runtime(config)?;
    cleanup_state(config)?;
    schedule_self_delete()?;
    Ok(())
}

fn cleanup_state(config: &AppConfig) -> Result<()> {
    if !config.state_dir.exists() {
        return Ok(());
    }
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let state = config
        .state_dir
        .canonicalize()
        .unwrap_or_else(|_| config.state_dir.clone());
    for entry in fs::read_dir(&state)? {
        let path = entry?.path();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if current.as_ref() == Some(&canonical) {
            continue;
        }
        if path.is_dir() {
            safe_remove_dir(&path, &state)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn remove_startup_entry() -> Result<()> {
    let mut key = std::ptr::null_mut();
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Run");
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    } == 0
    {
        unsafe {
            RegDeleteValueW(key, wide("HeadroomRoute").as_ptr());
            RegCloseKey(key);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_startup_entry() -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn schedule_self_delete() -> Result<()> {
    let path = wide(&std::env::current_exe()?.to_string_lossy());
    let status =
        unsafe { MoveFileExW(path.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    if status == 0 {
        return Err(anyhow!("无法安排程序文件删除"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn schedule_self_delete() -> Result<()> {
    Ok(())
}

fn safe_remove_dir(path: &Path, state_dir: &Path) -> Result<()> {
    let state = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !target.starts_with(&state) || target == state {
        return Err(anyhow!(
            "拒绝删除非 HeadroomRoute 目录: {}",
            target.display()
        ));
    }
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("删除失败: {}", path.display()))?;
    }
    Ok(())
}

fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    command
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_python_without_changing_config() {
        let config = AppConfig::default();
        let original = config.headroom_python.clone();
        assert!(config_with_python(&config, PathBuf::from("missing-python.exe")).is_err());
        assert_eq!(config.headroom_python, original);
    }
}
