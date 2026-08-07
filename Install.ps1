param(
    [switch]$StartNow,
    [int]$ProcessId = 0,
    [switch]$SkipPathUpdate,
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

$project = Split-Path -Parent $MyInvocation.MyCommand.Path
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
$shimSource = Join-Path $project 'hr.cmd'

New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
$target = Join-Path $InstallDir 'HeadroomRoute.exe'
$staged = Join-Path $InstallDir 'HeadroomRoute.new.exe'
$backup = Join-Path $InstallDir 'HeadroomRoute.previous.exe'
$cliTarget = Join-Path $InstallDir 'HeadroomRouteCLI.exe'
$cliStaged = Join-Path $InstallDir 'HeadroomRouteCLI.new.exe'
$cliBackup = Join-Path $InstallDir 'HeadroomRouteCLI.previous.exe'
$shimTarget = Join-Path $InstallDir 'hr.cmd'
$shimStaged = Join-Path $InstallDir 'hr.new.cmd'
$shimBackup = Join-Path $InstallDir 'hr.previous.cmd'
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
$settingsBackup = Join-Path $InstallDir 'update-settings-backup'
$settingNames = @('config.json', 'status.json')
$settingsPresent = @{}
$sourcePath = (Resolve-Path $source).Path
$targetPath = [System.IO.Path]::GetFullPath($target)
if ($sourcePath -eq $targetPath) { throw '安装源不能是当前运行文件' }

# Copy before stopping the current instance so a read/disk failure causes no downtime.
Copy-Item $sourcePath $staged -Force
$cliSourcePath = if ($hasCliSource) { (Resolve-Path $cliSource).Path }
if ($hasCliSource) { Copy-Item $cliSourcePath $cliStaged -Force }
if ($hasCliSource) {
    if (Test-Path $shimSource) {
        Copy-Item (Resolve-Path $shimSource).Path $shimStaged -Force
    } else {
        [System.IO.File]::WriteAllText($shimStaged, $shimContent, [System.Text.Encoding]::ASCII)
    }
}
$header = [System.IO.File]::ReadAllBytes($staged)
if ($header.Length -lt 2 -or $header[0] -ne 0x4d -or $header[1] -ne 0x5a) {
    Remove-Item $staged -Force
    if (Test-Path $cliStaged) { Remove-Item $cliStaged -Force }
    if (Test-Path $shimStaged) { Remove-Item $shimStaged -Force }
    throw '安装文件不是有效的 Windows 可执行文件'
}
if ($hasCliSource) {
    $cliHeader = [System.IO.File]::ReadAllBytes($cliStaged)
    if ($cliHeader.Length -lt 2 -or $cliHeader[0] -ne 0x4d -or $cliHeader[1] -ne 0x5a) {
        Remove-Item $staged, $cliStaged, $shimStaged -Force
        throw 'CLI 安装文件不是有效的 Windows 可执行文件'
    }
    $shimText = [System.IO.File]::ReadAllText($shimStaged)
    if ($shimText -notmatch '(?i)HeadroomRouteCLI(?:-[^"\\]*)?\.exe' -or $shimText -notmatch '%\*') {
        Remove-Item $staged, $cliStaged, $shimStaged -Force
        throw 'CLI 快捷命令文件无效'
    }
}

$running = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $targetPath })
if ($ProcessId) {
    $current = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($current -and $running.Id -notcontains $current.Id) { $running += $current }
}
$restart = $StartNow -or $running.Count -gt 0
$cliTargetPath = [System.IO.Path]::GetFullPath($cliTarget)
$runningCli = @(Get-Process -Name 'HeadroomRouteCLI' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $cliTargetPath })
$processesToStop = @($running) + @($runningCli)
$hadTarget = Test-Path $target
$hadCliTarget = Test-Path $cliTarget
$hadShimTarget = Test-Path $shimTarget
$targetMoved = $false
$newInstalled = $false
$cliTargetMoved = $false
$cliInstalled = $false
$shimTargetMoved = $false
$shimInstalled = $false
$pathAdded = $false
$pathUpdateFailed = $false
try {
    if ($processesToStop.Count) {
        $processesToStop | Stop-Process -Force
        foreach ($process in $processesToStop) {
            if (!$process.WaitForExit(10000)) { throw '旧版本未能在 10 秒内退出' }
        }
    }
    if (Test-Path $settingsBackup) { Remove-Item $settingsBackup -Recurse -Force }
    foreach ($name in $settingNames) {
        $settingsPresent[$name] = Test-Path (Join-Path $InstallDir $name)
        if ($settingsPresent[$name]) {
            New-Item -ItemType Directory -Path $settingsBackup -Force | Out-Null
            Copy-Item (Join-Path $InstallDir $name) (Join-Path $settingsBackup $name) -Force
        }
    }
    if (Test-Path $backup) { Remove-Item $backup -Force }
    if ($hadTarget) {
        Move-Item $target $backup -Force
        $targetMoved = $true
    }
    Move-Item $staged $target -Force
    $newInstalled = $true
    if ($hasCliSource) {
        if (Test-Path $cliBackup) { Remove-Item $cliBackup -Force }
        if ($hadCliTarget) {
            Move-Item $cliTarget $cliBackup -Force
            $cliTargetMoved = $true
        }
        Move-Item $cliStaged $cliTarget -Force
        $cliInstalled = $true
        if (Test-Path $shimBackup) { Remove-Item $shimBackup -Force }
        if ($hadShimTarget) {
            Move-Item $shimTarget $shimBackup -Force
            $shimTargetMoved = $true
        }
        Move-Item $shimStaged $shimTarget -Force
        $shimInstalled = $true
    }
    if ($restart) {
        $started = Start-Process $target -WindowStyle Hidden -PassThru
        Start-Sleep -Seconds 1
        $started.Refresh()
        if ($started.HasExited) { throw "新版本启动后立即退出（代码 $($started.ExitCode)）" }
    }
} catch {
    $failure = $_
    if (Test-Path $staged) { Remove-Item $staged -Force }
    if (Test-Path $cliStaged) { Remove-Item $cliStaged -Force }
    if (Test-Path $shimStaged) { Remove-Item $shimStaged -Force }
    if ($targetMoved -and (Test-Path $backup)) {
        if (Test-Path $target) { Remove-Item $target -Force }
        Move-Item $backup $target -Force
    } elseif (!$hadTarget -and $newInstalled -and (Test-Path $target)) {
        Remove-Item $target -Force
    }
    if ($cliTargetMoved -and (Test-Path $cliBackup)) {
        if (Test-Path $cliTarget) { Remove-Item $cliTarget -Force }
        Move-Item $cliBackup $cliTarget -Force
    } elseif (!$hadCliTarget -and $cliInstalled -and (Test-Path $cliTarget)) {
        Remove-Item $cliTarget -Force
    }
    if ($shimTargetMoved -and (Test-Path $shimBackup)) {
        if (Test-Path $shimTarget) { Remove-Item $shimTarget -Force }
        Move-Item $shimBackup $shimTarget -Force
    } elseif (!$hadShimTarget -and $shimInstalled -and (Test-Path $shimTarget)) {
        Remove-Item $shimTarget -Force
    }
    if ($pathAdded) {
        Remove-UserPathEntry $InstallDir
    }
    foreach ($name in $settingNames) {
        $saved = Join-Path $settingsBackup $name
        $current = Join-Path $InstallDir $name
        if ($settingsPresent[$name] -and (Test-Path $saved)) {
            Copy-Item $saved $current -Force
        } elseif ($settingsPresent.ContainsKey($name) -and !$settingsPresent[$name] -and (Test-Path $current)) {
            Remove-Item $current -Force
        }
    }
    if ($restart -and (Test-Path $target)) { Start-Process $target -WindowStyle Hidden | Out-Null }
    throw "升级失败，已恢复旧版本：$failure"
}

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
if ((Test-Path $shimTarget) -and (Test-Path $cliTarget)) {
    if ($SkipPathUpdate) {
        Write-Host '已安装快捷命令 hr.cmd；已跳过用户 PATH 更新（-SkipPathUpdate）。'
    } elseif (!$pathUpdateFailed) {
        Write-Host '已注册快捷命令 hr；请重新打开终端后使用：hr claude 或 hr codex'
    }
}
Write-Host '首次启动前，请从 TrafficMonitor 菜单退出旧 RouteAgent，或结束旧 Headroom RouteAgent 进程。'
