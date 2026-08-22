# Install the Native Library

This recipe builds the components other languages depend on. For the full prerequisites and
workspace build, see [Installation](../getting-started/installation.md).

> **You may not need to build it.** `Pkg.add("InfraStore")` downloads a prebuilt `libinfrastore_ffi`
> for the platform as a Julia artifact, and each tagged release also attaches the library and the
> generated `infrastore.h` to the
> [Releases page](https://github.com/NatLabRockies/infrastore/releases), linked statically against
> HDF5 and zlib. Pointing `INFRASTORE_LIB` at a downloaded library (step 3 below) skips the build.
> Build from source when you need a platform the release matrix does not cover, or a library
> matching a working tree.

## 1. Install Build Tools

HDF5 and zlib are compiled from vendored sources and linked statically, so there is **no system HDF5
to install**. The build needs `cmake`, a C compiler, and `protobuf` for the gRPC codegen:

```sh
# macOS
brew install cmake protobuf

# Debian / Ubuntu
sudo apt-get install cmake protobuf-compiler
```

Do not set `HDF5_DIR` — it redirects the vendored build at an external HDF5 while static libraries
are still requested, and the build fails. See
[Build Prerequisites](../getting-started/installation.md#build-prerequisites) for the system-library
alternative (`--no-default-features`).

## 2. Build What You Need

**The C ABI cdylib** (used by Julia and any C consumer):

```sh
cargo build -p infrastore-ffi --release
# -> target/release/libinfrastore_ffi.{dylib,so,dll}
# Regenerates the C header at crates/infrastore-ffi/include/infrastore.h
```

**The whole workspace** (core, server, proto, ffi):

```sh
cargo build --workspace
cargo test --workspace
```

The Python wheel is built separately with `maturin` — see
[Integrate with Python](./integrate-python.md).

## 3. Point Consumers at the Library

`InfraStore.jl` and other C consumers locate a locally built cdylib via an environment variable,
which takes precedence over the artifact `Pkg` installed:

```sh
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

Add it to your shell profile to make it permanent.

## Next

- [Integrate with Python](./integrate-python.md)
- [Integrate with Julia](./integrate-julia.md)
