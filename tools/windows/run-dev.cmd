@echo off
setlocal

cd /d "%~dp0\..\.."

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup-dev.ps1" run
set EXIT_CODE=%ERRORLEVEL%

if not "%EXIT_CODE%"=="0" (
  echo.
  echo Avalon Mapper Windows dev startup failed with exit code %EXIT_CODE%.
  echo Keep this window open and copy the error above.
  pause
)

exit /b %EXIT_CODE%
