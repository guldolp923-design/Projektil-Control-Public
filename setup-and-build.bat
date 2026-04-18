@echo off
:: PROJEKTIL Control — Setup & Build Script
:: Fuehre dieses Script als Administrator aus

echo.
echo ========================================
echo  PROJEKTIL Control — Setup
echo ========================================
echo.

:: 1. Node.js pruefen
node --version >nul 2>&1
if %errorlevel% neq 0 (
    echo [FEHLER] Node.js ist nicht installiert!
    echo Bitte installieren: https://nodejs.org
    pause & exit
)

:: 2. Rust pruefen
cargo --version >nul 2>&1
if %errorlevel% neq 0 (
    echo [FEHLER] Rust ist nicht installiert!
    echo Bitte installieren: https://rustup.rs
    pause & exit
)

:: 3. Tauri CLI installieren
echo [1/4] Tauri CLI installieren...
call npm install

:: 4. Dependencies
echo [2/4] Rust Dependencies laden...
cd src-tauri
cargo fetch
cd ..

:: 5. Build
echo [3/4] App bauen (Release)...
call npm run build

:: 6. Autostart einrichten
echo [4/4] Autostart einrichten...
set STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup
set EXE_PATH=%~dp0src-tauri\target\release\projektil-control.exe

if exist "%EXE_PATH%" (
    copy "%EXE_PATH%" "%STARTUP%\PROJEKTIL-Control.exe"
    echo [OK] Autostart eingerichtet: %STARTUP%\PROJEKTIL-Control.exe
) else (
    echo [WARN] EXE nicht gefunden — Autostart muss manuell eingerichtet werden
    echo Pfad: %EXE_PATH%
)

echo.
echo ========================================
echo  FERTIG!
echo  Die App startet beim naechsten Windows-
echo  Start automatisch.
echo.
echo  Manuell starten:
echo  %EXE_PATH%
echo ========================================
pause
