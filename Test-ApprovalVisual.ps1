param(
    [string]$Executable = (Join-Path $PSScriptRoot 'target\debug\HeadroomRoute.exe')
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class ApprovalVisualNative {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr hwnd, System.Text.StringBuilder className, int maxCount);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder text, int maxCount);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public const uint GW_CHILD = 5;
    public const uint GW_HWNDNEXT = 2;
}
"@

$exe = (Resolve-Path -LiteralPath $Executable).Path
$process = Start-Process -FilePath $exe -ArgumentList '--approval-demo' -PassThru
$window = [IntPtr]::Zero
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) { throw "approval demo exited early with code $($process.ExitCode)" }
        $process.Refresh()
        $window = $process.MainWindowHandle
        if ($window -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 50
    }
    if ($window -eq [IntPtr]::Zero) { throw 'approval popup did not appear' }
    $className = New-Object System.Text.StringBuilder 128
    if ([ApprovalVisualNative]::GetClassName($window, $className, $className.Capacity) -eq 0 -or
        $className.ToString() -ne 'HeadroomRouteApprovalWindow') {
        $texts = @()
        $child = [ApprovalVisualNative]::GetWindow($window, [ApprovalVisualNative]::GW_CHILD)
        while ($child -ne [IntPtr]::Zero) {
            $text = New-Object System.Text.StringBuilder 512
            [ApprovalVisualNative]::GetWindowText($child, $text, $text.Capacity) | Out-Null
            if ($text.Length) { $texts += $text.ToString() }
            $child = [ApprovalVisualNative]::GetWindow($child, [ApprovalVisualNative]::GW_HWNDNEXT)
        }
        throw "unexpected approval popup class: $className; text=$($texts -join ' | ')"
    }
    Start-Sleep -Milliseconds 450
    $rect = New-Object ApprovalVisualNative+RECT
    if (![ApprovalVisualNative]::GetWindowRect($window, [ref]$rect)) { throw 'could not read popup bounds' }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -lt 500 -or $height -lt 260) { throw "popup did not expand: ${width}x${height}" }
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
    $imagePath = Join-Path $PSScriptRoot 'target\approval-demo.png'
    $bitmap.Save($imagePath, [System.Drawing.Imaging.ImageFormat]::Png)
    $center = $bitmap.GetPixel([int]($width / 2), [int]($height / 2))
    $corner = $bitmap.GetPixel(2, 2)
    $graphics.Dispose()
    $bitmap.Dispose()
    if ($center.ToArgb() -eq $corner.ToArgb()) { throw 'popup screenshot appears blank' }

    $dpi = [ApprovalVisualNative]::GetDpiForWindow($window)
    if ($dpi -eq 0) { $dpi = 96 }
    $scale = { param($value) [int]($value * $dpi / 96) }
    $x = $width - (& $scale 65)
    $y = $height - (& $scale 37)
    $lParam = [IntPtr]((($y -band 0xffff) -shl 16) -bor ($x -band 0xffff))
    if (![ApprovalVisualNative]::PostMessage($window, 0x0202, [IntPtr]::Zero, $lParam)) { throw 'could not click allow button' }
    $deadline = [DateTime]::UtcNow.AddSeconds(3)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (![ApprovalVisualNative]::IsWindow($window)) { break }
        Start-Sleep -Milliseconds 50
    }
    if ([ApprovalVisualNative]::IsWindow($window)) { throw 'popup did not close after allow click' }
    Write-Output "approval visual test passed: ${width}x${height}, screenshot=$imagePath"
}
finally {
    if ($process -and !$process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    Get-Process -Name 'HeadroomRoute' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force -ErrorAction SilentlyContinue
}
