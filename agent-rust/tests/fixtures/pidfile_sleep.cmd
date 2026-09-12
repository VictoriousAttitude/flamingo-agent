@echo off
powershell -NoProfile -Command "$PID | Out-File -Encoding ascii -NoNewline '%~1'; Start-Sleep 10"
