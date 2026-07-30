param([switch]$StartNow)
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$manifest = Join-Path $project 'Cargo.toml'
$version = if (Test-Path $manifest) { (Select-String -Path $manifest -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value }
$versionedSource = if ($version) { Join-Path $project "dist\HeadroomRoute-$version.exe" }
$packagedSource = Get-ChildItem -Path $project -Filter 'HeadroomRoute-*.exe' | Select-Object -First 1 -ExpandProperty FullName
$source = if ($versionedSource -and (Test-Path $versionedSource)) { $versionedSource } elseif ($packagedSource) { $packagedSource } else { Join-Path $project 'dist\HeadroomRoute.exe' }
if (!(Test-Path $source)) { throw '请先运行 Build.ps1' }
$installDir = Join-Path $env:LOCALAPPDATA 'HeadroomRoute'
New-Item -ItemType Directory -Path $installDir -Force | Out-Null
$target = Join-Path $installDir 'HeadroomRoute.exe'
$running = Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $target }
$restart = $StartNow -or $null -ne $running
if ($running) { $running | Stop-Process -Force; $running | Wait-Process -Timeout 10 }
Copy-Item $source $target -Force
Write-Host "已安装到 $installDir"
Write-Host '首次启动前，请从 TrafficMonitor 菜单退出旧 RouteAgent，或结束旧 Headroom RouteAgent 进程。'
if ($restart) { Start-Process $target }
