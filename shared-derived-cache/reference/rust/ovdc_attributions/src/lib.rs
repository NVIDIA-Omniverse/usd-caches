// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Builds `ATTRIBUTIONS.md` from the resolved cargo dependency graph.
//!
//! The list comes from `cargo metadata`, not from `Cargo.lock`, because only
//! metadata reports a `manifest_path` for every package. That path is what
//! locates the license text: registry crates unpack under `$CARGO_HOME/registry`
//! and git dependencies under `$CARGO_HOME/git/checkouts`, and guessing either
//! layout from a package name and version would break on the git dependencies
//! this workspace has.
//!
//! Output is deterministic -- packages sorted, files sorted, no timestamp -- so
//! that a build regenerating the file does not produce a diff unless the
//! dependency set actually changed. [`check`] relies on that to verify the
//! committed copy in CI.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Filename stems that hold license text, lowercased.
///
/// Matched as a prefix so that `LICENSE-APACHE`, `LICENSE.md` and `COPYING.LESSER`
/// are all picked up. This is the same rule the SBOM path uses in
/// `docker/dockerfile/write-third-party-licenses.py`, kept in step deliberately
/// so the two artifacts do not disagree about what counts as a license file.
const LICENSE_STEMS: [&str; 5] = ["license", "licence", "copying", "notice", "unlicense"];

/// Upper bound on a single license file, in bytes.
///
/// A handful of crates vendor entire license *collections*; without a cap one of
/// them would dominate the file. Anything longer is truncated with a marker
/// rather than dropped, so the attribution is still present.
const MAX_LICENSE_BYTES: usize = 512 * 1024;

pub type Result<T> = std::result::Result<T, String>;

/// One third-party package and the license text shipped with it.
struct Package {
    name: String,
    version: String,
    /// The SPDX expression the crate declares, if it declares one.
    license: Option<String>,
    repository: Option<String>,
    /// `(filename, contents)`, sorted by filename.
    license_files: Vec<(String, String)>,
}

/// Runs `cargo metadata` for the workspace containing `manifest_path`.
///
/// `--offline` and `--locked` are both deliberate. By the time this runs the
/// outer build has already resolved and fetched everything, so neither can fail
/// for a legitimate build; together they stop this from being the thing that
/// silently mutates `Cargo.lock` or reaches the network mid-build.
pub fn cargo_metadata(cargo: &OsStr, manifest_path: &Path) -> Result<String> {
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .map_err(|err| format!("could not run `cargo metadata`: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "`cargo metadata` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    String::from_utf8(output.stdout)
        .map_err(|err| format!("`cargo metadata` emitted non-UTF-8 output: {err}"))
}

/// Whether `name` is a file holding license text.
fn is_license_file(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    LICENSE_STEMS.iter().any(|stem| lowered.starts_with(stem))
}

/// Reads a license file, truncating anything past [`MAX_LICENSE_BYTES`].
///
/// Lossy decoding rather than a hard error: a license that is not valid UTF-8 is
/// still an attribution obligation, and a mangled character is a better outcome
/// than omitting the package.
fn read_license(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }

    let truncated = bytes.len() > MAX_LICENSE_BYTES;
    let end = bytes.len().min(MAX_LICENSE_BYTES);
    // Trim back to a character boundary so the lossy conversion does not emit a
    // replacement character for a multi-byte sequence the cap split in half.
    let mut text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if truncated {
        text.push_str("\n\n[truncated by ovdc_attributions]");
    }
    Some(text.trim_end().to_string())
}

/// Collects the license files shipped in a package's directory.
///
/// Only the package root and a top-level `LICENSES/` directory are searched. A
/// deep walk would pull in the license files of a crate's own test fixtures and
/// vendored samples, which are not this project's attribution obligations.
fn collect_license_files(dir: &Path, declared: Option<&str>) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();

    let consider = |path: PathBuf, name: String, seen: &mut BTreeSet<String>| {
        if !seen.insert(name.clone()) {
            return None;
        }
        read_license(&path).map(|text| (name, text))
    };

    // A `license-file` in the manifest is authoritative and need not be named
    // like one, so it is taken regardless of the filename rule.
    if let Some(rel) = declared {
        let path = dir.join(rel);
        if let Some(entry) = consider(path, rel.to_string(), &mut seen) {
            found.push(entry);
        }
    }

    let mut scan = |root: &Path, prefix: &str| {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().is_file() || !is_license_file(&name) {
                continue;
            }
            let display = format!("{prefix}{name}");
            if let Some(item) = consider(entry.path(), display, &mut seen) {
                found.push(item);
            }
        }
    };

    scan(dir, "");
    scan(&dir.join("LICENSES"), "LICENSES/");

    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Extracts the third-party packages from `cargo metadata` output.
fn packages(metadata: &serde_json::Value) -> Result<Vec<Package>> {
    let members: BTreeSet<&str> = metadata
        .get("workspace_members")
        .and_then(|v| v.as_array())
        .ok_or("metadata has no `workspace_members`")?
        .iter()
        .filter_map(|id| id.as_str())
        .collect();

    let all = metadata
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or("metadata has no `packages`")?;

    let mut out = Vec::new();
    for package in all {
        let id = package
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // A null `source` means a path dependency. Combined with the member
        // check this drops every crate in this repository, including any that
        // is depended on by path but not listed as a workspace member.
        let is_local = package.get("source").is_none_or(serde_json::Value::is_null);
        if members.contains(id) || is_local {
            continue;
        }

        let string = |key: &str| {
            package
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };

        let manifest = package
            .get("manifest_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("package {id} has no `manifest_path`"))?;
        let dir = Path::new(manifest)
            .parent()
            .ok_or_else(|| format!("manifest path {manifest} has no parent"))?;

        let license_file = string("license_file");
        out.push(Package {
            name: string("name").unwrap_or_else(|| "unknown".to_string()),
            version: string("version").unwrap_or_default(),
            license: string("license"),
            repository: string("repository"),
            license_files: collect_license_files(dir, license_file.as_deref()),
        });
    }

    out.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    out.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    Ok(out)
}

/// Renders the markdown document for `metadata_json`.
pub fn render(metadata_json: &str) -> Result<String> {
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json).map_err(|err| format!("bad metadata JSON: {err}"))?;
    let packages = packages(&metadata)?;

    let without_text = packages
        .iter()
        .filter(|p| p.license_files.is_empty())
        .count();

    let mut out = String::new();
    out.push_str("# Third Party Attributions\n\n");
    out.push_str(
        "Every third-party crate in the resolved dependency graph, with the full text of \
         each license file shipped in its source distribution.\n\n",
    );
    out.push_str(
        "This file is generated. Do not edit it by hand -- run `cargo run -p ovdc_attributions \
         --bin gen-attributions` from `./rust`, or simply build the workspace, which regenerates \
         it whenever `Cargo.lock` changes.\n\n",
    );
    let _ = writeln!(out, "Packages: {}", packages.len());
    if without_text > 0 {
        let _ = writeln!(
            out,
            "\nOf these, {without_text} ship no license file in their source distribution; \
             the SPDX expression they declare is recorded instead."
        );
    }
    out.push('\n');

    for package in &packages {
        let _ = writeln!(out, "## {} {}\n", package.name, package.version);
        match &package.license {
            Some(license) => {
                let _ = writeln!(out, "- License: `{license}`");
            }
            None => out.push_str("- License: *not declared*\n"),
        }
        if let Some(repository) = &package.repository {
            let _ = writeln!(out, "- Repository: {repository}");
        }
        out.push('\n');

        if package.license_files.is_empty() {
            out.push_str("*No license file is distributed with this crate.*\n\n");
            continue;
        }

        for (name, text) in &package.license_files {
            let _ = writeln!(out, "### `{name}`\n");
            // Some crates ship license text in markdown with its own fences, so
            // a three-backtick block would terminate early.
            out.push_str("````text\n");
            out.push_str(text);
            out.push_str("\n````\n\n");
        }
    }

    Ok(out)
}

/// Writes `contents` to `path`, returning whether anything changed.
///
/// The read-compare is what keeps a build from rewriting an identical file and
/// updating its mtime, which would make the workspace look dirty to any tooling
/// watching timestamps.
pub fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(false);
    }
    std::fs::write(path, contents)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(true)
}

/// Whether the file at `path` matches what would be generated now.
pub fn check(path: &Path, contents: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|existing| existing == contents)
}

/// Regenerates the attributions file for the workspace at `workspace_root`.
///
/// Returns whether the file on disk changed.
pub fn generate(cargo: &OsStr, workspace_root: &Path) -> Result<bool> {
    let metadata = cargo_metadata(cargo, &workspace_root.join("Cargo.toml"))?;
    let rendered = render(&metadata)?;
    write_if_changed(&workspace_root.join("ATTRIBUTIONS.md"), &rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_filenames_match_by_prefix() {
        for name in [
            "LICENSE",
            "LICENSE-MIT",
            "license.md",
            "COPYING.LESSER",
            "NOTICE",
            "UNLICENSE",
            "Licence.txt",
        ] {
            assert!(is_license_file(name), "{name} should match");
        }

        for name in ["src", "Cargo.toml", "README.md", "relicense-notes.md"] {
            assert!(!is_license_file(name), "{name} should not match");
        }
    }

    #[test]
    fn workspace_members_and_path_deps_are_excluded() {
        let metadata = serde_json::json!({
            "workspace_members": ["path+file:///repo/rust/ovdc#0.1.0"],
            "packages": [
                {
                    "id": "path+file:///repo/rust/ovdc#0.1.0",
                    "name": "ovdc",
                    "version": "0.1.0",
                    "source": null,
                    "manifest_path": "/repo/rust/ovdc/Cargo.toml"
                },
                {
                    "id": "path+file:///repo/rust/ovdc_lru#0.1.0",
                    "name": "ovdc_lru",
                    "version": "0.1.0",
                    "source": null,
                    "manifest_path": "/repo/rust/ovdc_lru/Cargo.toml"
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                    "name": "serde",
                    "version": "1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "license": "MIT OR Apache-2.0",
                    "manifest_path": "/nonexistent/serde-1.0.0/Cargo.toml"
                }
            ]
        });

        let found = packages(&metadata).expect("metadata should parse");
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["serde"]);
    }

    #[test]
    fn render_records_crates_that_ship_no_license_file() {
        let metadata = serde_json::json!({
            "workspace_members": [],
            "packages": [{
                "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                "name": "serde",
                "version": "1.0.0",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "license": "MIT OR Apache-2.0",
                "repository": "https://github.com/serde-rs/serde",
                "manifest_path": "/nonexistent/serde-1.0.0/Cargo.toml"
            }]
        })
        .to_string();

        let out = render(&metadata).expect("render should succeed");
        assert!(out.contains("## serde 1.0.0"));
        assert!(out.contains("- License: `MIT OR Apache-2.0`"));
        assert!(out.contains("- Repository: https://github.com/serde-rs/serde"));
        assert!(out.contains("*No license file is distributed with this crate.*"));
    }

    #[test]
    fn render_is_deterministic_and_sorted() {
        let package = |name: &str, version: &str| {
            serde_json::json!({
                "id": format!("registry+https://x#{name}@{version}"),
                "name": name,
                "version": version,
                "source": "registry+https://x",
                "license": "MIT",
                "manifest_path": format!("/nonexistent/{name}-{version}/Cargo.toml")
            })
        };
        let metadata = serde_json::json!({
            "workspace_members": [],
            "packages": [package("zzz", "1.0.0"), package("aaa", "2.0.0"), package("aaa", "1.0.0")]
        })
        .to_string();

        let first = render(&metadata).expect("render should succeed");
        assert_eq!(first, render(&metadata).expect("render should succeed"));

        let order: Vec<usize> = ["## aaa 1.0.0", "## aaa 2.0.0", "## zzz 1.0.0"]
            .iter()
            .map(|heading| first.find(heading).expect("heading should be present"))
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "headings out of order"
        );
    }

    #[test]
    fn license_text_is_fenced_so_markdown_licenses_survive() {
        let dir = std::env::temp_dir().join("ovdc_attributions_fence_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        std::fs::write(dir.join("LICENSE.md"), "MIT\n\n```\nembedded fence\n```\n")
            .expect("license should be writable");

        let files = collect_license_files(&dir, None);
        assert_eq!(files.len(), 1);
        assert!(files[0].1.contains("embedded fence"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
