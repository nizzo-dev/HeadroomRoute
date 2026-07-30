param(
    [switch]$StartNow,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'HeadroomRoute')
)
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$manifest = Join-Path $project 'Cargo.toml'
$version = if (Test-Path $manifest) { (Select-String -Path $manifest -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value }
$versionedSource = if ($version) { Join-Path $project "dist\HeadroomRoute-$version.exe" }
$packagedSource = Get-ChildItem -Path $project -Filter 'HeadroomRoute-*.exe' | Select-Object -First 1 -ExpandProperty FullName
$source = if ($versionedSource -and (Test-Path $versionedSource)) { $versionedSource } elseif ($packagedSource) { $packagedSource } else { Join-Path $project 'dist\HeadroomRoute.exe' }
if (!(Test-Path $source)) { throw '请先运行 Build.ps1，或从发布 ZIP 中执行此脚本' }

New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
$target = Join-Path $InstallDir 'HeadroomRoute.exe'
$staged = Join-Path $InstallDir 'HeadroomRoute.new.exe'
$backup = Join-Path $InstallDir 'HeadroomRoute.previous.exe'
$sourcePath = (Resolve-Path $source).Path
$targetPath = [System.IO.Path]::GetFullPath($target)
if ($sourcePath -eq $targetPath) { throw '安装源不能是当前运行文件' }

# Copy before stopping the current instance so a read/disk failure causes no downtime.
Copy-Item $sourcePath $staged -Force
$header = [System.IO.File]::ReadAllBytes($staged)
if ($header.Length -lt 2 -or $header[0] -ne 0x4d -or $header[1] -ne 0x5a) {
    Remove-Item $staged -Force
    throw '安装文件不是有效的 Windows 可执行文件'
}

$running = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $targetPath })
$restart = $StartNow -or $running.Count -gt 0
$hadTarget = Test-Path $target
try {
    if ($running.Count) {
        $running | Stop-Process -Force
        foreach ($process in $running) {
            if (!$process.WaitForExit(10000)) { throw '旧版本未能在 10 秒内退出' }
        }
    }
    if (Test-Path $backup) { Remove-Item $backup -Force }
    if ($hadTarget) { Move-Item $target $backup -Force }
    Move-Item $staged $target -Force
    if ($restart) {
        $started = Start-Process $target -WindowStyle Hidden -PassThru
        Start-Sleep -Seconds 1
        $started.Refresh()
        if ($started.HasExited) { throw "新版本启动后立即退出（代码 $($started.ExitCode)）" }
    }
} catch {
    $failure = $_
    if (Test-Path $staged) { Remove-Item $staged -Force }
    if (Test-Path $backup) {
        if (Test-Path $target) { Remove-Item $target -Force }
        Move-Item $backup $target -Force
        if ($restart) { Start-Process $target -WindowStyle Hidden | Out-Null }
    } elseif (!$hadTarget -and (Test-Path $target)) {
        Remove-Item $target -Force
    }
    throw "升级失败，已恢复旧版本：$failure"
}

Write-Host "已安装到 $InstallDir"
if ($hadTarget) { Write-Host "旧版本备份：$backup" }
Write-Host '首次启动前，请从 TrafficMonitor 菜单退出旧 RouteAgent，或结束旧 Headroom RouteAgent 进程。'
