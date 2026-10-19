#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

"""Fill in Cargo license metadata and de-duplicate packages in Syft SBOMs.

Syft's Rust catalogers read Cargo.lock and the cargo-auditable payload, neither
of which carries a license. `cargo metadata` does: it exposes the `license`
field from every crate's Cargo.toml. This joins the two on (name, version).

Also collapses duplicate packages. Syft emits one entry per piece of evidence,
so the same crate can appear several times; SPDX consumers treat those as
distinct components and double-count the dependency set.

usage: enrich-cargo-licenses.py <cargo-metadata.json> <sbom> [<sbom> ...]
"""

import json
import re
import sys

# A conservative SPDX expression: identifiers, +, operators, parens. Anything
# else (e.g. ddcs's "Proprietary; Copyright (c) ...") becomes a LicenseRef.
SPDX_EXPR_RE = re.compile(
    r"^[A-Za-z0-9.\-+()]+(?:\s+(?:AND|OR|WITH)\s+[A-Za-z0-9.\-+()]+)*$"
)
LICENSE_REF_RE = re.compile(r"[^A-Za-z0-9.\-]")


def load_licenses(path: str) -> dict[tuple[str, str], str]:
    """Map (name, version) -> SPDX expression for every crate cargo knows about."""
    with open(path, encoding="utf-8") as handle:
        metadata = json.load(handle)

    licenses = {}
    for package in metadata.get("packages", []):
        raw = (package.get("license") or "").strip()
        if not raw:
            continue
        # Pre-SPDX crates use "MIT/Apache-2.0" (librocksdb-sys ships
        # "MIT/Apache-2.0/BSD-3-Clause"); the slash means OR.
        licenses[(package["name"], package["version"])] = re.sub(r"\s*/\s*", " OR ", raw)
    return licenses


def license_ref(name: str, version: str) -> str:
    return "LicenseRef-" + LICENSE_REF_RE.sub("-", f"{name}-{version}".strip("-"))


def dedupe_syft_json(doc: dict) -> int:
    """Collapse artifacts sharing a (name, version, type), keeping the first."""
    kept, remap, artifacts = {}, {}, []
    for artifact in doc.get("artifacts", []):
        key = (artifact.get("name"), artifact.get("version"), artifact.get("type"))
        winner = kept.get(key)
        if winner is None:
            kept[key] = artifact
            artifacts.append(artifact)
            continue
        # Fold the loser's evidence into the winner so no location is lost.
        seen = {json.dumps(loc, sort_keys=True) for loc in winner.get("locations", [])}
        for location in artifact.get("locations", []):
            if json.dumps(location, sort_keys=True) not in seen:
                winner.setdefault("locations", []).append(location)
        remap[artifact.get("id")] = winner.get("id")

    removed = len(doc.get("artifacts", [])) - len(artifacts)
    doc["artifacts"] = artifacts

    if remap:
        relationships, seen = [], set()
        for rel in doc.get("artifactRelationships", []):
            rel["parent"] = remap.get(rel.get("parent"), rel.get("parent"))
            rel["child"] = remap.get(rel.get("child"), rel.get("child"))
            if rel["parent"] == rel["child"]:
                continue
            fingerprint = (rel["parent"], rel["child"], rel.get("type"))
            if fingerprint not in seen:
                seen.add(fingerprint)
                relationships.append(rel)
        doc["artifactRelationships"] = relationships
    return removed


def dedupe_spdx_json(doc: dict) -> int:
    """Collapse packages sharing a (name, versionInfo), keeping the first.

    Packages without a version (or versioned by digest, like the synthetic
    package Syft emits for the scanned binary itself) are left untouched.
    """
    kept, remap, packages = {}, {}, []
    for package in doc.get("packages", []):
        name, version = package.get("name"), package.get("versionInfo")
        if not name or not version or version.startswith("sha256:"):
            packages.append(package)
            continue
        winner = kept.get((name, version))
        if winner is None:
            kept[(name, version)] = package
            packages.append(package)
        else:
            remap[package.get("SPDXID")] = winner.get("SPDXID")

    removed = len(doc.get("packages", [])) - len(packages)
    doc["packages"] = packages

    if remap:
        relationships, seen = [], set()
        for rel in doc.get("relationships", []):
            for end in ("spdxElementId", "relatedSpdxElement"):
                rel[end] = remap.get(rel.get(end), rel.get(end))
            if rel["spdxElementId"] == rel["relatedSpdxElement"]:
                continue
            fingerprint = (
                rel["spdxElementId"],
                rel["relatedSpdxElement"],
                rel.get("relationshipType"),
            )
            if fingerprint not in seen:
                seen.add(fingerprint)
                relationships.append(rel)
        doc["relationships"] = relationships

        if "documentDescribes" in doc:
            described, seen = [], set()
            for spdx_id in doc["documentDescribes"]:
                spdx_id = remap.get(spdx_id, spdx_id)
                if spdx_id not in seen:
                    seen.add(spdx_id)
                    described.append(spdx_id)
            doc["documentDescribes"] = described
    return removed


def enrich_syft_json(doc: dict, licenses: dict) -> int:
    matched = 0
    for artifact in doc.get("artifacts", []):
        if artifact.get("licenses"):
            continue
        expression = licenses.get((artifact.get("name"), artifact.get("version")))
        if not expression:
            continue
        artifact["licenses"] = [
            {
                "value": expression,
                "spdxExpression": expression if SPDX_EXPR_RE.match(expression) else "",
                "type": "declared",
                "urls": [],
                "locations": [],
            }
        ]
        matched += 1
    return matched


def enrich_spdx_json(doc: dict, licenses: dict) -> int:
    matched = 0
    extracted = {}
    for package in doc.get("packages", []):
        if package.get("licenseDeclared") not in (None, "", "NOASSERTION"):
            continue
        name, version = package.get("name"), package.get("versionInfo")
        expression = licenses.get((name, version))
        if not expression:
            continue

        if SPDX_EXPR_RE.match(expression):
            package["licenseDeclared"] = expression
        else:
            ref = license_ref(name, version)
            package["licenseDeclared"] = ref
            extracted[ref] = expression
        matched += 1

    if extracted:
        existing = doc.setdefault("hasExtractedLicensingInfos", [])
        known = {entry.get("licenseId") for entry in existing}
        for ref, text in sorted(extracted.items()):
            if ref not in known:
                existing.append({"licenseId": ref, "extractedText": text})
    return matched


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2

    licenses = load_licenses(sys.argv[1])
    print(f"cargo metadata: {len(licenses)} crates declare a license", file=sys.stderr)
    if not licenses:
        print(f"error: no licenses found in {sys.argv[1]}", file=sys.stderr)
        return 1

    for path in sys.argv[2:]:
        with open(path, encoding="utf-8") as handle:
            doc = json.load(handle)

        if "artifacts" in doc:
            removed = dedupe_syft_json(doc)
            matched = enrich_syft_json(doc, licenses)
            total = len(doc["artifacts"])
        elif "packages" in doc:
            removed = dedupe_spdx_json(doc)
            matched = enrich_spdx_json(doc, licenses)
            total = len(doc["packages"])
        else:
            print(f"error: {path} is neither syft-json nor spdx-json", file=sys.stderr)
            return 1

        with open(path, "w", encoding="utf-8") as handle:
            json.dump(doc, handle)
        print(
            f"{path}: {matched}/{total} licensed, {removed} duplicates removed",
            file=sys.stderr,
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
