#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -o errexit

# Hub must be installed into "$HOME/.local/share/ov/pkg/hub-<version>" to be
# found by Hub clients, and a file named ".installed" must exist in the Hub
# root directory for the Hub install to be considered valid. Optionally, a
# custom install directory may be used instead provided the OMNICLIENT_HUB_EXE
# environment variable is set.

Help()
{
  echo "Syntax: install.sh [OPTION]"
  echo "options:"
  echo "-h            	Print this Help."
  echo "-d <directory>	Install into the supplied directory."
  echo
  echo "Please note that the -d option is an experimental feature and subject to change. If used, the environment variable 'OMNICLIENT_HUB_EXE' must be set to this same path so that Hub can be found by clients."
  echo
}

INSTALL_ROOT="$HOME/.local/share/ov/pkg"

while getopts ":hd:" opt; do
  case ${opt} in
    h) # display help
      Help
      exit 0
      ;;
    d) # specify an install directory
      INSTALL_ROOT="${OPTARG}"
      ;;
    ?)
      echo "Invalid option: -${OPTARG}."
      exit 1
      ;;
  esac
done

SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
SOURCE_ROOT=$( dirname "${SCRIPT_DIR}" )
HUB_VERSION=$( "${SOURCE_ROOT}/target/release/hub" --version | awk '{print $2}' )
INSTALL_DIR="${INSTALL_ROOT}/hub-${HUB_VERSION}"

if [[ -d "$INSTALL_DIR" ]]; then
  echo "Destination directory $INSTALL_DIR already exists."
  exit
fi

mkdir -p "${INSTALL_DIR}"
cp -r "${SOURCE_ROOT}"/* "${INSTALL_DIR}"/.
touch "${INSTALL_DIR}"/.installed

echo "Hub ${HUB_VERSION} installed to ${INSTALL_DIR}."
