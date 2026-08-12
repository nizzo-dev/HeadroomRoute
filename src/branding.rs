//! Brand logo icons loaded from embedded `.ico` bytes.
//! Used for tray notification icon and main window title-bar / taskbar icon.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, IMAGE_FLAGS, LookupIconIdFromDirectoryEx,
};

/// Dark-surface mark (white logo) — tray + dark UI default.
static ICON_DARK_ICO: &[u8] = include_bytes!("../assets/branding/headroomroute-dark.ico");
/// Light-surface mark (black logo) — available if needed later.
#[allow(dead_code)]
static ICON_LIGHT_ICO: &[u8] = include_bytes!("../assets/branding/headroomroute-light.ico");

thread_local! {
    static CACHED_TRAY: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    static CACHED_BIG: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
    static CACHED_SMALL: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

const ICON_VERSION: u32 = 0x0003_0000;

/// Create an HICON from an in-memory `.ico`, preferring `cx`×`cy`.
/// Caller owns the handle and must `DestroyIcon` unless it is a shared cache entry.
pub(crate) unsafe fn icon_from_ico_bytes(bytes: &[u8], cx: i32, cy: i32) -> *mut c_void {
    if bytes.is_empty() {
        return ptr::null_mut();
    }
    // LookupIconIdFromDirectoryEx returns offset into the resource directory.
    let offset = LookupIconIdFromDirectoryEx(bytes.as_ptr(), 1, cx, cy, 0u32);
    if offset <= 0 || (offset as usize) >= bytes.len() {
        // Fall back: try treating the whole blob (works for some single-image ICOs).
        return CreateIconFromResourceEx(
            bytes.as_ptr(),
            bytes.len() as u32,
            1,
            ICON_VERSION,
            cx,
            cy,
            0u32 as IMAGE_FLAGS,
        );
    }
    let start = offset as usize;
    CreateIconFromResourceEx(
        bytes[start..].as_ptr(),
        (bytes.len() - start) as u32,
        1,
        ICON_VERSION,
        cx,
        cy,
        0u32 as IMAGE_FLAGS,
    )
}

fn cached_or_create(slot: &Cell<*mut c_void>, bytes: &[u8], cx: i32, cy: i32) -> *mut c_void {
    let existing = slot.get();
    if !existing.is_null() {
        return existing;
    }
    let icon = unsafe { icon_from_ico_bytes(bytes, cx, cy) };
    if !icon.is_null() {
        slot.set(icon);
    }
    icon
}

/// Tray-sized icon (16×16). Cached for process lifetime; do **not** DestroyIcon the result
/// when using the cache — `notify_data` currently destroys after Shell_NotifyIcon, so we
/// return a **fresh** copy each call for tray to keep that contract.
pub(crate) fn tray_icon() -> *mut c_void {
    // Fresh instance each time: tray_window destroys hIcon after NIM_ADD/MODIFY.
    unsafe { icon_from_ico_bytes(ICON_DARK_ICO, 16, 16) }
}

/// Large window icon (~32×32) for taskbar / Alt-Tab. Cached; do not destroy.
pub(crate) fn window_icon_big() -> *mut c_void {
    CACHED_BIG.with(|slot| cached_or_create(slot, ICON_DARK_ICO, 32, 32))
}

/// Small window icon (16×16) for title bar. Cached; do not destroy.
pub(crate) fn window_icon_small() -> *mut c_void {
    CACHED_SMALL.with(|slot| cached_or_create(slot, ICON_DARK_ICO, 16, 16))
}

/// Optional explicit destroy of cached icons (process exit). Safe no-op if unused.
#[allow(dead_code)]
pub(crate) unsafe fn destroy_cached_icons() {
    for slot in [&CACHED_TRAY, &CACHED_BIG, &CACHED_SMALL] {
        slot.with(|cell| {
            let icon = cell.replace(ptr::null_mut());
            if !icon.is_null() {
                DestroyIcon(icon);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_ico_bytes_are_present() {
        assert!(ICON_DARK_ICO.len() > 100, "dark ico should be embedded");
        assert_eq!(&ICON_DARK_ICO[0..4], b"\x00\x00\x01\x00", "ICO magic");
    }

    #[test]
    fn can_create_tray_sized_icon() {
        let icon = unsafe { icon_from_ico_bytes(ICON_DARK_ICO, 16, 16) };
        assert!(!icon.is_null(), "CreateIconFromResourceEx should succeed for 16x16");
        unsafe { DestroyIcon(icon) };
    }
}
