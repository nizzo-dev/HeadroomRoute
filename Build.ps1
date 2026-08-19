param(
    [string]$SigningCertificateThumbprint = $env:HEADROOM_ROUTE_SIGNING_CERT_THUMBPRINT,
    [ValidateSet('CurrentUser', 'LocalMachine')]
    [string]$CertificateStoreLocation = 'CurrentUser',
    [string]$TimestampServer = $env:HEADROOM_ROUTE_TIMESTAMP_SERVER,
    [switch]$RequireSignature
)
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$version = (Select-String -Path (Join-Path $project 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (!$version) { throw '无法从 Cargo.toml 读取版本号' }
function Get-Sha256([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try { [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $sha256.Dispose(); $stream.Dispose() }
}

function Copy-VersionedArtifact([string]$Source, [string]$Destination) {
    if ((Test-Path -LiteralPath $Destination) -and
        ((Get-Sha256 $Source) -ne (Get-Sha256 $Destination))) {
        throw "拒绝覆盖同版本的不同发布产物：$Destination。请先确认并更新 Cargo.toml 版本号。"
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Get-CodeSigningCertificate([string]$Thumbprint, [string]$StoreLocation) {
    $normalized = [string]($Thumbprint -replace '[^0-9A-Fa-f]', '')
    if (!$normalized) { return $null }
    $normalized = $normalized.ToUpperInvariant()
    $certificate = Get-Item -LiteralPath "Cert:\$StoreLocation\My\$normalized" -ErrorAction SilentlyContinue
    if (!$certificate) { throw "找不到签名证书：Cert:\$StoreLocation\My\$normalized" }
    if (!$certificate.HasPrivateKey) { throw "签名证书没有可用私钥：$normalized" }
    if ($certificate.NotBefore -gt (Get-Date) -or $certificate.NotAfter -le (Get-Date)) {
        throw "签名证书不在有效期内：$normalized"
    }
    $codeSigningOid = '1.3.6.1.5.5.7.3.3'
    if (-not @($certificate.EnhancedKeyUsageList | Where-Object { $_.ObjectId.Value -eq $codeSigningOid }).Count) {
        throw "证书不包含代码签名用途：$normalized"
    }
    return $certificate
}

function Set-AndConfirmSignature([string]$Path, $Certificate, [string]$TimestampUrl) {
    $parameters = @{
        FilePath = $Path
        Certificate = $Certificate
        HashAlgorithm = 'SHA256'
    }
    if (![string]::IsNullOrWhiteSpace($TimestampUrl)) { $parameters.TimestampServer = $TimestampUrl }
    $result = Set-AuthenticodeSignature @parameters
    if ($result.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "Authenticode 签名失败：$(Split-Path -Leaf $Path)（$($result.Status)：$($result.StatusMessage)）"
    }
    $verified = Get-AuthenticodeSignature -LiteralPath $Path
    if ($verified.Status -ne [System.Management.Automation.SignatureStatus]::Valid -or
        !$verified.SignerCertificate -or
        $verified.SignerCertificate.Thumbprint -ine $Certificate.Thumbprint) {
        throw "Authenticode 签名复核失败：$(Split-Path -Leaf $Path)"
    }
}
$signingCertificate = Get-CodeSigningCertificate $SigningCertificateThumbprint $CertificateStoreLocation
if (!$signingCertificate) {
    if ($RequireSignature) {
        throw "发布门禁要求 Authenticode 签名，但未找到可用的签名证书。请提供 -SigningCertificateThumbprint 或设置 HEADROOM_ROUTE_SIGNING_CERT_THUMBPRINT；证书需位于 $CertificateStoreLocation\My，包含私钥、Code Signing 用途（EKU 1.3.6.1.5.5.7.3.3）且在有效期内。"
    }
    Write-Warning '未配置 Authenticode 签名证书；本次开发构建将保持未签名。正式发布请传入 -RequireSignature 和签名证书指纹（或设置 HEADROOM_ROUTE_SIGNING_CERT_THUMBPRINT）。'
}
$xwin = Join-Path $env:LOCALAPPDATA 'cargo-xwin\xwin'
$lld = Join-Path (rustc --print sysroot) 'lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'
if (Test-Path (Join-Path $xwin 'sdk\lib\um\x86_64\kernel32.lib')) {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $lld
    $env:RUSTFLAGS = @('-Lnative=' + (Join-Path $xwin 'sdk\lib\um\x86_64'), '-Lnative=' + (Join-Path $xwin 'sdk\lib\ucrt\x86_64'), '-Lnative=' + (Join-Path $xwin 'crt\lib\x86_64')) -join ' '
}
function New-ReleaseZip([string]$MainExe, [string]$CliExe, [string]$DestinationZip) {
    $stage = Join-Path ([System.IO.Path]::GetTempPath()) ("headroom-route-zip-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        Copy-Item -LiteralPath $MainExe (Join-Path $stage "HeadroomRoute-$version.exe")
        Copy-Item -LiteralPath $CliExe (Join-Path $stage "HeadroomRouteCLI-$version.exe")
        Copy-Item -LiteralPath $cliShim (Join-Path $stage 'hr.cmd')
        foreach ($doc in @('Install.ps1', 'README.md', 'COMPATIBILITY.md', 'RELEASE.md', 'LICENSE')) {
            Copy-Item -LiteralPath (Join-Path $project $doc) (Join-Path $stage $doc)
        }
        if (Test-Path -LiteralPath $DestinationZip) {
            Remove-Item -LiteralPath $DestinationZip -Force
        }
        $ProgressPreference = 'SilentlyContinue'; Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $DestinationZip
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $packageArchive = [System.IO.Compression.ZipFile]::OpenRead($DestinationZip)
        try {
            $packageEntries = @($packageArchive.Entries | ForEach-Object { $_.FullName })
            foreach ($requiredFile in @("HeadroomRoute-$version.exe", "HeadroomRouteCLI-$version.exe", 'hr.cmd', 'Install.ps1')) {
                if ($packageEntries -notcontains $requiredFile) {
                    throw "发布包缺少必需文件：$requiredFile（$(Split-Path -Leaf $DestinationZip)）"
                }
            }
        }
        finally { $packageArchive.Dispose() }
    }
    finally {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Push-Location $project
try {
    cargo fmt -- --check
    if ($LASTEXITCODE -ne 0) { throw 'cargo fmt -- --check 未通过，请先运行 cargo fmt' }
    & (Join-Path $project 'Test-SourceLineLimit.ps1')
    cargo clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy --all-targets -- -D warnings 未通过' }
    cargo clippy --all-targets --features desktop -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy --features desktop 未通过' }
    cargo check
    if ($LASTEXITCODE -ne 0) { throw 'cargo check failed' }
    cargo check --features desktop
    if ($LASTEXITCODE -ne 0) { throw 'cargo check --features desktop failed' }
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
    cargo test --features desktop
    if ($LASTEXITCODE -ne 0) { throw 'cargo test --features desktop failed' }
    & (Join-Path $project 'Test-Install.ps1')
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    New-Item -ItemType Directory -Path (Join-Path $project 'dist') -Force | Out-Null
    $releaseExe = Join-Path $project 'target\release\HeadroomRoute.exe'
    $releaseCliExe = Join-Path $project 'target\release\HeadroomRouteCLI.exe'
    $versionedExe = Join-Path $project "dist\HeadroomRoute-$version.exe"
    $versionedCliExe = Join-Path $project "dist\HeadroomRouteCLI-$version.exe"
    $cliShim = Join-Path $project 'hr.cmd'
    if (!(Test-Path $cliShim)) { throw '找不到 CLI 快捷命令 shim：hr.cmd' }
    try {
        Copy-VersionedArtifact $releaseExe $versionedExe
    } catch [System.IO.IOException] {
        if (!(Test-Path $versionedExe) -or (Get-Sha256 $releaseExe) -ne (Get-Sha256 $versionedExe)) { throw }
        Write-Warning "dist\HeadroomRoute-$version.exe is locked but already matches this build."
    }
    Copy-VersionedArtifact $releaseCliExe $versionedCliExe

    cargo build --release --features desktop
    if ($LASTEXITCODE -ne 0) { throw 'cargo build --features desktop failed' }
    $versionedDesktopExe = Join-Path $project "dist\HeadroomRoute-$version-desktop.exe"
    try {
        Copy-VersionedArtifact $releaseExe $versionedDesktopExe
    } catch [System.IO.IOException] {
        if (!(Test-Path $versionedDesktopExe) -or (Get-Sha256 $releaseExe) -ne (Get-Sha256 $versionedDesktopExe)) { throw }
        Write-Warning "dist\HeadroomRoute-$version-desktop.exe is locked but already matches this build."
    }

    if ($signingCertificate) {
        if ([string]::IsNullOrWhiteSpace($TimestampServer)) {
            Write-Warning '正在生成无时间戳的 Authenticode 签名；证书过期后签名将无法继续验证。建议设置 HEADROOM_ROUTE_TIMESTAMP_SERVER。'
        }
        Set-AndConfirmSignature $versionedExe $signingCertificate $TimestampServer
        Set-AndConfirmSignature $versionedDesktopExe $signingCertificate $TimestampServer
        Set-AndConfirmSignature $versionedCliExe $signingCertificate $TimestampServer
        Write-Host "已使用证书 $($signingCertificate.Thumbprint) 签名并复核发布二进制。"
    }
    try {
        Copy-Item $versionedExe (Join-Path $project 'dist\HeadroomRoute.exe') -Force
    } catch [System.IO.IOException] {
        Write-Warning "dist\HeadroomRoute.exe is locked; use HeadroomRoute-$version.exe."
    }
    try {
        Copy-Item $versionedCliExe (Join-Path $project 'dist\HeadroomRouteCLI.exe') -Force
    } catch [System.IO.IOException] {
        Write-Warning "dist\HeadroomRouteCLI.exe is locked; use HeadroomRouteCLI-$version.exe."
    }
    $zip = Join-Path $project "dist\HeadroomRoute-$version-windows-x64.zip"
    $desktopZip = Join-Path $project "dist\HeadroomRoute-$version-desktop-windows-x64.zip"
    New-ReleaseZip $versionedExe $versionedCliExe $zip
    New-ReleaseZip $versionedDesktopExe $versionedCliExe $desktopZip
    $checksums = @($versionedExe, $versionedDesktopExe, $versionedCliExe, $zip, $desktopZip | ForEach-Object {
        $artifactPath = $_
        $hash = Get-Sha256 $artifactPath
        "$hash  $(Split-Path -Leaf $artifactPath)"
    })
    Set-Content -Path (Join-Path $project "dist\HeadroomRoute-$version-SHA256SUMS.txt") -Value $checksums -Encoding ascii
}
finally { Pop-Location }
