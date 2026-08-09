# HeadroomRoute 开发交接

更新时间：2026-08-08 20:00（Asia/Shanghai）

## 1. 协作与操作约束

- 所有代码实现交给 OpenCode；Codex 只拆任务、派单、审查、测试和验收。
- 并行开发指多个 OpenCode 会话并行，并且各会话的可写文件范围必须互斥。
- 全部路线图功能完成后，再让用户进行一次真实场景测试；中途不要求用户逐项测试。
- 保留工作区全部原有未提交改动，不提交、不推送、不发布、不改版本。
- 不运行 `Build.ps1`。任何打包或发布前，必须先提出 SemVer 建议并取得用户确认。
- 所有 shell 命令必须以 `rtk` 开头。仓库存在 `.codegraph/`，理解或定位代码时先使用 `rtk codegraph explore`。
- 本机缺少 `link.exe`，Rust 完整检查和测试需使用本文末尾的 xwin 环境变量。

## 2. 已完成并由 Codex 验收

### P0.1 / P0.2

- 统一预检、doctor、首次启动向导。
- 首次窗口前台激活、重新检测、单实例、关闭后托盘继续运行。
- 已完成 Windows GUI 实机验收。

### P0.3

- 安装/升级事务回滚、持久回滚快照和清单校验。
- Authenticode 构建与安装门禁。
- `COMPATIBILITY.md`、`RELEASE.md` 和扩充后的 `Test-Install.ps1`。
- 已验收签名拒绝、损坏备份、手动回滚以及安装前后用户 PATH 不变。

### P0.4 / P1.1

- 统一正常、降级、旁路、直连、恢复中五类运行状态。
- 托盘、完整状态、预检和诊断使用同一状态口径。
- 显式重启/同步优先显示恢复中，真实故障仍保持最高优先级。

### P1.3 / P2.2 后端能力

- 接管配置差异预览和确认 token。
- 配置备份、列出、校验和恢复，包括恢复“文件原本不存在”的状态。
- 便携配置版本化导入/导出和脱敏诊断 ZIP。
- 带 control token 的 `/_headroom_route_agent/v1/status`；旧状态端点仅保留 `service`、`version`。
- 扩充敏感信息脱敏规则；多文件事务会显式报告回滚失败。

上述已完成范围最近一次完整门禁结果：

```text
cargo clippy --all-targets -- -D warnings
cargo test: 154 passed
cargo fmt -- --check
git diff --check
Test-Install.ps1
```

## 3. 当前暂停点

所有与本项目相关的 OpenCode 开发进程均已停止，当前数量为 0。未提交、未回滚，也未覆盖工作区原有改动。

第二批三个核心草稿已经由 OpenCode 开始删减：

| 文件 | 原大小 | 当前大小 | 当前状态 |
| --- | ---: | ---: | --- |
| `src/operation_history.rs` | 57,321 B | 41,054 B | 已重写一轮，但仍有重复片段和语法错误 |
| `src/environment_recovery.rs` | 78,249 B | 17,990 B | 已大幅收敛，尚未编译验收 |
| `src/routing_policy.rs` | 58,061 B | 10,345 B | 已大幅收敛，尚未声明进 crate 或编译验收 |

`src/main.rs` 已加入：

```rust
mod environment_recovery;
mod operation_history;
```

尚未加入 `mod routing_policy;`。`AppConfig`、便携配置和运行时也尚未接入 `routing_strategy`。

P1.3/P2.2 的托盘入口尚未产生任何新修改。最后一个 OpenCode 托盘会话只完成代码阅读，拆分后的“仅 P1.3 接管预览”会话也在写入前被暂停。

## 4. 当前已知故障

xwin `cargo check` 当前失败于 `src/operation_history.rs`。`rg` 显示同一可疑语句至少出现在第 667 和 771 行：

```rust
let unquoted = trimmed.trim_matches(['"', '\\', '`']);
```

编译器报告字符字面量包含多个 codepoint、反斜杠 token 非法和字符字面量未闭合。该文件还有重复的脱敏逻辑片段，不能只机械修一行；续接 OpenCode 必须先审查并删除重复块，再修正语法。

普通 `rtk cargo check` 还会因为本机不存在 `link.exe` 失败，这不是仓库缺陷。必须使用 xwin 环境。

`rtk git diff --check` 在暂停前通过；`cargo fmt -- --check` 会因上述 Rust 解析错误失败。暂停后的代码尚未通过 clippy 或测试，不能视为已验收。

## 5. 建议续接顺序

1. 让一个 OpenCode 会话独占以下文件，完成核心基座：`src/operation_history.rs`、`src/environment_recovery.rs`、`src/routing_policy.rs`、`src/main.rs`、`src/model.rs`、`src/config.rs`、`src/config_portability.rs`。
2. 先删除 `operation_history.rs` 重复/损坏代码，声明三个模块，并确保模块测试真正参与 `cargo test`。
3. 在 `AppConfig` 增加 serde-default 的 `routing_strategy`，默认 `enabled=false`、`observe_only=true`，保证不改变现有流量。
4. 成本配置改为按 Provider ID 的明确映射，拒绝非有限值和负数；便携配置只包含不敏感策略字段。
5. 核心基座通过 fmt、clippy、test 后，再启动托盘 OpenCode 会话。
6. 托盘任务分两次串行写 `src/tray.rs`：先完成 P1.3 接管差异预览与确认，再完成 P2.2 备份/恢复/导入/导出/诊断 ZIP 菜单。
7. 接入 P1.2：历史加载保存、手动/自动切换记录、冷却和确认撤销。
8. 接入 P1.4：会话标记、异常退出恢复、休眠唤醒、VPN/系统代理变化、端口冲突分类和恢复退避。
9. 接入 P2.1：`proxy.rs` 仅解析 JSON 顶层 `model`；observe-only 只记录决策，apply 模式才改变有效 Provider；持久化脱敏、可解释决策。
10. 最后接托盘 P1.2/P1.4/P2.1 控件，更新 `IMPLEMENTATION_PLAN.md` 的职责描述，并执行全量验收。

## 6. 下一次 OpenCode 核心任务验收标准

- 不允许模块级 `allow(dead_code)` 掩盖大批未接入 API；删除重复抽象和无调用价值的公开接口。
- 操作历史保留：有界脱敏记录、原子加载/保存、冷却、撤销票据。
- 环境恢复保留：会话标记、事件状态机、去抖/退避、端口占用分类。
- 路由策略保留：确定性评分、可解释原因、observe/apply 行为和按 Provider ID 的成本配置。
- 默认配置和旧配置迁移不得改变用户当前路由行为。
- 便携配置不得包含 API key、auth.json、control token、session token 或请求正文。
- 本任务不得提前修改 `state.rs`、`worker.rs`、`proxy.rs` 或 `tray.rs`。

## 7. xwin 验收命令

```powershell
rtk powershell -NoProfile -Command "`$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'C:\Users\20668\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'; `$env:RUSTFLAGS = '-Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\sdk\lib\um\x86_64 -Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\sdk\lib\ucrt\x86_64 -Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\crt\lib\x86_64'; rtk cargo test"

rtk powershell -NoProfile -Command "`$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = 'C:\Users\20668\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'; `$env:RUSTFLAGS = '-Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\sdk\lib\um\x86_64 -Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\sdk\lib\ucrt\x86_64 -Lnative=C:\Users\20668\AppData\Local\cargo-xwin\xwin\crt\lib\x86_64'; rtk cargo clippy --all-targets -- -D warnings"

rtk cargo fmt -- --check
rtk git diff --check
rtk powershell -NoProfile -ExecutionPolicy Bypass -File .\Test-Install.ps1
```

不要运行 `Build.ps1`，直到用户确认打包版本号。

## 8. 工作区概况

当前工作区包含大量已验收功能和进行中的第二批代码，共有 13 个已跟踪文件被修改，并有 `.codegraph/`、文档及多个新 Rust 模块未跟踪。不要用 reset、checkout 或清理命令恢复工作区；后续必须在现有内容上继续。

## 9. 2026-08-10 启动预检报告文字重叠根因

- 现象：启动预检窗口完成异步检测后，报告顶部几行出现新旧文字叠加；后面的检测项文字正常。
- 根因：`src/tray.rs` 的 `precheck_report_edit` 创建了 `ES_READONLY` 多行 `EDIT` 控件，而 `precheck_window_proc` 对所有 `WM_CTLCOLORSTATIC` 消息统一设置了 `SetBkMode(..., TRANSPARENT)`。Windows 会将只读 EDIT 作为静态颜色控件发送该消息，因此报告框没有使用不透明背景清除旧像素。
- 触发链：窗口初始化时先显示“正在收集运行环境事实……”，后台检测完成后再通过 `SetWindowTextW` 写入最终报告。透明背景下重绘未清除前一版文字，形成截图中的残留叠字；报告文本本身使用 `\r\n`，布局矩形也未发生控件相互覆盖。
- 当前状态：根因已确认，修复尚未应用。后续应按控件句柄区分普通 STATIC 与报告 EDIT，使报告 EDIT 使用不透明窗口背景并完整擦除后再重绘。
