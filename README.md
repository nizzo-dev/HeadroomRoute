# Headroom Route

一个面向 Windows 的轻量 Headroom / Codex / Claude Code 路由托盘工具。核心、托盘和本地代理均在单个原生进程中运行，不依赖 Electron、.NET 桌面运行时或常驻 Python UI。

## 功能

- 自动发现 Codex `config.toml`、Claude Code `settings.json` 与 CC-Switch Provider。
- 同时代理 OpenAI Responses API 与 Anthropic Messages API，两套上游可独立切换。
- 管理 Headroom，监控上游健康并自动故障切换。
- 右键托盘图标切换 Provider、同步配置、重启 Headroom、复制脱敏诊断。
- 自动故障切换默认关闭；启用后，当前路由连续失败 3 次会切换到同协议中已验证健康的 Provider。
- 兼容旧 TrafficMonitor 插件使用的 `8790` 控制接口及状态文件。
- Codex 与 Claude Code 修改前创建带时间戳备份。工具自身配置不保存 API Key。
- 首次运行自动准备独立 Python 3.12 / Headroom 0.32.1 环境，不依赖系统 Python。
- 支持恢复 CLI 配置、修复托管运行环境和完全卸载。

## 使用

发布包解压后可直接运行版本化 EXE，或执行 `Install.ps1 -StartNow` 安装到 `%LOCALAPPDATA%\HeadroomRoute`。首次启动会自动发现组件、备份并配置 Codex 与 Claude Code，然后驻留通知区域。

再次运行 `Install.ps1` 可原位升级：脚本会先暂存并校验新 EXE，再停止安装目录中的运行实例、保留 `HeadroomRoute.previous.exe`、替换并自动重启；新版本无法启动时会恢复旧版本。

- 双击图标：查看状态。
- 右键图标：切换上游或打开管理菜单。
- `HeadroomRoute.exe --doctor`：输出脱敏诊断报告。
- `HeadroomRoute.exe --configure`：仅执行一次 Codex 与 Claude Code 路由配置。
- `HeadroomRoute.exe --configure-claude`：仅配置 Claude Code。
- `HeadroomRoute.exe --restore`：恢复 Codex 与 Claude Code 原始路由。
- `HeadroomRoute.exe --repair-runtime`：重新安装托管 Headroom 运行环境。
- `HeadroomRoute.exe --uninstall`：恢复配置并删除托管环境、启动项和程序。

数据目录为 `%LOCALAPPDATA%\HeadroomRoute`。高级用户可以编辑其中的 `config.json`。

## 构建

需要 Rust stable。运行 `Build.ps1`，产物位于 `dist`，包括版本化 EXE、Windows x64 ZIP 和 SHA-256 校验文件。版本号自动取自 `Cargo.toml`。已安装 Visual Studio C++ Build Tools 时使用系统工具链；开发机没有 SDK 时脚本也支持 cargo-xwin 缓存。

## 资源目标

当前 x64 Release 约 1.2 MB。隔离环境实测私有内存约 4 MB、工作集约 15.5 MB。网络请求并发时会按请求体和 TLS 缓冲临时增长。
