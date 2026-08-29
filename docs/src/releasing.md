# Releasing

infrastore ships from one repository to three registries, plus prebuilt binaries on the repository's
own releases. This page is the procedure.

| Channel  | Package(s)                                                                                     | Registry        |
| -------- | ---------------------------------------------------------------------------------------------- | --------------- |
| Rust     | `infrastore-core`, `infrastore-proto`, `infrastore-ffi`, `infrastore-server`, `infrastore-cli` | crates.io       |
| Python   | `infrastore`                                                                                   | PyPI            |
| Julia    | `InfraStore` (binaries via `Artifacts.toml` → GitHub Releases)                                 | Julia General   |
| Binaries | `infrastore`, `infrastore-server`, `libinfrastore_ffi` + header                                | GitHub Releases |

`infrastore-py` and `infrastore-bench` set `publish = false`: the first ships as the `infrastore`
wheel on PyPI rather than as a crate, and the second is an internal benchmarking tool.

## Versioning

The Rust crates, `InfraStore.jl`, and the Python package share the workspace version. Bump them
together and tag the repo once per release, so every registry pins the same commit.

The version lives in four places that must agree, and `cargo release` (below) writes all four:

| File                                  | Field                                                         | Written by                            |
| ------------------------------------- | ------------------------------------------------------------- | ------------------------------------- |
| `Cargo.toml`                          | `[workspace.package] version`                                 | cargo-release, natively               |
| `Cargo.toml`                          | `[workspace.dependencies]` pins on `infrastore-core`/`-proto` | cargo-release, natively               |
| `crates/infrastore-py/pyproject.toml` | `[project] version`                                           | `crates/infrastore-core/release.toml` |
| `julia/InfraStore.jl/Project.toml`    | `version`                                                     | `crates/infrastore-core/release.toml` |

The `[workspace.dependencies]` pins are easy to miss and fail late: `cargo publish` uploads
`infrastore-core` at the new version, then rejects `infrastore-proto` because its requirement still
names the old one. cargo-release moves `Cargo.lock` in the same commit.

Two more version strings ride along, neither load-bearing: the `VERSION=v0.8.0` download example in
`docs/src/getting-started/installation.md` and in `docs/src/how-to/use-cli.md`, so a version's
published docs point at that version's own binaries.

Two workflows guard this. `crates-release` refuses to publish if the tag does not match the
workspace version, and `python-wheels` opens with a `versions agree` job that parses all four files
above and fails unless they agree with each other and with the tag.

That job exists because v0.5.0 was tagged with `pyproject.toml` still at `0.4.0`. maturin lets
`[project] version` win over the Cargo workspace version, so every wheel job built and tested
`0.4.0` artifacts and passed; only the upload caught it, with `400 File already exists`, because
`0.4.0` was long since on PyPI. A PyPI filename can never be reused, so that tag was unpublishable
and the release had to move to `0.5.1`. The guard runs before the wheel matrix, so the same mistake
now costs seconds instead of a burnt version number.

## HDF5 linkage

Every distribution channel — the Rust crates, the Python wheel, and `InfraStore_jll` — ships with
the `vendored` feature: HDF5 and zlib are compiled from source and linked statically.
`pip install infrastore` and `cargo add infrastore-core` need no system libraries, only `cmake` and
a C compiler at build time, and every channel is backed by the exact HDF5 version infrastore was
tested against rather than whatever the target environment resolves.

For the JLL this is a deliberate departure from Julia-ecosystem convention (linking `HDF5_jll`),
made for two reasons:

- **Format control.** The store is a data artifact with a compatibility contract
  (`DATA_FORMAT_VERSION`); pinning the HDF5 that backs it removes an entire class of
  environment-dependent behavior. An `HDF5_jll` upgrade in a user's environment must not change how
  infrastore files are read or written.
- **No MPI dependency.** `HDF5_jll` is MPI-augmented and publishes no serial variant, so linking it
  forces an MPI runtime dependency and a 17-triplet build matrix onto a library that never calls MPI
  — and propagates that dependency to `InfraStore.jl` and InfrastructureSystems.jl.

Two libhdf5 copies in one Julia process (ours plus HDF5.jl's) is safe here: the cdylib exports only
its own `infrastore_*` symbols — the statically linked HDF5 symbols stay local, so nothing can
cross-resolve. The one scenario that is genuinely hazardous, opening a live store's `.h5` file
directly with HDF5.jl/NCDatasets.jl while a `Store` handle is open, is explicitly unsupported.

> **Never set `HDF5_DIR` in CI.** The vendored HDF5 build forwards it to cmake as `HDF5_ROOT` while
> still requesting static libraries; against a shared-only install such as conda-forge's this fails
> with `Could NOT find HDF5 (missing: HDF5_LIBRARIES HDF5_HL_LIBRARIES)`. To build against system
> libraries, use `--no-default-features` instead.

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

The bump is automated with [cargo-release](https://github.com/crate-ci/cargo-release)
(`cargo install cargo-release`), configured by `release.toml` at the workspace root and
`crates/infrastore-core/release.toml`. From a clean tree on a branch:

```sh
cargo release minor            # dry run -- prints every edit, changes nothing
cargo release minor --execute  # 0.8.0 -> 0.9.0, one commit
```

`patch` and `major` work the same way, and an explicit `cargo release 1.2.3 --execute` sets a
version outright. That one command rewrites `[workspace.package] version`, the
`[workspace.dependencies]` pins, `Cargo.lock`, `pyproject.toml`, `InfraStore.jl`'s `Project.toml`,
and the two docs download examples, then commits them as `Release v0.9.0`. The pre-commit hook runs
rustfmt, Clippy, dprint, and shellcheck on the way through.

Three details of the configuration are deliberate:

- **It does not publish.** `crates-release.yml` and `python-wheels.yml` own that, on the tag, with
  trusted publishing and no local credentials. Rehearse packaging separately with
  `cargo publish --workspace --dry-run`, which verifies every crate packages and builds.
- **It does not tag or push.** The tag drives all three release workflows, so it has to name the
  commit that ends up on `main` — not the local bump commit, which changes identity when the pull
  request merges.
- **It refuses to run on a dirty tree.** Untracked scratch files count; park them in
  `.git/info/exclude` if you keep any.

So open the bump as a pull request, and once it is merged:

```sh
git switch main && git pull
git tag v0.9.0
git push origin v0.9.0
```

Pushing the tag triggers three workflows: `crates-release`, `python-wheels`, and `release`.

Tag the merge commit once its own CI is green. `crates-release` now enforces this itself (see
[step 2](#2-rust--cratesio)), so a tag on a red or untested commit fails the gate instead of
publishing — but it enforces it by _waiting_, so tagging ahead of CI just means the release job sits
there. Nothing is lost either way; the tag does not need to be re-pushed once Test finishes.

When a version string moves to a new file, add a `[[pre-release-replacements]]` entry for it in
`crates/infrastore-core/release.toml` — not in the root `release.toml`, where cargo-release would
resolve it relative to each of the seven crate directories in turn. Every entry requires at least
one match, so a file that gets renamed fails the release instead of quietly going stale.

### 2. Rust → crates.io

Handled by `.github/workflows/crates-release.yml` on the tag.

Its `verify-ci` job runs first and refuses to publish a commit that has not passed `Test` on all
three platforms. The check is worth understanding, because the two workflows are joined by commit
rather than by ref: `crates-release` fires on the tag, but `test.yml` fires on pushes to `main` and
`dev` and never on tags, so there is no Test run attached to the tag itself. `verify-ci` looks up
the run for the **commit** the tag names — which exists because the release commit reaches `main`
before it is tagged — and requires `conclusion == "success"`.

It fails closed. A tag on a commit that never reached `main` has no Test run at all, and that is an
error rather than a pass; so is a run that is still going after 70 minutes. A tag pushed in the same
breath as the branch gets a 15-minute grace period for its run to appear before the job gives up.

This exists because an upload is irreversible in a way the rest of the pipeline is not: crates.io
never lets a version number be reused, so publishing from a broken commit burns that version
permanently, and the recovery is to abandon it and release the next one — which is what happened to
v0.5.0 on the PyPI side. Waiting for CI by hand worked only as long as whoever pushed the tag
remembered to; local checks are not a substitute either, since a maintainer runs them on one OS.

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

### 4. GitHub Release binaries

Handled by `.github/workflows/release.yml` on the tag. It builds the `infrastore` CLI, the
`infrastore-server` binary, and the `libinfrastore_ffi` cdylib plus its generated header, then
attaches one archive per platform — each with a `.sha256` sidecar — to a **draft** GitHub Release
with generated release notes. Review the notes and publish the draft by hand.

Because the workflow creates the draft itself, cut releases by pushing the tag rather than by
authoring a release in the GitHub UI first. A hand-made release is not a shortcut that saves the
wait — it is an **empty** release until `create-release` runs, and that job is gated on the whole
build matrix, so the assets do not exist until the slowest target (Windows) finishes. Two things
follow from that, both of which have bitten a release:

- **Step 5 below cannot start early.** `generate_artifacts.jl` downloads every
  `libinfrastore_ffi.<triplet>.tar.gz` to hash it, so against an assetless release it fails, and
  against a partially uploaded one it would hash whatever happens to be there.
- **Publishing it early does not stick reliably.** `softprops/action-gh-release` updates the
  existing release for the tag rather than erroring, and it sends `draft: true` in that update. It
  has left an already-published release published, but do not count on that: check the release's
  state after the workflow finishes, and publish it (again, if need be) once the assets are
  attached.

The supported sequence is: push the tag, wait for `release` to go green, then review and publish the
draft it left.

| Target                      | Runner           | Archive contents          |
| --------------------------- | ---------------- | ------------------------- |
| `aarch64-apple-darwin`      | `macos-14`       | executables and C library |
| `x86_64-unknown-linux-musl` | `ubuntu-latest`  | executables only          |
| `x86_64-unknown-linux-gnu`  | `ubuntu-latest`  | C library only            |
| `x86_64-pc-windows-msvc`    | `windows-latest` | executables and C library |

Linux is built twice on purpose. musl gives statically linked executables that run on any
distribution, including HPC login nodes with an older glibc than the runner — but a musl-built
cdylib loaded into a glibc Julia or Python process puts two C libraries in one address space, so the
shared library comes from a separate gnu build. On macOS the packaging step rewrites the dylib's
`LC_ID_DYLIB` to `@rpath/libinfrastore_ffi.dylib`; cargo otherwise bakes in the runner's absolute
build path.

The workflow builds selected packages rather than `--workspace`, which would drag in the PyO3 cdylib
(it needs an interpreter to link against, and ships as a wheel instead) and `infrastore-bench`. It
also uses `--locked`, so a release builds exactly what CI tested.

The same workflow deploys **versioned documentation** to a `/infrastore/<tag>/` subdirectory of
`gh-pages` and updates `versions.json`, which drives the docs version picker. It keeps the five most
recent releases and prunes older ones from both the manifest and disk. It shares the `pages`
concurrency group with `docs.yml`, which owns the `latest` build from `main`; the two must not push
to `gh-pages` at once.

To rehearse the builds without cutting a release, run the workflow manually — the `create-release`
and docs jobs are both gated on a tag ref, so a `workflow_dispatch` run only exercises the matrix.

### 5. Julia → General

`InfraStore.jl` ships its binaries as a self-hosted artifact: `julia/InfraStore.jl/Artifacts.toml`
names one `libinfrastore_ffi.<triplet>.tar.gz` per platform, built and attached to the GitHub
Release by `release.yml` on the tag. No JLL and no Yggdrasil review sits in the release path; the
only human gates are General's one-time three-day review of a new package and the 15-minute
AutoMerge on every version after. (The Yggdrasil recipe still exists for the day its PR merges — see
[Switching back to the JLL](#switching-back-to-the-jll).)

The ordering wrinkle this flow exists to solve: `Artifacts.toml` cannot be in the tagged commit,
because its URLs and hashes do not exist until the tag's binaries are built and uploaded.
Registration is therefore decoupled from the tag — Registrator registers whatever commit the comment
lands on:

1. **Publish the GitHub Release** for the tag (CI leaves it as a draft). This must come first: a
   draft's asset URLs are not publicly downloadable.
2. **Regenerate `Artifacts.toml`** on a branch:

   ```sh
   julia julia/generate_artifacts.jl v0.6.0
   ```

3. **Run the suite against the artifact**, with `INFRASTORE_LIB` unset — this is the path users get,
   and CI's `julia-artifact` job only smoke-tests it (between releases the wrapper on `main` may
   call FFI exports the released binary does not carry yet, so the full suite cannot run in CI
   unconditionally):

   ```sh
   julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.instantiate()'
   julia --project=julia/InfraStore.jl julia/InfraStore.jl/test/runtests.jl
   ```

4. **Merge, then comment on the merged commit** with
   [Registrator](https://github.com/JuliaRegistries/Registrator.jl), passing the subdirectory, which
   is required because the package is not at the repository root:

   ```
   @JuliaRegistrator register subdir=julia/InfraStore.jl
   ```

   The Registrator GitHub app must be installed on the repository (an org owner approves that); the
   JuliaHub web interface is the fallback.

   **Write the release notes into that same comment.** They are read from the trigger comment and
   nowhere else — there is no TagBot here and the GitHub Release's own notes are not consulted, so
   notes omitted at this point can only be restored by editing the registry PR body by hand, between
   its `<!-- BEGIN RELEASE NOTES -->` markers, before AutoMerge closes it. Put a blank line after
   the register line, then a `Release notes:` line, then the notes:

   ```
   @JuliaRegistrator register subdir=julia/InfraStore.jl

   Release notes:

   ## Breaking changes

   - ...
   ```

   Write them for someone consuming the Julia package, not for someone reading this repository's
   commits: what breaks, what is new, and what an existing artifact costs. A `DATA_FORMAT_VERSION`
   bump belongs at the top of "Breaking changes" every time it happens — it rejects every store an
   earlier version wrote, and the Julia user has no other channel that tells them so.

General's AutoMerge requires a public repository, an OSI-approved license file in the package
directory, and `[compat]` bounds for every non-stdlib dependency including `julia`. There is no
initial-version requirement — only prerelease and build metadata are rejected — so a package may
first register at any plain version (this one registered at 0.6.0). New packages sit a three-day
waiting period before merge. AutoMerge installs and loads the package on Linux x86_64, which
downloads the artifact, so a wrong hash or URL in `Artifacts.toml` fails registration instead of
shipping.

**Release assets are permanent.** Every registered version's `Artifacts.toml` points at this
repository's release URLs forever. Deleting an asset or a release — or moving the repository without
a redirect — breaks `Pkg.add` for every registered version that references it.

#### Switching back to the JLL

The Yggdrasil route was the original plan and remains the eventual destination; it stalled because
no maintainer would review a Rust recipe (see `JULIA_ARTIFACT_PLAN.md` for the full history). The
recipe lives on under `yggdrasil/`, pinned to the release it was last synced with. When the
[Yggdrasil PR](https://github.com/JuliaPackaging/Yggdrasil/pull/14290) finally merges:

1. Refresh the recipe's `version` and `GitSource` SHA to the current release
   (`git rev-parse vX.Y.Z^{commit}`; Yggdrasil requires a full commit SHA, not a tag). Only changes
   under `crates/`, `Cargo.toml`, or `Cargo.lock` need a new SHA — edits to the recipe itself do
   not, since Yggdrasil builds from its own copy. To test it locally (BinaryBuilder needs Docker on
   macOS):

   ```sh
   cd yggdrasil
   julia build_tarballs.jl --verbose --debug x86_64-linux-gnu
   ```

   A platform argument replaces the recipe's `platforms` list rather than filtering it; the listed
   platforms carry no extra tags, so bare triplets are exactly right.

2. Once `InfraStore_jll` is registered, cut the next `InfraStore.jl` version: delete
   `Artifacts.toml` and `julia/generate_artifacts.jl`, swap `lib_path()` to the JLL (the shape is
   parked on the `julia-jll-dep` branch), add `InfraStore_jll` to `[deps]` with a `[compat]` bound
   matching the version the JLL **first registers as** (check the registry: a bound below the
   earliest published version resolves to nothing), and drop the `julia-artifact` CI job. JLL UUIDs
   are deterministic — `BinaryBuilder.jll_uuid("InfraStore_jll")`.

3. Register that version with the same Registrator comment; it auto-merges in about 15 minutes. The
   artifact-era release assets stay up forever regardless (see above).

The recipe builds with the default `vendored` feature — see [HDF5 linkage](#hdf5-linkage) for why
the JLL statically links its own HDF5/zlib instead of depending on `HDF5_jll`. Expect Yggdrasil
reviewers to ask about that; the rationale is written out in the recipe's header comment. `HDF5_DIR`
must remain unset during the build. The recipe patches one thing in the source tree: it drops sha2's
`asm` feature, because BinaryBuilder forbids forcing an arch via `-march` and the ARMv8 crypto
kernels cannot be assembled there (x86-64 still detects SHA-NI at runtime).

### 6. Downstream

`InfrastructureSystems.jl` depends on `InfraStore.jl` for its Rust time-series backend: add
`InfraStore` to its `[deps]` and replace the raw `ccall`s in `src/rust_time_series_store.jl` with
calls into the package. `infrasys` consumes the PyPI wheel.
