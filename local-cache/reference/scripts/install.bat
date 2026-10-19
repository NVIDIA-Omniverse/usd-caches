@echo off
setlocal enableextensions
:: SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
:: SPDX-License-Identifier: Apache-2.0

:: Hub must be installed into "%LOCALAPPDATA%\ov\pkg\hub-<version>" to be
:: found by Hub clients, and a file named ".installed" must exist in the Hub
:: root directory for the Hub install to be considered valid.  Optionally, a
:: custom install directory may be used instead provided the
:: OMNICLIENT_HUB_EXE environment variable is set.

goto :init

:usage

  echo Syntax: install.bat [OPTION]
  echo options:
  echo -h            	Print this Help.
  echo -d ^<directory^>	Install into the supplied directory.
  echo.
  echo Please note that the -d option is an experimental feature and subject to change. If used, the environment variable 'OMNICLIENT_HUB_EXE' must be set to this same path so that Hub can be found by clients.
  echo.
  goto :eof

:init

  pushd %~dp0\..
  set SOURCE_ROOT=%CD%
  popd

  set INSTALL_ROOT=%LOCALAPPDATA%\ov\pkg

:parse

  if "%~1"=="" goto :main

  if /i "%~1"=="/h" call :usage "%~2" & goto :eof
  if /i "%~1"=="-h" call :usage "%~2" & goto :eof

  if /i "%~1"=="/d" set "INSTALL_ROOT=%~2" & shift & shift & goto :parse
  if /i "%~1"=="-d" set "INSTALL_ROOT=%~2" & shift & shift & goto :parse

  goto :usage

:main

  for /F "tokens=2" %%g in ('"%SOURCE_ROOT%"\target\release\hub.exe --version') do (SET HUB_VERSION=%%g)

  set INSTALL_DIR=%INSTALL_ROOT%\hub-%HUB_VERSION%

  if exist "%INSTALL_DIR%" (
      echo Destination directory %INSTALL_DIR% already exists.
      exit /b
  )

  md "%INSTALL_DIR%"
  xcopy "%SOURCE_ROOT%"\* "%INSTALL_DIR%"\. /s /e /q > nul
  copy /b NUL %INSTALL_DIR%\.installed > nul

  echo Hub %HUB_VERSION% installed to %INSTALL_DIR%.

: end
