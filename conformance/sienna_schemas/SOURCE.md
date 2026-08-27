# Vendored SiennaSchemas wire-format specs

Vendored copy of the TimeSeries and supplemental-attribute-association wire-format specs from
SiennaSchemas, used to validate the OpenAPI row fixtures in `conformance/openapi_row_fixtures/` (see
`crates/infrastore-core/tests/openapi_schema_conformance.rs`). infrastore has no build-time or CI
network access (`deny.toml` denies unknown sources and CI provisions nothing on any platform), so
this is a maintainer-run sync rather than a live fetch, mirroring the `conformance/` +
`julia/generate_artifacts.jl` precedent.

- **Source repo**: upstream is `Sienna-Platform/SiennaSchemas`. The sync script vendors whatever
  local checkout is passed to it.
- **Source commit**: `c8c2428a0d3c66a592dd2e8838cf65ba4021a6ea`
- **Sync note**: the vendored copy may include un-merged upstream changes from the local checkout
  used.
- **Synced**: 2026-08-21T04:55:40Z

## Pending upstream change

The time-series wire form now emits an `id` — the catalog row's own number, and the handle a
consumer stores to reference a series later — which these vendored schemas do not yet declare as a
property. Nothing fails today: the TimeSeries schemas leave `additionalProperties` at its permissive
default, so the field is tolerated rather than validated.

The row fixtures deliberately do not carry it. A fixture is a golden of one row's _content_, and an
id's value depends on how many rows were written before it, so pinning one would make the fixture
disagree with the same row exported from a differently-ordered store.

Re-sync once SiennaSchemas declares it, so `id` is validated rather than merely tolerated. Two
things to check when doing so:

- `id` should be an optional integer property on each of the six per-type TimeSeries schemas.
- `id` should join `TimeSeriesFeatures.propertyNames.not.enum` in `TimeSeries/common.json`,
  mirroring the store's own reserved feature names — a feature named `id` would otherwise shadow the
  row field.

`Core/Associations/SupplementalAttributeAssociation.json` needs no change: that wire form
deliberately carries no id, since nothing references an attachment.

## Refreshing

Run from the repository root:

```bash
scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
```

Defaults to `../SiennaSchemas` (a sibling checkout) when no path is given. The script copies exactly
the files above, preserving relative structure so their `$ref`s keep resolving, and rewrites this
file.
