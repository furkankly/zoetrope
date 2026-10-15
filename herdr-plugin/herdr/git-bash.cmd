@echo off
rem Windows only: run one of the plugin's bash scripts with Git for Windows' bash.
rem
rem A bare `bash` on Windows is usually C:\Windows\System32\bash.exe, which is
rem WSL: the script would run inside Linux, without Herdr's environment, against
rem a `herdr` that is not there. The scripts are written for the bash Git for
rem Windows ships, so this finds that one. Its bin\bash.exe, not
rem usr\bin\bash.exe, because only the former puts Git's awk, grep and cp on PATH.
rem
rem Batch rather than PowerShell because it sits on the path of every key press,
rem and powershell.exe takes half a second to start where cmd takes a few
rem milliseconds. No labels and no goto: the file is LF, and cmd finds labels
rem unreliably in an LF file.
setlocal
set "bash="

rem The installer records where it put Git, per machine or per user.
for %%k in (HKLM HKCU) do if not defined bash for /f "tokens=2,*" %%a in ('reg query "%%k\SOFTWARE\GitForWindows" /v InstallPath 2^>nul') do if /i "%%a"=="REG_SZ" if exist "%%b\bin\bash.exe" set "bash=%%b\bin\bash.exe"

rem Otherwise ask the git on PATH (Scoop, a portable install), whose exec path
rem is <root>/mingw64/libexec/git-core.
if not defined bash for /f "delims=" %%c in ('git --exec-path 2^>nul') do for %%r in ("%%c\..\..\..") do if exist "%%~fr\bin\bash.exe" set "bash=%%~fr\bin\bash.exe"

rem The exit code is read on its own line: a bare `exit /b`, or one on the same
rem line as the command, reports 0 to `cmd /c` whatever the script returned.
if defined bash "%bash%" %*
if defined bash exit /b %errorlevel%

rem Nothing to run the script with. Say so where it will be seen: in the pane,
rem held until enter like every other failure there, and otherwise as a
rem notification, since an action's output only reaches `herdr plugin log`.
set "msg=zoetrope runs its scripts with Git for Windows' bash and found none: no install in the registry, and no git on PATH that ships one. Install it (winget install Git.Git) and try again."
>&2 echo.
>&2 echo   %msg%
>&2 echo.
if defined HERDR_PLUGIN_ENTRYPOINT_ID set /p "_=  press enter to close "
if not defined HERDR_PLUGIN_ENTRYPOINT_ID if defined HERDR_BIN_PATH "%HERDR_BIN_PATH%" notification show "zoetrope" --body "%msg%" --sound none >nul 2>&1
exit /b 1
