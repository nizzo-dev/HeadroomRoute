use super::{WireRequest, connect_pipe};
use anyhow::Result;
use std::{io::Write, thread};

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
            "claude" => matches!(line, "❯" | ">"),
            _ => false,
        })
}

/// Codex end-of-turn chrome near the prompt.
///
/// Current Codex (0.14x+) renders English status such as `Worked for 12s` /
/// `N tokens used` rather than the older `• 已完成` bullet. Match both.
pub(super) fn completion_bullet_visible(contents: &str) -> bool {
    contents.lines().map(str::trim).any(line_looks_like_codex_completion)
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
                eprintln!(
                    "HeadroomRoute：无法发送回合通知（{cli} pid={pid}）：{error:#}"
                );
            }
        });
}

fn send_turn_result(cli: &str, pid: u32, result: &TurnResult) -> Result<()> {
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
    Ok(())
}

fn clamp(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        TurnResult, classify_turn_result, cli_input_prompt_ready, completion_bullet_visible,
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
        assert!(cli_input_prompt_ready(
            "codex",
            "›\n  gpt-5.6-luna low"
        ));
        assert!(!cli_input_prompt_ready("codex", "  gpt-5.6-luna low"));
    }

    #[test]
    fn detects_codex_completion_bullet_but_not_intermediate_steps() {
        assert!(completion_bullet_visible("• 已完成\n\n›\n  gpt-5.6-luna low"));
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
