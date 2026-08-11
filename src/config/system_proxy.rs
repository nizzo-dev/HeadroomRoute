use crate::model::AppConfig;
use anyhow::Result;

#[cfg(windows)]
use super::wide;
#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_DWORD, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};

pub fn outbound_proxy_url(config: &AppConfig) -> Option<String> {
    if !config.use_system_proxy {
        return None;
    }
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(value) = std::env::var(name)
            && let Some(url) = parse_proxy_server(&value)
        {
            return Some(url);
        }
    }
    windows_proxy_server().and_then(|value| parse_proxy_server(&value))
}

pub fn reqwest_outbound_proxy(config: &AppConfig) -> Result<Option<reqwest::Proxy>> {
    let Some(proxy_url) = outbound_proxy_url(config) else {
        return Ok(None);
    };
    let target = url::Url::parse(&proxy_url)?;
    Ok(Some(reqwest::Proxy::custom(move |request| {
        if matches!(request.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
            None
        } else {
            Some(target.clone())
        }
    })))
}

pub(super) fn parse_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let endpoint = if value.contains('=') {
        let mut https = None;
        let mut http = None;
        for part in value.split(';') {
            let Some((kind, address)) = part.split_once('=') else {
                continue;
            };
            match kind.trim().to_ascii_lowercase().as_str() {
                "https" => https = Some(address.trim()),
                "http" => http = Some(address.trim()),
                _ => {}
            }
        }
        https.or(http)?
    } else {
        value
    };
    let candidate = if endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("http://{endpoint}")
    };
    let url = url::Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(candidate.trim_end_matches('/').to_owned())
}

#[cfg(windows)]
fn windows_proxy_server() -> Option<String> {
    let mut key = std::ptr::null_mut();
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    } != 0
    {
        return None;
    }
    let enabled = query_registry_dword(key, "ProxyEnable") == Some(1);
    let server = enabled
        .then(|| query_registry_string(key, "ProxyServer"))
        .flatten();
    unsafe {
        RegCloseKey(key);
    }
    server
}

#[cfg(not(windows))]
fn windows_proxy_server() -> Option<String> {
    None
}

#[cfg(windows)]
fn query_registry_dword(
    key: windows_sys::Win32::System::Registry::HKEY,
    name: &str,
) -> Option<u32> {
    let name = wide(name);
    let mut value = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    let mut kind = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            &mut value as *mut u32 as *mut u8,
            &mut bytes,
        )
    };
    (status == 0 && kind == REG_DWORD).then_some(value)
}

#[cfg(windows)]
fn query_registry_string(
    key: windows_sys::Win32::System::Registry::HKEY,
    name: &str,
) -> Option<String> {
    let name = wide(name);
    let mut bytes = 0u32;
    let mut kind = 0u32;
    if unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        )
    } != 0
        || kind != REG_SZ
        || bytes < 2
    {
        return None;
    }
    let mut value = vec![0u16; bytes as usize / 2];
    if unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            value.as_mut_ptr() as *mut u8,
            &mut bytes,
        )
    } != 0
    {
        return None;
    }
    Some(
        String::from_utf16_lossy(&value)
            .trim_end_matches('\0')
            .to_owned(),
    )
}
