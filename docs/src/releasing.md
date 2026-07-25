# Releasing

infrastore ships from one repository to three registries. This page is the procedure.

| Channel | Package(s)                                                                                     | Registry      |
| ------- | ---------------------------------------------------------------------------------------------- | ------------- |
| Rust    | `infrastore-core`, `infrastore-proto`, `infrastore-ffi`, `infrastore-server`, `infrastore-cli` | crates.io     |
| Python  | `infrastore`                                                                                   | PyPI          |
| Julia   | `InfraStore_jll`, then `InfraStore`                                                            | Julia General |

`infrastore-py` and `infrastore-bench` set `publish = false`: the first ships as the `infrastore`
wheel on PyPI rather than as a crate, and the second is an internal benchmarking tool.

## Versioning

The Rust crates, the JLL, `InfraStore.jl`, and the Python package share the workspace version. Bump
them together and tag the repo once per release, so every registry pins the same commit.

The version lives in three places that must agree:

| File                                  | Field                         |
| ------------------------------------- | ----------------------------- |
| `Cargo.toml`                          | `[workspace.package] version` |
| `crates/infrastore-py/pyproject.toml` | `[project] version`           |
| `julia/InfraStore.jl/Project.toml`    | `version`                     |

The `crates-release` workflow refuses to publish if the tag does not match the workspace version.

## NetCDF linkage

The two distribution channels link NetCDF differently, and the choice is **not** interchangeable:

| Channel                   | Linkage                                  | How                                                                  |
| ------------------------- | ---------------------------------------- | -------------------------------------------------------------------- |
| Rust crates, Python wheel | vendored netcdf-c + HDF5 + zlib, static  | the `vendored` feature, enabled by default in every crate            |
| `InfraStore_jll` (Julia)  | dynamic, against `NetCDF_jll`/`HDF5_jll` | `cargo build --no-default-features` in `yggdrasil/build_tarballs.jl` |

Vendoring is what lets a downstream consumer get NetCDF for free — `pip install infrastore` and
`cargo add infrastore-core` need no system libraries, only `cmake` and a C compiler at build time.

The JLL is the exception. Julia's ecosystem already ships HDF5, and a statically vendored copy
inside `libinfrastore_ffi` would put **two libhdf5 instances in one process** alongside any other
JLL that links it. The recipe therefore passes `--no-default-features`; that flag is load-bearing.

> **Never set `HDF5_DIR` or `NETCDF_DIR` in CI.** The vendored netcdf-c build forwards them to cmake
> as `HDF5_ROOT` while still requesting static libraries; against a shared-only install such as
> conda-forge's this fails with `Could NOT find HDF5 (missing: HDF5_LIBRARIES HDF5_HL_LIBRARIES)`.
> To build against system libraries, use `--no-default-features` instead.

### Multiple HDF5 copies in one Python interpreter

The same hazard applies in reverse to the wheels, which carry a statically linked HDF5 while a
typical downstream environment also has `netCDF4` and `h5py`, each bundling its own. This is a
tested property, not an assumption: `python/tests/test_hdf5_interop.py` drives real reads and writes
through all three libraries in one process, in both initialization orders, and cibuildwheel runs the
suite against every built wheel in its target environment (`test-requires` / `test-command` in
`pyproject.toml`). Imports alone are not enough — on Linux a collision surfaces as silent symbol
interposition rather than a clean error, which is why the test exercises I/O.

If that check ever fails on a new platform, the fallbacks are to build that platform's wheels with
`--no-default-features` against system libraries, or to hide the HDF5 symbols with a version script.

**musllinux** stays in `skip` in `pyproject.toml`. Vendoring removed the original blocker (the
RPM-based `before-all` could not install HDF5 on musl), but the target has never been built, so
un-skipping it is a separate change that needs its own CI run.

## Cutting a release

### 1. Bump and tag

Update the three version fields above, then:

```sh
cargo publish --workspace --dry-run   # verifies every crate packages and builds
git commit -am "Release v0.1.0"
git tag v0.1.0
git push origin main --tags
```

Pushing the tag triggers both the `crates-release` and `python-wheels` workflows.

### 2. Rust → crates.io

Handled by `.github/workflows/crates-release.yml` on the tag.

`cargo publish --workspace` resolves the intra-workspace order itself (`infrastore-core` →
`infrastore-proto` → `infrastore-ffi` / `infrastore-server` / `infrastore-cli`) and waits for each
crate to land in the index before publishing its dependents, so the crates must not be published
individually.

Authentication uses crates.io [trusted publishing](https://crates.io/docs/trusted-publishing), which
needs a one-time setup per crate: on the crate's page, **Settings → Trusted Publishing → Add**, with
owner `NatLabRockies`, repository `infrastore`, workflow `crates-release.yml`, environment
`crates-io`. No token is stored in the repository.

> **Bootstrapping.** Unlike PyPI, crates.io has no "pending publisher" — a trusted publisher can
> only be attached to a crate that already exists, so the _first_ version of any new crate must be
> published by hand with an API token (`cargo publish --workspace` with `CARGO_REGISTRY_TOKEN` set).
> This is how v0.1.0 went out. It applies again only if a new crate joins the workspace, not to
> subsequent releases of the existing ones.

Because of that, and because a re-run of a release should not fail, the workflow first checks the
registry for each publishable crate at the workspace version and skips the upload entirely when they
are all present. Publishing a version that already exists is an error on crates.io, so without that
check, tagging after a manual publish would fail the job.

To rehearse without uploading, run the workflow manually with `dry_run` left checked.

### 3. Python → PyPI

Handled by `.github/workflows/python-wheels.yml` on the tag. cibuildwheel builds one abi3 wheel per
platform, runs the full pytest suite against each, and the `publish` job uploads to PyPI via trusted
publishing (environment `pypi`).

The abi3 floor is `abi3-py311`, set in three places that must agree: the `pyo3` feature in
`crates/infrastore-py/Cargo.toml`, `build = "cp311-*"` in `pyproject.toml`, and `requires-python`.

### 4. Julia → General

Two registrations, in order. The JLL must exist before `InfraStore.jl` can depend on it.

**4a. `InfraStore_jll`** — the compiled `libinfrastore_ffi`, one binary per platform.

1. Update the `GitSource` SHA in `yggdrasil/build_tarballs.jl` to the release commit
   (`git rev-parse v0.1.0^{commit}`) and `version` to match. Yggdrasil requires a full commit SHA; a
   tag name is not accepted. Only changes under `crates/`, `Cargo.toml`, or `Cargo.lock` need a new
   SHA — edits to the recipe itself do not, since Yggdrasil builds from its own copy of it.
2. Test from inside a Yggdrasil checkout — **not** from this repository. The recipe's
   `YGGDRASIL_DIR` include of `platforms/mpi.jl` resolves relative to the Yggdrasil tree:
   ```sh
   git clone https://github.com/JuliaPackaging/Yggdrasil
   mkdir -p Yggdrasil/I/InfraStore
   cp yggdrasil/build_tarballs.jl Yggdrasil/I/InfraStore/
   cd Yggdrasil/I/InfraStore
   julia build_tarballs.jl --verbose --debug \
       x86_64-linux-gnu-libgfortran5-cxx11-mpi+mpich
   ```
   **Pass the full tagged triplet.** A platform argument does not filter the recipe's `platforms`
   list, it replaces it, so a bare `x86_64-linux-gnu` builds for an untagged platform that no
   `HDF5_jll` artifact matches. Nothing is then installed into `${prefix}` and the build dies far
   away, inside cargo, with `hdf5-metno-sys` reporting an `Invalid HDF5 headers directory` — while
   the recipe's own platform list is never consulted. Omit the argument to build all 17.
3. Copy the recipe into a [Yggdrasil](https://github.com/JuliaPackaging/Yggdrasil) fork under
   `I/InfraStore/` and open a PR. Their CI builds every platform in the list and all must pass.
   Merging is done by Yggdrasil maintainers, so allow for review time.

`NetCDF_jll` and `HDF5_jll` are both MPI-augmented and publish only tagged artifacts, so the recipe
mirrors that: it runs its platforms through `MPI.augment_platforms` and pins
`libgfortran_version`/`cxxstring_abi` to the single combination `HDF5_jll` ships. Those tags are not
a claim about this crate's ABI — it is a pure C ABI cdylib — they are the coordinates BinaryBuilder
selects a dependency's artifact by. Re-check them against `HDF5_jll`'s release assets whenever its
compat bound moves.

The recipe patches two things in the source tree, both deliberate:

- it neutralizes `hdf5-metno-sys`'s runtime version probe, which `dlopen`s libhdf5 and therefore
  cannot work when cross-compiling — the header version from `HDF5_jll` is authoritative;
- it drops sha2's `asm` feature, because BinaryBuilder forbids forcing an arch via `-march` and the
  ARMv8 crypto kernels cannot be assembled there (x86-64 still detects SHA-NI at runtime).

**4b. `InfraStore.jl`** — the `ccall` wrapper and high-level API.

These changes are drafted below but **must not be applied until the JLL is registered** — adding a
dependency on a package General does not yet carry makes `Pkg.instantiate()` fail, which would break
the Julia test job.

**Step 1 — `julia/InfraStore.jl/Project.toml`.** Add the dependency and its compat bound:

```toml
 [deps]
 Dates = "ade2ca70-3891-5945-98fb-dc099432e06a"
+InfraStore_jll = "e72452fb-83b3-5caa-ac95-1fd73ac75842"
 JSON = "682c06a0-de6a-54ab-a142-c8b1cf79cde6"

 [compat]
+InfraStore_jll = "0.1"
 JSON = "0.21, 1"
 julia = "1.10"
```

JLL UUIDs are derived deterministically from the package name, so that value is already known —
`BinaryBuilder.jll_uuid("InfraStore_jll")`. (The same call reproduces the published UUIDs of
`NetCDF_jll` and `HDF5_jll` exactly, which is how it was checked.)

**Step 2 — `julia/InfraStore.jl/src/InfraStore.jl`.** Replace `_jll_library_path` and `lib_path`
with a direct import. `INFRASTORE_LIB` stays ahead of the JLL: that ordering is what lets a local
`cargo build` shadow the released binary with no code change, and the CI job relies on it.

```julia
import InfraStore_jll

const _LIB_REF = Ref{String}("")

"""
Path to the `libinfrastore_ffi` cdylib. The `INFRASTORE_LIB` environment
variable takes precedence (development builds); otherwise the
`InfraStore_jll` binary is used.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "INFRASTORE_LIB", "")
    if isempty(p)
        # The JLL is built for a fixed platform list, so a user on a target it
        # does not cover (musl, i686, armv7, ...) gets a loadable package with
        # no product. Say so, rather than failing later inside a `ccall`.
        InfraStore_jll.is_available() || error(
            "InfraStore_jll provides no binary for this platform. Build the " *
            "cdylib from source and point INFRASTORE_LIB at it.",
        )
        p = InfraStore_jll.libinfrastore_ffi
    end
    _LIB_REF[] = p
    return p
end
```

The `Base.identify_package` lookup this replaces exists only to let the package work before the JLL
is registered; once it is a real dependency, the soft lookup is dead weight.

**Step 3 — register.** Comment on the release commit with
[Registrator](https://github.com/JuliaRegistries/Registrator.jl), passing the subdirectory, which is
required because the package is not at the repository root:

```
@JuliaRegistrator register subdir=julia/InfraStore.jl
```

General's AutoMerge requires a public repository, an OSI-approved license file in the package
directory, `[compat]` bounds for every non-stdlib dependency including `julia`, and version `0.1.0`
for a new package. New packages also sit for a three-day waiting period before auto-merge.

### 5. Downstream

`InfrastructureSystems.jl` depends on `InfraStore.jl` for its Rust time-series backend: add
`InfraStore` to its `[deps]` and replace the raw `ccall`s in `src/rust_time_series_store.jl` with
calls into the package. `infrasys` consumes the PyPI wheel.
