$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$version = (Select-String -Path (Join-Path $project 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
if (!$version) { throw '无法从 Cargo.toml 读取版本号' }
$xwin = Join-Path $env:LOCALAPPDATA 'cargo-xwin\xwin'
$lld = Join-Path (rustc --print sysroot) 'lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'
if (Test-Path (Join-Path $xwin 'sdk\lib\um\x86_64\kernel32.lib')) {
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $lld
    $env:RUSTFLAGS = @('-Lnative=' + (Join-Path $xwin 'sdk\lib\um\x86_64'), '-Lnative=' + (Join-Path $xwin 'sdk\lib\ucrt\x86_64'), '-Lnative=' + (Join-Path $xwin 'crt\lib\x86_64')) -join ' '
}
Push-Location $project
try {
    cargo check
    if ($LASTEXITCODE -ne 0) { throw 'cargo check failed' }
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    New-Item -ItemType Directory -Path (Join-Path $project 'dist') -Force | Out-Null
    $releaseExe = Join-Path $project 'target\release\HeadroomRoute.exe'
    $versionedExe = Join-Path $project "dist\HeadroomRoute-$version.exe"
    Copy-Item $releaseExe $versionedExe -Force
    try {
        Copy-Item $releaseExe (Join-Path $project 'dist\HeadroomRoute.exe') -Force
    } catch [System.IO.IOException] {
        Write-Warning "dist\HeadroomRoute.exe is locked; use HeadroomRoute-$version.exe."
    }
    $zip = Join-Path $project "dist\HeadroomRoute-$version-windows-x64.zip"
    Compress-Archive -Path $versionedExe, (Join-Path $project 'Install.ps1'), (Join-Path $project 'README.md'), (Join-Path $project 'LICENSE') -DestinationPath $zip -Force
    $checksums = @($versionedExe, $zip | ForEach-Object {
        $artifactPath = $_
        $stream = [System.IO.File]::OpenRead($artifactPath)
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try { $hash = [System.BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
        finally { $sha256.Dispose(); $stream.Dispose() }
        "$hash  $(Split-Path -Leaf $artifactPath)"
    })
    Set-Content -Path (Join-Path $project "dist\HeadroomRoute-$version-SHA256SUMS.txt") -Value $checksums -Encoding ascii
}
finally { Pop-Location }
