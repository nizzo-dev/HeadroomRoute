use super::super::{ApprovalDecision, ConfirmationPrompt, cancel_remote_requests};
use super::io::update_remote_focus;
use std::{
    fs::File,
    io::{self, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

pub struct InputSink {
    pub file: Mutex<Option<File>>,
    pub next_approval_token: AtomicU64,
    pub active_approval_token: AtomicU64,
    pub pid: u32,
    pub session_pid: u32,
    pub source_window: u64,
    pub focus_known: AtomicBool,
    pub focused: AtomicBool,
    pub turn_pending: AtomicBool,
    pub turn_activity_seen: AtomicBool,
    pub turn_prompt_left: AtomicBool,
    pub turn_prompt_returned: AtomicBool,
    /// After submit, ignore a stale `• 已完成` until it leaves the region once
    /// (or non-prompt activity is seen). Prevents instant false completes.
    pub turn_completion_armed: AtomicBool,
    pub turn_input_has_text: AtomicBool,
}

impl InputSink {
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().unwrap();
        let Some(file) = file.as_mut() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        file.flush()
    }

    pub fn begin_approval(&self) -> u64 {
        let token = self
            .next_approval_token
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        match self.active_approval_token.compare_exchange(
            0,
            token,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => token,
            Err(_) => 0,
        }
    }

    pub fn finish_approval(
        &self,
        token: u64,
        decision: ApprovalDecision,
        prompt: &ConfirmationPrompt,
    ) {
        if token == 0
            || self
                .active_approval_token
                .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let answer = match decision {
            ApprovalDecision::Approved => Some(prompt.approve_answer),
            ApprovalDecision::ApprovedAlways => prompt.allow_rule_answer,
            ApprovalDecision::Feedback => prompt.feedback_answer,
            ApprovalDecision::Denied => Some(prompt.deny_answer),
            ApprovalDecision::Cancelled => None,
        };
        if let Some(answer) = answer {
            let _ = self.write(answer.as_bytes());
        }
    }

    pub fn write_user_input(&self, bytes: &[u8]) -> io::Result<()> {
        if self.active_approval_token.swap(0, Ordering::AcqRel) != 0 {
            let pid = self.pid;
            let _ = thread::Builder::new()
                .name("headroom-approval-cancel".into())
                .spawn(move || cancel_remote_requests(pid));
        }
        self.mark_turn_submitted(bytes);
        self.write(bytes)
    }

    pub fn mark_turn_submitted(&self, bytes: &[u8]) {
        let submitted = input_contains_submit(bytes);
        let has_text = input_has_user_text(bytes);
        if has_text {
            self.turn_input_has_text.store(true, Ordering::Release);
        }
        // Always consume the accumulated-text flag on submit.  Using
        // `has_text || swap(false)` leaves the flag set when text and Enter
        // arrive in the same read because `||` short-circuits.  The next bare
        // Enter would then be mistaken for a newly submitted turn.
        if submitted && self.turn_input_has_text.swap(false, Ordering::AcqRel) {
            self.turn_activity_seen.store(false, Ordering::Release);
            self.turn_prompt_left.store(false, Ordering::Release);
            self.turn_prompt_returned.store(false, Ordering::Release);
            self.turn_completion_armed.store(false, Ordering::Release);
            self.turn_pending.store(true, Ordering::Release);
        }
    }

    pub fn take_completed_turn(&self) -> bool {
        if !self.turn_pending.load(Ordering::Acquire)
            || !self.turn_activity_seen.load(Ordering::Acquire)
            || !self.turn_prompt_returned.load(Ordering::Acquire)
        {
            return false;
        }
        self.clear_turn();
        true
    }

    pub fn clear_turn(&self) {
        self.turn_pending.store(false, Ordering::Release);
        self.turn_activity_seen.store(false, Ordering::Release);
        self.turn_prompt_left.store(false, Ordering::Release);
        self.turn_prompt_returned.store(false, Ordering::Release);
        self.turn_completion_armed.store(false, Ordering::Release);
    }

    pub fn observe_focus(&self, bytes: &[u8]) -> Option<bool> {
        let focused = match bytes {
            b"\x1b[I" => true,
            b"\x1b[O" => false,
            _ => return None,
        };
        self.focus_known.store(true, Ordering::Release);
        self.focused.store(focused, Ordering::Release);
        let pid = self.pid;
        let session_pid = self.session_pid;
        let _ = thread::Builder::new()
            .name("headroom-focus-update".into())
            .spawn(move || {
                update_remote_focus(pid, focused);
                if session_pid != pid {
                    update_remote_focus(session_pid, focused);
                }
            });
        Some(focused)
    }

    pub fn close(&self) {
        self.file.lock().unwrap().take();
    }
}

/// Windows Terminal can encode Enter as a Win32 input record (`CSI ... _`)
/// or CSI-u key event instead of a literal CR/LF after Codex enables mode 9001.
fn input_contains_submit(bytes: &[u8]) -> bool {
    if bytes.iter().any(|byte| *byte == b'\r' || *byte == b'\n') {
        return true;
    }
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let Some((parameters, terminator, next)) = csi_sequence(bytes, index) else {
            break;
        };
        let enter = match terminator {
            b'u' => csi_parameter(parameters, 0) == Some(13),
            b'_' => {
                csi_parameter(parameters, 0) == Some(13) && csi_parameter(parameters, 3) != Some(0)
            }
            _ => false,
        };
        if enter {
            return true;
        }
        index = next;
    }
    false
}

fn csi_sequence(bytes: &[u8], start: usize) -> Option<(&[u8], u8, usize)> {
    if bytes.get(start).copied() != Some(0x1b) || bytes.get(start + 1).copied() != Some(b'[') {
        return None;
    }
    let parameters_start = start + 2;
    let end = bytes[parameters_start..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))?
        + parameters_start;
    Some((&bytes[parameters_start..end], bytes[end], end + 1))
}

fn csi_parameter(parameters: &[u8], index: usize) -> Option<u32> {
    std::str::from_utf8(parameters)
        .ok()?
        .split(';')
        .nth(index)?
        .split(':')
        .next()?
        .parse()
        .ok()
}

fn csi_text_codepoint(parameters: &[u8], terminator: u8) -> Option<u32> {
    match terminator {
        // CSI-u stores the Unicode code point in the first parameter.
        b'u' => csi_parameter(parameters, 0),
        // Windows Terminal's Win32 input record stores UnicodeChar third.
        b'_' => csi_parameter(parameters, 2),
        _ => None,
    }
}

fn is_user_text_codepoint(value: u32) -> bool {
    let Some(character) = char::from_u32(value) else {
        return false;
    };
    !character.is_control()
        && !matches!(
            value,
            0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd
        )
}

/// Returns whether an input chunk contains actual user text.
///
/// ConPTY/Windows Terminal keyboard protocols encode a single Enter key as an
/// ANSI control sequence (for example `CSI 13;1u` or a Win32 input record
/// ending in `_`). Those sequences contain ASCII punctuation and digits, so a
/// raw `byte >= b' '` check incorrectly treats a bare Enter as typed text.
/// Skip escape/control sequences and count only bytes outside them.
fn input_has_user_text(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == 0x1b {
            index += 1;
            if index >= bytes.len() {
                continue;
            }
            match bytes[index] {
                b'[' => {
                    let sequence_start = index - 1;
                    let Some((parameters, terminator, next)) = csi_sequence(bytes, sequence_start)
                    else {
                        break;
                    };
                    if csi_text_codepoint(parameters, terminator)
                        .is_some_and(is_user_text_codepoint)
                    {
                        return true;
                    }
                    index = next;
                }
                b']' => {
                    // OSC: consume until BEL or the ST sequence (ESC \\).
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'\\') {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => {
                    // Two-byte escape (arrows, focus, etc.).
                    index += 1;
                }
            }
            continue;
        }
        index += 1;
        if byte < 0x20 || byte == 0x7f {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod input_tests {
    use super::{InputSink, input_contains_submit, input_has_user_text};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    #[test]
    fn detects_raw_enter() {
        assert!(input_contains_submit(b"hello\r"));
        assert!(input_contains_submit(b"hello\n"));
    }

    #[test]
    fn detects_windows_terminal_enter_record() {
        assert!(input_contains_submit(b"\x1b[13;28;13;1;0;1_"));
        assert!(!input_contains_submit(b"\x1b[13;28;13;0;0;1_"));
    }

    #[test]
    fn detects_csi_u_enter() {
        assert!(input_contains_submit(b"\x1b[13;1u"));
    }

    #[test]
    fn bare_enter_sequences_are_not_user_text() {
        assert!(!input_has_user_text(b"\r"));
        assert!(!input_has_user_text(b"\n"));
        assert!(!input_has_user_text(b"\x1b[13;1u"));
        assert!(!input_has_user_text(b"\x1b[13;28;13;1;0;1_"));
        assert!(!input_has_user_text(b"\x1b[13;28;13;0;0;1_"));
        assert!(!input_has_user_text(b"\x1b[I"));
        assert!(!input_has_user_text(b"\x1b[200~\x1b[201~"));
    }

    #[test]
    fn text_with_protocol_enter_is_user_text() {
        assert!(input_has_user_text(b"hello\x1b[13;1u"));
        assert!(input_has_user_text(b"hello\x1b[13;28;13;1;0;1_"));
        assert!(input_has_user_text("你好\r".as_bytes()));
    }

    #[test]
    fn recognizes_printable_keyboard_protocol_records() {
        assert!(input_has_user_text(b"\x1b[97;1u"));
        assert!(input_has_user_text(b"\x1b[65;30;65;1;0;1_"));
        assert!(!input_has_user_text(b"\x1b[57361;1u"));
        assert!(!input_has_user_text(b"\x1b[A"));
    }

    #[test]
    fn text_and_enter_in_one_read_do_not_leak_into_next_enter() {
        let sink = InputSink {
            file: Mutex::new(None),
            next_approval_token: AtomicU64::new(1),
            active_approval_token: AtomicU64::new(0),
            pid: 1,
            session_pid: 1,
            source_window: 0,
            focus_known: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            turn_pending: AtomicBool::new(false),
            turn_activity_seen: AtomicBool::new(false),
            turn_prompt_left: AtomicBool::new(false),
            turn_prompt_returned: AtomicBool::new(false),
            turn_completion_armed: AtomicBool::new(false),
            turn_input_has_text: AtomicBool::new(false),
        };

        sink.mark_turn_submitted(b"hello\r");
        assert!(sink.turn_pending.load(Ordering::Acquire));
        sink.clear_turn();
        sink.mark_turn_submitted(b"\r");
        assert!(!sink.turn_pending.load(Ordering::Acquire));
    }
}
