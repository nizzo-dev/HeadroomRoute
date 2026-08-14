# 构建、签名与回滚

## 发布前提

正式打包前必须先确认版本号，不能由构建脚本自动修改版本。代码签名证书和私钥不得保存到仓库、发布 ZIP、日志或诊断包中。

`Build.ps1` 支持两种模式：

- 开发构建未配置证书时可以继续，但会显示明确警告，产物保持未签名。
- 正式发布应传入 `-RequireSignature`；未提供可用证书、签名失败或签名复核失败都会终止构建，不会生成可交付 ZIP。

证书解析在构建开始前完成（fail-fast）：未传证书指纹且未启用 `-RequireSignature` 时按开发构建继续；传了指纹但证书不存在、无私钥、已过期或缺少 Code Signing 用途时，无论是否启用门禁都会立即终止并给出可操作错误。开发机不应残留过期的 `HEADROOM_ROUTE_SIGNING_CERT_THUMBPRINT` 环境变量。

构建门禁顺序为 `cargo fmt -- --check` → `cargo clippy --all-targets -- -D warnings` → `cargo check` → `cargo test` → `Test-Install.ps1` → `cargo build --release`，任一失败即中止。

推荐通过当前用户证书存储区提供代码签名证书：

```powershell
$env:HEADROOM_ROUTE_SIGNING_CERT_THUMBPRINT = '<certificate thumbprint>'
$env:HEADROOM_ROUTE_TIMESTAMP_SERVER = 'https://<trusted timestamp service>'
.\Build.ps1 -RequireSignature
```

证书位于本机 `Cert:\CurrentUser\My`。如由受控构建机使用计算机级证书，可增加 `-CertificateStoreLocation LocalMachine`。证书必须包含私钥、处于有效期内，并具有 Code Signing EKU。时间戳服务地址由发布环境提供，仓库不绑定证书颁发机构或外部服务。

构建完成后复核版本化二进制（托盘主程序、桌面主程序、CLI）：

```powershell
Get-AuthenticodeSignature .\dist\HeadroomRoute-<version>.exe
Get-AuthenticodeSignature .\dist\HeadroomRoute-<version>-desktop.exe
Get-AuthenticodeSignature .\dist\HeadroomRouteCLI-<version>.exe
```

三项状态都必须为 `Valid`，签名者指纹必须与发布证书一致。SHA-256 清单在签名完成后生成，因此校验值对应最终签名产物。同一清单包含两个 ZIP：`HeadroomRoute-<version>-windows-x64.zip`（托盘）与 `HeadroomRoute-<version>-desktop-windows-x64.zip`（桌面）。应用内更新按编译进二进制的版本选择对应 ZIP，不会把托盘用户升级到桌面包。

## 安装签名策略

`Install.ps1` 在停止当前进程前验证暂存二进制：

- `-SignaturePolicy Warn`：默认值。未签名开发包显示警告并继续；存在但无效的签名仍会被拒绝。
- `-SignaturePolicy Require`：主程序和 CLI 都必须具有有效 Authenticode 签名。
- `-SignaturePolicy Skip`：仅供隔离测试或明确受控的开发环境使用。
- `-TrustedPublisherThumbprint <thumbprint>`：在有效签名基础上进一步固定发布者证书；也可通过 `HEADROOM_ROUTE_TRUSTED_PUBLISHER_THUMBPRINT` 提供。

正式发布包的安装验收必须使用 `-SignaturePolicy Require`。如果配置了发布者指纹，未签名、签名损坏和签名者不匹配都会在停止旧版本前失败。

## 升级与回滚

升级前，安装脚本会在 `<安装目录>\rollback\<UTC 时间戳>` 创建持久快照。快照包含主程序、CLI、`hr.cmd`、`config.json`、`status.json` 和带 SHA-256 的 `manifest.json`。

升级事务另有临时完整快照，用于在文件替换或新版本启动失败时恢复：

- 当前主程序、CLI 和 shim；
- 配置与状态文件；
- 原有的 `*.previous.*` 文件；
- 原有的 `update-settings-backup` 完整目录。

自动恢复有任何一步失败时，脚本会明确报告“自动恢复不完整”并给出持久回滚快照路径，不会声称已经恢复成功。

恢复最近一次升级前快照：

```powershell
.\Install.ps1 -Rollback -StartNow
```

恢复指定快照：

```powershell
.\Install.ps1 -Rollback -RollbackBackup '<backup directory or name>' -StartNow
```

手动回滚前，脚本还会创建 `before-manual-rollback` 安全快照。清单缺失、路径越界或 SHA-256 不匹配时，回滚会在替换文件前终止。

## 外部依赖限制

仓库无法提供或代管生产证书、硬件密钥、证书链信任和时间戳服务可用性。发布负责人需要在受控 Windows 构建机上配置这些外部条件，并保护证书私钥。没有真实受信任证书时，只能验证“未签名门禁与失败路径”，不能宣称完成生产签名验收。
