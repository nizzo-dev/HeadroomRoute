use std::{collections::HashMap, path::Path};

const PROJECT_HEADER_NAME: &str = "X-Headroom-Project";
const HEADROOM_PROJECT: &str = "HEADROOM_PROJECT";
const ANTHROPIC_CUSTOM_HEADERS: &str = "ANTHROPIC_CUSTOM_HEADERS";
const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";

/// Percent-encode like CPython `urllib.parse.quote(name, safe="-_.() ")`.
pub(super) fn quote_project_name(name: &str) -> String {
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'(' | b')' | b' ')
        {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub(super) fn project_name_from_cwd(cwd: &Path) -> Option<String> {
    let name = cwd.file_name()?.to_str()?.trim();
    if name.is_empty() {
        return None;
    }
    Some(quote_project_name(name))
}

pub(super) fn apply_headroom_project_env(cli: &str, cwd: &Path, env: &mut HashMap<String, String>) {
    let Some(project) = project_name_from_cwd(cwd) else {
        return;
    };
    if env_key(env, HEADROOM_PROJECT).is_none() {
        env.insert(HEADROOM_PROJECT.into(), project.clone());
    }
    if cli.eq_ignore_ascii_case("claude") {
        apply_claude_project_header(env, &project);
    }
    if cli.eq_ignore_ascii_case("codex") {
        apply_codex_project_url(env, &project);
    }
}

fn apply_claude_project_header(env: &mut HashMap<String, String>, project: &str) {
    let header_line = format!("{PROJECT_HEADER_NAME}: {project}");
    match env_key(env, ANTHROPIC_CUSTOM_HEADERS) {
        Some(key) => {
            let existing = env.get(&key).cloned().unwrap_or_default();
            if existing.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(PROJECT_HEADER_NAME))
            }) {
                return;
            }
            if existing.trim().is_empty() {
                env.insert(key, header_line);
            } else {
                env.insert(key, format!("{existing}\n{header_line}"));
            }
        }
        None => {
            env.insert(ANTHROPIC_CUSTOM_HEADERS.into(), header_line);
        }
    }
}

fn apply_codex_project_url(env: &mut HashMap<String, String>, project: &str) {
    let Some(key) = env_key(env, OPENAI_BASE_URL) else {
        return;
    };
    let Some(current) = env.get(&key).cloned() else {
        return;
    };
    let prefixed = with_local_project_prefix(&current, project);
    if prefixed != current {
        env.insert(key, prefixed);
    }
}

pub(super) fn with_local_project_prefix(base_url: &str, project: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(base_url) else {
        return base_url.to_owned();
    };
    if !matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    ) {
        return base_url.to_owned();
    }
    let path = parsed.path().to_owned();
    if path == "/p" || path.starts_with("/p/") {
        return base_url.to_owned();
    }
    let prefixed = if path == "/" || path.is_empty() {
        format!("/p/{project}")
    } else {
        format!("/p/{project}{path}")
    };
    parsed.set_path(&prefixed);
    let mut rendered = parsed.to_string();
    if base_url.ends_with('/') && !rendered.ends_with('/') {
        rendered.push('/');
    } else if !base_url.ends_with('/') && rendered.ends_with('/') && parsed.path() != "/" {
        rendered.pop();
    }
    rendered
}

pub(super) fn child_unicode_environment(cli: &str, cwd: &Path) -> Vec<u16> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    apply_headroom_project_env(cli, cwd, &mut env);
    unicode_environment_block(&env)
}

pub(super) fn unicode_environment_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut pairs: Vec<(&str, &str)> = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .filter(|(key, value)| !key.is_empty() && !key.contains('\0') && !value.contains('\0'))
        .collect();
    pairs.sort_by(|left, right| {
        left.0
            .to_ascii_uppercase()
            .cmp(&right.0.to_ascii_uppercase())
    });
    let mut block = Vec::new();
    for (key, value) in pairs {
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

fn env_key(env: &HashMap<String, String>, name: &str) -> Option<String> {
    env.keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn quotes_basename_like_headroom_wrap() {
        assert_eq!(
            project_name_from_cwd(&PathBuf::from(r"E:\HeadroomRoute\webview-main-console")),
            Some("webview-main-console".into())
        );
        assert_eq!(
            project_name_from_cwd(&PathBuf::from(r"E:\work\我的 项目")),
            Some("%E6%88%91%E7%9A%84 %E9%A1%B9%E7%9B%AE".into())
        );
        assert_eq!(
            project_name_from_cwd(&PathBuf::from(r"E:\work\foo_(bar)")),
            Some("foo_(bar)".into())
        );
    }

    #[test]
    fn claude_gets_header_and_project_env() {
        let mut env = HashMap::new();
        apply_headroom_project_env("claude", Path::new(r"E:\repos\demo"), &mut env);
        assert_eq!(
            env.get("HEADROOM_PROJECT").map(String::as_str),
            Some("demo")
        );
        assert_eq!(
            env.get("ANTHROPIC_CUSTOM_HEADERS").map(String::as_str),
            Some("X-Headroom-Project: demo")
        );
    }

    #[test]
    fn preserves_user_project_header_and_env() {
        let mut env = HashMap::from([
            ("HEADROOM_PROJECT".into(), "custom".into()),
            (
                "ANTHROPIC_CUSTOM_HEADERS".into(),
                "X-Request-Id: 1\nx-headroom-project: kept".into(),
            ),
        ]);
        apply_headroom_project_env("claude", Path::new(r"E:\repos\demo"), &mut env);
        assert_eq!(env["HEADROOM_PROJECT"], "custom");
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"],
            "X-Request-Id: 1\nx-headroom-project: kept"
        );
    }

    #[test]
    fn appends_project_header_to_existing_custom_headers() {
        let mut env =
            HashMap::from([("ANTHROPIC_CUSTOM_HEADERS".into(), "X-Request-Id: 1".into())]);
        apply_headroom_project_env("CLAUDE", Path::new(r"E:\repos\demo"), &mut env);
        assert_eq!(
            env["ANTHROPIC_CUSTOM_HEADERS"],
            "X-Request-Id: 1\nX-Headroom-Project: demo"
        );
    }

    #[test]
    fn prefixes_local_codex_base_url_only() {
        let mut env =
            HashMap::from([("OPENAI_BASE_URL".into(), "http://127.0.0.1:8787/v1".into())]);
        apply_headroom_project_env("codex", Path::new(r"E:\repos\demo"), &mut env);
        assert_eq!(env["OPENAI_BASE_URL"], "http://127.0.0.1:8787/p/demo/v1");
        assert_eq!(env["HEADROOM_PROJECT"], "demo");
        assert!(!env.contains_key("ANTHROPIC_CUSTOM_HEADERS"));

        let mut remote = HashMap::from([(
            "OPENAI_BASE_URL".into(),
            "https://api.example.com/v1".into(),
        )]);
        apply_headroom_project_env("codex", Path::new(r"E:\repos\demo"), &mut remote);
        assert_eq!(remote["OPENAI_BASE_URL"], "https://api.example.com/v1");
    }

    #[test]
    fn does_not_double_prefix_project_path() {
        assert_eq!(
            with_local_project_prefix("http://127.0.0.1:8787/p/demo/v1", "other"),
            "http://127.0.0.1:8787/p/demo/v1"
        );
    }

    #[test]
    fn unicode_environment_block_is_double_null_terminated() {
        let env = HashMap::from([("A".into(), "1".into()), ("B".into(), "2".into())]);
        let block = unicode_environment_block(&env);
        assert_eq!(*block.last().unwrap(), 0);
        assert_eq!(block[block.len() - 2], 0);
        let text = String::from_utf16_lossy(&block[..block.len() - 1]);
        assert!(text.contains("A=1"));
        assert!(text.contains("B=2"));
    }
}
