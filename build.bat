@echo off
if "%~1"=="" (
    cargo xtask run
) else (
    cargo xtask %*
)
