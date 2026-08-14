# Main console UI (desktop edition)

`app.html` is a zero-build, single-file console loaded into WebView2 via wry `with_html`
when building with `--features desktop`. The tray edition does not embed this file.
It ships dark and light palettes (`html[data-theme]`) with a dark / light / follow-system
selector in the header; the choice persists in WebView2 `localStorage`.

IPC:
- JS → Rust: `window.ipc.postMessage(JSON.stringify({type, ...}))`
  - `{type: "command", id}` — tray-command equivalent (whitelisted IDs only)
  - `{type: "switch_route", index}`
  - `{type: "theme", mode: "dark"|"light"|"system"}` — drives the DWM title-bar theme
- Rust → JS: `window.__hr.applySnapshot(object)`

The `theme` message keeps the host window frame (title bar, system chrome) in
sync with the HTML palette; `system` resolves against Windows app mode via the
`AppsUseLightTheme` registry value and re-applies on `WM_SETTINGCHANGE`.
