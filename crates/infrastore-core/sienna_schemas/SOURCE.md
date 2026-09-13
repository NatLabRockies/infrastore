# Vendored SiennaSchemas wire-format specs

Vendored copy of the TimeSeries and supplemental-attribute-association wire-format specs from
SiennaSchemas, used to validate the OpenAPI row fixtures in `conformance/openapi_row_fixtures/` (see
`crates/infrastore-core/tests/openapi_schema_conformance.rs`). infrastore has no build-time or CI
network access (`deny.toml` denies unknown sources and CI provisions nothing on any platform), so
this is a maintainer-run sync rather than a live fetch, mirroring the `conformance/` +
`julia/generate_artifacts.jl` precedent.

- **Source repo**: `Sienna-Platform/SiennaSchemas`
- **Release**: `v0.1.0` (named by `.schema-version` at the repository root)
- **Release commit**: `9f589ad266f55e91c946378cd6a4c88ba3554119`
- **Sync note**: the content is `git archive` of the release tag, never a working tree, so it is the
  published release and nothing else. To move to a newer release, change `.schema-version` and
  re-run the sync.
- **Synced**: 2026-09-13T01:05:31Z

## Refreshing

Run from the repository root:

```bash
scripts/sync_sienna_schemas.sh [path-to-SiennaSchemas-checkout]
```

Defaults to `../SiennaSchemas` (a sibling checkout) when no path is given. The script copies exactly
the files above, preserving relative structure so their `$ref`s keep resolving, and rewrites this
file.
