[CmdletBinding()]
param(
    [int]$ProcessId = 0,
    [string]$Output,
    [switch]$RequireAndroid
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class HdWindowSmoke {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    public delegate bool EnumProc(IntPtr hwnd, IntPtr state);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc callback, IntPtr state);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, EnumProc callback, IntPtr state);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr hwnd, StringBuilder value, int capacity);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, StringBuilder value, int capacity);
    public static string ClassName(IntPtr hwnd) { var value = new StringBuilder(256); GetClassName(hwnd, value, value.Capacity); return value.ToString(); }
    public static string WindowText(IntPtr hwnd) { var value = new StringBuilder(512); GetWindowText(hwnd, value, value.Capacity); return value.ToString(); }
}
'@

$root = [IntPtr]::Zero
[HdWindowSmoke]::EnumWindows({
    param([IntPtr]$hwnd, [IntPtr]$state)
    [uint32]$owner = 0
    [HdWindowSmoke]::GetWindowThreadProcessId($hwnd, [ref]$owner) | Out-Null
    $title = [HdWindowSmoke]::WindowText($hwnd)
    if (($ProcessId -gt 0 -and $owner -eq $ProcessId) -or ($ProcessId -eq 0 -and $title.StartsWith("HD"))) {
        $script:root = $hwnd
        return $false
    }
    return $true
}, [IntPtr]::Zero) | Out-Null
if ($root -eq [IntPtr]::Zero) {
    throw "HD top-level window was not found"
}

$windows = [Collections.Generic.List[object]]::new()
[HdWindowSmoke]::EnumChildWindows($root, {
    param([IntPtr]$hwnd, [IntPtr]$state)
    $rect = New-Object HdWindowSmoke+Rect
    [HdWindowSmoke]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
    $windows.Add([pscustomobject][ordered]@{
        hwnd = "0x{0:X}" -f $hwnd.ToInt64()
        class = [HdWindowSmoke]::ClassName($hwnd)
        visible = [HdWindowSmoke]::IsWindowVisible($hwnd)
        x = $rect.Left
        y = $rect.Top
        width = $rect.Right - $rect.Left
        height = $rect.Bottom - $rect.Top
        right = $rect.Right
        bottom = $rect.Bottom
    }) | Out-Null
    return $true
}, [IntPtr]::Zero) | Out-Null

$webviews = @($windows | Where-Object class -eq "WRY_WEBVIEW")
$native = @($windows | Where-Object class -eq "HD_NATIVE_DISPLAY_HOST_V2")
$crosvm = @($windows | Where-Object class -eq "CROSVM_1")
$visibleNative = @($native | Where-Object visible)
$overlaps = [Collections.Generic.List[object]]::new()
foreach ($webview in @($webviews | Where-Object visible)) {
    foreach ($display in $visibleNative) {
        $width = [Math]::Min($webview.right, $display.right) - [Math]::Max($webview.x, $display.x)
        $height = [Math]::Min($webview.bottom, $display.bottom) - [Math]::Max($webview.y, $display.y)
        if ($width -gt 0 -and $height -gt 0) {
            $overlaps.Add([pscustomobject]@{ webview = $webview.hwnd; native = $display.hwnd; width = $width; height = $height })
        }
    }
}
$badParking = @($webviews | Where-Object { -not $_.visible -and ($_.width -gt 1 -or $_.height -gt 1) })
$failures = [Collections.Generic.List[string]]::new()
if ($webviews.Count -ne 3) { $failures.Add("expected exactly three WRY_WEBVIEW surfaces") }
if ($native.Count -ne 1) { $failures.Add("expected exactly one NativeDisplayHost surface") }
if ($overlaps.Count -ne 0) { $failures.Add("a visible WebView overlaps the visible Android surface") }
if ($badParking.Count -ne 0) { $failures.Add("a hidden WebView is not parked at 1x1") }
if ($RequireAndroid -and $visibleNative.Count -ne 1) { $failures.Add("Android NativeDisplayHost is not visible") }
if ($RequireAndroid -and $crosvm.Count -lt 1) { $failures.Add("crosvm render child is not attached") }

$result = [pscustomobject][ordered]@{
    schema_version = 1
    root_hwnd = "0x{0:X}" -f $root.ToInt64()
    passed = $failures.Count -eq 0
    failures = $failures
    visible_webview_native_overlap_count = $overlaps.Count
    windows = $windows | Where-Object { $_.class -match "WRY|HD_NATIVE|CROSVM|subWin" }
}
$json = $result | ConvertTo-Json -Depth 7
if ($Output) {
    $parent = Split-Path -Parent $Output
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    Set-Content -LiteralPath $Output -Value $json -Encoding utf8NoBOM
}
$json
if (-not $result.passed) { exit 1 }
