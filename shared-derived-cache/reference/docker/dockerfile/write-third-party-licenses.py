#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import base64
import json
import re
import sys
from pathlib import PurePosixPath


LICENSE_FILE_RE = re.compile(r"(?:^|/)(?:license|copying|notice)", re.IGNORECASE)


def package_from_path(path: str) -> tuple[str, str]:
    parts = PurePosixPath(path).parts
    if parts and parts[0] == "/":
        parts = parts[1:]
    crate_dir = parts[0] if parts else ""
    match = re.match(r"(?P<name>.+)-(?P<version>\d[^/]*)$", crate_dir)
    if not match:
        return crate_dir or "unknown", ""
    return match.group("name"), match.group("version")


def digest_value(file_entry: dict, algorithm: str) -> str:
    for digest in file_entry.get("digests", []):
        if digest.get("algorithm", "").lower() == algorithm.lower():
            return digest.get("value", "")
    return ""


def decode_contents(file_entry: dict) -> str:
    contents = file_entry.get("contents", "")
    if not contents:
        return ""
    data = base64.b64decode(contents)
    return data.decode("utf-8", errors="replace").rstrip()


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: write-third-party-licenses.py <input.syft.json> <output.md>", file=sys.stderr)
        return 2

    with open(sys.argv[1], encoding="utf-8") as handle:
        sbom = json.load(handle)

    entries = []
    for file_entry in sbom.get("files", []):
        path = file_entry.get("location", {}).get("path", "")
        if not path or not LICENSE_FILE_RE.search(path):
            continue
        contents = decode_contents(file_entry)
        if not contents:
            continue
        package_name, package_version = package_from_path(path)
        metadata = file_entry.get("metadata", {})
        entries.append(
            {
                "package_name": package_name,
                "package_version": package_version,
                "path": path,
                "size": metadata.get("size"),
                "mime_type": metadata.get("mimeType", ""),
                "sha256": digest_value(file_entry, "sha256"),
                "contents": contents,
            }
        )

    entries.sort(key=lambda item: (item["package_name"], item["package_version"], item["path"]))

    with open(sys.argv[2], "w", encoding="utf-8") as handle:
        handle.write("# Third Party Licenses\n\n")
        handle.write("Generated from license-like files captured by Syft from `sources.tar.gz`.\n\n")
        handle.write(f"Collected license files: {len(entries)}\n\n")
        for entry in entries:
            package = entry["package_name"]
            if entry["package_version"]:
                package += f" {entry['package_version']}"

            handle.write(f"## {package}\n\n")
            handle.write(f"- File: `{entry['path']}`\n")
            if entry["size"] is not None:
                handle.write(f"- Size: `{entry['size']}` bytes\n")
            if entry["mime_type"]:
                handle.write(f"- MIME type: `{entry['mime_type']}`\n")
            if entry["sha256"]:
                handle.write(f"- SHA256: `{entry['sha256']}`\n")
            handle.write("\n")
            handle.write("```text\n")
            handle.write(entry["contents"])
            handle.write("\n```\n\n")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
