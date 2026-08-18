//! Dark/light styling for native Win32 popup menus (tray right-click).
//! uxtheme ordinals: 133 AllowDarkModeForWindow, 135 SetPreferredAppMode,
//! 136 FlushMenuThemes (Win10 1809+).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::BOOL;

const MODE_ALLOW_DARK: i32 = 1;
const MODE_FORCE_DARK: i32 = 2;
const MODE_FORCE_LIGHT: i32 = 3;

static PREFERRED: AtomicI32 = AtomicI32::new(MODE_ALLOW_DARK);

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();
type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;

struct UxTheme {
    set_preferred_app_mode: SetPreferredAppMode,
    flush_menu_themes: FlushMenuThemes,
    allow_dark_mode_for_window: Option<AllowDarkModeForWindow>,
}

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn remember_console_theme(mode: &str) {
    if let Some(preferred) = preferred_app_mode_for(mode) {
        PREFERRED.store(preferred, Ordering::Release);
    }
}

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn preferred_app_mode_for(mode: &str) -> Option<i32> {
    match mode {
        "dark" => Some(MODE_FORCE_DARK),
        "light" => Some(MODE_FORCE_LIGHT),
        "system" => Some(MODE_ALLOW_DARK),
        _ => None,
    }
}

/// Applies the remembered console theme to subsequent Win32 popup menus.
///
/// # Safety
/// Must run on the UI thread. `hwnd` is the menu owner, or null to skip
/// per-window dark-mode opt-in.
pub unsafe fn apply_to_menus(hwnd: HWND) {
    let Some(api) = uxtheme() else {
        return;
    };
    let preferred = PREFERRED.load(Ordering::Acquire);
    unsafe {
        (api.set_preferred_app_mode)(preferred);
        if !hwnd.is_null()
            && let Some(allow) = api.allow_dark_mode_for_window
        {
            let dark = BOOL::from(preferred != MODE_FORCE_LIGHT);
            let _ = allow(hwnd, dark);
        }
        (api.flush_menu_themes)();
    }
}

fn uxtheme() -> Option<&'static UxTheme> {
    static API: OnceLock<Option<UxTheme>> = OnceLock::new();
    API.get_or_init(load_uxtheme).as_ref()
}

fn load_uxtheme() -> Option<UxTheme> {
    let name: Vec<u16> = "uxtheme.dll".encode_utf16().chain(Some(0)).collect();
    let module = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return None;
    }
    let set_preferred_app_mode = unsafe { ordinal_proc(module, 135)? };
    let flush_menu_themes = unsafe { ordinal_proc(module, 136)? };
    let allow_dark_mode_for_window = unsafe { ordinal_proc(module, 133) };
    Some(UxTheme {
        set_preferred_app_mode: unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(
                set_preferred_app_mode,
            )
        },
        flush_menu_themes: unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(
                flush_menu_themes,
            )
        },
        allow_dark_mode_for_window: allow_dark_mode_for_window.map(|proc| unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, AllowDarkModeForWindow>(
                proc,
            )
        }),
    })
}

unsafe fn ordinal_proc(
    module: windows_sys::Win32::Foundation::HMODULE,
    ordinal: u16,
) -> Option<unsafe extern "system" fn() -> isize> {
    // MAKEINTRESOURCEA(ordinal): low-word integer passed as a PCSTR.
    unsafe { GetProcAddress(module, ordinal as usize as *const u8) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_console_theme_to_preferred_app_mode() {
        assert_eq!(preferred_app_mode_for("dark"), Some(MODE_FORCE_DARK));
        assert_eq!(preferred_app_mode_for("light"), Some(MODE_FORCE_LIGHT));
        assert_eq!(preferred_app_mode_for("system"), Some(MODE_ALLOW_DARK));
        assert_eq!(preferred_app_mode_for("blue"), None);
        remember_console_theme("dark");
        assert_eq!(PREFERRED.load(Ordering::Acquire), MODE_FORCE_DARK);
        remember_console_theme("system");
        assert_eq!(PREFERRED.load(Ordering::Acquire), MODE_ALLOW_DARK);
    }
}
