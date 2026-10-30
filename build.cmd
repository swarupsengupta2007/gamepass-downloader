@echo off
rem Build helper for gamepass-downloader. See AGENTS.md and UPSTREAM.md.
rem
rem   build            release build of target\release\gamepass-downloader.exe
rem   build all        also build target\release\xodus-cli.exe
rem   build test       run the workspace tests (known failures: UPSTREAM.md)
rem   build clean      remove target\
rem
rem Needs Rust 1.99.0 (MSVC), protoc and cmake on PATH, and Visual Studio
rem Build Tools with the Windows SDK (cl.exe and rc.exe are found without PATH).

setlocal
cd /d "%~dp0"

set "MODE=%~1"
if "%MODE%"=="" set "MODE=gui"
if /i "%MODE%"=="help" goto usage
if /i "%MODE%"=="/?" goto usage
if /i "%MODE%"=="clean" goto clean
if /i not "%MODE%"=="gui" if /i not "%MODE%"=="all" if /i not "%MODE%"=="test" goto usage

where cargo >nul 2>nul || (echo error: cargo not found. Install rustup and the 1.99.0 MSVC toolchain. & exit /b 1)
where protoc >nul 2>nul || if not defined PROTOC (echo error: protoc not found. The xodus crate runs prost-build; put protoc on PATH or set PROTOC. & exit /b 1)
where cmake >nul 2>nul || if not defined CMAKE (echo error: cmake not found. zlib-ng is built with CMake; put cmake on PATH or set CMAKE. & exit /b 1)

for /f "tokens=2" %%v in ('rustc --version') do set "RUSTC_VERSION=%%v"
if not "%RUSTC_VERSION%"=="1.99.0" echo warning: rustc %RUSTC_VERSION%; this project is built and tested with 1.99.0.

if /i "%MODE%"=="test" goto test

set "EXE=target\release\gamepass-downloader.exe"
rem A running window keeps its exe locked, and the link step would fail.
rem Windows still lets a running exe be renamed, so move it aside.
if exist "%EXE%.old" del "%EXE%.old" 2>nul
if exist "%EXE%" 2>nul (>>"%EXE%" call ) || (
    echo gamepass-downloader.exe is running; moving it to %EXE%.old
    move /y "%EXE%" "%EXE%.old" >nul || (echo error: could not move %EXE%. Close the app and run again. & exit /b 1)
)

set "PACKAGES=-p gamepass-gui"
if /i "%MODE%"=="all" set "PACKAGES=-p gamepass-gui -p xodus-cli"
cargo build --release --locked %PACKAGES%
if errorlevel 1 exit /b %errorlevel%

for %%f in ("%EXE%") do echo Built %%~ff (%%~zf bytes)
if /i "%MODE%"=="all" for %%f in ("target\release\xodus-cli.exe") do echo Built %%~ff (%%~zf bytes)
exit /b 0

:test
cargo test --release --workspace --no-fail-fast
exit /b %errorlevel%

:clean
cargo clean
exit /b %errorlevel%

:usage
echo usage: build [all ^| test ^| clean]
echo   (none)  release build of target\release\gamepass-downloader.exe
echo   all     also build target\release\xodus-cli.exe
echo   test    run the workspace tests
echo   clean   remove target\
exit /b 1
