# 兼容性与验证基线

本文件定义 HeadroomRoute 的发布验证范围。这里的“验证基线”表示每次正式发布应重复验证的组合，不表示对第三方工具未来版本的无限兼容承诺。

## 平台基线

| 组件 | 支持范围 | 发布验证基线 | 说明 |
| --- | --- | --- | --- |
| Windows | 仍在 Microsoft 支持周期内的 Windows 11 x64 | Windows 11 Pro x64，内部版本 26200 | 仅发布 x64 Windows 二进制 |
| Windows 10 | 22H2 x64，尽力兼容 | 非发布阻断项 | Windows 10 已结束常规支持，不再作为主要验证平台 |
| PowerShell | Windows PowerShell 5.1 或更高 | Windows PowerShell 5.1 | `Build.ps1`、`Install.ps1`、`Test-Install.ps1` 必须保持 5.1 语法兼容 |
| Python | CPython 3.10–3.12.x（`runtime.rs` 探测要求 `>= 3.10`） | CPython 3.12.10 | Python 3.13 及以上需单独验证后再纳入发布阻断矩阵 |
| Headroom | `headroom-ai[code]` **0.34.0 或 0.35.0**（`runtime.rs` 白名单） | 0.35.0 | 运行时探测接受这两个精确版本；新安装提示 0.35.0。增加其它版本前需先做兼容性验证 |
| Codex CLI | 不硬编码版本，要求当前稳定版通过包装器验证 | 0.147.0 | 版本命令：`codex --version`（输出形如 `codex-cli 0.147.0`） |
| Claude Code CLI | 不硬编码版本，要求当前稳定版通过包装器验证 | 2.1.220 | 版本命令：`claude --version`（输出形如 `2.1.220 (Claude Code)`） |

以上本机版本记录更新于 2026-08-19。第三方 CLI 的版本号是已验证样本，不是最低版本声明。Headroom 0.35.0 已核对 `headroom.cli` 与当前 `proxy` 启动参数均存在。

## 验证方法

每次正式发布按以下命令确认基线，并把实际版本号与结果写入发布说明。只跑“版本命令”只能证明存在性，不能代替交互验证。

### 平台与运行时探测

```powershell
# Windows 与 PowerShell（发布验证平台：Windows 11 Pro x64 内部版本 26200，Windows PowerShell 5.1）
[Environment]::OSVersion.Version
$PSVersionTable.PSVersion

# Python + Headroom 只读探测，与 HeadroomRoute --doctor / 托盘预检使用的逻辑一致（src/runtime.rs）：
python -c "import sys,importlib.metadata as m; assert sys.version_info >= (3,10); assert m.version('headroom-ai') in ('0.34.0', '0.35.0'); import headroom.cli"
# 退出码必须为 0；否则说明 Python 版本或 headroom-ai 版本不在基线内
```

### CLI 版本命令

```powershell
codex --version     # 当前样本：codex-cli 0.147.0
claude --version    # 当前样本：2.1.220 (Claude Code)
```

### 发布产物验证

```powershell
Get-AuthenticodeSignature .\dist\HeadroomRoute-<version>.exe      # 状态必须为 Valid
Get-AuthenticodeSignature .\dist\HeadroomRouteCLI-<version>.exe   # 状态必须为 Valid
Get-FileHash .\dist\HeadroomRoute-<version>-windows-x64.zip       # 与 SHA256SUMS 清单一致
```

## 发布验证项目

每次正式发布至少验证：

1. `cargo fmt -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check` 和 `cargo test` 全部通过；`Build.ps1` 将上述检查设为构建门禁。
2. `Test-Install.ps1` 在随机临时目录完成安装、运行中升级、故障恢复、签名策略与手动回滚，覆盖：默认 Warn 策略接受未签名开发包并告警、`-SignaturePolicy Require` 与 `-TrustedPublisherThumbprint` 拒绝未签名包、链不受信任的签名被拒绝、失败升级自动恢复升级前完整受管状态、损坏或缺少 manifest 的回滚备份被拒绝、手动回滚恢复旧版本；测试前后的用户级 PATH 完全一致。
3. 使用发布基线 Python（当前 3.12.10）执行上面“运行时探测”命令，以及一次 Headroom 实际启动检查。
4. 分别运行当前稳定版 Codex CLI 与 Claude Code CLI 的版本命令和一次真实交互会话；确认窗口、ConPTY 行为、Ctrl+C 与工作区信任提示属于发布阻断项，必须实机验证。
5. 在 Windows 11 x64 上验证托盘、开机启动、休眠唤醒、代理端口和多显示器 DPI 行为。
6. 发布二进制通过 Authenticode 验证，ZIP 内容完整，SHA-256 清单与产物一致。

若某一第三方版本未完成真实会话验证，应在发布说明中明确标记“未验证”，不能仅凭版本命令宣称兼容。
