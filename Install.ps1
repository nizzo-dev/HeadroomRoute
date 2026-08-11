param(
    [switch]$StartNow,
    [int]$ProcessId = 0,
    [switch]$SkipPathUpdate,
    [ValidateSet('Warn', 'Require', 'Skip')]
    [string]$SignaturePolicy = 'Warn',
    [string]$TrustedPublisherThumbprint = $env:HEADROOM_ROUTE_TRUSTED_PUBLISHER_THUMBPRINT,
    [switch]$Rollback,
    [string]$RollbackBackup,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'HeadroomRoute')
)
$ErrorActionPreference = 'Stop'

function Normalize-PathEntry([string]$Entry) {
    if ([string]::IsNullOrWhiteSpace($Entry)) { return '' }
    $expanded = [Environment]::ExpandEnvironmentVariables($Entry.Trim().Trim('"'))
    try {
        return ([System.IO.Path]::GetFullPath($expanded)).TrimEnd('\', '/')
    } catch {
        return $expanded.TrimEnd('\', '/')
    }
}

function Get-UserPathEntries {
    $raw = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ([string]::IsNullOrWhiteSpace($raw)) { return @() }
    @($raw -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
}

function Add-UserPathEntry([string]$Entry) {
    $canonical = Normalize-PathEntry $Entry
    if (!$canonical) { return $false }
    $entries = @(Get-UserPathEntries)
    foreach ($existing in $entries) {
        if ((Normalize-PathEntry $existing) -ieq $canonical) { return $false }
    }
    $updated = @($entries + $canonical) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    $currentEntries = @($env:Path -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    if (-not (@($currentEntries | Where-Object { (Normalize-PathEntry $_) -ieq $canonical }).Count)) {
        $env:Path = if ($env:Path) { "$canonical;$env:Path" } else { $canonical }
    }
    return $true
}

function Remove-UserPathEntry([string]$Entry) {
    $canonical = Normalize-PathEntry $Entry
    if (!$canonical) { return }
    $entries = @(Get-UserPathEntries | Where-Object { (Normalize-PathEntry $_) -ine $canonical })
    [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
}

function Get-FileSha256([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try { return [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $sha256.Dispose(); $stream.Dispose() }
}

function Test-PackageSignature([string]$Path, [string]$Policy, [string]$PinnedThumbprint) {
    if ($Policy -eq 'Skip') { return }
    $authenticodeCommand = Get-Command Get-AuthenticodeSignature -ErrorAction SilentlyContinue
    if (!$authenticodeCommand) {
        if ($Policy -eq 'Require' -or ![string]::IsNullOrWhiteSpace($PinnedThumbprint)) {
            throw '当前 PowerShell 环境缺少 Get-AuthenticodeSignature，无法执行要求的签名策略验证。'
        }
        Write-Warning '当前 PowerShell 环境缺少 Get-AuthenticodeSignature，无法验证未签名状态；开发构建按 Warn 策略继续。'
        return
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -eq [System.Management.Automation.SignatureStatus]::NotSigned) {
        if ($Policy -eq 'Require' -or ![string]::IsNullOrWhiteSpace($PinnedThumbprint)) {
            throw "安装包未签名，签名策略拒绝安装：$(Split-Path -Leaf $Path)"
        }
        Write-Warning "安装包未签名：$(Split-Path -Leaf $Path)。开发构建可以继续；正式发布请使用 -SignaturePolicy Require。"
        return
    }
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or !$signature.SignerCertificate) {
        throw "安装包 Authenticode 签名无效：$(Split-Path -Leaf $Path)（$($signature.Status)：$($signature.StatusMessage)）"
    }
    if (![string]::IsNullOrWhiteSpace($PinnedThumbprint)) {
        $expected = [string]($PinnedThumbprint -replace '[^0-9A-Fa-f]', '')
        $expected = $expected.ToUpperInvariant()
        if (!$expected -or $signature.SignerCertificate.Thumbprint -ine $expected) {
            throw "安装包签名者与受信任发布者指纹不匹配：$(Split-Path -Leaf $Path)"
        }
    }
}

function New-RollbackBackup([string]$Destination, [string]$Reason) {
    $managedFiles = @('HeadroomRoute.exe', 'HeadroomRouteCLI.exe', 'hr.cmd', 'config.json', 'status.json')
    if (-not @($managedFiles | Where-Object { Test-Path -LiteralPath (Join-Path $Destination $_) }).Count) {
        return $null
    }
    $rollbackRoot = Join-Path $Destination 'rollback'
    $name = '{0}-{1}' -f ([DateTime]::UtcNow.ToString('yyyyMMdd-HHmmssfff')), ([guid]::NewGuid().ToString('N').Substring(0, 8))
    $backupPath = Join-Path $rollbackRoot $name
    New-Item -ItemType Directory -Path $backupPath -Force | Out-Null
    try {
        $entries = foreach ($relativePath in $managedFiles) {
            $current = Join-Path $Destination $relativePath
            $present = Test-Path -LiteralPath $current
            $backupFile = Join-Path $backupPath $relativePath
            if ($present) { Copy-Item -LiteralPath $current -Destination $backupFile -Force }
            [ordered]@{
                relative_path = $relativePath
                present = $present
                sha256 = if ($present) { Get-FileSha256 $backupFile } else { $null }
            }
        }
        $manifest = [ordered]@{
            schema_version = 1
            created_utc = [DateTime]::UtcNow.ToString('o')
            reason = $Reason
            files = @($entries)
        }
        $json = $manifest | ConvertTo-Json -Depth 4
        [System.IO.File]::WriteAllText((Join-Path $backupPath 'manifest.json'), $json, (New-Object System.Text.UTF8Encoding($false)))
        return $backupPath
    } catch {
        Remove-Item -LiteralPath $backupPath -Recurse -Force -ErrorAction SilentlyContinue
        throw
    }
}

function Read-RollbackManifest([string]$BackupPath) {
    $manifestPath = Join-Path $BackupPath 'manifest.json'
    if (!(Test-Path -LiteralPath $manifestPath)) { throw "回滚备份缺少 manifest.json：$BackupPath" }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    if ($manifest.schema_version -ne 1) { throw "不支持的回滚备份格式：$($manifest.schema_version)" }
    $allowed = @('HeadroomRoute.exe', 'HeadroomRouteCLI.exe', 'hr.cmd', 'config.json', 'status.json')
    $manifestPaths = @($manifest.files | ForEach-Object { $_.relative_path })
    if ($manifestPaths.Count -ne $allowed.Count -or @($manifestPaths | Select-Object -Unique).Count -ne $allowed.Count) {
        throw '回滚清单文件项不完整或包含重复项'
    }
    foreach ($requiredPath in $allowed) {
        if ($manifestPaths -notcontains $requiredPath) { throw "回滚清单缺少文件项：$requiredPath" }
    }
    foreach ($entry in @($manifest.files)) {
        if ($allowed -notcontains $entry.relative_path) { throw "回滚清单包含非法路径：$($entry.relative_path)" }
        if ($entry.present) {
            $source = Join-Path $BackupPath $entry.relative_path
            if (!(Test-Path -LiteralPath $source)) { throw "回滚备份缺少文件：$($entry.relative_path)" }
            if ((Get-FileSha256 $source) -ine $entry.sha256) { throw "回滚备份校验失败：$($entry.relative_path)" }
        }
    }
    return $manifest
}

function Restore-ManagedState([string]$Destination, [string]$BackupPath, $Manifest) {
    foreach ($entry in @($Manifest.files)) {
        $targetPath = Join-Path $Destination $entry.relative_path
        if ($entry.present) {
            Copy-Item -LiteralPath (Join-Path $BackupPath $entry.relative_path) -Destination $targetPath -Force
        } elseif (Test-Path -LiteralPath $targetPath) {
            Remove-Item -LiteralPath $targetPath -Force
        }
    }
}

$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$installDirExisted = Test-Path -LiteralPath $InstallDir
$target = Join-Path $InstallDir 'HeadroomRoute.exe'
$backup = Join-Path $InstallDir 'HeadroomRoute.previous.exe'
$cliTarget = Join-Path $InstallDir 'HeadroomRouteCLI.exe'
$cliBackup = Join-Path $InstallDir 'HeadroomRouteCLI.previous.exe'
$shimTarget = Join-Path $InstallDir 'hr.cmd'
$shimBackup = Join-Path $InstallDir 'hr.previous.cmd'
$settingsBackup = Join-Path $InstallDir 'update-settings-backup'
$settingNames = @('config.json', 'status.json')

if ($RollbackBackup -and !$Rollback) { throw '-RollbackBackup 必须与 -Rollback 一起使用' }
if ($Rollback) {
    if (!$installDirExisted) { throw "安装目录不存在，无法回滚：$InstallDir" }
    $rollbackRoot = Join-Path $InstallDir 'rollback'
    if (!(Test-Path -LiteralPath $rollbackRoot)) { throw "没有可用的回滚备份：$rollbackRoot" }
    if ($RollbackBackup) {
        $selectedBackup = if ([System.IO.Path]::IsPathRooted($RollbackBackup)) { $RollbackBackup } else { Join-Path $rollbackRoot $RollbackBackup }
    } else {
        $selectedBackup = Get-ChildItem -LiteralPath $rollbackRoot -Directory |
            Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'manifest.json') } |
            Sort-Object Name -Descending |
            Select-Object -First 1 -ExpandProperty FullName
    }
    if (!$selectedBackup -or !(Test-Path -LiteralPath $selectedBackup)) { throw '找不到指定的回滚备份' }
    $rollbackRootPath = [System.IO.Path]::GetFullPath($rollbackRoot).TrimEnd('\') + '\'
    $selectedBackupPath = [System.IO.Path]::GetFullPath((Resolve-Path -LiteralPath $selectedBackup).Path)
    if (!$selectedBackupPath.StartsWith($rollbackRootPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw '回滚备份必须位于安装目录的 rollback 子目录中'
    }
    $rollbackManifest = Read-RollbackManifest $selectedBackupPath
    $targetPath = [System.IO.Path]::GetFullPath($target)
    $cliTargetPath = [System.IO.Path]::GetFullPath($cliTarget)
    $rollbackRunningMain = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $targetPath })
    $rollbackRunningCli = @(Get-Process -Name 'HeadroomRouteCLI' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $cliTargetPath })
    $rollbackProcesses = @($rollbackRunningMain) + @($rollbackRunningCli) | Sort-Object Id -Unique
    $restartAfterRollback = $StartNow -or $rollbackRunningMain.Count -gt 0
    $safetyBackup = New-RollbackBackup $InstallDir 'before-manual-rollback'
    $rollbackStarted = $null
    $rollbackFilesChanged = $false
    try {
        if ($rollbackProcesses.Count) {
            $rollbackProcesses | Stop-Process -Force
            foreach ($process in $rollbackProcesses) {
                if (!$process.WaitForExit(10000)) { throw '当前版本未能在 10 秒内退出' }
            }
        }
        $rollbackFilesChanged = $true
        Restore-ManagedState $InstallDir $selectedBackupPath $rollbackManifest
        if ($restartAfterRollback -and (Test-Path -LiteralPath $target)) {
            $rollbackStarted = Start-Process $target -WindowStyle Hidden -PassThru
            Start-Sleep -Seconds 1
            $rollbackStarted.Refresh()
            if ($rollbackStarted.HasExited) { throw "回滚版本启动后立即退出（代码 $($rollbackStarted.ExitCode)）" }
        } elseif ($restartAfterRollback) {
            Write-Warning '回滚备份中没有可启动的主程序 HeadroomRoute.exe；文件已恢复，但未启动应用。'
        }
    } catch {
        $rollbackFailure = $_
        if ($rollbackStarted -and !$rollbackStarted.HasExited) {
            Stop-Process -Id $rollbackStarted.Id -Force -ErrorAction SilentlyContinue
            $rollbackStarted.WaitForExit(10000) | Out-Null
        }
        try {
            if ($rollbackFilesChanged -and $safetyBackup) {
                $safetyManifest = Read-RollbackManifest $safetyBackup
                Restore-ManagedState $InstallDir $safetyBackup $safetyManifest
            }
            if ($rollbackRunningMain.Count -gt 0 -and (Test-Path -LiteralPath $target)) {
                Start-Process $target -WindowStyle Hidden | Out-Null
            }
        } catch {
            throw "回滚失败，且恢复回滚前状态也失败：$rollbackFailure；恢复错误：$_"
        }
        throw "回滚失败，已恢复回滚前状态：$rollbackFailure"
    }
    Write-Host "已从回滚备份恢复：$selectedBackupPath"
    if ($safetyBackup) { Write-Host "回滚前状态备份：$safetyBackup" }
    exit 0
}

$manifest = Join-Path $project 'Cargo.toml'
$version = if (Test-Path $manifest) { (Select-String -Path $manifest -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value }
$versionedSource = if ($version) { Join-Path $project "dist\HeadroomRoute-$version.exe" }
$packagedSource = Get-ChildItem -Path $project -Filter 'HeadroomRoute-*.exe' | Select-Object -First 1 -ExpandProperty FullName
$source = if ($versionedSource -and (Test-Path $versionedSource)) { $versionedSource } elseif ($packagedSource) { $packagedSource } else { Join-Path $project 'dist\HeadroomRoute.exe' }
if (!(Test-Path $source)) { throw '请先运行 Build.ps1，或从发布 ZIP 中执行此脚本' }
$versionedCliSource = if ($version) { Join-Path $project "dist\HeadroomRouteCLI-$version.exe" }
$packagedCliSource = Get-ChildItem -Path $project -Filter 'HeadroomRouteCLI-*.exe' | Select-Object -First 1 -ExpandProperty FullName
$cliSource = if ($versionedCliSource -and (Test-Path $versionedCliSource)) { $versionedCliSource } elseif ($packagedCliSource) { $packagedCliSource } else { Join-Path $project 'dist\HeadroomRouteCLI.exe' }
$hasCliSource = Test-Path $cliSource
if (!$hasCliSource) {
    if ($ProcessId -gt 0) {
        throw '更新包缺少 HeadroomRouteCLI.exe 与 hr.cmd，快捷命令 hr 未安装；请使用 Build.ps1 重新生成发布包'
    }
    Write-Warning '未找到 HeadroomRouteCLI.exe 与 hr.cmd：快捷命令 hr（hr codex / hr claude）将不可用，请运行 Build.ps1 生成完整发布包后重试。'
}
$shimSource = Join-Path $project 'hr.cmd'
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
$transactionDir = Join-Path $InstallDir ('.update-transaction-' + [guid]::NewGuid().ToString('N'))
$staged = Join-Path $transactionDir 'HeadroomRoute.new.exe'
$cliStaged = Join-Path $transactionDir 'HeadroomRouteCLI.new.exe'
$shimStaged = Join-Path $transactionDir 'hr.new.cmd'
$shimContent = @'
@echo off
setlocal

set "HR_CLI=%~dp0HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

set "HR_CLI=%~dp0dist\HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

for %%F in ("%~dp0HeadroomRouteCLI-*.exe") do (
    if exist "%%~fF" (
        set "HR_CLI=%%~fF"
        goto run
    )
)

set "HR_CLI=%LOCALAPPDATA%\HeadroomRoute\HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

>&2 echo HeadroomRoute CLI executable not found. Run Install.ps1 first.
exit /b 1

:run
"%HR_CLI%" %*
set "HR_EXIT=%ERRORLEVEL%"
endlocal & exit /b %HR_EXIT%
'@
$settingsPresent = @{}
$sourcePath = (Resolve-Path $source).Path
$targetPath = [System.IO.Path]::GetFullPath($target)
if ($sourcePath -eq $targetPath) { throw '安装源不能是当前运行文件' }

# Copy and validate before stopping the current instance so package failures cause no downtime.
$transactionOriginal = Join-Path $transactionDir 'original'
$transactionFileNames = @(
    'HeadroomRoute.exe',
    'HeadroomRouteCLI.exe',
    'hr.cmd',
    'HeadroomRoute.previous.exe',
    'HeadroomRouteCLI.previous.exe',
    'hr.previous.cmd',
    'config.json',
    'status.json'
)
$originalFiles = @{}
$hadSettingsBackup = $false
$operationStarted = $false
$started = $null
$rollbackBackupPath = $null
$pathAdded = $false
$pathUpdateFailed = $false
$hadTarget = Test-Path -LiteralPath $target
$hadCliTarget = Test-Path -LiteralPath $cliTarget
$hadShimTarget = Test-Path -LiteralPath $shimTarget
try {
    New-Item -ItemType Directory -Path $transactionDir, $transactionOriginal -Force | Out-Null
    Copy-Item -LiteralPath $sourcePath -Destination $staged -Force
    $cliSourcePath = if ($hasCliSource) { (Resolve-Path -LiteralPath $cliSource).Path }
    if ($hasCliSource) { Copy-Item -LiteralPath $cliSourcePath -Destination $cliStaged -Force }
    if ($hasCliSource) {
        if (Test-Path -LiteralPath $shimSource) {
            Copy-Item -LiteralPath (Resolve-Path -LiteralPath $shimSource).Path -Destination $shimStaged -Force
        } else {
            [System.IO.File]::WriteAllText($shimStaged, $shimContent, [System.Text.Encoding]::ASCII)
        }
    }

    $header = [System.IO.File]::ReadAllBytes($staged)
    if ($header.Length -lt 2 -or $header[0] -ne 0x4d -or $header[1] -ne 0x5a) {
        throw '安装文件不是有效的 Windows 可执行文件'
    }
    Test-PackageSignature $staged $SignaturePolicy $TrustedPublisherThumbprint
    if ($hasCliSource) {
        $cliHeader = [System.IO.File]::ReadAllBytes($cliStaged)
        if ($cliHeader.Length -lt 2 -or $cliHeader[0] -ne 0x4d -or $cliHeader[1] -ne 0x5a) {
            throw 'CLI 安装文件不是有效的 Windows 可执行文件'
        }
        Test-PackageSignature $cliStaged $SignaturePolicy $TrustedPublisherThumbprint
        $shimText = [System.IO.File]::ReadAllText($shimStaged)
        if ($shimText -notmatch '(?i)HeadroomRouteCLI(?:-[^"\\]*)?\.exe' -or $shimText -notmatch '%\*') {
            throw 'CLI 快捷命令文件无效'
        }
    }

    $running = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $targetPath })
    if ($ProcessId) {
        $current = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
        if ($current -and $current.ProcessName -like 'HeadroomRoute*' -and $running.Id -notcontains $current.Id) {
            $running += $current
        }
    }
    $restart = $StartNow -or $running.Count -gt 0
    $cliTargetPath = [System.IO.Path]::GetFullPath($cliTarget)
    $runningCli = @(Get-Process -Name 'HeadroomRouteCLI' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $cliTargetPath })
    $processesToStop = @($running) + @($runningCli) | Sort-Object Id -Unique

    foreach ($relativePath in $transactionFileNames) {
        $currentPath = Join-Path $InstallDir $relativePath
        $originalFiles[$relativePath] = Test-Path -LiteralPath $currentPath
        if ($originalFiles[$relativePath]) {
            Copy-Item -LiteralPath $currentPath -Destination (Join-Path $transactionOriginal $relativePath) -Force
        }
    }
    $hadSettingsBackup = Test-Path -LiteralPath $settingsBackup
    if ($hadSettingsBackup) {
        Copy-Item -LiteralPath $settingsBackup -Destination (Join-Path $transactionOriginal 'update-settings-backup') -Recurse -Force
    }
    $rollbackBackupPath = New-RollbackBackup $InstallDir 'before-upgrade'
    $operationStarted = $true

    if ($processesToStop.Count) {
        $processesToStop | Stop-Process -Force
        foreach ($process in $processesToStop) {
            if (!$process.WaitForExit(10000)) { throw '旧版本未能在 10 秒内退出' }
        }
    }
    if (Test-Path -LiteralPath $settingsBackup) { Remove-Item -LiteralPath $settingsBackup -Recurse -Force }
    foreach ($name in $settingNames) {
        $settingsPresent[$name] = Test-Path -LiteralPath (Join-Path $InstallDir $name)
        if ($settingsPresent[$name]) {
            New-Item -ItemType Directory -Path $settingsBackup -Force | Out-Null
            Copy-Item -LiteralPath (Join-Path $InstallDir $name) -Destination (Join-Path $settingsBackup $name) -Force
        }
    }
    if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
    if ($hadTarget) { Move-Item -LiteralPath $target -Destination $backup -Force }
    Move-Item -LiteralPath $staged -Destination $target -Force
    if ($hasCliSource) {
        if (Test-Path -LiteralPath $cliBackup) { Remove-Item -LiteralPath $cliBackup -Force }
        if ($hadCliTarget) { Move-Item -LiteralPath $cliTarget -Destination $cliBackup -Force }
        Move-Item -LiteralPath $cliStaged -Destination $cliTarget -Force
        if (Test-Path -LiteralPath $shimBackup) { Remove-Item -LiteralPath $shimBackup -Force }
        if ($hadShimTarget) { Move-Item -LiteralPath $shimTarget -Destination $shimBackup -Force }
        Move-Item -LiteralPath $shimStaged -Destination $shimTarget -Force
    }
    if ($restart) {
        $started = Start-Process $target -WindowStyle Hidden -PassThru
        Start-Sleep -Seconds 1
        $started.Refresh()
        if ($started.HasExited) { throw "新版本启动后立即退出（代码 $($started.ExitCode)）" }
    }
} catch {
    $failure = $_
    if ($operationStarted) {
        $restoreErrors = @()
        if ($started -and !$started.HasExited) {
            try {
                Stop-Process -Id $started.Id -Force -ErrorAction Stop
                $started.WaitForExit(10000) | Out-Null
            } catch { $restoreErrors += "停止失败的新版本：$_" }
        }
        foreach ($relativePath in $transactionFileNames) {
            try {
                $currentPath = Join-Path $InstallDir $relativePath
                if ($originalFiles[$relativePath]) {
                    Copy-Item -LiteralPath (Join-Path $transactionOriginal $relativePath) -Destination $currentPath -Force
                } elseif (Test-Path -LiteralPath $currentPath) {
                    Remove-Item -LiteralPath $currentPath -Force
                }
            } catch { $restoreErrors += "恢复 $relativePath 失败：$_" }
        }
        try {
            if (Test-Path -LiteralPath $settingsBackup) { Remove-Item -LiteralPath $settingsBackup -Recurse -Force }
            if ($hadSettingsBackup) {
                Copy-Item -LiteralPath (Join-Path $transactionOriginal 'update-settings-backup') -Destination $settingsBackup -Recurse -Force
            }
        } catch { $restoreErrors += "恢复设置备份失败：$_" }
        if ($restart -and (Test-Path -LiteralPath $target)) {
            try {
                $alreadyRunning = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $targetPath })
                if (!$alreadyRunning.Count) { Start-Process $target -WindowStyle Hidden | Out-Null }
            } catch { $restoreErrors += "重新启动旧版本失败：$_" }
        }
        Remove-Item -LiteralPath $transactionDir -Recurse -Force -ErrorAction SilentlyContinue
        if ($restoreErrors.Count) {
            $backupHint = if ($rollbackBackupPath) { "；可用回滚备份：$rollbackBackupPath" } else { '' }
            throw "升级失败，自动恢复不完整：$failure；$($restoreErrors -join '；')$backupHint"
        }
        throw "升级失败，已恢复升级前完整状态：$failure"
    }
    Remove-Item -LiteralPath $transactionDir -Recurse -Force -ErrorAction SilentlyContinue
    if (!$installDirExisted -and (Test-Path -LiteralPath $InstallDir) -and
        !@(Get-ChildItem -LiteralPath $InstallDir -Force).Count) {
        Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
    }
    throw
}
Remove-Item -LiteralPath $transactionDir -Recurse -Force -ErrorAction SilentlyContinue

if (!$SkipPathUpdate -and (Test-Path $shimTarget) -and (Test-Path $cliTarget)) {
    try {
        $pathAdded = Add-UserPathEntry $InstallDir
    } catch {
        $pathUpdateFailed = $true
        Write-Warning "无法将 $InstallDir 加入当前用户 PATH，请手动添加后重新打开终端。"
    }
}

Write-Host "已安装到 $InstallDir"
if ($hadTarget) { Write-Host "旧版本备份：$backup" }
if (Test-Path $settingsBackup) { Write-Host "设置备份：$settingsBackup" }
if ($rollbackBackupPath) { Write-Host "升级前回滚备份：$rollbackBackupPath" }
if ((Test-Path $shimTarget) -and (Test-Path $cliTarget)) {
    if ($SkipPathUpdate) {
        Write-Host '已安装快捷命令 hr.cmd；已跳过用户 PATH 更新（-SkipPathUpdate）。'
    } elseif (!$pathUpdateFailed) {
        Write-Host '已注册快捷命令 hr；请重新打开终端后使用：hr claude 或 hr codex'
    }
}
Write-Host '首次启动前，请从 TrafficMonitor 菜单退出旧 RouteAgent，或结束旧 Headroom RouteAgent 进程。'
