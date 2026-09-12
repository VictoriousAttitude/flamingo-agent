@echo off
set "last="
:loop
if "%~1"=="" goto done
set "last=%~1"
shift
goto loop
:done
type nul > "%last%"
