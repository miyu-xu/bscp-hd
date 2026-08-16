[CmdletBinding()]
param(
    [int]$ProcessId = 0,
    [long]$RootHwnd = 0,
    [string]$Output,
    [switch]$RequireAndroid,
    [switch]$VerifyTitlebarFocus
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
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr GetProp(IntPtr hwnd, string name);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
    [DllImport("user32.dll")] public static extern bool GetGUIThreadInfo(uint threadId, ref GuiThreadInfo info);
    [StructLayout(LayoutKind.Sequential)]
    public struct GuiThreadInfo {
        public uint Size;
        public uint Flags;
        public IntPtr Active;
        public IntPtr Focus;
        public IntPtr Capture;
        public IntPtr MenuOwner;
        public IntPtr MoveSize;
        public IntPtr Caret;
        public Rect CaretRect;
    }
    public static string ClassName(IntPtr hwnd) { var value = new StringBuilder(256); GetClassName(hwnd, value, value.Capacity); return value.ToString(); }
    public static string WindowText(IntPtr hwnd) { var value = new StringBuilder(512); GetWindowText(hwnd, value, value.Capacity); return value.ToString(); }
    public static IntPtr FocusWindow() { var info = new GuiThreadInfo { Size = (uint)Marshal.SizeOf<GuiThreadInfo>() }; return GetGUIThreadInfo(0, ref info) ? info.Focus : IntPtr.Zero; }
    public static bool Click(int x, int y) { if (!SetCursorPos(x, y)) return false; mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero); return true; }
}
'@

$root = if ($RootHwnd -ne 0) { [IntPtr]::new($RootHwnd) } else { [IntPtr]::Zero }
if ($root -eq [IntPtr]::Zero) {
    [HdWindowSmoke]::EnumWindows({
        param([IntPtr]$hwnd, [IntPtr]$state)
        [uint32]$owner = 0
        [HdWindowSmoke]::GetWindowThreadProcessId($hwnd, [ref]$owner) | Out-Null
        $title = [HdWindowSmoke]::WindowText($hwnd)
        if (($ProcessId -gt 0 -and $owner -eq $ProcessId -and $title.StartsWith("HD")) -or
            ($ProcessId -eq 0 -and $title.StartsWith("HD"))) {
            $script:root = $hwnd
            return $false
        }
        return $true
    }, [IntPtr]::Zero) | Out-Null
}
if ($root -eq [IntPtr]::Zero) {
    throw "HD top-level window was not found"
}

$windows = [Collections.Generic.List[object]]::new()
[HdWindowSmoke]::EnumChildWindows($root, {
    param([IntPtr]$hwnd, [IntPtr]$state)
    $rect = New-Object HdWindowSmoke+Rect
    [HdWindowSmoke]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
    $class = [HdWindowSmoke]::ClassName($hwnd)
    $latestViewport = $null
    $appliedViewport = $null
    if ($class -match '^CROSVM_\d+$') {
        $latestViewport = [HdWindowSmoke]::GetProp($hwnd, "HD_LATEST_VIEWPORT_V1").ToInt64()
        $appliedViewport = [HdWindowSmoke]::GetProp($hwnd, "HD_APPLIED_VIEWPORT_V1").ToInt64()
    }
    $windows.Add([pscustomobject][ordered]@{
        hwnd = "0x{0:X}" -f $hwnd.ToInt64()
        class = $class
        visible = [HdWindowSmoke]::IsWindowVisible($hwnd)
        x = $rect.Left
        y = $rect.Top
        width = $rect.Right - $rect.Left
        height = $rect.Bottom - $rect.Top
        right = $rect.Right
        bottom = $rect.Bottom
        viewport_latest = $latestViewport
        viewport_applied = $appliedViewport
    }) | Out-Null
    return $true
}, [IntPtr]::Zero) | Out-Null

$webviews = @($windows | Where-Object class -eq "WRY_WEBVIEW")
$native = @($windows | Where-Object class -eq "HD_NATIVE_DISPLAY_HOST_V2")
$crosvm = @($windows | Where-Object class -match '^CROSVM_\d+$')
$visibleNative = @($native | Where-Object visible)
$visibleCrosvm = @($crosvm | Where-Object visible)
$visibleSubWin = @($windows | Where-Object { $_.class -eq "subWin" -and $_.visible })
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
if ($RequireAndroid -and $visibleCrosvm.Count -ne 1) { $failures.Add("expected exactly one visible crosvm input child") }
if ($RequireAndroid -and $visibleSubWin.Count -ne 1) { $failures.Add("expected exactly one visible gfxstream render child") }
foreach ($display in $visibleCrosvm) {
    $expectedViewport = ([int64]$display.height -shl 16) -bor [int64]$display.width
    if ($display.viewport_latest -ne $display.viewport_applied) {
        $failures.Add("crosvm latest viewport is not acknowledged as applied")
    }
    if ($display.viewport_applied -ne $expectedViewport) {
        $failures.Add("crosvm applied viewport does not match its visible bounds")
    }
    $matchingRender = @($visibleSubWin | Where-Object {
        $_.x -eq $display.x -and $_.y -eq $display.y -and
        $_.width -eq $display.width -and $_.height -eq $display.height
    })
    if ($matchingRender.Count -ne 1) {
        $failures.Add("gfxstream render child does not match the crosvm input viewport")
    }
}

$titlebarFocus = $null
if ($VerifyTitlebarFocus) {
    $visibleWebviews = @($webviews | Where-Object visible)
    if ($visibleWebviews.Count -ne 1 -or $visibleNative.Count -ne 1) {
        $failures.Add("titlebar focus verification requires one visible WebView and NativeDisplayHost")
    } else {
        [HdWindowSmoke]::SetForegroundWindow($root) | Out-Null
        $display = $visibleNative[0]
        $displayClickPosted = [HdWindowSmoke]::Click(
            [int](($display.x + $display.right) / 2),
            [int](($display.y + $display.bottom) / 2))
        Start-Sleep -Milliseconds 250
        $focusBefore = [HdWindowSmoke]::FocusWindow()
        if ($focusBefore -eq [IntPtr]::Zero) {
            $titlebarFocus = [pscustomobject][ordered]@{
                physical_input_pending = $true
                display_click_posted = $displayClickPosted
                focus_before = "0x0"
            }
        } else {
            if (-not $displayClickPosted) {
                $failures.Add("could not focus the native Player surface before titlebar click")
            }
            $titlebar = $visibleWebviews[0]
            if (-not [HdWindowSmoke]::Click($titlebar.x + 18, $titlebar.y + 15)) {
                $failures.Add("could not click the WebView titlebar sidebar control")
            }
            Start-Sleep -Milliseconds 500
            $focusAfter = [HdWindowSmoke]::FocusWindow()
            $titlebarHwnd = [IntPtr]::new([Convert]::ToInt64($titlebar.hwnd.Substring(2), 16))
            if ($focusAfter -eq $titlebarHwnd) {
                $failures.Add("WebView titlebar pointerdown stole focus from the native Player surface")
            }
            if ($focusAfter -ne $focusBefore) {
                $failures.Add("WebView titlebar click changed the native Player focus target")
            }
            $titlebarFocus = [pscustomobject][ordered]@{
                physical_input_pending = $false
                physical_pointer_click = $true
                focus_before = "0x{0:X}" -f $focusBefore.ToInt64()
                focus_after = "0x{0:X}" -f $focusAfter.ToInt64()
                webview_focus_rejected = $focusAfter -ne $titlebarHwnd
            }
        }
    }
}

$result = [pscustomobject][ordered]@{
    schema_version = 2
    root_hwnd = "0x{0:X}" -f $root.ToInt64()
    passed = $failures.Count -eq 0
    failures = $failures
    visible_webview_native_overlap_count = $overlaps.Count
    titlebar_focus = $titlebarFocus
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
