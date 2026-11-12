@echo off
REM Build Feather installer using Inno Setup
REM Run from the installer directory after building feather.exe in release mode

set PROJECT_ROOT=%~dp0..
set RELEASE_EXE=%PROJECT_ROOT%\target\release\feather.exe
set ISS_FILE=%~dp0Feather.iss

if not exist "%RELEASE_EXE%" (
    echo ERROR: feather.exe not found at %RELEASE_EXE%
    echo Build the release first: cd .. ^& .\scripts\build.ps1
    exit /b 1
)

echo Building Inno Setup installer...
iscc "%ISS_FILE%"

if errorlevel 1 (
    echo ERROR: Inno Setup build failed
    exit /b 1
)

echo.
echo Installer built at %PROJECT_ROOT%\target\installer\Feather-1.1.0.exe
echo Inno Setup installer includes feather.exe correctly.
