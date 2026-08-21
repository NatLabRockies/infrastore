# Vendored SiennaSchemas wire-format specs

Vendored copy of the TimeSeries and supplemental-attribute-association wire-format specs from
SiennaSchemas, used to validate the OpenAPI row fixtures in `conformance/openapi_row_fixtures/` (see
`crates/infrastore-core/tests/openapi_schema_conformance.rs`). infrastore has no build-time or CI
network access (`deny.toml` denies unknown sources and CI provisions nothing on any platform), so
this is a maintainer-run sync rather than a live fetch, mirroring the `conformance/` +
`julia/generate_artifacts.jl` precedent.

- **Source repo**: upstream is `Sienna-Platform/SiennaSchemas`. The sync script vendors whatever
  local checkout is passed to it.
- **Source commit**: `b2cc374a3498f539442d540500da0c8017e4ab1d`
- **Sync note**: the vendored copy may include un-merged upstream changes from the local checkout
  used.
- **Synced**: 2026-08-21T00:27:00Z

## Refreshing

Run from the repository root:

```bash
scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
```

Defaults to `../SiennaSchemas` (a sibling checkout) when no path is given. The script copies exactly
the files above, preserving relative structure so their `$ref`s keep resolving, and rewrites this
file.
