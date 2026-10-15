@echo off
rem Bazel's workspace status on Windows (see workspace_status.sh).
for /f %%c in ('git rev-parse --short^=12 HEAD 2^>nul') do echo STABLE_GIT_COMMIT %%c
exit /b 0
