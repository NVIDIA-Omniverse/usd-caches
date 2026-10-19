#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import json
import os
import shutil
import subprocess
import sys
from collections import deque
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: prune-vendor <vendor-dir>", file=sys.stderr)
        return 2

    vendor_dir = Path(sys.argv[1])
    rust_triple = os.environ.get("RUST_TRIPLE")
    if not rust_triple:
        print("RUST_TRIPLE is required", file=sys.stderr)
        return 2

    metadata = json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                rust_triple,
            ],
            text=True,
        )
    )

    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    workspace = set(metadata["workspace_members"])

    roots = [
        package_id
        for package_id in workspace
        if any("bin" in target.get("kind", []) for target in packages[package_id].get("targets", []))
    ]
    if not roots:
        roots = list(workspace)

    seen = set()
    queue = deque(roots)
    while queue:
        package_id = queue.popleft()
        if package_id in seen:
            continue
        seen.add(package_id)

        for dependency in nodes.get(package_id, {}).get("deps", []):
            if any(
                dep_kind.get("kind") in (None, "normal", "build")
                for dep_kind in dependency.get("dep_kinds", [])
            ):
                queue.append(dependency["pkg"])

    keep = set()
    for package_id in seen:
        package = packages[package_id]
        if package.get("source"):
            keep.add(f"{package['name']}-{package['version']}")

    removed = 0
    for child in vendor_dir.iterdir():
        if child.is_dir() and child.name not in keep:
            shutil.rmtree(child)
            removed += 1

    print(f"kept {len(keep)} vendored crates; removed {removed} dev-only/unreachable crates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
