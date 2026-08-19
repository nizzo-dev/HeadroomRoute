//! Compile-time product edition. Tray and desktop share one crate; only the
//! shell (start-up window, main-console host, update ZIP name) differs.

#[cfg(feature = "desktop")]
pub const EDITION: &str = "desktop";
#[cfg(not(feature = "desktop"))]
pub const EDITION: &str = "tray";

/// Windows Run-key autostart; desktop edition stays in the tray.
pub const AUTOSTART_ARG: &str = "--autostart";

pub fn show_window_on_start() -> bool {
    cfg!(feature = "desktop") && !is_autostart_launch(std::env::args())
}

pub fn is_autostart_launch<S: AsRef<str>>(args: impl IntoIterator<Item = S>) -> bool {
    args.into_iter().any(|arg| arg.as_ref() == AUTOSTART_ARG)
}

pub fn release_archive_name(version: &str) -> String {
    match EDITION {
        "desktop" => format!("HeadroomRoute-{version}-desktop-windows-x64.zip"),
        _ => format!("HeadroomRoute-{version}-windows-x64.zip"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn archive_name_matches_compiled_edition() {
        let name = super::release_archive_name("1.2.3");
        if cfg!(feature = "desktop") {
            assert_eq!(name, "HeadroomRoute-1.2.3-desktop-windows-x64.zip");
        } else {
            assert_eq!(name, "HeadroomRoute-1.2.3-windows-x64.zip");
        }
    }

    #[test]
    fn desktop_opens_window_on_start() {
        assert!(super::is_autostart_launch(["--autostart"]));
        assert!(!super::is_autostart_launch(["--doctor"]));
        assert_eq!(
            super::show_window_on_start(),
            cfg!(feature = "desktop") && !super::is_autostart_launch(std::env::args())
        );
        assert_eq!(super::EDITION == "desktop", cfg!(feature = "desktop"));
    }
}
