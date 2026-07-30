#![cfg(windows)]

use anyhow::{Result, anyhow};
use std::{
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{
        CLEARTYPE_QUALITY, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET, DeleteObject, FW_REGULAR,
        FW_SEMIBOLD, GetMonitorInfoW, GetSysColorBrush, HFONT, MONITOR_DEFAULTTONEAREST,
        MONITORINFO, MonitorFromPoint, UpdateWindow,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{
            ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx, PBM_SETMARQUEE,
            PBM_SETPOS, PBM_SETRANGE32, PBS_MARQUEE, PROGRESS_CLASSW,
        },
        HiDpi::GetDpiForSystem,
        WindowsAndMessaging::*,
    },
};

const STATIC_LEFT_CENTERED: u32 = 0x0200;
const ID_CANCEL: usize = 1;
const DEFAULT_HINT: &str = "此过程将在后台安全完成，完成后会自动继续。";

enum ProgressMessage {
    Status(String),
    Progress(Option<u32>),
    Close,
}

struct ProgressControls {
    window: HWND,
    status: HWND,
    hint: HWND,
    bar: HWND,
    cancel: HWND,
}

pub struct ProgressWindow {
    sender: Sender<ProgressMessage>,
    thread: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
}

impl ProgressWindow {
    pub fn open(title: &str, initial: &str) -> Result<Self> {
        Self::open_with_hint(title, initial, DEFAULT_HINT)
    }

    pub fn open_with_hint(title: &str, initial: &str, hint: &str) -> Result<Self> {
        Self::open_inner(title, initial, hint, false)
    }

    pub fn open_cancelable_with_hint(title: &str, initial: &str, hint: &str) -> Result<Self> {
        Self::open_inner(title, initial, hint, true)
    }

    fn open_inner(title: &str, initial: &str, hint: &str, cancelable: bool) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let title = title.to_owned();
        let initial = initial.to_owned();
        let hint = hint.to_owned();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = cancelled.clone();
        let thread = thread::spawn(move || {
            progress_thread(
                receiver,
                ready_sender,
                title,
                initial,
                hint,
                cancelable,
                thread_cancelled,
            )
        });
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                thread: Some(thread),
                cancelled,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(anyhow!(error))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("进度窗口意外退出"))
            }
        }
    }

    pub fn set_status(&self, status: &str) {
        let _ = self.sender.send(ProgressMessage::Status(status.to_owned()));
    }

    pub fn set_progress(&self, percent: u32) {
        let _ = self
            .sender
            .send(ProgressMessage::Progress(Some(percent.min(100))));
    }

    pub fn set_indeterminate(&self) {
        let _ = self.sender.send(ProgressMessage::Progress(None));
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn close(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.sender.send(ProgressMessage::Close);
            let _ = thread.join();
        }
    }
}

impl Drop for ProgressWindow {
    fn drop(&mut self) {
        self.stop();
    }
}

fn progress_thread(
    receiver: Receiver<ProgressMessage>,
    ready: mpsc::SyncSender<Result<(), String>>,
    title: String,
    initial: String,
    hint: String,
    cancelable: bool,
    cancelled: Arc<AtomicBool>,
) {
    unsafe {
        let common_controls = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_PROGRESS_CLASS,
        };
        if InitCommonControlsEx(&common_controls) == 0 {
            let _ = ready.send(Err("无法初始化进度控件".into()));
            return;
        }

        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("HeadroomRouteProgressWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            let _ = ready.send(Err("无法注册进度窗口".into()));
            return;
        }

        let dpi = GetDpiForSystem().max(96) as i32;
        let scale = |value: i32| value.saturating_mul(dpi) / 96;
        let width = scale(560);
        let height = scale(232);
        let (x, y) = centered_position(width, height);
        let window_title = format!("HeadroomRoute — {title}");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name.as_ptr(),
            wide(&window_title).as_ptr(),
            WS_CAPTION | WS_POPUP | WS_CLIPCHILDREN | if cancelable { WS_SYSMENU } else { 0 },
            x,
            y,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err("无法创建进度窗口".into()));
            return;
        }

        let margin = scale(28);
        let heading = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&title).as_ptr(),
            WS_CHILD | WS_VISIBLE | STATIC_LEFT_CENTERED,
            margin,
            scale(20),
            width - margin * 2,
            scale(30),
            hwnd,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        let status = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&initial).as_ptr(),
            WS_CHILD | WS_VISIBLE | STATIC_LEFT_CENTERED,
            margin,
            scale(58),
            width - margin * 2,
            scale(26),
            hwnd,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        let bar = CreateWindowExW(
            0,
            PROGRESS_CLASSW,
            ptr::null(),
            WS_CHILD | WS_VISIBLE | PBS_MARQUEE,
            margin,
            scale(96),
            width - margin * 2,
            scale(10),
            hwnd,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        let hint_width = if cancelable {
            width - margin * 2 - scale(128)
        } else {
            width - margin * 2
        };
        let hint_label = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&hint).as_ptr(),
            WS_CHILD | WS_VISIBLE,
            margin,
            scale(124),
            hint_width,
            scale(42),
            hwnd,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if heading.is_null() || status.is_null() || bar.is_null() || hint_label.is_null() {
            DestroyWindow(hwnd);
            let _ = ready.send(Err("无法创建进度窗口内容".into()));
            return;
        }

        let cancel = if cancelable {
            CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide("取消").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                width - margin - scale(104),
                scale(130),
                scale(104),
                scale(34),
                hwnd,
                ID_CANCEL as _,
                instance,
                ptr::null(),
            )
        } else {
            ptr::null_mut()
        };
        if cancelable && cancel.is_null() {
            DestroyWindow(hwnd);
            let _ = ready.send(Err("无法创建取消按钮".into()));
            return;
        }

        let heading_font = create_font(scale(18), FW_SEMIBOLD as i32);
        let body_font = create_font(scale(15), FW_REGULAR as i32);
        let hint_font = create_font(scale(13), FW_REGULAR as i32);
        set_font(heading, heading_font);
        set_font(status, body_font);
        set_font(hint_label, hint_font);
        if !cancel.is_null() {
            set_font(cancel, body_font);
        }
        SendMessageW(bar, PBM_SETRANGE32, 0, 100);
        SendMessageW(bar, PBM_SETMARQUEE, 1, 35);

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        SetForegroundWindow(hwnd);
        if ready.send(Ok(())).is_err() {
            DestroyWindow(hwnd);
            delete_font(heading_font);
            delete_font(body_font);
            delete_font(hint_font);
            return;
        }
        run_message_loop(
            ProgressControls {
                window: hwnd,
                status,
                hint: hint_label,
                bar,
                cancel,
            },
            receiver,
            cancelable,
            cancelled,
        );
        delete_font(heading_font);
        delete_font(body_font);
        delete_font(hint_font);
    }
}

unsafe fn run_message_loop(
    controls: ProgressControls,
    receiver: Receiver<ProgressMessage>,
    cancelable: bool,
    cancelled: Arc<AtomicBool>,
) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    let mut cancel_requested = false;
    loop {
        while unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                return;
            }
            if cancelable
                && (message.message == WM_CLOSE
                    || (message.message == WM_COMMAND && message.wParam & 0xffff == ID_CANCEL))
            {
                cancelled.store(true, Ordering::Release);
                cancel_requested = true;
                unsafe {
                    ShowWindow(controls.cancel, SW_HIDE);
                    SetWindowTextW(controls.status, wide("正在安全取消下载").as_ptr());
                    SetWindowTextW(
                        controls.hint,
                        wide("正在结束下载并清理临时文件，请稍候。").as_ptr(),
                    );
                    set_progress(controls.bar, None);
                }
                continue;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        let mut next_status = None;
        let mut next_progress = None;
        let mut progress_changed = false;
        let mut close = false;
        loop {
            match receiver.try_recv() {
                Ok(ProgressMessage::Status(next)) => next_status = Some(next),
                Ok(ProgressMessage::Progress(next)) => {
                    next_progress = next;
                    progress_changed = true;
                }
                Ok(ProgressMessage::Close) | Err(TryRecvError::Disconnected) => {
                    close = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if close {
            unsafe { DestroyWindow(controls.window) };
            return;
        }
        if !cancel_requested {
            if let Some(next) = next_status {
                unsafe { SetWindowTextW(controls.status, wide(next.trim()).as_ptr()) };
            }
            if progress_changed {
                unsafe { set_progress(controls.bar, next_progress) };
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

unsafe fn set_progress(progress_bar: HWND, percent: Option<u32>) {
    if let Some(percent) = percent {
        unsafe {
            SendMessageW(progress_bar, PBM_SETMARQUEE, 0, 0);
            SendMessageW(progress_bar, PBM_SETPOS, percent as usize, 0);
        }
    } else {
        unsafe {
            SendMessageW(progress_bar, PBM_SETPOS, 0, 0);
            SendMessageW(progress_bar, PBM_SETMARQUEE, 1, 35);
        }
    }
}

unsafe fn create_font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Segoe UI").as_ptr(),
        )
    }
}

unsafe fn set_font(hwnd: HWND, font: HFONT) {
    if !font.is_null() {
        unsafe { SendMessageW(hwnd, WM_SETFONT, font as usize, 1) };
    }
}

unsafe fn delete_font(font: HFONT) {
    if !font.is_null() {
        unsafe {
            DeleteObject(font);
        }
    }
}

unsafe fn centered_position(width: i32, height: i32) -> (i32, i32) {
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor) };
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if !monitor.is_null() && unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let x = info.rcWork.left + (info.rcWork.right - info.rcWork.left - width) / 2;
        let y = info.rcWork.top + (info.rcWork.bottom - info.rcWork.top - height) / 2;
        (x, y)
    } else {
        (
            (unsafe { GetSystemMetrics(SM_CXSCREEN) } - width) / 2,
            (unsafe { GetSystemMetrics(SM_CYSCREEN) } - height) / 2,
        )
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => 0,
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
