# WebView2 主控制台 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the native Win32 tab console with a wry/WebView2 dark control panel that loads only while open, reuses existing tray commands, and keeps the app lightweight when the window is closed.

**Architecture:** Keep the hidden tray host and taskbar shell HWND. On show, create a `wry::WebView` child bound to the shell via raw `HWND` (`HasWindowHandle`). Frontend is zero-build HTML/CSS/JS embedded with `include_str!`. JS sends `{type,id}` IPC; Rust dispatches through `handle_command` / `switch_index`. On close, drop the WebView (destroy browser process) and hide the shell.

**Tech Stack:** Rust, `wry` (Windows/WebView2), `raw-window-handle`, existing `serde_json`, Win32 tray loop in `src/tray.rs`.

**Spec:** `docs/superpowers/specs/2026-08-12-webview-main-console-design.md`

## Global Constraints

- Windows only; Evergreen WebView2 runtime (do **not** bundle Fixed Runtime).
- Close path **must destroy** WebView; shell may stay hidden.
- No npm/build step; static `ui/main/*` only.
- Do not rewrite precheck / failover / approval to WebView in this plan.
- API keys must not appear in default snapshot JSON.
- Preserve public tray APIs: `show_main_window`, `destroy_main_window`, `dialog_owner`, `tray_host_hwnd`, `refresh_main_window_if_visible`.
- Dangerous commands still use existing Rust `MessageBox` / `handle_command` paths; destroy-on-exit uses tray host HWND.

---

### Task 1: Dependencies + HWND handle wrapper

**Files:**
- Modify: `Cargo.toml`
- Create: `src/tray/main_window/hwnd_handle.rs` (or keep private in `main_window.rs` if tiny)
- Modify: `src/tray/main_window.rs` (module wiring later; this task only deps + compile probe)

**Interfaces:**
- Produces: `struct ShellWindow(HWND)` implementing `raw_window_handle::HasWindowHandle` / `HasDisplayHandle` for Windows.
- Consumes: none yet.

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` dependencies:

```toml
wry = { version = "0.53", default-features = false, features = ["webkit2gtk"] }
```

**Do not** copy that blindly — on this Windows-only package use Windows-appropriate features. Prefer:

```toml
wry = "0.53"
raw-window-handle = "0.6"
```

Pin to a version that resolves on the machine (`cargo add wry raw-window-handle` is fine). If `wry` pulls `tao` transitively that is acceptable; do **not** add a second event loop.

- [ ] **Step 2: Implement HWND wrapper**

```rust
use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowsDisplayHandle, WindowsWindowHandle, HandleError, WindowHandle, DisplayHandle,
};
use std::num::NonZeroIsize;
use windows_sys::Win32::Foundation::HWND;

pub(super) struct ShellWindow(pub HWND);

impl HasWindowHandle for ShellWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let hwnd = NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?;
        let handle = WindowsWindowHandle::new(hwnd);
        // SAFETY: handle only used while shell HWND is alive on UI thread.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Windows(handle)) })
    }
}

impl HasDisplayHandle for ShellWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let handle = WindowsDisplayHandle::new();
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Windows(handle)) })
    }
}
```

Adjust exact `raw-window-handle` 0.6 constructors to match the resolved crate docs if names differ slightly.

- [ ] **Step 3: Cargo check deps**

Run: `cargo check --bin HeadroomRoute`

Expected: compiles (wrapper may be `dead_code` until later tasks).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/tray/main_window.rs
git commit -m "build: add wry and HWND handle wrapper for WebView console"
```

---

### Task 2: UI DTO + IPC parse (pure Rust, TDD)

**Files:**
- Create: `src/tray/main_window/ui_model.rs`
- Modify: `src/tray/tests.rs` (or `ui_model` inline tests)
- Modify: `src/tray/main_window.rs` to `mod ui_model;`

**Interfaces:**
- Produces:
  - `UiInbound` enum: `Ready`, `Command { id: usize }`, `SwitchRoute { index: usize }`
  - `fn parse_ui_message(body: &str) -> Option<UiInbound>`
  - `UiSnapshot` serde struct (frontend-facing, no API keys)
  - `fn build_ui_snapshot(app: &AppState) -> UiSnapshot` (or from `Snapshot` + extras)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn parses_command_and_switch_messages() {
    use super::ui_model::{parse_ui_message, UiInbound};
    assert!(matches!(
        parse_ui_message(r#"{"type":"command","id":102}"#),
        Some(UiInbound::Command { id: 102 })
    ));
    assert!(matches!(
        parse_ui_message(r#"{"type":"switch_route","index":3}"#),
        Some(UiInbound::SwitchRoute { index: 3 })
    ));
    assert!(matches!(
        parse_ui_message(r#"{"type":"ready"}"#),
        Some(UiInbound::Ready)
    ));
    assert!(parse_ui_message("not-json").is_none());
    assert!(parse_ui_message(r#"{"type":"command","id":-1}"#).is_none());
}

#[test]
fn ui_snapshot_omits_api_keys() {
    // Build a minimal Snapshot or call build_ui_snapshot with test AppState if feasible.
    // Assert serialized JSON does not contain "api_key" / "sk-".
}
```

If constructing full `AppState` is heavy, unit-test only `parse_ui_message` + a pure `UiSnapshot::from_parts(...)` helper.

- [ ] **Step 2: Run tests — expect fail**

Run: `cargo test --bin HeadroomRoute parses_command_and_switch_messages`

Expected: FAIL (module/function missing).

- [ ] **Step 3: Implement parser + DTO**

```rust
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiInbound {
    Ready,
    Command { id: usize },
    SwitchRoute { index: usize },
}

pub fn parse_ui_message(body: &str) -> Option<UiInbound> {
    serde_json::from_str(body).ok()
}

#[derive(Debug, Serialize)]
pub struct UiSnapshot {
    pub runtime_mode: String,
    pub runtime_reason: String,
    // ... fields needed by the four pages; routes without api_key
    pub recommended: Option<UiRecommended>,
    pub start_with_windows: bool,
    pub sync_in_progress: bool,
    pub restart_in_progress: bool,
    pub update_running: bool,
}

#[derive(Debug, Serialize)]
pub struct UiRecommended {
    pub id: usize,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct UiRoute {
    pub index: usize,
    pub protocol: String, // "openai" | "anthropic"
    pub name: String,
    pub provider: String,
    pub latency_ms: Option<u64>,
    pub evidence: String,
    pub selected: bool,
}
```

Map from `app.snapshot()`, `app.recovery_hint()`, `recommended_action(...)`, flags from atomics/`config.start_with_windows`. **Strip** `Route.api_key`.

- [ ] **Step 4: Tests pass**

Run: `cargo test --bin HeadroomRoute parses_command`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tray/main_window/ui_model.rs src/tray/main_window.rs src/tray/tests.rs
git commit -m "feat: add WebView UI snapshot DTO and IPC parser"
```

---

### Task 3: Static frontend skeleton (HTML/CSS/JS)

**Files:**
- Create: `ui/main/index.html`
- Create: `ui/main/app.css`
- Create: `ui/main/app.js`

**Interfaces:**
- Produces: page that defines `window.__hr = { applySnapshot(json) }` and sends IPC via `window.ipc.postMessage(JSON.stringify(...))` (wry default) — verify exact global name against resolved wry version (`window.ipc` vs `window.chrome.webview`).
- If wry injects `window.ipc.postMessage`, use that; document the chosen API in a one-line comment at top of `app.js`.

- [ ] **Step 1: HTML shell with four tabs**

```html
<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:;" />
  <title>Headroom Route</title>
  <style>/* in dev can link app.css; release embeds combined HTML */</style>
</head>
<body>
  <header class="top">
    <div class="brand">Headroom Route</div>
    <div id="mode-pill" class="pill">—</div>
  </header>
  <nav class="tabs" role="tablist">
    <button data-tab="status" class="active">状态</button>
    <button data-tab="routes">上游</button>
    <button data-tab="ops">运维</button>
    <button data-tab="settings">设置</button>
  </nav>
  <main>
    <section id="tab-status" class="panel active">...</section>
    <section id="tab-routes" class="panel">...</section>
    <section id="tab-ops" class="panel">...</section>
    <section id="tab-settings" class="panel">...</section>
  </main>
  <script src is not used — inline or single file strategy>
</script>
</body>
</html>
```

**Embedding strategy (pick one and stick to it):**

**Recommended:** one `index.html` with inlined CSS+JS via build-time concatenation in Rust:

```rust
const UI_HTML: &str = concat!(
    include_str!("../../../ui/main/index.head.html"),
    "<style>",
    include_str!("../../../ui/main/app.css"),
    "</style>",
    include_str!("../../../ui/main/index.body.html"),
    "<script>",
    include_str!("../../../ui/main/app.js"),
    "</script></body></html>"
);
```

Or simpler for v1: **single `ui/main/app.html`** file containing CSS+JS inline (easiest `with_html`). Prefer single file if splitting confuses paths.

- [ ] **Step 2: Dark CSS**

Use system font stack, dark background `#12141a`, cards, status dots (green/amber/red), compact button rows. No external fonts/CDN.

- [ ] **Step 3: JS applySnapshot + actions**

```js
function post(msg) {
  const data = JSON.stringify(msg);
  if (window.ipc && window.ipc.postMessage) window.ipc.postMessage(data);
  else if (window.chrome && window.chrome.webview) window.chrome.webview.postMessage(data);
}

window.__hr = {
  applySnapshot(snapshot) {
    // update texts, route list, checkbox states, disable sync/restart when in progress
  }
};

document.querySelectorAll("nav.tabs button").forEach((btn) => {
  btn.addEventListener("click", () => {
    /* toggle .active panels */
  });
});

// Example bindings:
// data-command="102" buttons -> post({type:"command", id: Number(...) })
// route rows -> post({type:"switch_route", index})
post({ type: "ready" });
```

- [ ] **Step 4: Manual browser sanity (optional)**

Open the HTML in Edge to check layout without Rust. No automated test required.

- [ ] **Step 5: Commit**

```bash
git add ui/main
git commit -m "feat: add static dark console HTML for WebView shell"
```

---

### Task 4: Replace native controls with WebView lifecycle

**Files:**
- Modify: `src/tray/main_window.rs` (major rewrite)
- Possibly split: `src/tray/main_window/webview.rs`
- Modify: `src/tray.rs` only if exports change (prefer keep signatures)

**Interfaces:**
- Keep: `register_main_window_class`, `create_main_window`, `show_main_window`, `destroy_main_window`, `refresh_main_window_if_visible`, `main_hwnd`, `dialog_owner`, `set_tray_host_hwnd`, `tray_host_hwnd`
- Internal: `Option<wry::WebView>` stored in `MainWindowState` or `RefCell`/mutex on UI thread only (tray is single-threaded UI — `RefCell` or plain `Option` in state box is enough).

- [ ] **Step 1: Slim shell state**

Remove Tab/control creation. State becomes:

```rust
struct MainWindowState {
    webview: Option<wry::WebView>,
}
```

`WM_CREATE`: no native children (or a fill static optional).  
`WM_SIZE`: if webview exists, `webview.set_bounds(full client rect)` if API requires (Windows wry often auto-resizes; verify).  
`WM_CLOSE`: call `teardown_webview(state)`; `ShowWindow(SW_HIDE)`; return 0.  
`WM_DESTROY`: teardown webview; clear `MAIN_HWND` if needed.

- [ ] **Step 2: Create WebView on first show**

```rust
pub(super) unsafe fn show_main_window() {
    let hwnd = main_hwnd();
    if hwnd.is_null() || IsWindow(hwnd) == 0 { return; }
    ensure_webview(hwnd);
    // ShowWindow / SetForegroundWindow as today
    push_snapshot(hwnd);
}

fn ensure_webview(hwnd: HWND) {
    let state = /* from GWLP_USERDATA */;
    if state.webview.is_some() { return; }
    let shell = ShellWindow(hwnd);
    let html = combined_ui_html();
    let builder = WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(|req| {
            // IMPORTANT: wry may call this off the UI thread depending on version.
            // Prefer PostMessage to tray/main hwnd with a custom WM_APP code carrying
            // a boxed String, then handle on UI thread.
            let body = req.body().clone();
            post_ui_ipc_to_main(body);
        });
    match builder.build(&shell) {
        Ok(wv) => state.webview = Some(wv),
        Err(err) => {
            notification::error(
                "无法打开主窗口",
                format!("需要 Microsoft Edge WebView2 Runtime。\n{err}"),
            );
        }
    }
}

fn teardown_webview(state: &mut MainWindowState) {
    state.webview.take(); // drop => destroy
}
```

**IPC threading:** Implement `WM_UI_IPC = WM_APP + 40` on **main shell** or tray host:

```rust
// poster (any thread):
let leaked = Box::into_raw(Box::new(body));
PostMessageW(main_hwnd, WM_UI_IPC, 0, leaked as LPARAM);

// UI thread handler:
let body = *Box::from_raw(lparam as *mut String);
dispatch_ui_message(&body);
```

- [ ] **Step 3: dispatch_ui_message**

```rust
fn dispatch_ui_message(body: &str) {
    match parse_ui_message(body) {
        Some(UiInbound::Ready) => push_snapshot(main_hwnd()),
        Some(UiInbound::Command { id }) => {
            if !is_allowed_ui_command(id) { return; }
            handle_command_for_ui(main_hwnd(), id);
            push_snapshot(main_hwnd());
        }
        Some(UiInbound::SwitchRoute { index }) => {
            if let Some(app) = APP.get() {
                if app.switch_index(index, "主窗口手动切换") {
                    let _ = app.write_status();
                }
            }
            push_snapshot(main_hwnd());
        }
        None => {}
    }
}

fn is_allowed_ui_command(id: usize) -> bool {
    matches!(id, ID_CHECK | ID_SYNC | /* full whitelist from design */)
        || (ID_ROUTE_BASE..ID_ROUTE_BASE+64).contains(&id) // if ever used
}
```

Reuse existing `handle_command_for_ui` destroy-id routing to tray host.

- [ ] **Step 4: push_snapshot**

```rust
fn push_snapshot(hwnd: HWND) {
    let Some(app) = APP.get() else { return };
    let state = /* ... */;
    let Some(wv) = state.webview.as_ref() else { return };
    let snap = build_ui_snapshot(app);
    let json = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
    // escape for JS string carefully — prefer serde_json::to_string then:
    let script = format!(
        "window.__hr && window.__hr.applySnapshot({});",
        json // json is already a JS object literal if inserted raw
    );
    let _ = wv.evaluate_script(&script);
}
```

- [ ] **Step 5: refresh_main_window_if_visible**

Only `push_snapshot` when visible && webview present (no-op if destroyed).

- [ ] **Step 6: Remove InitCommonControls tab dependency** if unused; keep if precheck still needs common controls elsewhere (failover already uses them — InitCommonControls may still be needed globally; can init once in `tray::run` instead of main window).

- [ ] **Step 7: cargo check + existing tests**

Run: `cargo test --bin HeadroomRoute`

Expected: PASS.

- [ ] **Step 8: Manual smoke**

1. Kill old instance; `cargo run --bin HeadroomRoute`
2. Left-click tray → dark WebView UI
3. Close X → confirm no lasting WebView CPU (Task Manager)
4. Reopen → UI works

- [ ] **Step 9: Commit**

```bash
git add src/tray/main_window.rs src/tray/main_window/ ui/main Cargo.toml Cargo.lock
git commit -m "feat: host main console in WebView2 with destroy-on-close"
```

---

### Task 5: Wire full four-page actions + polish

**Files:**
- Modify: `ui/main/*`
- Modify: `src/tray/main_window/ui_model.rs` (fields as needed)
- Modify: `README.md` (WebView2 prerequisite one-liner)

**Interfaces:**
- Every former native button has a `data-command` id matching `src/tray.rs` constants.
- Settings grid includes precheck/failover via command ids that open native dialogs (`ID_PRECHECK`, `ID_FAILOVER_EDITOR`) with `dialog_owner`.

- [ ] **Step 1: Complete JS bindings for all whitelisted commands**

Whitelist must include at least:  
`ID_CHECK, ID_SYNC, ID_RESTART, ID_AUTO, ID_BYPASS, ID_MANAGE_UPSTREAM, ID_FAILOVER_EDITOR, ID_STARTUP, ID_AUTO_UPDATE, ID_SHOW_API_KEY, ID_CONFIG, ID_LOGS, ID_DIAG, ID_PRECHECK, ID_RESET_METRICS, ID_TAKEOVER, ID_CREATE_BACKUP, ID_RESTORE_BACKUP, ID_EXPORT_PORTABLE, ID_IMPORT_PORTABLE, ID_DIAGNOSTIC_ZIP, ID_PROVIDER_IDS, ID_RELOAD_FAILOVER, ID_UPDATE, ID_REPAIR_RUNTIME, ID_SELECT_RUNTIME, ID_RESTORE, ID_UNINSTALL, ID_APPROVAL_DEMO`  
and recommended action ids dynamically.

- [ ] **Step 2: Route list UX**

Render routes grouped by protocol; click/double-click → `switch_route`.

- [ ] **Step 3: Checkbox sync**

`applySnapshot` sets checkbox `.checked` from snapshot; change events post `command` with toggle ids (Rust toggles still flip config — avoid double-toggle by using buttons or by reading desired state).

**Careful:** existing `ID_AUTO` handler **toggles**. Prefer:

- UI sends command on user click only;
- After snapshot, set checkbox to server state;
- Use `change` handler that only fires on user gesture.

- [ ] **Step 4: README note**

Under 前置环境 / 快速开始 add:

> 主控制台需要本机已安装 **Microsoft Edge WebView2 Runtime**（Windows 11 通常已具备）。仅托盘可在无 Runtime 时运行；打开主窗口时若缺失会提示。

- [ ] **Step 5: Record size**

```bash
cargo build --release --bin HeadroomRoute
ls -la target/release/HeadroomRoute.exe
```

Note size in commit message body or README optional.

- [ ] **Step 6: Full test + manual checklist from spec**

Run: `cargo test`

Manual: open/close memory, commands, native dialogs from settings, tray exit.

- [ ] **Step 7: Commit**

```bash
git add ui/main src/tray/main_window README.md
git commit -m "feat: complete WebView console pages and document WebView2 requirement"
```

---

### Task 6: Cleanup native dead code + regression guard

**Files:**
- Modify: `src/tray/main_window.rs` (delete leftover tab constants if any)
- Modify: `src/tray/tests.rs` add bridge tests if not already
- Modify: remove unused imports in `tray.rs`

- [ ] **Step 1: Delete unused Win32 tab control paths** from main_window (already replaced).

- [ ] **Step 2: Ensure `route_menu` dead_code still allowed or remove if permanently unused.**

- [ ] **Step 3: cargo test + clippy-ish check**

Run: `cargo test` and `cargo check`

- [ ] **Step 4: Final commit**

```bash
git add -u src/tray ui README.md
git commit -m "chore: remove native console remnants after WebView migration"
```

---

## Spec coverage check

| Spec item | Task |
|---|---|
| wry + static frontend | 1, 3, 4 |
| destroy WebView on close | 4 |
| shell hide, tray unchanged | 4 |
| four pages | 3, 5 |
| command bridge / no key leak | 2, 5 |
| Evergreen only + missing runtime UX | 4, 5 |
| precheck/failover stay native | 5 (command opens existing) |
| volume/memory expectations | 5 measure |

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-12-webview-main-console.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — this session runs tasks with checkpoints  

Which approach?
