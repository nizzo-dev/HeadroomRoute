param([switch]$StartNow)
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$versionedSource = Join-Path $project 'dist\HeadroomRoute-0.3.0.exe'
$source = if (Test-Path $versionedSource) { $versionedSource } else { Join-Path $project 'dist\HeadroomRoute.exe' }
if (!(Test-Path $source)) { throw '请先运行 Build.ps1' }
$installDir = Join-Path $env:LOCALAPPDATA 'HeadroomRoute'
New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item $source (Join-Path $installDir 'HeadroomRoute.exe') -Force
Write-Host "已安装到 $installDir"
Write-Host '首次启动前，请从 TrafficMonitor 菜单退出旧 RouteAgent，或结束旧 Headroom RouteAgent 进程。'
if ($StartNow) { Start-Process (Join-Path $installDir 'HeadroomRoute.exe') }
