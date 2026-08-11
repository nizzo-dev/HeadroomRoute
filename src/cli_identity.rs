use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

pub const CLI_PROTOCOL_VERSION: u32 = 1;
pub const CLI_VERSION_LINE_PREFIX: &str = "HeadroomRouteCLI";

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CliCompatibility {
    pub path: Option<String>,
    pub expected_version: String,
    pub detected_version: Option<String>,
    pub detected_protocol: Option<u32>,
    pub compatible: bool,
    pub reason: String,
}

impl CliCompatibility {
    pub fn inspect_cached(state_dir: &Path) -> Self {
        static CACHE: OnceLock<Mutex<HashMap<PathBuf, CliCompatibility>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(result) = cache.lock().unwrap().get(state_dir).cloned() {
            return result;
        }
        let result = Self::inspect(state_dir);
        cache
            .lock()
            .unwrap()
            .insert(state_dir.to_path_buf(), result.clone());
        result
    }

    pub fn inspect(state_dir: &Path) -> Self {
        let path = state_dir.join("HeadroomRouteCLI.exe");
        let expected_version = env!("CARGO_PKG_VERSION").to_owned();
        let Some(path_text) = path.to_str().map(str::to_owned) else {
            return Self::missing(
                expected_version,
                None,
                "CLI 路径不是有效的 Windows 路径".into(),
            );
        };
        if !path.is_file() {
            return Self::missing(
                expected_version,
                Some(path_text),
                "未找到已安装的 CLI wrapper".into(),
            );
        }

        let output = Command::new(&path)
            .arg("--version")
            .creation_flags(0x08000000)
            .output();
        let Ok(output) = output else {
            return Self::missing(
                expected_version,
                Some(path_text),
                "无法启动已安装的 CLI wrapper".into(),
            );
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let text = if text.trim().is_empty() {
            String::from_utf8_lossy(&output.stderr)
        } else {
            text
        };
        let (detected_version, detected_protocol) = parse_identity(&text);
        let compatible = identity_compatible(
            output.status.success(),
            &expected_version,
            detected_version.as_deref(),
            detected_protocol,
        );
        let reason = if compatible {
            "CLI wrapper 与托盘协议匹配".into()
        } else {
            "CLI wrapper 与当前托盘版本或通知协议不匹配".into()
        };
        Self {
            path: Some(path_text),
            expected_version,
            detected_version,
            detected_protocol,
            compatible,
            reason,
        }
    }

    fn missing(expected_version: String, path: Option<String>, reason: String) -> Self {
        Self {
            path,
            expected_version,
            detected_version: None,
            detected_protocol: None,
            compatible: false,
            reason,
        }
    }
}

fn parse_identity(text: &str) -> (Option<String>, Option<u32>) {
    let line = text
        .lines()
        .find(|line| line.contains(CLI_VERSION_LINE_PREFIX));
    let Some(line) = line else {
        return (None, None);
    };
    let mut fields = line.split_whitespace();
    let _ = fields.next();
    let version = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let protocol = fields
        .find_map(|field| field.strip_prefix("notification-protocol="))
        .and_then(|value| value.parse().ok());
    (version, protocol)
}

fn identity_compatible(
    status_success: bool,
    expected_version: &str,
    detected_version: Option<&str>,
    detected_protocol: Option<u32>,
) -> bool {
    status_success
        && detected_version == Some(expected_version)
        && detected_protocol == Some(CLI_PROTOCOL_VERSION)
}

trait CommandCreationFlags {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
}

#[cfg(windows)]
impl CommandCreationFlags for Command {
    fn creation_flags(&mut self, flags: u32) -> &mut Self {
        use std::os::windows::process::CommandExt;
        CommandExt::creation_flags(self, flags)
    }
}

#[cfg(not(windows))]
impl CommandCreationFlags for Command {
    fn creation_flags(&mut self, _flags: u32) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCompatibility, identity_compatible, parse_identity};

    #[test]
    fn parses_cli_identity() {
        assert_eq!(
            parse_identity("HeadroomRouteCLI 0.8.6 notification-protocol=1\n"),
            (Some("0.8.6".into()), Some(1))
        );
    }

    #[test]
    fn rejects_missing_identity() {
        assert_eq!(parse_identity("HeadroomRoute CLI\n"), (None, None));
    }

    #[test]
    fn rejects_same_version_without_notification_protocol() {
        assert!(!identity_compatible(true, "0.8.6", Some("0.8.6"), None));
        assert!(!identity_compatible(true, "0.8.6", Some("0.8.5"), Some(1)));
    }

    #[test]
    fn reports_missing_installed_cli() {
        let state_dir =
            std::env::temp_dir().join(format!("headroom-route-missing-cli-{}", std::process::id()));
        let result = CliCompatibility::inspect(&state_dir);
        assert!(!result.compatible);
        assert!(result.detected_version.is_none());
        assert!(result.reason.contains("未找到"));
    }
}
