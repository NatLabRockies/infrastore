# Yggdrasil build recipe for TimeSeriesStore_jll.
#
# This produces the `libtime_series_store_ffi` binary that `TimeSeriesStore.jl`
# (and, through it, InfrastructureSystems.jl) loads. It is built against the
# ecosystem's NetCDF_jll + HDF5_jll so there is a single libhdf5 in any Julia
# process (no Homebrew dependency, no version drift).
#
# To publish: copy this directory into a Yggdrasil fork under
# `T/TimeSeriesStore/`, pin `version` + the source commit, and open a PR.
# Run locally first with:
#   julia build_tarballs.jl --verbose --debug <triplet>

using BinaryBuilder, Pkg

name = "TimeSeriesStore"
version = v"0.1.0"

# Pin to a tagged release/commit of the Rust workspace before submitting.
sources = [
    GitSource(
        "https://github.com/NatLabRockies/time-series-store.git",
        "0000000000000000000000000000000000000000",  # TODO: pin commit SHA
    ),
]

# Build the FFI cdylib, linking the jll-provided NetCDF/HDF5.
script = raw"""
cd ${WORKSPACE}/srcdir/time-series-store

# Point the netcdf-sys / hdf5-metno-sys build scripts at the jll libraries.
export HDF5_DIR=${prefix}
export NETCDF_DIR=${prefix}
export PKG_CONFIG_PATH=${prefix}/lib/pkgconfig:${prefix}/share/pkgconfig
export RUSTFLAGS="-C link-arg=-L${libdir}"

cargo build --release --target ${rust_target} -p time-series-store-ffi

install -Dvm755 "target/${rust_target}/release/libtime_series_store_ffi.${dlext}" \
    "${libdir}/libtime_series_store_ffi.${dlext}"
install -Dvm644 "crates/time-series-store-ffi/include/time_series_store.h" \
    "${includedir}/time_series_store.h"
"""

# Start from the platforms NetCDF_jll/HDF5_jll support; the Rust toolchain in
# BinaryBuilder covers the usual glibc/musl/macOS/windows targets. Narrow this
# list to whatever actually builds when iterating in Yggdrasil.
platforms = supported_platforms()
platforms = expand_cxxstring_abis(platforms)

products = [
    LibraryProduct("libtime_series_store_ffi", :libtime_series_store_ffi),
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
