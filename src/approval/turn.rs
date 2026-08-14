use super::{WireRequest, connect_pipe};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    thread,
};

const RECENT_PROMPT_LINES: usize = 8;
const MAX_ERROR_CHARS: usize = 300;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TurnResult {
    Completed,
    Failed(String),
}

pub(super) fn cli_input_prompt_ready(cli: &str, contents: &str) -> bool {
    contents
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(RECENT_PROMPT_LINES)
        .any(|line| match cli {
            "codex" => {
                matches!(line, "›" | "❯") || line.starts_with("› ") || line.starts_with("❯ ")
            }
            "claude" => claude_input_prompt_line(line),
            _ => false,
        })
}

fn claude_input_prompt_line(line: &str) -> bool {
    matches!(line, "❯" | ">") || claude_placeholder_prompt(line)
}

fn claude_placeholder_prompt(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("❯ ").or_else(|| line.strip_prefix("> ")) else {
        return false;
    };
    if claude_numbered_menu_line(line) {
        return false;
    }
    let lower = rest.trim().to_ascii_lowercase();
    lower.is_empty() || lower.starts_with("ask claude") || lower.contains("to continue")
}

fn claude_numbered_menu_line(line: &str) -> bool {
    let rest = line.trim_start_matches(['❯', '>', ' ']).trim_start();
    rest.starts_with(|character: char| character.is_ascii_digit()) && rest.contains('.')
}

/// True when `after` gained a non-prompt line that is not just the submitted
/// user text sitting on `❯` / `>`.  Enter echo (`❯ hi` then a fresh `❯`)
/// must not count as a finished reply.
pub(super) fn claude_screen_has_new_reply(before: &str, after: &str) -> bool {
    let previous: HashSet<&str> = before
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    after.lines().map(str::trim).any(|line| {
        !line.is_empty()
            && !previous.contains(line)
            && !claude_input_prompt_line(line)
            && !line.starts_with("❯ ")
            && !line.starts_with("> ")
            && !line.starts_with("› ")
    })
}

/// Codex end-of-turn chrome near the prompt.
///
/// Current Codex (0.14x+) renders English status such as `Worked for 12s` /
/// `N tokens used` rather than the older `• 已完成` bullet. Match both.
pub(super) fn completion_bullet_visible(contents: &str) -> bool {
    contents
        .lines()
        .map(str::trim)
        .any(line_looks_like_codex_completion)
}

fn line_looks_like_codex_completion(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let body = line
        .strip_prefix('•')
        .or_else(|| line.strip_prefix('·'))
        .map(str::trim)
        .unwrap_or(line);
    let lower = body.to_ascii_lowercase();
    body.contains("已完成")
        || lower.contains("worked for")
        || lower.contains("tokens used")
        || lower.eq("done")
        || lower.eq("finished")
        || lower.starts_with("done ")
        || lower.starts_with("finished ")
}

pub(super) fn classify_turn_result(cli: &str, contents: &str) -> TurnResult {
    if cli != "codex" {
        return TurnResult::Completed;
    }

    for line in contents.lines().rev().map(str::trim) {
        if let Some(message) = line.strip_prefix('■') {
            let message = message.trim();
            if !message.is_empty() {
                return TurnResult::Failed(clamp(message, MAX_ERROR_CHARS));
            }
        }
        if line.starts_with('•') {
            break;
        }
    }
    TurnResult::Completed
}

pub(super) fn notify_turn_result(cli: &str, pid: u32, result: TurnResult) {
    let cli = cli.to_owned();
    let _ = thread::Builder::new()
        .name("headroom-turn-notification".into())
        .spawn(move || {
            if let Err(error) = send_turn_result(&cli, pid, &result) {
                eprintln!("HeadroomRoute：无法发送回合通知（{cli} pid={pid}）：{error:#}");
            }
        });
}

pub(super) fn send_turn_result(cli: &str, pid: u32, result: &TurnResult) -> Result<()> {
    let (kind, summary) = match result {
        TurnResult::Completed => ("turn_completed", String::new()),
        TurnResult::Failed(message) => ("turn_failed", message.clone()),
    };
    let payload = WireRequest {
        kind: kind.into(),
        cli: cli.into(),
        pid,
        cwd: String::new(),
        action: String::new(),
        summary,
        allow_rule: false,
        feedback: false,
        source_window: 0,
        focus_known: false,
        focused: false,
        demo: false,
    };
    let mut body = serde_json::to_vec(&payload)?;
    body.push(b'\n');
    let mut stream = connect_pipe()?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    if reader.read_line(&mut response)? == 0 {
        anyhow::bail!("通知服务未返回确认");
    }
    let accepted = serde_json::from_str::<Value>(&response)
        .ok()
        .and_then(|value| {
            value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|reason| reason == kind);
    if !accepted {
        anyhow::bail!("通知服务拒绝请求：{}", response.trim());
    }
    Ok(())
}

/// Handles Codex's legacy `notify` hook. Codex appends the JSON payload as
/// the final argument to the configured command.
#[allow(dead_code)] // Used through the CLI-only notify entry point.
pub(super) fn run_codex_notify(args: &[String]) -> Result<()> {
    let Some(pid) = args.first().and_then(|value| value.parse::<u32>().ok()) else {
        return Ok(());
    };
    let Some(payload) = args.last() else {
        return Ok(());
    };
    if is_codex_completion_payload(payload) {
        send_turn_result("codex", pid, &TurnResult::Completed)?;
    }
    Ok(())
}

/// Claude Code `Stop` / `StopFailure` hooks send JSON on stdin (not argv).
#[allow(dead_code)] // Used through the CLI-only notify entry point.
pub(super) fn run_claude_notify(args: &[String]) -> Result<()> {
    let Some(pid) = args.first().and_then(|value| value.parse::<u32>().ok()) else {
        return Ok(());
    };
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    match claude_hook_turn_result(&stdin) {
        Some(result) => send_turn_result("claude", pid, &result),
        None => Ok(()),
    }
}

pub(super) fn write_claude_stop_hook_settings(executable: &Path, pid: u32) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("headroom-route-claude-stop-{pid}.json"));
    fs::write(&path, claude_stop_hook_settings(executable, pid))
        .with_context(|| format!("写入 Claude Stop 钩子配置失败：{}", path.display()))?;
    Ok(path)
}

pub(super) fn claude_stop_hook_settings(executable: &Path, pid: u32) -> String {
    let command = format!("\"{}\" --claude-notify {pid}", executable.display());
    let hook = serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 8
        }]
    });
    serde_json::json!({
        "hooks": {
            "Stop": [hook.clone()],
            "StopFailure": [hook]
        }
    })
    .to_string()
}

fn claude_hook_turn_result(payload: &str) -> Option<TurnResult> {
    let value = serde_json::from_str::<Value>(payload).ok()?;
    match value.get("hook_event_name").and_then(Value::as_str)? {
        "Stop" => Some(TurnResult::Completed),
        "StopFailure" => Some(TurnResult::Failed(claude_stop_failure_summary(&value))),
        _ => None,
    }
}

fn claude_stop_failure_summary(value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("last_assistant_message").and_then(Value::as_str))
        .map(|text| clamp(text, MAX_ERROR_CHARS))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Claude 本轮异常结束".into())
}

#[allow(dead_code)]
fn is_codex_completion_payload(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|kind| kind == "agent-turn-complete")
}

#[cfg(test)]
mod notify_tests {
    use super::{
        TurnResult, claude_hook_turn_result, claude_stop_hook_settings, is_codex_completion_payload,
    };
    use std::path::Path;

    #[test]
    fn accepts_only_agent_turn_complete_payloads() {
        assert!(is_codex_completion_payload(
            r#"{"type":"agent-turn-complete","thread-id":"t"}"#
        ));
        assert!(!is_codex_completion_payload(r#"{"type":"turn-started"}"#));
        assert!(!is_codex_completion_payload("not json"));
    }

    #[test]
    fn claude_stop_hook_notifies_completion_not_prompt_submit() {
        assert_eq!(
            claude_hook_turn_result(r#"{"hook_event_name":"Stop","stop_hook_active":false}"#),
            Some(TurnResult::Completed)
        );
        assert!(claude_hook_turn_result(r#"{"hook_event_name":"UserPromptSubmit"}"#).is_none());
        assert!(claude_hook_turn_result(r#"{"hook_event_name":"SubagentStop"}"#).is_none());
    }

    #[test]
    fn claude_stop_failure_hook_notifies_failure() {
        assert_eq!(
            claude_hook_turn_result(r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#),
            Some(TurnResult::Failed("rate_limit".into()))
        );
    }

    #[test]
    fn claude_session_settings_point_at_the_cli_notify_entry() {
        let json = claude_stop_hook_settings(Path::new(r"C:\Apps\HeadroomRouteCLI.exe"), 42);
        assert!(json.contains("--claude-notify 42"));
        assert!(json.contains("HeadroomRouteCLI.exe"));
        assert!(json.contains("\"Stop\""));
        assert!(json.contains("\"StopFailure\""));
    }
}

fn clamp(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        TurnResult, classify_turn_result, claude_screen_has_new_reply, cli_input_prompt_ready,
        completion_bullet_visible,
    };

    #[test]
    fn detects_recent_codex_prompt_without_matching_stale_history() {
        assert!(cli_input_prompt_ready(
            "codex",
            "• 已完成\n\n›\n  gpt-5.6-luna low"
        ));
        assert!(!cli_input_prompt_ready(
            "codex",
            &format!("›\n{}\nWorking", "old output\n".repeat(10))
        ));
    }

    #[test]
    fn detects_current_codex_prompt_with_placeholder_text() {
        assert!(cli_input_prompt_ready(
            "codex",
            "• 已完成\n\n› Ask Codex to do anything\n  gpt-5.6 low"
        ));
    }

    #[test]
    fn detects_codex_prompt_when_caret_sits_on_model_status_row() {
        // Mirrors TerminalScreen::prompt_region_text around a caret on the
        // model line: the › glyph is on the previous row, not the cursor line.
        assert!(cli_input_prompt_ready("codex", "›\n  gpt-5.6-luna low"));
        assert!(!cli_input_prompt_ready("codex", "  gpt-5.6-luna low"));
    }

    #[test]
    fn detects_claude_prompt_with_placeholder_text() {
        assert!(cli_input_prompt_ready("claude", "❯"));
        assert!(cli_input_prompt_ready("claude", "❯ Ask Claude to continue"));
        assert!(cli_input_prompt_ready("claude", "> Ask Claude to continue"));
        assert!(!cli_input_prompt_ready("claude", "Ask Claude to continue"));
        assert!(!cli_input_prompt_ready("claude", "❯ 只回复 pong"));
        assert!(!cli_input_prompt_ready(
            "claude",
            "> 1. Yes, I trust this folder"
        ));
        assert!(!cli_input_prompt_ready("claude", "> 1. No, exit"));
    }

    #[test]
    fn submit_echo_is_not_a_claude_reply() {
        assert!(!claude_screen_has_new_reply("❯", "❯ 只回复 pong\n❯"));
        assert!(claude_screen_has_new_reply("❯", "❯ 只回复 pong\npong\n❯"));
    }

    #[test]
    fn detects_codex_completion_bullet_but_not_intermediate_steps() {
        assert!(completion_bullet_visible(
            "• 已完成\n\n›\n  gpt-5.6-luna low"
        ));
        assert!(completion_bullet_visible("• Done\n›"));
        assert!(completion_bullet_visible(
            "hello\nWorked for 3s\n›\n  gpt-5.6 low"
        ));
        assert!(completion_bullet_visible("12 tokens used\n›"));
        assert!(completion_bullet_visible("• Worked for 1m 2s\n›"));
        assert!(!completion_bullet_visible("• 正在运行\n›"));
        assert!(!completion_bullet_visible("›\n  gpt-5.6 low"));
        assert!(!completion_bullet_visible("■ exceeded retry limit\n›"));
        assert!(!completion_bullet_visible("Working…\n›"));
    }

    #[test]
    fn classifies_codex_retry_limit_as_failure() {
        let screen = "› 请问你是什么模型\n\n■ exceeded retry limit, last status: 429 Too Many Requests, request id: test-SIN\n\n›";
        assert_eq!(
            classify_turn_result("codex", screen),
            TurnResult::Failed(
                "exceeded retry limit, last status: 429 Too Many Requests, request id: test-SIN"
                    .into()
            )
        );
    }

    #[test]
    fn latest_success_marker_wins_over_an_older_error() {
        let screen = "■ old request failed\n› retry\n• 这次已经完成\n›";
        assert_eq!(classify_turn_result("codex", screen), TurnResult::Completed);
    }
}
