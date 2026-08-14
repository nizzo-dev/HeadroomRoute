[CmdletBinding()]
param(
    [int]$MaxLines = 600,
    [int]$WarnLines = 500
)
$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $MyInvocation.MyCommand.Path
$ceilingFile = Join-Path $project 'SourceLineCeilings.txt'

function Get-PhysicalLineCount([string]$Path) {
    return @(Get-Content -LiteralPath $Path).Count
}

function Get-RelativePath([string]$Path) {
    $relative = $Path
    if ($Path.StartsWith($project, [System.StringComparison]::OrdinalIgnoreCase)) {
        $relative = $Path.Substring($project.Length).TrimStart('\', '/')
    }
    return ($relative -replace '\\', '/')
}

function Get-LineCeilings([string]$Path) {
    $ceilings = @{}
    if (-not (Test-Path -LiteralPath $Path)) {
        return $ceilings
    }
    Get-Content -LiteralPath $Path | ForEach-Object {
        $line = $_.Trim()
        if ($line.Length -eq 0 -or $line.StartsWith('#')) {
            return
        }
        $parts = $line -split '\s+', 2
        if ($parts.Count -ne 2) {
            throw "invalid ceiling row: $_"
        }
        $ceilings[$parts[1].Replace('\', '/')] = [int]$parts[0]
    }
    return $ceilings
}

$files = New-Object System.Collections.Generic.List[System.IO.FileInfo]
Get-ChildItem -LiteralPath (Join-Path $project 'src') -Recurse -File -Filter '*.rs' | ForEach-Object { $files.Add($_) }
$uiRoot = Join-Path $project 'ui'
if (Test-Path -LiteralPath $uiRoot) {
    Get-ChildItem -LiteralPath $uiRoot -Recurse -File | Where-Object {
        $_.Extension -match '^\.(html|css|js)$'
    } | ForEach-Object { $files.Add($_) }
}

$ceilings = Get-LineCeilings $ceilingFile
$over = New-Object System.Collections.Generic.List[object]
$grown = New-Object System.Collections.Generic.List[object]
$warn = New-Object System.Collections.Generic.List[object]
$stale = New-Object System.Collections.Generic.List[object]

foreach ($file in $files) {
    $lines = Get-PhysicalLineCount $file.FullName
    $relative = Get-RelativePath $file.FullName
    $row = [pscustomobject]@{ Lines = $lines; Path = $relative; Ceiling = $null }
    if ($ceilings.ContainsKey($relative)) {
        $row.Ceiling = $ceilings[$relative]
        if ($lines -gt $row.Ceiling) {
            $grown.Add($row)
        } elseif ($lines -lt $row.Ceiling) {
            $stale.Add($row)
        }
        continue
    }
    if ($lines -gt $MaxLines) {
        $over.Add($row)
    } elseif ($lines -ge $WarnLines) {
        $warn.Add($row)
    }
}

if ($stale.Count -gt 0) {
    Write-Warning "These frozen files shrank; lower the ceiling in SourceLineCeilings.txt."
    $stale | Sort-Object Lines -Descending | ForEach-Object {
        Write-Warning ('  {0,4}  {1}  (ceiling {2})' -f $_.Lines, $_.Path, $_.Ceiling)
    }
}

if ($warn.Count -gt 0) {
    Write-Warning "These unfrozen files are at or above $WarnLines lines (fail above $MaxLines)."
    $warn | Sort-Object Lines -Descending | ForEach-Object {
        Write-Warning ('  {0,4}  {1}' -f $_.Lines, $_.Path)
    }
}

$failed = $false
if ($grown.Count -gt 0) {
    Write-Host "These frozen files grew past SourceLineCeilings.txt:"
    $grown | Sort-Object Lines -Descending | ForEach-Object {
        Write-Host ('  {0,4}  {1}  (frozen at {2})' -f $_.Lines, $_.Path, $_.Ceiling)
    }
    $failed = $true
}

if ($over.Count -gt 0) {
    Write-Host "These new or unfrozen files exceed the $MaxLines-line ceiling:"
    $over | Sort-Object Lines -Descending | ForEach-Object {
        Write-Host ('  {0,4}  {1}' -f $_.Lines, $_.Path)
    }
    $failed = $true
}

if ($failed) {
    throw 'source line limit failed: frozen files must not grow; new files must stay around 500 (fail above 600)'
}

Write-Host ("Source line limit ok ({0} file(s) checked, {1} frozen)." -f $files.Count, $ceilings.Count)
