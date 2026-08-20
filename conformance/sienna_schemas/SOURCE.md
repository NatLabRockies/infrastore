# Vendored SiennaSchemas wire-format specs

Vendored copy of the TimeSeries and supplemental-attribute-association wire-format specs from
SiennaSchemas, used to validate the OpenAPI row fixtures in `conformance/openapi_row_fixtures/` (see
`crates/infrastore-core/tests/openapi_schema_conformance.rs`). infrastore has no build-time or CI
network access (`deny.toml` denies unknown sources and CI provisions nothing on any platform), so
this is a maintainer-run sync rather than a live fetch, mirroring the `conformance/` +
`julia/generate_artifacts.jl` precedent.

- **Source repo**: `/Users/jdlara/cache/psy6/SiennaSchemas` (local checkout; upstream is
  NatLabRockies/SiennaSchemas at the time of writing, but this vendors whatever checkout is passed
  to the sync script).
- **Source commit**: `d395f6192ea4aae2d4993267970a828e5d90965e`
- **Dirty working tree at sync time**: the source checkout had uncommitted changes when this copy
  was made, vendored as-is (working-tree state, not the commit above). Modified files:

  - `Core/Associations/SupplementalAttributeAssociation.json`
  - `TimeSeries/Deterministic.json`
  - `TimeSeries/DeterministicSingleTimeSeries.json`
  - `TimeSeries/NonSequentialTimeSeries.json`
  - `TimeSeries/Probabilistic.json`
  - `TimeSeries/Scenarios.json`
  - `TimeSeries/SingleTimeSeries.json`
  - `TimeSeries/common.json`
  - `scripts/validate_units.py`
  - `docs/sienna-units-composite.pptx`
  - `docs/sienna-units-qxt.pptx`
- **Synced**: 2026-08-20T05:32:36Z

## Refreshing

Run from the repository root:

```bash
scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
```

Defaults to `../SiennaSchemas` (a sibling checkout) when no path is given. The script copies exactly
the files above, preserving relative structure so their `$ref`s keep resolving, and rewrites this
file.
