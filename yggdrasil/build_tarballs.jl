# Yggdrasil build recipe for InfraStore_jll.
#
# This produces the `libinfrastore_ffi` binary that `InfraStore.jl` (and,
# through it, InfrastructureSystems.jl) loads. It is built against the
# ecosystem's NetCDF_jll + HDF5_jll so there is a single libhdf5 in any Julia
# process (no Homebrew dependency, no version drift).
#
# ---------------------------------------------------------------------------
# THIS RECIPE ONLY RUNS FROM INSIDE A YGGDRASIL CHECKOUT.
#
# `YGGDRASIL_DIR` below resolves `platforms/mpi.jl` relative to the Yggdrasil
# tree, so `julia build_tarballs.jl` from this repository will fail on the
# include. To work on it:
#
#   git clone https://github.com/JuliaPackaging/Yggdrasil   # or your fork
#   mkdir -p Yggdrasil/I/InfraStore
#   cp build_tarballs.jl Yggdrasil/I/InfraStore/
#   cd Yggdrasil/I/InfraStore
#   julia build_tarballs.jl --verbose --debug x86_64-linux-gnu
#
# Yggdrasil groups recipes by first letter, hence `I/InfraStore/`.
# ---------------------------------------------------------------------------
#
# Why the MPI machinery below, for a library that never calls MPI:
# NetCDF_jll and HDF5_jll are both MPI-augmented -- they ship one artifact per
# MPI ABI (none/mpich/openmpi/mpitrampoline) and select between them with an
# `mpi` platform tag. A dependent that requests plain, untagged platforms has
# no artifact to resolve against, so nothing is installed into ${prefix} and
# the build fails later and confusingly (hdf5-metno-sys panicking on a missing
# H5pubconf.h). Any consumer of those JLLs has to mirror the augmentation.

using BinaryBuilder, Pkg
using Base.BinaryPlatforms

const YGGDRASIL_DIR = "../.."
include(joinpath(YGGDRASIL_DIR, "platforms", "mpi.jl"))

name = "InfraStore"
version = v"0.1.0"

# Pin to the commit the release tag points at. Yggdrasil requires a full commit
# SHA here -- a tag name is not accepted -- and the commit must already be
# pushed. Get it with:
#
#   git rev-parse v0.1.0^{commit}
#
# This is v0.1.0 of https://github.com/NatLabRockies/infrastore. Note that only
# changes under crates/, Cargo.toml, or Cargo.lock require a new SHA here;
# edits to this recipe do not, since Yggdrasil builds from its own copy.
sources = [
    GitSource(
        "https://github.com/NatLabRockies/infrastore.git",
        "be5a3d01015d8d18a3c91a9cdfe93f6769d9ab76",
    ),
]

# Build the FFI cdylib, linking the jll-provided NetCDF/HDF5.
script = raw"""
cd ${WORKSPACE}/srcdir/infrastore

# Point the netcdf-sys / hdf5-metno-sys build scripts at the jll libraries.
export HDF5_DIR=${prefix}
export NETCDF_DIR=${prefix}
export PKG_CONFIG_PATH=${prefix}/lib/pkgconfig:${prefix}/share/pkgconfig
export RUSTFLAGS="-C link-arg=-L${libdir}"

# Fetch deps so their build scripts can be patched before compiling.
cargo fetch --target ${rust_target}

# hdf5-metno-sys (via netcdf-sys) probes the HDF5 runtime version by dlopen()ing
# libhdf5 — impossible when cross-compiling. The header version from HDF5_jll is
# authoritative, so neutralize the runtime probe.
#
# This is a brittle match against upstream source: if it stops matching, the sed
# silently does nothing and the build fails at the probe instead. Verify against
# the hdf5-metno-sys version in Cargo.lock when bumping dependencies.
for f in $(find "${CARGO_HOME:-$HOME/.cargo}" /opt -type f -path '*hdf5-metno-sys*/build.rs' 2>/dev/null | sort -u); do
    sed -i 's/[[:space:]]*validate_runtime_version(&config);/ \/\/ skipped: no cross-compile runtime probe/' "$f"
done

# BinaryBuilder forbids forcing an arch via -march, so sha2's ARMv8-crypto `asm`
# kernels cannot be assembled here; use the portable SHA-256 (x86_64 still detects
# SHA-NI at runtime) for the distributed binary.
sed -i 's/sha2 = { workspace = true, features = \["asm"\] }/sha2 = { workspace = true }/' \
    crates/infrastore-core/Cargo.toml

# `--no-default-features` turns OFF infrastore's `vendored` feature, which is on by
# default and would build netcdf-c + HDF5 from source and link them statically.
# That is right for standalone Rust/Python consumers but wrong here: this binary
# must link the NetCDF_jll/HDF5_jll libraries declared below so a Julia process
# has exactly one libhdf5. Do not drop this flag.
cargo build --release --no-default-features --target ${rust_target} -p infrastore-ffi

install -Dvm755 "target/${rust_target}/release/libinfrastore_ffi.${dlext}" \
    "${libdir}/libinfrastore_ffi.${dlext}"
install -Dvm644 "crates/infrastore-ffi/include/infrastore.h" \
    "${includedir}/infrastore.h"
"""

# Teaches the generated JLL how to tag the running system with an MPI ABI, so it
# selects the same variant its NetCDF/HDF5 dependencies resolved to.
augment_platform_block = """
    using Base.BinaryPlatforms
    $(MPI.augment)
    augment_platform!(platform::Platform) = augment_mpi!(platform)
"""

# The tier-1 targets that cover essentially every InfrastructureSystems.jl user,
# deliberately narrower than `supported_platforms()`. Every platform listed must
# build for the Yggdrasil PR to merge, and MPI augmentation multiplies this list
# by the number of MPI ABIs -- so start small and widen once these are green.
#
# The `libgfortran_version` / `cxxstring_abi` tags are NOT about this crate's own
# ABI -- it is a pure C ABI cdylib with no Fortran or C++ in its link closure.
# They are the coordinates BinaryBuilder uses to *select a dependency's*
# artifact, and HDF5_jll publishes only tagged builds:
#
#     x86_64-linux-gnu-libgfortran5-cxx11-mpi+mpich
#     x86_64-apple-darwin-libgfortran5-mpi+mpich        (macOS carries no cxx tag)
#
# A platform missing those tags matches none of them, nothing gets installed into
# ${prefix}, and the build dies much later with hdf5-metno-sys reporting an
# "Invalid HDF5 headers directory". NetCDF_jll, by contrast, ships artifacts
# tagged with `mpi` alone; tags an artifact does not declare are ignored when
# matching, so these fuller platforms select both correctly.
#
# Rather than `expand_cxxstring_abis` / `expand_gfortran_versions`, which would
# multiply the matrix by combinations HDF5_jll does not publish, these are pinned
# to the single combination it does. After MPI augmentation this yields exactly
# the 17 triplets HDF5_jll ships. Re-check against its release assets when
# bumping the HDF5_jll compat bound.
platforms = [
    Platform("x86_64", "linux"; libc = "glibc", libgfortran_version = v"5", cxxstring_abi = "cxx11"),
    Platform("aarch64", "linux"; libc = "glibc", libgfortran_version = v"5", cxxstring_abi = "cxx11"),
    Platform("x86_64", "macos"; libgfortran_version = v"5"),
    Platform("aarch64", "macos"; libgfortran_version = v"5"),
    Platform("x86_64", "windows"; libgfortran_version = v"5", cxxstring_abi = "cxx11"),
]

platforms, platform_dependencies = MPI.augment_platforms(platforms)

products = [
    LibraryProduct("libinfrastore_ffi", :libinfrastore_ffi),
]

# HDF5_jll is pinned to the same compat bound NetCDF_jll declares. That
# agreement is the whole point of this recipe: if the two resolved to different
# HDF5_jll versions we would be back to two libhdf5 in one process, which is
# exactly what vendoring was avoided for.
dependencies = [
    Dependency("NetCDF_jll"; compat="401.1000.100"),
    Dependency("HDF5_jll"; compat="2.1.2"),
]
append!(dependencies, platform_dependencies)

# Don't look for `mpiwrapper.so` when BinaryBuilder examines and `dlopen`s the
# shared libraries. (MPItrampoline will skip its automatic initialization.)
ENV["MPITRAMPOLINE_DELAY_INIT"] = "1"

build_tarballs(
    ARGS, name, version, sources, script, platforms, products, dependencies;
    compilers = [:c, :rust],
    julia_compat = "1.10",
    augment_platform_block,
    # Must be >= the workspace's `rust-version`, and the workspace is on edition
    # 2024 (Rust >= 1.85). BinaryBuilder otherwise defaults to whatever its
    # newest Rust shard happens to be, which makes the toolchain drift silently.
    #
    # This is a tight constraint worth watching: 1.94.0 is currently BOTH the
    # workspace MSRV and the newest shard BinaryBuilderBase ships. Raising
    # `rust-version` in the root Cargo.toml above 1.94 makes this JLL
    # unbuildable until Yggdrasil publishes a matching RustBase artifact, so
    # check the available versions before bumping the MSRV:
    #
    #   https://github.com/JuliaPackaging/BinaryBuilderBase.jl/blob/master/Artifacts.toml
    preferred_rust_version = v"1.94.0",
)
