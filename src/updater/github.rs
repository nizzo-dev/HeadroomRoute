//! Latest-release lookup that survives GitHub being blocked in some networks.
//! Order: GitHub API, then github.com latest page, then public GitHub prefixes.
//! Downloads retry the same list. SHA-256 still comes from the chosen host.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    StatusCode,
    blocking::{Client, RequestBuilder, Response},
    header::{ACCEPT, RANGE, USER_AGENT},
};
use serde::Deserialize;
use std::{thread, time::Duration};

pub const RELEASE_PAGE: &str = "https://github.com/nizzo-dev/HeadroomRoute/releases/latest";
const RELEASE_API: &str = "https://api.github.com/repos/nizzo-dev/HeadroomRoute/releases/latest";
const RELEASE_DOWNLOAD: &str = "https://github.com/nizzo-dev/HeadroomRoute/releases/download";
const API_VERSION: &str = "2022-11-28";
const DOWNLOAD_ATTEMPTS: usize = 3;
/// Prefixes that fetch a full `https://github.com/...` URL when GitHub is blocked.
const GITHUB_MIRRORS: &[&str] = &[
    "https://ghfast.top/",
    "https://gh-proxy.com/",
    "https://mirror.ghproxy.com/",
];

#[derive(Clone, Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub published_at: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
}

pub fn user_agent(version: &str) -> String {
    format!("HeadroomRoute/{version} (+https://github.com/nizzo-dev/HeadroomRoute)")
}

pub fn apply_download_headers(builder: RequestBuilder, version: &str) -> RequestBuilder {
    builder.header(USER_AGENT, user_agent(version))
}

pub fn fetch_latest_release(client: &Client, current: &str) -> Result<GithubRelease> {
    let mut errors = Vec::new();
    for url in url_candidates(RELEASE_API) {
        match fetch_from_api(client, current, &url) {
            Ok(release) => return Ok(release),
            Err(error) => errors.push(error),
        }
    }
    fetch_from_latest_pages(client, current).with_context(|| summarize_api_errors(&errors))
}

pub fn get_text(client: &Client, github_url: &str, version: &str) -> Result<String> {
    send_download(client, github_url, version, None)?
        .text()
        .context("无法读取更新文件")
}

pub fn send_download(
    client: &Client,
    github_url: &str,
    version: &str,
    range_start: Option<u64>,
) -> Result<Response> {
    let mut last_error = None;
    for url in url_candidates(github_url) {
        let mut builder = apply_download_headers(client.get(&url), version);
        if let Some(start) = range_start {
            builder = builder.header(RANGE, format!("bytes={start}-"));
        }
        match send_with_retry(builder) {
            Ok(response) if download_status_ok(response.status(), range_start.is_some()) => {
                return Ok(response);
            }
            Ok(response) => last_error = Some(anyhow!("{}", status_message(response.status()))),
            Err(error) => last_error = Some(error.into()),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的下载地址"))).context(
        "GitHub 与公共镜像均无法下载。请开启系统代理后再试，或用浏览器打开 GitHub Release 页面",
    )
}

pub fn send_with_retry(request: RequestBuilder) -> reqwest::Result<Response> {
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

pub fn tag_from_url(url: &str) -> Option<String> {
    const MARKER: &str = "/releases/tag/";
    let rest = url.split(MARKER).nth(1)?;
    let tag = rest.split(['?', '#', '/']).next()?.trim();
    version_token(tag)
}

pub fn url_candidates(url: &str) -> Vec<String> {
    let mut urls = Vec::with_capacity(GITHUB_MIRRORS.len() + 1);
    urls.push(url.to_owned());
    if is_github_hosted(url) {
        for prefix in GITHUB_MIRRORS {
            urls.push(format!("{prefix}{url}"));
        }
    }
    urls
}

fn fetch_from_api(client: &Client, current: &str, url: &str) -> Result<GithubRelease> {
    let response = send_with_retry(
        apply_download_headers(client.get(url), current)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION),
    )
    .context("无法连接 GitHub API")?;
    ensure_success(&response)?;
    response.json().context("无法解析 GitHub Release")
}

fn fetch_from_latest_pages(client: &Client, current: &str) -> Result<GithubRelease> {
    let mut last_error = None;
    for url in url_candidates(RELEASE_PAGE) {
        match fetch_latest_page(client, current, &url) {
            Ok(release) => return Ok(release),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("无法打开 GitHub 发布页"))).context(
        "GitHub 当前网络不可达，公共镜像也未能获取版本。请检查系统代理，或用浏览器打开 https://github.com/nizzo-dev/HeadroomRoute/releases/latest",
    )
}

fn fetch_latest_page(client: &Client, current: &str, url: &str) -> Result<GithubRelease> {
    let response = send_with_retry(
        client
            .get(url)
            .header(USER_AGENT, user_agent(current))
            .header(ACCEPT, "text/html"),
    )
    .context("无法打开 GitHub 发布页")?;
    ensure_success(&response)?;
    let version = version_from_latest_response(response)?;
    Ok(release_from_tag(&version))
}

fn version_from_latest_response(response: Response) -> Result<String> {
    if let Some(version) = tag_from_url(response.url().as_str()) {
        return Ok(version);
    }
    let final_url = response.url().to_string();
    let html = response.text().context("无法读取 GitHub 发布页")?;
    tag_from_html(&html).ok_or_else(|| anyhow!("无法从 GitHub 发布页解析版本号：{final_url}"))
}

fn tag_from_html(html: &str) -> Option<String> {
    html.split("/releases/tag/")
        .nth(1)
        .and_then(|rest| rest.split(|ch: char| !is_tag_char(ch)).next())
        .and_then(version_token)
}

fn version_token(tag: &str) -> Option<String> {
    let version = tag.trim().trim_start_matches(['v', 'V']);
    if version.split('.').count() == 3 {
        Some(version.to_owned())
    } else {
        None
    }
}

fn is_tag_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')
}

fn release_from_tag(version: &str) -> GithubRelease {
    let archive = crate::edition::release_archive_name(version);
    let checksums = format!("HeadroomRoute-{version}-SHA256SUMS.txt");
    GithubRelease {
        tag_name: format!("v{version}"),
        name: Some(format!("HeadroomRoute {version}")),
        body: Some(
            "GitHub 直连不可用，已从发布页或镜像读取版本；更新明细请查看 GitHub Release。".into(),
        ),
        published_at: None,
        draft: false,
        prerelease: false,
        assets: vec![asset(&archive, version), asset(&checksums, version)],
    }
}

fn asset(name: &str, version: &str) -> ReleaseAsset {
    ReleaseAsset {
        name: name.to_owned(),
        browser_download_url: format!("{RELEASE_DOWNLOAD}/v{version}/{name}"),
        size: u64::MAX,
    }
}

fn is_github_hosted(url: &str) -> bool {
    url.starts_with("https://github.com/")
        || url.starts_with("https://api.github.com/")
        || url.starts_with("https://objects.githubusercontent.com/")
        || url.starts_with("https://release-assets.githubusercontent.com/")
}

fn download_status_ok(status: StatusCode, ranged: bool) -> bool {
    status.is_success() || (ranged && status == StatusCode::PARTIAL_CONTENT)
}

fn ensure_success(response: &Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    bail!("{}", status_message(status));
}

fn status_message(status: StatusCode) -> String {
    match status.as_u16() {
        403 => format!(
            "GitHub 拒绝访问（HTTP 403）。可能是 API 限流或当前网络无法访问 GitHub。可稍后重试，或用浏览器打开 {RELEASE_PAGE}"
        ),
        429 => "GitHub API 请求过于频繁（HTTP 429），请稍后再试".into(),
        _ => format!("GitHub Releases 返回错误：{status}"),
    }
}

fn summarize_api_errors(errors: &[anyhow::Error]) -> String {
    let first = errors
        .first()
        .map(|error| format!("{error}"))
        .unwrap_or_else(|| "GitHub API 不可用".into());
    format!("GitHub API 不可用（{first}），已改用发布页和镜像")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_redirect_tag_urls() {
        assert_eq!(
            tag_from_url("https://github.com/nizzo-dev/HeadroomRoute/releases/tag/v0.9.2"),
            Some("0.9.2".into())
        );
        assert_eq!(
            tag_from_url(
                "https://ghfast.top/https://github.com/nizzo-dev/HeadroomRoute/releases/tag/v0.9.2"
            ),
            Some("0.9.2".into())
        );
        assert_eq!(tag_from_url(RELEASE_PAGE), None);
    }

    #[test]
    fn parses_tag_embedded_in_html() {
        let html = r#"<a href="/nizzo-dev/HeadroomRoute/releases/tag/v0.9.3">0.9.3</a>"#;
        assert_eq!(tag_from_html(html), Some("0.9.3".into()));
    }

    #[test]
    fn github_urls_try_direct_then_mirrors() {
        let urls = url_candidates(RELEASE_PAGE);
        assert_eq!(urls[0], RELEASE_PAGE);
        assert!(
            urls.iter()
                .any(|url| url.starts_with("https://ghfast.top/https://github.com/"))
        );
        assert_eq!(url_candidates("https://example.com/file").len(), 1);
    }

    #[test]
    fn page_fallback_builds_download_assets() {
        let release = release_from_tag("0.9.2");
        assert_eq!(release.tag_name, "v0.9.2");
        let zip = crate::edition::release_archive_name("0.9.2");
        assert!(release.assets.iter().any(|asset| {
            asset.name == zip
                && asset
                    .browser_download_url
                    .contains("/releases/download/v0.9.2/")
        }));
    }
}
