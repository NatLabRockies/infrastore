# Yggdrasil build recipe for InfraStore_jll.
#
# This produces the `libinfrastore_ffi` binary that `InfraStore.jl`
# (and, through it, InfrastructureSystems.jl) loads. It is built against the
# ecosystem's NetCDF_jll + HDF5_jll so there is a single libhdf5 in any Julia
# process (no Homebrew dependency, no version drift).
#
# To publish: copy this directory into a Yggdrasil fork under
# `C/InfraStore/`, pin `version` + the source commit, and open a PR.
# Run locally first with:
#   julia build_tarballs.jl --verbose --debug <triplet>

using BinaryBuilder, Pkg

name = "InfraStore"
version = v"0.1.0"

# Pin to the commit the release tag points at. Yggdrasil requires a full commit
# SHA here -- a tag name is not accepted -- so this has to be updated to the
# `v$(version)` commit before opening the submission PR, and that commit must
# already be pushed to origin. Get it with:
#
#   git rev-parse v0.1.0^{commit}
#
# RELEASE CHECKLIST: this SHA is a placeholder until the tag exists.
sources = [
    GitSource(
        "https://github.com/NatLabRockies/infrastore.git",
        "fde88b96d1ad53c64f03dba761cc903f75d78d42",
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

# The tier-1 targets that cover essentially every InfrastructureSystems.jl user,
# deliberately narrower than `supported_platforms()`. A full expansion mostly
# produces targets the Rust toolchain or NetCDF_jll/HDF5_jll cannot satisfy, and
# each failure costs a Yggdrasil CI round trip. Add platforms once these build.
#
# No `expand_cxxstring_abis`: this is a pure C ABI cdylib with no C++ in its
# link closure, so the libstdc++ string ABI does not apply and expanding it
# would just double the build matrix.
platforms = [
    Platform("x86_64", "linux"; libc = "glibc"),
    Platform("aarch64", "linux"; libc = "glibc"),
    Platform("x86_64", "macos"),
    Platform("aarch64", "macos"),
    Platform("x86_64", "windows"),
]

products = [
    LibraryProduct("libinfrastore_ffi", :libinfrastore_ffi),
]

dependencies = [
    Dependency("NetCDF_jll"),
    Dependency("HDF5_jll"),
]

build_tarballs(
    ARGS, name, version, sources, script, platforms, products, dependencies;
    compilers = [:c, :rust],
    julia_compat = "1.10",
)
