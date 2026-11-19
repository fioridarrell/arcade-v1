@echo off
rem Install Arcade V1.0 from this folder: what Install.exe does, without the installer program.
rem It runs scripts\install.ps1 with the files in bin\ (nothing is downloaded).
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\install.ps1" --prebuilt "%~dp0."
if errorlevel 1 pause
