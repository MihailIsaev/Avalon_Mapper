@echo off
setlocal

cd /d "%~dp0..\.."

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup-dev.ps1" build-installer
set EXIT_CODE=%ERRORLEVEL%

if not "%EXIT_CODE%"=="0" (
  echo Avalon Mapper Windows installer build failed with exit code %EXIT_CODE%.
  pause
  exit /b %EXIT_CODE%
)

echo.
echo Installer was written to dist\windows\AvalonMapperSetup.exe
pause
