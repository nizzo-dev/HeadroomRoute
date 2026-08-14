use super::super::{
    classify_turn_result, claude_screen_has_new_reply, cli_input_prompt_ready,
    completion_bullet_visible, confirmation_prompt, notify_turn_result, request_approval,
};
use super::io::{normalize_console_size, parent_console_size, write_cli_output};
use super::sink::InputSink;
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::Read,
    sync::{Arc, atomic::Ordering},
    thread,
};
use windows_sys::Win32::System::Console::COORD;

pub(super) fn read_cli_output(
    mut output: File,
    input: Arc<InputSink>,
    cli: String,
    pid: u32,
    cwd: String,
    turn_notify_hook: bool,
) -> Result<()> {
    let mut buffer = [0u8; 4096];
    let mut terminal = TerminalScreen::new(parent_console_size());
    let mut turn_screen_baseline: Option<String> = None;
    loop {
        terminal.resize_if_needed();
        let read = output.read(&mut buffer).context("读取 CLI 输出失败")?;
        if read == 0 {
            break;
        }
        let pending_before_output = input.turn_pending.load(Ordering::Acquire);
        if pending_before_output && turn_screen_baseline.is_none() {
            turn_screen_baseline = Some(terminal.contents());
        }
        write_cli_output(&buffer[..read])?;
        terminal.process(&buffer[..read]);
        let screen = terminal.contents();
        // Codex keeps the caret on the model/status row under the › prompt, so
        // completion detection must look at the cursor neighborhood—not only the
        // single cursor line—otherwise idle turns never fire turn_completed.
        let prompt_region = terminal.prompt_region_text();
        let prompt_ready = cli_input_prompt_ready(&cli, &prompt_region);
        let output_activity = cli == "codex"
            && turn_screen_baseline
                .as_deref()
                .is_some_and(|baseline| codex_response_added(baseline, &screen));
        if output_activity {
            input.turn_activity_seen.store(true, Ordering::Release);
        }
        if pending_before_output {
            let (prompt_left, prompt_returned) =
                advance_prompt_cycle(input.turn_prompt_left.load(Ordering::Acquire), prompt_ready);
            if prompt_left {
                input.turn_prompt_left.store(true, Ordering::Release);
            }
            if prompt_returned {
                input.turn_prompt_returned.store(true, Ordering::Release);
            }
        }
        let visible_confirmation = confirmation_prompt(&cli, &screen);
        // End-of-turn chrome ("Worked for", "tokens used") often sits a few rows
        // above the caret; scan the bottom of the screen as well as the region.
        let completion_scan = terminal.completion_scan_text();
        if !turn_notify_hook
            && input.turn_pending.load(Ordering::Acquire)
            && visible_confirmation.is_none()
        {
            let completion_bullet = cli == "codex" && completion_bullet_visible(&completion_scan);
            // Arm bullet fallback only after the stale completion marker leaves
            // (or after non-prompt activity). Avoids firing on the previous turn's •.
            if !completion_bullet && !input.turn_completion_armed.swap(true, Ordering::AcqRel) {
                approval_trace(&format!(
                    "turn completion armed ({cli}); region={}",
                    clamp_trace(&prompt_region)
                ));
            }
            if prompt_ready {
                // Preferred: left prompt then returned. Codex often keeps › near
                // the caret all turn, so also accept a freshly armed completion bullet.
                // Claude can deliver a short reply and the returning prompt within
                // one output chunk; with no prompt leave observed, rely on the
                // screen change since the baseline instead.
                let completed = if cli == "codex" {
                    completion_bullet && input.take_completed_turn()
                } else if input.take_completed_turn() {
                    true
                } else if same_read_completion(
                    turn_screen_baseline.as_deref(),
                    &screen,
                    input.turn_activity_seen.load(Ordering::Acquire),
                ) {
                    input.clear_turn();
                    true
                } else {
                    false
                };
                if completed {
                    let result = classify_turn_result(&cli, &screen);
                    approval_trace(&format!(
                        "turn complete detected ({cli}): {result:?}; scan={}",
                        clamp_trace(&completion_scan)
                    ));
                    notify_turn_result(&cli, pid, result);
                }
            } else if !input.turn_activity_seen.swap(true, Ordering::AcqRel) {
                input.turn_completion_armed.store(true, Ordering::Release);
                approval_trace(&format!(
                    "turn activity seen ({cli}); region={}",
                    clamp_trace(&prompt_region)
                ));
            }
        }
        if !input.turn_pending.load(Ordering::Acquire) {
            turn_screen_baseline = None;
        }
        if let Some(prompt) = visible_confirmation {
            let dedupe_key = prompt.dedupe_key().to_owned();
            let should_request = terminal.last_prompt_key.as_ref() != Some(&dedupe_key);
            if should_request {
                approval_trace(&format!("visible confirmation detected: {dedupe_key}"));
                terminal.last_prompt_key = Some(dedupe_key);
                let token = input.begin_approval();
                if token != 0 {
                    let approval_input = input.clone();
                    let approval_cli = cli.clone();
                    let approval_cwd = cwd.clone();
                    let approval_prompt = prompt.clone();
                    let spawned = thread::Builder::new()
                        .name("headroom-approval-request".into())
                        .spawn(move || {
                            let decision = request_approval(
                                &approval_cli,
                                pid,
                                &approval_cwd,
                                &approval_prompt,
                                approval_input.source_window,
                                approval_input.focus_known.load(Ordering::Acquire),
                                approval_input.focused.load(Ordering::Acquire),
                            );
                            approval_input.finish_approval(token, decision, &approval_prompt);
                        });
                    if spawned.is_err() {
                        let _ = input.active_approval_token.compare_exchange(
                            token,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            }
        } else {
            terminal.last_prompt_key = None;
        }
    }
    Ok(())
}

pub fn approval_trace(message: &str) {
    if std::env::var_os("HEADROOM_ROUTE_APPROVAL_TRACE").is_some() {
        eprintln!("HeadroomRoute approval trace: {message}");
    }
}

fn clamp_trace(value: &str) -> String {
    let flat: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    trimmed.chars().take(160).collect()
}

fn advance_prompt_cycle(prompt_left: bool, prompt_ready: bool) -> (bool, bool) {
    (prompt_left || !prompt_ready, prompt_left && prompt_ready)
}

/// A reply whose whole output (and the returning input prompt) arrive in one
/// read chunk never shows a prompt-less frame, so the leave/return cycle never
/// fires.  Require a new non-prompt line: Enter echo that only repeats the
/// submitted text on `❯` must not count as completion.
fn same_read_completion(baseline: Option<&str>, screen: &str, activity_seen: bool) -> bool {
    !activity_seen
        && baseline
            .is_some_and(|before| before != screen && claude_screen_has_new_reply(before, screen))
}

fn codex_response_added(before: &str, after: &str) -> bool {
    let previous = before
        .lines()
        .map(str::trim)
        .collect::<std::collections::HashSet<_>>();
    after.lines().map(str::trim).any(|line| {
        (line.starts_with('•') || line.starts_with('·') || line.starts_with('■'))
            && !previous.contains(line)
    }) || (after.contains("Worked for") && !before.contains("Worked for"))
        || (after.contains("tokens used") && !before.contains("tokens used"))
}
pub struct TerminalScreen {
    parser: vt100::Parser,
    size: COORD,
    last_prompt_key: Option<String>,
}

impl TerminalScreen {
    pub fn new(size: COORD) -> Self {
        let size = normalize_console_size(size);
        Self {
            parser: vt100::Parser::new(size.Y as u16, size.X as u16, 0),
            size,
            last_prompt_key: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Text around the caret used for input-prompt detection.
    ///
    /// Codex often parks the cursor on the model line directly under `›` /
    /// `❯`, so include the preceding row. Do not include older rows: for a
    /// short reply the submitted prompt can remain two rows above the caret and
    /// would otherwise make the turn look idle for its entire lifetime.
    fn prompt_region_text(&self) -> String {
        let screen = self.parser.screen();
        let (row, _) = screen.cursor_position();
        let width = self.size.X.max(1) as u16;
        let row = row as usize;
        let start = row.saturating_sub(1);
        let end = row;
        screen
            .rows(0, width)
            .enumerate()
            .filter_map(|(index, line)| (index >= start && index <= end).then_some(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Bottom-of-screen text used to spot Codex end-of-turn chrome.
    ///
    /// `Worked for` / `tokens used` can sit several rows above the caret while
    /// the prompt stays near the bottom; the narrow prompt region misses them.
    fn completion_scan_text(&self) -> String {
        const BOTTOM_ROWS: usize = 12;
        let screen = self.parser.screen();
        let width = self.size.X.max(1) as u16;
        let rows: Vec<String> = screen.rows(0, width).collect();
        let start = rows.len().saturating_sub(BOTTOM_ROWS);
        rows[start..].join("\n")
    }

    fn resize_if_needed(&mut self) {
        let size = normalize_console_size(parent_console_size());
        if size.X != self.size.X || size.Y != self.size.Y {
            self.parser = vt100::Parser::new(size.Y as u16, size.X as u16, 0);
            self.size = size;
            self.last_prompt_key = None;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_region_includes_prompt_on_row_above_caret() {
        let mut terminal = TerminalScreen::new(COORD { X: 80, Y: 8 });
        terminal.process("› ready\r\n  gpt-5.6-sol medium".as_bytes());

        assert!(cli_input_prompt_ready(
            "codex",
            &terminal.prompt_region_text()
        ));
    }

    #[test]
    fn prompt_region_excludes_submitted_prompt_two_rows_above_caret() {
        let mut terminal = TerminalScreen::new(COORD { X: 80, Y: 8 });
        terminal.process("› submitted\r\n• short reply\r\nworking".as_bytes());

        assert!(!cli_input_prompt_ready(
            "codex",
            &terminal.prompt_region_text()
        ));
    }

    #[test]
    fn same_read_reply_completes_without_a_prompt_cycle() {
        let baseline = Some("❯ hi\n");
        let screen = "❯ hi\nHello!\n❯";
        assert!(same_read_completion(baseline, screen, false));
    }

    #[test]
    fn same_read_completion_requires_an_actual_screen_change() {
        let baseline = "❯ hi\n";
        assert!(!same_read_completion(Some(baseline), baseline, false));
        assert!(!same_read_completion(None, "anything", false));
    }

    #[test]
    fn observed_activity_defers_completion_to_the_prompt_cycle() {
        assert!(!same_read_completion(Some("a"), "b", true));
    }

    #[test]
    fn enter_echo_without_a_reply_is_not_same_read_completion() {
        assert!(!same_read_completion(Some("❯"), "❯ 只回复 pong\n❯", false));
    }

    #[test]
    fn ignores_prompt_redraw_with_only_stale_bullets() {
        let before = "• previous answer\n› old prompt\n";
        let after = "• previous answer\n› new prompt\n";
        assert!(!codex_response_added(before, after));
    }

    #[test]
    fn detects_new_codex_response_bullet() {
        let before = "• previous answer\n› old prompt\n";
        let after = "• previous answer\n• new answer\n› new prompt\n";
        assert!(codex_response_added(before, after));
    }

    #[test]
    fn prompt_must_leave_before_it_can_return() {
        assert_eq!(advance_prompt_cycle(false, true), (false, false));
        assert_eq!(advance_prompt_cycle(false, false), (true, false));
        assert_eq!(advance_prompt_cycle(true, false), (true, false));
        assert_eq!(advance_prompt_cycle(true, true), (true, true));
    }
}
