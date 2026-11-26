#!/usr/bin/env sh
# CycloneDX SBOM regeneration (F041): one SBOM per workspace crate
# into sbom/. Rerun whenever Cargo.lock moves (cargo cyclonedx emits
# next to each crate's manifest; this script consolidates them).
# Advisory and license policy live in deny.toml (cargo deny check).
set -eu
rm -f sbom/*.cdx.json
cargo cyclonedx --describe crate --format json
mkdir -p sbom
for f in crates/*/*.cdx.json; do
    [ -e "$f" ] || { echo "error: no SBOMs were generated" >&2; exit 1; }
    mv "$f" sbom/
done
echo "SBOMs: $(ls sbom/*.cdx.json | wc -l)"
