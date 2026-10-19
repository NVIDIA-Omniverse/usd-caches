# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Run with python3 -B scripts/test_apply_spdx_headers.py."""

import importlib.util
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("apply-spdx-headers.py")
SPEC = importlib.util.spec_from_file_location("spdx", SCRIPT)
spdx = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(spdx)


class HeaderTests(unittest.TestCase):
    def test_supported_comments(self):
        for name in ["a.rs", "a.proto", "a.hcl", "a.txt"]:
            self.assertTrue(spdx.header_for(Path(name)).startswith("// SPDX-"))
        for name in ["a.yaml", "a.yml", "a.py", "a.sh", "a.toml", "a.tlp",
                     "a.ext", ".dockerignore", ".gitattributes", ".gitignore",
                     ".helmignore", ".ignore", "Makefile", "Dockerfile", "ci.Dockerfile"]:
            self.assertTrue(spdx.header_for(Path(name)).startswith("# SPDX-"))
        self.assertTrue(spdx.header_for(Path("README.md")).startswith("<!--"))
        for name in ["docker/helm/templates/service.yaml", "docker/helm/templates/statefulset.yml",
                     "docker/helm/templates/NOTES.txt", "docker/helm/templates/_helpers.tpl"]:
            self.assertTrue(spdx.header_for(Path(name)).startswith("{{/*"))
        self.assertIsNone(spdx.header_for(Path("a.json")))

    def test_preamble_and_empty_files(self):
        shell = Path("a.sh")
        original = b"#!/bin/sh\r\necho hello\r\n"
        result = spdx.add_header(original, spdx.header_for(shell), shell)
        self.assertTrue(result.startswith(b"#!/bin/sh\r\n# SPDX-"))
        self.assertTrue(result.endswith(b"echo hello\r\n"))
        self.assertNotIn(b"\n", result.replace(b"\r\n", b""))
        docker = Path("Dockerfile")
        directives = b"# syntax=docker/dockerfile:1\n# escape=`\n"
        result = spdx.add_header(directives + b"FROM ubuntu\n", spdx.header_for(docker), docker)
        self.assertTrue(result.startswith(directives + b"\n# SPDX-"))
        bom = b"\xef\xbb\xbf"
        self.assertTrue(spdx.add_header(bom + b"body\n", spdx.header_for(shell), shell).startswith(bom))
        empty = spdx.add_header(b"", spdx.header_for(Path("a.rs")), Path("a.rs"))
        self.assertFalse(empty.endswith(b"\n\n"))

    def test_cli_check_apply_and_repeat(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            subprocess.run(["git", "init", "-q", temp], check=True)
            (root / "scripts").mkdir()
            shutil.copyfile(SCRIPT, root / "scripts" / SCRIPT.name)
            command = ["python3", "-B", str(root / "scripts" / SCRIPT.name)]
            existing = b"// SPDX-License-Identifier: MIT\nfn main() {}\n"
            (root / "old.rs").write_bytes(existing)
            (root / "new.sh").write_bytes(b"#!/bin/sh\necho hello\n")
            (root / "new.sh").chmod(0o755)
            (root / ".gitignore").write_text("ignored.rs\n")
            (root / "ignored.rs").write_text("ignored\n")
            (root / "linked.rs").symlink_to(root / "old.rs")
            old_stat = (root / "old.rs").stat().st_mtime_ns
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 1)
            self.assertNotIn(b"SPDX", (root / "new.sh").read_bytes())
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual((root / "old.rs").read_bytes(), existing)
            self.assertEqual((root / "old.rs").stat().st_mtime_ns, old_stat)
            self.assertEqual((root / "ignored.rs").read_text(), "ignored\n")
            self.assertTrue((root / "linked.rs").is_symlink())
            self.assertEqual((root / "new.sh").stat().st_mode & 0o777, 0o755)
            stat = (root / "new.sh").stat().st_mtime_ns
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual((root / "new.sh").stat().st_mtime_ns, stat)
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 0)


if __name__ == "__main__":
    unittest.main()
