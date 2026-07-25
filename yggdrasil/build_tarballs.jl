# Yggdrasil build recipe for Castore_jll.
#
# This produces the `libcastore_ffi` binary that `Castore.jl`
# (and, through it, InfrastructureSystems.jl) loads. It is built against the
# ecosystem's NetCDF_jll + HDF5_jll so there is a single libhdf5 in any Julia
# process (no Homebrew dependency, no version drift).
#
# To publish: copy this directory into a Yggdrasil fork under
# `C/Castore/`, pin `version` + the source commit, and open a PR.
# Run locally first with:
#   julia build_tarballs.jl --verbose --debug <triplet>

using BinaryBuilder, Pkg

name = "Castore"
version = v"0.1.0"

# Pin to a tagged release/commit of the Rust workspace. NOTE: this commit must be
# pushed to origin before the Yggdrasil build can fetch it (switch to a release tag
# for the submission PR).
sources = [
    GitSource(
        "https://github.com/NatLabRockies/castore.git",
        "fde88b96d1ad53c64f03dba761cc903f75d78d42",
    ),
]

# Build the FFI cdylib, linking the jll-provided NetCDF/HDF5.
script = raw"""
cd ${WORKSPACE}/srcdir/castore

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
    crates/castore-core/Cargo.toml

# `--no-default-features` turns OFF castore's `vendored` feature, which is on by
# default and would build netcdf-c + HDF5 from source and link them statically.
# That is right for standalone Rust/Python consumers but wrong here: this binary
# must link the NetCDF_jll/HDF5_jll libraries declared below so a Julia process
# has exactly one libhdf5. Do not drop this flag.
cargo build --release --no-default-features --target ${rust_target} -p castore-ffi

install -Dvm755 "target/${rust_target}/release/libcastore_ffi.${dlext}" \
    "${libdir}/libcastore_ffi.${dlext}"
install -Dvm644 "crates/castore-ffi/include/castore.h" \
    "${includedir}/castore.h"
"""

# Start from the platforms NetCDF_jll/HDF5_jll support; the Rust toolchain in
# BinaryBuilder covers the usual glibc/musl/macOS/windows targets. Narrow this
# list to whatever actually builds when iterating in Yggdrasil.
platforms = supported_platforms()
platforms = expand_cxxstring_abis(platforms)

products = [
    LibraryProduct("libcastore_ffi", :libcastore_ffi),
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
