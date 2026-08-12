# Main console UI

`app.html` is a zero-build, single-file dark console loaded into WebView2 via wry `with_html`.

IPC:
- JS → Rust: `window.ipc.postMessage(JSON.stringify({type, ...}))`
- Rust → JS: `window.__hr.applySnapshot(object)`
