$ErrorActionPreference = 'Stop'
$root = Join-Path ([System.IO.Path]::GetTempPath()) ("headroom-route-install-test-" + [guid]::NewGuid().ToString('N'))
$package = Join-Path $root 'package'
$install = Join-Path $root 'app'

function Compile-Dummy([string]$Path, [string]$Type, [string]$Marker, [bool]$StayRunning, [bool]$CorruptSettings) {
    $wait = if ($StayRunning) { 'Thread.Sleep(Timeout.Infinite);' } else { '' }
    $corrupt = if ($CorruptSettings) { 'File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "config.json"), "corrupted"); File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "status.json"), "corrupted");' } else { '' }
    $code = @"
using System;
using System.IO;
using System.Threading;
public static class $Type {
    public static void Main() {
        File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "$Marker"), "started");
        $corrupt
        $wait
    }
}
"@
    Add-Type -TypeDefinition $code -OutputAssembly $Path -OutputType ConsoleApplication
}

function File-Hash([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try { [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '') }
    finally { $sha256.Dispose(); $stream.Dispose() }
}

function Wait-ForFile([string]$Path) {
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while (!(Test-Path $Path) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 50 }
    if (!(Test-Path $Path)) { throw "Timed out waiting for $Path" }
}

try {
    New-Item -ItemType Directory -Path $package, $install | Out-Null
    $target = Join-Path $install 'HeadroomRoute.exe'
    $source = Join-Path $package 'HeadroomRoute-9.9.9.exe'
    $bad = Join-Path $root 'bad.exe'
    Compile-Dummy $target 'OldVersion' 'old.started' $true $false
    Compile-Dummy $source 'NewVersion' 'new.started' $true $false
    Compile-Dummy $bad 'BadVersion' 'bad.started' $false $true
    Copy-Item (Join-Path $PSScriptRoot 'Install.ps1') $package
    Set-Content (Join-Path $install 'config.json') 'original-config' -Encoding UTF8
    Set-Content (Join-Path $install 'status.json') 'original-status' -Encoding UTF8
    $oldHash = File-Hash $target
    $newHash = File-Hash $source
    $configHash = File-Hash (Join-Path $install 'config.json')
    $statusHash = File-Hash (Join-Path $install 'status.json')

    $oldProcess = Start-Process $target -WindowStyle Hidden -PassThru
    $portableProcess = Start-Process $source -WindowStyle Hidden -PassThru
    Wait-ForFile (Join-Path $install 'old.started')
    Wait-ForFile (Join-Path $package 'new.started')
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install -ProcessId $portableProcess.Id
    if ($LASTEXITCODE -ne 0) { throw 'Running upgrade failed' }
    Wait-ForFile (Join-Path $install 'new.started')
    $oldProcess.Refresh()
    $portableProcess.Refresh()
    if (!$oldProcess.HasExited) { throw 'Old process was not stopped' }
    if (!$portableProcess.HasExited) { throw 'Portable process was not stopped' }
    if ((File-Hash $target) -ne $newHash) { throw 'New executable was not installed' }
    if ((File-Hash (Join-Path $install 'HeadroomRoute.previous.exe')) -ne $oldHash) { throw 'Previous executable was not preserved' }
    if ((File-Hash (Join-Path $install 'config.json')) -ne $configHash) { throw 'Successful upgrade changed config.json' }
    if ((File-Hash (Join-Path $install 'status.json')) -ne $statusHash) { throw 'Successful upgrade changed status.json' }
    if ((File-Hash (Join-Path $install 'update-settings-backup\config.json')) -ne $configHash) { throw 'Config backup was not created' }
    if ((File-Hash (Join-Path $install 'update-settings-backup\status.json')) -ne $statusHash) { throw 'Status backup was not created' }

    Remove-Item (Join-Path $install 'new.started') -Force
    Copy-Item $bad $source -Force
    $ErrorActionPreference = 'Continue'
    $rollbackOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install 2>&1
    $rollbackExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($rollbackExit -eq 0) { throw 'Broken upgrade unexpectedly succeeded' }
    if (($rollbackOutput | Out-String) -notmatch '已恢复旧版本') { throw 'Rollback failure was not reported' }
    Wait-ForFile (Join-Path $install 'new.started')
    if ((File-Hash $target) -ne $newHash) { throw 'Failed upgrade did not restore the previous executable' }
    if ((File-Hash (Join-Path $install 'config.json')) -ne $configHash) { throw 'Rollback did not restore config.json' }
    if ((File-Hash (Join-Path $install 'status.json')) -ne $statusHash) { throw 'Rollback did not restore status.json' }

    Write-Host 'Install upgrade and rollback test passed'
}
finally {
    Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$root*" } | Stop-Process -Force
    $tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    $resolvedRoot = [System.IO.Path]::GetFullPath($root)
    if ($resolvedRoot.StartsWith($tempRoot) -and (Split-Path -Leaf $resolvedRoot).StartsWith('headroom-route-install-test-')) {
        Remove-Item $resolvedRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
