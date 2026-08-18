//! Host-window (non-client) theme driven by the console UI's theme choice.
//! The WebView client area is colored by CSS; these helpers keep the title
//! bar and system chrome in sync via DWM immersive dark mode, probing the
//! registry for the system-wide app mode when the UI choice is "system".

use super::super::wide;

use std::ffi::c_void;
use std::ptr;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::core::BOOL;

/// Theme choice reported by the WebView console for the host frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostTheme {
    Light,
    Dark,
    System,
}

pub(super) fn parse_host_theme(mode: &str) -> Option<HostTheme> {
    match mode {
        "light" => Some(HostTheme::Light),
        "dark" => Some(HostTheme::Dark),
        "system" => Some(HostTheme::System),
        _ => None,
    }
}

/// Applies immersive dark mode to the non-client title bar. Attribute 20 is
/// the documented DWMWA_USE_IMMERSIVE_DARK_MODE; older Win10 builds only
/// honor the legacy attribute slot 19, so both are set with the same value.
pub(super) unsafe fn apply_host_theme(hwnd: HWND, theme: HostTheme) {
    if hwnd.is_null() {
        return;
    }
    let dark = BOOL::from(match theme {
        HostTheme::Dark => true,
        HostTheme::Light => false,
        HostTheme::System => system_app_is_dark(),
    });
    // SAFETY: both calls pass a pointer to the stack BOOL valid for the call.
    for attribute in [20u32, 19u32] {
        let _ = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                (&dark as *const BOOL).cast::<c_void>(),
                std::mem::size_of::<BOOL>() as u32,
            )
        };
    }
    super::super::popup_theme::remember_console_theme(match theme {
        HostTheme::Dark => "dark",
        HostTheme::Light => "light",
        HostTheme::System => "system",
    });
    super::super::popup_theme::apply_to_menus(hwnd);
}

/// True when Windows is in dark app mode (`AppsUseLightTheme` is 0).
/// Missing values or registry errors fall back to light mode.
fn system_app_is_dark() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("AppsUseLightTheme");
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&mut value as *mut u32).cast::<c_void>(),
            &mut size,
        )
    };
    status == 0 && value == 0
}

/// Compares two null-terminated UTF-16 strings by pointer.
unsafe fn wcs_equal(a: *const u16, b: *const u16) -> bool {
    let mut index = 0usize;
    loop {
        // SAFETY: both pointers are valid null-terminated wide strings.
        let ca = unsafe { *a.add(index) };
        let cb = unsafe { *b.add(index) };
        if ca != cb {
            return false;
        }
        if ca == 0 {
            return true;
        }
        index += 1;
    }
}

/// Re-applies the system-following frame theme when Windows reports that the
/// app color scheme changed (`WM_SETTINGCHANGE` with "ImmersiveColorSet").
pub(super) unsafe fn refresh_system_theme(hwnd: HWND, setting: *const u16) {
    if hwnd.is_null() || setting.is_null() {
        return;
    }
    let target = wide("ImmersiveColorSet");
    // SAFETY: strings are valid null-terminated wide strings.
    if unsafe { wcs_equal(setting, target.as_ptr()) } {
        // SAFETY: hwnd is a live HWND on the UI thread.
        unsafe { apply_host_theme(hwnd, HostTheme::System) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_supported_modes() {
        assert_eq!(parse_host_theme("dark"), Some(HostTheme::Dark));
        assert_eq!(parse_host_theme("light"), Some(HostTheme::Light));
        assert_eq!(parse_host_theme("system"), Some(HostTheme::System));
        assert_eq!(parse_host_theme("blue"), None);
        assert_eq!(parse_host_theme(""), None);
    }

    #[test]
    fn wcs_equality_matches_null_terminated_strings() {
        let a = wide("ImmersiveColorSet");
        let b = wide("ImmersiveColorSet");
        let c = wide("ImmersiveColorset");
        let d = wide("ImmersiveColorSetX");
        unsafe {
            assert!(wcs_equal(a.as_ptr(), b.as_ptr()));
            assert!(!wcs_equal(a.as_ptr(), c.as_ptr()));
            assert!(!wcs_equal(a.as_ptr(), d.as_ptr()));
        }
    }
}
