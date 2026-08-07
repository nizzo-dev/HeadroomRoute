@echo off
setlocal

set "HR_CLI=%~dp0HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

set "HR_CLI=%~dp0dist\HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

for %%F in ("%~dp0HeadroomRouteCLI-*.exe") do (
    if exist "%%~fF" (
        set "HR_CLI=%%~fF"
        goto run
    )
)

set "HR_CLI=%LOCALAPPDATA%\HeadroomRoute\HeadroomRouteCLI.exe"
if exist "%HR_CLI%" goto run

>&2 echo HeadroomRoute CLI executable not found. Run Install.ps1 first.
exit /b 1

:run
"%HR_CLI%" %*
set "HR_EXIT=%ERRORLEVEL%"
endlocal & exit /b %HR_EXIT%
