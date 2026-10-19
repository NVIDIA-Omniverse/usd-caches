@ECHO OFF

pushd %~dp0

REM Command file for Sphinx documentation. Sphinx >= 8.2 needs Python >= 3.11;
REM uv provisions it and the packages in requirements.txt.

set SOURCEDIR=.
set BUILDDIR=_build

if "%1" == "" goto help
if "%1" == "clean" goto clean

if not "%SPHINXBUILD%" == "" (
	%SPHINXBUILD% -M %1 %SOURCEDIR% %BUILDDIR% %SPHINXOPTS% %O%
	goto end
)

where uv >NUL 2>NUL
if errorlevel 1 (
	echo.
	echo.uv was not found. Install it with "winget install astral-sh.uv",
	echo.open a new terminal, and re-run.
	exit /b 1
)

uv run --no-project --python ">=3.11" --with-requirements requirements.txt sphinx-build -M %1 %SOURCEDIR% %BUILDDIR% %SPHINXOPTS% %O%
goto end

:clean
if exist %BUILDDIR% rmdir /s /q %BUILDDIR%
goto end

:help
uv run --no-project --python ">=3.11" --with-requirements requirements.txt sphinx-build -M help %SOURCEDIR% %BUILDDIR% %SPHINXOPTS% %O%

:end
popd
