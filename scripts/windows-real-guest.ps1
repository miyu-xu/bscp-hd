[CmdletBinding()]
param(
    [Guid]$InstanceId = [Guid]::Empty,

    [string]$ArtifactRoot = "D:\bscp\bscp-vm-artifacts-20260721-aosp-all-targets\products\android\vsoc_x86_64\direct-linux",

    [string]$DistRoot = "",

    [string]$DataRoot = "",

    [string]$Hdctl = "",

    [string]$HdUi = "",

    [string]$Adb = "",

    [string]$Aapt2 = "",

    [ValidateRange(0, 1000)]
    [int]$LifecycleCycles = 0,

    [ValidateRange(0, 1440)]
    [int]$StabilityMinutes = 0,

    [ValidateRange(0, 4096)]
    [int]$MaxWorkerHandleGrowth = 64,

    [ValidateRange(0, 4096)]
    [int]$MaxRuntimeHandleGrowth = 128,

    [ValidateRange(0, 16384)]
    [int]$MaxWorkerPrivateGrowthMiB = 64,

    [ValidateRange(0, 16384)]
    [int]$MaxRuntimePrivateGrowthMiB = 1024,

    [ValidateRange(1, 4096)]
    [int]$MaxStabilityLogMiB = 256,

    [bool]$DevFastArtifacts = $true,

    [switch]$RunActions,

    [switch]$RunAdbLossPowerFallback,

    [switch]$RunBugreport,

    [switch]$RunUwbFira,

    [switch]$RunLocationProbe,

    [switch]$RunLocationRoute,

    [switch]$RunScreenRecording,

    [string]$LocationProbeApk = "",

    [switch]$RunBluetoothHogp,

    [string]$Apk = "",

    [ValidateRange(0, 3)]
    [int]$SecondaryDisplayCount = 0,

    [ValidateRange(1, 32)]
    [int]$GuestCpuCount = 4,

    [ValidateRange(1024, 32768)]
    [int]$GuestMemoryMiB = 4096,

    [string]$GuestHwuiRenderer = "",

    [ValidateRange(0, 120)]
    [int]$GuestSettleSeconds = 0,

    [switch]$RunUiDisplayInput,

    [string]$SecondarySpec = "",

    [string]$Output = "out\windows-real-guest"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $repoRoot
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $repoRoot $Output }
$isolated = $InstanceId -eq [Guid]::Empty
if ($isolated) {
    $InstanceId = [Guid]::NewGuid()
}
if ([string]::IsNullOrWhiteSpace($DistRoot)) {
    $DistRoot = Join-Path $workspaceRoot "out\dist"
}
$hostToolsRoot = Join-Path $DistRoot "windows\bin"
if ([string]::IsNullOrWhiteSpace($Hdctl)) {
    $Hdctl = Join-Path $hostToolsRoot "hdctl.exe"
}
if ([string]::IsNullOrWhiteSpace($HdUi)) {
    $HdUi = Join-Path $hostToolsRoot "hd.exe"
}
if ([string]::IsNullOrWhiteSpace($Adb)) {
    $packagedAdb = Join-Path $hostToolsRoot "adb.exe"
    $Adb = if (Test-Path -LiteralPath $packagedAdb -PathType Leaf) {
        $packagedAdb
    } else {
        Join-Path $env:LOCALAPPDATA "Android\Sdk\platform-tools\adb.exe"
    }
}
if ([string]::IsNullOrWhiteSpace($DataRoot)) {
    $DataRoot = if ($isolated) { Join-Path $outputRoot "data" } else { "D:\hd-v2-data" }
}
if ([string]::IsNullOrWhiteSpace($Aapt2)) {
    $packagedAapt2 = Join-Path $hostToolsRoot "aapt2.exe"
    if (Test-Path -LiteralPath $packagedAapt2 -PathType Leaf) {
        $Aapt2 = $packagedAapt2
    } else {
        $buildToolsRoot = Join-Path $env:LOCALAPPDATA "Android\Sdk\build-tools"
        $Aapt2 = Get-ChildItem -LiteralPath $buildToolsRoot -Recurse -File -Filter "aapt2.exe" -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending |
            Select-Object -First 1 -ExpandProperty FullName
    }
}
$packagedTooling = -not [string]::IsNullOrWhiteSpace($Aapt2) -and
    [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Adb)).Equals(
        [IO.Path]::GetFullPath($hostToolsRoot),
        [StringComparison]::OrdinalIgnoreCase) -and
    [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Aapt2)).Equals(
        [IO.Path]::GetFullPath($hostToolsRoot),
        [StringComparison]::OrdinalIgnoreCase)
if ($isolated -and -not $packagedTooling) {
    throw "isolated Windows product regression requires packaged adb.exe and aapt2.exe"
}
if ($DevFastArtifacts) {
    $rootfs = Join-Path $ArtifactRoot "aggregate_android.sparse.img"
    if (-not (Test-Path -LiteralPath $rootfs -PathType Leaf)) {
        $rootfs = Join-Path $ArtifactRoot "aggregate_android.img"
    }
    $env:HD_DEV_FAST_ARTIFACTS = "1"
    $env:HD_DEV_GUEST_BUNDLE_ROOT = $ArtifactRoot
    $env:HD_DEV_GUEST_ROOTFS = $rootfs
    $env:HD_DEV_HOST_TOOLS_ROOT = $hostToolsRoot
    $env:HD_DEV_ADB = $Adb
    $env:HD_DEV_AAPT2 = $Aapt2
} else {
    Remove-Item Env:HD_DEV_FAST_ARTIFACTS -ErrorAction SilentlyContinue
}
$evidencePath = Join-Path $outputRoot "windows-real-guest.json"
$specPath = Join-Path $outputRoot "android-spec.json"
$startedAt = [DateTimeOffset]::UtcNow
$cycles = [Collections.Generic.List[object]]::new()
$samples = [Collections.Generic.List[object]]::new()
$actions = [Collections.Generic.List[string]]::new()
$actionReadbacks = [Collections.Generic.List[object]]::new()
$skippedControls = [Collections.Generic.List[object]]::new()
$secondaryId = $null
$created = $false
$hostProcess = $null
$hostStdout = Join-Path $outputRoot "hd-host.stdout.txt"
$hostStderr = Join-Path $outputRoot "hd-host.stderr.txt"
$directSelection = $null
$bugreportEvidence = $null
$uwbFiraEvidence = $null
$locationProbeEvidence = $null
$locationRouteEvidence = $null
$screenRecordingEvidence = $null
$backendWindowEvidence = $null
$multiDisplayEvidence = $null
$uiDisplayInputEvidence = $null
$displaySelectionEvidence = $null
$adbLossPowerEvidence = $null
$targetedReadinessEvidence = $null
$readinessTimeline = [Collections.Generic.List[object]]::new()
$uiProcess = $null
$uiRuntimeRoot = $null

if (-not ("HdRealGuestWindowNative" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class HdRealGuestWindowNative {
    public delegate bool EnumWindowsProc(IntPtr window, IntPtr parameter);

    [StructLayout(LayoutKind.Sequential)]
    public struct Point {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct Rect {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct GuiThreadInfo {
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

    [StructLayout(LayoutKind.Sequential)]
    private struct BitmapInfoHeader {
        public uint Size;
        public int Width;
        public int Height;
        public ushort Planes;
        public ushort BitCount;
        public uint Compression;
        public uint SizeImage;
        public int XPelsPerMeter;
        public int YPelsPerMeter;
        public uint ClrUsed;
        public uint ClrImportant;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct BitmapInfo {
        public BitmapInfoHeader Header;
        public uint Colors;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MouseInput {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }

    [StructLayout(LayoutKind.Explicit)]
    private struct InputUnion {
        [FieldOffset(0)] public MouseInput Mouse;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Input {
        public uint Type;
        public InputUnion Union;
    }

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr parameter);

    [DllImport("user32.dll")]
    public static extern bool EnumChildWindows(
        IntPtr parent,
        EnumWindowsProc callback,
        IntPtr parameter);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr window);

    [DllImport("user32.dll")]
    public static extern uint GetDpiForWindow(IntPtr window);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("user32.dll")]
    private static extern bool GetGUIThreadInfo(uint threadId, ref GuiThreadInfo info);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr window, StringBuilder text, int capacity);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetClassNameW(IntPtr window, StringBuilder text, int capacity);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindWindowExW(
        IntPtr parent,
        IntPtr childAfter,
        string className,
        string windowName);

    [DllImport("user32.dll")]
    public static extern IntPtr GetParent(IntPtr window);

    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr window, out Rect rect);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr window, out Rect rect);

    [DllImport("user32.dll")]
    private static extern IntPtr GetDC(IntPtr window);

    [DllImport("user32.dll")]
    private static extern int ReleaseDC(IntPtr window, IntPtr dc);

    [DllImport("gdi32.dll")]
    private static extern IntPtr CreateCompatibleDC(IntPtr dc);

    [DllImport("gdi32.dll")]
    private static extern bool DeleteDC(IntPtr dc);

    [DllImport("gdi32.dll")]
    private static extern IntPtr CreateDIBSection(
        IntPtr dc,
        ref BitmapInfo info,
        uint usage,
        out IntPtr bits,
        IntPtr section,
        uint offset);

    [DllImport("gdi32.dll")]
    private static extern IntPtr SelectObject(IntPtr dc, IntPtr value);

    [DllImport("gdi32.dll")]
    private static extern int SetStretchBltMode(IntPtr dc, int mode);

    [DllImport("gdi32.dll")]
    private static extern bool StretchBlt(
        IntPtr destination,
        int destinationX,
        int destinationY,
        int destinationWidth,
        int destinationHeight,
        IntPtr source,
        int sourceX,
        int sourceY,
        int sourceWidth,
        int sourceHeight,
        uint operation);

    [DllImport("gdi32.dll")]
    private static extern IntPtr CreateRectRgn(int left, int top, int right, int bottom);

    [DllImport("gdi32.dll")]
    private static extern bool DeleteObject(IntPtr value);

    [DllImport("user32.dll")]
    private static extern int GetWindowRgn(IntPtr window, IntPtr region);

    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr window, ref Point point);

    [DllImport("user32.dll")]
    public static extern bool GetCursorPos(out Point point);

    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    public static extern IntPtr WindowFromPoint(Point point);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);

    [DllImport("user32.dll")]
    public static extern bool BringWindowToTop(IntPtr window);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr window, int command);

    [DllImport("user32.dll")]
    public static extern bool IsZoomed(IntPtr window);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool SetWindowPos(
        IntPtr window,
        IntPtr insertAfter,
        int x,
        int y,
        int width,
        int height,
        uint flags);

    [DllImport("user32.dll")]
    public static extern IntPtr GetAncestor(IntPtr window, uint flags);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint SendInput(uint count, Input[] inputs, int size);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern bool PostMessageW(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr SendMessageW(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr GetPropW(IntPtr window, string name);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetPropW(IntPtr window, string name, IntPtr value);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr RemovePropW(IntPtr window, string name);

    public static string WindowText(IntPtr window) {
        var text = new StringBuilder(512);
        GetWindowTextW(window, text, text.Capacity);
        return text.ToString();
    }

    public static string WindowClass(IntPtr window) {
        var text = new StringBuilder(512);
        GetClassNameW(window, text, text.Capacity);
        return text.ToString();
    }

    public static IntPtr FocusWindow() {
        var info = new GuiThreadInfo { Size = (uint)Marshal.SizeOf<GuiThreadInfo>() };
        return GetGUIThreadInfo(0, ref info) ? info.Focus : IntPtr.Zero;
    }

    public static ulong WindowProperty(IntPtr window, string name) {
        return unchecked((ulong)GetPropW(window, name).ToInt64());
    }

    public static bool IsWindowRegionEmpty(IntPtr window) {
        var probe = CreateRectRgn(0, 0, 0, 0);
        if (probe == IntPtr.Zero) {
            return false;
        }
        var regionType = GetWindowRgn(window, probe);
        DeleteObject(probe);
        return regionType == 1;
    }

    public static bool SetWindowProperty(IntPtr window, string name, ulong value) {
        if (window == IntPtr.Zero || value == 0 || value > long.MaxValue) {
            return false;
        }
        return SetPropW(window, name, new IntPtr(unchecked((long)value)));
    }

    public static void RemoveWindowProperty(IntPtr window, string name) {
        if (window != IntPtr.Zero) {
            RemovePropW(window, name);
        }
    }

    public static IntPtr FindVisibleTopLevelWindow(uint processId, string title) {
        var result = IntPtr.Zero;
        EnumWindows((window, parameter) => {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            if (owner == processId && IsWindowVisible(window) && WindowText(window) == title) {
                result = window;
                return false;
            }
            return true;
        }, IntPtr.Zero);
        return result;
    }

    public static IntPtr[] Descendants(IntPtr parent) {
        var result = new System.Collections.Generic.List<IntPtr>();
        EnumChildWindows(parent, (window, parameter) => {
            result.Add(window);
            return true;
        }, IntPtr.Zero);
        return result.ToArray();
    }

    public static IntPtr FindVisibleTopWebView(IntPtr root, int expectedLogicalHeight) {
        var dpi = GetDpiForWindow(root);
        if (dpi == 0) {
            dpi = 96;
        }
        var expectedHeight = Math.Max(
            1,
            (int)Math.Round(expectedLogicalHeight * dpi / 96.0));
        foreach (var window in Descendants(root)) {
            if (!IsWindowVisible(window) || WindowClass(window) != "WRY_WEBVIEW") {
                continue;
            }
            Rect bounds;
            if (GetClientRect(window, out bounds) &&
                bounds.Right - bounds.Left >= 100 &&
                Math.Abs((bounds.Bottom - bounds.Top) - expectedHeight) <= 1) {
                return window;
            }
        }
        return IntPtr.Zero;
    }

    public static bool ClickClientPoint(IntPtr window, int x, int y) {
        Rect bounds;
        if (!GetClientRect(window, out bounds) || x < 0 || y < 0 ||
            x >= bounds.Right - bounds.Left || y >= bounds.Bottom - bounds.Top) {
            return false;
        }
        var screen = new Point { X = x, Y = y };
        var previous = new Point();
        if (!ClientToScreen(window, ref screen) || !GetCursorPos(out previous)) {
            return false;
        }
        var root = GetAncestor(window, 2);
        if (root != IntPtr.Zero) {
            SetForegroundWindow(root);
        }
        if (!SetCursorPos(screen.X, screen.Y)) {
            return false;
        }
        var inputs = new[] {
            new Input {
                Type = 0,
                Union = new InputUnion {
                    Mouse = new MouseInput { Flags = 0x0002 }
                }
            },
            new Input {
                Type = 0,
                Union = new InputUnion {
                    Mouse = new MouseInput { Flags = 0x0004 }
                }
            }
        };
        var sent = SendInput((uint)inputs.Length, inputs, Marshal.SizeOf<Input>());
        System.Threading.Thread.Sleep(80);
        SetCursorPos(previous.X, previous.Y);
        return sent == (uint)inputs.Length;
    }

    public static bool DragClientPoint(
        IntPtr window,
        int startX,
        int startY,
        int endX,
        int endY,
        int steps,
        int intervalMilliseconds) {
        Rect bounds;
        if (!GetClientRect(window, out bounds) || steps < 1 || intervalMilliseconds < 0 ||
            startX < 0 || startY < 0 || endX < 0 || endY < 0 ||
            startX >= bounds.Right - bounds.Left || endX >= bounds.Right - bounds.Left ||
            startY >= bounds.Bottom - bounds.Top || endY >= bounds.Bottom - bounds.Top) {
            return false;
        }
        var start = new Point { X = startX, Y = startY };
        var end = new Point { X = endX, Y = endY };
        var previous = new Point();
        if (!ClientToScreen(window, ref start) || !ClientToScreen(window, ref end) ||
            !GetCursorPos(out previous)) {
            return false;
        }
        var root = GetAncestor(window, 2);
        if (root != IntPtr.Zero) {
            SetForegroundWindow(root);
        }
        if (!SetCursorPos(start.X, start.Y)) {
            return false;
        }
        var down = new[] {
            new Input {
                Type = 0,
                Union = new InputUnion { Mouse = new MouseInput { Flags = 0x0002 } }
            }
        };
        if (SendInput(1, down, Marshal.SizeOf<Input>()) != 1) {
            SetCursorPos(previous.X, previous.Y);
            return false;
        }
        var moved = true;
        for (var step = 1; step <= steps; step++) {
            var x = start.X + (end.X - start.X) * step / steps;
            var y = start.Y + (end.Y - start.Y) * step / steps;
            if (!SetCursorPos(x, y)) {
                moved = false;
                break;
            }
            if (intervalMilliseconds > 0) {
                System.Threading.Thread.Sleep(intervalMilliseconds);
            }
        }
        var up = new[] {
            new Input {
                Type = 0,
                Union = new InputUnion { Mouse = new MouseInput { Flags = 0x0004 } }
            }
        };
        var released = SendInput(1, up, Marshal.SizeOf<Input>()) == 1;
        if (intervalMilliseconds > 0) {
            System.Threading.Thread.Sleep(intervalMilliseconds);
        }
        SetCursorPos(previous.X, previous.Y);
        return moved && released;
    }

    public static bool PrepareForPhysicalInput(IntPtr window) {
        var root = GetAncestor(window, 2);
        if (root == IntPtr.Zero) {
            return false;
        }
        ShowWindow(root, 9);
        const uint preserveGeometryAndShow = 0x0001 | 0x0002 | 0x0040;
        var raised = SetWindowPos(
            root,
            new IntPtr(-1),
            0,
            0,
            0,
            0,
            preserveGeometryAndShow);
        var brought = BringWindowToTop(root);
        var foreground = SetForegroundWindow(root);
        System.Threading.Thread.Sleep(100);
        return raised && brought && foreground;
    }

    public static void RestoreAfterPhysicalInput(IntPtr window) {
        var root = GetAncestor(window, 2);
        if (root == IntPtr.Zero) {
            return;
        }
        const uint preserveGeometryAndShow = 0x0001 | 0x0002 | 0x0040;
        SetWindowPos(root, new IntPtr(-2), 0, 0, 0, 0, preserveGeometryAndShow);
    }

    public static string HitTestChainAtClientPoint(IntPtr window, int x, int y) {
        Rect bounds;
        if (!GetClientRect(window, out bounds) || x < 0 || y < 0 ||
            x >= bounds.Right - bounds.Left || y >= bounds.Bottom - bounds.Top) {
            return "invalid-client-point";
        }
        var screen = new Point { X = x, Y = y };
        if (!ClientToScreen(window, ref screen)) {
            return "client-to-screen-failed";
        }
        var current = WindowFromPoint(screen);
        var result = new StringBuilder();
        for (var depth = 0; current != IntPtr.Zero && depth < 16; depth++) {
            uint owner;
            GetWindowThreadProcessId(current, out owner);
            if (depth > 0) {
                result.AppendLine();
            }
            result.Append(depth)
                .Append(" hwnd=0x").Append(current.ToInt64().ToString("X"))
                .Append(" pid=").Append(owner)
                .Append(" class=").Append(WindowClass(current))
                .Append(" title=").Append(WindowText(current));
            current = GetParent(current);
        }
        return result.ToString();
    }

    public static bool PostClientClick(IntPtr window, int x, int y) {
        Rect bounds;
        if (!GetClientRect(window, out bounds) || x < 0 || y < 0 ||
            x >= bounds.Right - bounds.Left || y >= bounds.Bottom - bounds.Top ||
            x > ushort.MaxValue || y > ushort.MaxValue) {
            return false;
        }
        var point = new IntPtr((y << 16) | x);
        return PostMessageW(window, 0x0200, IntPtr.Zero, point) &&
            PostMessageW(window, 0x0201, new IntPtr(1), point) &&
            PostMessageW(window, 0x0202, IntPtr.Zero, point);
    }

    public static bool SendCurrentSize(IntPtr window, uint sizeType) {
        Rect bounds;
        if (!GetClientRect(window, out bounds)) {
            return false;
        }
        var width = bounds.Right - bounds.Left;
        var height = bounds.Bottom - bounds.Top;
        if (width <= 0 || height <= 0 || width > ushort.MaxValue || height > ushort.MaxValue) {
            return false;
        }
        var packed = new IntPtr((height << 16) | width);
        SendMessageW(window, 0x0005, new IntPtr(sizeType), packed);
        return true;
    }

    // Samples the final desktop composition, not gfxstream's source ColorBuffer. The 64x64
    // downsample keeps this diagnostic independent from the zero-copy display path and cheap
    // enough to catch a one-frame DWM black flash without perturbing the cadence probe.
    public sealed class DesktopFrameProbe : IDisposable {
        private readonly IntPtr window;
        private readonly int intervalMilliseconds;
        private System.Threading.Thread thread;
        private volatile bool stopping;
        private long samples;
        private long nearBlackFrames;
        private long maxConsecutiveNearBlackFrames;
        private long captureFailures;
        private long maxBlackPixelRatioPpm;
        private long distinctFrames;
        private long frameTransitions;
        private long maxConsecutiveIdenticalFrames;

        public DesktopFrameProbe(IntPtr window, int intervalMilliseconds) {
            this.window = window;
            this.intervalMilliseconds = Math.Max(1, intervalMilliseconds);
        }

        public long Samples { get { return System.Threading.Interlocked.Read(ref samples); } }
        public long NearBlackFrames { get { return System.Threading.Interlocked.Read(ref nearBlackFrames); } }
        public long MaxConsecutiveNearBlackFrames { get { return System.Threading.Interlocked.Read(ref maxConsecutiveNearBlackFrames); } }
        public long CaptureFailures { get { return System.Threading.Interlocked.Read(ref captureFailures); } }
        public long MaxBlackPixelRatioPpm { get { return System.Threading.Interlocked.Read(ref maxBlackPixelRatioPpm); } }
        public long DistinctFrames { get { return System.Threading.Interlocked.Read(ref distinctFrames); } }
        public long FrameTransitions { get { return System.Threading.Interlocked.Read(ref frameTransitions); } }
        public long MaxConsecutiveIdenticalFrames { get { return System.Threading.Interlocked.Read(ref maxConsecutiveIdenticalFrames); } }

        public void Start() {
            if (thread != null) {
                throw new InvalidOperationException("desktop frame probe already started");
            }
            thread = new System.Threading.Thread(CaptureLoop) {
                IsBackground = true,
                Name = "HD DWM frame probe"
            };
            thread.Start();
        }

        public void Stop() {
            stopping = true;
            if (thread != null && !thread.Join(5000)) {
                throw new TimeoutException("desktop frame probe did not stop within 5 seconds");
            }
        }

        public void Dispose() {
            Stop();
        }

        private void CaptureLoop() {
            const int probeWidth = 64;
            const int probeHeight = 64;
            const uint sourceCopyAndLayeredWindows = 0x40CC0020;
            var screen = GetDC(IntPtr.Zero);
            var memory = screen == IntPtr.Zero ? IntPtr.Zero : CreateCompatibleDC(screen);
            var bits = IntPtr.Zero;
            var bitmap = IntPtr.Zero;
            var previous = IntPtr.Zero;
            try {
                if (screen == IntPtr.Zero || memory == IntPtr.Zero) {
                    System.Threading.Interlocked.Increment(ref captureFailures);
                    return;
                }
                var info = new BitmapInfo {
                    Header = new BitmapInfoHeader {
                        Size = (uint)Marshal.SizeOf<BitmapInfoHeader>(),
                        Width = probeWidth,
                        Height = -probeHeight,
                        Planes = 1,
                        BitCount = 32,
                        Compression = 0,
                        SizeImage = probeWidth * probeHeight * 4
                    }
                };
                bitmap = CreateDIBSection(memory, ref info, 0, out bits, IntPtr.Zero, 0);
                if (bitmap == IntPtr.Zero || bits == IntPtr.Zero) {
                    System.Threading.Interlocked.Increment(ref captureFailures);
                    return;
                }
                previous = SelectObject(memory, bitmap);
                SetStretchBltMode(memory, 3);
                var pixels = new byte[probeWidth * probeHeight * 4];
                long consecutiveNearBlackFrames = 0;
                long consecutiveIdenticalFrames = 0;
                ulong previousFingerprint = 0;
                var hasPreviousFingerprint = false;
                var fingerprints = new System.Collections.Generic.HashSet<ulong>();
                var schedule = System.Diagnostics.Stopwatch.StartNew();
                long nextSampleAtMilliseconds = 0;
                while (!stopping) {
                    Rect rect;
                    if (!GetWindowRect(window, out rect) || rect.Right <= rect.Left || rect.Bottom <= rect.Top ||
                        !StretchBlt(memory, 0, 0, probeWidth, probeHeight, screen,
                            rect.Left, rect.Top, rect.Right - rect.Left, rect.Bottom - rect.Top,
                            sourceCopyAndLayeredWindows)) {
                        System.Threading.Interlocked.Increment(ref captureFailures);
                    } else {
                        Marshal.Copy(bits, pixels, 0, pixels.Length);
                        long blackPixels = 0;
                        ulong fingerprint = 1469598103934665603UL;
                        for (var offset = 0; offset < pixels.Length; offset += 4) {
                            if (pixels[offset] <= 12 && pixels[offset + 1] <= 12 && pixels[offset + 2] <= 12) {
                                ++blackPixels;
                            }
                            fingerprint ^= pixels[offset];
                            fingerprint *= 1099511628211UL;
                            fingerprint ^= pixels[offset + 1];
                            fingerprint *= 1099511628211UL;
                            fingerprint ^= pixels[offset + 2];
                            fingerprint *= 1099511628211UL;
                        }
                        var ratioPpm = blackPixels * 1000000L / (probeWidth * probeHeight);
                        System.Threading.Interlocked.Increment(ref samples);
                        if (fingerprints.Add(fingerprint)) {
                            System.Threading.Interlocked.Exchange(ref distinctFrames, fingerprints.Count);
                        }
                        if (hasPreviousFingerprint && fingerprint == previousFingerprint) {
                            ++consecutiveIdenticalFrames;
                        } else {
                            if (hasPreviousFingerprint) {
                                System.Threading.Interlocked.Increment(ref frameTransitions);
                            }
                            consecutiveIdenticalFrames = 1;
                            previousFingerprint = fingerprint;
                            hasPreviousFingerprint = true;
                        }
                        UpdateMaximum(ref maxConsecutiveIdenticalFrames, consecutiveIdenticalFrames);
                        UpdateMaximum(ref maxBlackPixelRatioPpm, ratioPpm);
                        if (ratioPpm >= 980000) {
                            System.Threading.Interlocked.Increment(ref nearBlackFrames);
                            ++consecutiveNearBlackFrames;
                            UpdateMaximum(ref maxConsecutiveNearBlackFrames, consecutiveNearBlackFrames);
                        } else {
                            consecutiveNearBlackFrames = 0;
                        }
                    }
                    nextSampleAtMilliseconds += intervalMilliseconds;
                    var delay = nextSampleAtMilliseconds - schedule.ElapsedMilliseconds;
                    if (delay > 0) {
                        System.Threading.Thread.Sleep((int)Math.Min(delay, int.MaxValue));
                    } else {
                        // A slow capture never creates a catch-up burst. Resume the requested
                        // cadence from the completion time of the sample that crossed its budget.
                        nextSampleAtMilliseconds = schedule.ElapsedMilliseconds;
                    }
                }
            } finally {
                if (previous != IntPtr.Zero && memory != IntPtr.Zero) {
                    SelectObject(memory, previous);
                }
                if (bitmap != IntPtr.Zero) {
                    DeleteObject(bitmap);
                }
                if (memory != IntPtr.Zero) {
                    DeleteDC(memory);
                }
                if (screen != IntPtr.Zero) {
                    ReleaseDC(IntPtr.Zero, screen);
                }
            }
        }

        private static void UpdateMaximum(ref long target, long value) {
            var current = System.Threading.Interlocked.Read(ref target);
            while (value > current) {
                var observed = System.Threading.Interlocked.CompareExchange(ref target, value, current);
                if (observed == current) {
                    return;
                }
                current = observed;
            }
        }
    }

    public static long ExerciseInteractiveResize(IntPtr root, int widthDelta, int heightDelta, int steps) {
        Rect original;
        if (root == IntPtr.Zero || steps < 1 || !GetWindowRect(root, out original)) {
            return -1;
        }
        var width = original.Right - original.Left;
        var height = original.Bottom - original.Top;
        if (width < 1 || height < 1) {
            return -1;
        }
        const uint wmEnterSizeMove = 0x0231;
        const uint wmExitSizeMove = 0x0232;
        const uint noZOrderNoActivate = 0x0004 | 0x0010;
        var timer = System.Diagnostics.Stopwatch.StartNew();
        SendMessageW(root, wmEnterSizeMove, IntPtr.Zero, IntPtr.Zero);
        try {
            for (var step = 1; step <= steps; step++) {
                var targetWidth = width + widthDelta * step / steps;
                var targetHeight = height + heightDelta * step / steps;
                if (!SetWindowPos(root, IntPtr.Zero, original.Left, original.Top,
                                  targetWidth, targetHeight, noZOrderNoActivate)) {
                    return -1;
                }
            }
            for (var step = steps - 1; step >= 0; step--) {
                var targetWidth = width + widthDelta * step / steps;
                var targetHeight = height + heightDelta * step / steps;
                if (!SetWindowPos(root, IntPtr.Zero, original.Left, original.Top,
                                  targetWidth, targetHeight, noZOrderNoActivate)) {
                    return -1;
                }
            }
        } finally {
            SendMessageW(root, wmExitSizeMove, IntPtr.Zero, IntPtr.Zero);
            timer.Stop();
        }
        return timer.ElapsedMilliseconds;
    }
}
"@
}

function Invoke-Hdctl {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $prefix = @("--data-root", $script:DataRoot)
    if ($script:isolated) {
        $prefix += "--no-start-host"
    }
    $output = & $script:Hdctl @prefix @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "hdctl $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join [Environment]::NewLine)
}

function Get-ScopedRuntimeProcesses {
    $needle = $script:DataRoot.ToLowerInvariant()
    return @(Get-CimInstance Win32_Process | Where-Object {
        $_.CommandLine -and $_.CommandLine.ToLowerInvariant().Contains($needle) -and
        $_.Name -in @(
            "hd-host.exe", "hd-worker.exe", "hd-ui-smoke.exe", "msedgewebview2.exe",
            "crosvm.exe", "hd-device-sim.exe",
            "hd-frame-producer.exe", "hd-adb-bridge.exe", "hd-rootcanal-adapter.exe",
            "hd-casimir-adapter.exe", "hd-uwb-adapter.exe", "hd-modem-adapter.exe"
        )
    } | Select-Object ProcessId, ParentProcessId, Name, CommandLine)
}

function Start-IsolatedHost {
    if ((Get-ScopedRuntimeProcesses).Count -ne 0) {
        throw "isolated Android data root already has running HD processes"
    }
    $hostExecutable = Join-Path $script:hostToolsRoot "hd-host.exe"
    $argumentLine = "--data-root `"$($script:DataRoot.Replace('"', '\"'))`""
    $previousRustLog = [Environment]::GetEnvironmentVariable("RUST_LOG", "Process")
    try {
        # The release gate consumes structured info-level Worker evidence. Keep an
        # ambient developer RUST_LOG=warn from silently disabling that contract.
        [Environment]::SetEnvironmentVariable("RUST_LOG", "info", "Process")
        $script:hostProcess = Start-Process `
            -FilePath $hostExecutable `
            -ArgumentList $argumentLine `
            -WorkingDirectory $script:hostToolsRoot `
            -RedirectStandardOutput $script:hostStdout `
            -RedirectStandardError $script:hostStderr `
            -WindowStyle Hidden `
            -PassThru
    } finally {
        [Environment]::SetEnvironmentVariable("RUST_LOG", $previousRustLog, "Process")
    }
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
    do {
        if ($script:hostProcess.HasExited) {
            $stderr = if (Test-Path -LiteralPath $script:hostStderr -PathType Leaf) {
                Get-Content -Raw -LiteralPath $script:hostStderr
            } else { "" }
            throw "isolated HD Host exited during startup: $stderr"
        }
        try {
            $health = Invoke-Hdctl health | ConvertFrom-Json
            if ($health.pid -eq $script:hostProcess.Id) {
                return
            }
        } catch {
            Start-Sleep -Milliseconds 100
        }
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "isolated HD Host did not become healthy within 15 seconds"
}

function Save-FailureRunEvidence {
    $source = Join-Path $script:DataRoot "runs\$($script:InstanceId)"
    if (-not (Test-Path -LiteralPath $source -PathType Container)) {
        return
    }
    $destination = Join-Path $script:outputRoot "failed-runs"
    if (Test-Path -LiteralPath $destination) {
        throw "refusing to replace existing failure evidence: $destination"
    }
    Copy-Item -LiteralPath $source -Destination $destination -Recurse
}

function Get-Instance {
    param([Guid]$Id)
    return (Invoke-Hdctl show $Id.ToString() | ConvertFrom-Json)
}

function Get-FrameMetrics {
    param($Instance)
    $path = Join-Path $script:DataRoot "runs\$($Instance.spec.id)\$($Instance.active_run_id)\frame-metrics-v2.json"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "frame metrics are missing: $path"
    }
    $metrics = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
    if ($metrics.generation -ne $Instance.frame_generation -or
        $metrics.produced_frames -ne $metrics.imported_frames -or
        $metrics.imported_frames -ne $metrics.presented_frames -or
        $metrics.dropped_frames -ne 0 -or
        $metrics.cpu_readback_bytes -ne 0 -or
        $metrics.software_blit_count -ne 0) {
        throw "strict frame invariant failed for run $($Instance.active_run_id)"
    }
    return $metrics
}

function Read-SharedTextFile {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return ""
    }
    $stream = [IO.FileStream]::new(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
    try {
        $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::UTF8, $true, 4096, $true)
        try {
            return $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
}

function Assert-NoUnmappedDisplayFrames {
    param($Instance, $Metrics)
    $runRoot = Join-Path $script:DataRoot "runs\$($Instance.spec.id)\$($Instance.active_run_id)"
    $logPath = Join-Path $runRoot "crosvm.stderr.log"
    $manifestPath = Join-Path $runRoot "manifest.json"
    if (-not (Test-Path -LiteralPath $logPath -PathType Leaf)) {
        throw "crosvm stderr is missing for frame broker audit: $logPath"
    }
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
        throw "run manifest is missing for native display audit: $manifestPath"
    }
    $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    $frameRequired = $manifest.launch.environment.HD_FRAME_REQUIRED
    $nativeZeroCopyRequired = $manifest.launch.environment.HD_NATIVE_ZERO_COPY_REQUIRED
    $brokerConfigured = $null -ne $manifest.launch.environment.HD_FRAME_BROKER_V2
    if ($frameRequired -ne "0" -or $nativeZeroCopyRequired -ne "1" -or
        $brokerConfigured) {
        throw "Windows native Player run did not enforce direct zero-copy without the external frame broker"
    }
    $logStream = [IO.FileStream]::new(
        $logPath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
    try {
        $logReader = [IO.StreamReader]::new(
            $logStream,
            [Text.Encoding]::UTF8,
            $true,
            4096,
            $true)
        try {
            $logText = $logReader.ReadToEnd()
        } finally {
            $logReader.Dispose()
        }
    } finally {
        $logStream.Dispose()
    }
    $unmappedCount = [regex]::Matches(
        $logText,
        [regex]::Escape("HD display selection rejected unmapped color buffer")).Count
    if ($unmappedCount -ne 0) {
        throw "gfxstream lost scanout mapping for $unmappedCount display frame(s) in run $($Instance.active_run_id)"
    }
    $cpuFramebufferFlipCount = [regex]::Matches(
        $logText,
        [regex]::Escape("strict Windows zero-copy display rejected a CPU framebuffer flip")).Count
    if ($cpuFramebufferFlipCount -ne 0) {
        throw "crosvm attempted $cpuFramebufferFlipCount CPU framebuffer flip(s) while native zero-copy was required in run $($Instance.active_run_id)"
    }
    $brokerBackpressureCount = [regex]::Matches(
        $logText,
        [regex]::Escape("HD strict frame broker rejected color buffer")).Count
    $logDigest = [Convert]::ToHexString(
        [Security.Cryptography.SHA256]::HashData(
            [Text.Encoding]::UTF8.GetBytes($logText))).ToLowerInvariant()
    return [ordered]@{
        contract = "selected-display-inflight-multibuffer-zero-copy"
        run_id = $Instance.active_run_id
        native_display_direct = $true
        native_zero_copy_required = $true
        external_frame_broker_configured = $false
        crosvm_stderr = $logPath
        crosvm_stderr_snapshot_sha256 = $logDigest
        unmapped_display_frame_count = 0
        cpu_framebuffer_flip_attempt_count = 0
        recording_broker_backpressure_count = $brokerBackpressureCount
        frame_metrics = $Metrics
    }
}

function Assert-Ready {
    param([Guid]$Id)
    $attemptId = [Guid]::NewGuid().ToString()
    $started = [DateTimeOffset]::UtcNow
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(120)
    $lastDetail = "instance status has not been observed"
    $lastRecordedDetail = ""
    do {
        $instance = Get-Instance $Id
        if ($instance.status.desired -ne "running" -or $instance.status.observed -ne "ready") {
            $lastDetail = "desired=$($instance.status.desired) observed=$($instance.status.observed)"
            if ($lastDetail -ne $lastRecordedDetail) {
                $script:readinessTimeline.Add([ordered]@{
                    attempt_id = $attemptId
                    elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
                    stage = "instance_state"
                    detail = $lastDetail
                })
                $lastRecordedDetail = $lastDetail
            }
            Start-Sleep -Milliseconds 500
            continue
        }
        if ([string]::IsNullOrWhiteSpace($instance.adb_serial)) {
            $lastDetail = "Ready instance has no ADB serial"
            if ($lastDetail -ne $lastRecordedDetail) {
                $script:readinessTimeline.Add([ordered]@{
                    attempt_id = $attemptId
                    elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
                    stage = "adb_serial"
                    detail = $lastDetail
                })
                $lastRecordedDetail = $lastDetail
            }
            Start-Sleep -Milliseconds 500
            continue
        }
        if (-not $instance.adb_ready) {
            # Native host display readiness intentionally precedes adbd. Wait for the separately
            # authenticated deferred ADB transition instead of treating a product-Ready display
            # as an immediate offline failure.
            $deferredMarker = $null
            if ($null -ne $instance.active_run_id) {
                $deferredMarkerPath = Join-Path $script:DataRoot (
                    "runs\{0}\{1}\deferred-adb-readiness-v1.json" -f
                        $Id, $instance.active_run_id)
                try {
                    if (Test-Path -LiteralPath $deferredMarkerPath -PathType Leaf) {
                        $deferredMarker = Read-SharedTextFile $deferredMarkerPath | ConvertFrom-Json
                    }
                } catch {
                    $deferredMarker = $null
                }
            }
            $lastDetail = if ($null -eq $deferredMarker) {
                "Ready native display is waiting for deferred ADB readiness"
            } else {
                "Ready native display is waiting for deferred ADB readiness: stage=$($deferredMarker.stage) detail=$($deferredMarker.detail)"
            }
            if ($lastDetail -ne $lastRecordedDetail) {
                $script:readinessTimeline.Add([ordered]@{
                    attempt_id = $attemptId
                    elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
                    stage = "adb_transport"
                    detail = $lastDetail
                    serial = $instance.adb_serial
                    worker_stage = if ($null -eq $deferredMarker) { $null } else { $deferredMarker.stage }
                    worker_detail = if ($null -eq $deferredMarker) { $null } else { $deferredMarker.detail }
                })
                $lastRecordedDetail = $lastDetail
            }
            Start-Sleep -Milliseconds 500
            continue
        }
        $boot = ((& $script:Adb -s $instance.adb_serial shell getprop sys.boot_completed 2>&1) -join "`n").Trim()
        if ($LASTEXITCODE -ne 0 -or $boot -ne "1") {
            $lastDetail = "sys.boot_completed=$boot"
            if ($lastDetail -ne $lastRecordedDetail) {
                $script:readinessTimeline.Add([ordered]@{
                    attempt_id = $attemptId
                    elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
                    stage = "android_boot"
                    detail = $lastDetail
                    serial = $instance.adb_serial
                })
                $lastRecordedDetail = $lastDetail
            }
            Start-Sleep -Milliseconds 500
            continue
        }
        $packages = ((& $script:Adb -s $instance.adb_serial shell cmd package list packages android 2>&1) -join "`n").Trim()
        if ($LASTEXITCODE -ne 0 -or $packages -notmatch "package:android") {
            $lastDetail = "package manager readback=$packages"
            if ($lastDetail -ne $lastRecordedDetail) {
                $script:readinessTimeline.Add([ordered]@{
                    attempt_id = $attemptId
                    elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
                    stage = "package_manager"
                    detail = $lastDetail
                    serial = $instance.adb_serial
                })
                $lastRecordedDetail = $lastDetail
            }
            Start-Sleep -Milliseconds 500
            continue
        }
        $metrics = Get-FrameMetrics $instance
        $script:readinessTimeline.Add([ordered]@{
            attempt_id = $attemptId
            elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
            stage = "complete"
            detail = "display, ADB, Android boot and PackageManager are ready"
            serial = $instance.adb_serial
        })
        return [pscustomobject]@{ instance = $instance; metrics = $metrics }
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    $script:readinessTimeline.Add([ordered]@{
        attempt_id = $attemptId
        elapsed_millis = [long]([DateTimeOffset]::UtcNow - $started).TotalMilliseconds
        stage = "timed_out"
        detail = $lastDetail
    })
    throw "instance $Id did not reach complete display/ADB/Android readiness within 120 seconds: $lastDetail"
}

function Assert-Stopped {
    param([Guid]$Id, [int]$WorkerPid)
    $instance = Get-Instance $Id
    if ($instance.status.observed -ne "stopped" -or
        $null -ne $instance.active_run_id -or
        $null -ne $instance.adb_serial) {
        throw "instance $Id did not persist the strict Stopped state"
    }
    if (-not (Get-Process -Id $WorkerPid -ErrorAction SilentlyContinue)) {
        throw "idle worker $WorkerPid exited unexpectedly after stop"
    }
    $runtimeChildren = Get-CimInstance Win32_Process -Filter "ParentProcessId=$WorkerPid" |
        Where-Object {
            $_.Name -eq "crosvm.exe" -or
            $_.Name -eq "hd-device-sim.exe" -or
            $_.Name -eq "hd-frame-producer.exe" -or
            $_.Name -eq "hd-adb-bridge.exe" -or
            $_.Name -like "hd-*-adapter.exe"
        }
    if ($runtimeChildren) {
        throw "runtime child processes remain below idle worker $WorkerPid"
    }
}

function Get-RuntimeProcessSnapshot {
    param([int]$WorkerPid)
    $processRows = @(Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name)
    $descendantIds = [Collections.Generic.HashSet[int]]::new()
    [void]$descendantIds.Add($WorkerPid)
    do {
        $added = $false
        foreach ($row in $processRows) {
            if ($descendantIds.Contains([int]$row.ParentProcessId) -and
                $descendantIds.Add([int]$row.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)

    $live = @($descendantIds | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
    if (-not ($live | Where-Object Id -eq $WorkerPid)) {
        throw "worker $WorkerPid disappeared while collecting the stability process snapshot"
    }
    return [pscustomobject]@{
        process_count = $live.Count
        handles = [long](($live | Measure-Object -Property HandleCount -Sum).Sum)
        private_bytes = [long](($live | Measure-Object -Property PrivateMemorySize64 -Sum).Sum)
        processes = @($live | Sort-Object Id | ForEach-Object {
            [ordered]@{
                pid = $_.Id
                name = $_.ProcessName
                handles = $_.HandleCount
                private_bytes = $_.PrivateMemorySize64
            }
        })
    }
}

function Get-StabilityLogSnapshot {
    param([Guid]$Id, [string]$RunId)
    $files = [Collections.Generic.List[IO.FileInfo]]::new()
    $workerLogRoot = Join-Path $script:DataRoot "logs\workers"
    if (Test-Path -LiteralPath $workerLogRoot -PathType Container) {
        Get-ChildItem -LiteralPath $workerLogRoot -File -Filter "$Id.jsonl*" -ErrorAction SilentlyContinue |
            ForEach-Object { $files.Add($_) }
    }
    $runRoot = Join-Path $script:DataRoot "runs\$Id\$RunId"
    if (Test-Path -LiteralPath $runRoot -PathType Container) {
        Get-ChildItem -LiteralPath $runRoot -File -Recurse -ErrorAction SilentlyContinue |
            ForEach-Object { $files.Add($_) }
    }
    return [pscustomobject]@{
        file_count = $files.Count
        bytes = [long](($files | Measure-Object -Property Length -Sum).Sum)
    }
}

function Invoke-Action {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    Invoke-Hdctl action $script:InstanceId.ToString() @Arguments | Out-Null
    $script:actions.Add(($Arguments -join ":"))
}

function Get-VisibleTopLevelWindows {
    param([int[]]$ProcessIds)

    $wanted = [Collections.Generic.HashSet[uint32]]::new()
    foreach ($processId in $ProcessIds) {
        [void]$wanted.Add([uint32]$processId)
    }
    $windows = [Collections.Generic.List[object]]::new()
    $callback = [HdRealGuestWindowNative+EnumWindowsProc]{
        param([IntPtr]$window, [IntPtr]$parameter)
        if (-not [HdRealGuestWindowNative]::IsWindowVisible($window)) {
            return $true
        }
        [uint32]$ownerPid = 0
        [void][HdRealGuestWindowNative]::GetWindowThreadProcessId($window, [ref]$ownerPid)
        if (-not $wanted.Contains($ownerPid)) {
            return $true
        }
        $title = [Text.StringBuilder]::new(512)
        $className = [Text.StringBuilder]::new(256)
        [void][HdRealGuestWindowNative]::GetWindowTextW($window, $title, $title.Capacity)
        [void][HdRealGuestWindowNative]::GetClassNameW($window, $className, $className.Capacity)
        $windows.Add([ordered]@{
            process_id = [int]$ownerPid
            handle = "0x$($window.ToInt64().ToString('X'))"
            title = $title.ToString()
            class_name = $className.ToString()
        })
        return $true
    }
    if (-not [HdRealGuestWindowNative]::EnumWindows($callback, [IntPtr]::Zero)) {
        throw "EnumWindows failed while auditing hidden crosvm surfaces"
    }
    return @($windows)
}

function Get-HdAttachedScanouts {
    param(
        [Parameter(Mandatory = $true)][IntPtr]$RootWindow,
        [Parameter(Mandatory = $true)][int[]]$ProcessIds
    )

    $wanted = [Collections.Generic.HashSet[uint32]]::new()
    foreach ($processId in $ProcessIds) {
        [void]$wanted.Add([uint32]$processId)
    }
    $scanouts = [Collections.Generic.List[object]]::new()
    foreach ($window in [HdRealGuestWindowNative]::Descendants($RootWindow)) {
        [uint32]$ownerPid = 0
        [void][HdRealGuestWindowNative]::GetWindowThreadProcessId($window, [ref]$ownerPid)
        if (-not $wanted.Contains($ownerPid) -or
            -not [HdRealGuestWindowNative]::IsWindowVisible($window)) {
            continue
        }
        $className = [HdRealGuestWindowNative]::WindowClass($window)
        $title = [HdRealGuestWindowNative]::WindowText($window)
        if ($className -notlike "CROSVM_*" -or $title -notmatch '^crosvm-scanout-(\d+)$') {
            continue
        }
        $bounds = [HdRealGuestWindowNative+Rect]::new()
        if (-not [HdRealGuestWindowNative]::GetClientRect($window, [ref]$bounds)) {
            throw "GetClientRect failed for attached crosvm scanout"
        }
        $scanouts.Add([pscustomobject][ordered]@{
            process_id = [int]$ownerPid
            handle = "0x$($window.ToInt64().ToString('X'))"
            raw_handle = $window.ToInt64()
            class_name = $className
            title = $title
            scanout_id = [int]$Matches[1]
            client_width = $bounds.Right - $bounds.Left
            client_height = $bounds.Bottom - $bounds.Top
        })
    }
    return @($scanouts)
}

function Wait-HdAttachedScanout {
    param(
        [Parameter(Mandatory = $true)][IntPtr]$RootWindow,
        [Parameter(Mandatory = $true)][int[]]$ProcessIds,
        [Parameter(Mandatory = $true)][int]$ScanoutId,
        [int]$TimeoutSeconds = 20
    )

    $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $attached = @(Get-HdAttachedScanouts $RootWindow $ProcessIds)
        $matching = @($attached | Where-Object scanout_id -eq $ScanoutId)
        if ($attached.Count -eq 1 -and $matching.Count -eq 1) {
            return $matching[0]
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "HD UI did not attach exactly scanout $ScanoutId within $TimeoutSeconds seconds"
}

function Start-HdctlActionProcess {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $script:Hdctl
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in @(
        "--data-root", $script:DataRoot, "--no-start-host", "action",
        $script:InstanceId.ToString()
    ) + $Arguments) {
        [void]$startInfo.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "failed to start background hdctl action"
    }
    return $process
}

function Read-BtsnoopHeader {
    param([Parameter(Mandatory = $true)][string]$Path)
    $header = [byte[]]::new(16)
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $offset = 0
        while ($offset -lt $header.Length) {
            $read = $stream.Read($header, $offset, $header.Length - $offset)
            if ($read -eq 0) { throw "Bluetooth HCI capture is shorter than its btsnoop header" }
            $offset += $read
        }
    } finally {
        $stream.Dispose()
    }
    return [Convert]::ToHexString($header).ToLowerInvariant()
}

function Read-BigEndianUInt32 {
    param([Parameter(Mandatory = $true)][IO.BinaryReader]$Reader)
    $bytes = $Reader.ReadBytes(4)
    if ($bytes.Count -ne 4) { throw "truncated big-endian uint32" }
    [Array]::Reverse($bytes)
    return [BitConverter]::ToUInt32($bytes, 0)
}

function Read-BigEndianUInt64 {
    param([Parameter(Mandatory = $true)][IO.BinaryReader]$Reader)
    $bytes = $Reader.ReadBytes(8)
    if ($bytes.Count -ne 8) { throw "truncated big-endian uint64" }
    [Array]::Reverse($bytes)
    return [BitConverter]::ToUInt64($bytes, 0)
}

function Read-Mp4Boxes {
    param(
        [Parameter(Mandatory = $true)][IO.BinaryReader]$Reader,
        [Parameter(Mandatory = $true)][long]$Start,
        [Parameter(Mandatory = $true)][long]$End,
        [Parameter(Mandatory = $true)][bool]$TopLevel,
        [Parameter(Mandatory = $true)]$Metrics,
        [int]$Depth = 0
    )
    if ($Depth -gt 8) { throw "MP4 box nesting exceeds the gate boundary" }
    $position = $Start
    while ($position -lt $End) {
        if ($End - $position -lt 8) { throw "truncated MP4 box header" }
        [void]$Reader.BaseStream.Seek($position, [IO.SeekOrigin]::Begin)
        [ulong]$size = Read-BigEndianUInt32 $Reader
        $kindBytes = $Reader.ReadBytes(4)
        if ($kindBytes.Count -ne 4) { throw "truncated MP4 box kind" }
        $kind = [Text.Encoding]::ASCII.GetString($kindBytes)
        [long]$headerSize = 8
        if ($size -eq 1) {
            $size = Read-BigEndianUInt64 $Reader
            $headerSize = 16
        } elseif ($size -eq 0) {
            $size = [ulong]($End - $position)
        }
        if ($size -gt [ulong][long]::MaxValue) { throw "MP4 box size exceeds the gate boundary" }
        [long]$boxSize = [long]$size
        [long]$payloadStart = $position + $headerSize
        [long]$payloadEnd = $position + $boxSize
        if ($boxSize -lt $headerSize -or $payloadEnd -gt $End) {
            throw "invalid MP4 box size for $kind"
        }
        if ($TopLevel) { [void]$Metrics.TopLevel.Add($kind) }

        if ($kind -eq "stsz") {
            if ($payloadEnd - $payloadStart -lt 12) { throw "truncated MP4 sample-size box" }
            [void]$Reader.BaseStream.Seek($payloadStart + 8, [IO.SeekOrigin]::Begin)
            [void]$Metrics.SampleCounts.Add([long](Read-BigEndianUInt32 $Reader))
        } elseif ($kind -eq "mvhd") {
            if ($payloadEnd - $payloadStart -lt 20) { throw "truncated MP4 movie header" }
            [void]$Reader.BaseStream.Seek($payloadStart, [IO.SeekOrigin]::Begin)
            $version = $Reader.ReadByte()
            if ($version -eq 0) {
                [void]$Reader.BaseStream.Seek($payloadStart + 12, [IO.SeekOrigin]::Begin)
                [ulong]$timescale = Read-BigEndianUInt32 $Reader
                [ulong]$duration = Read-BigEndianUInt32 $Reader
            } elseif ($version -eq 1 -and $payloadEnd - $payloadStart -ge 32) {
                [void]$Reader.BaseStream.Seek($payloadStart + 20, [IO.SeekOrigin]::Begin)
                [ulong]$timescale = Read-BigEndianUInt32 $Reader
                [ulong]$duration = Read-BigEndianUInt64 $Reader
            } else {
                throw "unsupported or truncated MP4 movie header"
            }
            if ($timescale -eq 0) { throw "MP4 movie header has a zero timescale" }
            [void]$Metrics.Durations.Add([long][Math]::Round(
                ([double]$duration * 1000.0) / [double]$timescale))
        } elseif ($kind -eq "tkhd") {
            if ($payloadEnd - $payloadStart -lt 8) { throw "truncated MP4 track header" }
            [void]$Reader.BaseStream.Seek($payloadEnd - 8, [IO.SeekOrigin]::Begin)
            $width = [long][Math]::Round([double](Read-BigEndianUInt32 $Reader) / 65536.0)
            $height = [long][Math]::Round([double](Read-BigEndianUInt32 $Reader) / 65536.0)
            if ($width -gt 0 -and $height -gt 0) {
                [void]$Metrics.Dimensions.Add("${width}x${height}")
            }
        }

        if ($kind -in @("moov", "trak", "mdia", "minf", "stbl")) {
            Read-Mp4Boxes -Reader $Reader -Start $payloadStart -End $payloadEnd `
                -TopLevel $false -Metrics $Metrics -Depth ($Depth + 1)
        }
        $position = $payloadEnd
    }
    if ($position -ne $End) { throw "MP4 box traversal did not end on a boundary" }
}

function Get-Mp4Metrics {
    param([Parameter(Mandatory = $true)][string]$Path)
    $metrics = [pscustomobject]@{
        TopLevel = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        SampleCounts = [Collections.Generic.List[long]]::new()
        Durations = [Collections.Generic.List[long]]::new()
        Dimensions = [Collections.Generic.List[string]]::new()
    }
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    $reader = [IO.BinaryReader]::new($stream, [Text.Encoding]::ASCII, $false)
    try {
        Read-Mp4Boxes -Reader $reader -Start 0 -End $stream.Length -TopLevel $true -Metrics $metrics
    } finally {
        $reader.Dispose()
    }
    foreach ($required in @("ftyp", "mdat", "moov")) {
        if (-not $metrics.TopLevel.Contains($required)) { throw "MP4 is missing $required" }
    }
    $sampleCount = ($metrics.SampleCounts | Measure-Object -Maximum).Maximum
    $durationMillis = ($metrics.Durations | Measure-Object -Maximum).Maximum
    $dimensions = @($metrics.Dimensions | Sort-Object -Unique)
    if ([long]$sampleCount -lt 2 -or [long]$durationMillis -le 0 -or $dimensions.Count -eq 0) {
        throw "MP4 has no usable video sample table, duration, or dimensions"
    }
    return [pscustomobject]@{
        sample_count = [long]$sampleCount
        media_duration_millis = [long]$durationMillis
        dimensions = $dimensions
    }
}

function Invoke-AdbShell {
    param(
        [Parameter(Mandatory = $true)][string]$Serial,
        [Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments
    )
    $output = & $script:Adb -s $Serial shell @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "adb shell $($Arguments -join ' ') failed: $($output -join [Environment]::NewLine)"
    }
    return ($output -join "`n").Replace("`r", "")
}

function Add-ActionReadback {
    param([string]$Control, [string]$Requested, [string]$Actual)
    $script:actionReadbacks.Add([ordered]@{
        control = $Control
        requested = $Requested
        actual = $Actual
    })
}

function Get-LastSensorEventLine {
    param([string]$Dump, [string]$SensorName)
    $escapedName = [Regex]::Escape($SensorName)
    $match = [Regex]::Match(
        $Dump,
        "(?ms)^${escapedName}: last \d+ events\r?\n(?<events>(?:[ \t]+\d+[^\r\n]*(?:\r?\n|$))+)")
    if (-not $match.Success) {
        throw "SensorService has no recent event block for $SensorName"
    }
    return @($match.Groups["events"].Value -split "\r?\n" |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) })[-1]
}

function Save-AndroidUiDump {
    param([string]$Serial, [string]$Path)
    $dump = & $script:Adb -s $Serial shell uiautomator dump /sdcard/hd-window.xml 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Android UI hierarchy dump failed: $($dump -join [Environment]::NewLine)"
    }
    $xml = & $script:Adb -s $Serial exec-out cat /sdcard/hd-window.xml 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Android UI hierarchy read failed: $($xml -join [Environment]::NewLine)"
    }
    Set-Content -LiteralPath $Path -Value ($xml -join "`n") -Encoding utf8NoBOM
}

function Get-AndroidUiNodeCenter {
    param([string]$Path, [string]$Attribute, [string]$Value)
    [xml]$document = Get-Content -Raw -LiteralPath $Path
    foreach ($node in $document.SelectNodes("//node")) {
        if ($node.GetAttribute($Attribute) -ne $Value) { continue }
        if ($node.GetAttribute("bounds") -match '^\[(\d+),(\d+)\]\[(\d+),(\d+)\]$') {
            return [pscustomobject]@{
                x = ([int]$Matches[1] + [int]$Matches[3]) / 2
                y = ([int]$Matches[2] + [int]$Matches[4]) / 2
            }
        }
    }
    return $null
}

function Stop-IfRunning {
    param([Guid]$Id)
    $instance = Get-Instance $Id
    if ($instance.status.desired -eq "running") {
        Invoke-Hdctl stop $Id.ToString() --graceful-timeout-ms 20000 | Out-Null
    }
}

if (-not (Test-Path -LiteralPath $Hdctl -PathType Leaf)) { throw "hdctl is missing: $Hdctl" }
if (-not (Test-Path -LiteralPath $Adb -PathType Leaf)) { throw "adb is missing: $Adb" }
if ($RunUiDisplayInput -and -not (Test-Path -LiteralPath $HdUi -PathType Leaf)) {
    throw "-RunUiDisplayInput requires the HD UI executable: $HdUi"
}
if ($RunUiDisplayInput) {
    $rootHdUi = Join-Path $DistRoot "hd.exe"
    if (Test-Path -LiteralPath $rootHdUi -PathType Leaf) {
        $selectedHdUiHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $HdUi).Hash
        $rootHdUiHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $rootHdUi).Hash
        if ($selectedHdUiHash -ne $rootHdUiHash) {
            throw "Windows runtime contains divergent HD UI entry points; refusing to regress an older windows\bin\hd.exe"
        }
    }
}
if ($RunUiDisplayInput -and
    -not (Test-Path -LiteralPath (Join-Path $hostToolsRoot "WebView2Loader.dll") -PathType Leaf)) {
    throw "-RunUiDisplayInput requires packaged WebView2Loader.dll beside the Windows runtime"
}
if ($RunBluetoothHogp -and -not $RunActions) {
    throw "-RunBluetoothHogp requires -RunActions"
}
if ($RunUwbFira -and -not $RunActions) {
    throw "-RunUwbFira requires -RunActions"
}
if ($RunLocationProbe -and -not $RunActions) {
    throw "-RunLocationProbe requires -RunActions"
}
if ($RunLocationRoute -and -not $RunActions) {
    throw "-RunLocationRoute requires -RunActions"
}
if ($RunLocationRoute -and -not $RunLocationProbe) {
    throw "-RunLocationRoute requires -RunLocationProbe so the same real GPS framework probe verifies the route"
}
if ($RunScreenRecording -and -not $RunActions) {
    throw "-RunScreenRecording requires -RunActions"
}
if ($SecondaryDisplayCount -gt 0 -and -not $RunActions) {
    throw "-SecondaryDisplayCount requires -RunActions"
}
if ($RunUiDisplayInput -and -not $RunActions) {
    throw "-RunUiDisplayInput requires -RunActions"
}
if ($RunAdbLossPowerFallback -and (-not $RunActions -or -not $isolated)) {
    throw "-RunAdbLossPowerFallback requires -RunActions and an isolated generated instance"
}
if ($RunUiDisplayInput -and -not $isolated) {
    throw "-RunUiDisplayInput requires an isolated generated instance and data root"
}
if ($isolated -and $GuestSettleSeconds -ne 0) {
    throw "isolated Windows product regression forbids Guest settle workarounds"
}
if ($RunLocationProbe -or $RunLocationRoute) {
    if ([string]::IsNullOrWhiteSpace($LocationProbeApk) -or
        -not (Test-Path -LiteralPath $LocationProbeApk -PathType Leaf)) {
        throw "-RunLocationProbe requires a regular -LocationProbeApk"
    }
    $LocationProbeApk = (Resolve-Path -LiteralPath $LocationProbeApk).Path
}
if ($DevFastArtifacts) {
    foreach ($relative in @("kernel", "initrd_android.img", "android_fstab.dt")) {
        $path = Join-Path $ArtifactRoot $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Android artifact is missing: $path"
        }
    }
    if (-not (Test-Path -LiteralPath $env:HD_DEV_GUEST_ROOTFS -PathType Leaf)) {
        throw "Android aggregate image is missing: $($env:HD_DEV_GUEST_ROOTFS)"
    }
    if ([string]::IsNullOrWhiteSpace($Aapt2) -or
        -not (Test-Path -LiteralPath $Aapt2 -PathType Leaf)) {
        throw "aapt2 is missing: $Aapt2"
    }
}
$requiredRuntimeNames = @(
    "hd-host.exe", "hd-worker.exe", "hdctl.exe", "crosvm.exe", "hd-device-sim.exe",
    "hd-frame-producer.exe", "hd-adb-bridge.exe", "hd-rootcanal-adapter.exe",
    "hd-casimir-adapter.exe", "hd-uwb-adapter.exe", "hd-modem-adapter.exe",
    "libgfxstream_backend.dll", "libEGL.dll", "libGLESv2.dll", "vulkan-1.dll"
)
foreach ($runtimeName in $requiredRuntimeNames) {
    $path = Join-Path $hostToolsRoot $runtimeName
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Windows Android runtime is missing: $path"
    }
}
if ($isolated) {
    if (Test-Path -LiteralPath $outputRoot) {
        $existingOutput = @(Get-ChildItem -LiteralPath $outputRoot -Force -ErrorAction Stop)
        if ($existingOutput.Count -ne 0) {
            throw "refusing to reuse a non-empty Android smoke evidence root: $outputRoot"
        }
    }
    if (Test-Path -LiteralPath $DataRoot) {
        $existingData = @(Get-ChildItem -LiteralPath $DataRoot -Force -ErrorAction Stop)
        if ($existingData.Count -ne 0) {
            throw "refusing to reuse a non-empty Android smoke data root: $DataRoot"
        }
    }
}
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
$runtimeClosurePaths = @($requiredRuntimeNames | ForEach-Object { Join-Path $hostToolsRoot $_ })
if ($RunUiDisplayInput) {
    $runtimeClosurePaths += $HdUi
    $runtimeClosurePaths += Join-Path $hostToolsRoot "WebView2Loader.dll"
    $runtimeClosurePaths += @(Get-ChildItem -LiteralPath (Join-Path $hostToolsRoot "ui") -Recurse -File |
        ForEach-Object { $_.FullName })
    $rootHdUi = Join-Path $DistRoot "hd.exe"
    if (Test-Path -LiteralPath $rootHdUi -PathType Leaf) {
        $runtimeClosurePaths += $rootHdUi
    }
}
$resolvedDistRoot = (Resolve-Path -LiteralPath $DistRoot).Path.TrimEnd('\')
$runtimeClosureEntries = @($runtimeClosurePaths | Sort-Object -Unique | ForEach-Object {
    $file = Get-Item -LiteralPath $_ -ErrorAction Stop
    $relativePath = if ($file.FullName.StartsWith(
        "$resolvedDistRoot\",
        [StringComparison]::OrdinalIgnoreCase)) {
        $file.FullName.Substring($resolvedDistRoot.Length + 1)
    } else {
        $file.FullName
    }
    [ordered]@{
        path = $relativePath.Replace('\', '/')
        size_bytes = [long]$file.Length
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()
    }
})
$runtimeClosureManifestPath = Join-Path $outputRoot "windows-runtime-closure.json"
($runtimeClosureEntries | ConvertTo-Json -Depth 4) + "`n" |
    Set-Content -LiteralPath $runtimeClosureManifestPath -Encoding utf8NoBOM
$runtimeClosureManifestSha256 =
    (Get-FileHash -Algorithm SHA256 -LiteralPath $runtimeClosureManifestPath).Hash.ToLowerInvariant()
if ($isolated -and -not (Test-Path -LiteralPath $DataRoot)) {
    New-Item -ItemType Directory -Path $DataRoot -Force | Out-Null
}

$spec = [ordered]@{
    schema_version = 2
    id = $InstanceId.ToString()
    name = "Windows AOSP Android 15 Controls"
    guest_kind = "android"
    microdroid = $null
    cpu_count = $GuestCpuCount
    memory_mib = $GuestMemoryMiB
    display = [ordered]@{
        width = 1080; height = 1920; dpi = 420; refresh_rate_hz = 60
        orientation = "portrait"; vsync = "on"; show_host_fps = $false
        secondary_displays = @()
    }
    adb = [ordered]@{ mode = "loopback"; host_port = $null; executable = $null }
    artifacts = $null
    boot = [ordered]@{ kernel_log_level = 4; panic_timeout_seconds = 5; boot_animation = $true }
    devices = [ordered]@{
        bluetooth = $true; nfc = $true; uwb = $true; modem = $true
        gnss = $true; sensors = $true; network = $true; audio = $true
        camera = $true; power = $true; touchpad = $false
    }
    host_audio_input = "disabled"
    restart_policy = "never"
    labels = [ordered]@{ gate = "windows-android-aosp-controls" }
}
for ($secondaryIndex = 1; $secondaryIndex -le $SecondaryDisplayCount; $secondaryIndex++) {
    $spec.display.secondary_displays += [ordered]@{
        id = [Guid]::NewGuid().ToString()
        name = "副屏 $secondaryIndex"
        width = if (($secondaryIndex % 2) -eq 1) { 1280 } else { 1024 }
        height = if (($secondaryIndex % 2) -eq 1) { 720 } else { 768 }
        dpi = if (($secondaryIndex % 2) -eq 1) { 240 } else { 200 }
        refresh_rate_hz = 60
    }
}

$failure = $null
$failureStack = $null
try {
    if ($isolated) {
        if ($DevFastArtifacts) {
            $directSelection = Invoke-Hdctl prepare-direct-dev-artifacts | ConvertFrom-Json
            if ([string]::IsNullOrWhiteSpace($directSelection.guest_bundle_digest) -or
                [string]::IsNullOrWhiteSpace($directSelection.host_bundle_digest) -or
                -not (Test-Path -LiteralPath $directSelection.store_root -PathType Container)) {
                throw "direct Android development artifact preparation returned an invalid selection"
            }
            $spec.artifacts = $directSelection
        }
        Start-IsolatedHost
        $spec | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $specPath -Encoding utf8NoBOM
        $createdRecord = Invoke-Hdctl create --spec $specPath | ConvertFrom-Json
        if ($createdRecord.spec.id -ne $InstanceId.ToString() -or
            $createdRecord.spec.guest_kind -ne "android") {
            throw "Host returned the wrong created Android instance identity"
        }
        $created = $true
    }
    $initial = Get-Instance $InstanceId
    $guestDigest = $initial.spec.artifacts.guest_bundle_digest
    $hostDigest = $initial.spec.artifacts.host_bundle_digest

    if ($RunActions) {
        if ($initial.status.observed -ne "ready") {
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
        }
        $actionReady = Assert-Ready $InstanceId
        $actionSerial = $actionReady.instance.adb_serial
        $readinessProbe = [Diagnostics.Stopwatch]::StartNew()
        $packageBackgroundHandler = Invoke-AdbShell -Serial $actionSerial -Arguments @(
            "cmd", "package", "wait-for-background-handler", "--timeout", "5000")
        $readinessProbe.Stop()
        if ($packageBackgroundHandler.Trim() -ne "Success") {
            throw "Host reported Ready before the PackageManager background handler drained: $packageBackgroundHandler"
        }
        $targetedReadinessEvidence = [ordered]@{
            contract = "boot-user-unlock-and-bounded-package-manager-background-handler"
            primary_user_unlocked = $true
            foreground_handler_responsive = $true
            background_handler_idle = $true
            global_broadcast_idle_required = $false
            verification_elapsed_ms = [int64]$readinessProbe.ElapsedMilliseconds
        }
        if ($GuestSettleSeconds -gt 0) {
            Start-Sleep -Seconds $GuestSettleSeconds
        }
        $runtimeSnapshot = Get-RuntimeProcessSnapshot ([int]$actionReady.instance.worker.pid)
        $crosvmProcessIds = @($runtimeSnapshot.processes |
            Where-Object name -eq "crosvm" |
            ForEach-Object { [int]$_.pid })
        if ($crosvmProcessIds.Count -eq 0) {
            throw "crosvm is missing while auditing hidden backend windows"
        }
        $visibleBackendWindows = @(Get-VisibleTopLevelWindows $crosvmProcessIds)
        $backendWindowEvidence = [ordered]@{
            contract = "hidden-backend-surface"
            crosvm_process_ids = $crosvmProcessIds
            visible_top_level_window_count = $visibleBackendWindows.Count
            visible_top_level_windows = $visibleBackendWindows
        }
        if ($visibleBackendWindows.Count -ne 0) {
            throw "hidden crosvm backend exposed $($visibleBackendWindows.Count) standalone Android window(s)"
        }
        if ($SecondaryDisplayCount -gt 0) {
            $runtimeDisplays = @($actionReady.instance.runtime_displays)
            if ($runtimeDisplays.Count -ne (1 + $SecondaryDisplayCount)) {
                throw "cold-start runtime display count is $($runtimeDisplays.Count), expected $(1 + $SecondaryDisplayCount)"
            }
            $primaryDisplay = @($runtimeDisplays | Where-Object { $_.display_id.kind -eq "primary" })
            if ($primaryDisplay.Count -ne 1 -or [int]$primaryDisplay[0].scanout_id -ne 0 -or
                [int]$primaryDisplay[0].width -ne 1080 -or [int]$primaryDisplay[0].height -ne 1920 -or
                [int]$primaryDisplay[0].dpi -ne 420) {
                throw "cold-start primary display identity or geometry is invalid"
            }

            $androidDisplayReadbacks = [Collections.Generic.List[object]]::new()
            for ($secondaryIndex = 1; $secondaryIndex -le $SecondaryDisplayCount; $secondaryIndex++) {
                $expected = $spec.display.secondary_displays[$secondaryIndex - 1]
                $runtime = @($runtimeDisplays | Where-Object {
                    $_.display_id.kind -eq "secondary" -and $_.display_id.id -eq $expected.id
                })
                if ($runtime.Count -ne 1 -or [int]$runtime[0].scanout_id -ne $secondaryIndex -or
                    [int]$runtime[0].width -ne [int]$expected.width -or
                    [int]$runtime[0].height -ne [int]$expected.height -or
                    [int]$runtime[0].dpi -ne [int]$expected.dpi -or
                    [int]$runtime[0].refresh_rate_hz -ne [int]$expected.refresh_rate_hz) {
                    throw "cold-start secondary display $secondaryIndex lost stable identity, scanout, or geometry"
                }

                $androidDisplayId = $secondaryIndex * 2
                $wmSize = Invoke-AdbShell -Serial $actionSerial -Arguments @(
                    "wm", "size", "-d", $androidDisplayId.ToString()
                )
                $wmDensity = Invoke-AdbShell -Serial $actionSerial -Arguments @(
                    "wm", "density", "-d", $androidDisplayId.ToString()
                )
                $expectedSize = "$($expected.width)x$($expected.height)"
                if (-not $wmSize.Contains($expectedSize)) {
                    throw "Android display $androidDisplayId size does not contain $expectedSize`: $wmSize"
                }
                if ($wmDensity -notmatch "(?:Physical|Override) density:\s*$($expected.dpi)(?:\s|$)") {
                    throw "Android display $androidDisplayId density does not contain $($expected.dpi)`: $wmDensity"
                }
                $androidDisplayReadbacks.Add([ordered]@{
                    product_id = $expected.id
                    scanout_id = $secondaryIndex
                    android_display_id = $androidDisplayId
                    width = [int]$expected.width
                    height = [int]$expected.height
                    dpi = [int]$expected.dpi
                    refresh_rate_hz = [int]$expected.refresh_rate_hz
                    wm_size = $wmSize.Trim()
                    wm_density = $wmDensity.Trim()
                })
            }
            $inputDump = Invoke-AdbShell $actionSerial dumpsys input
            $inputDumpPath = Join-Path $outputRoot "input-display-viewports.txt"
            [IO.File]::WriteAllText($inputDumpPath, "$inputDump`n", [Text.UTF8Encoding]::new($false))
            foreach ($androidDisplay in $androidDisplayReadbacks) {
                if ($inputDump -notmatch "displayId=$($androidDisplay.android_display_id)(?:,|\s)") {
                    throw "Android input dispatcher omitted display viewport $($androidDisplay.android_display_id)"
                }
            }
            $multiDisplayEvidence = [ordered]@{
                contract = "cold-start-stable-product-display-to-scanout-to-android-viewport"
                runtime_displays = $runtimeDisplays
                android_display_readbacks = $androidDisplayReadbacks
                input_viewports_artifact = $inputDumpPath
                input_viewports_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $inputDumpPath).Hash.ToLowerInvariant()
            }
            $actions.Add("cold-start-multi-display:$SecondaryDisplayCount")
            Add-ActionReadback multi_display `
                "primary+secondary=$($runtimeDisplays.Count);scanouts=0..$SecondaryDisplayCount" `
                "stable-product-id-to-scanout-to-Android-display-and-input-viewport"
        }
        foreach ($rotation in @(
            [pscustomobject]@{ orientation = "portrait"; value = 0 },
            [pscustomobject]@{ orientation = "landscape"; value = 1 },
            [pscustomobject]@{ orientation = "reverse-portrait"; value = 2 },
            [pscustomobject]@{ orientation = "reverse-landscape"; value = 3 },
            [pscustomobject]@{ orientation = "portrait"; value = 0 }
        )) {
            Invoke-Action rotate $rotation.orientation
            $window = Invoke-AdbShell $actionSerial dumpsys window
            if ($window -notmatch "(?m)^\s*mRotation=$($rotation.value)(?:\s|$)") {
                throw "WindowManager did not apply $($rotation.orientation) rotation $($rotation.value)"
            }
            Add-ActionReadback rotation $rotation.orientation "mRotation=$($rotation.value)"
        }
        if ($RunUiDisplayInput) {
            $webRoot = Join-Path $hostToolsRoot "ui"
            if (-not (Test-Path -LiteralPath (Join-Path $webRoot "index.html") -PathType Leaf)) {
                throw "packaged HD UI assets are missing: $webRoot"
            }
            $uiRuntimeRoot = Join-Path $outputRoot "ui-runtime"
            New-Item -ItemType Directory -Path $uiRuntimeRoot -Force | Out-Null
            $uiExecutable = Join-Path $uiRuntimeRoot "hd-ui-smoke.exe"
            Copy-Item -LiteralPath $HdUi -Destination $uiExecutable
            foreach ($dependency in @(
                "WebView2Loader.dll", "libgcc_s_seh-1.dll", "libstdc++-6.dll", "libwinpthread-1.dll"
            )) {
                $dependencyPath = Join-Path $hostToolsRoot $dependency
                if (Test-Path -LiteralPath $dependencyPath -PathType Leaf) {
                    Copy-Item -LiteralPath $dependencyPath -Destination $uiRuntimeRoot
                }
            }
            $uiStdout = Join-Path $outputRoot "hd-ui.stdout.txt"
            $uiStderr = Join-Path $outputRoot "hd-ui.stderr.txt"
            $uiProcess = Start-Process `
                -FilePath $uiExecutable `
                -ArgumentList @("--data-root", $DataRoot, "--web-root", $webRoot) `
                -WorkingDirectory $uiRuntimeRoot `
                -RedirectStandardOutput $uiStdout `
                -RedirectStandardError $uiStderr `
                -PassThru

            $rootWindow = [IntPtr]::Zero
            $titlebarWebView = [IntPtr]::Zero
            $renderWindow = [IntPtr]::Zero
            $uiDeadline = [DateTimeOffset]::UtcNow.AddSeconds(40)
            do {
                if ($uiProcess.HasExited) {
                    $uiError = if (Test-Path -LiteralPath $uiStderr) {
                        Get-Content -Raw -LiteralPath $uiStderr
                    } else { "" }
                    throw "isolated HD UI exited before publishing its WebView titlebar: $uiError"
                }
                $rootWindow = [HdRealGuestWindowNative]::FindVisibleTopLevelWindow(
                    [uint32]$uiProcess.Id,
                    "HD Android")
                if ($rootWindow -ne [IntPtr]::Zero) {
                    $legacyTitlebar = [HdRealGuestWindowNative]::FindWindowExW(
                        $rootWindow,
                        [IntPtr]::Zero,
                        "HD_NATIVE_TITLEBAR_V1",
                        $null)
                    if ($legacyTitlebar -ne [IntPtr]::Zero) {
                        throw "Windows Player created the removed native titlebar layer"
                    }
                    $titlebarWebView = [HdRealGuestWindowNative]::FindVisibleTopWebView(
                        $rootWindow,
                        30)
                }
                if ($titlebarWebView -ne [IntPtr]::Zero) {
                    $renderCandidate = [HdRealGuestWindowNative]::Descendants($rootWindow) |
                        Where-Object {
                            [HdRealGuestWindowNative]::WindowClass($_) -in @("subWin", "vulkan-subWin") -and
                            [HdRealGuestWindowNative]::IsWindowVisible($_)
                        } |
                        Select-Object -First 1
                    $renderWindow = if ($null -eq $renderCandidate) {
                        [IntPtr]::Zero
                    } else {
                        [IntPtr]::new($renderCandidate.ToInt64())
                    }
                }
                if ($renderWindow -ne [IntPtr]::Zero) {
                    break
                }
                Start-Sleep -Milliseconds 100
            } while ([DateTimeOffset]::UtcNow -lt $uiDeadline)
            if ($rootWindow -eq [IntPtr]::Zero -or $titlebarWebView -eq [IntPtr]::Zero -or
                $renderWindow -eq [IntPtr]::Zero) {
                throw "isolated HD UI did not publish its 30 px WebView titlebar and gfxstream render child"
            }
            $webviewDataDirectory = [IO.Path]::GetFullPath(
                (Join-Path $DataRoot "cache\webview2"))
            $legacyWebviewDataDirectory = Join-Path $uiRuntimeRoot "hd-ui-smoke.exe.WebView2"
            $webviewProfileProcesses = @()
            $webviewProfileDeadline = [DateTimeOffset]::UtcNow.AddSeconds(5)
            do {
                $webviewProfileProcesses = @(Get-CimInstance Win32_Process `
                    -Filter "Name='msedgewebview2.exe'" -ErrorAction SilentlyContinue |
                    Where-Object {
                        -not [string]::IsNullOrWhiteSpace([string]$_.CommandLine) -and
                        ([string]$_.CommandLine).Contains(
                            $webviewDataDirectory,
                            [StringComparison]::OrdinalIgnoreCase)
                    })
                if ($webviewProfileProcesses.Count -eq 0) {
                    Start-Sleep -Milliseconds 100
                }
            } while ($webviewProfileProcesses.Count -eq 0 -and
                [DateTimeOffset]::UtcNow -lt $webviewProfileDeadline)
            if (-not (Test-Path -LiteralPath $webviewDataDirectory -PathType Container) -or
                $webviewProfileProcesses.Count -eq 0 -or
                (Test-Path -LiteralPath $legacyWebviewDataDirectory)) {
                throw "Windows Player did not isolate WebView2 state under the HD data root"
            }

            $uiWindowTreePath = Join-Path $outputRoot "titlebar-window-tree.txt"
            $uiWindows = @($rootWindow) + @([HdRealGuestWindowNative]::Descendants($rootWindow))
            $visibleWebViews = @($uiWindows | Where-Object {
                [HdRealGuestWindowNative]::WindowClass($_) -eq "WRY_WEBVIEW" -and
                [HdRealGuestWindowNative]::IsWindowVisible($_)
            })
            $windowTree = foreach ($uiWindow in $uiWindows) {
                [uint32]$ownerPid = 0
                [void][HdRealGuestWindowNative]::GetWindowThreadProcessId(
                    $uiWindow,
                    [ref]$ownerPid)
                $client = [HdRealGuestWindowNative+Rect]::new()
                $hasClient = [HdRealGuestWindowNative]::GetClientRect($uiWindow, [ref]$client)
                [ordered]@{
                    hwnd = "0x$($uiWindow.ToInt64().ToString('X'))"
                    parent = "0x$([HdRealGuestWindowNative]::GetParent($uiWindow).ToInt64().ToString('X'))"
                    pid = $ownerPid
                    class = [HdRealGuestWindowNative]::WindowClass($uiWindow)
                    title = [HdRealGuestWindowNative]::WindowText($uiWindow)
                    visible = [HdRealGuestWindowNative]::IsWindowVisible($uiWindow)
                    client_width = if ($hasClient) { $client.Right - $client.Left } else { $null }
                    client_height = if ($hasClient) { $client.Bottom - $client.Top } else { $null }
                }
            }
            [IO.File]::WriteAllText(
                $uiWindowTreePath,
                (($windowTree | ConvertTo-Json -Depth 4) + "`n"),
                [Text.UTF8Encoding]::new($false))
            if ($visibleWebViews.Count -ne 1 -or $visibleWebViews[0] -ne $titlebarWebView) {
                throw "Windows Player must expose only the titlebar WebView while its sidebar and content surfaces are hidden"
            }
            $uiDisplayInputEvidence = [ordered]@{
                contract = "compact-webview-titlebar-and-real-pointer-input"
                ui_pid = $uiProcess.Id
                root_window = "0x$($rootWindow.ToInt64().ToString('X'))"
                titlebar_webview = "0x$($titlebarWebView.ToInt64().ToString('X'))"
                visible_webview_count = $visibleWebViews.Count
                hidden_body_webviews_excluded = $true
                guest_settle_seconds = $GuestSettleSeconds
                window_tree_artifact = $uiWindowTreePath
                window_tree_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $uiWindowTreePath).Hash.ToLowerInvariant()
                webview_data_directory = $webviewDataDirectory
                webview_process_ids = @($webviewProfileProcesses | ForEach-Object ProcessId)
                legacy_executable_webview_profile_absent = $true
                physical_input_pending = $true
            }

            $primaryAttached = Wait-HdAttachedScanout $rootWindow $crosvmProcessIds 0
            $displayHostWindow = [HdRealGuestWindowNative]::GetParent($renderWindow)
            $inputWindow = [IntPtr]::new([long]$primaryAttached.raw_handle)
            $inputCapabilitiesPath = Join-Path $outputRoot "titlebar-pointer-getevent-capabilities.txt"
            $inputCapabilities = Invoke-AdbShell $actionSerial getevent -lp
            [IO.File]::WriteAllText(
                $inputCapabilitiesPath,
                "$inputCapabilities`n",
                [Text.UTF8Encoding]::new($false))
            $primaryTouchMatch = [regex]::Match(
                $inputCapabilities,
                '(?ms)^add device \d+:\s+(?<path>/dev/input/event\d+)\s+name:\s+"HD Android Display 0"(?<body>.*?)(?=^add device \d+:|\z)')
            if (-not $primaryTouchMatch.Success) {
                throw "Android getevent capabilities omitted the primary HD Android Display 0 touchscreen"
            }
            $primaryTouchDevicePath = $primaryTouchMatch.Groups["path"].Value
            $primaryTouchBody = $primaryTouchMatch.Groups["body"].Value
            $primaryTouchXMaxMatch = [regex]::Match(
                $primaryTouchBody,
                'ABS_MT_POSITION_X\s+:\s+value\s+\d+,\s+min\s+\d+,\s+max\s+(?<max>\d+)')
            $primaryTouchYMaxMatch = [regex]::Match(
                $primaryTouchBody,
                'ABS_MT_POSITION_Y\s+:\s+value\s+\d+,\s+min\s+\d+,\s+max\s+(?<max>\d+)')
            if (-not $primaryTouchXMaxMatch.Success -or -not $primaryTouchYMaxMatch.Success) {
                throw "primary HD Android Display 0 touchscreen omitted absolute multitouch ranges"
            }
            $inputPhysicalWidth = [long]$primaryTouchXMaxMatch.Groups["max"].Value
            $inputPhysicalHeight = [long]$primaryTouchYMaxMatch.Groups["max"].Value
            if ($inputPhysicalWidth -ne [long]$actionReady.instance.spec.display.width -or
                $inputPhysicalHeight -ne [long]$actionReady.instance.spec.display.height) {
                throw "primary touchscreen physical range did not match the Android instance resolution"
            }
            $uiDisplayInputEvidence["primary_touch_device"] = [ordered]@{
                path = $primaryTouchDevicePath
                name = "HD Android Display 0"
                physical_extent = "${inputPhysicalWidth}x${inputPhysicalHeight}"
                capabilities_artifact = $inputCapabilitiesPath
            }
            $activeRunRoot = Join-Path `
                $DataRoot `
                "runs\$InstanceId\$($actionReady.instance.active_run_id)"
            $uiRotationEvidence = [Collections.Generic.List[object]]::new()
            $uiDisplayInputEvidence["rotation_geometry"] = $uiRotationEvidence
            foreach ($uiRotation in @(
                [pscustomobject]@{ orientation = "landscape"; value = 1; encoded = 2; x_num = 1; x_den = 4; y_num = 1; y_den = 4; x_high = $true; y_high = $false },
                [pscustomobject]@{ orientation = "reverse-portrait"; value = 2; encoded = 3; x_num = 1; x_den = 3; y_num = 1; y_den = 3; x_high = $true; y_high = $true },
                [pscustomobject]@{ orientation = "reverse-landscape"; value = 3; encoded = 4; x_num = 1; x_den = 4; y_num = 1; y_den = 3; x_high = $false; y_high = $true },
                [pscustomobject]@{ orientation = "portrait"; value = 0; encoded = 1; x_num = 2; x_den = 5; y_num = 1; y_den = 5; x_high = $false; y_high = $false }
            )) {
                if (-not [HdRealGuestWindowNative]::PrepareForPhysicalInput($renderWindow)) {
                    throw "Windows UI rotation geometry requires an interactive desktop"
                }
                $uiRotationProbe = [HdRealGuestWindowNative+DesktopFrameProbe]::new(
                    $renderWindow,
                    16)
                $uiRotationCommitted = $false
                $uiRotationWindowDump = ""
                $uiRotationHostWidth = 0
                $uiRotationHostHeight = 0
                $uiRotationRenderWidth = 0
                $uiRotationRenderHeight = 0
                $uiRotationInputWidth = 0
                $uiRotationInputHeight = 0
                $uiRotationAppliedViewport = [uint64]0
                $uiRotationGfxstreamViewport = [uint64]0
                $uiRotationAppliedRotation = [uint64]0
                $uiRotationForcedViewport = [uint64]0
                $uiRotationForcedRotation = [uint64]0
                $uiRotationPointerEvidence = $null
                $uiRotationCrosvmLog = Join-Path $activeRunRoot "crosvm.stderr.log"
                $uiRotationLogBefore = Read-SharedTextFile $uiRotationCrosvmLog
                try {
                    $uiRotationProbe.Start()
                    Invoke-Action rotate $uiRotation.orientation
                    $uiRotationDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
                    do {
                        $uiRotationWindowDump = Invoke-AdbShell $actionSerial dumpsys window
                        $uiRotationHostRect = [HdRealGuestWindowNative+Rect]::new()
                        $uiRotationRenderRect = [HdRealGuestWindowNative+Rect]::new()
                        $uiRotationInputRect = [HdRealGuestWindowNative+Rect]::new()
                        $uiRotationHostMeasured = [HdRealGuestWindowNative]::GetClientRect(
                            $displayHostWindow,
                            [ref]$uiRotationHostRect)
                        $uiRotationRenderMeasured = [HdRealGuestWindowNative]::GetClientRect(
                            $renderWindow,
                            [ref]$uiRotationRenderRect)
                        $uiRotationInputMeasured = [HdRealGuestWindowNative]::GetClientRect(
                            $inputWindow,
                            [ref]$uiRotationInputRect)
                        $uiRotationHostWidth = $uiRotationHostRect.Right - $uiRotationHostRect.Left
                        $uiRotationHostHeight = $uiRotationHostRect.Bottom - $uiRotationHostRect.Top
                        $uiRotationRenderWidth = $uiRotationRenderRect.Right - $uiRotationRenderRect.Left
                        $uiRotationRenderHeight = $uiRotationRenderRect.Bottom - $uiRotationRenderRect.Top
                        $uiRotationInputWidth = $uiRotationInputRect.Right - $uiRotationInputRect.Left
                        $uiRotationInputHeight = $uiRotationInputRect.Bottom - $uiRotationInputRect.Top
                        $uiRotationIsLandscape =
                            $uiRotation.orientation -in @("landscape", "reverse-landscape")
                        $uiRotationExpectedGuestWidth = if ($uiRotationIsLandscape) {
                            [long]$actionReady.instance.spec.display.height
                        } else {
                            [long]$actionReady.instance.spec.display.width
                        }
                        $uiRotationExpectedGuestHeight = if ($uiRotationIsLandscape) {
                            [long]$actionReady.instance.spec.display.width
                        } else {
                            [long]$actionReady.instance.spec.display.height
                        }
                        $uiRotationAspectCrossError = [Math]::Abs(
                            [long]$uiRotationHostWidth * $uiRotationExpectedGuestHeight -
                            [long]$uiRotationHostHeight * $uiRotationExpectedGuestWidth)
                        $uiRotationAspectTolerance = [Math]::Max(
                            $uiRotationExpectedGuestWidth,
                            $uiRotationExpectedGuestHeight)
                        $uiRotationExpectedViewport =
                            ([uint64]$uiRotationHostHeight -shl 16) -bor
                            [uint64]$uiRotationHostWidth
                        $uiRotationAppliedViewport = [HdRealGuestWindowNative]::WindowProperty(
                            $inputWindow,
                            "HD_APPLIED_VIEWPORT_V1")
                        $uiRotationGfxstreamViewport = [HdRealGuestWindowNative]::WindowProperty(
                            $renderWindow,
                            "HD_GFXSTREAM_APPLIED_VIEWPORT_V1")
                        $uiRotationAppliedRotation = [HdRealGuestWindowNative]::WindowProperty(
                            $inputWindow,
                            "HD_APPLIED_ROTATION_V1")
                        $uiRotationForcedViewport = [HdRealGuestWindowNative]::WindowProperty(
                            $inputWindow,
                            "HD_FORCED_VIEWPORT_V1")
                        $uiRotationForcedRotation = [HdRealGuestWindowNative]::WindowProperty(
                            $inputWindow,
                            "HD_FORCED_ROTATION_V1")
                        $uiRotationGeometryAligned =
                            $uiRotationHostMeasured -and
                            $uiRotationRenderMeasured -and
                            $uiRotationInputMeasured -and
                            $uiRotationHostWidth -gt 0 -and
                            $uiRotationHostHeight -gt 0 -and
                            $uiRotationRenderWidth -eq $uiRotationHostWidth -and
                            $uiRotationRenderHeight -eq $uiRotationHostHeight -and
                            $uiRotationInputWidth -eq $uiRotationHostWidth -and
                            $uiRotationInputHeight -eq $uiRotationHostHeight
                        $uiRotationAspectAligned = if ($uiRotationIsLandscape) {
                            $uiRotationHostWidth -gt $uiRotationHostHeight
                        } else {
                            $uiRotationHostWidth -lt $uiRotationHostHeight
                        }
                        $uiRotationAspectAligned = $uiRotationAspectAligned -and
                            $uiRotationAspectCrossError -le $uiRotationAspectTolerance
                        $uiRotationCommitted =
                            $uiRotationWindowDump -match
                                "(?m)^\s*mRotation=$($uiRotation.value)(?:\s|$)" -and
                            $uiRotationGeometryAligned -and
                            $uiRotationAspectAligned -and
                            $uiRotationAppliedRotation -eq [uint64]$uiRotation.encoded -and
                            $uiRotationAppliedViewport -eq $uiRotationExpectedViewport -and
                            $uiRotationGfxstreamViewport -eq $uiRotationExpectedViewport -and
                            $uiRotationForcedViewport -eq $uiRotationExpectedViewport -and
                            $uiRotationForcedRotation -eq [uint64]$uiRotation.encoded
                        if (-not $uiRotationCommitted) {
                            Start-Sleep -Milliseconds 100
                        }
                    } while (-not $uiRotationCommitted -and
                        [DateTimeOffset]::UtcNow -lt $uiRotationDeadline)
                    $uiRotationSampleDeadline = [DateTimeOffset]::UtcNow.AddSeconds(1)
                    while ($uiRotationProbe.Samples -lt 20 -and
                        [DateTimeOffset]::UtcNow -lt $uiRotationSampleDeadline) {
                        Start-Sleep -Milliseconds 16
                    }
                    if ($uiRotationCommitted) {
                        $rotationPrimeX = [Math]::Max(1, [int](
                            $uiRotationRenderWidth * 3 / 4))
                        $rotationPrimeY = [Math]::Max(1, [int](
                            $uiRotationRenderHeight * 2 / 3))
                        if (-not [HdRealGuestWindowNative]::PostClientClick(
                            $renderWindow,
                            $rotationPrimeX,
                            $rotationPrimeY)) {
                            throw "Windows could not prime the $($uiRotation.orientation) pointer coordinate state"
                        }
                        # Linux evdev suppresses unchanged ABS axes. Complete the diagnostic prime
                        # before opening the strict eight-event listener so the later physical click
                        # is guaranteed to change both X and Y; only that SendInput click is evidence.
                        Start-Sleep -Milliseconds 100
                        $rotationInputPath = Join-Path $outputRoot `
                            "rotation-pointer-$($uiRotation.orientation)-getevent.txt"
                        $rotationInputErrorPath = Join-Path $outputRoot `
                            "rotation-pointer-$($uiRotation.orientation)-getevent.stderr.txt"
                        $rotationGetevent = Start-Process -FilePath $Adb -ArgumentList @(
                            "-s", $actionSerial, "shell", "getevent", "-lt", "-c", "8",
                            $primaryTouchDevicePath
                        ) -WindowStyle Hidden -RedirectStandardOutput $rotationInputPath `
                            -RedirectStandardError $rotationInputErrorPath -PassThru
                        try {
                            Start-Sleep -Milliseconds 500
                            $rotationClickX = [Math]::Max(1, [int](
                                $uiRotationRenderWidth * $uiRotation.x_num / $uiRotation.x_den))
                            $rotationClickY = [Math]::Max(1, [int](
                                $uiRotationRenderHeight * $uiRotation.y_num / $uiRotation.y_den))
                            if (-not [HdRealGuestWindowNative]::ClickClientPoint(
                                $renderWindow,
                                $rotationClickX,
                                $rotationClickY)) {
                                throw "Windows SendInput failed for $($uiRotation.orientation) pointer quadrant probe"
                            }
                            if (-not $rotationGetevent.WaitForExit(5000)) {
                                Stop-Process -Id $rotationGetevent.Id -Force -ErrorAction SilentlyContinue
                                [void]$rotationGetevent.WaitForExit(2000)
                            }
                        } finally {
                            if (-not $rotationGetevent.HasExited) {
                                Stop-Process -Id $rotationGetevent.Id -Force -ErrorAction SilentlyContinue
                                [void]$rotationGetevent.WaitForExit(2000)
                            }
                        }
                        $rotationInputEvents = if (Test-Path -LiteralPath $rotationInputPath) {
                            Get-Content -Raw -LiteralPath $rotationInputPath
                        } else { "" }
                        $rotationXMatches = [regex]::Matches(
                            $rotationInputEvents,
                            '(?:ABS_MT_POSITION_X|ABS_X)\s+(?<value>[0-9a-fA-F]+)')
                        $rotationYMatches = [regex]::Matches(
                            $rotationInputEvents,
                            '(?:ABS_MT_POSITION_Y|ABS_Y)\s+(?<value>[0-9a-fA-F]+)')
                        $rotationPointerComplete =
                            $rotationInputEvents -match 'BTN_(?:TOUCH|LEFT)\s+DOWN' -and
                            $rotationInputEvents -match 'BTN_(?:TOUCH|LEFT)\s+UP' -and
                            $rotationXMatches.Count -gt 0 -and $rotationYMatches.Count -gt 0
                        $rotationGuestX = if ($rotationXMatches.Count -gt 0) {
                            [Convert]::ToInt64(
                                $rotationXMatches[$rotationXMatches.Count - 1].Groups["value"].Value,
                                16)
                        } else { -1 }
                        $rotationGuestY = if ($rotationYMatches.Count -gt 0) {
                            [Convert]::ToInt64(
                                $rotationYMatches[$rotationYMatches.Count - 1].Groups["value"].Value,
                                16)
                        } else { -1 }
                        $rotationXHigh = $rotationGuestX * 2 -ge $inputPhysicalWidth
                        $rotationYHigh = $rotationGuestY * 2 -ge $inputPhysicalHeight
                        $rotationQuadrantMatches = $rotationPointerComplete -and
                            $rotationXHigh -eq [bool]$uiRotation.x_high -and
                            $rotationYHigh -eq [bool]$uiRotation.y_high
                        $uiRotationPointerEvidence = [ordered]@{
                            transport = "Win32-SendInput-to-crosvm-to-Android-getevent"
                            artifact = $rotationInputPath
                            artifact_sha256 = (Get-FileHash -Algorithm SHA256 `
                                -LiteralPath $rotationInputPath).Hash.ToLowerInvariant()
                            coordinate_state_primed = $true
                            diagnostic_prime_point = "${rotationPrimeX}x${rotationPrimeY}"
                            host_point = "${rotationClickX}x${rotationClickY}"
                            guest_physical_point = "${rotationGuestX}x${rotationGuestY}"
                            guest_physical_extent = "${inputPhysicalWidth}x${inputPhysicalHeight}"
                            expected_x_high = [bool]$uiRotation.x_high
                            expected_y_high = [bool]$uiRotation.y_high
                            observed_x_high = $rotationXHigh
                            observed_y_high = $rotationYHigh
                            complete_contact = $rotationPointerComplete
                            quadrant_matches = $rotationQuadrantMatches
                        }
                    }
                } finally {
                    $uiRotationProbe.Stop()
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                }
                $uiRotationLogAfter = Read-SharedTextFile $uiRotationCrosvmLog
                $uiRotationLogTail = if ($uiRotationLogAfter.StartsWith($uiRotationLogBefore)) {
                    $uiRotationLogAfter.Substring($uiRotationLogBefore.Length)
                } else {
                    $uiRotationLogAfter
                }
                $uiRotationSwapchainRecreates = @(
                    [regex]::Matches(
                        $uiRotationLogTail,
                        'Creating swapchain with size (\d+)x(\d+)\.') |
                        ForEach-Object { "$($_.Groups[1].Value)x$($_.Groups[2].Value)" }
                )
                $uiRotationEvidence.Add([ordered]@{
                    orientation = $uiRotation.orientation
                    android_rotation = $uiRotation.value
                    committed = $uiRotationCommitted
                    host_extent = "${uiRotationHostWidth}x${uiRotationHostHeight}"
                    render_extent = "${uiRotationRenderWidth}x${uiRotationRenderHeight}"
                    input_extent = "${uiRotationInputWidth}x${uiRotationInputHeight}"
                    expected_guest_extent =
                        "${uiRotationExpectedGuestWidth}x${uiRotationExpectedGuestHeight}"
                    aspect_cross_error = $uiRotationAspectCrossError
                    maximum_aspect_cross_error = $uiRotationAspectTolerance
                    applied_viewport = $uiRotationAppliedViewport
                    gfxstream_viewport = $uiRotationGfxstreamViewport
                    applied_rotation = $uiRotationAppliedRotation
                    forced_viewport = $uiRotationForcedViewport
                    forced_rotation = $uiRotationForcedRotation
                    swapchain_recreate_count = $uiRotationSwapchainRecreates.Count
                    swapchain_sizes = $uiRotationSwapchainRecreates
                    minimum_dwm_samples = 20
                    dwm_sample_acquisition_timeout_millis = 1000
                    dwm_samples = $uiRotationProbe.Samples
                    dwm_near_black_frames = $uiRotationProbe.NearBlackFrames
                    dwm_max_consecutive_near_black_frames =
                        $uiRotationProbe.MaxConsecutiveNearBlackFrames
                    dwm_capture_failures = $uiRotationProbe.CaptureFailures
                    pointer_quadrant = $uiRotationPointerEvidence
                })
                if (-not $uiRotationCommitted -or
                    $uiRotationProbe.Samples -lt 20 -or
                    $uiRotationProbe.CaptureFailures -ne 0 -or
                    $uiRotationProbe.NearBlackFrames -ne 0 -or
                    $uiRotationProbe.MaxConsecutiveNearBlackFrames -ne 0 -or
                    $uiRotationSwapchainRecreates.Count -gt 1 -or
                    $null -eq $uiRotationPointerEvidence -or
                    -not $uiRotationPointerEvidence.quadrant_matches) {
                    throw "Windows UI rotation did not atomically align Android, Player, gfxstream, crosvm input and DWM composition"
                }
            }
            $titlebarAspectBounds = [HdRealGuestWindowNative+Rect]::new()
            if (-not [HdRealGuestWindowNative]::GetClientRect(
                $titlebarWebView,
                [ref]$titlebarAspectBounds)) {
                throw "GetClientRect failed for the WebView titlebar before maximized rotation"
            }
            $titlebarAspectHeight = $titlebarAspectBounds.Bottom - $titlebarAspectBounds.Top
            $clickWebViewWindowMaximize = {
                param([string]$Operation)
                $bounds = [HdRealGuestWindowNative+Rect]::new()
                if (-not [HdRealGuestWindowNative]::GetClientRect(
                    $titlebarWebView,
                    [ref]$bounds)) {
                    throw "GetClientRect failed before the WebView $Operation click"
                }
                $width = $bounds.Right - $bounds.Left
                $height = $bounds.Bottom - $bounds.Top
                $dpi = [HdRealGuestWindowNative]::GetDpiForWindow($rootWindow)
                if ($dpi -eq 0) { $dpi = 96 }
                $logicalWidth = [double]$width * 96.0 / [double]$dpi
                $logicalControlWidth = if ($logicalWidth -le 640.0) {
                    34.0
                } elseif ($logicalWidth -le 760.0) {
                    40.0
                } else {
                    42.0
                }
                $controlWidth = [Math]::Max(
                    1,
                    [int][Math]::Round($logicalControlWidth * [double]$dpi / 96.0))
                # Window controls are close, maximize, minimize from right to left. Click the
                # physical center of maximize using the same responsive widths as style.css.
                $x = $width - $controlWidth - [int][Math]::Floor($controlWidth / 2.0)
                $y = [int][Math]::Floor($height / 2.0)
                if ($x -lt 0 -or $y -lt 0 -or $x -ge $width -or $y -ge $height) {
                    throw "WebView $Operation click coordinate escaped the titlebar"
                }
                $prepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput($titlebarWebView)
                try {
                    if (-not $prepared -or
                        -not [HdRealGuestWindowNative]::ClickClientPoint(
                            $titlebarWebView,
                            $x,
                            $y)) {
                        throw "Windows SendInput could not click WebView maximize for $Operation"
                    }
                } finally {
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($titlebarWebView)
                }
                [pscustomobject]@{
                    operation = $Operation
                    input_path = "Win32-SendInput-to-WebView-window-control"
                    x = $x
                    y = $y
                    dpi = $dpi
                    responsive_control_width_px = $controlWidth
                    physical_input_prepared = $prepared
                }
            }
            $measureWindowRotation = {
                param(
                    [string]$Orientation,
                    [int]$AndroidRotation,
                    [uint64]$EncodedRotation
                )
                $isLandscape = $Orientation -in @("landscape", "reverse-landscape")
                $expectedGuestWidth = if ($isLandscape) {
                    [long]$actionReady.instance.spec.display.height
                } else {
                    [long]$actionReady.instance.spec.display.width
                }
                $expectedGuestHeight = if ($isLandscape) {
                    [long]$actionReady.instance.spec.display.width
                } else {
                    [long]$actionReady.instance.spec.display.height
                }
                $rootClient = [HdRealGuestWindowNative+Rect]::new()
                $hostClient = [HdRealGuestWindowNative+Rect]::new()
                $renderClient = [HdRealGuestWindowNative+Rect]::new()
                $inputClient = [HdRealGuestWindowNative+Rect]::new()
                $rootMeasured = [HdRealGuestWindowNative]::GetClientRect($rootWindow, [ref]$rootClient)
                $hostMeasured = [HdRealGuestWindowNative]::GetClientRect($displayHostWindow, [ref]$hostClient)
                $renderMeasured = [HdRealGuestWindowNative]::GetClientRect($renderWindow, [ref]$renderClient)
                $inputMeasured = [HdRealGuestWindowNative]::GetClientRect($inputWindow, [ref]$inputClient)
                $rootWidth = $rootClient.Right - $rootClient.Left
                $rootHeight = $rootClient.Bottom - $rootClient.Top
                $rootContentHeight = [Math]::Max(0, $rootHeight - $titlebarAspectHeight)
                $hostWidth = $hostClient.Right - $hostClient.Left
                $hostHeight = $hostClient.Bottom - $hostClient.Top
                $renderWidth = $renderClient.Right - $renderClient.Left
                $renderHeight = $renderClient.Bottom - $renderClient.Top
                $inputWidth = $inputClient.Right - $inputClient.Left
                $inputHeight = $inputClient.Bottom - $inputClient.Top
                $aspectCrossError = [Math]::Abs(
                    [long]$hostWidth * $expectedGuestHeight -
                    [long]$hostHeight * $expectedGuestWidth)
                $aspectTolerance = [Math]::Max($expectedGuestWidth, $expectedGuestHeight)
                $expectedViewport = ([uint64]$hostHeight -shl 16) -bor [uint64]$hostWidth
                $windowDump = Invoke-AdbShell $actionSerial dumpsys window
                $appliedViewport = [HdRealGuestWindowNative]::WindowProperty(
                    $inputWindow,
                    "HD_APPLIED_VIEWPORT_V1")
                $gfxstreamViewport = [HdRealGuestWindowNative]::WindowProperty(
                    $renderWindow,
                    "HD_GFXSTREAM_APPLIED_VIEWPORT_V1")
                $appliedRotation = [HdRealGuestWindowNative]::WindowProperty(
                    $inputWindow,
                    "HD_APPLIED_ROTATION_V1")
                $aligned = $rootMeasured -and $hostMeasured -and $renderMeasured -and $inputMeasured -and
                    $hostWidth -gt 0 -and $hostHeight -gt 0 -and
                    $renderWidth -eq $hostWidth -and $renderHeight -eq $hostHeight -and
                    $inputWidth -eq $hostWidth -and $inputHeight -eq $hostHeight -and
                    (($isLandscape -and $hostWidth -gt $hostHeight) -or
                        (-not $isLandscape -and $hostWidth -lt $hostHeight)) -and
                    $aspectCrossError -le $aspectTolerance -and
                    $windowDump -match "(?m)^\s*mRotation=${AndroidRotation}(?:\s|$)" -and
                    $appliedRotation -eq $EncodedRotation -and
                    $appliedViewport -eq $expectedViewport -and
                    $gfxstreamViewport -eq $expectedViewport
                [pscustomobject]@{
                    aligned = $aligned
                    orientation = $Orientation
                    root_extent = "${rootWidth}x${rootHeight}"
                    root_content_extent = "${rootWidth}x${rootContentHeight}"
                    host_extent = "${hostWidth}x${hostHeight}"
                    render_extent = "${renderWidth}x${renderHeight}"
                    input_extent = "${inputWidth}x${inputHeight}"
                    no_letterbox = [Math]::Abs($rootWidth - $hostWidth) -le 1 -and
                        [Math]::Abs($rootContentHeight - $hostHeight) -le 1
                    aspect_cross_error = $aspectCrossError
                    maximum_aspect_cross_error = $aspectTolerance
                    applied_viewport = $appliedViewport
                    gfxstream_viewport = $gfxstreamViewport
                    applied_rotation = $appliedRotation
                }
            }
            $maximizedRotationProbe = [HdRealGuestWindowNative+DesktopFrameProbe]::new(
                $renderWindow,
                16)
            $maximizedCrosvmLog = Join-Path $activeRunRoot "crosvm.stderr.log"
            $maximizedLogBefore = Read-SharedTextFile $maximizedCrosvmLog
            $maximizedLandscape = $null
            $maximizedAfterNativeSize = $null
            $restoredLandscape = $null
            $restoredPortrait = $null
            $maximizeClickEvidence = $null
            $restoreClickEvidence = $null
            try {
                $maximizedRotationProbe.Start()
                $maximizeClickEvidence = & $clickWebViewWindowMaximize "maximize"
                $maximizeDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
                while (-not [HdRealGuestWindowNative]::IsZoomed($rootWindow) -and
                    [DateTimeOffset]::UtcNow -lt $maximizeDeadline) {
                    Start-Sleep -Milliseconds 50
                }
                if (-not [HdRealGuestWindowNative]::IsZoomed($rootWindow)) {
                    throw "Windows root did not enter the maximized state"
                }
                Invoke-Action rotate landscape
                $maximizedRotationDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
                do {
                    $maximizedLandscape = & $measureWindowRotation "landscape" 1 2
                    if (-not $maximizedLandscape.aligned) {
                        Start-Sleep -Milliseconds 100
                    }
                } while (-not $maximizedLandscape.aligned -and
                    [DateTimeOffset]::UtcNow -lt $maximizedRotationDeadline)
                if (-not $maximizedLandscape.aligned -or
                    -not [HdRealGuestWindowNative]::IsZoomed($rootWindow)) {
                    throw "maximized Windows Player did not commit the landscape Android geometry"
                }
                if (-not [HdRealGuestWindowNative]::SendCurrentSize($rootWindow, 2)) {
                    throw "Windows could not replay the maximized WM_SIZE aspect transaction"
                }
                $nativeSizeDeadline = [DateTimeOffset]::UtcNow.AddSeconds(5)
                do {
                    $maximizedAfterNativeSize = & $measureWindowRotation "landscape" 1 2
                    if (-not $maximizedAfterNativeSize.aligned) {
                        Start-Sleep -Milliseconds 50
                    }
                } while (-not $maximizedAfterNativeSize.aligned -and
                    [DateTimeOffset]::UtcNow -lt $nativeSizeDeadline)
                if (-not $maximizedAfterNativeSize.aligned) {
                    throw "maximized WM_SIZE replay used a stale portrait aspect controller"
                }
                $restoreClickEvidence = & $clickWebViewWindowMaximize "restore"
                $restoreDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
                do {
                    $restoredLandscape = & $measureWindowRotation "landscape" 1 2
                    if ([HdRealGuestWindowNative]::IsZoomed($rootWindow) -or
                        -not $restoredLandscape.aligned -or
                        -not $restoredLandscape.no_letterbox) {
                        Start-Sleep -Milliseconds 100
                    }
                } while (([HdRealGuestWindowNative]::IsZoomed($rootWindow) -or
                        -not $restoredLandscape.aligned -or
                        -not $restoredLandscape.no_letterbox) -and
                    [DateTimeOffset]::UtcNow -lt $restoreDeadline)
                if ([HdRealGuestWindowNative]::IsZoomed($rootWindow) -or
                    -not $restoredLandscape.aligned -or
                    -not $restoredLandscape.no_letterbox) {
                    throw "restored Windows Player did not adopt the landscape window aspect without letterboxing"
                }
                Invoke-Action rotate portrait
                $portraitRestoreDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
                do {
                    $restoredPortrait = & $measureWindowRotation "portrait" 0 1
                    if (-not $restoredPortrait.aligned -or -not $restoredPortrait.no_letterbox) {
                        Start-Sleep -Milliseconds 100
                    }
                } while ((-not $restoredPortrait.aligned -or
                        -not $restoredPortrait.no_letterbox) -and
                    [DateTimeOffset]::UtcNow -lt $portraitRestoreDeadline)
                if (-not $restoredPortrait.aligned -or -not $restoredPortrait.no_letterbox) {
                    throw "Windows Player did not restore the original portrait aspect without letterboxing"
                }
                $maximizedSampleDeadline = [DateTimeOffset]::UtcNow.AddSeconds(1)
                while ($maximizedRotationProbe.Samples -lt 30 -and
                    [DateTimeOffset]::UtcNow -lt $maximizedSampleDeadline) {
                    Start-Sleep -Milliseconds 16
                }
            } finally {
                if ([HdRealGuestWindowNative]::IsZoomed($rootWindow)) {
                    [void][HdRealGuestWindowNative]::ShowWindow($rootWindow, 9)
                }
                $maximizedRotationProbe.Stop()
            }
            $maximizedLogAfter = Read-SharedTextFile $maximizedCrosvmLog
            $maximizedLogTail = if ($maximizedLogAfter.StartsWith($maximizedLogBefore)) {
                $maximizedLogAfter.Substring($maximizedLogBefore.Length)
            } else {
                $maximizedLogAfter
            }
            $maximizedSwapchainRecreates = @(
                [regex]::Matches(
                    $maximizedLogTail,
                    'Creating swapchain with size (\d+)x(\d+)\.') |
                    ForEach-Object { "$($_.Groups[1].Value)x$($_.Groups[2].Value)" }
            )
            if ($maximizedRotationProbe.Samples -lt 30 -or
                $maximizedRotationProbe.CaptureFailures -ne 0 -or
                $maximizedRotationProbe.NearBlackFrames -ne 0 -or
                $maximizedRotationProbe.MaxConsecutiveNearBlackFrames -ne 0 -or
                $maximizedRotationProbe.DistinctFrames -lt 4 -or
                $maximizedRotationProbe.FrameTransitions -lt 4 -or
                $maximizedSwapchainRecreates.Count -gt 4) {
                throw "maximized rotate/restore produced an incomplete, black, or frozen DWM frame sequence"
            }
            $uiDisplayInputEvidence["maximized_rotation_restore"] = [ordered]@{
                contract = "maximized-rotation-native-aspect-and-letterbox-free-restore"
                maximize_control = $maximizeClickEvidence
                restore_control = $restoreClickEvidence
                maximized_landscape = $maximizedLandscape
                maximized_after_native_wm_size = $maximizedAfterNativeSize
                restored_landscape = $restoredLandscape
                restored_portrait = $restoredPortrait
                dwm_samples = $maximizedRotationProbe.Samples
                dwm_near_black_frames = $maximizedRotationProbe.NearBlackFrames
                dwm_max_consecutive_near_black_frames =
                    $maximizedRotationProbe.MaxConsecutiveNearBlackFrames
                dwm_capture_failures = $maximizedRotationProbe.CaptureFailures
                dwm_distinct_frames = $maximizedRotationProbe.DistinctFrames
                dwm_frame_transitions = $maximizedRotationProbe.FrameTransitions
                swapchain_recreate_count = $maximizedSwapchainRecreates.Count
                swapchain_sizes = $maximizedSwapchainRecreates
            }
            $resizeCrosvmLog = Join-Path $activeRunRoot "crosvm.stderr.log"
            Start-Sleep -Milliseconds 250
            $resizeLogBefore = Read-SharedTextFile $resizeCrosvmLog
            $interactiveResizeSteps = 8
            $interactiveResizeElapsedMs = [HdRealGuestWindowNative]::ExerciseInteractiveResize(
                $rootWindow,
                96,
                168,
                $interactiveResizeSteps)
            if ($interactiveResizeElapsedMs -lt 0 -or $interactiveResizeElapsedMs -gt 2000) {
                throw "Windows interactive resize did not complete within the 2 second UI-thread budget: $interactiveResizeElapsedMs ms"
            }
            $inputRegionDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                $inputVisualRegionEmpty =
                    [HdRealGuestWindowNative]::IsWindowRegionEmpty($inputWindow)
                if (-not $inputVisualRegionEmpty) {
                    Start-Sleep -Milliseconds 50
                }
            } while (-not $inputVisualRegionEmpty -and
                [DateTimeOffset]::UtcNow -lt $inputRegionDeadline)
            if (-not $inputVisualRegionEmpty) {
                throw "embedded crosvm input HWND retained a visual region that can cover gfxstream"
            }
            $uiDisplayInputEvidence["crosvm_input_visual_region_empty"] = $true
            $resizeGeometryDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            $resizeGeometryAligned = $false
            do {
                $hostRect = [HdRealGuestWindowNative+Rect]::new()
                $renderRectAfterResize = [HdRealGuestWindowNative+Rect]::new()
                $inputRectAfterResize = [HdRealGuestWindowNative+Rect]::new()
                $hostMeasured = [HdRealGuestWindowNative]::GetClientRect(
                    $displayHostWindow,
                    [ref]$hostRect)
                $renderMeasured = [HdRealGuestWindowNative]::GetClientRect(
                    $renderWindow,
                    [ref]$renderRectAfterResize)
                $inputMeasured = [HdRealGuestWindowNative]::GetClientRect(
                    $inputWindow,
                    [ref]$inputRectAfterResize)
                $resizeGeometryAligned = $hostMeasured -and $renderMeasured -and $inputMeasured -and
                    ($hostRect.Right - $hostRect.Left) -eq ($renderRectAfterResize.Right - $renderRectAfterResize.Left) -and
                    ($hostRect.Bottom - $hostRect.Top) -eq ($renderRectAfterResize.Bottom - $renderRectAfterResize.Top) -and
                    ($hostRect.Right - $hostRect.Left) -eq ($inputRectAfterResize.Right - $inputRectAfterResize.Left) -and
                    ($hostRect.Bottom - $hostRect.Top) -eq ($inputRectAfterResize.Bottom - $inputRectAfterResize.Top)
                if (-not $resizeGeometryAligned) {
                    Start-Sleep -Milliseconds 50
                }
            } while (-not $resizeGeometryAligned -and [DateTimeOffset]::UtcNow -lt $resizeGeometryDeadline)
            if (-not $resizeGeometryAligned) {
                throw "Windows interactive resize left NativeDisplayHost, gfxstream render and crosvm input geometry split"
            }
            # The low-priority settle timer and Worker heartbeat can finish after the HWNDs first
            # report matching geometry. Include that tail and reject visual stalls that frame
            # counters cannot see because a swapchain recreate is not a dropped Guest frame.
            $finalResizeWidth = $hostRect.Right - $hostRect.Left
            $finalResizeHeight = $hostRect.Bottom - $hostRect.Top
            $finalResizeSize = "${finalResizeWidth}x${finalResizeHeight}"
            $finalResizeViewportProperty =
                ([uint64]$finalResizeHeight -shl 16) -bor [uint64]$finalResizeWidth
            $swapchainExtentDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                Start-Sleep -Milliseconds 100
                $resizeLogAfter = Read-SharedTextFile $resizeCrosvmLog
                $allSwapchainSizes = @(
                    [regex]::Matches($resizeLogAfter, 'Creating swapchain with size (\d+)x(\d+)\.') |
                        ForEach-Object { "$($_.Groups[1].Value)x$($_.Groups[2].Value)" }
                )
                $nativeSwapchainViewportProperty =
                    [HdRealGuestWindowNative]::WindowProperty(
                        $renderWindow,
                        "HD_GFXSTREAM_APPLIED_VIEWPORT_V1")
            } while ($nativeSwapchainViewportProperty -ne $finalResizeViewportProperty -and
                $finalResizeSize -notin $allSwapchainSizes -and
                [DateTimeOffset]::UtcNow -lt $swapchainExtentDeadline)
            $resizeCrosvmLogArtifact = Join-Path $outputRoot "titlebar-crosvm-resize.log"
            [IO.File]::WriteAllText(
                $resizeCrosvmLogArtifact,
                $resizeLogAfter,
                [Text.UTF8Encoding]::new($false))
            $uiDisplayInputEvidence["native_resize_crosvm_log"] = $resizeCrosvmLogArtifact
            $uiDisplayInputEvidence["native_resize_crosvm_log_sha256"] =
                (Get-FileHash -Algorithm SHA256 -LiteralPath $resizeCrosvmLogArtifact).Hash.ToLowerInvariant()
            $resizeLogTail = if ($resizeLogAfter.StartsWith($resizeLogBefore)) {
                $resizeLogAfter.Substring($resizeLogBefore.Length)
            } else {
                $resizeLogAfter
            }
            $projectionProperties = [ordered]@{}
            foreach ($propertyName in @(
                "HD_LATEST_VIEWPORT_V1",
                "HD_LATEST_ROTATION_V1",
                "HD_APPLIED_VIEWPORT_V1",
                "HD_APPLIED_ROTATION_V1",
                "HD_FORCE_VIEWPORT_PENDING_V1",
                "HD_FORCED_VIEWPORT_V1",
                "HD_FORCED_ROTATION_V1",
                "HD_GFXSTREAM_SETUP_VIEWPORT_V1",
                "HD_GFXSTREAM_SETUP_BLOCKED_V1",
                "HD_GFXSTREAM_COMMIT_VIEWPORT_V1",
                "HD_GFXSTREAM_COMMIT_STATUS_V1")) {
                $projectionProperties[$propertyName] = [HdRealGuestWindowNative]::WindowProperty(
                    $inputWindow,
                    $propertyName)
            }
            $uiDisplayInputEvidence["native_projection_properties"] = $projectionProperties
            $uiDisplayInputEvidence["native_swapchain_viewport_property"] =
                $nativeSwapchainViewportProperty
            $resizeSwapchainSizes = @(
                [regex]::Matches($resizeLogTail, 'Creating swapchain with size (\d+)x(\d+)\.') |
                    ForEach-Object { "$($_.Groups[1].Value)x$($_.Groups[2].Value)" }
            )
            if ($nativeSwapchainViewportProperty -ne $finalResizeViewportProperty) {
                throw "Windows gfxstream applied viewport never reached the final native extent ${finalResizeSize}: property=$nativeSwapchainViewportProperty logs=$($allSwapchainSizes -join ',')"
            }
            $verifiedSwapchainSizes = @($allSwapchainSizes)
            if ($finalResizeSize -notin $verifiedSwapchainSizes) {
                $verifiedSwapchainSizes += $finalResizeSize
            }
            $intermediateSwapchainSizes = @(
                $resizeSwapchainSizes | Where-Object { $_ -ne $finalResizeSize }
            )
            if ($intermediateSwapchainSizes.Count -ne 0 -or $resizeSwapchainSizes.Count -gt 1) {
                throw "Windows interactive resize recreated gfxstream swapchains instead of coalescing to the final extent: $($resizeSwapchainSizes -join ',')"
            }
            $uiDisplayInputEvidence["interactive_resize_steps"] = $interactiveResizeSteps * 2
            $uiDisplayInputEvidence["interactive_resize_elapsed_ms"] = $interactiveResizeElapsedMs
            $uiDisplayInputEvidence["interactive_resize_budget_ms"] = 2000
            $uiDisplayInputEvidence["interactive_resize_final_geometry_aligned"] = $true
            $uiDisplayInputEvidence["native_swapchain_final_extent_verified"] = $finalResizeSize
            $uiDisplayInputEvidence["native_swapchain_extent_sequence"] = $verifiedSwapchainSizes
            $uiDisplayInputEvidence["interactive_resize_swapchain_recreate_count"] = $resizeSwapchainSizes.Count
            $uiDisplayInputEvidence["interactive_resize_swapchain_sizes"] = $resizeSwapchainSizes

            # Minimize is a local HD composition state. The Player must reveal the already
            # presented gfxstream frame on restore without replacing/reparenting either crosvm
            # child or rebuilding the Vulkan swapchain.
            $minimizeLogBefore = Read-SharedTextFile $resizeCrosvmLog
            $minimizeSwapchainCountBefore = [regex]::Matches(
                $minimizeLogBefore,
                'Creating swapchain with size \d+x\d+\.').Count
            [void][HdRealGuestWindowNative]::ShowWindow($rootWindow, 6)
            $minimizeDeadline = [DateTimeOffset]::UtcNow.AddSeconds(5)
            while (-not [HdRealGuestWindowNative]::IsIconic($rootWindow) -and
                [DateTimeOffset]::UtcNow -lt $minimizeDeadline) {
                Start-Sleep -Milliseconds 25
            }
            if (-not [HdRealGuestWindowNative]::IsIconic($rootWindow)) {
                throw "Windows Player did not enter the minimized state"
            }
            Start-Sleep -Milliseconds 100
            [void][HdRealGuestWindowNative]::ShowWindow($rootWindow, 9)
            $minimizeRestoreProbe = [HdRealGuestWindowNative+DesktopFrameProbe]::new(
                $renderWindow,
                16)
            try {
                $minimizeRestoreProbe.Start()
                $minimizeRestoreDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
                do {
                    $minimizeChildrenRetained =
                        -not [HdRealGuestWindowNative]::IsIconic($rootWindow) -and
                        [HdRealGuestWindowNative]::IsWindowVisible($renderWindow) -and
                        [HdRealGuestWindowNative]::IsWindowVisible($inputWindow) -and
                        [HdRealGuestWindowNative]::GetParent($renderWindow) -eq $displayHostWindow -and
                        [HdRealGuestWindowNative]::GetParent($inputWindow) -eq $displayHostWindow
                    if (-not $minimizeChildrenRetained -or $minimizeRestoreProbe.Samples -lt 30) {
                        Start-Sleep -Milliseconds 16
                    }
                } while ((-not $minimizeChildrenRetained -or $minimizeRestoreProbe.Samples -lt 30) -and
                    [DateTimeOffset]::UtcNow -lt $minimizeRestoreDeadline)
            } finally {
                $minimizeRestoreProbe.Stop()
            }
            if (-not $minimizeChildrenRetained -or
                $minimizeRestoreProbe.Samples -lt 30 -or
                $minimizeRestoreProbe.CaptureFailures -ne 0 -or
                $minimizeRestoreProbe.NearBlackFrames -ne 0 -or
                $minimizeRestoreProbe.MaxConsecutiveNearBlackFrames -ne 0) {
                throw "minimize/restore did not synchronously reveal the retained Android frame"
            }
            Start-Sleep -Milliseconds 250
            $minimizeLogAfter = Read-SharedTextFile $resizeCrosvmLog
            $minimizeSwapchainCountAfter = [regex]::Matches(
                $minimizeLogAfter,
                'Creating swapchain with size \d+x\d+\.').Count
            if ($minimizeSwapchainCountAfter -ne $minimizeSwapchainCountBefore) {
                throw "minimize/restore rebuilt the gfxstream swapchain"
            }
            $uiDisplayInputEvidence["minimize_restore"] = [ordered]@{
                contract = "retained-session-frame-without-swapchain-rebuild"
                render_window_retained = $true
                input_window_retained = $true
                dwm_samples = $minimizeRestoreProbe.Samples
                dwm_near_black_frames = $minimizeRestoreProbe.NearBlackFrames
                dwm_capture_failures = $minimizeRestoreProbe.CaptureFailures
                swapchain_recreate_count = 0
            }

            if (-not [string]::IsNullOrWhiteSpace($GuestHwuiRenderer)) {
                $null = Invoke-AdbShell `
                    -Serial $actionSerial `
                    -Arguments @("setprop", "debug.hwui.renderer", $GuestHwuiRenderer)
                $null = Invoke-AdbShell `
                    -Serial $actionSerial `
                    -Arguments @("am", "force-stop", "com.android.settings")
            }
            $null = Invoke-AdbShell `
                -Serial $actionSerial `
                -Arguments @("am", "start", "-a", "android.settings.SETTINGS")
            Start-Sleep -Seconds 1
            $animationBefore = Assert-Ready $InstanceId
            $animationLogBefore = Read-SharedTextFile -Path $resizeCrosvmLog
            if ([Guid]$animationBefore.instance.active_run_id -ne [Guid]$actionReady.instance.active_run_id) {
                throw "pointer animation stress changed the active Android run before input"
            }
            $renderWidth = $renderRectAfterResize.Right - $renderRectAfterResize.Left
            $renderHeight = $renderRectAfterResize.Bottom - $renderRectAfterResize.Top
            # Keep the stress gesture off the later diagnostic click's center X. Linux evdev does
            # not repeat an unchanged ABS_MT_POSITION_X value, and the strict click gate must
            # observe both axes from its own gesture rather than inheriting the stress position.
            $dragX = [Math]::Max(1, [int]($renderWidth / 3))
            $dragTop = [Math]::Max(1, [int]($renderHeight / 4))
            $dragBottom = [Math]::Max($dragTop + 1, [int]($renderHeight * 3 / 4))
            $dragCount = 8
            $dragSteps = 32
            # 125 Hz (8 ms) is a common physical mouse report rate and is deliberately not an
            # integer divisor of a 60 Hz display. This catches cadence implementations that reset
            # their throttle origin on every accepted host sample and silently degrade to ~41 Hz.
            $pointerSampleIntervalMillis = 8
            $warmupDragCount = 2
            $animationPhysicalInputPrepared =
                [HdRealGuestWindowNative]::PrepareForPhysicalInput($renderWindow)
            if (-not $animationPhysicalInputPrepared) {
                throw "Windows 125 Hz pointer stress requires an unlocked interactive desktop"
            }
            for ($drag = 0; $drag -lt $warmupDragCount; $drag++) {
                $startY = if (($drag % 2) -eq 0) { $dragBottom } else { $dragTop }
                $endY = if (($drag % 2) -eq 0) { $dragTop } else { $dragBottom }
                if (-not [HdRealGuestWindowNative]::DragClientPoint(
                    $renderWindow,
                    $dragX,
                    $startY,
                    $dragX,
                    $endY,
                    $dragSteps,
                    $pointerSampleIntervalMillis)) {
                    throw "Windows could not warm the pointer-driven Android animation"
                }
            }
            Start-Sleep -Milliseconds 500
            $null = Invoke-AdbShell `
                -Serial $actionSerial `
                -Arguments @("dumpsys", "gfxinfo", "com.android.settings", "reset")
            # Exclude app cold-start/JIT and the explicit warm-up from both host and guest frame
            # statistics. The strict probe below retains its original cadence budget.
            $animationBefore = Assert-Ready $InstanceId
            $animationLogBefore = Read-SharedTextFile -Path $resizeCrosvmLog
            $animationLogcatCleared = $false
            try {
                $null = Invoke-AdbShell -Serial $actionSerial -Arguments @("logcat", "-c")
                $animationLogcatCleared = $true
            } catch { }
            $cadenceProbeProperty = "HD_GFXSTREAM_CADENCE_PROBE_V1"
            $cadenceProbeEpoch = [uint64](([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() % 2147483646) + 1)
            if (-not [HdRealGuestWindowNative]::SetWindowProperty(
                $renderWindow,
                $cadenceProbeProperty,
                $cadenceProbeEpoch)) {
                throw "Windows could not arm the gfxstream cadence probe"
            }
            try {
                for ($drag = 0; $drag -lt $dragCount; $drag++) {
                    $startY = if (($drag % 2) -eq 0) { $dragBottom } else { $dragTop }
                    $endY = if (($drag % 2) -eq 0) { $dragTop } else { $dragBottom }
                    if (-not [HdRealGuestWindowNative]::DragClientPoint(
                        $renderWindow,
                        $dragX,
                        $startY,
                        $dragX,
                        $endY,
                        $dragSteps,
                        $pointerSampleIntervalMillis)) {
                        throw "Windows could not post pointer animation stress through the gfxstream render child"
                    }
                }
                # Metrics are published at one-second boundaries on a successful present. Poll the
                # exact probe epoch instead of accepting a stale sample from before this gesture.
                $cadenceDeadline = [DateTimeOffset]::UtcNow.AddSeconds(5)
                do {
                    Start-Sleep -Milliseconds 100
                    $animationAfter = Assert-Ready $InstanceId
                    if ([uint64]$animationAfter.metrics.cadence_probe_epoch -eq $cadenceProbeEpoch -and
                        [long]$animationAfter.metrics.cadence_probe_frames -ge 20) {
                        break
                    }
                } while ([DateTimeOffset]::UtcNow -lt $cadenceDeadline)
            } finally {
                [HdRealGuestWindowNative]::RemoveWindowProperty(
                    $renderWindow,
                    $cadenceProbeProperty)
                [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
            }
            if ([Guid]$animationAfter.instance.active_run_id -ne [Guid]$actionReady.instance.active_run_id) {
                throw "pointer animation stress changed the active Android run after input"
            }
            if ([uint64]$animationAfter.metrics.cadence_probe_epoch -ne $cadenceProbeEpoch -or
                [long]$animationAfter.metrics.cadence_probe_frames -lt 20) {
                throw "pointer-driven Android animation did not publish a complete cadence probe"
            }
            $animationPresentedDelta = [long]$animationAfter.metrics.presented_frames -
                [long]$animationBefore.metrics.presented_frames
            $animationDroppedDelta = [long]$animationAfter.metrics.dropped_frames -
                [long]$animationBefore.metrics.dropped_frames
            $animationReadbackDelta = [long]$animationAfter.metrics.cpu_readback_bytes -
                [long]$animationBefore.metrics.cpu_readback_bytes
            $animationSoftwareBlitDelta = [long]$animationAfter.metrics.software_blit_count -
                [long]$animationBefore.metrics.software_blit_count
            $animationOver16Delta = [long]$animationAfter.metrics.host_present_over_16ms -
                [long]$animationBefore.metrics.host_present_over_16ms
            $animationOver33Delta = [long]$animationAfter.metrics.host_present_over_33ms -
                [long]$animationBefore.metrics.host_present_over_33ms
            $cadenceFrames = [long]$animationAfter.metrics.cadence_probe_frames
            $cadenceIntervals = [Math]::Max(1, $cadenceFrames - 1)
            $sourceCadenceIntervals =
                [long]$animationAfter.metrics.cadence_probe_source_intervals
            $cadenceAverageNs = [long]([long]$animationAfter.metrics.cadence_probe_interval_ns_total /
                $cadenceIntervals)
            $sourceCadenceAverageNs = [long]([long]$animationAfter.metrics.cadence_probe_source_interval_ns_total /
                [Math]::Max(1, $sourceCadenceIntervals))
            $maximumCadenceAverageNs = [long]20000000
            $maximumPostWorkerStageNs = [long]33333333
            $postWorkerQueueDelayNsMax =
                [long]$animationAfter.metrics.cadence_probe_post_worker_queue_delay_ns_max
            $postWorkerWorkNsMax =
                [long]$animationAfter.metrics.cadence_probe_post_worker_work_ns_max
            $presentMode = [string]$animationAfter.metrics.present_mode
            if ($presentMode -notin @("fifo", "mailbox", "immediate")) {
                throw "gfxstream did not publish a recognized Vulkan present mode"
            }
            $minimumCadenceHostAvailableMemoryBytes = [long](8GB)
            $maximumCadenceHostMemoryLoadPercent = 80
            $animationLogAfter = Read-SharedTextFile -Path $resizeCrosvmLog
            $animationLogTail = if ($animationLogAfter.StartsWith($animationLogBefore)) {
                $animationLogAfter.Substring($animationLogBefore.Length)
            } else {
                $animationLogAfter
            }
            $audioUnderrunErrorCount = [regex]::Matches(
                $animationLogTail,
                [regex]::Escape("Underrun. No new DescriptorChain while running")).Count
            $audioOverrunErrorCount = [regex]::Matches(
                $animationLogTail,
                [regex]::Escape("Overrun. No new DescriptorChain while running")).Count
            $pointerAnimationStress = [ordered]@{
                app = "android.settings.SETTINGS"
                input_path = "Win32-SendInput-capture-to-crosvm-EventDevice"
                drag_count = $dragCount
                steps_per_drag = $dragSteps
                host_sample_interval_millis = $pointerSampleIntervalMillis
                host_sample_rate_hz = [Math]::Round(1000.0 / $pointerSampleIntervalMillis, 1)
                warmup_drag_count = $warmupDragCount
                guest_gfxinfo_reset_after_warmup = $true
                guest_hwui_renderer = if ([string]::IsNullOrWhiteSpace($GuestHwuiRenderer)) {
                    "image-default"
                } else {
                    $GuestHwuiRenderer
                }
                presented_frames_delta = $animationPresentedDelta
                dropped_frames_delta = $animationDroppedDelta
                cpu_readback_bytes_delta = $animationReadbackDelta
                software_blit_count_delta = $animationSoftwareBlitDelta
                host_present_over_16ms_delta = $animationOver16Delta
                host_present_over_33ms_delta = $animationOver33Delta
                cadence_probe_epoch = $cadenceProbeEpoch
                cadence_probe_frames = $cadenceFrames
                cadence_average_interval_ns = $cadenceAverageNs
                maximum_cadence_average_interval_ns = $maximumCadenceAverageNs
                cadence_interval_ns_max = [long]$animationAfter.metrics.cadence_probe_interval_ns_max
                cadence_over_33ms = [long]$animationAfter.metrics.cadence_probe_over_33ms
                cadence_over_50ms = [long]$animationAfter.metrics.cadence_probe_over_50ms
                cadence_over_100ms = [long]$animationAfter.metrics.cadence_probe_over_100ms
                source_cadence_average_interval_ns = $sourceCadenceAverageNs
                source_cadence_intervals = $sourceCadenceIntervals
                expected_source_cadence_intervals = $cadenceIntervals
                maximum_source_cadence_average_interval_ns = $maximumCadenceAverageNs
                source_cadence_interval_ns_max = [long]$animationAfter.metrics.cadence_probe_source_interval_ns_max
                source_cadence_over_33ms = [long]$animationAfter.metrics.cadence_probe_source_over_33ms
                source_cadence_over_50ms = [long]$animationAfter.metrics.cadence_probe_source_over_50ms
                source_cadence_over_100ms = [long]$animationAfter.metrics.cadence_probe_source_over_100ms
                post_worker_queue_delay_ns_max = $postWorkerQueueDelayNsMax
                maximum_post_worker_queue_delay_ns = $maximumPostWorkerStageNs
                post_worker_queue_over_16ms = [long]$animationAfter.metrics.cadence_probe_post_worker_queue_over_16ms
                post_worker_queue_over_33ms = [long]$animationAfter.metrics.cadence_probe_post_worker_queue_over_33ms
                post_worker_work_ns_max = $postWorkerWorkNsMax
                maximum_post_worker_work_ns = $maximumPostWorkerStageNs
                post_worker_work_over_16ms = [long]$animationAfter.metrics.cadence_probe_post_worker_work_over_16ms
                post_worker_work_over_33ms = [long]$animationAfter.metrics.cadence_probe_post_worker_work_over_33ms
                audio_underrun_error_count = $audioUnderrunErrorCount
                audio_overrun_error_count = $audioOverrunErrorCount
                host_present_latency_ns_max = [long]$animationAfter.metrics.cadence_probe_host_present_latency_ns_max
                present_mode = $presentMode
                swapchain_recreate_count = [long]$animationAfter.metrics.cadence_probe_swapchain_recreate_count
                swapchain_recreate_failure_count = [long]$animationAfter.metrics.cadence_probe_swapchain_failure_count
                swapchain_out_of_date_count = [long]$animationAfter.metrics.cadence_probe_swapchain_out_of_date_count
                aspect_mismatch_count = [long]$animationAfter.metrics.cadence_probe_aspect_mismatch_count
                source_extent_change_count = [long]$animationAfter.metrics.cadence_probe_source_extent_change_count
                source_extent = "$($animationAfter.metrics.source_width)x$($animationAfter.metrics.source_height)"
                swapchain_extent = "$($animationAfter.metrics.swapchain_width)x$($animationAfter.metrics.swapchain_height)"
                host_available_memory_bytes = [long]$animationAfter.metrics.host_available_memory_bytes
                host_memory_load_percent = [int]$animationAfter.metrics.host_memory_load_percent
                minimum_host_available_memory_bytes = $minimumCadenceHostAvailableMemoryBytes
                maximum_host_memory_load_percent = $maximumCadenceHostMemoryLoadPercent
                guest_logcat_cleared_before_probe = $animationLogcatCleared
            }
            $uiDisplayInputEvidence["pointer_animation_stress"] = $pointerAnimationStress
            $animationGfxInfoPath = Join-Path $outputRoot "pointer-animation-gfxinfo.txt"
            try {
                $animationGfxInfo = Invoke-AdbShell `
                    -Serial $actionSerial `
                    -Arguments @("dumpsys", "gfxinfo", "com.android.settings", "framestats")
                [IO.File]::WriteAllText(
                    $animationGfxInfoPath,
                    "$animationGfxInfo`n",
                    [Text.UTF8Encoding]::new($false))
                $pointerAnimationStress["guest_gfxinfo_artifact"] = $animationGfxInfoPath
            } catch {
                $pointerAnimationStress["guest_gfxinfo_error"] = $_.Exception.Message
            }
            $animationLogcatPath = Join-Path $outputRoot "pointer-animation-logcat.txt"
            try {
                $animationLogcat = Invoke-AdbShell `
                    -Serial $actionSerial `
                    -Arguments @("logcat", "-d", "-v", "threadtime")
                [IO.File]::WriteAllText(
                    $animationLogcatPath,
                    "$animationLogcat`n",
                    [Text.UTF8Encoding]::new($false))
                $pointerAnimationStress["guest_logcat_artifact"] = $animationLogcatPath
                $pointerAnimationStress["guest_logcat_sha256"] =
                    (Get-FileHash -Algorithm SHA256 -LiteralPath $animationLogcatPath).Hash.ToLowerInvariant()
            } catch {
                $pointerAnimationStress["guest_logcat_error"] = $_.Exception.Message
            }
            if ($RunScreenRecording) {
                $pointerRecordingStarted = $false
                $pointerRecordingPath = $null
                $pointerRecordingPhysicalInputPrepared =
                    [HdRealGuestWindowNative]::PrepareForPhysicalInput($renderWindow)
                try {
                    if (-not $pointerRecordingPhysicalInputPrepared) {
                        throw "Windows pointer source recording requires an unlocked interactive desktop"
                    }
                    $pointerRecordingStatus = Invoke-Hdctl screen-record-start `
                        $InstanceId.ToString() --display primary --max-duration-seconds 10 |
                        ConvertFrom-Json
                    $pointerRecordingStarted = $true
                    for ($drag = 0; $drag -lt 4; $drag++) {
                        $startY = if (($drag % 2) -eq 0) { $dragBottom } else { $dragTop }
                        $endY = if (($drag % 2) -eq 0) { $dragTop } else { $dragBottom }
                        if (-not [HdRealGuestWindowNative]::DragClientPoint(
                            $renderWindow,
                            $dragX,
                            $startY,
                            $dragX,
                            $endY,
                            $dragSteps,
                            5)) {
                            throw "Windows could not exercise pointer input during source-frame diagnosis"
                        }
                    }
                    Start-Sleep -Seconds 1
                    $pointerRecording = Invoke-Hdctl screen-record-stop $InstanceId.ToString() |
                        ConvertFrom-Json
                    $pointerRecordingStarted = $false
                    $pointerRecordingPath = [IO.Path]::GetFullPath([string]$pointerRecording.path)
                    if (-not (Test-Path -LiteralPath $pointerRecordingPath -PathType Leaf) -or
                        (Get-Item -LiteralPath $pointerRecordingPath).Length -le 1024) {
                        throw "pointer source-frame recording did not produce a usable MP4"
                    }
                    $pointerRecordingArtifact = Join-Path $outputRoot "pointer-source-recording.mp4"
                    Copy-Item -LiteralPath $pointerRecordingPath -Destination $pointerRecordingArtifact
                    $pointerMp4 = Get-Mp4Metrics $pointerRecordingArtifact
                    $pointerExpectedDimensions =
                        "$($actionReady.instance.spec.display.width)x$($actionReady.instance.spec.display.height)"

                    $workerLogRoot = Join-Path $DataRoot "logs\workers"
                    $pointerRecordingId = ([Guid]$pointerRecording.id).ToString()
                    $pointerHostStatsLines = @()
                    $pointerHostStatsDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
                    do {
                        if (Test-Path -LiteralPath $workerLogRoot -PathType Container) {
                            $pointerHostStatsLines = @(Get-ChildItem -LiteralPath $workerLogRoot `
                                -File -Filter "$InstanceId.jsonl*" -ErrorAction SilentlyContinue |
                                ForEach-Object {
                                    Select-String -LiteralPath $_.FullName `
                                        -Pattern "worker.screen_recording.host_stats" -SimpleMatch |
                                        ForEach-Object Line
                                } | Where-Object { $_.Contains($pointerRecordingId) })
                        }
                        if ($pointerHostStatsLines.Count -eq 0) {
                            Start-Sleep -Milliseconds 100
                        }
                    } while ($pointerHostStatsLines.Count -eq 0 -and
                        [DateTimeOffset]::UtcNow -lt $pointerHostStatsDeadline)
                    if ($pointerHostStatsLines.Count -eq 0) {
                        throw "pointer source-frame recording did not publish gfxstream statistics"
                    }
                    $pointerHostStatsLine = $pointerHostStatsLines[-1]
                    $pointerEncodedMatch = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"encoded_frames"\s*:\s*(?<count>\d+)')
                    $pointerDroppedMatch = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"dropped_frames"\s*:\s*(?<count>\d+)')
                    $pointerNearBlackMatch = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"near_black_frames"\s*:\s*(?<count>\d+)')
                    $pointerConsecutiveBlackMatch = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"max_consecutive_near_black_frames"\s*:\s*(?<count>\d+)')
                    $pointerMaxGapMatch = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"max_source_frame_gap_millis"\s*:\s*(?<count>\d+)')
                    $pointerGapOver100Match = [Regex]::Match(
                        $pointerHostStatsLine,
                        '"source_frame_gaps_over_100_millis"\s*:\s*(?<count>\d+)')
                    if (-not $pointerEncodedMatch.Success -or
                        -not $pointerDroppedMatch.Success -or
                        -not $pointerNearBlackMatch.Success -or
                        -not $pointerConsecutiveBlackMatch.Success -or
                        -not $pointerMaxGapMatch.Success -or
                        -not $pointerGapOver100Match.Success) {
                        throw "pointer source-frame recording omitted encoder, black-frame or cadence evidence"
                    }
                    $pointerAnimationStress["source_recording"] = [ordered]@{
                        artifact = $pointerRecordingArtifact
                        input_path = "Win32-SendInput-capture-to-crosvm-EventDevice"
                        physical_input_prepared = $pointerRecordingPhysicalInputPrepared
                        sha256 = (Get-FileHash -Algorithm SHA256 `
                            -LiteralPath $pointerRecordingArtifact).Hash.ToLowerInvariant()
                        encoded_frames = [long]$pointerEncodedMatch.Groups["count"].Value
                        dropped_frames = [long]$pointerDroppedMatch.Groups["count"].Value
                        sample_count = [long]$pointerMp4.sample_count
                        dimensions = $pointerMp4.dimensions
                        near_black_frames =
                            [long]$pointerNearBlackMatch.Groups["count"].Value
                        max_consecutive_near_black_frames =
                            [long]$pointerConsecutiveBlackMatch.Groups["count"].Value
                        max_source_frame_gap_millis =
                            [long]$pointerMaxGapMatch.Groups["count"].Value
                        source_frame_gaps_over_100_millis =
                            [long]$pointerGapOver100Match.Groups["count"].Value
                    }
                    if ([long]$pointerEncodedMatch.Groups["count"].Value -lt 12 -or
                        [long]$pointerDroppedMatch.Groups["count"].Value -ne 0 -or
                        [long]$pointerMp4.sample_count -ne
                            [long]$pointerEncodedMatch.Groups["count"].Value -or
                        $pointerMp4.dimensions -notcontains $pointerExpectedDimensions -or
                        [long]$pointerNearBlackMatch.Groups["count"].Value -ne 0 -or
                        [long]$pointerConsecutiveBlackMatch.Groups["count"].Value -ne 0 -or
                        [long]$pointerMaxGapMatch.Groups["count"].Value -gt 100 -or
                        [long]$pointerGapOver100Match.Groups["count"].Value -ne 0) {
                        throw "gfxstream dropped, undersampled, blackened or stalled source frames during pointer interaction"
                    }
                } finally {
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                    if ($pointerRecordingStarted) {
                        try {
                            Invoke-Hdctl screen-record-stop $InstanceId.ToString() | Out-Null
                        } catch { }
                    }
                    if (-not [string]::IsNullOrWhiteSpace($pointerRecordingPath) -and
                        (Test-Path -LiteralPath $pointerRecordingPath -PathType Leaf)) {
                        [IO.File]::Delete($pointerRecordingPath)
                    }
                }
            }
            if ([long]$animationAfter.metrics.host_available_memory_bytes -lt
                    $minimumCadenceHostAvailableMemoryBytes -or
                [int]$animationAfter.metrics.host_memory_load_percent -gt
                    $maximumCadenceHostMemoryLoadPercent) {
                throw "host resource pressure invalidated the native present cadence sample"
            }
            if ($animationPresentedDelta -lt 12 -or $animationDroppedDelta -ne 0 -or
                $animationReadbackDelta -ne 0 -or $animationSoftwareBlitDelta -ne 0 -or
                $sourceCadenceIntervals -ne $cadenceIntervals -or
                $audioUnderrunErrorCount -ne 0 -or $audioOverrunErrorCount -ne 0 -or
                $animationOver16Delta -ne 0 -or $animationOver33Delta -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_host_present_latency_ns_max -gt 16000000 -or
                $cadenceAverageNs -gt $maximumCadenceAverageNs -or
                $sourceCadenceAverageNs -gt $maximumCadenceAverageNs -or
                [long]$animationAfter.metrics.cadence_probe_interval_ns_max -gt 50000000 -or
                [long]$animationAfter.metrics.cadence_probe_over_33ms -gt 2 -or
                [long]$animationAfter.metrics.cadence_probe_over_50ms -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_over_100ms -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_source_interval_ns_max -gt 50000000 -or
                [long]$animationAfter.metrics.cadence_probe_source_over_33ms -gt 2 -or
                [long]$animationAfter.metrics.cadence_probe_source_over_50ms -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_source_over_100ms -ne 0 -or
                $postWorkerQueueDelayNsMax -le 0 -or
                $postWorkerQueueDelayNsMax -gt $maximumPostWorkerStageNs -or
                [long]$animationAfter.metrics.cadence_probe_post_worker_queue_over_16ms -gt 2 -or
                [long]$animationAfter.metrics.cadence_probe_post_worker_queue_over_33ms -ne 0 -or
                $postWorkerWorkNsMax -le 0 -or
                $postWorkerWorkNsMax -gt $maximumPostWorkerStageNs -or
                [long]$animationAfter.metrics.cadence_probe_post_worker_work_over_16ms -gt 2 -or
                [long]$animationAfter.metrics.cadence_probe_post_worker_work_over_33ms -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_swapchain_recreate_count -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_swapchain_failure_count -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_swapchain_out_of_date_count -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_aspect_mismatch_count -ne 0 -or
                [long]$animationAfter.metrics.cadence_probe_source_extent_change_count -ne 0) {
                throw "pointer-driven Android animation violated the native present budget"
            }

            # The gfxstream recorder above proves that the source ColorBuffer stays valid, but it
            # cannot see a black frame introduced later by WebView2/DWM sibling composition. Run a
            # separate downsampled screen-DC probe after the cadence gate so its diagnostic readback
            # cannot influence the performance result. This is test-only and never enters HD's
            # zero-copy display path.
            $dwmProbePrepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput($renderWindow)
            $dwmProbeHitTest = [HdRealGuestWindowNative]::HitTestChainAtClientPoint(
                $renderWindow,
                [Math]::Max(1, [int]($renderWidth / 2)),
                [Math]::Max(1, [int]($renderHeight / 2)))
            $renderHandlePattern = "^0 hwnd=0x$($renderWindow.ToInt64().ToString('X'))(?:\s|$)"
            $dwmProbeUnoccluded = $dwmProbeHitTest -match $renderHandlePattern
            if ($dwmProbePrepared -and $dwmProbeUnoccluded) {
                $dwmProbe = $null
                try {
                    $dwmProbe = [HdRealGuestWindowNative+DesktopFrameProbe]::new(
                        $renderWindow,
                        16)
                    $dwmProbe.Start()
                    for ($drag = 0; $drag -lt 4; $drag++) {
                        $startY = if (($drag % 2) -eq 0) { $dragBottom } else { $dragTop }
                        $endY = if (($drag % 2) -eq 0) { $dragTop } else { $dragBottom }
                        if (-not [HdRealGuestWindowNative]::DragClientPoint(
                            $renderWindow,
                            $dragX,
                            $startY,
                            $dragX,
                            $endY,
                            $dragSteps,
                            $pointerSampleIntervalMillis)) {
                            throw "Windows could not exercise physical Win32 pointer input during DWM composition diagnosis"
                        }
                    }
                } finally {
                    if ($null -ne $dwmProbe) {
                        $dwmProbe.Stop()
                    }
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                }
                $dwmProbeEvidence = [ordered]@{
                    status = "measured"
                    contract = "final-desktop-composition-not-source-colorbuffer"
                    input_path = "Win32-SendInput-capture-to-crosvm-EventDevice"
                    sample_interval_millis = 16
                    samples = $dwmProbe.Samples
                    near_black_frames = $dwmProbe.NearBlackFrames
                    max_consecutive_near_black_frames = $dwmProbe.MaxConsecutiveNearBlackFrames
                    max_black_pixel_ratio_ppm = $dwmProbe.MaxBlackPixelRatioPpm
                    distinct_frames = $dwmProbe.DistinctFrames
                    frame_transitions = $dwmProbe.FrameTransitions
                    max_consecutive_identical_frames = $dwmProbe.MaxConsecutiveIdenticalFrames
                    maximum_consecutive_identical_frames = 8
                    capture_failures = $dwmProbe.CaptureFailures
                    hit_test = $dwmProbeHitTest
                }
                if ($dwmProbe.Samples -lt 20 -or $dwmProbe.CaptureFailures -ne 0 -or
                    $dwmProbe.NearBlackFrames -ne 0 -or
                    $dwmProbe.MaxConsecutiveNearBlackFrames -ne 0 -or
                    $dwmProbe.DistinctFrames -lt 4 -or $dwmProbe.FrameTransitions -lt 4 -or
                    $dwmProbe.MaxConsecutiveIdenticalFrames -gt 8) {
                    throw "Windows final DWM composition produced a black, frozen, or incomplete frame sequence during pointer interaction"
                }
            } else {
                [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                $dwmProbeEvidence = [ordered]@{
                    status = "failed-noninteractive-or-occluded-desktop"
                    contract = "final-desktop-composition-not-source-colorbuffer"
                    prepared = $dwmProbePrepared
                    unoccluded = $dwmProbeUnoccluded
                    hit_test = $dwmProbeHitTest
                }
            }
            $pointerAnimationStress["dwm_composition_probe"] = $dwmProbeEvidence
            if ($dwmProbeEvidence.status -ne "measured") {
                throw "Windows final DWM composition could not be measured on an interactive, unoccluded desktop"
            }

            # Repeatedly exercise the known left titlebar control while sampling the Android
            # rectangle from the composed desktop. This covers the user-visible WebView2/DWM
            # transition that source recording cannot observe and also proves that an overlay
            # sidebar does not resize/recreate the Vulkan swapchain or steal guest focus.
            $titlebarProbePrepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput(
                $titlebarWebView)
            if ($titlebarProbePrepared) {
                $titlebarDwmProbe = $null
                try {
                    $titlebarBoundsForProbe = [HdRealGuestWindowNative+Rect]::new()
                    if (-not [HdRealGuestWindowNative]::GetClientRect(
                        $titlebarWebView,
                        [ref]$titlebarBoundsForProbe)) {
                        throw "GetClientRect failed for titlebar DWM composition probe"
                    }
                    $titlebarProbeX = [Math]::Min(
                        18,
                        [Math]::Max(1, $titlebarBoundsForProbe.Right - 1))
                    $titlebarProbeY = [Math]::Min(
                        15,
                        [Math]::Max(1, $titlebarBoundsForProbe.Bottom - 1))
                    $titlebarFocusBefore = [HdRealGuestWindowNative]::FocusWindow()
                    $titlebarCompositionLogBefore = Read-SharedTextFile -Path $resizeCrosvmLog
                    $titlebarDwmProbe = [HdRealGuestWindowNative+DesktopFrameProbe]::new(
                        $renderWindow,
                        16)
                    $titlebarDwmProbe.Start()
                    for ($click = 0; $click -lt 6; $click++) {
                        if (-not [HdRealGuestWindowNative]::ClickClientPoint(
                            $titlebarWebView,
                            $titlebarProbeX,
                            $titlebarProbeY)) {
                            throw "Windows could not click the WebView sidebar control during DWM composition diagnosis"
                        }
                        Start-Sleep -Milliseconds 120
                    }
                    Start-Sleep -Milliseconds 250
                    $titlebarFocusAfter = [HdRealGuestWindowNative]::FocusWindow()
                } finally {
                    if ($null -ne $titlebarDwmProbe) {
                        $titlebarDwmProbe.Stop()
                    }
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($titlebarWebView)
                }
                $titlebarCompositionLogAfter = Read-SharedTextFile -Path $resizeCrosvmLog
                $titlebarCompositionLogTail = if ($titlebarCompositionLogAfter.StartsWith(
                    $titlebarCompositionLogBefore)) {
                    $titlebarCompositionLogAfter.Substring($titlebarCompositionLogBefore.Length)
                } else {
                    $titlebarCompositionLogAfter
                }
                $titlebarSwapchainRecreates = @(
                    [regex]::Matches(
                        $titlebarCompositionLogTail,
                        'Creating swapchain with size (\d+)x(\d+)\.') |
                        ForEach-Object { "$($_.Groups[1].Value)x$($_.Groups[2].Value)" }
                )
                $titlebarDwmEvidence = [ordered]@{
                    status = "measured"
                    contract = "webview-titlebar-overlay-with-stable-android-composition"
                    control = "toggle_sidebar"
                    click_count = 6
                    sample_interval_millis = 16
                    samples = $titlebarDwmProbe.Samples
                    near_black_frames = $titlebarDwmProbe.NearBlackFrames
                    max_consecutive_near_black_frames =
                        $titlebarDwmProbe.MaxConsecutiveNearBlackFrames
                    max_black_pixel_ratio_ppm = $titlebarDwmProbe.MaxBlackPixelRatioPpm
                    capture_failures = $titlebarDwmProbe.CaptureFailures
                    focus_before = "0x$($titlebarFocusBefore.ToInt64().ToString('X'))"
                    focus_after = "0x$($titlebarFocusAfter.ToInt64().ToString('X'))"
                    expected_guest_focus = "0x$($inputWindow.ToInt64().ToString('X'))"
                    focus_stable = $titlebarFocusBefore -eq $inputWindow -and
                        $titlebarFocusAfter -eq $inputWindow
                    swapchain_recreate_count = $titlebarSwapchainRecreates.Count
                    swapchain_sizes = $titlebarSwapchainRecreates
                }
                if ($titlebarDwmProbe.Samples -lt 30 -or
                    $titlebarDwmProbe.CaptureFailures -ne 0 -or
                    $titlebarDwmProbe.NearBlackFrames -ne 0 -or
                    $titlebarDwmProbe.MaxConsecutiveNearBlackFrames -ne 0 -or
                    $titlebarFocusBefore -ne $inputWindow -or
                    $titlebarFocusAfter -ne $inputWindow -or
                    $titlebarSwapchainRecreates.Count -ne 0) {
                    throw "WebView titlebar interaction flashed DWM, changed guest focus, or recreated the Android swapchain"
                }
            } else {
                [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($titlebarWebView)
                $titlebarDwmEvidence = [ordered]@{
                    status = "failed-noninteractive-desktop"
                    contract = "webview-titlebar-overlay-with-stable-android-composition"
                }
            }
            $pointerAnimationStress["titlebar_dwm_composition_probe"] = $titlebarDwmEvidence
            if ($titlebarDwmEvidence.status -ne "measured") {
                throw "WebView titlebar DWM composition could not be measured on an interactive desktop"
            }
            $inputEventsPath = Join-Path $outputRoot "titlebar-pointer-getevent.txt"
            $inputEventsErrorPath = Join-Path $outputRoot "titlebar-pointer-getevent.stderr.txt"
            $inputHitTestPath = Join-Path $outputRoot "titlebar-pointer-hit-test.txt"
            $getevent = Start-Process -FilePath $Adb -ArgumentList @(
                "-s", $actionSerial, "shell", "getevent", "-lt", "-c", "8",
                $primaryTouchDevicePath
            ) -WindowStyle Hidden -RedirectStandardOutput $inputEventsPath `
                -RedirectStandardError $inputEventsErrorPath -PassThru
            try {
                Start-Sleep -Milliseconds 750
                $renderRect = [HdRealGuestWindowNative+Rect]::new()
                if (-not [HdRealGuestWindowNative]::GetClientRect($renderWindow, [ref]$renderRect)) {
                    throw "GetClientRect failed for gfxstream render child"
                }
                $clickX = [Math]::Max(1, [int]($renderRect.Right - $renderRect.Left) / 2)
                $clickY = [Math]::Max(1, [int]($renderRect.Bottom - $renderRect.Top) / 2)
                $physicalInputPrepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput(
                    $renderWindow)
                try {
                    $inputHitTest = [HdRealGuestWindowNative]::HitTestChainAtClientPoint(
                        $renderWindow,
                        $clickX,
                        $clickY)
                    [IO.File]::WriteAllText(
                        $inputHitTestPath,
                        "prepare_for_physical_input=$physicalInputPrepared`n$inputHitTest`n",
                        [Text.UTF8Encoding]::new($false))
                    if (-not [HdRealGuestWindowNative]::ClickClientPoint(
                        $renderWindow,
                        $clickX,
                        $clickY)) {
                        throw "Windows SendInput could not click the gfxstream render child"
                    }
                } finally {
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                }
                if (-not $getevent.WaitForExit(8000)) {
                    Stop-Process -Id $getevent.Id -Force -ErrorAction SilentlyContinue
                    [void]$getevent.WaitForExit(2000)
                }
            } finally {
                if (-not $getevent.HasExited) {
                    Stop-Process -Id $getevent.Id -Force -ErrorAction SilentlyContinue
                    [void]$getevent.WaitForExit(2000)
                }
            }
            $inputEvents = if (Test-Path -LiteralPath $inputEventsPath) {
                Get-Content -Raw -LiteralPath $inputEventsPath
            } else { "" }
            $pointerDown = $inputEvents -match 'BTN_(?:TOUCH|LEFT)\s+DOWN'
            $pointerUp = $inputEvents -match 'BTN_(?:TOUCH|LEFT)\s+UP'
            $pointerX = $inputEvents -match '(?:ABS_(?:MT_)?POSITION_X|ABS_X|REL_X)'
            $pointerY = $inputEvents -match '(?:ABS_(?:MT_)?POSITION_Y|ABS_Y|REL_Y)'
            if (-not $pointerDown -or -not $pointerUp -or -not $pointerX -or -not $pointerY) {
                # Keep the physical SendInput assertion strict, but isolate a product routing
                # failure from a crosvm/Guest input-device failure in the same captured run.
                $directEventsPath = Join-Path $outputRoot "titlebar-render-direct-message-getevent.txt"
                $directEventsErrorPath = Join-Path $outputRoot "titlebar-render-direct-message-getevent.stderr.txt"
                $directGetevent = Start-Process -FilePath $Adb -ArgumentList @(
                    "-s", $actionSerial, "shell", "getevent", "-lt", "-c", "8",
                    $primaryTouchDevicePath
                ) -WindowStyle Hidden -RedirectStandardOutput $directEventsPath `
                    -RedirectStandardError $directEventsErrorPath -PassThru
                try {
                    Start-Sleep -Milliseconds 500
                    if (-not [HdRealGuestWindowNative]::PostClientClick(
                        $renderWindow,
                        $clickX,
                        $clickY)) {
                        throw "Windows could not post a diagnostic click to the gfxstream render child"
                    }
                    if (-not $directGetevent.WaitForExit(5000)) {
                        Stop-Process -Id $directGetevent.Id -Force -ErrorAction SilentlyContinue
                        [void]$directGetevent.WaitForExit(2000)
                    }
                } finally {
                    if (-not $directGetevent.HasExited) {
                        Stop-Process -Id $directGetevent.Id -Force -ErrorAction SilentlyContinue
                        [void]$directGetevent.WaitForExit(2000)
                    }
                }
                $directEvents = if (Test-Path -LiteralPath $directEventsPath) {
                    Get-Content -Raw -LiteralPath $directEventsPath
                } else { "" }
                $directComplete =
                    $directEvents -match 'BTN_(?:TOUCH|LEFT)\s+DOWN' -and
                    $directEvents -match 'BTN_(?:TOUCH|LEFT)\s+UP' -and
                    $directEvents -match '(?:ABS_(?:MT_)?POSITION_X|ABS_X|REL_X)' -and
                    $directEvents -match '(?:ABS_(?:MT_)?POSITION_Y|ABS_Y|REL_Y)'
                if ($directComplete) {
                    if (-not $physicalInputPrepared -and
                        $inputHitTest -match 'class=LockScreenBackstopFrame') {
                        throw "Windows physical pointer gate requires an unlocked interactive desktop; direct gfxstream render dispatch reached Android"
                    }
                    throw "Windows physical pointer routing did not reach Android; direct gfxstream render dispatch reached Android"
                }
                throw "Neither Windows physical pointer routing nor direct gfxstream render dispatch reached Android"
            }

            $postUiReady = Assert-Ready $InstanceId
            if ([Guid]$postUiReady.instance.active_run_id -ne [Guid]$actionReady.instance.active_run_id -or
                $postUiReady.instance.status.observed -ne "ready") {
                throw "WebView titlebar display/input test did not preserve the Ready Android run"
            }

            $firstUiPid = $uiProcess.Id
            $originalRenderWindow = $renderWindow
            $originalInputWindow = [IntPtr]::new([long]$primaryAttached.raw_handle)
            $pointerFocusFailures = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_POINTER_FOCUS_FAILURES_V1")
            $pointerFocusMaxMicros = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_POINTER_FOCUS_MAX_MICROS_V1")
            $cachedInputWindow = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_CROSVM_INPUT_CHILD_V1")
            $cachedRenderWindow = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_GFXSTREAM_RENDER_CHILD_V1")
            $pointerForwardFailures = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_POINTER_FORWARD_FAILURES_V1")
            $pointerForwardMaxMicros = [HdRealGuestWindowNative]::WindowProperty(
                $displayHostWindow,
                "HD_POINTER_FORWARD_MAX_MICROS_V1")
            if ($pointerFocusFailures -ne 0 -or
                $pointerFocusMaxMicros -eq 0 -or
                $pointerFocusMaxMicros -gt 20000) {
                throw "native Android pointer focus restoration failed or exceeded the 20 ms UI-thread budget"
            }
            if ($pointerForwardFailures -ne 0 -or
                $pointerForwardMaxMicros -eq 0 -or
                $pointerForwardMaxMicros -gt 20000) {
                throw "native Android pointer handoff dropped messages or exceeded the 20 ms UI-thread budget"
            }
            if ($cachedInputWindow -ne $inputWindow.ToInt64()) {
                throw "native Android pointer forwarding did not retain the verified crosvm input HWND"
            }
            if ($cachedRenderWindow -ne $renderWindow.ToInt64()) {
                throw "interactive resize did not retain the verified gfxstream render HWND"
            }
            $titlebarBounds = [HdRealGuestWindowNative+Rect]::new()
            if (-not [HdRealGuestWindowNative]::GetClientRect(
                $titlebarWebView,
                [ref]$titlebarBounds)) {
                throw "GetClientRect failed for the WebView titlebar"
            }
            $titlebarWidth = $titlebarBounds.Right - $titlebarBounds.Left
            $titlebarHeight = $titlebarBounds.Bottom - $titlebarBounds.Top
            $titlebarDpi = [HdRealGuestWindowNative]::GetDpiForWindow($rootWindow)
            $expectedTitlebarHeight = [Math]::Max(
                1,
                [int][Math]::Round(30.0 * $titlebarDpi / 96.0))
            if ([Math]::Abs($titlebarHeight - $expectedTitlebarHeight) -gt 1) {
                throw "WebView titlebar physical height did not match the root-window DPI"
            }
            $closeX = $titlebarWidth - 21
            $closeY = [int][Math]::Floor($titlebarHeight / 2)
            $titlebarPhysicalInputPrepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput(
                $titlebarWebView)
            try {
                if (-not $titlebarPhysicalInputPrepared -or
                    -not [HdRealGuestWindowNative]::ClickClientPoint(
                        $titlebarWebView,
                        $closeX,
                        $closeY)) {
                    throw "Windows SendInput could not click the WebView titlebar close button"
                }
            } finally {
                [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($titlebarWebView)
            }
            if (-not $uiProcess.WaitForExit(20000)) {
                throw "WebView titlebar close button did not close the isolated HD UI within 20 seconds"
            }
            $firstUiExitCode = $uiProcess.ExitCode
            $postCloseReady = Assert-Ready $InstanceId
            if ([Guid]$postCloseReady.instance.active_run_id -ne [Guid]$actionReady.instance.active_run_id) {
                throw "closing Windows Player changed the active Android run"
            }
            $parkingDeadline = [DateTimeOffset]::UtcNow.AddSeconds(20)
            $parkingWindow = [IntPtr]::Zero
            $parkedInputParent = [IntPtr]::Zero
            do {
                $parkingWindow = [HdRealGuestWindowNative]::GetParent($originalRenderWindow)
                $parkedInputParent = [HdRealGuestWindowNative]::GetParent($originalInputWindow)
                if ($parkingWindow -ne [IntPtr]::Zero -and
                    $parkedInputParent -eq $parkingWindow -and
                    [HdRealGuestWindowNative]::WindowText($parkingWindow) -eq "HD crosvm display parking" -and
                    -not [HdRealGuestWindowNative]::IsWindowVisible($originalRenderWindow) -and
                    -not [HdRealGuestWindowNative]::IsWindowVisible($originalInputWindow)) {
                    break
                }
                Start-Sleep -Milliseconds 100
            } while ([DateTimeOffset]::UtcNow -lt $parkingDeadline)
            if ($parkingWindow -eq [IntPtr]::Zero -or
                $parkedInputParent -ne $parkingWindow -or
                [HdRealGuestWindowNative]::WindowText($parkingWindow) -ne "HD crosvm display parking" -or
                [HdRealGuestWindowNative]::IsWindowVisible($originalRenderWindow) -or
                [HdRealGuestWindowNative]::IsWindowVisible($originalInputWindow)) {
                throw "closing Windows Player did not park both gfxstream render and crosvm input windows"
            }
            if (@(Get-VisibleTopLevelWindows $crosvmProcessIds).Count -ne 0) {
                throw "closing Windows Player exposed a standalone crosvm/gfxstream window"
            }

            $reopenStdout = Join-Path $outputRoot "hd-ui-reopen.stdout.txt"
            $reopenStderr = Join-Path $outputRoot "hd-ui-reopen.stderr.txt"
            $uiProcess = Start-Process `
                -FilePath $uiExecutable `
                -ArgumentList @("--data-root", $DataRoot, "--web-root", $webRoot) `
                -WorkingDirectory $uiRuntimeRoot `
                -RedirectStandardOutput $reopenStdout `
                -RedirectStandardError $reopenStderr `
                -PassThru
            $reopenedRoot = [IntPtr]::Zero
            $reopenedTitlebarWebView = [IntPtr]::Zero
            $reopenedRender = [IntPtr]::Zero
            $reopenDeadline = [DateTimeOffset]::UtcNow.AddSeconds(40)
            do {
                if ($uiProcess.HasExited) {
                    $reopenError = if (Test-Path -LiteralPath $reopenStderr) {
                        Get-Content -Raw -LiteralPath $reopenStderr
                    } else { "" }
                    throw "reopened HD UI exited before reattaching its native display: $reopenError"
                }
                $reopenedRoot = [HdRealGuestWindowNative]::FindVisibleTopLevelWindow(
                    [uint32]$uiProcess.Id,
                    "HD Android")
                if ($reopenedRoot -ne [IntPtr]::Zero) {
                    $reopenedLegacyTitlebar = [HdRealGuestWindowNative]::FindWindowExW(
                        $reopenedRoot,
                        [IntPtr]::Zero,
                        "HD_NATIVE_TITLEBAR_V1",
                        $null)
                    if ($reopenedLegacyTitlebar -ne [IntPtr]::Zero) {
                        throw "reopened Windows Player created the removed native titlebar layer"
                    }
                    $reopenedTitlebarWebView = [HdRealGuestWindowNative]::FindVisibleTopWebView(
                        $reopenedRoot,
                        30)
                    $reopenedRenderCandidate = [HdRealGuestWindowNative]::Descendants($reopenedRoot) |
                        Where-Object {
                            [HdRealGuestWindowNative]::WindowClass($_) -in @("subWin", "vulkan-subWin") -and
                            [HdRealGuestWindowNative]::IsWindowVisible($_)
                        } |
                        Select-Object -First 1
                    $reopenedRender = if ($null -eq $reopenedRenderCandidate) {
                        [IntPtr]::Zero
                    } else {
                        [IntPtr]::new($reopenedRenderCandidate.ToInt64())
                    }
                }
                if ($reopenedTitlebarWebView -ne [IntPtr]::Zero -and
                    $reopenedRender -ne [IntPtr]::Zero) {
                    break
                }
                Start-Sleep -Milliseconds 100
            } while ([DateTimeOffset]::UtcNow -lt $reopenDeadline)
            if ($reopenedRoot -eq [IntPtr]::Zero -or
                $reopenedTitlebarWebView -eq [IntPtr]::Zero -or
                $reopenedRender -eq [IntPtr]::Zero) {
                throw "reopened HD UI did not publish its WebView titlebar and gfxstream render child"
            }
            $reopenedInput = Wait-HdAttachedScanout $reopenedRoot $crosvmProcessIds 0
            if ($reopenedRender -ne $originalRenderWindow -or
                [long]$reopenedInput.raw_handle -ne $originalInputWindow.ToInt64() -or
                [HdRealGuestWindowNative]::GetParent($reopenedRender) -eq $parkingWindow -or
                [HdRealGuestWindowNative]::GetParent($originalInputWindow) -eq $parkingWindow) {
                throw "reopened Windows Player did not reattach the original render/input sibling pair"
            }
            $reopenedDisplayHostWindow = [HdRealGuestWindowNative]::GetParent($reopenedRender)
            $reopenedRenderBounds = [HdRealGuestWindowNative+Rect]::new()
            if (-not [HdRealGuestWindowNative]::GetClientRect(
                $reopenedRender,
                [ref]$reopenedRenderBounds)) {
                throw "GetClientRect failed for the reopened gfxstream render child"
            }
            $reopenedPhysicalInputPrepared = [HdRealGuestWindowNative]::PrepareForPhysicalInput(
                $reopenedRender)
            try {
                if (-not $reopenedPhysicalInputPrepared -or
                    -not [HdRealGuestWindowNative]::ClickClientPoint(
                        $reopenedRender,
                        [Math]::Max(1, [int](($reopenedRenderBounds.Right - $reopenedRenderBounds.Left) / 2)),
                        [Math]::Max(1, [int](($reopenedRenderBounds.Bottom - $reopenedRenderBounds.Top) / 2)))) {
                    throw "Windows SendInput could not exercise native pointer focus after Player reattachment"
                }
            } finally {
                [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($reopenedRender)
            }
            $reopenedPointerFocusFailures = [HdRealGuestWindowNative]::WindowProperty(
                $reopenedDisplayHostWindow,
                "HD_POINTER_FOCUS_FAILURES_V1")
            $reopenedPointerFocusMaxMicros = [HdRealGuestWindowNative]::WindowProperty(
                $reopenedDisplayHostWindow,
                "HD_POINTER_FOCUS_MAX_MICROS_V1")
            $reopenedCachedInputWindow = [HdRealGuestWindowNative]::WindowProperty(
                $reopenedDisplayHostWindow,
                "HD_CROSVM_INPUT_CHILD_V1")
            $reopenedPointerForwardFailures = [HdRealGuestWindowNative]::WindowProperty(
                $reopenedDisplayHostWindow,
                "HD_POINTER_FORWARD_FAILURES_V1")
            $reopenedPointerForwardMaxMicros = [HdRealGuestWindowNative]::WindowProperty(
                $reopenedDisplayHostWindow,
                "HD_POINTER_FORWARD_MAX_MICROS_V1")
            if ($reopenedPointerFocusFailures -ne 0 -or
                $reopenedPointerFocusMaxMicros -eq 0 -or
                $reopenedPointerFocusMaxMicros -gt 20000) {
                throw "reopened Windows Player pointer focus restoration failed or exceeded the 20 ms UI-thread budget"
            }
            if ($reopenedPointerForwardFailures -ne 0 -or
                $reopenedPointerForwardMaxMicros -eq 0 -or
                $reopenedPointerForwardMaxMicros -gt 20000) {
                throw "reopened Windows Player pointer handoff failed or exceeded the 20 ms UI-thread budget"
            }
            if ($reopenedCachedInputWindow -ne $originalInputWindow.ToInt64()) {
                throw "reopened Windows Player did not cache the reattached crosvm input HWND"
            }
            $postReopenReady = Assert-Ready $InstanceId
            if ([Guid]$postReopenReady.instance.active_run_id -ne [Guid]$actionReady.instance.active_run_id) {
                throw "reopening Windows Player changed the active Android run"
            }
            $uiDisplayInputFinalEvidence = [ordered]@{
                contract = "compact-webview-titlebar-and-real-pointer-input"
                ui_pid = $firstUiPid
                root_window = "0x$($rootWindow.ToInt64().ToString('X'))"
                titlebar_webview = "0x$($titlebarWebView.ToInt64().ToString('X'))"
                titlebar_dpi = $titlebarDpi
                titlebar_width_px = $titlebarWidth
                titlebar_height_px = $titlebarHeight
                expected_titlebar_height_px = $expectedTitlebarHeight
                legacy_native_titlebar_absent = $true
                visible_webview_count = $visibleWebViews.Count
                hidden_body_webviews_excluded = $true
                window_tree_artifact = $uiWindowTreePath
                window_tree_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $uiWindowTreePath).Hash.ToLowerInvariant()
                interactive_resize_steps = $interactiveResizeSteps * 2
                interactive_resize_elapsed_ms = $interactiveResizeElapsedMs
                interactive_resize_budget_ms = 2000
                interactive_resize_final_geometry_aligned = $true
                pointer_animation_stress = $pointerAnimationStress
                native_pointer_focus_failure_count = $pointerFocusFailures
                native_pointer_focus_max_micros = $pointerFocusMaxMicros
                native_pointer_focus_budget_micros = 20000
                native_pointer_cached_input_window = "0x$($cachedInputWindow.ToString('X'))"
                native_resize_cached_render_window = "0x$($cachedRenderWindow.ToString('X'))"
                native_pointer_forward_failure_count = $pointerForwardFailures
                native_pointer_forward_max_micros = $pointerForwardMaxMicros
                native_pointer_forward_budget_micros = 20000
                render_window = "0x$($renderWindow.ToInt64().ToString('X'))"
                titlebar_display_selector_absent = $true
                primary_attached = $primaryAttached
                primary_input = [ordered]@{
                    method = "Win32-SendInput"
                    client_x = $clickX
                    client_y = $clickY
                    down = $pointerDown
                    up = $pointerUp
                    x_axis = $pointerX
                    y_axis = $pointerY
                    getevent_artifact = $inputEventsPath
                    getevent_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $inputEventsPath).Hash.ToLowerInvariant()
                    capabilities_artifact = $inputCapabilitiesPath
                    capabilities_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $inputCapabilitiesPath).Hash.ToLowerInvariant()
                }
                ready_run_preserved = $true
                webview_close_exited = $true
                webview_close_method = "Win32-SendInput"
                webview_close_client_x = $closeX
                webview_close_client_y = $closeY
                ui_exit_code = $firstUiExitCode
                parking_window = "0x$($parkingWindow.ToInt64().ToString('X'))"
                render_and_input_parked = $true
                standalone_backend_windows_after_close = 0
                reopened_ui_pid = $uiProcess.Id
                reopened_root_window = "0x$($reopenedRoot.ToInt64().ToString('X'))"
                reopened_titlebar_webview = "0x$($reopenedTitlebarWebView.ToInt64().ToString('X'))"
                reopened_render_window = "0x$($reopenedRender.ToInt64().ToString('X'))"
                reopened_input = $reopenedInput
                reopened_native_pointer_focus_failure_count = $reopenedPointerFocusFailures
                reopened_native_pointer_focus_max_micros = $reopenedPointerFocusMaxMicros
                reopened_native_pointer_cached_input_window = "0x$($reopenedCachedInputWindow.ToString('X'))"
                reopened_native_pointer_forward_failure_count = $reopenedPointerForwardFailures
                reopened_native_pointer_forward_max_micros = $reopenedPointerForwardMaxMicros
                original_sibling_handles_preserved = $true
                reopen_ready_run_preserved = $true
                reopened_left_attached = $true
            }
            foreach ($entry in $uiDisplayInputFinalEvidence.GetEnumerator()) {
                $uiDisplayInputEvidence[$entry.Key] = $entry.Value
            }
            $actions.Add("compact-webview-titlebar-pointer-close-reopen")
            Add-ActionReadback titlebar_display_input `
                "primary-render;Win32-SendInput;close;reopen" `
                "titlebar-display-selector=absent;Android-getevent=down+up+x+y;parked-and-reattached=true;ready-run-preserved=true"
        }
        foreach ($key in @("home", "recent", "back", "volume-up", "volume-down")) {
            Invoke-Action key $key
        }
        Invoke-Action key power
        Start-Sleep -Milliseconds 500
        Invoke-Action key power
        Start-Sleep -Milliseconds 500
        $power = Invoke-AdbShell $actionSerial dumpsys power
        if ($power -notmatch "mWakefulness=Awake|Wakefulness: Awake") {
            throw "power button round trip did not restore Android to Awake"
        }
        Add-ActionReadback navigation_keys "home,recent,back,volume-up,volume-down,power" "framework-and-power-readback"
        $displaySelectionReady = Assert-Ready $InstanceId
        if ([Guid]$displaySelectionReady.instance.active_run_id -ne
            [Guid]$actionReady.instance.active_run_id) {
            throw "navigation/render audit changed the active Android run"
        }
        $displaySelectionEvidence = Assert-NoUnmappedDisplayFrames `
            $displaySelectionReady.instance `
            $displaySelectionReady.metrics

        if ($RunLocationProbe) {
            $locationProbeInstalled = $false
            try {
                $installOutput = & $Adb -s $actionSerial install -r -t $LocationProbeApk 2>&1
                if ($LASTEXITCODE -ne 0 -or ($installOutput -join "`n") -notmatch "Success") {
                    throw "location probe APK installation failed: $($installOutput -join [Environment]::NewLine)"
                }
                $locationProbeInstalled = $true
                Invoke-AdbShell $actionSerial pm grant com.hd.locationprobe android.permission.ACCESS_COARSE_LOCATION | Out-Null
                Invoke-AdbShell $actionSerial pm grant com.hd.locationprobe android.permission.ACCESS_FINE_LOCATION | Out-Null
                Invoke-AdbShell $actionSerial cmd location set-location-enabled true | Out-Null
                Invoke-AdbShell $actionSerial am force-stop com.hd.locationprobe | Out-Null
                Invoke-AdbShell $actionSerial run-as com.hd.locationprobe rm -f files/location.txt | Out-Null
                Invoke-AdbShell $actionSerial input keyevent KEYCODE_WAKEUP | Out-Null
                Invoke-AdbShell $actionSerial wm dismiss-keyguard | Out-Null
                Invoke-AdbShell -Serial $actionSerial -Arguments @(
                    "am", "start", "-W",
                    "-n", "com.hd.locationprobe/.LocationProbeActivity",
                    "--es", "expected_latitude", "31.2304000",
                    "--es", "expected_longitude", "121.4737000",
                    "--es", "expected_altitude", "4.000",
                    "--es", "expected_accuracy", "3.000"
                ) | Out-Null

                Invoke-Action location 312304000 1214737000 --altitude-mm 4000 --accuracy-mm 3000
                $locationProbeReadback = ""
                $locationProbeMatched = $false
                for ($locationProbeAttempt = 1; $locationProbeAttempt -le 80; $locationProbeAttempt++) {
                    try {
                        $locationProbeReadback = Invoke-AdbShell $actionSerial run-as com.hd.locationprobe cat files/location.txt
                    } catch {
                        $locationProbeReadback = ""
                    }
                    if ($locationProbeReadback -match "status=match" -and
                        $locationProbeReadback -match "provider=gps" -and
                        $locationProbeReadback -match "latitude=31\.2304000(?:\s|$)" -and
                        $locationProbeReadback -match "longitude=121\.4737000(?:\s|$)" -and
                        $locationProbeReadback -match "altitude=4\.000(?:\s|$)" -and
                        $locationProbeReadback -match "accuracy=3\.000(?:\s|$)" -and
                        $locationProbeReadback -match "has_altitude=true" -and
                        $locationProbeReadback -match "has_accuracy=true") {
                        $locationProbeMatched = $true
                        break
                    }
                    Start-Sleep -Milliseconds 250
                }
                $locationProbeReadbackPath = Join-Path $outputRoot "location-framework-callback.txt"
                [IO.File]::WriteAllText(
                    $locationProbeReadbackPath,
                    "$locationProbeReadback`n",
                    [Text.UTF8Encoding]::new($false))
                if (-not $locationProbeMatched) {
                    $locationDump = Invoke-AdbShell $actionSerial dumpsys location
                    [IO.File]::WriteAllText(
                        (Join-Path $outputRoot "location-framework-dumpsys.txt"),
                        "$locationDump`n",
                        [Text.UTF8Encoding]::new($false))
                    $activityDump = Invoke-AdbShell $actionSerial dumpsys activity activities
                    [IO.File]::WriteAllText(
                        (Join-Path $outputRoot "location-probe-activity.txt"),
                        "$activityDump`n",
                        [Text.UTF8Encoding]::new($false))
                    $locationLog = Invoke-AdbShell -Serial $actionSerial -Arguments @(
                        "logcat", "-d", "-v", "threadtime",
                        "-s", "HDLocationProbe:V", "GnssLocationProvider:V", "GnssManager:V", "*:S"
                    )
                    [IO.File]::WriteAllText(
                        (Join-Path $outputRoot "location-probe-logcat.txt"),
                        "$locationLog`n",
                        [Text.UTF8Encoding]::new($false))
                    throw "real GPS LocationManager callback did not preserve all controlled fields: $locationProbeReadback"
                }
                $locationDeliveryPath = Get-ChildItem `
                    -LiteralPath (Join-Path $DataRoot "runs\$InstanceId") `
                    -Recurse -File -Filter "location-delivery-v1.json" |
                    Select-Object -First 1 -ExpandProperty FullName
                if ([string]::IsNullOrWhiteSpace($locationDeliveryPath)) {
                    throw "fixed-location component did not publish delivery evidence"
                }
                $locationDelivery = Get-Content -LiteralPath $locationDeliveryPath -Raw | ConvertFrom-Json
                if ([long]$locationDelivery.delivered_sequence -lt 2) {
                    throw "fixed-location component did not deliver both the initial and controlled Guest samples"
                }
                $locationDeliveryArtifact = Join-Path $outputRoot "location-delivery-v1.json"
                Copy-Item -LiteralPath $locationDeliveryPath -Destination $locationDeliveryArtifact
                $locationProbeEvidence = [ordered]@{
                    package = "com.hd.locationprobe"
                    apk_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $LocationProbeApk).Hash.ToLowerInvariant()
                    provider = "gps"
                    latitude = 31.2304000
                    longitude = 121.4737000
                    altitude = 4.000
                    accuracy = 3.000
                    attempts = $locationProbeAttempt
                    delivered_sequence = [long]$locationDelivery.delivered_sequence
                    mock_provider_used = $false
                    artifact = $locationProbeReadbackPath
                    delivery_artifact = $locationDeliveryArtifact
                    package_uninstalled = $false
                }
                $actions.Add("location-framework-probe")
                Add-ActionReadback location `
                    "lat=31.2304000,lon=121.4737000,alt=4.000,accuracy=3.000" `
                    "LocationManager GPS callback exact-match;mock-provider=false"
            } finally {
                if ($locationProbeInstalled) {
                    try { Invoke-AdbShell $actionSerial am force-stop com.hd.locationprobe | Out-Null } catch { }
                    $uninstallOutput = & $Adb -s $actionSerial uninstall com.hd.locationprobe 2>&1
                    if ($LASTEXITCODE -ne 0 -and $null -eq $failure) {
                        throw "location probe APK uninstall failed: $($uninstallOutput -join [Environment]::NewLine)"
                    }
                    if ($null -ne $locationProbeEvidence) {
                        $locationProbeEvidence["package_uninstalled"] = $true
                    }
                }
            }
        } else {
            Invoke-Action location 312304000 1214737000 --altitude-mm 4000 --accuracy-mm 3000
            $location = Invoke-AdbShell $actionSerial dumpsys location
            Add-ActionReadback location "lat=31.2304000,lon=121.4737000,alt=4.000,accuracy=3.000" "typed-fixed-location-action-accepted;dumpsys-bytes=$([Text.Encoding]::UTF8.GetByteCount($location));framework-callback-not-asserted"
        }

        if ($RunLocationRoute) {
            $routePath = Join-Path $outputRoot "location-route.kml"
            [IO.File]::WriteAllText(
                $routePath,
                '<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Placemark><LineString><coordinates>-122.0840577,37.4219999,5 -122.0839000,37.4221000,6 -122.0837000,37.4223000,7</coordinates></LineString></Placemark></Document></kml>',
                [Text.UTF8Encoding]::new($false))
            $locationDelivery = Get-Content -LiteralPath $locationDeliveryPath -Raw | ConvertFrom-Json
            $routeDeliveriesBefore = [long]$locationDelivery.delivered_sequence
            if ($routeDeliveriesBefore -lt 2) {
                throw "location route requires the verified fixed-location delivery baseline"
            }

            $routeProbeInstalled = $false
            $routeStarted = $false
            try {
                $routeInstallOutput = & $Adb -s $actionSerial install -r -t $LocationProbeApk 2>&1
                if ($LASTEXITCODE -ne 0 -or ($routeInstallOutput -join "`n") -notmatch "Success") {
                    throw "location route probe APK installation failed: $($routeInstallOutput -join [Environment]::NewLine)"
                }
                $routeProbeInstalled = $true
                Invoke-AdbShell $actionSerial pm grant com.hd.locationprobe android.permission.ACCESS_COARSE_LOCATION | Out-Null
                Invoke-AdbShell $actionSerial pm grant com.hd.locationprobe android.permission.ACCESS_FINE_LOCATION | Out-Null
                Invoke-AdbShell $actionSerial am force-stop com.hd.locationprobe | Out-Null
                Invoke-AdbShell $actionSerial run-as com.hd.locationprobe rm -f files/location.txt | Out-Null
                Invoke-AdbShell $actionSerial input keyevent KEYCODE_WAKEUP | Out-Null
                Invoke-AdbShell $actionSerial wm dismiss-keyguard | Out-Null
                Invoke-AdbShell -Serial $actionSerial -Arguments @(
                    "am", "start", "-W",
                    "-n", "com.hd.locationprobe/.LocationProbeActivity",
                    "--es", "expected_latitude", "37.4223000",
                    "--es", "expected_longitude", "-122.0837000",
                    "--es", "expected_altitude", "7.000",
                    "--es", "expected_accuracy", "5.000"
                ) | Out-Null

                Invoke-Action route-start $routePath --interval-ms 500 --repeat
                $routeStarted = $true
                Start-Sleep -Milliseconds 700
                $routePlaying = Get-Instance $InstanceId
                if ($null -eq $routePlaying.location_route -or
                    $routePlaying.location_route.state -ne "playing" -or
                    [int]$routePlaying.location_route.point_count -ne 3) {
                    throw "location route did not enter the expected playing state"
                }
                Invoke-Action route-pause
                $routePaused = Get-Instance $InstanceId
                if ($null -eq $routePaused.location_route -or
                    $routePaused.location_route.state -ne "paused") {
                    throw "location route did not enter the paused state"
                }
                $pausedPoint = [int]$routePaused.location_route.current_point
                Start-Sleep -Milliseconds 700
                $routePausedStable = Get-Instance $InstanceId
                if ($null -eq $routePausedStable.location_route -or
                    $routePausedStable.location_route.state -ne "paused" -or
                    [int]$routePausedStable.location_route.current_point -ne $pausedPoint) {
                    throw "location route advanced while paused"
                }
                Invoke-Action route-resume

                $routeProbeReadback = ""
                $routeProbeMatched = $false
                $routeDeliveriesAfter = $routeDeliveriesBefore
                for ($routeProbeAttempt = 1; $routeProbeAttempt -le 80; $routeProbeAttempt++) {
                    try {
                        $routeProbeReadback = Invoke-AdbShell $actionSerial run-as com.hd.locationprobe cat files/location.txt
                    } catch {
                        $routeProbeReadback = ""
                    }
                    try {
                        $locationDelivery = Get-Content -LiteralPath $locationDeliveryPath -Raw | ConvertFrom-Json
                        $routeDeliveriesAfter = [long]$locationDelivery.delivered_sequence
                    } catch { }
                    if ($routeProbeReadback -match "status=match" -and
                        $routeProbeReadback -match "provider=gps" -and
                        $routeProbeReadback -match "latitude=37\.4223000(?:\s|$)" -and
                        $routeProbeReadback -match "longitude=-122\.0837000(?:\s|$)" -and
                        $routeProbeReadback -match "altitude=7\.000(?:\s|$)" -and
                        $routeProbeReadback -match "accuracy=5\.000(?:\s|$)" -and
                        $routeProbeReadback -match "has_altitude=true" -and
                        $routeProbeReadback -match "has_accuracy=true" -and
                        $routeDeliveriesAfter -gt $routeDeliveriesBefore) {
                        $routeProbeMatched = $true
                        break
                    }
                    Start-Sleep -Milliseconds 250
                }
                $routeProbeReadbackPath = Join-Path $outputRoot "location-route-framework-callback.txt"
                [IO.File]::WriteAllText(
                    $routeProbeReadbackPath,
                    "$routeProbeReadback`n",
                    [Text.UTF8Encoding]::new($false))
                if (-not $routeProbeMatched) {
                    $routeLocationDump = Invoke-AdbShell $actionSerial dumpsys location
                    [IO.File]::WriteAllText(
                        (Join-Path $outputRoot "location-route-framework-dumpsys.txt"),
                        "$routeLocationDump`n",
                        [Text.UTF8Encoding]::new($false))
                    throw "real GPS LocationManager callback did not observe the controlled route point: $routeProbeReadback"
                }

                Invoke-Action route-stop
                $routeStarted = $false
                $routeStopped = Get-Instance $InstanceId
                if ($null -ne $routeStopped.location_route -or
                    $null -eq $routeStopped.last_location_route -or
                    $routeStopped.last_location_route.reason -ne "stopped" -or
                    [int]$routeStopped.last_location_route.applied_points -lt 2) {
                    throw "location route did not publish a complete stopped result"
                }
                $locationDelivery = Get-Content -LiteralPath $locationDeliveryPath -Raw | ConvertFrom-Json
                $routeDeliveriesAfter = [long]$locationDelivery.delivered_sequence
                if ($routeDeliveriesAfter -le $routeDeliveriesBefore) {
                    throw "location route did not advance the Guest delivery sequence"
                }
                $routeDeliveryArtifact = Join-Path $outputRoot "location-route-delivery-v1.json"
                Copy-Item -LiteralPath $locationDeliveryPath -Destination $routeDeliveryArtifact
                $locationRouteEvidence = [ordered]@{
                    package = "com.hd.locationprobe"
                    apk_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $LocationProbeApk).Hash.ToLowerInvariant()
                    provider = "gps"
                    latitude = 37.4223000
                    longitude = -122.0837000
                    altitude = 7.000
                    accuracy = 5.000
                    attempts = $routeProbeAttempt
                    delivered_sequence_before = $routeDeliveriesBefore
                    delivered_sequence_after = $routeDeliveriesAfter
                    point_count = 3
                    applied_points = [int]$routeStopped.last_location_route.applied_points
                    paused_point_stable = $true
                    mock_provider_used = $false
                    artifact = $routeProbeReadbackPath
                    delivery_artifact = $routeDeliveryArtifact
                    package_uninstalled = $false
                }
                Add-ActionReadback location_route `
                    "KML,start,pause,resume,stop,repeat;lat=37.4223000,lon=-122.0837000,alt=7.000,accuracy=5.000" `
                    "LocationManager GPS route callback exact-match;mock-provider=false;delivery=$routeDeliveriesBefore->$routeDeliveriesAfter"
            } finally {
                if ($routeStarted) {
                    try { Invoke-Action route-stop } catch { }
                }
                if ($routeProbeInstalled) {
                    try { Invoke-AdbShell $actionSerial am force-stop com.hd.locationprobe | Out-Null } catch { }
                    $routeUninstallOutput = & $Adb -s $actionSerial uninstall com.hd.locationprobe 2>&1
                    if ($LASTEXITCODE -ne 0 -and $null -eq $failure) {
                        throw "location route probe APK uninstall failed: $($routeUninstallOutput -join [Environment]::NewLine)"
                    }
                    if ($null -ne $locationRouteEvidence) {
                        $locationRouteEvidence["package_uninstalled"] = $true
                    }
                }
            }
        }

        Invoke-Action battery 73 --charging --temperature-deci-celsius 280
        $battery = Invoke-AdbShell $actionSerial dumpsys battery
        foreach ($expected in @("AC powered: true", "level: 73", "temperature: 280")) {
            if (-not $battery.Contains($expected)) { throw "battery readback omitted $expected" }
        }
        Add-ActionReadback battery "level=73,charging=true,temperature=280" "BatteryService-match"

        Invoke-Action network 25 10 --bandwidth-kbps 50000
        $networkInterface = $null
        $qdisc = $null
        foreach ($candidate in @("eth2", "eth1", "eth0")) {
            try {
                $candidateQdisc = Invoke-AdbShell $actionSerial su 0 tc qdisc show dev $candidate
                if ($candidateQdisc.Contains("qdisc netem")) {
                    $networkInterface = $candidate
                    $qdisc = $candidateQdisc
                    break
                }
            } catch { }
        }
        if ($null -eq $networkInterface -or
            -not $qdisc.Contains("delay 25.0ms") -or
            -not $qdisc.Contains("loss 0.1%")) {
            throw "Android eth0/eth1/eth2 did not preserve the verified netem action"
        }
        Add-ActionReadback network "latency=25ms,loss=0.1%,bandwidth=50000kbps" "$networkInterface $qdisc"
        Invoke-Action network 0 0

        if ($actionReady.instance.spec.devices.sensors) {
            $baselineSensors = Invoke-AdbShell $actionSerial dumpsys sensorservice
            $baselineSensorReadbackPath = Join-Path $outputRoot "sensorservice-baseline.txt"
            [IO.File]::WriteAllText($baselineSensorReadbackPath, $baselineSensors, [Text.UTF8Encoding]::new($false))
            Invoke-Action sensor-pose 90000 0 0 --transition-ms 200
            Start-Sleep -Milliseconds 250
            $sensors = Invoke-AdbShell $actionSerial dumpsys sensorservice
            $sensorReadbackPath = Join-Path $outputRoot "sensorservice-after-injection.txt"
            [IO.File]::WriteAllText($sensorReadbackPath, $sensors, [Text.UTF8Encoding]::new($false))
            $sensorReadbackSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $sensorReadbackPath).Hash.ToLowerInvariant()
            foreach ($sensorName in @("Accel Sensor", "Gyro Sensor", "Magnetic Field Sensor", "Light Sensor", "Proximity Sensor")) {
                if (-not $sensors.Contains($sensorName)) { throw "SensorService omitted $sensorName" }
            }
            if ($sensors -notmatch '\) 0\.00, 0\.00, -9\.81,') {
                throw "AOSP Guest motion injector did not expose the derived accelerometer frame"
            }
            if ($sensors -notmatch '\) 0\.00, -48\.40, -5\.90,') {
                throw "AOSP Guest motion injector did not expose the derived magnetic-field frame"
            }
            if ($sensors -notmatch '\) 7\.85, 0\.00, 0\.00,') {
                throw "AOSP Guest motion injector did not expose the derived gyroscope frame"
            }
            if (-not $sensors.Contains("Mode : NORMAL")) {
                throw "SensorService did not remain in NORMAL mode after one-shot motion injection"
            }
            Add-ActionReadback sensors "pose=x:90deg,y:0deg,z:0deg;transition_ms=200" "AOSP-Guest-one-shot-motion-triplet-observed;SensorService-mode=NORMAL;injected_sha256=$sensorReadbackSha256"
        } else { throw "Windows AOSP Guest motion capability was unexpectedly unavailable" }

        $peer = [Guid]::NewGuid()
        Invoke-Action bluetooth-create $peer.ToString() HD-Acceptance-Peer
        Invoke-Action bluetooth-advertise $peer.ToString() true
        Invoke-Action bluetooth-advertise $peer.ToString() false
        Invoke-Action bluetooth-remove $peer.ToString()
        $bluetooth = Invoke-AdbShell $actionSerial dumpsys bluetooth_manager
        if ($bluetooth -notmatch "enabled: true|state: ON|STATE_ON") {
            throw "Bluetooth framework was not healthy after the RootCanal peer lifecycle"
        }
        Add-ActionReadback bluetooth "create,advertise-on,advertise-off,remove" "framework-alive"

        if ($RunBluetoothHogp) {
            $hidPeer = [Guid]::NewGuid()
            # Keep the complete local name inside the legacy 31-byte advertising payload so
            # Android Settings can display and match the exact product identity without truncation.
            $hidName = "HD-HOGP-$($hidPeer.ToString('N').Substring(0, 6))"
            Invoke-Action bluetooth-hid-keyboard $hidPeer.ToString() $hidName
            Invoke-AdbShell $actionSerial input keyevent WAKEUP | Out-Null
            try { Invoke-AdbShell $actionSerial wm dismiss-keyguard | Out-Null } catch { }
            Invoke-AdbShell -Serial $actionSerial -Arguments @(
                "am", "start", "-a", "android.settings.BLUETOOTH_SETTINGS"
            ) | Out-Null

            $pairPageOpened = $false
            for ($attempt = 0; $attempt -lt 20; $attempt++) {
                $uiPath = Join-Path $outputRoot "bluetooth-hid-connected-devices-$attempt.xml"
                Save-AndroidUiDump $actionSerial $uiPath
                $center = Get-AndroidUiNodeCenter $uiPath text "Pair new device"
                if ($null -ne $center) {
                    Invoke-AdbShell $actionSerial input tap ([int]$center.x) ([int]$center.y) | Out-Null
                    $pairPageOpened = $true
                    break
                }
                Start-Sleep -Milliseconds 250
            }
            if (-not $pairPageOpened) {
                throw "Android Bluetooth settings did not expose the Pair new device action"
            }

            $hidDeviceTapped = $false
            for ($attempt = 0; $attempt -lt 60; $attempt++) {
                $uiPath = Join-Path $outputRoot "bluetooth-hid-discovery-$attempt.xml"
                Save-AndroidUiDump $actionSerial $uiPath
                $center = Get-AndroidUiNodeCenter $uiPath text $hidName
                if ($null -ne $center) {
                    Invoke-AdbShell $actionSerial input tap ([int]$center.x) ([int]$center.y) | Out-Null
                    $hidDeviceTapped = $true
                    break
                }
                Start-Sleep -Milliseconds 500
            }
            if (-not $hidDeviceTapped) {
                throw "Android Bluetooth settings did not discover the HOGP keyboard"
            }

            for ($attempt = 0; $attempt -lt 20; $attempt++) {
                $uiPath = Join-Path $outputRoot "bluetooth-hid-pairing-$attempt.xml"
                Save-AndroidUiDump $actionSerial $uiPath
                $center = Get-AndroidUiNodeCenter $uiPath resource-id "android:id/button1"
                if ($null -ne $center) {
                    Invoke-AdbShell $actionSerial input tap ([int]$center.x) ([int]$center.y) | Out-Null
                    break
                }
                $bond = Invoke-AdbShell $actionSerial dumpsys bluetooth_manager
                if ($bond.Contains($hidName)) { break }
                Start-Sleep -Milliseconds 250
            }

            $hidReady = $false
            $lastHidError = $null
            for ($attempt = 0; $attempt -lt 120; $attempt++) {
                $bond = Invoke-AdbShell $actionSerial dumpsys bluetooth_manager
                if ($bond.Contains($hidName)) {
                    try {
                        Invoke-Action bluetooth-hid-keyboard-report $hidPeer.ToString()
                        $hidReady = $true
                        break
                    } catch {
                        $lastHidError = $_.Exception.Message
                    }
                }
                Start-Sleep -Milliseconds 250
            }
            Set-Content -LiteralPath (Join-Path $outputRoot "bluetooth-hid-bond.txt") `
                -Value $bond -Encoding utf8NoBOM
            if (-not $hidReady) {
                throw "Android did not complete encrypted HOGP discovery and input notification subscription: $lastHidError"
            }

            $geteventOutput = Join-Path $outputRoot "bluetooth-hid-getevent.txt"
            $geteventError = Join-Path $outputRoot "bluetooth-hid-getevent.stderr.txt"
            $getevent = Start-Process -FilePath $Adb -ArgumentList @(
                "-s", $actionSerial, "shell", "getevent", "-lt"
            ) -WindowStyle Hidden -RedirectStandardOutput $geteventOutput `
                -RedirectStandardError $geteventError -PassThru
            try {
                Start-Sleep -Milliseconds 500
                Invoke-Action bluetooth-hid-keyboard-report $hidPeer.ToString() --keys 4
                Invoke-Action bluetooth-hid-keyboard-report $hidPeer.ToString()
                Start-Sleep -Milliseconds 750
            } finally {
                if (-not $getevent.HasExited) {
                    Stop-Process -Id $getevent.Id -Force -ErrorAction SilentlyContinue
                }
                [void]$getevent.WaitForExit(2000)
            }
            $geteventText = Get-Content -Raw -LiteralPath $geteventOutput
            foreach ($marker in @("KEY_A", "DOWN", "UP")) {
                if (-not $geteventText.Contains($marker)) {
                    throw "Android input subsystem omitted HOGP marker $marker"
                }
            }
            $hidState = Get-Instance $InstanceId
            $hidRecord = @($hidState.bluetooth_peers | Where-Object peer_id -eq $hidPeer.ToString())
            if ($hidRecord.Count -ne 1 -or $hidRecord[0].kind -ne "hid_keyboard" -or
                $hidRecord[0].keyboard_reports_sent -ne 3) {
                throw "HOGP keyboard state did not preserve three readiness/down/up reports"
            }
            Invoke-Action bluetooth-remove $hidPeer.ToString()
            Add-ActionReadback bluetooth_hogp_keyboard `
                "pair,encrypted-input-subscription,KEY_A-down-up" `
                "peer=$hidName,reports=3,guest-input=KEY_A-DOWN-UP"
        }

        $captureDurationMillis = 6000
        $captureProcess = Start-HdctlActionProcess bluetooth-hci-capture --duration-ms $captureDurationMillis
        $bluetoothReenabled = $false
        try {
            Start-Sleep -Milliseconds 500
            Invoke-AdbShell $actionSerial cmd bluetooth_manager disable | Out-Null
            Start-Sleep -Milliseconds 1000
            Invoke-AdbShell $actionSerial cmd bluetooth_manager enable | Out-Null
            $bluetoothReenabled = $true
            if (-not $captureProcess.WaitForExit($captureDurationMillis + 10000)) {
                $captureProcess.Kill($true)
                throw "bounded Bluetooth HCI capture did not finish"
            }
            $captureStdout = $captureProcess.StandardOutput.ReadToEnd()
            $captureStderr = $captureProcess.StandardError.ReadToEnd()
            if ($captureProcess.ExitCode -ne 0) {
                throw "Bluetooth HCI capture failed: $captureStderr"
            }
            Set-Content -LiteralPath (Join-Path $outputRoot "bluetooth-hci-capture-action.json") `
                -Value $captureStdout -Encoding utf8NoBOM
            Set-Content -LiteralPath (Join-Path $outputRoot "bluetooth-hci-capture-action.stderr.txt") `
                -Value $captureStderr -Encoding utf8NoBOM
        } finally {
            if (-not $bluetoothReenabled) {
                try { Invoke-AdbShell $actionSerial cmd bluetooth_manager enable | Out-Null } catch { }
            }
            if ($null -ne $captureProcess -and -not $captureProcess.HasExited) {
                $captureProcess.Kill($true)
                [void]$captureProcess.WaitForExit(2000)
            }
        }
        $actions.Add("bluetooth-hci-capture:$captureDurationMillis")
        $captureState = Get-Instance $InstanceId
        if ($captureState.status.observed -ne "ready" -or
            $captureState.active_run_id -ne $actionReady.instance.active_run_id -or
            $null -eq $captureState.last_bluetooth_hci_capture) {
            throw "Bluetooth HCI capture did not preserve the Ready run and typed record"
        }
        $capture = $captureState.last_bluetooth_hci_capture
        if ($capture.requested_duration_ms -ne $captureDurationMillis -or
            $capture.packets_captured -le 0 -or $capture.output_size_bytes -le 16 -or
            $capture.output_size_bytes -gt 4MB -or $capture.truncated) {
            throw "Bluetooth HCI capture record violates the bounded real-traffic contract"
        }
        if ($capture.file_name -ne "rootcanal-hci-$($capture.capture_id).btsnoop") {
            throw "Bluetooth HCI capture file name is not bound to its generated UUID"
        }
        $capturePath = Join-Path $DataRoot `
            "runs\$InstanceId\$($captureState.active_run_id)\components\$($capture.file_name)"
        if (-not (Test-Path -LiteralPath $capturePath -PathType Leaf)) {
            throw "Bluetooth HCI capture artifact is missing: $capturePath"
        }
        $captureFile = Get-Item -LiteralPath $capturePath
        if ($captureFile.Length -ne $capture.output_size_bytes -or
            (Read-BtsnoopHeader $capturePath) -ne "6274736e6f6f700000000001000003ea") {
            throw "Bluetooth HCI capture bytes do not match standard btsnoop HCI UART metadata"
        }
        $captureEvidencePath = Join-Path $outputRoot $capture.file_name
        Copy-Item -LiteralPath $capturePath -Destination $captureEvidencePath
        $captureSha256 = (Get-FileHash -LiteralPath $captureEvidencePath -Algorithm SHA256).Hash.ToLowerInvariant()
        $bluetoothDeadline = [DateTimeOffset]::UtcNow.AddSeconds(20)
        do {
            $bluetooth = Invoke-AdbShell $actionSerial dumpsys bluetooth_manager
            if ($bluetooth -match "enabled: true" -and
                $bluetooth -match "state: ON|state ON|STATE_ON") {
                break
            }
            Start-Sleep -Milliseconds 500
        } while ([DateTimeOffset]::UtcNow -lt $bluetoothDeadline)
        if ($bluetooth -notmatch "enabled: true" -or
            $bluetooth -notmatch "state: ON|state ON|STATE_ON") {
            throw "Android Bluetooth framework did not recover after real HCI capture traffic"
        }
        Add-ActionReadback bluetooth_hci_capture `
            "duration_ms=$captureDurationMillis" `
            "packets=$($capture.packets_captured),bytes=$($capture.output_size_bytes),sha256=$captureSha256"
        Invoke-Action nfc-type2 D101055401656E6869
        Invoke-Action nfc-remove
        Invoke-Action nfc-type4 D101055401656E6869
        Invoke-Action nfc-remove
        $nfc = Invoke-AdbShell $actionSerial getprop init.svc.nfc_hal_service
        if ($nfc.Trim() -ne "running") { throw "NFC HAL was not running after Type 2/Type 4 actions" }
        Add-ActionReadback nfc "type2,remove,type4,remove" "nfc_hal_service=running"

        $uwbServices = Invoke-AdbShell $actionSerial service list
        $uwbHalPids = (Invoke-AdbShell $actionSerial pidof android.hardware.uwb-service).Trim()
        $uwbDump = Invoke-AdbShell $actionSerial dumpsys uwb
        if ([string]::IsNullOrWhiteSpace($uwbHalPids) -or
            -not $uwbServices.Contains("android.hardware.uwb.IUwb/default") -or
            $uwbServices -notmatch '(?m)^\s*\d+\s+uwb:' -or
            -not $uwbDump.Contains("mCountryCode:") -or -not $uwbDump.Contains("US")) {
            throw "Android UWB HAL/framework did not expose the fixed US policy"
        }
        Invoke-Action uwb-ranging 321
        $uwbState = Get-Instance $InstanceId
        if ([int]$uwbState.uwb_ranging.distance_cm -ne 321 -or
            $uwbState.status.observed -ne "ready") {
            throw "UWB typed distance control did not preserve the Ready instance state"
        }
        Add-ActionReadback uwb `
            "distance_cm=321" `
            "HAL/AIDL/framework-alive;country=US;typed-state=321cm;active-FiRa-session-not-asserted"

        if ($RunUwbFira) {
            $firaSessionId = 77
            $firaStarted = $false
            $firaStartOutput = ""
            $firaReportsOutput = ""
            $firaStopOutput = ""
            $firaStartPath = Join-Path $outputRoot "uwb-fira-start.txt"
            $firaReportsPath = Join-Path $outputRoot "uwb-fira-reports.txt"
            $firaStopPath = Join-Path $outputRoot "uwb-fira-stop.txt"
            try {
                Invoke-AdbShell $actionSerial cmd uwb stop-all-ranging-sessions | Out-Null
                $firaStartOutput = Invoke-AdbShell `
                    -Serial $actionSerial `
                    -Arguments @(
                        "cmd", "uwb", "start-fira-ranging-session",
                        "-i", "$firaSessionId",
                        "-R", "enabled",
                        "-e", "none",
                        "-f", "tof"
                    )
                if ($firaStartOutput -notmatch "Ranging session opened with params:" -or
                    $firaStartOutput -notmatch "Ranging session started for sessionId: $firaSessionId") {
                    throw "Android UWB shell did not open and start the FiRa session: $firaStartOutput"
                }
                $firaStarted = $true
                $firaDeadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
                do {
                    $firaReportsOutput = Invoke-AdbShell $actionSerial cmd uwb `
                        get-ranging-session-reports $firaSessionId
                    if ($firaReportsOutput -match "RangingReport\[" -and
                        $firaReportsOutput -match "distance measurement:\s*DistanceMeasurement\[meters:\s*3\.21(?:0+)?\b" -and
                        $firaReportsOutput -match "status:\s*0\b") {
                        break
                    }
                    Start-Sleep -Milliseconds 250
                } while ([DateTimeOffset]::UtcNow -lt $firaDeadline)
                if ($firaReportsOutput -notmatch "RangingReport\[" -or
                    $firaReportsOutput -notmatch "distance measurement:\s*DistanceMeasurement\[meters:\s*3\.21(?:0+)?\b" -or
                    $firaReportsOutput -notmatch "status:\s*0\b") {
                    throw "Android framework did not publish the controlled 321 cm FiRa report: $firaReportsOutput"
                }
                $actions.Add("uwb-fira-session:$firaSessionId")
                Add-ActionReadback uwb_fira `
                    "session_id=$firaSessionId,distance_cm=321" `
                    "framework-session=opened+started;measurement=3.21m;status=0"
                $uwbFiraEvidence = [ordered]@{
                    session_id = $firaSessionId
                    requested_distance_cm = 321
                    observed_distance_m = 3.21
                    measurement_status = 0
                    start_artifact = $firaStartPath
                    reports_artifact = $firaReportsPath
                    stop_artifact = $firaStopPath
                }
            } finally {
                if ($firaStarted) {
                    $firaStopOutput = Invoke-AdbShell $actionSerial cmd uwb `
                        stop-ranging-session $firaSessionId
                    if ($firaStopOutput -notmatch "Ranging session stopped" -or
                        $firaStopOutput -notmatch "Ranging session closed") {
                        throw "Android UWB shell did not stop and close the FiRa session: $firaStopOutput"
                    }
                }
                [IO.File]::WriteAllText(
                    $firaStartPath,
                    $firaStartOutput,
                    [Text.UTF8Encoding]::new($false)
                )
                [IO.File]::WriteAllText(
                    $firaReportsPath,
                    $firaReportsOutput,
                    [Text.UTF8Encoding]::new($false)
                )
                [IO.File]::WriteAllText(
                    $firaStopPath,
                    $firaStopOutput,
                    [Text.UTF8Encoding]::new($false)
                )
            }
        }

        Invoke-Action modem-state 310260 17 `
            --operator-long-name HD-Mobile-QA `
            --operator-short-name HD-QA `
            --registered true
        $modemDeadline = [DateTimeOffset]::UtcNow.AddSeconds(30)
        do {
            $rilState = (Invoke-AdbShell $actionSerial getprop init.svc.vendor.ril-daemon).Trim()
            $rilPids = (Invoke-AdbShell $actionSerial pidof libcuttlefish-rild).Trim()
            $operatorNumeric = (Invoke-AdbShell $actionSerial getprop gsm.operator.numeric).Trim()
            $operatorAlpha = (Invoke-AdbShell $actionSerial getprop gsm.operator.alpha).Trim()
            $modemServices = Invoke-AdbShell $actionSerial service list
            $telephonyRegistry = Invoke-AdbShell $actionSerial dumpsys telephony.registry
            if ($rilState -eq "running" -and
                -not [string]::IsNullOrWhiteSpace($rilPids) -and
                $modemServices.Contains("android.hardware.radio") -and
                $operatorNumeric.Contains("310260") -and
                $operatorAlpha.Contains("HD-Mobile-QA") -and
                $telephonyRegistry.Contains("310260") -and
                $telephonyRegistry -match 'mVoiceRegState=0\(IN_SERVICE\)' -and
                $telephonyRegistry -match 'mGsm=CellSignalStrengthGsm: rssi=-79\b') {
                break
            }
            Start-Sleep -Milliseconds 250
        } while ([DateTimeOffset]::UtcNow -lt $modemDeadline)
        if ($rilState -ne "running" -or
            [string]::IsNullOrWhiteSpace($rilPids) -or
            -not $modemServices.Contains("android.hardware.radio") -or
            -not $operatorNumeric.Contains("310260") -or
            -not $operatorAlpha.Contains("HD-Mobile-QA") -or
            -not $telephonyRegistry.Contains("310260") -or
            $telephonyRegistry -notmatch 'mVoiceRegState=0\(IN_SERVICE\)' -or
            $telephonyRegistry -notmatch 'mGsm=CellSignalStrengthGsm: rssi=-79\b') {
            throw "Android RIL/Radio/framework did not publish the typed modem state"
        }
        $modemRegistryPath = Join-Path $outputRoot "telephony-registry-after-modem-action.txt"
        [IO.File]::WriteAllText(
            $modemRegistryPath,
            $telephonyRegistry,
            [Text.UTF8Encoding]::new($false)
        )
        $modemRegistrySha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $modemRegistryPath).Hash.ToLowerInvariant()
        $modemState = Get-Instance $InstanceId
        if ($modemState.modem_state.operator_numeric -ne "310260" -or
            $modemState.modem_state.operator_long_name -ne "HD-Mobile-QA" -or
            $modemState.modem_state.operator_short_name -ne "HD-QA" -or
            [int]$modemState.modem_state.signal_strength -ne 17 -or
            -not [bool]$modemState.modem_state.registered -or
            $modemState.status.observed -ne "ready") {
            throw "Host modem state did not match the Guest-observed typed action"
        }
        Add-ActionReadback modem `
            "operator=310260,long=HD-Mobile-QA,short=HD-QA,signal=17,registered=true" `
            "RIL/Radio/framework=alive;gsm.operator.numeric=$operatorNumeric;gsm.operator.alpha=$operatorAlpha;voice=IN_SERVICE;gsm_rssi=-79dBm;registry_sha256=$modemRegistrySha256"

        if (-not [string]::IsNullOrWhiteSpace($Apk)) {
            if (-not (Test-Path -LiteralPath $Apk -PathType Leaf)) { throw "APK is missing: $Apk" }
            Invoke-Hdctl install $InstanceId.ToString() $Apk | Out-Null
            $packagePath = ((& $Adb -s (Get-Instance $InstanceId).adb_serial shell cmd package path com.hd.acceptance 2>&1) -join "`n").Trim()
            if ($LASTEXITCODE -ne 0 -or $packagePath -notmatch "^package:") {
                throw "installed acceptance package was not read back: $packagePath"
            }
            $actions.Add("install:com.hd.acceptance")
        }

        if ($RunAdbLossPowerFallback) {
            $beforeLoss = Get-Instance $InstanceId
            if (-not $beforeLoss.adb_ready -or
                [string]::IsNullOrWhiteSpace($beforeLoss.adb_serial)) {
                throw "ADB-loss power fallback requires a Ready ADB transport"
            }
            $disconnectOutput = (& $Adb disconnect $beforeLoss.adb_serial 2>&1) -join "`n"
            if ($LASTEXITCODE -ne 0) {
                throw "failed to remove the isolated ADB transport: $disconnectOutput"
            }
            Invoke-Hdctl action $InstanceId.ToString() key power | Out-Null
            $fallbackDeadline = [DateTimeOffset]::UtcNow.AddSeconds(5)
            $afterLoss = $null
            do {
                $afterLoss = Get-Instance $InstanceId
                if (-not $afterLoss.adb_ready) {
                    break
                }
                Start-Sleep -Milliseconds 100
            } while ([DateTimeOffset]::UtcNow -lt $fallbackDeadline)
            if ($afterLoss.adb_ready -or $afterLoss.status.observed -ne "ready" -or
                $afterLoss.active_run_id -ne $beforeLoss.active_run_id) {
                throw "definitive ADB loss did not clear stale readiness while preserving the Ready run"
            }
            $afterProcesses = Get-RuntimeProcessSnapshot ([int]$afterLoss.worker.pid)
            if (-not ($afterProcesses.processes | Where-Object name -eq "crosvm")) {
                throw "native power fallback lost the running crosvm Guest"
            }
            $adbLossPowerEvidence = [ordered]@{
                contract = "definitive-adb-loss-single-native-power-fallback"
                run_id = $afterLoss.active_run_id
                worker_pid = [int]$afterLoss.worker.pid
                adb_serial = $beforeLoss.adb_serial
                disconnect_output = $disconnectOutput.Trim()
                adb_ready_before = [bool]$beforeLoss.adb_ready
                adb_ready_after = [bool]$afterLoss.adb_ready
                observed_after = $afterLoss.status.observed
                crosvm_process_count = @($afterProcesses.processes | Where-Object name -eq "crosvm").Count
            }
            $actions.Add("power:adb-loss-platform-recovery")
            Add-ActionReadback power `
                "adb=definitively-offline;fallback=platform_recovery" `
                "adb_ready=false;same-run=$($afterLoss.active_run_id);crosvm=alive"
        }
    }

    if ($RunScreenRecording) {
        $recordingReady = Assert-Ready $InstanceId
        $recordingRunId = [Guid]$recordingReady.instance.active_run_id
        $recordingFrameGeneration = [long]$recordingReady.instance.frame_generation
        $recordingPresentedBefore = [long]$recordingReady.metrics.presented_frames
        $recordingReadbackBefore = [long]$recordingReady.metrics.cpu_readback_bytes
        $recordingSoftwareBlitBefore = [long]$recordingReady.metrics.software_blit_count
        $recordingStarted = $false
        $recordingPath = $null
        $recordingPhysicalInputPrepared = $false
        try {
            $recordingStatus = Invoke-Hdctl screen-record-start $InstanceId.ToString() `
                --display primary --max-duration-seconds 10 | ConvertFrom-Json
            if ([Guid]$recordingStatus.instance_id -ne $InstanceId -or
                [int]$recordingStatus.max_duration_seconds -ne 10 -or
                $recordingStatus.display_id.kind -ne "primary") {
                throw "screen recording start returned the wrong instance, display, or duration"
            }
            $recordingStarted = $true
            foreach ($key in @("recent", "home", "back")) {
                Invoke-Action key $key
                Start-Sleep -Milliseconds 500
            }
            if ($RunUiDisplayInput -and $null -ne $renderWindow) {
                # Exercise the same physical cursor/SendInput/render-HWND path as a user while
                # gfxstream records its source ColorBuffers. Posted window messages bypass DWM
                # hit-testing and focus, so they cannot prove the reported mouse-triggered flash.
                $recordingPhysicalInputPrepared =
                    [HdRealGuestWindowNative]::PrepareForPhysicalInput($renderWindow)
                try {
                    if (-not $recordingPhysicalInputPrepared) {
                        throw "Windows could not prepare physical pointer input during host frame recording"
                    }
                    for ($drag = 0; $drag -lt 4; $drag++) {
                        $startY = if (($drag % 2) -eq 0) { $dragBottom } else { $dragTop }
                        $endY = if (($drag % 2) -eq 0) { $dragTop } else { $dragBottom }
                        if (-not [HdRealGuestWindowNative]::DragClientPoint(
                            $renderWindow,
                            $dragX,
                            $startY,
                            $dragX,
                            $endY,
                            $dragSteps,
                            $pointerSampleIntervalMillis)) {
                            throw "Windows SendInput could not exercise pointer input during host frame recording"
                        }
                    }
                } finally {
                    [HdRealGuestWindowNative]::RestoreAfterPhysicalInput($renderWindow)
                }
            }
            Start-Sleep -Seconds 2
            $recording = Invoke-Hdctl screen-record-stop $InstanceId.ToString() | ConvertFrom-Json
            $recordingStarted = $false
            if ([Guid]$recording.id -ne [Guid]$recordingStatus.id -or
                [Guid]$recording.instance_id -ne $InstanceId -or
                $recording.display_id.kind -ne "primary") {
                throw "screen recording stop returned the wrong recording identity"
            }
            $recordingPath = [IO.Path]::GetFullPath([string]$recording.path)
            $videosDirectory = [Environment]::GetFolderPath([Environment+SpecialFolder]::MyVideos)
            if ([string]::IsNullOrWhiteSpace($videosDirectory)) {
                throw "Windows Videos known folder is unavailable"
            }
            $managedRecordingDirectory = [IO.Path]::GetFullPath((Join-Path $videosDirectory "HD"))
            $expectedRecordingName = "$InstanceId-$(([Guid]$recording.id).ToString('N')).mp4"
            if ([IO.Path]::GetDirectoryName($recordingPath) -ne $managedRecordingDirectory -or
                [IO.Path]::GetFileName($recordingPath) -ne $expectedRecordingName -or
                -not (Test-Path -LiteralPath $recordingPath -PathType Leaf)) {
                throw "screen recording escaped the managed Windows Videos/HD path"
            }
            $recordingFile = Get-Item -LiteralPath $recordingPath
            if ($recordingFile.Length -ne [long]$recording.size_bytes -or
                $recordingFile.Length -le 1024 -or [long]$recording.duration_millis -le 0) {
                throw "screen recording size or wall duration violates the product boundary"
            }
            $recordingSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $recordingPath).Hash.ToLowerInvariant()
            if ($recordingSha256 -ne ([string]$recording.sha256).ToLowerInvariant()) {
                throw "screen recording SHA-256 does not match the Worker record"
            }
            $recordingAcl = Get-Acl -LiteralPath $recordingPath
            if (-not $recordingAcl.AreAccessRulesProtected) {
                throw "screen recording inherited an ambient Windows ACL"
            }
            $allowedRecordingSids = @(
                [Security.Principal.WindowsIdentity]::GetCurrent().User.Value,
                "S-1-5-18"
            )
            $recordingAccessSids = @($recordingAcl.Access | ForEach-Object {
                $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
            } | Sort-Object -Unique)
            if (($recordingAccessSids | Where-Object { $_ -notin $allowedRecordingSids }).Count -ne 0 -or
                ($allowedRecordingSids | Where-Object { $_ -notin $recordingAccessSids }).Count -ne 0) {
                throw "screen recording ACL is not limited to the current user and SYSTEM"
            }

            $mp4 = Get-Mp4Metrics $recordingPath
            if ($mp4.dimensions -notcontains "1080x1920") {
                throw "screen recording dimensions do not match the selected 1080x1920 display: $($mp4.dimensions -join ',')"
            }
            if (($mp4.media_duration_millis * 100) -lt ([long]$recording.duration_millis * 75) -or
                $mp4.media_duration_millis -gt ([long]$recording.duration_millis + 1000)) {
                throw "screen recording media duration does not track its wall duration"
            }

            $workerLogRoot = Join-Path $DataRoot "logs\workers"
            $recordingIdText = ([Guid]$recording.id).ToString()
            $hostStatsLines = @()
            $hostStatsDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                if (Test-Path -LiteralPath $workerLogRoot -PathType Container) {
                    $hostStatsLines = @(Get-ChildItem -LiteralPath $workerLogRoot -File `
                        -Filter "$InstanceId.jsonl*" -ErrorAction SilentlyContinue | ForEach-Object {
                            Select-String -LiteralPath $_.FullName `
                                -Pattern "worker.screen_recording.host_stats" -SimpleMatch |
                                ForEach-Object Line
                        } | Where-Object { $_.Contains($recordingIdText) })
                }
                if ($hostStatsLines.Count -eq 0) {
                    Start-Sleep -Milliseconds 100
                }
            } while ($hostStatsLines.Count -eq 0 -and
                [DateTimeOffset]::UtcNow -lt $hostStatsDeadline)
            if ($hostStatsLines.Count -eq 0) {
                throw "screen recording did not publish gfxstream host encoder statistics"
            }
            $hostStatsLine = $hostStatsLines[-1]
            $encodedMatch = [Regex]::Match($hostStatsLine, '"encoded_frames"\s*:\s*(?<count>\d+)')
            $droppedMatch = [Regex]::Match($hostStatsLine, '"dropped_frames"\s*:\s*(?<count>\d+)')
            $initialStaticFrameMatch = [Regex]::Match(
                $hostStatsLine,
                '"initial_static_frame"\s*:\s*(?<captured>true|false)')
            $initialFrameYDirectionMatch = [Regex]::Match(
                $hostStatsLine,
                '"initial_frame_y_direction"\s*:\s*(?<direction>-?\d+)')
            $nearBlackFramesMatch = [Regex]::Match(
                $hostStatsLine,
                '"near_black_frames"\s*:\s*(?<count>\d+)')
            $maxConsecutiveNearBlackFramesMatch = [Regex]::Match(
                $hostStatsLine,
                '"max_consecutive_near_black_frames"\s*:\s*(?<count>\d+)')
            $maxSourceFrameGapMatch = [Regex]::Match(
                $hostStatsLine,
                '"max_source_frame_gap_millis"\s*:\s*(?<count>\d+)')
            $sourceFrameGapsOver100Match = [Regex]::Match(
                $hostStatsLine,
                '"source_frame_gaps_over_100_millis"\s*:\s*(?<count>\d+)')
            if (-not $encodedMatch.Success -or [long]$encodedMatch.Groups["count"].Value -lt 2 -or
                -not $droppedMatch.Success -or
                -not $initialStaticFrameMatch.Success -or
                $initialStaticFrameMatch.Groups["captured"].Value -ne "true" -or
                -not $initialFrameYDirectionMatch.Success -or
                [int]$initialFrameYDirectionMatch.Groups["direction"].Value -ne 1 -or
                -not $nearBlackFramesMatch.Success -or
                -not $maxConsecutiveNearBlackFramesMatch.Success -or
                -not $maxSourceFrameGapMatch.Success -or
                -not $sourceFrameGapsOver100Match.Success) {
                throw "screen recording host statistics omitted usable frame counts or an upright static initial frame"
            }
            if ([long]$droppedMatch.Groups["count"].Value -ne 0 -or
                [long]$mp4.sample_count -ne [long]$encodedMatch.Groups["count"].Value -or
                [long]$nearBlackFramesMatch.Groups["count"].Value -ne 0 -or
                [long]$maxConsecutiveNearBlackFramesMatch.Groups["count"].Value -ne 0 -or
                [long]$maxSourceFrameGapMatch.Groups["count"].Value -gt 100 -or
                [long]$sourceFrameGapsOver100Match.Groups["count"].Value -ne 0) {
                throw "gfxstream dropped, mismatched, blackened or stalled frames during the recorded Android interaction"
            }
            $hostStatsArtifact = Join-Path $outputRoot "screen-recording-host-stats.jsonl"
            [IO.File]::WriteAllText(
                $hostStatsArtifact,
                "$hostStatsLine`n",
                [Text.UTF8Encoding]::new($false))

            $recordingArtifact = Join-Path $outputRoot "screen-recording.mp4"
            Copy-Item -LiteralPath $recordingPath -Destination $recordingArtifact
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath $recordingArtifact).Hash.ToLowerInvariant() -ne
                $recordingSha256) {
                throw "screen recording evidence copy does not match the verified source"
            }
            $postRecordingReady = Assert-Ready $InstanceId
            if ([Guid]$postRecordingReady.instance.active_run_id -ne $recordingRunId -or
                [long]$postRecordingReady.instance.frame_generation -ne $recordingFrameGeneration -or
                $null -ne $postRecordingReady.instance.screen_recording -or
                [Guid]$postRecordingReady.instance.last_screen_recording.id -ne [Guid]$recording.id) {
                throw "screen recording did not return the same run to Ready"
            }
            # Native display metrics are published on a successful present at most once per
            # second. Cross that boundary twice: the first post-stop transition flushes the
            # recording-only readback counters, and the second proves ordinary zero-copy
            # presentation no longer changes either real counter.
            Start-Sleep -Milliseconds 1100
            Invoke-Action key recent
            $postRecordingCheckpointDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                $postRecordingCheckpoint = Assert-Ready $InstanceId
                if ([long]$postRecordingCheckpoint.metrics.presented_frames -le
                    $recordingPresentedBefore) {
                    Start-Sleep -Milliseconds 100
                }
            } while ([long]$postRecordingCheckpoint.metrics.presented_frames -le
                $recordingPresentedBefore -and
                [DateTimeOffset]::UtcNow -lt $postRecordingCheckpointDeadline)
            $recordingReadbackCheckpoint =
                [long]$postRecordingCheckpoint.metrics.cpu_readback_bytes
            $recordingSoftwareBlitCheckpoint =
                [long]$postRecordingCheckpoint.metrics.software_blit_count
            if ([Guid]$postRecordingCheckpoint.instance.active_run_id -ne $recordingRunId -or
                [long]$postRecordingCheckpoint.instance.frame_generation -ne
                    $recordingFrameGeneration -or
                $null -ne $postRecordingCheckpoint.instance.screen_recording -or
                [long]$postRecordingCheckpoint.metrics.presented_frames -le
                    $recordingPresentedBefore -or
                $recordingReadbackCheckpoint -le $recordingReadbackBefore -or
                $recordingSoftwareBlitCheckpoint -lt $recordingSoftwareBlitBefore) {
                throw "screen recording did not publish real bounded readback/blit accounting"
            }

            Start-Sleep -Milliseconds 1100
            Invoke-Action key home
            $postRecordingMetricsDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                $postRecordingReady = Assert-Ready $InstanceId
                if ([long]$postRecordingReady.metrics.presented_frames -le
                    [long]$postRecordingCheckpoint.metrics.presented_frames) {
                    Start-Sleep -Milliseconds 100
                }
            } while ([long]$postRecordingReady.metrics.presented_frames -le
                [long]$postRecordingCheckpoint.metrics.presented_frames -and
                [DateTimeOffset]::UtcNow -lt $postRecordingMetricsDeadline)
            if ([Guid]$postRecordingReady.instance.active_run_id -ne $recordingRunId -or
                [long]$postRecordingReady.instance.frame_generation -ne $recordingFrameGeneration -or
                $null -ne $postRecordingReady.instance.screen_recording -or
                [Guid]$postRecordingReady.instance.last_screen_recording.id -ne [Guid]$recording.id -or
                [long]$postRecordingReady.metrics.presented_frames -le
                    [long]$postRecordingCheckpoint.metrics.presented_frames -or
                [long]$postRecordingReady.metrics.cpu_readback_bytes -ne
                    $recordingReadbackCheckpoint -or
                [long]$postRecordingReady.metrics.software_blit_count -ne
                    $recordingSoftwareBlitCheckpoint) {
                throw "screen recording did not resume zero-copy presentation in the same Ready run"
            }
            $screenRecordingEvidence = [ordered]@{
                id = [string]$recording.id
                display = "primary"
                original_path = $recordingPath
                artifact = $recordingArtifact
                sha256 = $recordingSha256
                size_bytes = [long]$recording.size_bytes
                sample_count = [long]$mp4.sample_count
                wall_duration_millis = [long]$recording.duration_millis
                media_duration_millis = [long]$mp4.media_duration_millis
                dimensions = $mp4.dimensions
                encoded_frames = [long]$encodedMatch.Groups["count"].Value
                dropped_frames = [long]$droppedMatch.Groups["count"].Value
                hardware_h264_required = $true
                recording_readback_scope = "recording-only"
                cpu_readback_bytes_before = $recordingReadbackBefore
                cpu_readback_bytes_after_recording = $recordingReadbackCheckpoint
                cpu_readback_bytes_recording_delta =
                    $recordingReadbackCheckpoint - $recordingReadbackBefore
                cpu_readback_bytes_post_recording_delta =
                    [long]$postRecordingReady.metrics.cpu_readback_bytes -
                    $recordingReadbackCheckpoint
                software_blit_count_before = $recordingSoftwareBlitBefore
                software_blit_count_after_recording = $recordingSoftwareBlitCheckpoint
                software_blit_count_recording_delta =
                    $recordingSoftwareBlitCheckpoint - $recordingSoftwareBlitBefore
                software_blit_count_post_recording_delta =
                    [long]$postRecordingReady.metrics.software_blit_count -
                    $recordingSoftwareBlitCheckpoint
                initial_static_frame = [bool]::Parse(
                    $initialStaticFrameMatch.Groups["captured"].Value)
                initial_frame_y_direction = [int]$initialFrameYDirectionMatch.Groups["direction"].Value
                near_black_frames = [long]$nearBlackFramesMatch.Groups["count"].Value
                max_consecutive_near_black_frames =
                    [long]$maxConsecutiveNearBlackFramesMatch.Groups["count"].Value
                max_source_frame_gap_millis = [long]$maxSourceFrameGapMatch.Groups["count"].Value
                source_frame_gaps_over_100_millis =
                    [long]$sourceFrameGapsOver100Match.Groups["count"].Value
                pointer_bridge_exercised = [bool]$RunUiDisplayInput
                pointer_input_path = if ($RunUiDisplayInput) {
                    "Win32-SendInput-capture-to-crosvm-EventDevice"
                } else {
                    $null
                }
                physical_input_prepared = [bool]$recordingPhysicalInputPrepared
                post_recording_zero_copy = $true
                presented_frames_before = $recordingPresentedBefore
                presented_frames_after = [long]$postRecordingReady.metrics.presented_frames
                source_removed = $false
            }
            Add-ActionReadback screen_recording `
                "primary,10s-bounded" `
                "gfxstream-MediaFoundation-hardware-H264;1080x1920;samples=$($mp4.sample_count);media_ms=$($mp4.media_duration_millis);post-zero-copy=true"
        } finally {
            if ($recordingStarted) {
                try { Invoke-Hdctl screen-record-stop $InstanceId.ToString() | Out-Null } catch { }
            }
            if (-not [string]::IsNullOrWhiteSpace($recordingPath) -and
                (Test-Path -LiteralPath $recordingPath -PathType Leaf)) {
                [IO.File]::Delete($recordingPath)
                if (Test-Path -LiteralPath $recordingPath) {
                    throw "screen recording test file remained in the user Videos directory"
                }
                if ($null -ne $screenRecordingEvidence) {
                    $screenRecordingEvidence["source_removed"] = $true
                }
            }
        }
    }

    if ($RunBugreport) {
        $bugreportInitial = Get-Instance $InstanceId
        if ($bugreportInitial.status.observed -ne "ready") {
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
        }
        $bugreportReady = Assert-Ready $InstanceId
        $bugreportRunId = [Guid]$bugreportReady.instance.active_run_id
        $bugreport = Invoke-Hdctl bugreport $InstanceId.ToString() | ConvertFrom-Json
        if ([Guid]$bugreport.instance_id -ne $InstanceId -or
            [Guid]$bugreport.run_id -ne $bugreportRunId -or
            -not [bool]$bugreport.contains_sensitive_data) {
            throw "Android bugreport record is not bound to the Ready instance/run"
        }
        $managedDiagnostics = [IO.Path]::GetFullPath((Join-Path $DataRoot "diagnostics"))
        $bugreportPath = [IO.Path]::GetFullPath([string]$bugreport.path)
        $managedPrefix = $managedDiagnostics.TrimEnd([IO.Path]::DirectorySeparatorChar) +
            [IO.Path]::DirectorySeparatorChar
        if (-not $bugreportPath.StartsWith($managedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Android bugreport escaped the managed diagnostics directory"
        }
        $expectedBugreportName = "android-bugreport-$InstanceId-$(([Guid]$bugreport.id).ToString('N')).zip"
        if ([IO.Path]::GetFileName($bugreportPath) -ne $expectedBugreportName -or
            -not (Test-Path -LiteralPath $bugreportPath -PathType Leaf)) {
            throw "Android bugreport artifact name or path is invalid"
        }
        $bugreportFile = Get-Item -LiteralPath $bugreportPath
        if ($bugreportFile.Length -ne [long]$bugreport.size_bytes -or
            $bugreportFile.Length -lt 22 -or $bugreportFile.Length -gt 256MB) {
            throw "Android bugreport artifact violates the bounded size contract"
        }
        $bugreportSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $bugreportPath).Hash.ToLowerInvariant()
        if ($bugreportSha256 -ne ([string]$bugreport.sha256).ToLowerInvariant()) {
            throw "Android bugreport artifact SHA-256 does not match the Worker record"
        }
        $bugreportAcl = Get-Acl -LiteralPath $bugreportPath
        if (-not $bugreportAcl.AreAccessRulesProtected) {
            throw "Android bugreport artifact inherited an ambient Windows ACL"
        }
        $allowedSids = @(
            [Security.Principal.WindowsIdentity]::GetCurrent().User.Value,
            "S-1-5-18"
        )
        $accessSids = @($bugreportAcl.Access | ForEach-Object {
            $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        } | Sort-Object -Unique)
        if (($accessSids | Where-Object { $_ -notin $allowedSids }).Count -ne 0 -or
            ($allowedSids | Where-Object { $_ -notin $accessSids }).Count -ne 0) {
            throw "Android bugreport artifact ACL is not limited to the current user and SYSTEM"
        }
        $archive = [IO.Compression.ZipFile]::OpenRead($bugreportPath)
        try {
            $entryNames = @($archive.Entries | ForEach-Object FullName)
        } finally {
            $archive.Dispose()
        }
        $mainEntries = @($entryNames | Where-Object { $_ -match '(^|/)bugreport[^/]*\.txt$' })
        if ($entryNames.Count -eq 0 -or $mainEntries.Count -eq 0) {
            throw "Android bugreport ZIP has no AOSP bugreport text member"
        }
        $bugreportEvidencePath = Join-Path $outputRoot $expectedBugreportName
        Copy-Item -LiteralPath $bugreportPath -Destination $bugreportEvidencePath
        $postBugreportReady = Assert-Ready $InstanceId
        if ([Guid]$postBugreportReady.instance.active_run_id -ne $bugreportRunId -or
            $postBugreportReady.instance.adb_serial -ne $bugreportReady.instance.adb_serial) {
            throw "Android bugreport did not preserve the Ready run and ADB identity"
        }
        $bugreportEvidence = [ordered]@{
            id = [string]$bugreport.id
            instance_id = [string]$bugreport.instance_id
            run_id = [string]$bugreport.run_id
            artifact = $bugreportEvidencePath
            size_bytes = [long]$bugreport.size_bytes
            sha256 = $bugreportSha256
            zip_entry_count = $entryNames.Count
            main_text_members = $mainEntries
            acl_sids = $accessSids
            ready_after_capture = $true
        }
        $actions.Add("bugreport:$($bugreport.id)")
        Add-ActionReadback bugreport `
            "full-aosp-dumpstate-sensitive-confirmed" `
            "bytes=$($bugreport.size_bytes),entries=$($entryNames.Count),sha256=$bugreportSha256,ready_after_capture=true"
    }

    if (-not [string]::IsNullOrWhiteSpace($SecondarySpec)) {
        if (-not (Test-Path -LiteralPath $SecondarySpec -PathType Leaf)) { throw "secondary spec is missing: $SecondarySpec" }
        $secondaryId = [Guid](Get-Content -Raw -LiteralPath $SecondarySpec | ConvertFrom-Json).id
        Invoke-Hdctl create --spec $SecondarySpec | Out-Null
        Invoke-Hdctl start $secondaryId.ToString() | Out-Null
        $primaryReady = Assert-Ready $InstanceId
        $secondaryReady = Assert-Ready $secondaryId
        if ($primaryReady.instance.worker.pid -eq $secondaryReady.instance.worker.pid -or
            $primaryReady.instance.adb_serial -eq $secondaryReady.instance.adb_serial -or
            $primaryReady.instance.active_run_id -eq $secondaryReady.instance.active_run_id) {
            throw "dual-instance identity or endpoint isolation failed"
        }
        Invoke-Hdctl stop $secondaryId.ToString() --graceful-timeout-ms 20000 | Out-Null
        Invoke-Hdctl delete $secondaryId.ToString() | Out-Null
        $secondaryId = $null
        $actions.Add("dual-instance")
    }

    if ($LifecycleCycles -gt 0) {
        Stop-IfRunning $InstanceId
        for ($cycle = 1; $cycle -le $LifecycleCycles; $cycle++) {
            $cycleStarted = [DateTimeOffset]::UtcNow
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
            $ready = Assert-Ready $InstanceId
            $workerPid = [int]$ready.instance.worker.pid
            Invoke-Hdctl stop $InstanceId.ToString() --graceful-timeout-ms 20000 | Out-Null
            Assert-Stopped $InstanceId $workerPid
            $cycles.Add([ordered]@{
                cycle = $cycle
                run_id = $ready.instance.active_run_id
                frame_generation = $ready.instance.frame_generation
                worker_pid = $workerPid
                presented_frames = $ready.metrics.presented_frames
                duration_ms = [long]([DateTimeOffset]::UtcNow - $cycleStarted).TotalMilliseconds
            })
            Write-Host "lifecycle $cycle/$LifecycleCycles passed"
        }
    }

    if ($StabilityMinutes -gt 0) {
        $instance = Get-Instance $InstanceId
        if ($instance.status.observed -ne "ready") {
            Invoke-Hdctl start $InstanceId.ToString() | Out-Null
        }
        $stable = Assert-Ready $InstanceId
        $stableRun = $stable.instance.active_run_id
        $stableWorkerPid = [int]$stable.instance.worker.pid
        $lastPresentedFrames = [long]$stable.metrics.presented_frames
        if ($lastPresentedFrames -le 0) {
            throw "stability run has no successful native presents before quiescence"
        }
        $deadline = [DateTimeOffset]::UtcNow.AddMinutes($StabilityMinutes)
        while ([DateTimeOffset]::UtcNow -lt $deadline) {
            Start-Sleep -Seconds 30
            $sample = Assert-Ready $InstanceId
            if ($sample.instance.active_run_id -ne $stableRun) {
                throw "stability run changed from $stableRun to $($sample.instance.active_run_id)"
            }
            if ([int]$sample.instance.worker.pid -ne $stableWorkerPid) {
                throw "stability worker changed from $stableWorkerPid to $($sample.instance.worker.pid)"
            }
            $presentedFrames = [long]$sample.metrics.presented_frames
            if ($presentedFrames -lt $lastPresentedFrames) {
                throw "presented frame counter regressed from $lastPresentedFrames to $presentedFrames"
            }
            $lastPresentedFrames = $presentedFrames
            $workerProcess = Get-Process -Id $stableWorkerPid
            $runtimeProcesses = Get-RuntimeProcessSnapshot $stableWorkerPid
            # Windows Player presents the gfxstream-owned child HWND directly. A separate frame
            # producer is the legacy external-memory/copy route and must not run in the live
            # zero-copy topology (recording may use its own explicitly bounded capture path).
            if ($runtimeProcesses.processes | Where-Object name -eq "hd-frame-producer") {
                throw "legacy frame producer is present in the native direct-display runtime tree"
            }
            if (-not ($runtimeProcesses.processes | Where-Object name -eq "crosvm")) {
                throw "crosvm is missing from the live runtime process tree"
            }
            $logs = Get-StabilityLogSnapshot $InstanceId $stableRun
            $samples.Add([ordered]@{
                timestamp = [DateTimeOffset]::UtcNow.ToString("O")
                presented_frames = $sample.metrics.presented_frames
                host_present_latency_ns_total = $sample.metrics.host_present_latency_ns_total
                host_present_latency_ns_max = $sample.metrics.host_present_latency_ns_max
                host_present_over_16ms = $sample.metrics.host_present_over_16ms
                host_present_over_33ms = $sample.metrics.host_present_over_33ms
                worker_handles = $workerProcess.HandleCount
                worker_private_bytes = $workerProcess.PrivateMemorySize64
                runtime_process_count = $runtimeProcesses.process_count
                native_display_direct = $true
                legacy_frame_producer_present = $false
                runtime_handles = $runtimeProcesses.handles
                runtime_private_bytes = $runtimeProcesses.private_bytes
                runtime_processes = $runtimeProcesses.processes
                log_file_count = $logs.file_count
                log_bytes = $logs.bytes
            })
            Write-Host "stability sample $($samples.Count) passed"
        }

        if ($samples.Count -lt 2) {
            throw "stability validation produced fewer than two samples"
        }
        $firstSample = $samples[0]
        $lastSample = $samples[$samples.Count - 1]
        $workerHandleGrowth = [long]$lastSample.worker_handles - [long]$firstSample.worker_handles
        $runtimeHandleGrowth = [long]$lastSample.runtime_handles - [long]$firstSample.runtime_handles
        $workerPrivateGrowth = [long]$lastSample.worker_private_bytes - [long]$firstSample.worker_private_bytes
        $runtimePrivateGrowth = [long]$lastSample.runtime_private_bytes - [long]$firstSample.runtime_private_bytes
        if ($workerHandleGrowth -gt $MaxWorkerHandleGrowth) {
            throw "worker handle growth $workerHandleGrowth exceeds limit $MaxWorkerHandleGrowth"
        }
        if ($runtimeHandleGrowth -gt $MaxRuntimeHandleGrowth) {
            throw "runtime handle growth $runtimeHandleGrowth exceeds limit $MaxRuntimeHandleGrowth"
        }
        if ($workerPrivateGrowth -gt ([long]$MaxWorkerPrivateGrowthMiB * 1MB)) {
            throw "worker private-byte growth $workerPrivateGrowth exceeds limit $MaxWorkerPrivateGrowthMiB MiB"
        }
        if ($runtimePrivateGrowth -gt ([long]$MaxRuntimePrivateGrowthMiB * 1MB)) {
            throw "runtime private-byte growth $runtimePrivateGrowth exceeds limit $MaxRuntimePrivateGrowthMiB MiB"
        }
        if ([int]$lastSample.runtime_process_count -ne [int]$firstSample.runtime_process_count) {
            throw "runtime process count changed from $($firstSample.runtime_process_count) to $($lastSample.runtime_process_count)"
        }
        if ([long]$lastSample.log_bytes -gt ([long]$MaxStabilityLogMiB * 1MB)) {
            throw "stability logs reached $($lastSample.log_bytes) bytes and exceed limit $MaxStabilityLogMiB MiB"
        }
    }
} catch {
    $failure = $_.Exception.Message
    $failureStack = $_.ScriptStackTrace
} finally {
    if ($isolated -and $null -ne $failure) {
        try {
            Save-FailureRunEvidence
        } catch {
            $failure = "$failure; failure evidence capture failed: $($_.Exception.Message)"
        }
    }
    if ($null -ne $uiProcess -and -not $uiProcess.HasExited) {
        try {
            if ($uiProcess.CloseMainWindow()) {
                [void]$uiProcess.WaitForExit(10000)
            }
        } catch { }
        if (-not $uiProcess.HasExited) {
            Stop-Process -Id $uiProcess.Id -Force -ErrorAction SilentlyContinue
            [void]$uiProcess.WaitForExit(5000)
            if ($null -eq $failure) {
                $failure = "isolated HD UI required forced cleanup"
            }
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($uiRuntimeRoot) -and
        (Test-Path -LiteralPath $uiRuntimeRoot -PathType Container)) {
        try {
            $resolvedOutput = [IO.Path]::GetFullPath($outputRoot).TrimEnd(
                [IO.Path]::DirectorySeparatorChar,
                [IO.Path]::AltDirectorySeparatorChar)
            $resolvedUiRuntime = [IO.Path]::GetFullPath($uiRuntimeRoot)
            $expectedUiRuntime = Join-Path $resolvedOutput "ui-runtime"
            if (-not $resolvedUiRuntime.Equals(
                $expectedUiRuntime,
                [StringComparison]::OrdinalIgnoreCase)) {
                throw "refusing to remove an unexpected UI runtime path: $resolvedUiRuntime"
            }
            $uiCleanupDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
            do {
                try {
                    Remove-Item -LiteralPath $resolvedUiRuntime -Recurse -Force
                } catch {
                    if ([DateTimeOffset]::UtcNow -ge $uiCleanupDeadline) {
                        throw
                    }
                    Start-Sleep -Milliseconds 250
                }
            } while (Test-Path -LiteralPath $resolvedUiRuntime)
        } catch {
            if ($null -eq $failure) {
                $failure = "isolated UI runtime cleanup failed: $($_.Exception.Message)"
            }
        }
    }
    if ($null -ne $secondaryId) {
        try {
            $secondary = Get-Instance $secondaryId
            if ($secondary.status.desired -eq "running") {
                Invoke-Hdctl stop $secondaryId.ToString() --graceful-timeout-ms 20000 | Out-Null
            } else {
                $deadline = [DateTimeOffset]::UtcNow.AddMinutes(2)
                while ($secondary.status.observed -ne "stopped" -and [DateTimeOffset]::UtcNow -lt $deadline) {
                    Start-Sleep -Milliseconds 250
                    $secondary = Get-Instance $secondaryId
                }
            }
            Invoke-Hdctl delete $secondaryId.ToString() | Out-Null
        } catch { }
    }
    if ($isolated -and $created) {
        try {
            $current = Get-Instance $InstanceId
            if ($current.status.observed -notin @("defined", "stopped", "deleted")) {
                Invoke-Hdctl stop $InstanceId.ToString() --force | Out-Null
                $current = Get-Instance $InstanceId
            }
            if ($current.status.observed -ne "deleted") {
                Invoke-Hdctl delete $InstanceId.ToString() | Out-Null
            }
        } catch {
            if ($null -eq $failure) { $failure = "isolated instance cleanup failed: $($_.Exception.Message)" }
        }
    }
    if ($isolated -and $null -ne $hostProcess) {
        try {
            Invoke-Hdctl shutdown --stop-all | Out-Null
        } catch {
            if ($null -eq $failure) { $failure = "isolated Host shutdown failed: $($_.Exception.Message)" }
        }
        try {
            [void]$hostProcess.WaitForExit(10000)
        } catch {
            if ($null -eq $failure) { $failure = "isolated Host did not exit after shutdown" }
        }
        if (-not $hostProcess.HasExited) {
            Stop-Process -Id $hostProcess.Id -Force -ErrorAction SilentlyContinue
        }
        Start-Sleep -Milliseconds 250
    }
    $remaining = @()
    if ($isolated) {
        # WebView2 and the contained runtime may finish asynchronously after their owning UI/Host
        # process exits. Give only processes scoped to this isolated data root a bounded natural
        # exit window; never terminate or count unrelated desktop processes.
        $remainingDeadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
        do {
            $remaining = @(Get-ScopedRuntimeProcesses)
            if ($remaining.Count -gt 0 -and [DateTimeOffset]::UtcNow -lt $remainingDeadline) {
                Start-Sleep -Milliseconds 250
            }
        } while ($remaining.Count -gt 0 -and
            [DateTimeOffset]::UtcNow -lt $remainingDeadline)
    }
    if ($isolated -and $remaining.Count -ne 0 -and $null -eq $failure) {
        $failure = "isolated Windows Android processes remain after shutdown: $($remaining.Name -join ', ')"
    }
    $evidence = [ordered]@{
        schema_version = 2
        platform = "windows"
        architecture = "x86_64"
        generated_at = [DateTimeOffset]::UtcNow.ToString("O")
        started_at = $startedAt.ToString("O")
        status = if ($null -eq $failure) { "pass" } else { "fail" }
        error = $failure
        error_stack = $failureStack
        instance_id = $InstanceId.ToString()
        guest_bundle_digest = $guestDigest
        host_bundle_digest = $hostDigest
        isolated = $isolated
        request = [ordered]@{
            run_actions = [bool]$RunActions
            run_ui_display_input = [bool]$RunUiDisplayInput
            run_screen_recording = [bool]$RunScreenRecording
            guest_settle_seconds = $GuestSettleSeconds
            guest_cpu_count = $GuestCpuCount
            guest_memory_mib = $GuestMemoryMiB
            secondary_display_count = $SecondaryDisplayCount
            dev_fast_artifacts = $DevFastArtifacts
            aggregate_guest_image = if ($DevFastArtifacts) {
                (Resolve-Path -LiteralPath $env:HD_DEV_GUEST_ROOTFS).Path
            } else {
                $null
            }
            aggregate_guest_image_size_bytes = if ($DevFastArtifacts) {
                [long](Get-Item -LiteralPath $env:HD_DEV_GUEST_ROOTFS).Length
            } else {
                $null
            }
        }
        artifact_root = if ($DevFastArtifacts) { (Resolve-Path -LiteralPath $ArtifactRoot).Path } else { $null }
        dist_root = (Resolve-Path -LiteralPath $DistRoot).Path
        adb_executable = (Resolve-Path -LiteralPath $Adb).Path
        adb_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $Adb).Hash.ToLowerInvariant()
        aapt2_executable = if ([string]::IsNullOrWhiteSpace($Aapt2)) {
            $null
        } else {
            (Resolve-Path -LiteralPath $Aapt2).Path
        }
        aapt2_sha256 = if ([string]::IsNullOrWhiteSpace($Aapt2)) {
            $null
        } else {
            (Get-FileHash -Algorithm SHA256 -LiteralPath $Aapt2).Hash.ToLowerInvariant()
        }
        packaged_tooling = $packagedTooling
        runtime_closure = [ordered]@{
            manifest = $runtimeClosureManifestPath
            manifest_sha256 = $runtimeClosureManifestSha256
            file_count = $runtimeClosureEntries.Count
            files = $runtimeClosureEntries
        }
        data_root = (Resolve-Path -LiteralPath $DataRoot).Path
        host_pid = if ($null -ne $hostProcess) { $hostProcess.Id } else { $null }
        host_stdout = if ($isolated) { $hostStdout } else { $null }
        host_stderr = if ($isolated) { $hostStderr } else { $null }
        remaining_process_count = $remaining.Count
        direct_artifact_selection = $directSelection
        capability_and_bundle_revalidation = if ($DevFastArtifacts) {
            "skipped-by-design"
        } else {
            "performed-by-host"
        }
        dev_fast_artifacts = $DevFastArtifacts
        actions = $actions
        action_readbacks = $actionReadbacks
        skipped_controls = $skippedControls
        location_framework_callback = $locationProbeEvidence
        location_route_framework_callback = $locationRouteEvidence
        screen_recording = $screenRecordingEvidence
        backend_windows = $backendWindowEvidence
        multi_display = $multiDisplayEvidence
        titlebar_display_input = $uiDisplayInputEvidence
        targeted_android_readiness = $targetedReadinessEvidence
        readiness_timeline = $readinessTimeline
        display_selection = $displaySelectionEvidence
        adb_loss_power_fallback = $adbLossPowerEvidence
        uwb_fira = $uwbFiraEvidence
        android_bugreport = $bugreportEvidence
        lifecycle = [ordered]@{ requested = $LifecycleCycles; completed = $cycles.Count; cycles = $cycles }
        stability = [ordered]@{
            requested_minutes = $StabilityMinutes
            completed_samples = $samples.Count
            static_frame_quiescence_allowed = $true
            samples = $samples
        }
    }
    $evidence | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $evidencePath -Encoding utf8NoBOM
    Write-Host "evidence=$evidencePath"
}

if ($null -ne $failure) { throw $failure }
