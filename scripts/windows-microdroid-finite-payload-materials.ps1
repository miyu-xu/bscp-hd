[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SkeletonApk,
    [ValidateSet("x86_64", "arm64-v8a")]
    [string]$Abi = "x86_64",
    [string]$Output = "out\windows-microdroid-finite-payload-materials",
    [string]$NdkRoot = "$env:LOCALAPPDATA\Android\Sdk\ndk\29.0.14206865",
    [string]$BuildTools = "$env:LOCALAPPDATA\Android\Sdk\build-tools\37.0.0",
    [string]$SevenZip = "C:\Program Files\7-Zip\7z.exe",
    [string]$OpenSsl = "C:\Program Files\Git\usr\bin\openssl.exe",
    [string]$JavaHome = "C:\Program Files\Android\Android Studio\jbr"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$outputRoot = if ([IO.Path]::IsPathRooted($Output)) {
    [IO.Path]::GetFullPath($Output)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $Output))
}
$skeleton = (Resolve-Path -LiteralPath $SkeletonApk -ErrorAction Stop).Path
$clang = Join-Path $NdkRoot "toolchains\llvm\prebuilt\windows-x86_64\bin\clang.exe"
$zipAlign = Join-Path $BuildTools "zipalign.exe"
$apkSigner = Join-Path $BuildTools "apksigner.bat"
$java = Join-Path $JavaHome "bin\java.exe"
$compilerTarget = if ($Abi -eq "x86_64") { "x86_64-linux-android35" } else { "aarch64-linux-android35" }
$artifactAbi = if ($Abi -eq "x86_64") { "x86_64" } else { "arm64" }
foreach ($required in @($skeleton, $clang, $zipAlign, $apkSigner, $SevenZip, $OpenSsl, $java)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "required finite Payload material input is missing: $required"
    }
}
if (Test-Path -LiteralPath $outputRoot) {
    throw "refusing to overwrite finite Payload material output: $outputRoot"
}
New-Item -ItemType Directory -Path $outputRoot | Out-Null
$work = Join-Path $outputRoot ".work"
New-Item -ItemType Directory -Path $work | Out-Null

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )
    & $FilePath @Arguments 2>&1 | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath failed with exit code $LASTEXITCODE"
    }
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Build-Payload {
    param(
        [Parameter(Mandatory = $true)][int]$ExitCode,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$SigningKey,
        [Parameter(Mandatory = $true)][string]$SigningCertificate
    )
    $libraryName = "HdFinitePayload$Name.so"
    $source = Join-Path $work "$Name.c"
    $library = Join-Path $work $libraryName
    $stage = Join-Path $work "stage-$Name"
    $unsigned = Join-Path $work "$Name-unsigned.apk"
    $aligned = Join-Path $work "$Name-aligned.apk"
    $outputApk = Join-Path $outputRoot "microdroid-finite-$($Name.ToLowerInvariant())-$artifactAbi-v3.apk"

    [IO.File]::WriteAllText(
        $source,
        "__attribute__((visibility(`"default`"))) int AVmPayload_main(void) { return $ExitCode; }`n",
        [Text.UTF8Encoding]::new($false)
    )
    Invoke-Native -FilePath $clang -Arguments @(
        "--target=$compilerTarget",
        "-shared",
        "-fPIC",
        "-nostdlib",
        "-Wl,--no-undefined",
        "-Wl,-soname,$libraryName",
        "-o", $library,
        $source
    )

    New-Item -ItemType Directory -Path $stage | Out-Null
    Invoke-Native -FilePath $SevenZip -Arguments @("x", "-y", "-o$stage", $skeleton)
    $signatureDirectory = Join-Path $stage "META-INF"
    if (Test-Path -LiteralPath $signatureDirectory -PathType Container) {
        [IO.Directory]::Delete($signatureDirectory, $true)
    }
    $abiDirectory = Join-Path $stage "lib\$Abi"
    if (Test-Path -LiteralPath $abiDirectory -PathType Container) {
        [IO.Directory]::Delete($abiDirectory, $true)
    }
    New-Item -ItemType Directory -Path $abiDirectory | Out-Null
    Copy-Item -LiteralPath $library -Destination (Join-Path $abiDirectory $libraryName)
    $assetsDirectory = Join-Path $stage "assets"
    New-Item -ItemType Directory -Path $assetsDirectory -Force | Out-Null
    $config = [ordered]@{
        task = [ordered]@{
            type = "microdroid_launcher"
            command = $libraryName
        }
        export_tombstones = $true
    } | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText(
        (Join-Path $assetsDirectory "vm_config.json"),
        "$config`n",
        [Text.UTF8Encoding]::new($false)
    )

    Push-Location $stage
    try {
        Invoke-Native -FilePath $SevenZip -Arguments @("a", "-tzip", "-mx=0", $unsigned, ".\*")
    } finally {
        Pop-Location
    }
    Invoke-Native -FilePath $zipAlign -Arguments @("-p", "-f", "4", $unsigned, $aligned)
    Invoke-Native -FilePath $apkSigner -Arguments @(
        "sign",
        "--key", $SigningKey,
        "--cert", $SigningCertificate,
        "--v1-signing-enabled", "false",
        "--v2-signing-enabled", "false",
        "--v3-signing-enabled", "true",
        "--v4-signing-enabled", "false",
        "--out", $outputApk,
        $aligned
    )
    Invoke-Native -FilePath $apkSigner -Arguments @(
        "verify",
        "--verbose",
        "--print-certs",
        $outputApk
    )
    Invoke-Native -FilePath $zipAlign -Arguments @("-c", "-p", "4", $outputApk)

    return [ordered]@{
        exit_code = $ExitCode
        apk = [IO.Path]::GetFileName($outputApk)
        apk_size = (Get-Item -LiteralPath $outputApk).Length
        apk_sha256 = Get-Sha256 -Path $outputApk
        payload_library = $libraryName
    }
}

$originalJavaHome = $env:JAVA_HOME
$env:JAVA_HOME = $JavaHome
try {
    $privatePem = Join-Path $work "finite-payload-private.pem"
    $privateDer = Join-Path $work "finite-payload-private.pk8"
    $certificate = Join-Path $work "finite-payload-certificate.pem"
    Invoke-Native -FilePath $OpenSsl -Arguments @(
        "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", $privatePem
    )
    Invoke-Native -FilePath $OpenSsl -Arguments @(
        "req", "-new", "-x509", "-sha256", "-days", "7",
        "-key", $privatePem,
        "-subj", "/CN=HD Microdroid finite Payload development gate/",
        "-out", $certificate
    )
    Invoke-Native -FilePath $OpenSsl -Arguments @(
        "pkcs8", "-topk8", "-nocrypt", "-outform", "DER",
        "-in", $privatePem,
        "-out", $privateDer
    )

    $materials = @(
        Build-Payload -ExitCode 0 -Name "Exit0" -SigningKey $privateDer -SigningCertificate $certificate
        Build-Payload -ExitCode 17 -Name "Exit17" -SigningKey $privateDer -SigningCertificate $certificate
    )
    $publicCertificate = Join-Path $outputRoot "finite-payload-development-certificate.pem"
    Copy-Item -LiteralPath $certificate -Destination $publicCertificate
    $result = [ordered]@{
        schema_version = 1
        status = "pass"
        build_host = "windows-x86_64-gnu"
        abi = $Abi
        android_api = 35
        signing = [ordered]@{
            channel = "development-test-only"
            schemes = @("v3")
            certificate = [IO.Path]::GetFileName($publicCertificate)
            certificate_file_sha256 = Get-Sha256 -Path $publicCertificate
            private_key_retained = $false
        }
        skeleton = [ordered]@{
            path = $skeleton
            sha256 = Get-Sha256 -Path $skeleton
        }
        payloads = $materials
    }
    $resultPath = Join-Path $outputRoot "result.json"
    [IO.File]::WriteAllText(
        $resultPath,
        "$(ConvertTo-Json $result -Depth 6)`n",
        [Text.UTF8Encoding]::new($false)
    )
    Write-Output $resultPath
} finally {
    $env:JAVA_HOME = $originalJavaHome
    $expectedWork = Join-Path $outputRoot ".work"
    if ([IO.Path]::GetFullPath($work) -ne [IO.Path]::GetFullPath($expectedWork)) {
        throw "refusing to clean unexpected finite Payload work directory: $work"
    }
    if (Test-Path -LiteralPath $work -PathType Container) {
        [IO.Directory]::Delete($work, $true)
    }
}
