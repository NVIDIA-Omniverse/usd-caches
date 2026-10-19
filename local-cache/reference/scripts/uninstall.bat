<# : batch portion (contained within a PowerShell multi-line comment)
:: SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
:: SPDX-License-Identifier: Apache-2.0

:: The Windows batch processor doesn't have a way to parse JSON, so we defer
:: the parsing to the PowerShell code at the end. We extract .cache.path and
:: print it to stdout to set into a variable in the batch file code below.

@echo off
setlocal enableextensions

goto :init

:usage

  echo Syntax: uninstall.bat [OPTION]
  echo options:
  echo -h	Print this Help.
  echo -d	Delete the Hub install directory.
  echo.
  goto :eof

:init

  pushd %~dp0\..
  set INSTALL_DIR=%CD%
  popd

  pushd %~dp0\..\..
  set INSTALL_ROOT=%CD%
  popd

  set INSTALLED_FILE="%INSTALL_DIR%\.installed"

  for /F "tokens=2" %%g in ('"%INSTALL_DIR%"\target\release\hub.exe --version') do (SET HUB_VERSION=%%g)
  for /F "delims=" %%g in ('powershell "iex (${%~f0} | Out-String)"') do (SET CACHE_DIR=%%g)
  for /F "delims=" %%g in ('echo %cd% ^| findstr /b /c:"%INSTALL_DIR%"') do (SET CD_MATCH=%%g)

  if defined CD_MATCH (
    set FINAL_DIR=%INSTALL_ROOT%
  )
  if not defined CD_MATCH (
    set FINAL_DIR=%CD%
  )

:parse

  if "%~1"=="" goto :main

  if /i "%~1"=="/h" call :usage "%~2" & goto :eof
  if /i "%~1"=="-h" call :usage "%~2" & goto :eof

  if /i "%~1"=="/d" set "DELETE_INSTALL=yes" & shift & goto :parse
  if /i "%~1"=="-d" set "DELETE_INSTALL=yes" & shift & goto :parse

  goto :usage

:main

  if exist "%INSTALLED_FILE%" (
    del /Q "%INSTALL_DIR%\.installed"
    set HUB_INSTALLED=yes
  )

  :: Sleep for 1 second to give any concurrent taks that may be trying to start
  :: Hub, etc, a chance to complete.
  timeout /t 1 /nobreak > NUL

  :: Shutdown even if not installed just in case this instance is still running.
  "%INSTALL_DIR%\target\release\hub.exe" --shutdown > NUL

  if defined HUB_INSTALLED (
    echo Hub %HUB_VERSION% uninstalled from %INSTALL_DIR%. Cache is still present in %CACHE_DIR%.
  )
  if not defined HUB_INSTALLED (
    echo Hub %HUB_VERSION% in %INSTALL_DIR% is already uninstalled.
  )

  if not defined DELETE_INSTALL (
    goto :eof
  )

  :: This is a weird way to delete the install directory (including this batch
  :: file) and terminate without generating a "file not found" error message.
  (goto) 2>nul & endlocal & cd %FINAL_DIR% & rd /s /q %INSTALL_DIR%

: end batch / begin PowerShell hybrid code #>


add-type -AssemblyName System.Web.Extensions
$JSON = new-object Web.Script.Serialization.JavaScriptSerializer
$exe = "{0}\\target\\release\hub.exe" -f $env:INSTALL_DIR
$config = iex "$exe --show-config" | Select-Object -SkipLast 1 | Out-String
$obj = $JSON.DeserializeObject($config)

"{0}" -f $obj["cache"]["path"].Replace('/', '\\')
