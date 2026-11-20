#!/usr/bin/env python3
"""Generate a CycloneDX 1.5 SBOM for the RustVNC workspace.

M3 requires a machine-readable inventory of what ships. `cargo metadata` already
knows every crate, version, licence and checksum, so this needs no extra tool and
works the same locally and in CI.

    python3 scripts/sbom.py > sbom.cdx.json

The output lists every third-party crate that ends up in the shipped binaries,
plus the two workspace crates, with `purl`s so a scanner can match them.
"""

from __future__ import annotations

import json
import subprocess
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cargo_metadata() -> dict:
    output = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(output.stdout)


def purl(name: str, version: str) -> str:
    return f"pkg:cargo/{name}@{version}"


def licence(package: dict) -> str | None:
    expression = package.get("license")
    if expression:
        return expression
    return None


def component(package: dict, root_ids: set[str]) -> dict:
    name = package["name"]
    version = package["version"]
    is_workspace = package["id"] in root_ids
    entry = {
        "type": "application" if is_workspace else "library",
        "bom-ref": purl(name, version),
        "name": name,
        "version": version,
        "purl": purl(name, version),
        "scope": "required",
    }
    expression = licence(package)
    if expression:
        entry["licenses"] = [{"license": {"id": expression}}]
    checksum = (package.get("checksum") or "").strip()
    if checksum.startswith("sha256:"):
        entry["hashes"] = [{"alg": "SHA-256", "content": checksum.split(":", 1)[1]}]
    source = package.get("source")
    if source:
        entry["externalReferences"] = [{"type": "distribution", "url": source}]
    return entry


def main() -> int:
    metadata = cargo_metadata()
    root_ids = set(metadata.get("workspace_members", []))
    members = {package["id"] for package in metadata["packages"]}
    components = [
        component(package, root_ids)
        for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"]))
        if package["id"] in members
    ]
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid4()}",
        "version": 1,
        "metadata": {
            "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "tools": [{"vendor": "RustVNC", "name": "scripts/sbom.py", "version": "1"}],
            "component": {
                "type": "application",
                "name": "RustVNC",
                "bom-ref": "rustvnc-workspace",
            },
        },
        "components": components,
    }
    json.dump(bom, sys.stdout, indent=2, ensure_ascii=False)
    sys.stdout.write("\n")
    print(
        f"# {len(components)} components, {sum(1 for c in components if c['type'] == 'application')} workspace crates",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
