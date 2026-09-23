@echo off
rem cargo-win.bat — run cargo with the MSVC (vcvars64) environment loaded.
rem
rem Why: Git Bash ships its own /usr/bin/link.exe (coreutils hard-link tool),
rem which shadows the MSVC linker and breaks every rust `windows-msvc` build
rem ("link: extra operand …"). Loading vcvars64.bat inside cmd gives rustc the
rem real linker + INCLUDE/LIB paths.
rem
rem Usage:  cargo-win.bat check [more cargo args...]
setlocal

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "VS_PATH="
if exist "%VSWHERE%" (
  for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VS_PATH=%%i"
)
if not defined VS_PATH set "VS_PATH=%ProgramFiles%\Microsoft Visual Studio\2022\Community"

call "%VS_PATH%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if errorlevel 1 (
  echo [cargo-win] vcvars64.bat failed; is the VS C++ workload installed? 1>&2
  exit /b 1
)

cd /d "%~dp0src-tauri"
"%USERPROFILE%\.cargo\bin\cargo.exe" %*
