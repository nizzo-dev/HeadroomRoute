use std::{
    cell::RefCell,
    collections::VecDeque,
    ptr,
    sync::{OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DEFAULT_CHARSET,
        DEFAULT_PITCH, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
        DeleteObject, DrawTextW, EndPaint, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, FillRect,
        GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        OUT_DEFAULT_PRECIS, PAINTSTRUCT, SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::GetDpiForSystem,
        WindowsAndMessaging::{
            AW_CENTER, AW_HIDE, AnimateWindow, CS_HREDRAW, CS_VREDRAW, CreateWindowExW,
            DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, HWND_TOPMOST, IDC_ARROW,
            KillTimer, LoadCursorW, MoveWindow, RegisterClassW, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SetTimer, SetWindowPos, TranslateMessage, WM_DESTROY, WM_LBUTTONUP,
            WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            WS_POPUP,
        },
    },
};

const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 16;
const POPUP_OPEN_ANIMATION_MS: u32 = 180;
const POPUP_CLOSE_ANIMATION_MS: u32 = 140;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    Success,
    Info,
    Warning,
    Error,
}

impl Level {
    fn timeout(self) -> Duration {
        match self {
            Self::Success | Self::Info => Duration::from_secs(3),
            Self::Warning | Self::Error => Duration::from_secs(6),
        }
    }

    fn color(self) -> u32 {
        match self {
            Self::Success => rgb(67, 214, 142),
            Self::Info => rgb(88, 166, 255),
            Self::Warning => rgb(255, 190, 72),
            Self::Error => rgb(255, 92, 108),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success => "成功",
            Self::Info => "信息",
            Self::Warning => "警告",
            Self::Error => "错误",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub level: Level,
    pub title: String,
    pub message: String,
}

struct Envelope {
    request: Request,
    acknowledged: Option<mpsc::Sender<()>>,
}

struct Pending {
    envelope: Envelope,
    repeats: u32,
}

struct Current {
    pending: Pending,
    expires_at: Instant,
}

struct HostState {
    receiver: mpsc::Receiver<Envelope>,
    queue: VecDeque<Pending>,
    current: Option<Current>,
}

static SENDER: OnceLock<mpsc::Sender<Envelope>> = OnceLock::new();

thread_local! {
    static HOST: RefCell<Option<HostState>> = const { RefCell::new(None) };
}

pub fn success(title: impl Into<String>, message: impl Into<String>) {
    enqueue(Level::Success, title, message);
}

pub fn info(title: impl Into<String>, message: impl Into<String>) {
    enqueue(Level::Info, title, message);
}

pub fn warning(title: impl Into<String>, message: impl Into<String>) {
    enqueue(Level::Warning, title, message);
}

pub fn error(title: impl Into<String>, message: impl Into<String>) {
    enqueue(Level::Error, title, message);
}

pub fn enqueue(level: Level, title: impl Into<String>, message: impl Into<String>) {
    let _ = sender().send(Envelope {
        request: Request {
            level,
            title: title.into(),
            message: message.into(),
        },
        acknowledged: None,
    });
}

pub fn blocking_error(title: impl Into<String>, message: impl Into<String>) {
    blocking(Level::Error, title, message);
}

pub fn blocking_info(title: impl Into<String>, message: impl Into<String>) {
    blocking(Level::Info, title, message);
}

fn blocking(level: Level, title: impl Into<String>, message: impl Into<String>) {
    let (tx, rx) = mpsc::channel();
    let _ = sender().send(Envelope {
        request: Request {
            level,
            title: title.into(),
            message: message.into(),
        },
        acknowledged: Some(tx),
    });
    let _ = rx.recv_timeout(Duration::from_secs(10));
}

fn sender() -> &'static mpsc::Sender<Envelope> {
    SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("headroom-notification-host".into())
            .spawn(move || run_host(receiver))
            .expect("unable to start notification host");
        sender
    })
}

fn run_host(receiver: mpsc::Receiver<Envelope>) {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("HeadroomRouteNotificationIsland");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            lpszClassName: class_name.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&class) == 0 {
            return;
        }
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            wide("HeadroomRoute").as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        HOST.with(|host| {
            *host.borrow_mut() = Some(HostState {
                receiver,
                queue: VecDeque::new(),
                current: None,
            });
        });
        SetTimer(hwnd, TIMER_ID, TIMER_MS, None);
        let mut message = std::mem::zeroed();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER if wparam == TIMER_ID => {
            unsafe { tick(hwnd) };
            0
        }
        WM_LBUTTONUP => {
            unsafe { close_current(hwnd) };
            0
        }
        WM_PAINT => {
            unsafe { paint(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { KillTimer(hwnd, TIMER_ID) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn tick(hwnd: HWND) {
    let mut changed = false;
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        let Some(host) = host.as_mut() else { return };
        while let Ok(envelope) = host.receiver.try_recv() {
            if let Some(current) = host.current.as_mut()
                && current.pending.envelope.request == envelope.request
            {
                current.pending.repeats = current.pending.repeats.saturating_add(1);
                current.expires_at = Instant::now() + envelope.request.level.timeout();
                if let Some(done) = envelope.acknowledged {
                    let _ = done.send(());
                }
                changed = true;
                continue;
            }
            if let Some(back) = host.queue.back_mut()
                && back.envelope.request == envelope.request
            {
                back.repeats = back.repeats.saturating_add(1);
                if let Some(done) = envelope.acknowledged {
                    let _ = done.send(());
                }
            } else {
                host.queue.push_back(Pending {
                    envelope,
                    repeats: 1,
                });
            }
        }
        if host
            .current
            .as_ref()
            .is_some_and(|item| Instant::now() >= item.expires_at)
        {
            acknowledge(host.current.take());
            changed = true;
        }
        if host.current.is_none()
            && let Some(pending) = host.queue.pop_front()
        {
            let timeout = pending.envelope.request.level.timeout();
            host.current = Some(Current {
                pending,
                expires_at: Instant::now() + timeout,
            });
            changed = true;
        }
    });
    if changed {
        if has_current() {
            unsafe {
                position(hwnd);
                AnimateWindow(hwnd, POPUP_OPEN_ANIMATION_MS, AW_CENTER);
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                InvalidateRect(hwnd, ptr::null(), 1);
            }
        } else {
            unsafe { AnimateWindow(hwnd, POPUP_CLOSE_ANIMATION_MS, AW_HIDE | AW_CENTER) };
        }
    }
}

unsafe fn close_current(hwnd: HWND) {
    HOST.with(|host| {
        if let Some(host) = host.borrow_mut().as_mut() {
            acknowledge(host.current.take());
        }
    });
    unsafe { AnimateWindow(hwnd, POPUP_CLOSE_ANIMATION_MS, AW_HIDE | AW_CENTER) };
}

fn acknowledge(current: Option<Current>) {
    if let Some(Current { pending, .. }) = current
        && let Some(done) = pending.envelope.acknowledged
    {
        let _ = done.send(());
    }
}

fn has_current() -> bool {
    HOST.with(|host| {
        host.borrow()
            .as_ref()
            .is_some_and(|host| host.current.is_some())
    })
}

unsafe fn position(hwnd: HWND) {
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    let width = 520 * dpi / 96;
    let height = 82 * dpi / 96;
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTONEAREST) };
    unsafe { GetMonitorInfoW(monitor, &mut monitor_info) };
    let x = monitor_info.rcWork.left
        + (monitor_info.rcWork.right - monitor_info.rcWork.left - width) / 2;
    let y = monitor_info.rcWork.top + 14 * dpi / 96;
    unsafe {
        MoveWindow(hwnd, x, y, width, height, 1);
        let radius = 22 * dpi / 96;
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius, radius);
        SetWindowRgn(hwnd, region, 1);
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut paint = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) };
    let background = unsafe { CreateSolidBrush(rgb(24, 25, 28)) };
    unsafe {
        FillRect(dc, &rect, background);
        DeleteObject(background);
        SetBkMode(dc, TRANSPARENT as i32);
    }

    HOST.with(|host| {
        let host = host.borrow();
        let Some(current) = host.as_ref().and_then(|host| host.current.as_ref()) else {
            return;
        };
        let request = &current.pending.envelope.request;
        let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
        let title_font = unsafe {
            CreateFontW(
                -16 * dpi / 96,
                0,
                0,
                0,
                FW_SEMIBOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                OUT_DEFAULT_PRECIS.into(),
                0,
                0,
                (DEFAULT_PITCH | FF_DONTCARE).into(),
                wide("Microsoft YaHei UI").as_ptr(),
            )
        };
        let body_font = unsafe {
            CreateFontW(
                -14 * dpi / 96,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                OUT_DEFAULT_PRECIS.into(),
                0,
                0,
                (DEFAULT_PITCH | FF_DONTCARE).into(),
                wide("Microsoft YaHei UI").as_ptr(),
            )
        };
        let old = unsafe { windows_sys::Win32::Graphics::Gdi::SelectObject(dc, title_font) };
        unsafe { SetTextColor(dc, request.level.color()) };
        let repeat = if current.pending.repeats > 1 {
            format!(" · {} ×{}", request.level.label(), current.pending.repeats)
        } else {
            format!(" · {}", request.level.label())
        };
        let title = wide(&format!("{}{}", request.title, repeat));
        let mut title_rect = RECT {
            left: 24 * dpi / 96,
            top: 8 * dpi / 96,
            right: rect.right - 24 * dpi / 96,
            bottom: 38 * dpi / 96,
        };
        unsafe {
            DrawTextW(
                dc,
                title.as_ptr(),
                -1,
                &mut title_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            windows_sys::Win32::Graphics::Gdi::SelectObject(dc, body_font);
            SetTextColor(dc, rgb(235, 237, 240));
        }
        let body = wide(&request.message.replace(['\r', '\n'], " "));
        let mut body_rect = RECT {
            left: 24 * dpi / 96,
            top: 36 * dpi / 96,
            right: rect.right - 24 * dpi / 96,
            bottom: rect.bottom - 8 * dpi / 96,
        };
        unsafe {
            DrawTextW(
                dc,
                body.as_ptr(),
                -1,
                &mut body_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            windows_sys::Win32::Graphics::Gdi::SelectObject(dc, old);
            DeleteObject(title_font);
            DeleteObject(body_font);
        }
    });
    unsafe { EndPaint(hwnd, &paint) };
}

const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_timeouts_follow_severity() {
        assert_eq!(Level::Success.timeout(), Duration::from_secs(3));
        assert_eq!(Level::Info.timeout(), Duration::from_secs(3));
        assert_eq!(Level::Warning.timeout(), Duration::from_secs(6));
        assert_eq!(Level::Error.timeout(), Duration::from_secs(6));
    }
}
