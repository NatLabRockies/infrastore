#!/usr/bin/env bash
# Refreshes conformance/sienna_schemas/ from a local SiennaSchemas checkout.
# Maintainer-run only: never wired into build.rs or CI (this repo's policy is
# no network/build-time fetching; see conformance/sienna_schemas/SOURCE.md).
#
# Usage: scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
#   Default source path: ../SiennaSchemas (sibling of this repo checkout)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_ARG="${1:-$REPO_ROOT/../SiennaSchemas}"
DEST="$REPO_ROOT/conformance/sienna_schemas"

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
#   Core/common.json (referenced for UnitSystem)
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
  "Core/common.json"
)

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

cd "$SRC"
SOURCE_REF="$(git rev-parse HEAD)"
DIRTY_FILES="$(git status --porcelain)"
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
  echo "- **Source repo**: \`$SRC\` (local checkout; upstream is"
  echo "  NatLabRockies/SiennaSchemas at the time of writing, but this vendors whatever"
  echo "  checkout is passed to the sync script)."
  echo "- **Source commit**: \`$SOURCE_REF\`"
  if [ -n "$DIRTY_FILES" ]; then
    echo "- **Dirty working tree at sync time**: the source checkout had uncommitted"
    echo "  changes when this copy was made, vendored as-is (working-tree state, not the"
    echo "  commit above). Modified files:"
    echo
    while IFS= read -r line; do
      echo "  - \`${line:3}\`"
    done <<<"$DIRTY_FILES"
  else
    echo "- **Dirty working tree at sync time**: none; the source checkout was clean."
  fi
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
