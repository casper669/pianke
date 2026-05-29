@echo off
REM Pianke launcher for Windows.
REM
REM IMPORTANT: this file is intentionally ASCII-only. Chinese Windows cmd.exe
REM reads .bat files using the OEM code page (usually GBK/936), and UTF-8
REM multibyte sequences get mis-parsed, breaking long lines into fragments.
REM All Chinese user-facing strings live in scripts/launcher.py instead,
REM which Python handles correctly regardless of console code page.
REM
REM First double-click may show "Windows protected your PC":
REM   click "More info" -> "Run anyway". Won't show again.

setlocal enableextensions enabledelayedexpansion
chcp 65001 >nul 2>&1

cd /d "%~dp0"

echo.
echo ============================================================
echo   Pianke launcher
echo ============================================================

REM ---- 1. find or install uv ----
set "UV="
where uv >nul 2>&1 && set "UV=uv"

if not defined UV (
  if exist "%USERPROFILE%\.local\bin\uv.exe" set "UV=%USERPROFILE%\.local\bin\uv.exe"
)
if not defined UV (
  if exist "%USERPROFILE%\.cargo\bin\uv.exe" set "UV=%USERPROFILE%\.cargo\bin\uv.exe"
)

if not defined UV (
  echo.
  echo [first-run setup] Downloading uv ^(Python toolchain, ~30MB^)...
  echo   This step only happens once.
  powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://astral.sh/uv/install.ps1 | iex"
  if errorlevel 1 (
    echo.
    echo [ERROR] uv install failed.
    echo         Common cause: astral.sh CDN is overseas, network unstable.
    echo         Retry later, or manually run in PowerShell:
    echo           irm https://astral.sh/uv/install.ps1 ^| iex
    echo.
    pause
    exit /b 1
  )
  REM re-probe after install
  if exist "%USERPROFILE%\.local\bin\uv.exe" set "UV=%USERPROFILE%\.local\bin\uv.exe"
  if not defined UV (
    if exist "%USERPROFILE%\.cargo\bin\uv.exe" set "UV=%USERPROFILE%\.cargo\bin\uv.exe"
  )
  echo   [OK] uv installed
)

if not defined UV (
  echo.
  echo [ERROR] uv installed but executable not found.
  echo         Close this window and double-click the launcher again.
  echo.
  pause
  exit /b 1
)

set "PATH=%USERPROFILE%\.local\bin;%USERPROFILE%\.cargo\bin;%PATH%"

REM ---- 2. run launcher.py via uv (uv will fetch a managed Python if needed) ----
echo.
echo Preparing Python environment...
"%UV%" run --no-project --python ">=3.10" -- python scripts\launcher.py
set "RC=%errorlevel%"

if not "%RC%"=="0" (
  echo.
  echo [launcher exited with code %RC%]
  pause
)
endlocal & exit /b %RC%
