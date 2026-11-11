@echo off
REM Build Feather MSI installer using WiX v4
REM Run from the installer directory after building feather.exe in release mode
REM NOTE: WiX v4 7.0.1 has a known bug where File elements are ignored (cabinet empty).
REM Workaround: Build MSI without Media element, then manually inject file.

set PROJECT_ROOT=%~dp0..
set RELEASE_EXE=%PROJECT_ROOT%\target\release\feather.exe
set WIX_OUT=%PROJECT_ROOT%\target\installer
set PRODUCT_WXS=%~dp0Product.wxs

if not exist "%RELEASE_EXE%" (
    echo ERROR: feather.exe not found at %RELEASE_EXE%
    echo Build the release first: cd .. ^& .\scripts\build.ps1
    exit /b 1
)

if not exist "%WIX_OUT%" mkdir "%WIX_OUT%"

REM Copy exe to installer directory for WiX to find it
copy "%RELEASE_EXE%" "%~dp0feather.exe" >nul
if errorlevel 1 (
    echo ERROR: Failed to copy feather.exe
    exit /b 1
)

echo Building MSI installer (structure only - file injection needed)...
wix build -o "%WIX_OUT%\Feather-1.0.0.msi" "%PRODUCT_WXS%"

if errorlevel 1 (
    echo ERROR: WiX build failed
    exit /b 1
)

echo.
echo MSI structure built at %WIX_OUT%\Feather-1.0.0.msi
echo.
echo ============================================================
echo WiX v4 7.0.1 BUG WORKAROUND REQUIRED:
echo The MSI has correct UI/shortcuts/features but NO FILES in cabinet.
echo.
echo Option 1: Use Inno Setup instead (recommended)
echo   - Run: .\build-innosetup.bat
echo.
echo Option 2: Manually inject feather.exe into MSI cabinet
echo   - Requires Orca/msidb or WiX v3 candle/light
echo.
echo Option 3: Use feather.exe directly from target\release\
echo ============================================================