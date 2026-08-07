#![cfg(windows)]

#[allow(dead_code)]
#[path = "approval.rs"]
mod approval;

const TERMINAL_CLEANUP: &[u8] = b"\x1b[?9001l\x1b[?1004l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[0m\r\n";

fn main() {
    let _console = ConsoleSettings::configure();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let cli_args = if args.first().map(String::as_str) == Some("run") {
        &args[1..]
    } else {
        &args[..]
    };
    let exit_code = match approval::run_cli_command(cli_args) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("HeadroomRoute CLI 启动失败：{error:#}");
            1
        }
    };
    drop(_console);
    std::process::exit(exit_code);
}

struct ConsoleSettings {
    input: windows_sys::Win32::Foundation::HANDLE,
    output: windows_sys::Win32::Foundation::HANDLE,
    input_mode: Option<u32>,
    output_mode: Option<u32>,
    input_code_page: u32,
    output_code_page: u32,
}

impl ConsoleSettings {
    fn configure() -> Self {
        use windows_sys::Win32::System::Console::*;

        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let input_code_page = GetConsoleCP();
            let output_code_page = GetConsoleOutputCP();
            if input_code_page != 0 {
                let _ = SetConsoleCP(65001);
            }
            if output_code_page != 0 {
                let _ = SetConsoleOutputCP(65001);
            }
            let mut input_mode = 0;
            let saved_input_mode =
                (GetConsoleMode(input, &mut input_mode) != 0).then_some(input_mode);
            if let Some(mode) = saved_input_mode {
                let _ = SetConsoleMode(input, forwarded_input_mode(mode));
            }
            let mut output_mode = 0;
            let saved_output_mode =
                (GetConsoleMode(output, &mut output_mode) != 0).then_some(output_mode);
            if let Some(mode) = saved_output_mode {
                let _ = SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
            Self {
                input,
                output,
                input_mode: saved_input_mode,
                output_mode: saved_output_mode,
                input_code_page,
                output_code_page,
            }
        }
    }
}

fn forwarded_input_mode(mode: u32) -> u32 {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    };

    mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)
        | ENABLE_VIRTUAL_TERMINAL_INPUT
}

impl Drop for ConsoleSettings {
    fn drop(&mut self) {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Console::*;

        unsafe {
            if self.output_mode.is_some() {
                let mut written = 0;
                let _ = WriteFile(
                    self.output,
                    TERMINAL_CLEANUP.as_ptr(),
                    TERMINAL_CLEANUP.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
            if let Some(mode) = self.input_mode {
                let _ = SetConsoleMode(self.input, mode);
            }
            if let Some(mode) = self.output_mode {
                let _ = SetConsoleMode(self.output, mode);
            }
            if self.input_code_page != 0 {
                let _ = SetConsoleCP(self.input_code_page);
            }
            if self.output_code_page != 0 {
                let _ = SetConsoleOutputCP(self.output_code_page);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TERMINAL_CLEANUP, forwarded_input_mode};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT,
    };

    #[test]
    fn ctrl_c_is_forwarded_instead_of_terminating_wrapper() {
        let original =
            ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_MOUSE_INPUT;
        let forwarded = forwarded_input_mode(original);
        assert_eq!(forwarded & ENABLE_PROCESSED_INPUT, 0);
        assert_eq!(forwarded & ENABLE_LINE_INPUT, 0);
        assert_eq!(forwarded & ENABLE_ECHO_INPUT, 0);
        assert_ne!(forwarded & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(forwarded & ENABLE_MOUSE_INPUT, 0);
    }

    #[test]
    fn cleanup_disables_codex_private_input_modes() {
        let cleanup = std::str::from_utf8(TERMINAL_CLEANUP).unwrap();
        assert!(cleanup.contains("\x1b[?9001l"));
        assert!(cleanup.contains("\x1b[?1004l"));
        assert!(cleanup.contains("\x1b[?2004l"));
        assert!(cleanup.contains("\x1b[?25h"));
        assert!(!cleanup.contains("\x1b[?1049l"));
        assert!(cleanup.ends_with("\r\n"));
    }
}
