#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -o errexit

Help()
{
  echo "Syntax: uninstall.sh [OPTION]"
  echo "options:"
  echo "-h	Print this Help."
  echo "-d	Delete the Hub install directory."
  echo
}

while getopts ":hd" opt; do
  case ${opt} in
    h) # display help
      Help
      exit 0
      ;;
    d) # delete the install directory
      DELETE_INSTALL=true
      ;;
    ?)
      echo "Invalid option: -${OPTARG}."
      exit 1
      ;;
  esac
done

SCRIPT_DIR=$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )
INSTALL_DIR=$( dirname "${SCRIPT_DIR}" )
INSTALLED_FILE="${INSTALL_DIR}/.installed"
HUB_VERSION=$( "${INSTALL_DIR}/target/release/hub" --version | awk '{print $2}' )
CACHE_DIR=$( "${INSTALL_DIR}/target/release/hub" --show-config | head -n -1 | jq ".cache.path" )

if [ -f "${INSTALLED_FILE}" ]; then
  rm -f "${INSTALLED_FILE}"
  HUB_INSTALLED=true
else
  HUB_INSTALLED=false
fi

# Sleep for 1 second to give any concurrent taks that may be trying to start
# Hub, etc, a chance to complete.
sleep 1s

# Shutdown even if not installed just in case this instance is still running.
"${INSTALL_DIR}/target/release/hub" --shutdown > /dev/null

if [ "$DELETE_INSTALL" = true ] ; then
  rm -fr "${INSTALL_DIR}"
fi

if [ "$HUB_INSTALLED" = true ] ; then
  echo "Hub ${HUB_VERSION} uninstalled from ${INSTALL_DIR}. Cache is still present in ${CACHE_DIR}."
else
  echo "Hub ${HUB_VERSION} in ${INSTALL_DIR} is already uninstalled."
fi
