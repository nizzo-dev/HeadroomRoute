use crate::{config, model::AppConfig};
use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
#[cfg(windows)]
use windows_sys::Win32::{
    Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW},
    System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    },
};

const UV_VERSION: &str = "0.12.0";
const UV_SHA256: &str = "68200e25de594df92387186bbfb9d9df606ec1d87efaa0ae0c7f690970e53db6";
const PYTHON_VERSION: &str = "3.12.13";
const HEADROOM_VERSION: &str = "0.32.1";

pub fn managed_python(config: &AppConfig) -> PathBuf {
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

pub fn ensure_runtime(config: &AppConfig, mut progress: impl FnMut(&str)) -> Result<PathBuf> {
    if let Some(path) = find_valid_python(config) {
        return Ok(path);
    }
    let runtime = config.state_dir.join("runtime");
    let staging = config.state_dir.join("runtime.installing");
    if staging.exists() {
        safe_remove_dir(&staging, &config.state_dir)?;
    }
    fs::create_dir_all(staging.join("uv"))?;
    fs::create_dir_all(config.state_dir.join("downloads"))?;
    let uv_zip = config
        .state_dir
        .join(format!("downloads/uv-{UV_VERSION}-windows-x64.zip"));
    let uv_url = format!(
        "https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/uv-x86_64-pc-windows-msvc.zip"
    );
    if !file_has_sha256(&uv_zip, UV_SHA256) {
        progress("正在下载运行环境安装器");
        download(config, &uv_url, &uv_zip)?;
        if !file_has_sha256(&uv_zip, UV_SHA256) {
            return Err(anyhow!("uv 下载文件校验失败"));
        }
    }
    progress("正在解压运行环境安装器");
    extract_zip(&uv_zip, &staging.join("uv"))?;
    let uv = staging.join("uv/uv.exe");
    if !uv.exists() {
        return Err(anyhow!("uv.exe 未在下载包中找到"));
    }
    let log_path = config.state_dir.join("runtime-install.log");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    progress("正在下载独立 Python 3.12");
    run_uv(
        config,
        &uv,
        &["python", "install", PYTHON_VERSION],
        &mut log,
    )?;
    progress("正在创建 Headroom 环境");
    let venv = staging.join("venv");
    run_uv(
        config,
        &uv,
        &[
            "venv",
            venv.to_string_lossy().as_ref(),
            "--python",
            PYTHON_VERSION,
        ],
        &mut log,
    )?;
    let python = venv.join("Scripts/python.exe");
    progress("正在安装 Headroom，首次安装可能需要数分钟");
    run_uv(
        config,
        &uv,
        &[
            "pip",
            "install",
            "--python",
            python.to_string_lossy().as_ref(),
            &format!("headroom-ai[code]=={HEADROOM_VERSION}"),
        ],
        &mut log,
    )?;
    progress("正在验证 Headroom 环境");
    if !validate_python(&python) {
        return Err(anyhow!(
            "Headroom 环境验证失败，请查看 {}",
            log_path.display()
        ));
    }
    fs::write(
        staging.join("managed-runtime.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"managed_by":"HeadroomRoute","python":PYTHON_VERSION,"headroom":HEADROOM_VERSION,"uv":UV_VERSION}),
        )?,
    )?;
    if runtime.exists() {
        safe_remove_dir(&runtime, &config.state_dir)?;
    }
    fs::rename(&staging, &runtime)?;
    relocate_venv(&runtime.join("venv/pyvenv.cfg"), &staging, &runtime)?;
    let final_python = managed_python(config);
    if !validate_python(&final_python) {
        return Err(anyhow!("安装完成后的 Headroom 环境无法运行"));
    }
    Ok(final_python)
}

fn relocate_venv(config_path: &Path, old_root: &Path, new_root: &Path) -> Result<()> {
    if !config_path.exists() {
        return Err(anyhow!("虚拟环境缺少 pyvenv.cfg"));
    }
    let original = fs::read_to_string(config_path)?;
    let updated = original.replace(
        old_root.to_string_lossy().as_ref(),
        new_root.to_string_lossy().as_ref(),
    );
    fs::write(config_path, updated)?;
    Ok(())
}

pub fn remove_managed_runtime(config: &AppConfig) -> Result<()> {
    let runtime = config.state_dir.join("runtime");
    if !runtime.join("managed-runtime.json").exists() {
        return Ok(());
    }
    safe_remove_dir(&runtime, &config.state_dir)
}

pub fn repair_runtime(config: &AppConfig, progress: impl FnMut(&str)) -> Result<PathBuf> {
    remove_managed_runtime(config)?;
    let staging = config.state_dir.join("runtime.installing");
    if staging.exists() {
        safe_remove_dir(&staging, &config.state_dir)?;
    }
    let mut repaired = config.clone();
    repaired.headroom_python = None;
    ensure_runtime(&repaired, progress)
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

fn download(config: &AppConfig, url: &str, path: &Path) -> Result<()> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(None)
        .user_agent("HeadroomRoute/0.3 runtime-bootstrap");
    if let Some(proxy) = config::reqwest_outbound_proxy(config)? {
        builder = builder.proxy(proxy);
    }
    let mut response = builder.build()?.get(url).send()?.error_for_status()?;
    let temp = path.with_extension("download");
    let mut file = fs::File::create(&temp)?;
    std::io::copy(&mut response, &mut file)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)?;
    Ok(())
}

fn extract_zip(path: &Path, target: &Path) -> Result<()> {
    let file = fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let output = target.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(output)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

fn run_uv(config: &AppConfig, uv: &Path, args: &[&str], log: &mut fs::File) -> Result<()> {
    writeln!(log, "\n> uv {}", args.join(" "))?;
    let stdout = log.try_clone()?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(uv);
    command
        .args(args)
        .env(
            "UV_PYTHON_INSTALL_DIR",
            config.state_dir.join("runtime.installing/python"),
        )
        .env("UV_CACHE_DIR", config.state_dir.join("cache/uv"))
        .env("UV_NO_PROGRESS", "1")
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    if let Some(proxy) = config::outbound_proxy_url(config) {
        command.env("HTTPS_PROXY", &proxy).env("HTTP_PROXY", proxy);
    }
    let status = hidden(&mut command).status()?;
    if !status.success() {
        return Err(anyhow!(
            "运行环境安装步骤失败：uv {}，请查看 {}",
            args.join(" "),
            config.state_dir.join("runtime-install.log").display()
        ));
    }
    Ok(())
}

fn file_has_sha256(path: &Path, expected: &str) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => hasher.update(&buffer[..count]),
            Err(_) => return false,
        }
    }
    format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(expected)
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
    use super::relocate_venv;
    use std::fs;

    #[test]
    fn relocates_windows_venv_home_after_atomic_install() {
        let dir =
            std::env::temp_dir().join(format!("headroom-runtime-relocate-{}", std::process::id()));
        let old = dir.join("runtime.installing");
        let new = dir.join("runtime");
        fs::create_dir_all(&new).unwrap();
        let cfg = new.join("pyvenv.cfg");
        fs::write(&cfg, format!("home = {}\\python\\cpython\n", old.display())).unwrap();
        relocate_venv(&cfg, &old, &new).unwrap();
        let text = fs::read_to_string(&cfg).unwrap();
        assert!(text.contains(new.to_string_lossy().as_ref()));
        assert!(!text.contains(old.to_string_lossy().as_ref()));
        let _ = fs::remove_dir_all(dir);
    }
}
