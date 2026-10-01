@echo off
REM ============================================================================
REM x-idd install: trust a self-signed cert, sign the package, install the
REM virtual display.  No test-signing, so NO REBOOT required.
REM
REM Requires admin. Logs everything to C:\Gemlink\xidd-install.txt.
REM Rollback: uninstall.bat (same folder).
REM ============================================================================
setlocal enabledelayedexpansion
set LOG=C:\Gemlink\xidd-install.txt
set CERTSUBJ=GemLink IDD Test
set DRV=C:\Gemlink\runner\_work\GEM-Link\GEM-Link\alvr\x-idd\driver
REM Install from the STAMPED package dir: the source INF still has $ARCH$ and no
REM DriverVer, so driver selection fails against it.
set PKG=%DRV%\x64\Release\XIddDriver
set SIGNTOOL=C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe
set DEVCON=C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe

> %LOG% echo === x-idd INSTALL %DATE% %TIME% ===

echo [1/5] creating + trusting a self-signed code-signing cert...
REM Reuse an existing cert if one is already trusted, so re-running this script
REM does not pile up duplicate certificates in the machine store.
>> %LOG% 2>&1 powershell -NoProfile -Command ^
  "$c = Get-ChildItem Cert:\LocalMachine\My | Where-Object {$_.Subject -eq 'CN=%CERTSUBJ%'} | Select-Object -First 1;" ^
  "if (-not $c) {" ^
  "  $c = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=%CERTSUBJ%' -CertStoreLocation Cert:\LocalMachine\My -KeyUsage DigitalSignature -KeyExportPolicy Exportable;" ^
  "  Export-Certificate -Cert $c -FilePath C:\Gemlink\gemlink.cer | Out-Null;" ^
  "  Import-Certificate -FilePath C:\Gemlink\gemlink.cer -CertStoreLocation Cert:\LocalMachine\Root | Out-Null;" ^
  "  Import-Certificate -FilePath C:\Gemlink\gemlink.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null" ^
  "};" ^
  "Write-Output ('thumbprint=' + $c.Thumbprint)"
if errorlevel 1 goto :fail

echo [2/5] signing the driver package...
"%SIGNTOOL%" sign /fd SHA256 /sm /s My /n "%CERTSUBJ%" "%PKG%\XIddDriver.dll" >> %LOG% 2>&1
"%SIGNTOOL%" sign /fd SHA256 /sm /s My /n "%CERTSUBJ%" "%PKG%\xidddriver.cat" >> %LOG% 2>&1
if errorlevel 1 goto :fail

echo [3/5] installing the root-enumerated device (this is what creates the display)...
cd /d "%PKG%"
REM clear any orphan node from a previous failed attempt
"%DEVCON%" remove Root\XIddDriver >nul 2>&1
>> %LOG% 2>&1 "%DEVCON%" install XIddDriver.inf Root\XIddDriver
if errorlevel 1 goto :fail

echo [4/5] waiting for the display to arrive...
timeout /t 5 /nobreak >nul

echo [5/5] current display adapters:
>> %LOG% 2>&1 powershell -NoProfile -Command "Get-CimInstance Win32_VideoController | Select-Object Name,CurrentHorizontalResolution,CurrentVerticalResolution | Format-Table -AutoSize"
>> %LOG% 2>&1 powershell -NoProfile -Command "Get-PnpDevice -Class Display -PresentOnly | Select-Object Status,FriendlyName,InstanceId | Format-Table -AutoSize"

echo === INSTALL OK === >> %LOG%
type %LOG%
exit /b 0

:fail
echo === INSTALL FAILED (rc=%ERRORLEVEL%) === >> %LOG%
type %LOG%
exit /b 1
