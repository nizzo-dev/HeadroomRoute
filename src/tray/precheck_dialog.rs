use super::*;
use crate::precheck::{PrecheckAction, PrecheckReport};
use std::sync::atomic::{AtomicBool, Ordering};

mod precheck_wizard;

const ID_PRECHECK_SUMMARY: usize = 310;
const ID_PRECHECK_REPORT: usize = 311;
const ID_PRECHECK_RECHECK: usize = 312;
const ID_PRECHECK_COPY: usize = 313;
const ID_PRECHECK_CLOSE: usize = 314;
const ID_PRECHECK_ACTION_BASE: usize = 400;
const PRECHECK_TIMER: usize = 5;
/// Readonly multiline EDIT 走 WM_CTLCOLORSTATIC。必须用不透明背景，否则换行后的「说明」「建议」会叠字。
pub(super) const PRECHECK_REPORT_BK_MODE: i32 = OPAQUE as i32;
const PRECHECK_ACTION_SLOTS: [PrecheckAction; 3] = [
    PrecheckAction::SelectPython,
    PrecheckAction::SyncRoutes,
    PrecheckAction::OpenConfig,
];

/// Prevent duplicate dialogs from the tray menu and startup precheck.
static PRECHECK_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(super) enum PrecheckResult {
    Report(PrecheckReport),
    Failed,
}

pub(super) struct PrecheckDialog {
    parent: HWND,
    app: Arc<AppState>,
    report: Option<PrecheckReport>,
    collecting: bool,
    failed: bool,
    receiver: Option<mpsc::Receiver<PrecheckResult>>,
    body_font: usize,
    title_font: usize,
    layout: PrecheckLayout,
}

pub(super) fn precheck_action_label(action: PrecheckAction) -> &'static str {
    match action {
        PrecheckAction::SelectPython => "选择 Headroom Python...",
        PrecheckAction::SyncRoutes => "同步 Codex + Claude / CC-Switch",
        PrecheckAction::OpenConfig => "打开 config.json",
    }
}

/// 紧凑布局下的动作按钮短标签，保证极小工作区内文本不被裁切。
pub(super) fn precheck_action_compact_label(action: PrecheckAction) -> &'static str {
    match action {
        PrecheckAction::SelectPython => "选择",
        PrecheckAction::SyncRoutes => "同步",
        PrecheckAction::OpenConfig => "配置",
    }
}

/// 预检动作按钮槽位到现有托盘命令的映射，动作必须经用户点击后才执行。
pub(super) fn precheck_action_command(slot: usize) -> Option<usize> {
    match slot {
        0 => Some(ID_SELECT_RUNTIME),
        1 => Some(ID_SYNC),
        2 => Some(ID_CONFIG),
        _ => None,
    }
}

#[allow(unused_imports)]
use precheck_layout::{PrecheckLayout, precheck_layout, precheck_scale};

/// 预检窗口所在显示器：优先取 owner 窗口所在显示器的工作区，失败时回退到主显示器工作区。
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn precheck_work_area(owner: HWND) -> RECT {
    let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
    if !monitor.is_null() {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            rcMonitor: std::mem::zeroed(),
            rcWork: std::mem::zeroed(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            return info.rcWork;
        }
    }
    let mut work_area = RECT {
        left: 0,
        top: 0,
        right: GetSystemMetrics(SM_CXSCREEN),
        bottom: GetSystemMetrics(SM_CYSCREEN),
    };
    SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        &mut work_area as *mut RECT as *mut c_void,
        0,
    );
    work_area
}

/// 预检窗口所用 DPI：优先取 owner 窗口的 DPI，失败时回退到系统 DPI。
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn precheck_dpi(owner: HWND) -> u32 {
    if !owner.is_null() {
        let dpi = GetDpiForWindow(owner);
        if dpi != 0 {
            return dpi;
        }
    }
    GetDpiForSystem().max(96)
}

/// 打开启动预检向导。收集在后台线程运行；关闭窗口不会修改任何配置。
#[allow(unsafe_op_in_unsafe_fn)]
pub(super) unsafe fn show_precheck(parent: HWND) {
    if PRECHECK_ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(app) = APP.get().cloned() else {
        PRECHECK_ACTIVE.store(false, Ordering::Release);
        return;
    };
    let instance = GetModuleHandleW(ptr::null());
    let ex_style = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE;
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    AdjustWindowRectEx(&mut frame, style, 0, ex_style);
    let layout = precheck_layout(
        precheck_dpi(parent),
        precheck_work_area(parent),
        frame.right - frame.left,
        frame.bottom - frame.top,
    );
    let dialog = Box::new(PrecheckDialog {
        parent,
        app,
        report: None,
        collecting: true,
        failed: false,
        receiver: None,
        body_font: 0,
        title_font: 0,
        layout,
    });
    EnableWindow(parent, 0);
    let raw = Box::into_raw(dialog);
    let class_name = wide("HeadroomRoutePrecheck");
    let title = wide("启动预检");
    let window = CreateWindowExW(
        ex_style,
        class_name.as_ptr(),
        title.as_ptr(),
        style,
        layout.window_x,
        layout.window_y,
        layout.window_width,
        layout.window_height,
        parent,
        ptr::null_mut(),
        instance,
        raw.cast(),
    );
    if window.is_null() {
        // CreateWindowExW 失败时 Box 仍归本处；成功后由 WM_NCDESTROY 释放，两条路径互斥。
        drop(Box::from_raw(raw));
        EnableWindow(parent, 1);
        PRECHECK_ACTIVE.store(false, Ordering::Release);
        notify(
            parent,
            "预检窗口创建失败",
            "无法打开启动预检界面，请稍后重试",
        );
        return;
    }
    // 自动打开时进程常非前台；先 TOPMOST 再取消，避免预检窗落到控制台后面。
    SetWindowPos(
        window,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
    SetWindowPos(window, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    SetForegroundWindow(window);
    let mut message: MSG = std::mem::zeroed();
    while IsWindow(window) != 0 && GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
        if IsDialogMessageW(window, &message) == 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // 模态循环可能因 WM_QUIT 退出而窗口仍在；销毁以保证 WM_NCDESTROY 一定执行。
    if IsWindow(window) != 0 {
        DestroyWindow(window);
    }
}

// The dialog Box is installed during WM_NCCREATE and released exactly once at WM_NCDESTROY.
#[allow(unsafe_op_in_unsafe_fn)]
pub(super) unsafe extern "system" fn precheck_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let dialog = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PrecheckDialog;
    match message {
        WM_CREATE => {
            (*dialog).create_controls(hwnd);
            (*dialog).start_collect(hwnd);
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            // Readonly EDIT 发 WM_CTLCOLORSTATIC。透明背景会让「说明」「建议」叠字。
            SetBkMode(wparam as _, PRECHECK_REPORT_BK_MODE);
            SetBkColor(wparam as _, GetSysColor(COLOR_WINDOW));
            SetTextColor(wparam as _, GetSysColor(COLOR_WINDOWTEXT));
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_TIMER if wparam == PRECHECK_TIMER => {
            (*dialog).poll_result(hwnd);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            let code = (wparam >> 16) & 0xffff;
            if code == BN_CLICKED as usize {
                if id == ID_PRECHECK_RECHECK {
                    (*dialog).start_collect(hwnd);
                } else if id == ID_PRECHECK_COPY {
                    (*dialog).copy_report(hwnd);
                } else if id == ID_PRECHECK_CLOSE {
                    DestroyWindow(hwnd);
                } else if (ID_PRECHECK_ACTION_BASE..ID_PRECHECK_ACTION_BASE + 3).contains(&id) {
                    let slot = id - ID_PRECHECK_ACTION_BASE;
                    if let Some(command) = precheck_action_command(slot) {
                        // 动作交给托盘窗口执行；预检窗保持打开，用户可再点「重新检测」。
                        unsafe { handle_command((*dialog).parent, command) };
                    }
                }
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, PRECHECK_TIMER);
            EnableWindow((*dialog).parent, 1);
            SetForegroundWindow((*dialog).parent);
            0
        }
        WM_NCDESTROY => {
            if (*dialog).body_font != 0 {
                DeleteObject((*dialog).body_font as _);
            }
            if (*dialog).title_font != 0 {
                DeleteObject((*dialog).title_font as _);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(dialog));
            PRECHECK_ACTIVE.store(false, Ordering::Release);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
impl PrecheckDialog {
    unsafe fn create_controls(&mut self, hwnd: HWND) {
        let stock_font = GetStockObject(DEFAULT_GUI_FONT) as usize;
        let instance = GetModuleHandleW(ptr::null());
        self.body_font = CreateFontW(
            self.layout.body_font,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let font = if self.body_font == 0 {
            stock_font
        } else {
            self.body_font
        };
        self.title_font = CreateFontW(
            self.layout.title_font,
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        ) as usize;
        let title_font = if self.title_font == 0 {
            font
        } else {
            self.title_font
        };
        let title = self.layout.title;
        let summary = self.layout.summary;
        let report = self.layout.report;
        let recheck = self.layout.recheck;
        let copy = self.layout.copy;
        let close = self.layout.close;
        editor_control(
            hwnd,
            "STATIC",
            "启动预检",
            title.x,
            title.y,
            title.width,
            title.height,
            0,
            WS_CHILD | WS_VISIBLE,
            instance,
            title_font,
        );
        editor_control(
            hwnd,
            "STATIC",
            "正在检测本地环境，请稍候...",
            summary.x,
            summary.y,
            summary.width,
            summary.height,
            ID_PRECHECK_SUMMARY,
            WS_CHILD | WS_VISIBLE,
            instance,
            font,
        );
        precheck_report_edit(
            hwnd,
            report.x,
            report.y,
            report.width,
            report.height,
            ID_PRECHECK_REPORT,
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | ES_MULTILINE as u32
                | ES_READONLY as u32
                | ES_AUTOVSCROLL as u32
                | WS_VSCROLL,
            instance,
            font,
        );
        for (slot, action) in PRECHECK_ACTION_SLOTS.into_iter().enumerate() {
            let rect = self.layout.actions[slot];
            let label = if self.layout.compact {
                precheck_action_compact_label(action)
            } else {
                precheck_action_label(action)
            };
            editor_control(
                hwnd,
                "BUTTON",
                label,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                ID_PRECHECK_ACTION_BASE + slot,
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                instance,
                font,
            );
        }
        editor_control(
            hwnd,
            "BUTTON",
            "重新检测",
            recheck.x,
            recheck.y,
            recheck.width,
            recheck.height,
            ID_PRECHECK_RECHECK,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "复制报告",
            copy.x,
            copy.y,
            copy.width,
            copy.height,
            ID_PRECHECK_COPY,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            instance,
            font,
        );
        editor_control(
            hwnd,
            "BUTTON",
            "关闭",
            close.x,
            close.y,
            close.width,
            close.height,
            ID_PRECHECK_CLOSE,
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            instance,
            font,
        );
        SetTimer(hwnd, PRECHECK_TIMER, 100, None);
        self.update_ui(hwnd);
    }

    unsafe fn start_collect(&mut self, hwnd: HWND) {
        self.report = None;
        self.failed = false;
        self.collecting = true;
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        let config = self.app.inner.lock().unwrap().config.clone();
        self.update_ui(hwnd);
        // 后台线程只持有克隆的 config 与 tx；窗口关闭后 send 失败，线程自行退出。
        let _ = thread::Builder::new()
            .name("headroom-precheck".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    precheck::collect(&config)
                }));
                let outcome = match result {
                    Ok(report) => PrecheckResult::Report(report),
                    Err(_) => PrecheckResult::Failed,
                };
                let _ = tx.send(outcome);
            });
    }

    unsafe fn poll_result(&mut self, hwnd: HWND) {
        if !self.collecting {
            return;
        }
        let outcome = match self.receiver.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(value) => Some(value),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Some(PrecheckResult::Failed),
            },
            None => return,
        };
        self.receiver = None;
        self.collecting = false;
        match outcome {
            Some(PrecheckResult::Report(report)) => {
                self.report = Some(report);
                self.failed = false;
            }
            Some(PrecheckResult::Failed) => {
                self.failed = true;
                notify(hwnd, "预检失败", "后台检测未完成，请点击“重新检测”重试");
            }
            None => {}
        }
        self.update_ui(hwnd);
    }

    unsafe fn update_ui(&mut self, hwnd: HWND) {
        let summary = if self.collecting {
            "正在检测本地环境，请稍候...".to_owned()
        } else if self.failed {
            "预检未完成，请点击“重新检测”重试。".to_owned()
        } else {
            self.report.as_ref().map_or_else(
                || "暂无预检结果。".to_owned(),
                |report| report.summary_line(),
            )
        };
        SetWindowTextW(
            GetDlgItem(hwnd, ID_PRECHECK_SUMMARY as i32),
            wide(&summary).as_ptr(),
        );
        let summary_control = GetDlgItem(hwnd, ID_PRECHECK_SUMMARY as i32);
        if !summary_control.is_null() {
            InvalidateRect(summary_control, ptr::null(), 1);
            UpdateWindow(summary_control);
        }
        let report_text = if self.collecting {
            "正在收集运行环境事实（读取本地配置并进行只读的 Headroom 版本验证），不会修改任何配置，也不会读取 API Key。".to_owned()
        } else {
            self.report
                .as_ref()
                .map_or_else(String::new, precheck_wizard::wizard_text)
        };
        SetWindowTextW(
            GetDlgItem(hwnd, ID_PRECHECK_REPORT as i32),
            wide(&report_text).as_ptr(),
        );
        let report_control = GetDlgItem(hwnd, ID_PRECHECK_REPORT as i32);
        if !report_control.is_null() {
            InvalidateRect(report_control, ptr::null(), 1);
            UpdateWindow(report_control);
        }
        let actions = self
            .report
            .as_ref()
            .map_or_else(Vec::new, precheck_wizard::wizard_actions);
        for (slot, action) in PRECHECK_ACTION_SLOTS.into_iter().enumerate() {
            let show = !self.collecting && !self.failed && actions.contains(&action);
            ShowWindow(
                GetDlgItem(hwnd, (ID_PRECHECK_ACTION_BASE + slot) as i32),
                if show { SW_SHOW } else { SW_HIDE },
            );
        }
        EnableWindow(
            GetDlgItem(hwnd, ID_PRECHECK_RECHECK as i32),
            if self.collecting { 0 } else { 1 },
        );
    }

    unsafe fn copy_report(&self, hwnd: HWND) {
        let Some(report) = self.report.as_ref() else {
            notify(hwnd, "预检报告不可用", "请等待检测完成后再复制");
            return;
        };
        match copy_clipboard(hwnd, &report.to_text()) {
            Ok(()) => notify(hwnd, "预检报告已复制", "报告不包含 API Key"),
            Err(error) => notify(hwnd, "复制预检报告失败", &error.to_string()),
        }
    }
}
