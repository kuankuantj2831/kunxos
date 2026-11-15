@echo off
setlocal
cd /d "%~dp0"
title KunxOS - QEMU
where python >nul 2>nul
if errorlevel 1 (
    echo Python was not found. Install Python 3 and add it to PATH.
    pause
    exit /b 1
)
if exist "build\windows\target\riscv64gc-unknown-none-elf\release\rust-kernel" (
    python "tools\run_windows.py" %*
) else (
    echo Building KunxOS for the first launch. Please wait...
    python "tools\run_windows.py" --build %*
)
if errorlevel 1 (
    echo KunxOS could not start. See qemu-windows-serial.error.log.
    pause
    exit /b 1
)
endlocal
