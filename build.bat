@echo off
setlocal EnableExtensions

set "HD_ROOT=%~dp0"
if /I "%~1"=="--help" (
    echo Usage: %~nx0
    echo   Validates the AI process contract, builds HD for x86_64-pc-windows-gnu, and audits PE imports.
    exit /b 0
)
if not "%~1"=="" (
    echo Error: unknown argument: %~1 1>&2
    exit /b 2
)
if not defined RUST_TARGET set "RUST_TARGET=x86_64-pc-windows-gnu"
if /I not "%RUST_TARGET%"=="x86_64-pc-windows-gnu" (
    echo Error: HD Windows builds require RUST_TARGET=x86_64-pc-windows-gnu. 1>&2
    exit /b 2
)

if not defined MINGW_PATH if exist "C:\workspace\mingw64\bin\gcc.exe" set "MINGW_PATH=C:\workspace\mingw64"
if not defined MINGW_PATH if exist "C:\tools\mingw64\bin\gcc.exe" set "MINGW_PATH=C:\tools\mingw64"
if not defined MINGW_PATH if exist "C:\msys64\mingw64\bin\gcc.exe" set "MINGW_PATH=C:\msys64\mingw64"
if not defined MINGW_PATH if exist "C:\mingw-w64\mingw64\bin\gcc.exe" set "MINGW_PATH=C:\mingw-w64\mingw64"
if not defined MINGW_PATH (
    echo Error: MinGW-w64 was not found. Set MINGW_PATH to the toolchain root. 1>&2
    exit /b 2
)
if not exist "%MINGW_PATH%\bin\gcc.exe" (
    echo Error: invalid MINGW_PATH: %MINGW_PATH% 1>&2
    exit /b 2
)

set "PATH=%MINGW_PATH%\bin;%PATH%"
set "CC=%MINGW_PATH%\bin\gcc.exe"
set "CXX=%MINGW_PATH%\bin\g++.exe"
set "AR=%MINGW_PATH%\bin\ar.exe"
set "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=%MINGW_PATH%\bin\gcc.exe"
set "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_AR=%MINGW_PATH%\bin\ar.exe"

where npm.cmd >nul 2>&1
if errorlevel 1 (
    echo Error: npm.cmd was not found. Node.js is required to build the HD WebView UI. 1>&2
    exit /b 2
)

pushd "%HD_ROOT%web"
if not exist "node_modules\.package-lock.json" (
    call npm.cmd ci
    if errorlevel 1 (
        popd
        exit /b 1
    )
)
call npm.cmd run build
if errorlevel 1 (
    popd
    exit /b 1
)
popd

echo Building HD WebView shell and runtime with %CC% for %RUST_TARGET%

cargo run --manifest-path "%HD_ROOT%Cargo.toml" --target "%RUST_TARGET%" -p xtask -- process-check
if errorlevel 1 exit /b 1

cargo build --manifest-path "%HD_ROOT%Cargo.toml" --workspace --bins --release --target "%RUST_TARGET%"
if errorlevel 1 exit /b 1

cargo run --manifest-path "%HD_ROOT%Cargo.toml" --target "%RUST_TARGET%" -p xtask -- pe-audit ^
    --bin-dir "%HD_ROOT%target\%RUST_TARGET%\release" ^
    --objdump "%MINGW_PATH%\bin\objdump.exe"
exit /b %ERRORLEVEL%
