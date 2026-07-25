//! Shared helpers for the `infrastore-core` integration tests.
//!
//! Included with `mod common;` from each test file that needs it. Because each
//! integration-test file is its own crate, unused helpers here would trip
//! `dead_code`; every item therefore carries `#[allow(dead_code)]`.

use infrastore_core::{Store, create_store, open_store};

/// Run `populate` to write data, then `verify` to read it back, once per
/// backend. For NetCDF the store is flushed, dropped, and reopened read-only
/// between the two phases, exercising the persisted format.
///
/// `verify` receives the backend name (`"memory"` / `"netcdf"`) so assertion
/// messages identify which variant failed.
#[allow(dead_code)]
pub fn for_each_backend<T>(populate: impl Fn(&mut Store) -> T, verify: impl Fn(&Store, &T, &str)) {
    // In-memory backend: same store instance for write and read.
    {
        let mut store = create_store(None, true).unwrap();
        let state = populate(&mut store);
        verify(&store, &state, "memory");
    }
    // NetCDF backend: persist, reopen read-only, then read.
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");
        let state = {
            let mut store = create_store(Some(path.as_path()), false).unwrap();
            let state = populate(&mut store);
            store.flush().unwrap();
            state
        };
        let store = open_store(path.as_path(), true).unwrap();
        verify(&store, &state, "netcdf");
    }
}

/// Like [`for_each_backend`], but `verify` gets a mutable store so it can
/// exercise write-direction APIs (rename, remove, copy, ...).
///
/// The NetCDF variant is flushed and reopened **read-write** before `verify`,
/// so the mutations run against a store whose state came off disk.
#[allow(dead_code)]
pub fn for_each_backend_mut<T>(
    populate: impl Fn(&mut Store) -> T,
    verify: impl Fn(&mut Store, &T, &str),
) {
    // In-memory backend: same store instance for write and read.
    {
        let mut store = create_store(None, true).unwrap();
        let state = populate(&mut store);
        verify(&mut store, &state, "memory");
    }
    // NetCDF backend: persist, reopen read-write, then mutate + read.
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.nc");
        let state = {
            let mut store = create_store(Some(path.as_path()), false).unwrap();
            let state = populate(&mut store);
            store.flush().unwrap();
            state
        };
        let mut store = open_store(path.as_path(), false).unwrap();
        verify(&mut store, &state, "netcdf");
    }
}
