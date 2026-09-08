@echo off
chcp 65001 >nul
set PYTHONUTF8=1
cd /d "%~dp0"
where py >nul 2>nul
if not errorlevel 1 (
    py -3 tools\start_gallery.py %*
) else (
    python tools\start_gallery.py %*
)
if errorlevel 1 (
    echo Startup failed. Install Python 3 and Docker, then check the error above.
    pause
    exit /b 1
)
