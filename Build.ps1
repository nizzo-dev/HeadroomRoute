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
Push-Location $project
try {
    cargo fmt -- --check
    if ($LASTEXITCODE -ne 0) { throw 'cargo fmt -- --check 未通过，请先运行 cargo fmt' }
    cargo clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy --all-targets -- -D warnings 未通过' }
    cargo check
    if ($LASTEXITCODE -ne 0) { throw 'cargo check failed' }
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
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
        Copy-Item $releaseExe $versionedExe -Force
    } catch [System.IO.IOException] {
        if (!(Test-Path $versionedExe) -or (Get-Sha256 $releaseExe) -ne (Get-Sha256 $versionedExe)) { throw }
        Write-Warning "dist\HeadroomRoute-$version.exe is locked but already matches this build."
    }
    Copy-Item $releaseCliExe $versionedCliExe -Force

    if ($signingCertificate) {
        if ([string]::IsNullOrWhiteSpace($TimestampServer)) {
            Write-Warning '正在生成无时间戳的 Authenticode 签名；证书过期后签名将无法继续验证。建议设置 HEADROOM_ROUTE_TIMESTAMP_SERVER。'
        }
        Set-AndConfirmSignature $versionedExe $signingCertificate $TimestampServer
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
    Compress-Archive -Path $versionedExe, $versionedCliExe, $cliShim, (Join-Path $project 'Install.ps1'), (Join-Path $project 'README.md'), (Join-Path $project 'COMPATIBILITY.md'), (Join-Path $project 'RELEASE.md'), (Join-Path $project 'LICENSE') -DestinationPath $zip -Force
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $packageArchive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $packageEntries = @($packageArchive.Entries | ForEach-Object { $_.FullName })
        foreach ($requiredFile in @("HeadroomRouteCLI-$version.exe", 'hr.cmd')) {
            if ($packageEntries -notcontains $requiredFile) {
                throw "发布包缺少必需文件：$requiredFile"
            }
        }
    }
    finally { $packageArchive.Dispose() }
    $checksums = @($versionedExe, $versionedCliExe, $zip | ForEach-Object {
        $artifactPath = $_
        $hash = Get-Sha256 $artifactPath
        "$hash  $(Split-Path -Leaf $artifactPath)"
    })
    Set-Content -Path (Join-Path $project "dist\HeadroomRoute-$version-SHA256SUMS.txt") -Value $checksums -Encoding ascii
}
finally { Pop-Location }
