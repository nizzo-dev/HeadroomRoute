//! Brand logo icons loaded from embedded `.ico` bytes.
//! Used for tray notification icon and main window title-bar / taskbar icon.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, IMAGE_FLAGS,
};

/// Dark-surface mark (white logo) — tray + dark UI default.
static ICON_DARK_ICO: &[u8] = include_bytes!("../assets/branding/headroomroute-dark.ico");
/// Light-surface mark (black logo) — available if needed later.
#[allow(dead_code)]
static ICON_LIGHT_ICO: &[u8] = include_bytes!("../assets/branding/headroomroute-light.ico");

thread_local! {
    // Keep every DPI-sized handle alive while a window or its registered class
    // may still reference it. DPI changes are rare, so this remains bounded.
    static CACHED_BIG: RefCell<Vec<(i32, *mut c_void)>> = const { RefCell::new(Vec::new()) };
    static CACHED_SMALL: RefCell<Vec<(i32, *mut c_void)>> = const { RefCell::new(Vec::new()) };
}

const ICON_VERSION: u32 = 0x0003_0000;

/// Create an HICON from an in-memory `.ico`, preferring `cx`×`cy`.
/// Caller owns the handle and must `DestroyIcon` unless it is a shared cache entry.
pub(crate) unsafe fn icon_from_ico_bytes(bytes: &[u8], cx: i32, cy: i32) -> *mut c_void {
    let Some(image) = ico_image_for_size(bytes, cx.max(cy)) else {
        return ptr::null_mut();
    };
    CreateIconFromResourceEx(
        image.as_ptr(),
        image.len() as u32,
        1,
        ICON_VERSION,
        cx,
        cy,
        0u32 as IMAGE_FLAGS,
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Select the closest ICO image, preferring downscaling over upscaling.
fn ico_image_for_size(bytes: &[u8], target: i32) -> Option<&[u8]> {
    if read_u16(bytes, 0)? != 0 || read_u16(bytes, 2)? != 1 {
        return None;
    }
    let count = read_u16(bytes, 4)? as usize;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = 6 + index * 16;
        let width = match *bytes.get(base)? {
            0 => 256,
            value => i32::from(value),
        };
        let height = match *bytes.get(base + 1)? {
            0 => 256,
            value => i32::from(value),
        };
        let length = read_u32(bytes, base + 8)? as usize;
        let offset = read_u32(bytes, base + 12)? as usize;
        let image = bytes.get(offset..offset.checked_add(length)?)?;
        entries.push((width.max(height), image));
    }
    entries.sort_by_key(|(size, _)| *size);
    entries
        .iter()
        .find(|(size, _)| *size >= target)
        .or_else(|| entries.last())
        .map(|(_, image)| *image)
}

fn icon_size_for_dpi(base: i32, dpi: u32) -> i32 {
    ((base as i64 * dpi.max(96) as i64 + 48) / 96) as i32
}

fn cached_or_create_sized(
    cache: &RefCell<Vec<(i32, *mut c_void)>>,
    bytes: &[u8],
    size: i32,
) -> *mut c_void {
    let mut cache = cache.borrow_mut();
    if let Some((_, icon)) = cache.iter().find(|(cached_size, _)| *cached_size == size) {
        return *icon;
    }
    let icon = unsafe { icon_from_ico_bytes(bytes, size, size) };
    if !icon.is_null() {
        cache.push((size, icon));
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

/// Large window icon for taskbar / Alt-Tab, sized to the system's DPI-scaled
/// icon metric (32 px at 100%, 48 px at 150%, 64 px at 200%) so Windows never
/// has to upscale a small bitmap on high-DPI displays. Cached per size.
pub(crate) fn window_icon_big_for_dpi(dpi: u32) -> *mut c_void {
    let size = icon_size_for_dpi(32, dpi);
    CACHED_BIG.with(|cache| cached_or_create_sized(cache, ICON_DARK_ICO, size))
}

/// Small window icon (16 px at 100% DPI) for the title bar. Cached per size.
pub(crate) fn window_icon_small_for_dpi(dpi: u32) -> *mut c_void {
    let size = icon_size_for_dpi(16, dpi);
    CACHED_SMALL.with(|cache| cached_or_create_sized(cache, ICON_DARK_ICO, size))
}

pub(crate) fn window_icon_big() -> *mut c_void {
    window_icon_big_for_dpi(unsafe { GetDpiForSystem() })
}

/// Optional explicit destroy of cached icons (process exit). Safe no-op if unused.
#[allow(dead_code)]
pub(crate) unsafe fn destroy_cached_icons() {
    let reset = |cache: &RefCell<Vec<(i32, *mut c_void)>>| {
        for (_, icon) in cache.borrow_mut().drain(..) {
            DestroyIcon(icon);
        }
    };
    CACHED_BIG.with(reset);
    CACHED_SMALL.with(reset);
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
        assert!(
            !icon.is_null(),
            "CreateIconFromResourceEx should succeed for 16x16"
        );
        unsafe { DestroyIcon(icon) };
    }

    #[test]
    fn can_create_dpi_sized_icon() {
        let icon = unsafe { icon_from_ico_bytes(ICON_DARK_ICO, 48, 48) };
        assert!(
            !icon.is_null(),
            "CreateIconFromResourceEx should succeed for 48x48 (150% DPI big icon)"
        );
        unsafe { DestroyIcon(icon) };
    }

    #[test]
    fn icon_sizes_scale_with_dpi() {
        assert_eq!(icon_size_for_dpi(16, 96), 16);
        assert_eq!(icon_size_for_dpi(16, 144), 24);
        assert_eq!(icon_size_for_dpi(32, 192), 64);
    }

    #[test]
    fn ico_selection_prefers_downscaling() {
        let image_40 = ico_image_for_size(ICON_DARK_ICO, 40).expect("40 px selection");
        let image_48 = ico_image_for_size(ICON_DARK_ICO, 48).expect("48 px layer");
        assert_eq!(image_40.as_ptr(), image_48.as_ptr());

        let image_300 = ico_image_for_size(ICON_DARK_ICO, 300).expect("largest layer");
        let image_256 = ico_image_for_size(ICON_DARK_ICO, 256).expect("256 px layer");
        assert_eq!(image_300.as_ptr(), image_256.as_ptr());
    }

    #[test]
    fn malformed_ico_is_rejected() {
        assert!(ico_image_for_size(&[], 16).is_none());
        assert!(ico_image_for_size(b"not an ico", 16).is_none());
        assert!(ico_image_for_size(&ICON_DARK_ICO[..20], 16).is_none());
    }

    #[test]
    fn big_window_icon_reuses_cached_handle() {
        let first = window_icon_big_for_dpi(144);
        assert!(!first.is_null(), "big window icon should exist");
        let second = window_icon_big_for_dpi(144);
        assert_eq!(first, second, "same DPI size must reuse the cached HICON");
    }
}
