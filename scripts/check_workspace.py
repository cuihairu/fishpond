#!/usr/bin/env python3
"""Workspace structure gates: §3.2 member set and §3.3 layering rules.

Exits non-zero with a message for every violated rule. Run locally or in CI:

    python3 scripts/check_workspace.py
"""

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# §3.2: the authoritative set of Rust workspace members (package names).
EXPECTED_MEMBERS = {
    "fishpond-domain",
    "fishpond-provider-sdk",
    "fishpond-provider-official",
    "fishpond-provider-mock",
    "fishpond-storage",
    "fishpond-runtime",
    "fishpond-scheduler",
    "fishpond-router",
    "fishpond-operations",
    "fishpond-events",
    "fishpond-monitor",
    "fishpond-telemetry",
    "fishpond-mcp",
    "fishpond-server",
    "fishpond-worker",
    "fishpond-cli",
}

# §3.3: `operations` must never depend on the delivery/front layers.
FORBIDDEN_IN_OPERATIONS = {"fishpond-server", "fishpond-worker", "fishpond-router"}

# F009M: the MCP facade may only build on the service/domain/storage seam;
# never on worker/browser-side crates or provider implementations.
MCP_ALLOWED_INTERNAL = {"fishpond-domain", "fishpond-storage", "fishpond-server"}

# §3.3: provider implementations may only build on the SDK, domain and storage.
PROVIDER_ALLOWED_INTERNAL = {"fishpond-provider-sdk", "fishpond-domain", "fishpond-storage"}
PROVIDER_CRATES = {"fishpond-provider-official", "fishpond-provider-mock"}

DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")

errors: list[str] = []


def load_manifest(path: Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def check_domain_deps(deps: dict, table: str) -> None:
    if deps:
        errors.append(
            f"fishpond-domain has {table} {sorted(deps)}: §3.3 forbids external crates in domain"
        )


def check_operations(internal: set[str]) -> None:
    bad = internal & FORBIDDEN_IN_OPERATIONS
    if bad:
        errors.append(
            f"fishpond-operations depends on {sorted(bad)}: §3.3 forbids server/worker/router in operations"
        )


def check_provider(name: str, internal: set[str]) -> None:
    bad = internal - PROVIDER_ALLOWED_INTERNAL
    if bad:
        errors.append(
            f"{name} depends on {sorted(bad)}: §3.3 allows only "
            f"{sorted(PROVIDER_ALLOWED_INTERNAL)} in provider implementations"
        )


def check_mcp(internal: set[str]) -> None:
    bad = internal - MCP_ALLOWED_INTERNAL
    if bad:
        errors.append(
            f"fishpond-mcp depends on {sorted(bad)}: F009M allows only "
            f"{sorted(MCP_ALLOWED_INTERNAL)} in the MCP facade"
        )


# F030: the telemetry plane is a service-seam crate — never on the
# delivery/front layers.
TELEMETRY_FORBIDDEN = {"fishpond-server", "fishpond-worker", "fishpond-cli"}


def check_telemetry(internal: set[str]) -> None:
    bad = internal & TELEMETRY_FORBIDDEN
    if bad:
        errors.append(
            f"fishpond-telemetry depends on {sorted(bad)}: F030 forbids "
            "server/worker/cli in telemetry"
        )


def main() -> int:
    # Rule 1: workspace member set must equal the §3.2 list exactly.
    # Cargo declares members as directory paths, so compare paths against the
    # expected crates/<short-name> layout.
    root_manifest = load_manifest(ROOT / "Cargo.toml")
    member_paths = set(root_manifest.get("workspace", {}).get("members", []))
    expected_paths = {f"crates/{name.removeprefix('fishpond-')}" for name in EXPECTED_MEMBERS}
    missing = expected_paths - member_paths
    extra = member_paths - expected_paths
    if missing:
        errors.append(f"workspace is missing members {sorted(missing)} (§3.2)")
    if extra:
        errors.append(f"workspace has undeclared members {sorted(extra)} (§3.2)")

    # Rule 2+: per-crate package naming and layering rules.
    for member_path in sorted(member_paths):
        manifest_path = ROOT / member_path / "Cargo.toml"
        if not manifest_path.exists():
            errors.append(f"member {member_path}: {manifest_path} does not exist")
            continue
        manifest = load_manifest(manifest_path)
        name = manifest.get("package", {}).get("name")
        expected_name = "fishpond-" + member_path.removeprefix("crates/")
        if name != expected_name:
            errors.append(
                f"member {member_path}: package name is {name!r}, expected {expected_name!r}"
            )
            continue

        all_deps: set[str] = set()
        for table in DEPENDENCY_TABLES:
            deps = manifest.get(table, {})
            all_deps |= set(deps)
            if name == "fishpond-domain":
                check_domain_deps(deps, table)

        internal = {d for d in all_deps if d.startswith("fishpond-")}
        if name == "fishpond-operations":
            check_operations(internal)
        if name in PROVIDER_CRATES:
            check_provider(name, internal)
        if name == "fishpond-mcp":
            check_mcp(internal)
        if name == "fishpond-telemetry":
            check_telemetry(internal)

    for err in errors:
        print(f"ERROR: {err}", file=sys.stderr)
    if errors:
        print(f"workspace structure check failed ({len(errors)} error(s))", file=sys.stderr)
        return 1
    print(f"workspace structure ok: {len(EXPECTED_MEMBERS)} members, layering rules satisfied")
    return 0


if __name__ == "__main__":
    sys.exit(main())
