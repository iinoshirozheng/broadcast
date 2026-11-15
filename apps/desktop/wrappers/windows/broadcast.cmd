@echo off
setlocal
set "BROADCAST_RUNTIME_FILE=%APPDATA%\Broadcast\launcher-runtime.json"
"%~dp0..\broadcast-cli.exe" %*
exit /b %ERRORLEVEL%
