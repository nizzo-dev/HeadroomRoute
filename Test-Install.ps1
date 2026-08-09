$ErrorActionPreference = 'Stop'
$root = Join-Path ([System.IO.Path]::GetTempPath()) ("headroom-route-install-test-" + [guid]::NewGuid().ToString('N'))
$package = Join-Path $root 'package'
$install = Join-Path $root 'app'
$originalUserPath = [Environment]::GetEnvironmentVariable('Path', 'User')

function Compile-Dummy([string]$Path, [string]$Type, [string]$Marker, [bool]$StayRunning, [bool]$CorruptSettings, [bool]$RecordArguments = $false) {
    $wait = if ($StayRunning) { 'Thread.Sleep(Timeout.Infinite);' } else { '' }
    $corrupt = if ($CorruptSettings) { 'File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "config.json"), "corrupted"); File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "status.json"), "corrupted");' } else { '' }
    $recordArgumentsCode = if ($RecordArguments) { 'File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "cli.args"), String.Join("|", Environment.GetCommandLineArgs()));' } else { '' }
    $code = @"
using System;
using System.IO;
using System.Threading;
public static class $Type {
    public static void Main() {
        File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "$Marker"), "started");
        $corrupt
        $recordArgumentsCode
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
    $cliTarget = Join-Path $install 'HeadroomRouteCLI.exe'
    $cliSource = Join-Path $package 'HeadroomRouteCLI-9.9.9.exe'
    $shimTarget = Join-Path $install 'hr.cmd'
    $bad = Join-Path $root 'bad.exe'
    Compile-Dummy $target 'OldVersion' 'old.started' $true $false
    Compile-Dummy $source 'NewVersion' 'new.started' $true $false
    Compile-Dummy $cliTarget 'OldCliVersion' 'old-cli.started' $false $false
    Compile-Dummy $cliSource 'NewCliVersion' 'new-cli.started' $false $false $true
    Compile-Dummy $bad 'BadVersion' 'bad.started' $false $true
    Copy-Item (Join-Path $PSScriptRoot 'Install.ps1') $package
    Copy-Item (Join-Path $PSScriptRoot 'hr.cmd') $package
    Set-Content $shimTarget '@echo old-shim' -Encoding ascii
    Set-Content (Join-Path $install 'config.json') 'original-config' -Encoding UTF8
    Set-Content (Join-Path $install 'status.json') 'original-status' -Encoding UTF8
    $oldHash = File-Hash $target
    $newHash = File-Hash $source
    $oldCliHash = File-Hash $cliTarget
    $newCliHash = File-Hash $cliSource
    $oldShimHash = File-Hash $shimTarget
    $configHash = File-Hash (Join-Path $install 'config.json')
    $statusHash = File-Hash (Join-Path $install 'status.json')

    Push-Location $package
    try {
        & $env:ComSpec /d /s /c 'hr codex --flag "hello world"'
        if ($LASTEXITCODE -ne 0) { throw 'Packaged CLI shim failed from CMD' }
    } finally {
        Pop-Location
    }
    Wait-ForFile (Join-Path $package 'cli.args')
    $packagedArgs = [System.IO.File]::ReadAllText((Join-Path $package 'cli.args'))
    if ($packagedArgs -notmatch '\|codex\|--flag\|hello world$') { throw "Packaged CLI shim did not forward CMD arguments: $packagedArgs" }
    Remove-Item (Join-Path $package 'cli.args'), (Join-Path $package 'new-cli.started') -Force

    $warnInstall = Join-Path $root 'warn-app'
    New-Item -ItemType Directory -Path $warnInstall | Out-Null
    $pathBeforeWarnPolicy = [Environment]::GetEnvironmentVariable('Path', 'User')
    $ErrorActionPreference = 'Continue'
    $warnPolicyOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $warnInstall -SkipPathUpdate 2>&1
    $warnPolicyExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($warnPolicyExit -ne 0) { throw 'Default Warn policy rejected an unsigned dev package' }
    if (($warnPolicyOutput | Out-String) -notmatch '未签名') { throw 'Default Warn policy did not warn about the unsigned package' }
    if ((File-Hash (Join-Path $warnInstall 'HeadroomRoute.exe')) -ne $newHash) { throw 'Warn-policy install did not install the package' }
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforeWarnPolicy) { throw 'Warn-policy install unexpectedly modified the user PATH' }

    $oldProcess = Start-Process $target -WindowStyle Hidden -PassThru
    $portableProcess = Start-Process $source -WindowStyle Hidden -PassThru
    Wait-ForFile (Join-Path $install 'old.started')
    Wait-ForFile (Join-Path $package 'new.started')
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install -ProcessId $portableProcess.Id -SkipPathUpdate -SignaturePolicy Skip
    if ($LASTEXITCODE -ne 0) { throw 'Running upgrade failed' }
    Wait-ForFile (Join-Path $install 'new.started')
    $oldProcess.Refresh()
    $portableProcess.Refresh()
    if (!$oldProcess.HasExited) { throw 'Old process was not stopped' }
    if (!$portableProcess.HasExited) { throw 'Portable process was not stopped' }
    if ((File-Hash $target) -ne $newHash) { throw 'New executable was not installed' }
    if ((File-Hash $cliTarget) -ne $newCliHash) { throw 'New CLI executable was not installed' }
    $shimText = [System.IO.File]::ReadAllText($shimTarget)
    if ($shimText -notmatch '(?i)HeadroomRouteCLI(?:-[^"\\]*)?\.exe' -or $shimText -notmatch '%\*') { throw 'CLI shim was not installed correctly' }
    $newShimHash = File-Hash $shimTarget
    if ((File-Hash (Join-Path $install 'hr.previous.cmd')) -ne $oldShimHash) { throw 'Previous CLI shim was not preserved' }
    Remove-Item (Join-Path $install 'cli.args') -Force -ErrorAction SilentlyContinue
    $processPath = $env:Path
    Push-Location $root
    try {
        $env:Path = "$install;$processPath"
        & $env:ComSpec /d /s /c 'hr claude --flag "hello world"'
        if ($LASTEXITCODE -ne 0) { throw 'Installed CLI shim failed from CMD PATH lookup' }
    } finally {
        $env:Path = $processPath
        Pop-Location
    }
    Wait-ForFile (Join-Path $install 'cli.args')
    $forwardedArgs = [System.IO.File]::ReadAllText((Join-Path $install 'cli.args'))
    if ($forwardedArgs -notmatch '\|claude\|--flag\|hello world$') { throw "CLI shim did not forward arguments: $forwardedArgs" }
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $originalUserPath) { throw '-SkipPathUpdate unexpectedly changed the user PATH' }
    if ((File-Hash (Join-Path $install 'HeadroomRoute.previous.exe')) -ne $oldHash) { throw 'Previous executable was not preserved' }
    if ((File-Hash (Join-Path $install 'HeadroomRouteCLI.previous.exe')) -ne $oldCliHash) { throw 'Previous CLI executable was not preserved' }
    if ((File-Hash (Join-Path $install 'config.json')) -ne $configHash) { throw 'Successful upgrade changed config.json' }
    if ((File-Hash (Join-Path $install 'status.json')) -ne $statusHash) { throw 'Successful upgrade changed status.json' }
    if ((File-Hash (Join-Path $install 'update-settings-backup\config.json')) -ne $configHash) { throw 'Config backup was not created' }
    if ((File-Hash (Join-Path $install 'update-settings-backup\status.json')) -ne $statusHash) { throw 'Status backup was not created' }

    $rollbackBackups = @(Get-ChildItem -LiteralPath (Join-Path $install 'rollback') -Directory)
    if ($rollbackBackups.Count -ne 1) { throw "Successful upgrade created $($rollbackBackups.Count) rollback backups instead of one" }
    $upgradeRollbackBackup = $rollbackBackups[0].FullName
    $rollbackManifest = Get-Content -LiteralPath (Join-Path $upgradeRollbackBackup 'manifest.json') -Raw | ConvertFrom-Json
    $rollbackMain = @($rollbackManifest.files | Where-Object { $_.relative_path -eq 'HeadroomRoute.exe' })
    $rollbackCli = @($rollbackManifest.files | Where-Object { $_.relative_path -eq 'HeadroomRouteCLI.exe' })
    if ($rollbackManifest.schema_version -ne 1 -or $rollbackMain.Count -ne 1 -or $rollbackMain[0].sha256 -ine $oldHash) { throw 'Rollback manifest did not record the previous main executable' }
    if ($rollbackCli.Count -ne 1 -or $rollbackCli[0].sha256 -ine $oldCliHash) { throw 'Rollback manifest did not record the previous CLI executable' }
    if ((File-Hash (Join-Path $upgradeRollbackBackup 'config.json')) -ne $configHash) { throw 'Rollback backup did not preserve config.json' }
    if ((File-Hash (Join-Path $upgradeRollbackBackup 'status.json')) -ne $statusHash) { throw 'Rollback backup did not preserve status.json' }

    $pathBeforeSignatureGate = [Environment]::GetEnvironmentVariable('Path', 'User')
    $ErrorActionPreference = 'Continue'
    $signatureOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install -SkipPathUpdate -SignaturePolicy Require 2>&1
    $signatureExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($signatureExit -eq 0) { throw 'Required-signature install unexpectedly accepted unsigned binaries' }
    if (($signatureOutput | Out-String) -notmatch '签名') { throw 'Required-signature failure did not explain the signature requirement' }
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforeSignatureGate) { throw 'Signature gate unexpectedly modified the user PATH' }
    if ((File-Hash $target) -ne $newHash -or (File-Hash $cliTarget) -ne $newCliHash) { throw 'Rejected unsigned install changed installed binaries' }
    $runningAfterSignatureGate = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq [System.IO.Path]::GetFullPath($target) })
    if (!$runningAfterSignatureGate.Count) { throw 'Signature gate stopped the running installed version' }

    $selfSignedCertificate = $null
    try {
        $selfSignedCertificate = New-SelfSignedCertificate -Subject 'CN=HeadroomRoute Route Test' -Type CodeSigningCert -CertStoreLocation 'Cert:\CurrentUser\My' -KeyAlgorithm RSA -KeyLength 2048 -HashAlgorithm SHA256 -NotAfter (Get-Date).AddDays(1) -ErrorAction Stop
    } catch {
        Write-Warning "跳过链不受信任的签名拒绝测试（无法创建自签名证书）：$_"
    }
    if ($selfSignedCertificate) {
        try {
            $sigPackage = Join-Path $root 'sig-package'
            New-Item -ItemType Directory -Path $sigPackage | Out-Null
            $sigExe = Join-Path $sigPackage 'HeadroomRoute-9.9.9.exe'
            Copy-Item $source $sigExe
            Set-AuthenticodeSignature -FilePath $sigExe -Certificate $selfSignedCertificate -HashAlgorithm SHA256 | Out-Null
            if ((Get-AuthenticodeSignature -LiteralPath $sigExe).Status -eq [System.Management.Automation.SignatureStatus]::NotSigned) {
                throw 'Self-signed certificate failed to embed a signature'
            }
            Copy-Item (Join-Path $PSScriptRoot 'Install.ps1') $sigPackage
            $sigInstall = Join-Path $root 'sig-app'
            New-Item -ItemType Directory -Path $sigInstall | Out-Null
            $pathBeforeInvalidSignature = [Environment]::GetEnvironmentVariable('Path', 'User')
            $ErrorActionPreference = 'Continue'
            $invalidSignatureOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $sigPackage 'Install.ps1') -InstallDir $sigInstall -SkipPathUpdate 2>&1
            $invalidSignatureExit = $LASTEXITCODE
            $ErrorActionPreference = 'Stop'
            if ($invalidSignatureExit -eq 0) { throw 'Signature with an untrusted chain was unexpectedly accepted' }
            if (($invalidSignatureOutput | Out-String) -notmatch '签名') { throw 'Invalid-signature rejection did not explain the signature problem' }
            if (Test-Path -LiteralPath (Join-Path $sigInstall 'HeadroomRoute.exe')) { throw 'Rejected invalid-signature install still installed a binary' }
            if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforeInvalidSignature) { throw 'Invalid-signature rejection unexpectedly modified the user PATH' }
        } finally {
            Remove-Item "Cert:\CurrentUser\My\$($selfSignedCertificate.Thumbprint)" -Force -ErrorAction SilentlyContinue
        }
    }

    $pathBeforePinnedPolicy = [Environment]::GetEnvironmentVariable('Path', 'User')
    $ErrorActionPreference = 'Continue'
    $pinnedOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install -SkipPathUpdate -TrustedPublisherThumbprint ('A' * 40) 2>&1
    $pinnedExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($pinnedExit -eq 0) { throw 'Pinned publisher thumbprint unexpectedly accepted unsigned binaries' }
    if (($pinnedOutput | Out-String) -notmatch '签名策略') { throw 'Pinned-thumbprint rejection did not explain the signature requirement' }
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforePinnedPolicy) { throw 'Pinned-thumbprint rejection unexpectedly modified the user PATH' }
    if ((File-Hash $target) -ne $newHash -or (File-Hash $cliTarget) -ne $newCliHash) { throw 'Rejected pinned-thumbprint install changed installed binaries' }
    $runningAfterPinnedGate = @(Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq [System.IO.Path]::GetFullPath($target) })
    if (!$runningAfterPinnedGate.Count) { throw 'Pinned-thumbprint gate stopped the running installed version' }

    Remove-Item (Join-Path $install 'new.started') -Force
    Set-Content (Join-Path $install 'update-settings-backup\keep.marker') 'keep-original-backup' -Encoding ascii
    $previousMainHash = File-Hash (Join-Path $install 'HeadroomRoute.previous.exe')
    $previousCliHash = File-Hash (Join-Path $install 'HeadroomRouteCLI.previous.exe')
    $previousShimHash = File-Hash (Join-Path $install 'hr.previous.cmd')
    Copy-Item $bad $source -Force
    $pathBeforeRollback = [Environment]::GetEnvironmentVariable('Path', 'User')
    $ErrorActionPreference = 'Continue'
    $rollbackOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -InstallDir $install -SkipPathUpdate -SignaturePolicy Skip 2>&1
    $rollbackExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforeRollback) { throw '-SkipPathUpdate unexpectedly changed the user PATH' }
    if ($rollbackExit -eq 0) { throw 'Broken upgrade unexpectedly succeeded' }
    if (($rollbackOutput | Out-String) -notmatch '已恢复升级前完整状态') { throw 'Rollback failure was not reported' }
    Wait-ForFile (Join-Path $install 'new.started')
    if ((File-Hash $target) -ne $newHash) { throw 'Failed upgrade did not restore the previous executable' }
    if ((File-Hash $cliTarget) -ne $newCliHash) { throw 'Failed upgrade did not restore the previous CLI executable' }
    if ((File-Hash $shimTarget) -ne $newShimHash) { throw 'Failed upgrade did not restore the previous CLI shim' }
    if ((File-Hash (Join-Path $install 'config.json')) -ne $configHash) { throw 'Rollback did not restore config.json' }
    if ((File-Hash (Join-Path $install 'status.json')) -ne $statusHash) { throw 'Rollback did not restore status.json' }
    if ((File-Hash (Join-Path $install 'HeadroomRoute.previous.exe')) -ne $previousMainHash) { throw 'Rollback changed the pre-existing main executable backup' }
    if ((File-Hash (Join-Path $install 'HeadroomRouteCLI.previous.exe')) -ne $previousCliHash) { throw 'Rollback changed the pre-existing CLI executable backup' }
    if ((File-Hash (Join-Path $install 'hr.previous.cmd')) -ne $previousShimHash) { throw 'Rollback changed the pre-existing shim backup' }
    if ([System.IO.File]::ReadAllText((Join-Path $install 'update-settings-backup\keep.marker')) -notmatch 'keep-original-backup') { throw 'Rollback did not restore the complete pre-existing settings backup directory' }

    $updatePackage = Join-Path $root 'update-package'
    $updateInstall = Join-Path $root 'update-app'
    New-Item -ItemType Directory -Path $updatePackage, $updateInstall | Out-Null
    Compile-Dummy (Join-Path $updatePackage 'HeadroomRoute-9.9.9.exe') 'UpdateVersion' 'update.started' $true $false
    Compile-Dummy (Join-Path $updatePackage 'HeadroomRouteCLI-9.9.9.exe') 'UpdateCliVersion' 'update-cli.started' $false $false $true
    Copy-Item (Join-Path $PSScriptRoot 'Install.ps1') $updatePackage
    Copy-Item (Join-Path $PSScriptRoot 'hr.cmd') $updatePackage
    $updatePathBefore = [Environment]::GetEnvironmentVariable('Path', 'User')
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $updatePackage 'Install.ps1') -StartNow -SkipPathUpdate -SignaturePolicy Skip -InstallDir $updateInstall -ProcessId 999999
    if ($LASTEXITCODE -ne 0) { throw 'Update-path install failed' }
    Wait-ForFile (Join-Path $updateInstall 'update.started')
    if (!(Test-Path (Join-Path $updateInstall 'HeadroomRouteCLI.exe'))) { throw 'Update path did not provision the CLI executable' }
    $updateShimText = [System.IO.File]::ReadAllText((Join-Path $updateInstall 'hr.cmd'))
    if ($updateShimText -notmatch '%\*') { throw 'Update path did not provision the CLI shim' }
    $updatePathAfter = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($updatePathAfter -ne $updatePathBefore) { throw 'Update-path install unexpectedly modified the user PATH' }
    $savedPath = $env:Path
    Push-Location $root
    try {
        $env:Path = "$updateInstall;$savedPath"
        & $env:ComSpec /d /s /c 'hr codex --flag "hello world"'
        if ($LASTEXITCODE -ne 0) { throw 'Update-path shim failed from CMD PATH lookup' }
    } finally {
        $env:Path = $savedPath
        Pop-Location
    }
    Wait-ForFile (Join-Path $updateInstall 'cli.args')
    $updateArgs = [System.IO.File]::ReadAllText((Join-Path $updateInstall 'cli.args'))
    if ($updateArgs -notmatch '\|codex\|--flag\|hello world$') { throw "Update-path CLI shim did not forward arguments: $updateArgs" }

    $brokenPackage = Join-Path $root 'broken-package'
    New-Item -ItemType Directory -Path $brokenPackage | Out-Null
    Compile-Dummy (Join-Path $brokenPackage 'HeadroomRoute-9.9.9.exe') 'BrokenUpdateVersion' 'broken-update.started' $false $false
    Copy-Item (Join-Path $PSScriptRoot 'Install.ps1') $brokenPackage
    $ErrorActionPreference = 'Continue'
    $missingCliOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $brokenPackage 'Install.ps1') -SkipPathUpdate -SignaturePolicy Skip -InstallDir $install -ProcessId 999999 2>&1
    $missingCliExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($missingCliExit -eq 0) { throw 'Updater-path install without CLI source unexpectedly succeeded' }
    if (($missingCliOutput | Out-String) -notmatch 'HeadroomRouteCLI') { throw 'Missing-CLI failure did not name the CLI executable' }
    if ((File-Hash $target) -ne $newHash) { throw 'Rejected install changed the installed executable' }
    if ((File-Hash $cliTarget) -ne $newCliHash) { throw 'Rejected install changed the installed CLI executable' }
    if ((File-Hash $shimTarget) -ne $newShimHash) { throw 'Rejected install changed the installed shim' }

    $rollbackBackupsAfterFailure = @(Get-ChildItem -LiteralPath (Join-Path $install 'rollback') -Directory | Sort-Object Name -Descending)
    if ($rollbackBackupsAfterFailure.Count -lt 2) { throw 'Expected a rollback backup from the failed upgrade attempt' }
    $failedUpgradeBackup = $rollbackBackupsAfterFailure[0].FullName
    $failedUpgradeManifest = Get-Content -LiteralPath (Join-Path $failedUpgradeBackup 'manifest.json') -Raw | ConvertFrom-Json
    $failedUpgradeMain = @($failedUpgradeManifest.files | Where-Object { $_.relative_path -eq 'HeadroomRoute.exe' })
    if ($failedUpgradeMain.Count -ne 1 -or $failedUpgradeMain[0].sha256 -ine $newHash) { throw 'Failed-upgrade rollback backup did not snapshot the pre-attempt state' }
    $configHashBeforeCorruptRollback = File-Hash (Join-Path $install 'config.json')
    Add-Content -LiteralPath (Join-Path $failedUpgradeBackup 'config.json') -Value 'tampered'
    $ErrorActionPreference = 'Continue'
    $corruptRollbackOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -Rollback -RollbackBackup $failedUpgradeBackup -SkipPathUpdate -InstallDir $install 2>&1
    $corruptRollbackExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($corruptRollbackExit -eq 0) { throw 'Corrupted rollback backup was unexpectedly accepted' }
    if (($corruptRollbackOutput | Out-String) -notmatch '校验') { throw 'Corrupted rollback rejection did not mention SHA-256 verification' }
    if ((File-Hash $target) -ne $newHash -or (File-Hash (Join-Path $install 'config.json')) -ne $configHashBeforeCorruptRollback) { throw 'Rejected corrupted rollback changed installed state' }

    $orphanBackup = Join-Path (Join-Path $install 'rollback') ('orphan-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $orphanBackup | Out-Null
    $ErrorActionPreference = 'Continue'
    $orphanOutput = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -Rollback -RollbackBackup $orphanBackup -SkipPathUpdate -InstallDir $install 2>&1
    $orphanExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($orphanExit -eq 0) { throw 'Rollback backup without manifest.json was unexpectedly accepted' }
    if (($orphanOutput | Out-String) -notmatch 'manifest') { throw 'Missing-manifest rejection did not name manifest.json' }
    if ((File-Hash $target) -ne $newHash) { throw 'Rejected missing-manifest rollback changed installed state' }

    Remove-Item (Join-Path $install 'old.started') -Force -ErrorAction SilentlyContinue
    $pathBeforeManualRollback = [Environment]::GetEnvironmentVariable('Path', 'User')
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $package 'Install.ps1') -Rollback -RollbackBackup $upgradeRollbackBackup -StartNow -SkipPathUpdate -InstallDir $install
    if ($LASTEXITCODE -ne 0) { throw 'Manual rollback failed' }
    Wait-ForFile (Join-Path $install 'old.started')
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $pathBeforeManualRollback) { throw 'Manual rollback unexpectedly modified the user PATH' }
    if ((File-Hash $target) -ne $oldHash) { throw 'Manual rollback did not restore the previous main executable' }
    if ((File-Hash $cliTarget) -ne $oldCliHash) { throw 'Manual rollback did not restore the previous CLI executable' }
    if ((File-Hash $shimTarget) -ne $oldShimHash) { throw 'Manual rollback did not restore the previous CLI shim' }
    if ((File-Hash (Join-Path $install 'config.json')) -ne $configHash) { throw 'Manual rollback did not restore config.json' }
    if ((File-Hash (Join-Path $install 'status.json')) -ne $statusHash) { throw 'Manual rollback did not restore status.json' }
    if (@(Get-ChildItem -LiteralPath (Join-Path $install 'rollback') -Directory).Count -lt 3) { throw 'Manual rollback did not preserve a rollback safety backup' }

    Write-Host 'Install upgrade and rollback test passed'
}
finally {
    Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$root*" } | Stop-Process -Force
    $tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    $resolvedRoot = [System.IO.Path]::GetFullPath($root)
    if ($resolvedRoot.StartsWith($tempRoot) -and (Split-Path -Leaf $resolvedRoot).StartsWith('headroom-route-install-test-')) {
        Remove-Item $resolvedRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ([Environment]::GetEnvironmentVariable('Path', 'User') -ne $originalUserPath) {
        throw 'Install tests changed the persistent user PATH'
    }
}
