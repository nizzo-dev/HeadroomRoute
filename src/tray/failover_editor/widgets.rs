use super::*;

#[allow(clippy::too_many_arguments, unsafe_op_in_unsafe_fn)]
pub(crate) unsafe fn editor_control(
    parent: HWND,
    class: &str,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: usize,
    style: u32,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    let control = CreateWindowExW(
        if class == "LISTBOX" {
            WS_EX_CLIENTEDGE
        } else {
            0
        },
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        style,
        x,
        y,
        width,
        height,
        parent,
        id as _,
        instance,
        ptr::null(),
    );
    SendMessageW(control, WM_SETFONT, font, 1);
    control
}

/// 预检报告只读编辑框专用：仅此处给 EDIT 加 `WS_EX_CLIENTEDGE` 边框，
/// 不影响既有故障转移编辑器的控件外观。
#[allow(clippy::too_many_arguments, unsafe_op_in_unsafe_fn)]
pub(crate) unsafe fn precheck_report_edit(
    parent: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: usize,
    style: u32,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    font: usize,
) -> HWND {
    let control = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        wide("EDIT").as_ptr(),
        wide("").as_ptr(),
        style,
        x,
        y,
        width,
        height,
        parent,
        id as _,
        instance,
        ptr::null(),
    );
    SendMessageW(control, WM_SETFONT, font, 1);
    control
}
