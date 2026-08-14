use std::{
    fs::File,
    io::{self, Write},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub id: u64,
    pub popup_kind: PopupKind,
    pub cli: String,
    pub pid: u32,
    pub cwd: String,
    pub action: String,
    pub summary: String,
    pub allow_rule: bool,
    pub feedback: bool,
    pub source_window: u64,
    pub focus_known: bool,
    pub focused: bool,
    pub demo: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopupKind {
    Confirmation,
    Success,
    Error,
}

impl ConfirmationPrompt {
    pub(super) fn dedupe_key(&self) -> &str {
        if self.action.ends_with("请求执行一项操作") {
            &self.summary
        } else {
            &self.action
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct WireRequest {
    #[serde(default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) cli: String,
    #[serde(default)]
    pub(super) pid: u32,
    #[serde(default)]
    pub(super) cwd: String,
    #[serde(default)]
    pub(super) action: String,
    #[serde(default)]
    pub(super) summary: String,
    #[serde(default)]
    pub(super) allow_rule: bool,
    #[serde(default)]
    pub(super) feedback: bool,
    #[serde(default)]
    pub(super) source_window: u64,
    #[serde(default)]
    pub(super) focus_known: bool,
    #[serde(default)]
    pub(super) focused: bool,
    #[serde(default)]
    pub(super) demo: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConfirmationPrompt {
    pub(super) action: String,
    pub(super) summary: String,
    pub(super) approve_answer: &'static str,
    pub(super) allow_rule_answer: Option<&'static str>,
    pub(super) feedback_answer: Option<&'static str>,
    pub(super) deny_answer: &'static str,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct WireResponse {
    pub(super) approved: bool,
    pub(super) reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ApprovalDecision {
    Approved,
    ApprovedAlways,
    Feedback,
    Denied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalChoice {
    Deny,
    AllowOnce,
    AllowRule,
    Feedback,
}

pub(super) fn valid_wire_request(request: &WireRequest) -> bool {
    if request.kind == "turn_completed" || request.kind == "turn_failed" {
        return request.pid != 0
            && matches!(request.cli.as_str(), "codex" | "claude")
            && (request.kind == "turn_completed" || !request.summary.trim().is_empty());
    }
    (matches!(
        request.kind.as_str(),
        "cancel_pid" | "focus_update" | "session_register" | "session_close"
    ) && request.pid > 0)
        || (request.kind == "approval_request"
            && !request.cli.trim().is_empty()
            && request.pid > 0
            && !request.action.trim().is_empty()
            && !request.summary.trim().is_empty())
}

pub(super) fn write_response(stream: &mut File, decision: ApprovalDecision) -> io::Result<()> {
    let (approved, reason) = match decision {
        ApprovalDecision::Approved => (true, "approved"),
        ApprovalDecision::ApprovedAlways => (true, "approved_always"),
        ApprovalDecision::Feedback => (false, "feedback"),
        ApprovalDecision::Denied => (false, "denied"),
        ApprovalDecision::Cancelled => (false, "cancelled"),
    };
    write_reason(stream, approved, reason)
}

pub(super) fn write_reason(stream: &mut File, approved: bool, reason: &str) -> io::Result<()> {
    let mut body = serde_json::to_vec(&WireResponse {
        approved,
        reason: reason.into(),
    })
    .map_err(io::Error::other)?;
    body.push(b'\n');
    stream.write_all(&body)?;
    stream.flush()
}

pub(super) fn confirmation_prompt(cli: &str, text: &str) -> Option<ConfirmationPrompt> {
    let cleaned = strip_ansi(text);
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_ascii_lowercase();
    let onboarding_prompt = [
        "accessing workspace",
        "trust this folder",
        "trust this workspace",
        "quick safety check",
        "security guide",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if onboarding_prompt {
        return None;
    }
    let permission_marker = match cli {
        "codex" => [
            "would you like to run",
            "would you like to allow",
            "do you want to run",
            "allow this command",
            "approve this command",
            "approval required",
        ]
        .iter()
        .any(|marker| lower.contains(marker)),
        "claude" => [
            "do you want to proceed",
            "would you like to proceed",
            "needs your permission",
            "needs permission",
            "requires permission",
            "allow once",
            "allow always",
            "yes, allow",
            // Claude Code 2.x network and extension-install permissions.
            "do you want to allow this connection",
            "would you like to install it",
            "permission to run",
        ]
        .iter()
        .any(|marker| lower.contains(marker)),
        _ => false,
    };
    // A dialog without a trailing question mark (for example
    // `WebSearchTool requires permission.` plus options) must still count as a
    // permission prompt when the option list is visible.
    let question_evidence = lower.contains('?')
        || lower.contains("y/n")
        || lower.contains("yes/no")
        || has_numbered_options(&lower);
    let choice_evidence = ["deny", "reject", "cancel", "y/n", "yes/no"]
        .iter()
        .any(|word| lower.contains(word))
        || contains_word(&lower, "yes")
        || contains_word(&lower, "no");
    if !(permission_marker && question_evidence && choice_evidence) {
        return None;
    }
    let summary = prompt_summary(&cleaned);
    let (approve_answer, allow_rule_answer, feedback_answer, deny_answer) =
        confirmation_answers(&summary);
    Some(ConfirmationPrompt {
        action: extract_prompt_action(text, cli),
        summary,
        approve_answer,
        allow_rule_answer,
        feedback_answer,
        deny_answer,
    })
}

fn has_numbered_options(lower: &str) -> bool {
    ["1.", "2.", "3.", "(1)", "(2)", "(3)"]
        .iter()
        .any(|marker| lower.contains(marker))
}

pub(super) fn prompt_summary(text: &str) -> String {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            let countdown = lower.ends_with('s')
                && lower[..lower.len().saturating_sub(1)]
                    .trim()
                    .chars()
                    .all(|character| character.is_ascii_digit());
            !lower.starts_with("working (") && !lower.ends_with("esc to interrupt") && !countdown
        })
        .collect::<Vec<_>>();
    if lines.len() > 14 {
        lines.drain(..lines.len() - 14);
    }
    let mut summary = lines.join(" | ");
    if summary.chars().count() > 420 {
        summary = summary
            .chars()
            .skip(summary.chars().count() - 420)
            .collect();
    }
    summary
}

pub(super) fn extract_prompt_action(text: &str, cli: &str) -> String {
    let cleaned = strip_ansi(text);
    let candidate = cleaned
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            (line.starts_with(['$', '>', '›', '❯'])
                || line.to_ascii_lowercase().starts_with("run "))
                && !lower.contains("yes")
                && !lower.contains("no")
        })
        .map(|line| line.trim_start_matches(['$', '>', '›', '❯']).trim())
        .filter(|line| !line.is_empty());
    clamp_text(
        candidate.unwrap_or(match cli {
            "codex" => "Codex 请求执行一项操作",
            "claude" => "Claude Code 请求执行一项操作",
            _ => "CLI 请求执行一项操作",
        }),
        300,
    )
}

pub(super) fn contains_word(text: &str, word: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| token == word)
}

pub(super) fn confirmation_answers(
    summary: &str,
) -> (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    &'static str,
) {
    let lower = summary.to_ascii_lowercase();
    let numbered = has_numbered_options(&lower);
    if numbered && (lower.contains("yes") || lower.contains("allow")) {
        let has_second = ["2.", "(2)"].iter().any(|marker| lower.contains(marker));
        let has_third = ["3.", "(3)"].iter().any(|marker| lower.contains(marker));
        let allow_rule = (has_second
            && ["always", "don't ask", "do not ask", "again"]
                .iter()
                .any(|word| lower.contains(word)))
        .then_some("2\n");
        let feedback = (has_third
            && lower.contains("tell")
            && ["different", "instead", "feedback"]
                .iter()
                .any(|word| lower.contains(word)))
        .then_some("3\n");
        let deny = if has_third { "3\n" } else { "2\n" };
        ("1\n", allow_rule, feedback, deny)
    } else {
        ("y\n", None, None, "n\n")
    }
}

pub(super) fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escape = false;
    for character in value.chars() {
        if escape {
            if character.is_ascii_alphabetic() || character == '\x07' {
                escape = false;
            }
            continue;
        }
        if character == '\x1b' {
            escape = true;
            continue;
        }
        if character == '\r' {
            output.push('\n');
        } else if !character.is_control() || character == '\n' || character == '\t' {
            output.push(character);
        }
    }
    output
}

pub(super) fn clamp_text(value: &str, max_chars: usize) -> String {
    let mut text = value
        .chars()
        .filter(|character| !matches!(character, '\0' | '\r' | '\n'))
        .collect::<String>();
    if text.chars().count() > max_chars {
        text = text.chars().take(max_chars).collect();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{WireRequest, valid_wire_request};

    fn turn_request(kind: &str, cli: &str, pid: u32, summary: &str) -> WireRequest {
        WireRequest {
            kind: kind.into(),
            cli: cli.into(),
            pid,
            cwd: String::new(),
            action: String::new(),
            summary: summary.into(),
            allow_rule: false,
            feedback: false,
            source_window: 0,
            focus_known: false,
            focused: false,
            demo: false,
        }
    }

    #[test]
    fn accepts_session_end_only_for_supported_cli_with_pid() {
        assert!(valid_wire_request(&turn_request(
            "turn_completed",
            "codex",
            42,
            ""
        )));
        assert!(valid_wire_request(&turn_request(
            "turn_completed",
            "claude",
            42,
            ""
        )));
        assert!(valid_wire_request(&turn_request(
            "turn_failed",
            "codex",
            42,
            "HTTP 429"
        )));
        assert!(!valid_wire_request(&turn_request(
            "turn_failed",
            "codex",
            42,
            ""
        )));
        assert!(!valid_wire_request(&turn_request(
            "turn_completed",
            "other",
            42,
            ""
        )));
        assert!(!valid_wire_request(&turn_request(
            "turn_completed",
            "codex",
            0,
            ""
        )));
    }
}
