#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Add SPDX headers to supported tracked and unignored files in this repository."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys


COPYRIGHT = "SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved."
LICENSE = "SPDX-License-Identifier: Apache-2.0"
COMMENT_PREFIXES = {
    ".rs": "//",
    ".yaml": "#",
    ".yml": "#",
    ".py": "#",
    ".sh": "#",
    ".toml": "#",
    ".tlp": "#",
    ".txt": "//",
    ".proto": "//",
    ".hcl": "//",
    # .ext files in this repository are OpenSSL extension configuration files.
    ".ext": "#",
    ".dockerignore": "#",
    ".gitignore": "#",
    ".ignore": "#",
    ".gitattributes": "#",
    ".helmignore": "#",
}


def header_for(path):
    if path.suffix == ".tpl" or (
        "templates" in path.parts and "helm" in path.parts
        and (path.suffix in {".yaml", ".yml"} or path.name == "NOTES.txt")
    ):
        return "{{/*\n" + COPYRIGHT + "\n" + LICENSE + "\n*/}}\n"
    if path.suffix == ".md":
        return f"<!--\n{COPYRIGHT}\n{LICENSE}\n-->\n\n"
    # pathlib treats bare dotfiles as names without a suffix.
    if path.name in {"Makefile", "Dockerfile"} or path.name in COMMENT_PREFIXES or path.name.endswith(".Dockerfile"):
        prefix = COMMENT_PREFIXES.get(path.name, "#")
    else:
        prefix = COMMENT_PREFIXES.get(path.suffix)
    if prefix is None:
        return None
    return f"{prefix} {COPYRIGHT}\n{prefix} {LICENSE}\n\n"


def add_header(original, header, path):
    newline = b"\r\n" if b"\r\n" in original else b"\n"
    encoded_header = header.encode().replace(b"\n", newline)
    if not original.strip():
        return encoded_header.rstrip(b"\r\n") + newline
    # Keep a UTF-8 BOM and executable shebang ahead of the header.
    offset = 3 if original.startswith(b"\xef\xbb\xbf") else 0
    if original[offset:].startswith(b"#!"):
        end = original.find(b"\n", offset)
        if end == -1:
            return original + newline + encoded_header
        offset = end + 1
    if path.name == "Dockerfile" or path.name.endswith(".Dockerfile"):
        # Docker stops recognizing parser directives after an ordinary comment.
        while re.match(rb"#\s*(?:syntax|escape|check)\s*=", original[offset:], re.IGNORECASE):
            end = original.find(b"\n", offset)
            if end == -1:
                return original + newline + newline + encoded_header
            offset = end + 1
        if offset:
            encoded_header = newline + encoded_header
    return original[:offset] + encoded_header + original[offset:]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="list missing headers without writing; exit 1 if any are missing")
    parser.add_argument("paths", nargs="*", help="limit to repository-relative files or directories (default: entire repository)")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    try:
        listing = subprocess.run(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", *args.paths],
            cwd=root, check=True, stdout=subprocess.PIPE,
        ).stdout
        missing = 0
        for name in sorted(set(listing.split(b"\0")) - {b""}):
            relative = Path(os.fsdecode(name))
            path = root / relative
            header = header_for(relative)
            if header is None or path.is_symlink() or not path.is_file():
                continue
            original = path.read_bytes()
            if b"SPDX-License-Identifier:" in original or b"SPDX-FileCopyrightText:" in original:
                continue
            missing += 1
            if not args.check:
                path.write_bytes(add_header(original, header, relative))
            print(f"{'Missing' if args.check else 'Added'}: {relative}")
        print(f"{missing} file(s) {'need headers' if args.check else 'updated'}.")
        return 1 if args.check and missing else 0
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
