# Install the Native Library

This recipe builds the components other languages depend on. For the full prerequisites and
workspace build, see [Installation](../getting-started/installation.md).

## 1. Install System Libraries

```sh
# macOS
brew install hdf5 protobuf

# Debian / Ubuntu
sudo apt-get install libhdf5-dev protobuf-compiler
```

If the build fails with `Unable to locate HDF5 root directory and/or headers`, set `HDF5_DIR`:

```sh
export HDF5_DIR="$(brew --prefix hdf5)"          # macOS
export HDF5_DIR=/usr/lib/x86_64-linux-gnu/hdf5/serial  # Linux
```

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

The Julia binding and other C consumers locate the cdylib via an environment variable:

```sh
export INFRASTORE_LIB=$PWD/target/release/libinfrastore_ffi.dylib  # .so on Linux
```

Add it to your shell profile to make it permanent.

## Next

- [Integrate with Python](./integrate-python.md)
- [Integrate with Julia](./integrate-julia.md)
