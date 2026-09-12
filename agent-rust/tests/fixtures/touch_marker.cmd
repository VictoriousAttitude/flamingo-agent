@echo off
rem Records that this child ran: the log path is the sixth argument.
for %%I in ("%~6") do md "%%~dpI" 2>nul
type nul > "%~6"
