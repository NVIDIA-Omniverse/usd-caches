# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import os
import platform
import subprocess
import sys

def main():
    system = platform.system().lower() # 'linux', 'darwin', 'windows'
    machine = platform.machine().lower() # 'x86_64', 'arm64'

    binary_name = f"hub"
    if system == "windows":
        binary_name += ".exe"
    package_dir = os.path.dirname(__file__)
    binary_path = os.path.join(package_dir, "package", "target", "release", binary_name)
    if not os.path.exists(binary_path):
        print(f"Unable to find {binary_path}", file=sys.stderr)
        sys.exit(1)
    if system != "windows" and not os.access(binary_path, os.X_OK):
        os.chmod(binary_path, 0o755)
    result = subprocess.run([binary_path] + sys.argv[1:])
    sys.exit(result.returncode)

    try:
        if system == "windows":
            sys.exit(subprocess.call(args))
        else:
            os.execv(binary, args)
    except Exception as e:
        sys.exit(f"Failed to execute {e}")
