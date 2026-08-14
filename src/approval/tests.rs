use super::{
    ApprovalDecision, ConfirmationPrompt, InputSink, TerminalScreen, build_command_line,
    confirmation_answers, confirmation_prompt, connect_pipe, is_cli_executable_name,
    prompt_summary, quote_cmd_arg, start_server, strip_ansi,
};
use std::{
    io::{BufRead, BufReader, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64},
    },
    time::Duration,
};
use windows_sys::Win32::System::Console::COORD;

#[test]
fn detects_confirmation_prompt_but_not_normal_output() {
    assert!(
        confirmation_prompt(
            "codex",
            "Would you like to allow this command to run? Yes / No"
        )
        .is_some()
    );
    assert!(confirmation_prompt("codex", "Build completed successfully").is_none());
}

#[test]
fn ignores_generic_questions_without_an_explicit_permission_marker() {
    assert!(confirmation_prompt("codex", "Should we proceed? Yes / No").is_none());
    assert!(confirmation_prompt("claude", "The answer is yes or no").is_none());
}

#[test]
fn leaves_workspace_trust_prompt_to_native_cli_input() {
    let prompt = "Accessing workspace: C:\\Users\\HD Quick safety check: Is this a project you created or one you trust? Claude Code'll be able to read, edit, and execute files here. Security guide 1. Yes, I trust this folder 2. No, exit";
    assert!(confirmation_prompt("claude", prompt).is_none());
}

#[test]
fn strips_terminal_control_sequences() {
    assert_eq!(strip_ansi("\x1b[32mAllow?\x1b[0m\r\n"), "Allow?\n\n");
}

#[test]
fn bounds_prompt_summary() {
    let prompt = confirmation_prompt(
        "codex",
        &format!(
            "{} Would you like to allow this command? Yes No",
            "x".repeat(600)
        ),
    )
    .unwrap();
    assert!(prompt.summary.chars().count() <= 420);
}

#[test]
fn summarizes_the_visible_prompt_without_working_status() {
    let summary = prompt_summary(
        "Working (12s • esc to interrupt)\r\nWould you like to allow this command?\r\nYes / No\r\n",
    );
    assert!(!summary.contains("Working"));
    assert!(summary.contains("Would you like to allow"));
}

#[test]
fn detects_confirmation_from_the_rendered_terminal_screen() {
    let mut terminal = TerminalScreen::new(COORD { X: 120, Y: 40 });
    terminal.process(
            b"\x1b[?9001h\x1b[?1004hWould you like to allow this command to run?\r\n1. Yes\r\n2. No\r\n",
        );
    let screen = terminal.contents();
    assert!(
        confirmation_prompt("codex", &screen).is_some(),
        "rendered screen: {screen:?}"
    );
}

#[test]
fn terminal_input_cancels_an_active_popup_without_injecting_an_answer() {
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
    let prompt = ConfirmationPrompt {
        action: "git status".into(),
        summary: "Would you like to allow this command? Yes / No".into(),
        approve_answer: "y\n",
        allow_rule_answer: None,
        feedback_answer: None,
        deny_answer: "n\n",
    };
    let token = sink.begin_approval();
    assert_ne!(token, 0);
    sink.finish_approval(token, ApprovalDecision::Cancelled, &prompt);
    assert_eq!(
        sink.active_approval_token
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[test]
fn large_terminal_output_no_longer_hides_an_active_confirmation() {
    let output = format!(
        "Would you like to allow this command? Yes / No {}",
        "normal output ".repeat(140)
    );
    assert!(confirmation_prompt("codex", &output).is_some());
}

#[test]
fn selects_first_or_last_numbered_permission_option() {
    let prompt = "1. Yes, allow once 2. Yes always 3. No";
    assert_eq!(
        confirmation_answers(prompt),
        ("1\n", Some("2\n"), None, "3\n")
    );
    assert_eq!(
        confirmation_answers("Proceed? (y/n)"),
        ("y\n", None, None, "n\n")
    );
}

#[test]
fn exposes_native_allow_rule_and_feedback_answers() {
    let answers = confirmation_answers(
        "1. Yes, allow once 2. Yes, and don't ask again 3. No, and tell Codex what to do differently",
    );
    assert_eq!(answers, ("1\n", Some("2\n"), Some("3\n"), "3\n"));
}

#[test]
fn extracts_command_from_confirmation_prompt() {
    let prompt = confirmation_prompt(
        "claude",
        "Claude needs permission\r\n> cargo test\r\nProceed? Yes / No",
    )
    .unwrap();
    assert_eq!(prompt.action, "cargo test");
}

#[test]
fn quotes_cli_arguments_for_cmd() {
    assert_eq!(quote_cmd_arg("codex"), "codex");
    assert_eq!(quote_cmd_arg("hello world"), "\"hello world\"");
    assert!(
        build_command_line("claude", &["--model".into(), "sonnet 4".into()])
            .contains("claude --model \"sonnet 4\"")
    );
}

#[test]
fn recognizes_installed_and_versioned_cli_names() {
    assert!(is_cli_executable_name("HeadroomRouteCLI"));
    assert!(is_cli_executable_name("headroomroutecli-0.6.9"));
    assert!(!is_cli_executable_name("HeadroomRoute"));
}

#[test]
fn local_pipe_accepts_current_user_and_rejects_invalid_payload() {
    start_server();
    std::thread::sleep(Duration::from_millis(100));
    let mut stream = connect_pipe().expect("approval pipe should be available");
    stream.write_all(b"{}\n").unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(response.contains("invalid_request"));
}
