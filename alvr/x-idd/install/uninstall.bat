@echo off
REM ============================================================================
REM x-idd rollback: remove the virtual display, drop the driver package, and
REM untrust/delete the self-signed cert.  One command, no reboot.
REM
REM Run this if the install misbehaves. SSH access survives a bad display
REM driver, so this is always reachable.
REM ============================================================================
setlocal
set LOG=C:\Gemlink\xidd-uninstall.txt
set CERTSUBJ=GemLink IDD Test
set DEVCON=C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe

> %LOG% echo === x-idd UNINSTALL %DATE% %TIME% ===

echo [1/4] removing the device...
>> %LOG% 2>&1 "%DEVCON%" remove Root\XIddDriver
>> %LOG% 2>&1 "%DEVCON%" remove XIddDriver

echo [2/4] deleting the driver package from the store...
>> %LOG% 2>&1 pnputil /delete-driver xidddriver.inf /uninstall /force

echo [3/4] verifying it is gone...
>> %LOG% 2>&1 powershell -NoProfile -Command "Get-PnpDevice -Class Display -PresentOnly | Select-Object Status,FriendlyName | Format-Table -AutoSize"

echo [4/4] removing the test cert from the trust stores...
>> %LOG% 2>&1 certutil -delstore Root "%CERTSUBJ%"
>> %LOG% 2>&1 certutil -delstore TrustedPublisher "%CERTSUBJ%"
>> %LOG% 2>&1 powershell -NoProfile -Command "Get-ChildItem Cert:\LocalMachine\My | Where-Object {$_.Subject -eq 'CN=%CERTSUBJ%'} | Remove-Item -Force"

echo === UNINSTALL DONE === >> %LOG%
type %LOG%
exit /b 0
