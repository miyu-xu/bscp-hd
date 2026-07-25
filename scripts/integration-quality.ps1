[CmdletBinding()]
param(
    [string]$Task = "automation/tasks/workspace-integration.json",
    [string]$Output = "out/ai/integration",
    [string]$MingwRoot = "C:\workspace\mingw64",
    [string]$CrosvmFeatures = "whpx,composite-disk,android-sparse,net,slirp,balloon,gpu,gfxstream,vulkan_display,vulkano",
    [string]$Android17GfxstreamRun = "",
    [switch]$RunRootBuild,
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$hdRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$workspaceRoot = Split-Path -Parent $hdRoot
$taskPath = if ([IO.Path]::IsPathRooted($Task)) { $Task } else { Join-Path $hdRoot $Task }
$taskPath = (Resolve-Path $taskPath).Path
$outputPath = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $hdRoot $Output }
$logsPath = Join-Path $outputPath "logs"
[void](New-Item -ItemType Directory -Force -Path $logsPath)

$mingwBin = Join-Path $MingwRoot "bin"
$gcc = Join-Path $mingwBin "gcc.exe"
$make = Join-Path $mingwBin "mingw32-make.exe"
$objdump = Join-Path $mingwBin "objdump.exe"
$cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
$git = (Get-Command git.exe -ErrorAction Stop).Source
$taskDocument = Get-Content -LiteralPath $taskPath -Raw | ConvertFrom-Json
$requiredGates = @($taskDocument.required_gates)

if (-not $PlanOnly) {
    foreach ($required in @($gcc, $make, $objdump)) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
            throw "Required MinGW tool is missing: $required"
        }
    }
}

$env:PATH = "$mingwBin;$env:PATH"
$env:CC = $gcc
$env:CXX = Join-Path $mingwBin "g++.exe"
$env:AR = Join-Path $mingwBin "ar.exe"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $gcc

function Invoke-Gate {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Executable,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$WorkingDirectory
    )

    $commandText = "$Executable $($Arguments -join ' ')"
    $logRelative = "logs/$Name.log"
    $logPath = Join-Path $outputPath $logRelative
    [Console]::WriteLine("[$Name] $commandText")

    if ($PlanOnly) {
        [IO.File]::WriteAllText(
            $logPath,
            "plan-only: $commandText`nworking-directory: $WorkingDirectory`n",
            [Text.UTF8Encoding]::new($false)
        )
        return [pscustomobject][ordered]@{
            name = $Name
            command = $commandText
            status = "skipped"
            duration_ms = 0
            log_path = $logRelative
            summary = "plan-only"
        }
    }

    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $Executable
        $startInfo.WorkingDirectory = $WorkingDirectory
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        foreach ($argument in $Arguments) {
            [void]$startInfo.ArgumentList.Add($argument)
        }

        $process = [Diagnostics.Process]::new()
        $process.StartInfo = $startInfo
        [void]$process.Start()
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $stopwatch.Stop()

        $body = "command: $commandText`nexit: $($process.ExitCode)`n`n[stdout]`n$stdout`n[stderr]`n$stderr"
        [IO.File]::WriteAllText($logPath, $body, [Text.UTF8Encoding]::new($false))
        $status = if ($process.ExitCode -eq 0) { "pass" } else { "fail" }
        return [pscustomobject][ordered]@{
            name = $Name
            command = $commandText
            status = $status
            duration_ms = [long]$stopwatch.ElapsedMilliseconds
            log_path = $logRelative
            summary = "exit $($process.ExitCode)"
        }
    }
    catch {
        $stopwatch.Stop()
        [IO.File]::WriteAllText(
            $logPath,
            "command: $commandText`nstart error: $($_.Exception.Message)`n",
            [Text.UTF8Encoding]::new($false)
        )
        return [pscustomobject][ordered]@{
            name = $Name
            command = $commandText
            status = "fail"
            duration_ms = [long]$stopwatch.ElapsedMilliseconds
            log_path = $logRelative
            summary = "failed to start: $($_.Exception.Message)"
        }
    }
}

$records = [Collections.Generic.List[object]]::new()
if ($requiredGates -contains "angle-runtime-probe") {
    $records.Add((Invoke-Gate `
        -Name "angle-runtime-probe" `
        -Executable (Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe") `
        -Arguments @(
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
            (Join-Path $workspaceRoot "scripts\check_angle_runtime.ps1"),
            "-RuntimeDir", (Join-Path $workspaceRoot "out\dist\windows\gfx\angle")
        ) `
        -WorkingDirectory $workspaceRoot))
}
if ($requiredGates -contains "android17-real-guest") {
    if (-not $Android17GfxstreamRun) {
        throw "-Android17GfxstreamRun is required for the android17-real-guest gate"
    }
    $records.Add((Invoke-Gate `
        -Name "android17-real-guest" `
        -Executable (Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe") `
        -Arguments @(
            "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
            (Join-Path $workspaceRoot "scripts\check_android17_gfxstream_evidence.ps1"),
            "-RunDir", $Android17GfxstreamRun,
            "-ResultPath", (Join-Path $outputPath "android17-gfxstream-result.json")
        ) `
        -WorkingDirectory $workspaceRoot))
}
$records.Add((Invoke-Gate `
    -Name "hd-quality" `
    -Executable $cargo `
    -Arguments @("run", "--target", "x86_64-pc-windows-gnu", "-p", "xtask", "--", "quality") `
    -WorkingDirectory $hdRoot))

$gfxstreamBuild = Join-Path $workspaceRoot "out\gfxstream_build_windows"
$records.Add((Invoke-Gate `
    -Name "gfxstream-mingw-build" `
    -Executable $make `
    -Arguments @("-C", $gfxstreamBuild, "gfxstream_backend", "-j4") `
    -WorkingDirectory $workspaceRoot))

# rutabaga_gfx uses GFXSTREAM_PATH as the explicit Windows GNU link contract. Without it,
# its build script falls back to pkg-config, which correctly rejects a host/target cross query.
# Build the backend first so the crosvm gates validate the same artifact used by build_all.bat.
$env:GFXSTREAM_PATH = $gfxstreamBuild

$crosvmRoot = Join-Path $workspaceRoot "external\crosvm"
$crosvmArguments = @(
    "+stable", "check", "-p", "crosvm",
    "--target", "x86_64-pc-windows-gnu",
    "--features", $CrosvmFeatures,
    "--offline"
)
$records.Add((Invoke-Gate `
    -Name "crosvm-windows-gnu-check" `
    -Executable $cargo `
    -Arguments $crosvmArguments `
    -WorkingDirectory $crosvmRoot))
$records.Add((Invoke-Gate `
    -Name "crosvm-test-targets-compile" `
    -Executable $cargo `
    -Arguments ($crosvmArguments + "--tests") `
    -WorkingDirectory $crosvmRoot))
$records.Add((Invoke-Gate `
    -Name "crosvm-diff-check" `
    -Executable $git `
    -Arguments @("diff", "--check") `
    -WorkingDirectory $crosvmRoot))

$records.Add((Invoke-Gate `
    -Name "gfxstream-diff-check" `
    -Executable $git `
    -Arguments @("diff", "--check") `
    -WorkingDirectory (Join-Path $workspaceRoot "hardware\google\gfxstream")))

if ($RunRootBuild) {
    $records.Add((Invoke-Gate `
        -Name "root-build-all" `
        -Executable (Join-Path $env:SystemRoot "System32\cmd.exe") `
        -Arguments @("/d", "/c", "build_all.bat") `
        -WorkingDirectory $workspaceRoot))
}

$gateReportPath = Join-Path $outputPath "integration-gates.json"
$gateReport = [ordered]@{
    schema_version = 2
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    source = "scripts/integration-quality.ps1"
    gates = $records
}
$gateJson = $gateReport | ConvertTo-Json -Depth 8
[IO.File]::WriteAllText($gateReportPath, "$gateJson`n", [Text.UTF8Encoding]::new($false))

if ($PlanOnly) {
    Write-Host "Integration plan written to $gateReportPath"
    exit 0
}

& $cargo run --target x86_64-pc-windows-gnu -p xtask -- readback `
    --task $taskPath `
    --output $outputPath `
    --gate-report $gateReportPath
$readbackExit = $LASTEXITCODE
$failed = @($records | Where-Object { $_.status -ne "pass" })
if ($readbackExit -ne 0) {
    throw "Readback generation failed with exit code $readbackExit"
}
if ($failed.Count -ne 0) {
    throw "$($failed.Count) integration gate(s) failed; inspect $outputPath"
}

Write-Host "HD workspace integration passed; readback: $outputPath"
