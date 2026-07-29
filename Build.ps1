$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
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
    $versionedExe = Join-Path $project 'dist\HeadroomRoute-0.3.0.exe'
    Copy-Item $releaseExe $versionedExe -Force
    try {
        Copy-Item $releaseExe (Join-Path $project 'dist\HeadroomRoute.exe') -Force
    } catch [System.IO.IOException] {
        Write-Warning 'dist\HeadroomRoute.exe is locked; use HeadroomRoute-0.3.0.exe.'
    }
}
finally { Pop-Location }
