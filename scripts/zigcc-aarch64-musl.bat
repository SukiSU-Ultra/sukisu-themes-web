@echo off
setlocal enabledelayedexpansion
set "out="
for %%a in (%*) do (
  set "arg=%%~a"
  if /I not "!arg:~0,9!"=="--target=" (
    set out=!out! "%%~a"
  )
)
zig cc -target aarch64-linux-musl !out!
