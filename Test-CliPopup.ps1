[CmdletBinding()]
param(
    [string]$Executable,
    [switch]$IncludeApproval
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($Executable)) {
    $Executable = Join-Path $PSScriptRoot 'dist\HeadroomRoute.exe'
}

$resolvedExecutable = (Resolve-Path -LiteralPath $Executable -ErrorAction Stop).Path
$arguments = [System.Collections.Generic.List[string]]::new()
$arguments.Add('--notification-demo')

if ($IncludeApproval) {
    $arguments.Add('--approval-demo')
}

Write-Host "Popup test: $resolvedExecutable"
Write-Host 'The AI completion and AI error popups will appear in sequence. Press Enter to stop.'

$process = Start-Process -FilePath $resolvedExecutable -ArgumentList $arguments.ToArray() -PassThru
try {
    [Console]::ReadLine() | Out-Null
}
finally {
    if (-not $process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }

    $process.Dispose()
}
