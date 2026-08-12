# WebView2 主控制台设计（轻量约束）

日期：2026-08-12  
状态：已确认方向，待实现计划

## 背景与动机

当前主控制台（`src/tray/main_window.rs`）是原生 Win32 控件（Tab + STATIC/BUTTON/LISTBOX/EDIT），功能完整但观感接近系统对话框，用户反馈「太丑」。

同时产品定位是**轻量 Windows 托盘工具**（当前 release 约 **2.1 MB**，无 Electron、无捆绑 Python）。不能为了 UI 退化成重型桌面壳。

## 目标

1. 主控制台视觉质量达到现代暗色控制台水准。
2. 保持轻量：不捆绑浏览器运行时；关窗后尽量释放 WebView 内存。
3. 业务逻辑不重写：继续复用 `AppState`、`handle_command`、现有 command id。
4. 托盘、预检、故障转移、确认悬浮窗首版仍原生。

## 非目标

- 不用 Electron / Tauri 整壳重写。
- 不引入 npm 构建链 / 前端框架（首版）。
- 不把预检 / 故障转移 / approval 一并 Web 化（可后续迭代）。
- 不支持非 Windows。
- 不关窗常驻 WebView 以换「秒开」。

## 已确认决策

| 项 | 选择 |
|---|---|
| 路线 | 有约束的 WebView2 主窗 |
| 库 | `wry`（底层 WebView2） |
| 前端 | 零构建：纯 HTML/CSS/JS，`include_str!` 进二进制 |
| 范围 | 仅主控制台四页 |
| 关窗 | **销毁 WebView**（更轻）；HWND 可 Hide 或一并 Destroy 后重建 |
| 子对话框 | 仍原生 Win32 |
| 托盘 | 仍原生 `Shell_NotifyIcon` |

## 体积与内存预期（诚实上限）

### 安装包 / exe

| 项 | 预期 |
|---|---|
| 当前 release exe | ~2.1 MB |
| 增加 `wry` + 少量静态前端 | 大约 **+1～3 MB** 量级（依赖压缩与 LTO；实现后以实测为准） |
| WebView2 **Evergreen** Runtime | **不打进安装包**；多数 Win10/11 已有 Edge 组件；缺失时引导系统安装器 |
| Fixed Version Runtime | **不采用**（会 +100 MB 级，违背轻量） |

### 运行时内存

| 状态 | 预期 |
|---|---|
| 仅托盘、主窗未开 | 与现在接近（无 WebView 进程） |
| 主窗打开（WebView 活着） | 额外约 **几十～150+ MB**（Edge 渲染进程；机器与页面复杂度影响大） |
| 主窗关闭（WebView 已 Destroy） | 应回到接近仅托盘水位（允许短时缓存；目标是不长期挂着浏览器） |

**产品话术**：主窗是「需要时再打开的控制台」，不是常驻仪表盘；打开才付 UI 成本。

## 架构

```
tray host (0×0, native)          // 消息、定时器、托盘图标
  └─ main shell HWND             // WS_OVERLAPPEDWINDOW，任务栏可见
        └─ wry WebView2          // 仅 show 时创建；WM_CLOSE / Hide 路径上 Destroy
              └─ ui/main/*       // HTML/CSS/JS（embed）

JS  ──postMessage/IPC──►  Rust command dispatcher  ──► handle_command / AppState
Rust ──eval/push JSON──►  JS render(snapshot)
```

### 生命周期（关窗销毁）

1. 启动：创建隐藏托盘宿主 +（可选）创建空主壳或延迟到首次打开再创建壳。
2. **首次「打开主窗口」**：
   - 若无壳 HWND → `CreateWindowEx` 主壳
   - 创建 `wry::WebView`，`navigate_to_string` 或自定义协议加载 embed HTML
   - `ShowWindow` + 推送首包 snapshot
3. **定时刷新**：仅当主窗可见且 WebView 存在时，把 `snapshot` JSON 推给前端（节流，如 1 s，与现 timer 对齐）。
4. **点 X / 关闭**：
   - `Destroy` WebView（必做）
   - 主壳 `ShowWindow(SW_HIDE)` **或** Destroy 壳并清空 `MAIN_HWND`（推荐：**壳 Hide + WebView Destroy**，再开只需重建 WebView，壳创建成本更低且任务栏行为稳定）
5. **托盘退出**：Destroy WebView → Destroy 主壳 → 现有退出路径。

推荐默认：**壳常驻隐藏 + WebView 开关窗销毁/重建**。

### 单实例与左键托盘

行为与现实现一致：左键 / 「打开主窗口」→ show + 确保 WebView 已建；二次启动策略首版不改。

## 前后端协议

### 前端 → Rust（用户操作）

JSON 消息，例如：

```json
{ "type": "command", "id": 102 }
{ "type": "switch_route", "index": 3 }
{ "type": "ready" }
```

- `command.id` 使用现有托盘 command 常量（`ID_CHECK`、`ID_SYNC`、…）。
- `switch_route` 映射 `app.switch_index`（同 `ID_ROUTE_BASE + index`）。
- 危险操作仍由 Rust 侧 `MessageBox` / 现有确认逻辑处理（JS 只发命令）。

### Rust → 前端（状态）

```json
{
  "type": "snapshot",
  "payload": { /* 精简自 Snapshot + recovery_hint + recommended_action */ }
}
```

规则：

- **不**把 API Key 默认塞进 snapshot；仅当设置开启且前端明确要详情时再给脱敏/按需字段。
- 指标、路由名、延迟、健康、开关状态足够渲染四页。
- `recommended_action: { id, label } | null`。

可用 `webview.evaluate_script` 调用 `window.__hr.applySnapshot(...)`，或 wry IPC 回推。

## UI 信息架构（四页不变）

与现主窗一致，仅换呈现：

1. **状态**：模式、分项健康、指标、最近错误、建议操作按钮  
2. **上游**：接管开关、路由卡片/列表、双击或按钮切换  
3. **运维**：自动切换、旁路、检查/同步/重启、打开故障转移（调原生 `show_failover_editor`）  
4. **设置**：启动/更新/API Key、预检/诊断/备份/维护按钮网格  

视觉：暗色、清晰层级、Segoe UI / 系统字体栈、状态色（健康/降级/不可用），**不要**花哨动效。

## 文件结构（实现时）

```
src/tray/main_window.rs          # 改为壳 + WebView 生命周期；保留 tray_host/main_hwnd API
src/tray/main_window/bridge.rs   # 可选：IPC 解析、snapshot DTO
ui/main/index.html
ui/main/app.css
ui/main/app.js
Cargo.toml                       # wry（windows）、可能需 webview2-com 传递特征
```

静态资源通过 `include_str!("../../ui/main/index.html")` 等嵌入；开发期可加 `HR_UI_DEV=1` 从磁盘加载以便热改（可选，非必须）。

## 依赖与发布

- `wry` with Windows / WebView2。
- 检测 WebView2 Runtime；缺失时通知用户并给出 [Evergreen 引导安装](https://developer.microsoft.com/microsoft-edge/webview2/) 链接或官方 bootstrapper（**不**静默下载巨大 Fixed Runtime）。
- CI / `Install.ps1`：文档注明「需要 WebView2 Runtime」；可选在安装脚本检测。

## 风险与缓解

| 风险 | 缓解 |
|---|---|
| 开窗内存升高 | 关窗 Destroy WebView；文案上主窗非常驻 |
| 无 WebView2 的老机器 | 启动打开主窗时检测 + 明确错误，不让托盘整体崩溃 |
| wry 与现有 `GetMessage` 循环 | 在现有 tray 消息循环同线程创建 WebView；避免第二 UI 线程除非 wry 要求 |
| IPC 注入 / 任意脚本 | 只 load embed 或自定义 https 虚拟主机；不启用远程 URL；校验 command id 白名单 |
| 与原生对话框 z-order | 子对话框 owner = 主壳 HWND（可见时） |

## 验证标准

1. 仅托盘运行时，任务管理器中无持续 Edge/WebView 渲染进程（或可忽略的短时残留后消失）。  
2. 打开主窗 → 四页可用，命令与原生版行为一致。  
3. 关主窗 → WebView 销毁，内存明显回落。  
4. 再开主窗 → WebView 重建，状态正确。  
5. release 体积增量可接受（记录前后 `HeadroomRoute.exe` 大小）。  
6. 无 WebView2 时有可读提示，托盘仍可退出/基础操作。  
7. `cargo test` 现有测试不回归；bridge 解析可单测。

## 实现分期

- **P0**：壳 + wry 空白页 + show/hide/destroy 生命周期 + Runtime 检测  
- **P1**：snapshot bridge + 状态/运维页  
- **P2**：上游页 + 设置页命令按钮  
- **P3**：视觉抛光、无 Runtime 引导、体积/内存笔记写入 README

## 参考

- 当前主窗与托盘 API：`src/tray/main_window.rs`、`src/tray/commands.rs`、`src/tray/menu.rs`
- 现 snapshot 字段：`AppState::snapshot` / `model::Snapshot`
- WebView2 分发概念：Microsoft Edge WebView2 发行说明（Evergreen vs Fixed）
