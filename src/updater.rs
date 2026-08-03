#![cfg(windows)]

use crate::{config, model::AppConfig, progress::ProgressWindow};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    StatusCode,
    blocking::{Client, RequestBuilder, Response},
    header::{RANGE, USER_AGENT},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_YESNO, MessageBoxW,
};
use zip::ZipArchive;

const RELEASE_API: &str = "https://api.github.com/repos/nizzo-dev/HeadroomRoute/releases/latest";
const RELEASE_PAGE: &str = "https://github.com/nizzo-dev/HeadroomRoute/releases/latest";
const DOWNLOAD_ATTEMPTS: usize = 3;
static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    published_at: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug)]
struct UpdateInfo {
    version: String,
    title: String,
    notes: String,
    published_at: String,
    archive: ReleaseAsset,
    checksums: ReleaseAsset,
}

struct PreparedUpdate {
    installer: PathBuf,
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

pub fn start_interactive(owner: usize, config: AppConfig) -> bool {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    thread::spawn(move || {
        if let Err(error) = run_interactive(owner as HWND, &config) {
            show_message(
                owner as HWND,
                "检查更新失败",
                &format!("{error:#}"),
                MB_OK | MB_ICONERROR,
            );
        }
        RUNNING.store(false, Ordering::Release);
    });
    true
}

pub fn check_background(config: &AppConfig) -> Result<Option<String>> {
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return Ok(None);
    }
    let result = (|| {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        if let Some(proxy) = config::reqwest_outbound_proxy(config)? {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().context("无法创建更新请求")?;
        Ok(
            check_for_update(&client, env!("CARGO_PKG_VERSION"))?.map(|update| {
                format!(
                    "发现 HeadroomRoute v{}（{}）；可从“设置与诊断”手动检查并安装",
                    update.version, update.title
                )
            }),
        )
    })();
    RUNNING.store(false, Ordering::Release);
    result
}

fn run_interactive(owner: HWND, config: &AppConfig) -> Result<()> {
    let progress = ProgressWindow::open_with_hint(
        "检查软件更新",
        "正在连接 GitHub Releases",
        "正在安全检查最新正式版，通常只需几秒钟。",
    )?;
    let mut client_builder = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    let mut download_client_builder = Client::builder().connect_timeout(Duration::from_secs(10));
    if let Some(proxy) = config::reqwest_outbound_proxy(config)? {
        client_builder = client_builder.proxy(proxy.clone());
        download_client_builder = download_client_builder.proxy(proxy);
    }
    let client = client_builder.build().context("无法创建更新请求")?;
    let download_client = download_client_builder
        .build()
        .context("无法创建更新下载请求")?;
    let update = check_for_update(&client, env!("CARGO_PKG_VERSION"));
    progress.close();
    let Some(update) = update? else {
        show_message(
            owner,
            "检查软件更新",
            &format!("当前 v{} 已是最新正式版。", env!("CARGO_PKG_VERSION")),
            MB_OK | MB_ICONINFORMATION,
        );
        return Ok(());
    };

    let detail = format!(
        "当前版本：v{}\r\n最新版本：v{}\r\n发布时间：{}\r\n\r\n{}\r\n\r\n更新明细：\r\n{}\r\n\r\n是否下载更新？",
        env!("CARGO_PKG_VERSION"),
        update.version,
        update.published_at,
        update.title,
        update.notes
    );
    if show_message(
        owner,
        "发现软件更新",
        &detail,
        MB_YESNO | MB_ICONINFORMATION,
    ) != IDYES
    {
        return Ok(());
    }

    let progress = ProgressWindow::open_cancelable_with_hint(
        "下载软件更新",
        "正在准备下载",
        "可随时取消；取消不会影响当前版本和已有设置。",
    )?;
    let prepared = download_update(
        &client,
        &download_client,
        &config.state_dir,
        &update,
        &progress,
    );
    progress.close();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let detail = format!(
                "{error:#}\r\n\r\n已保留未完成的下载，下次会自动续传。是否改用浏览器打开官方 Release 页面？"
            );
            if show_message(owner, "下载更新失败", &detail, MB_YESNO | MB_ICONERROR) == IDYES
            {
                let _ = Command::new("explorer.exe").arg(RELEASE_PAGE).spawn();
            }
            return Ok(());
        }
    };
    let Some(prepared) = prepared else {
        show_message(
            owner,
            "下载已取消",
            "更新没有安装，现有程序和设置未发生变化。",
            MB_OK | MB_ICONINFORMATION,
        );
        return Ok(());
    };

    if show_message(
        owner,
        "更新已准备完成",
        "更新包已通过 SHA-256 校验。是否立即重启并更新？\r\n\r\n用户设置将先备份，升级失败会自动恢复旧版本。",
        MB_YESNO | MB_ICONINFORMATION,
    ) != IDYES
    {
        return Ok(());
    }

    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&prepared.installer)
        .arg("-StartNow")
        .arg("-InstallDir")
        .arg(&config.state_dir)
        .arg("-ProcessId")
        .arg(std::process::id().to_string())
        .spawn()
        .context("无法启动外部升级程序")?;
    Ok(())
}

fn check_for_update(client: &Client, current: &str) -> Result<Option<UpdateInfo>> {
    let release: GithubRelease = send_with_retry(
        client
            .get(RELEASE_API)
            .header(USER_AGENT, format!("HeadroomRoute/{current}")),
    )
    .context("无法连接 GitHub")?
    .error_for_status()
    .context("GitHub Releases 返回错误")?
    .json()
    .context("无法解析 GitHub Release")?;
    select_update(release, current)
}

fn select_update(release: GithubRelease, current: &str) -> Result<Option<UpdateInfo>> {
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = release.tag_name.trim_start_matches(['v', 'V']).to_owned();
    if parse_version(&version)? <= parse_version(current)? {
        return Ok(None);
    }
    let archive_name = format!("HeadroomRoute-{version}-windows-x64.zip");
    let checksum_name = format!("HeadroomRoute-{version}-SHA256SUMS.txt");
    let asset = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .cloned()
            .ok_or_else(|| anyhow!("Release 缺少 {name}"))
    };
    Ok(Some(UpdateInfo {
        version,
        title: release
            .name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        notes: release
            .body
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "本版本未提供更新明细。".into()),
        published_at: release
            .published_at
            .unwrap_or_else(|| "未知".into())
            .replace('T', " ")
            .trim_end_matches('Z')
            .to_owned(),
        archive: asset(&archive_name)?,
        checksums: asset(&checksum_name)?,
    }))
}

fn download_update(
    client: &Client,
    download_client: &Client,
    state_dir: &Path,
    update: &UpdateInfo,
    progress: &ProgressWindow,
) -> Result<Option<PreparedUpdate>> {
    let update_dir = state_dir
        .join("updates")
        .join(format!("v{}", update.version));
    let package_dir = update_dir.join("package");
    fs::create_dir_all(&package_dir).context("无法创建更新目录")?;
    let archive_path = update_dir.join(&update.archive.name);
    let partial_path = archive_path.with_extension("zip.part");
    match download_file(download_client, &update.archive, &partial_path, progress) {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_file(&partial_path);
            return Ok(None);
        }
        Err(error) => return Err(error),
    }

    progress.set_indeterminate();
    progress.set_status("正在下载并核对 SHA-256 校验清单");
    if progress.is_cancelled() {
        return Ok(None);
    }
    let checksum_text = send_with_retry(client.get(&update.checksums.browser_download_url).header(
        USER_AGENT,
        format!("HeadroomRoute/{}", env!("CARGO_PKG_VERSION")),
    ))
    .context("无法下载 SHA-256 校验清单")?
    .error_for_status()
    .context("SHA-256 校验清单下载失败")?
    .text()
    .context("无法读取 SHA-256 校验清单")?;
    if progress.is_cancelled() {
        return Ok(None);
    }
    let expected = checksum_for(&checksum_text, &update.archive.name)?;
    let actual = sha256_file(&partial_path)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = fs::remove_file(&partial_path);
        bail!("更新包 SHA-256 不匹配，已拒绝安装");
    }
    fs::write(update_dir.join(&update.checksums.name), checksum_text)
        .context("无法保存 SHA-256 校验清单")?;
    if archive_path.exists() {
        fs::remove_file(&archive_path).context("无法替换旧更新包")?;
    }
    fs::rename(&partial_path, &archive_path).context("无法完成更新包下载")?;

    progress.set_status("校验通过，正在解压更新程序");
    if progress.is_cancelled() {
        return Ok(None);
    }
    let installer = package_dir.join("Install.ps1");
    let executable = package_dir.join(format!("HeadroomRoute-{}.exe", update.version));
    extract_files(
        &archive_path,
        &[
            ("Install.ps1", installer.as_path()),
            (
                &format!("HeadroomRoute-{}.exe", update.version),
                executable.as_path(),
            ),
        ],
    )?;
    if progress.is_cancelled() {
        return Ok(None);
    }
    Ok(Some(PreparedUpdate { installer }))
}

fn download_file(
    client: &Client,
    asset: &ReleaseAsset,
    destination: &Path,
    progress: &ProgressWindow,
) -> Result<bool> {
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        let mut offset = fs::metadata(destination)
            .map(|value| value.len())
            .unwrap_or(0);
        if offset == asset.size {
            progress.set_progress(100);
            return Ok(true);
        }
        if offset > asset.size {
            fs::write(destination, []).context("无法重置无效的更新临时文件")?;
            offset = 0;
        }
        let mut request = client.get(&asset.browser_download_url).header(
            USER_AGENT,
            format!("HeadroomRoute/{}", env!("CARGO_PKG_VERSION")),
        );
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let result = (|| {
            let response = request.send()?.error_for_status()?;
            let resumed = offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
            let start = if resumed { offset } else { 0 };
            let total = response
                .content_length()
                .map(|remaining| start + remaining)
                .unwrap_or(asset.size);
            write_download(
                response,
                destination,
                start,
                total,
                || progress.is_cancelled(),
                |done, total| {
                    let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                    progress.set_progress(percent as u32);
                    progress.set_status(&format!(
                        "正在下载：{percent}%（{} / {}）",
                        format_bytes(done),
                        format_bytes(total)
                    ));
                },
            )
        })();
        match result {
            Ok(done) => return Ok(done),
            Err(_) if attempt < DOWNLOAD_ATTEMPTS => {
                progress.set_status(&format!("连接中断，正在进行第 {} 次重试", attempt + 1));
                thread::sleep(Duration::from_millis(500 * attempt as u64));
            }
            Err(error) => return Err(error).context("无法下载更新包"),
        }
    }
    unreachable!()
}

fn write_download<R, C, P>(
    mut source: R,
    destination: &Path,
    offset: u64,
    total: u64,
    cancelled: C,
    mut report: P,
) -> Result<bool>
where
    R: Read,
    C: Fn() -> bool,
    P: FnMut(u64, u64),
{
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(offset > 0)
            .truncate(offset == 0)
            .open(destination)
            .context("无法创建更新临时文件")?;
        let mut buffer = [0u8; 64 * 1024];
        let mut downloaded = offset;
        loop {
            if cancelled() {
                return Ok(false);
            }
            let count = source.read(&mut buffer).context("读取更新包失败")?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count]).context("写入更新包失败")?;
            downloaded += count as u64;
            report(downloaded, total);
        }
        if downloaded != total {
            bail!("更新包下载不完整：{} / {} 字节", downloaded, total);
        }
        file.sync_all().context("无法保存完整更新包")?;
        Ok(true)
    })();
    if matches!(result, Ok(false)) {
        let _ = fs::remove_file(destination);
    }
    result
}

fn send_with_retry(request: RequestBuilder) -> reqwest::Result<Response> {
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match request
            .try_clone()
            .expect("update requests are cloneable")
            .send()
        {
            Ok(response) => return Ok(response),
            Err(_) if attempt < DOWNLOAD_ATTEMPTS => {
                thread::sleep(Duration::from_millis(500 * attempt as u64));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

fn extract_files(archive_path: &Path, files: &[(&str, &Path)]) -> Result<()> {
    let file = File::open(archive_path).context("无法打开更新包")?;
    let mut archive = ZipArchive::new(file).context("更新包不是有效的 ZIP")?;
    for (name, destination) in files {
        let index = (0..archive.len())
            .find(|index| {
                archive
                    .by_index(*index)
                    .map(|entry| entry.name().replace('\\', "/") == *name)
                    .unwrap_or(false)
            })
            .ok_or_else(|| anyhow!("更新包缺少 {name}"))?;
        let mut source = archive.by_index(index).context("无法读取更新包内容")?;
        let mut output = File::create(destination)
            .with_context(|| format!("无法解压 {}", destination.display()))?;
        std::io::copy(&mut source, &mut output).context("无法解压更新文件")?;
        output.sync_all().context("无法保存更新文件")?;
    }
    Ok(())
}

fn parse_version(value: &str) -> Result<[u64; 3]> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        bail!("不支持的版本号：{value}");
    }
    Ok([
        parts[0].parse().context("主版本号无效")?,
        parts[1].parse().context("次版本号无效")?,
        parts[2].parse().context("修订版本号无效")?,
    ])
}

fn checksum_for(text: &str, file_name: &str) -> Result<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else { continue };
        let Some(name) = fields.next() else { continue };
        if name.trim_start_matches('*') == file_name
            && hash.len() == 64
            && hash.chars().all(|value| value.is_ascii_hexdigit())
        {
            return Ok(hash.to_owned());
        }
    }
    bail!("SHA-256 校验清单中缺少 {file_name}")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("无法读取更新包")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).context("无法校验更新包")?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn format_bytes(value: u64) -> String {
    if value >= 1024 * 1024 {
        format!("{:.1} MB", value as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", value as f64 / 1024.0)
    }
}

fn show_message(owner: HWND, title: &str, text: &str, flags: u32) -> i32 {
    unsafe { MessageBoxW(owner, wide(text).as_ptr(), wide(title).as_ptr(), flags) }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{GithubRelease, checksum_for, parse_version, select_update, write_download};
    use std::{cell::Cell, fs, io::Cursor};

    #[test]
    fn compares_three_part_versions() {
        assert!(parse_version("0.5.0").unwrap() > parse_version("0.4.9").unwrap());
        assert!(parse_version("1.0.0").unwrap() > parse_version("0.99.99").unwrap());
        assert!(parse_version("1.0").is_err());
        assert!(parse_version("1.0.0-beta").is_err());
    }

    #[test]
    fn reads_named_sha256_only() {
        let hash = "a".repeat(64);
        let text = format!("{hash}  HeadroomRoute-0.5.0-windows-x64.zip\n");
        assert_eq!(
            checksum_for(&text, "HeadroomRoute-0.5.0-windows-x64.zip").unwrap(),
            hash
        );
        assert!(checksum_for(&text, "other.zip").is_err());
    }

    fn release(prerelease: bool) -> GithubRelease {
        serde_json::from_value(serde_json::json!({
            "tag_name": "v0.5.0",
            "name": "HeadroomRoute 0.5.0",
            "body": "Changes",
            "published_at": "2026-07-30T00:00:00Z",
            "draft": false,
            "prerelease": prerelease,
            "assets": [
                {"name": "HeadroomRoute-0.5.0-windows-x64.zip", "browser_download_url": "https://example/zip", "size": 100},
                {"name": "HeadroomRoute-0.5.0-SHA256SUMS.txt", "browser_download_url": "https://example/sums", "size": 100}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn selects_newer_stable_release_only() {
        assert!(select_update(release(false), "0.4.0").unwrap().is_some());
        assert!(select_update(release(false), "0.5.0").unwrap().is_none());
        assert!(select_update(release(true), "0.4.0").unwrap().is_none());
    }

    #[test]
    fn cancelled_download_removes_partial_file() {
        let path = std::env::temp_dir().join(format!(
            "headroom-route-download-test-{}.part",
            std::process::id()
        ));
        let reports = Cell::new(0);
        let completed = write_download(
            Cursor::new(vec![1u8; 128 * 1024]),
            &path,
            0,
            128 * 1024,
            || reports.get() > 0,
            |_, _| reports.set(reports.get() + 1),
        )
        .unwrap();
        assert!(!completed);
        assert!(!path.exists());
    }

    #[test]
    fn resumes_partial_download() {
        let path = std::env::temp_dir().join(format!(
            "headroom-route-resume-test-{}.part",
            std::process::id()
        ));
        fs::write(&path, [1u8, 2]).unwrap();
        assert!(write_download(Cursor::new([3u8, 4]), &path, 2, 4, || false, |_, _| {}).unwrap());
        assert_eq!(fs::read(&path).unwrap(), [1, 2, 3, 4]);
        let _ = fs::remove_file(path);
    }
}
