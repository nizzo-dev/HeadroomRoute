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
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{COLOR_WINDOW, GetSysColorBrush, UpdateWindow},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

const STATIC_CENTERED: u32 = 0x0001 | 0x0200;
const ID_CANCEL: usize = 1;

enum ProgressMessage {
    Status(String),
    Close,
}

pub struct ProgressWindow {
    sender: Sender<ProgressMessage>,
    thread: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
}

impl ProgressWindow {
    pub fn open(title: &str, initial: &str) -> Result<Self> {
        Self::open_inner(title, initial, false)
    }

    pub fn open_cancelable(title: &str, initial: &str) -> Result<Self> {
        Self::open_inner(title, initial, true)
    }

    fn open_inner(title: &str, initial: &str, cancelable: bool) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let title = title.to_owned();
        let initial = initial.to_owned();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = cancelled.clone();
        let thread = thread::spawn(move || {
            progress_thread(
                receiver,
                ready_sender,
                title,
                initial,
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
    cancelable: bool,
    cancelled: Arc<AtomicBool>,
) {
    unsafe {
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

        let width = 500;
        let height = 170;
        let x = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - height) / 2;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            wide(&title).as_ptr(),
            WS_CAPTION | WS_POPUP | WS_SYSMENU,
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

        let label = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            wide(&initial).as_ptr(),
            WS_CHILD | WS_VISIBLE | STATIC_CENTERED,
            20,
            15,
            width - 40,
            if cancelable { height - 85 } else { height - 55 },
            hwnd,
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if label.is_null() {
            DestroyWindow(hwnd);
            let _ = ready.send(Err("无法创建进度提示".into()));
            return;
        }

        let cancel = if cancelable {
            CreateWindowExW(
                0,
                wide("BUTTON").as_ptr(),
                wide("取消下载").as_ptr(),
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON as u32,
                width / 2 - 55,
                height - 70,
                110,
                30,
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

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        if ready.send(Ok(())).is_err() {
            DestroyWindow(hwnd);
            return;
        }
        run_message_loop(
            hwnd, label, cancel, receiver, initial, cancelable, cancelled,
        );
    }
}

unsafe fn run_message_loop(
    hwnd: HWND,
    label: HWND,
    cancel: HWND,
    receiver: Receiver<ProgressMessage>,
    initial: String,
    cancelable: bool,
    cancelled: Arc<AtomicBool>,
) {
    let mut message: MSG = unsafe { std::mem::zeroed() };
    let mut status = initial;
    let mut animation = Instant::now();
    let mut dots = 0;
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
                unsafe {
                    ShowWindow(cancel, SW_HIDE);
                    SetWindowTextW(label, wide("正在取消下载，请稍候...").as_ptr());
                }
                continue;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        match receiver.try_recv() {
            Ok(ProgressMessage::Status(next)) => {
                status = next.trim_end_matches(['.', '。']).to_owned();
                dots = 0;
                animation = Instant::now();
                unsafe { update_label(label, &status, dots, cancelable) };
            }
            Ok(ProgressMessage::Close) | Err(TryRecvError::Disconnected) => {
                unsafe { DestroyWindow(hwnd) };
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        if animation.elapsed() >= Duration::from_millis(500) {
            dots = (dots + 1) % 4;
            unsafe { update_label(label, &status, dots, cancelable) };
            animation = Instant::now();
        }
        thread::sleep(Duration::from_millis(30));
    }
}

unsafe fn update_label(label: HWND, status: &str, dots: usize, cancelable: bool) {
    let hint = if cancelable {
        "可点击取消下载。"
    } else {
        "请勿关闭程序，完成后将自动提示。"
    };
    let text = format!("{status}{}\r\n\r\n{hint}", ".".repeat(dots),);
    unsafe { SetWindowTextW(label, wide(&text).as_ptr()) };
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
