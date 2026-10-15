@echo off
rem Bazel's workspace status on Windows (see workspace_status.sh).
setlocal
rem The top of a checkout prints no way up; outside one, git fails.
git rev-parse --show-cdup >nul 2>&1 || exit /b 0
for /f %%u in ('git rev-parse --show-cdup') do exit /b 0
for /f %%c in ('git rev-parse --short^=12 HEAD 2^>nul') do set commit=%%c
if not defined commit exit /b 0
git --no-optional-locks status --porcelain --untracked-files=no >nul 2>&1 || set dirty=.dirty
for /f %%d in ('git --no-optional-locks status --porcelain --untracked-files^=no 2^>nul') do set dirty=.dirty
echo STABLE_GIT_COMMIT %commit%%dirty%
exit /b 0
