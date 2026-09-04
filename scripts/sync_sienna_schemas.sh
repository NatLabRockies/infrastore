#!/usr/bin/env bash
# Refreshes crates/infrastore-core/sienna_schemas/ from a local SiennaSchemas checkout.
# Maintainer-run only: never wired into build.rs or CI (this repo's policy is
# no network/build-time fetching; see crates/infrastore-core/sienna_schemas/SOURCE.md).
#
# Usage: scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
#   Default source path: ../SiennaSchemas (sibling of this repo checkout)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_ARG="${1:-$REPO_ROOT/../SiennaSchemas}"
DEST="$REPO_ROOT/crates/infrastore-core/sienna_schemas"

if [ ! -d "$SRC_ARG" ]; then
  echo "error: source checkout not found at $SRC_ARG" >&2
  echo "usage: scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]" >&2
  exit 1
fi
SRC="$(cd "$SRC_ARG" && pwd)"

# Exact file set (relative to $SRC), preserving directory structure so the
# $refs among them keep resolving:
#   TimeSeries/*.json (six per-type schemas + common.json + the oneOf wrapper)
#   Core/Associations/SupplementalAttributeAssociation.json
#   Core/common.json (trimmed: only UnitSystem is $ref'd by any vendored
#     schema, so we vendor just that definition rather than the full
#     1000+-line core-component schema; see TRIMMED_DEFINITIONS below)
FILES=(
  "TimeSeries/Deterministic.json"
  "TimeSeries/DeterministicSingleTimeSeries.json"
  "TimeSeries/NonSequentialTimeSeries.json"
  "TimeSeries/Probabilistic.json"
  "TimeSeries/Scenarios.json"
  "TimeSeries/SingleTimeSeries.json"
  "TimeSeries/common.json"
  "TimeSeries/TimeSeriesAssociation.json"
  "Core/Associations/SupplementalAttributeAssociation.json"
)

# Definitions to keep from Core/common.json, trimmed out of the file it's
# copied from. Extend this list (and re-run) if a future vendored schema
# $refs another definition from that file.
TRIMMED_DEFINITIONS=("UnitSystem")

rm -rf "$DEST"
mkdir -p "$DEST/TimeSeries" "$DEST/Core/Associations"

for f in "${FILES[@]}"; do
  src_file="$SRC/$f"
  if [ ! -f "$src_file" ]; then
    echo "error: expected schema file missing from source checkout: $src_file" >&2
    exit 1
  fi
  cp "$src_file" "$DEST/$f"
done

CORE_COMMON_SRC="$SRC/Core/common.json"
if [ ! -f "$CORE_COMMON_SRC" ]; then
  echo "error: expected schema file missing from source checkout: $CORE_COMMON_SRC" >&2
  exit 1
fi
python3 - "$CORE_COMMON_SRC" "$DEST/Core/common.json" "${TRIMMED_DEFINITIONS[@]}" <<'PYEOF'
import json
import sys

src_path, dest_path, *keep = sys.argv[1:]
with open(src_path) as f:
    data = json.load(f)

# Follow whichever key the source uses rather than hardcoding one. Upstream
# moved `definitions` -> `$defs`, and the vendored copy has to keep the source's
# spelling verbatim: the TimeSeries schemas `$ref` into this file by that exact
# pointer, so trimming into the other key silently dangles every one of them.
defs_key = next((k for k in ("$defs", "definitions") if k in data), None)
if defs_key is None:
    sys.exit(f"error: {src_path} has neither a $defs nor a definitions block")

missing = [name for name in keep if name not in data[defs_key]]
if missing:
    sys.exit(f"error: {defs_key} not found in {src_path}: {missing}")

trimmed = {"$schema": data["$schema"]}
# `id` disappeared upstream; carry it only when the source still has one.
if "id" in data:
    trimmed["id"] = data["id"]
trimmed[defs_key] = {name: data[defs_key][name] for name in keep}
with open(dest_path, "w") as f:
    f.write(json.dumps(trimmed, indent=2, ensure_ascii=False) + "\n")
PYEOF

cd "$SRC"
SOURCE_REF="$(git rev-parse HEAD)"
cd - >/dev/null

SOURCE_MD="$DEST/SOURCE.md"
{
  echo "# Vendored SiennaSchemas wire-format specs"
  echo
  echo "Vendored copy of the TimeSeries and supplemental-attribute-association wire-format"
  echo "specs from SiennaSchemas, used to validate the OpenAPI row fixtures in"
  echo "\`conformance/openapi_row_fixtures/\` (see"
  echo "\`crates/infrastore-core/tests/openapi_schema_conformance.rs\`). infrastore has no"
  echo "build-time or CI network access (\`deny.toml\` denies unknown sources and CI"
  echo "provisions nothing on any platform), so this is a maintainer-run sync rather than a"
  echo "live fetch, mirroring the \`conformance/\` + \`julia/generate_artifacts.jl\` precedent."
  echo
  echo "- **Source repo**: upstream is \`Sienna-Platform/SiennaSchemas\`. The sync script"
  echo "  vendors whatever local checkout is passed to it."
  echo "- **Source commit**: \`$SOURCE_REF\`"
  echo "- **Sync note**: the vendored copy may include un-merged upstream changes from the"
  echo "  local checkout used."
  echo "- **Synced**: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  echo "## Refreshing"
  echo
  echo "Run from the repository root:"
  echo
  echo '```bash'
  echo "scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]"
  echo '```'
  echo
  echo "Defaults to \`../SiennaSchemas\` (a sibling checkout) when no path is given. The"
  echo "script copies exactly the files above, preserving relative structure so their"
  echo "\`\$ref\`s keep resolving, and rewrites this file."
} >"$SOURCE_MD"

if command -v dprint >/dev/null 2>&1; then
  dprint fmt "$SOURCE_MD"
else
  echo "note: dprint not found on PATH; run 'dprint fmt $SOURCE_MD' before committing" >&2
fi

echo "synced $DEST from $SRC @ $SOURCE_REF"
