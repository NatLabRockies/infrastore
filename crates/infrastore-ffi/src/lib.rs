//! C ABI for `infrastore`. Used by the Julia binding (`TimeSeries.jl`)
//! and any other language that can call C.
//!
//! v0 surface — read/write SingleTimeSeries with optional features (passed as
//! a JSON object). Errors are reported via int32 status codes and a thread-
//! local message accessed through [`infrastore_last_error_message`].
//!
//! # Concurrency
//!
//! **One handle, one thread at a time — reads included.** A handle may be moved
//! between threads, and two handles over two different stores are independent,
//! but a single handle must never be in two calls at once. `Store` is `Send` and
//! not `Sync`: its catalog is a `rusqlite::Connection`, which is unsound to
//! share across threads even for reading, so a concurrent pair of *reads*
//! through one handle is undefined behaviour and not merely a lost update. The
//! per-function `# Safety` sections say "must not be used concurrently"; this is
//! what that means, and why none of them carves out an exception for readers.
//!
//! The error state is per-thread, which follows from the same rule: a handle's
//! calls all happen on one thread at a time, so
//! [`infrastore_last_error_message`] reports the call the caller just made.
//!
//! Two handles onto the *same* on-disk artifact are a different question,
//! answered by SQLite and HDF5 rather than by this crate: one writer at a time,
//! and a reader open while a writer is running may observe a partial state.
//!
//! # ABI conventions
//!
//! These hold for **every** exported function, and the per-function `# Safety`
//! sections do not repeat them — those cover only what is specific to the call.
//!
//! **Handles.** Every `handle` / `batch` / `reader` argument must be a live,
//! non-null handle created by this library and not yet freed, and must not be
//! in two calls at once (see Concurrency above). A handle taken by a mutating
//! call must additionally come from a store opened read-write.
//!
//! **Strings in.** Every `*const c_char` must point to a null-terminated UTF-8
//! string that stays valid for the duration of the call. Arguments documented
//! as optional may be null; the rest may not, and a null one returns
//! `INFRASTORE_ERR_NULL_POINTER`.
//!
//! **Scalars out.** Every `out_*` pointer to a scalar must be valid for writing
//! one value.
//!
//! **Owned strings out.** An `out_*` naming a string receives either null (the
//! value is unset) or an owned C string the caller must free exactly once with
//! [`infrastore_string_free`].
//!
//! **Owned buffers out.** An `out_*` naming a buffer is paired with a length
//! out-param and must be freed exactly once with the deallocator matching its
//! element type — `infrastore_buffer_free_u8`, `_u64`, `_f64`, and so on —
//! passing the companion length. A freed pointer must not be used again.
//!
//! **Probe-then-fetch.** A call taking `buf` / `cap` / `out_len` writes the
//! required length to `out_len` and fills `buf` only when `cap` is large
//! enough. `buf` may be null (a length probe); when non-null it must be valid
//! for `cap` bytes.
//!
//! **Handles out.** A call returning a handle writes it through an `out_*`
//! pointer valid for one pointer; the caller releases it exactly once with the
//! matching destructor — [`infrastore_store_free`],
//! [`infrastore_batch_free`], and so on. Every destructor also accepts null.
//!
//! **Owner arguments.** Where a call takes `owner_id` / `owner_category`, the
//! category is `0` = Component, `1` = SupplementalAttribute.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::ptr;
use std::slice;

use chrono::{DateTime, Utc};
use infrastore_core as core_lib;
use serde_json::Value;

/// A handle may be moved between threads (see the module's Concurrency
/// section), which holds only while the store stays `Send`. `Sync` is
/// deliberately *not* asserted here: it does not hold, and the documented rule
/// depends on it not holding.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<core_lib::Store>();
};

// ---- Status codes ---------------------------------------------------------

pub const INFRASTORE_OK: i32 = 0;
pub const INFRASTORE_ERR_NULL_POINTER: i32 = 1;
pub const INFRASTORE_ERR_INVALID_UTF8: i32 = 2;
pub const INFRASTORE_ERR_INVALID_PARAMETER: i32 = 3;
pub const INFRASTORE_ERR_NOT_FOUND: i32 = 4;
pub const INFRASTORE_ERR_DUPLICATE: i32 = 5;
pub const INFRASTORE_ERR_INTEGRITY: i32 = 6;
pub const INFRASTORE_ERR_READ_ONLY: i32 = 7;
pub const INFRASTORE_ERR_IO: i32 = 8;
/// The store on disk was written in a different, incompatible on-disk format
/// than this build reads. There is no in-place upgrade.
pub const INFRASTORE_ERR_INCOMPATIBLE_FORMAT: i32 = 9;
/// The endpoint pair of an association is already associated. Distinct from
/// `INFRASTORE_ERR_DUPLICATE`, which is about time-series identity.
pub const INFRASTORE_ERR_DUPLICATE_ASSOCIATION: i32 = 10;
/// A store already exists where one was about to be created. Creating there
/// would discard its arrays while keeping its catalog, leaving a store that
/// reopens cleanly with every array missing.
pub const INFRASTORE_ERR_STORE_EXISTS: i32 = 11;
/// The HDF5 file and its catalog do not carry the same generation stamp: they
/// are halves of two different saves.
pub const INFRASTORE_ERR_MISMATCHED_ARTIFACT: i32 = 12;
/// A caller supplied an explicit association id that is already in use.
///
/// Distinct from `INFRASTORE_ERR_DUPLICATE_ASSOCIATION`, which is the endpoint
/// pair colliding, and from `INFRASTORE_ERR_DUPLICATE`, which is a series'
/// identity colliding. The three mean different things to a caller: a taken id
/// says the ids being imported do not fit this store.
pub const INFRASTORE_ERR_DUPLICATE_ASSOCIATION_ID: i32 = 13;
/// An id-addressed call that was given an expected owner named a row belonging
/// to a different one.
///
/// Distinct from `INFRASTORE_ERR_NOT_FOUND`, which says the id names no row at
/// all: here the row is there and the caller's belief about who owns it is
/// what has gone stale — a series can be reassigned, and the id follows it.
pub const INFRASTORE_ERR_OWNER_MISMATCH: i32 = 14;
/// The catalog is at an older schema revision than this build and the store was
/// opened read-only, so nothing could migrate it.
///
/// Actionable, which is why it is its own code: open the store once for writing
/// (or run `infrastore upgrade`) and the ladder runs.
pub const INFRASTORE_ERR_CATALOG_MIGRATION_REQUIRED: i32 = 15;
/// The catalog is at a newer schema revision than this build understands.
///
/// The mirror of `INFRASTORE_ERR_CATALOG_MIGRATION_REQUIRED`, and the remedy is
/// the other one: upgrade the software, not the store.
pub const INFRASTORE_ERR_CATALOG_TOO_NEW: i32 = 16;
pub const INFRASTORE_ERR_INTERNAL: i32 = 99;

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn set_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

fn map_core_error(e: core_lib::TimeSeriesError) -> i32 {
    use core_lib::TimeSeriesError as E;
    let code = match &e {
        E::NotFound => INFRASTORE_ERR_NOT_FOUND,
        E::DuplicateTimeSeries => INFRASTORE_ERR_DUPLICATE,
        E::DuplicateAssociation(_) => INFRASTORE_ERR_DUPLICATE_ASSOCIATION,
        E::DuplicateAssociationId(_) => INFRASTORE_ERR_DUPLICATE_ASSOCIATION_ID,
        E::InvalidParameter(_) => INFRASTORE_ERR_INVALID_PARAMETER,
        E::IntegrityError(_) => INFRASTORE_ERR_INTEGRITY,
        E::ReadOnlyStore => INFRASTORE_ERR_READ_ONLY,
        E::Io(_) => INFRASTORE_ERR_IO,
        E::IncompatibleFormat { .. } => INFRASTORE_ERR_INCOMPATIBLE_FORMAT,
        E::StoreExists { .. } => INFRASTORE_ERR_STORE_EXISTS,
        E::MismatchedArtifact { .. } => INFRASTORE_ERR_MISMATCHED_ARTIFACT,
        E::OwnerMismatch { .. } => INFRASTORE_ERR_OWNER_MISMATCH,
        E::CatalogMigrationRequired { .. } => INFRASTORE_ERR_CATALOG_MIGRATION_REQUIRED,
        E::CatalogTooNew { .. } => INFRASTORE_ERR_CATALOG_TOO_NEW,
        _ => INFRASTORE_ERR_INTERNAL,
    };
    set_error(e.to_string());
    code
}

/// The optional owner guard an id-addressed call carries: `None` when
/// `has_owner` is false, and otherwise the `(owner_id, owner_category)` pair to
/// hold the addressed row to.
///
/// The ABI spells "no guard" as a flag rather than a sentinel owner id, because
/// every `i64` is a legitimate owner id and no value is free to mean "unset".
fn optional_owner(
    has_owner: bool,
    owner_id: i64,
    owner_category: i32,
) -> Result<Option<(i64, core_lib::OwnerCategory)>, i32> {
    if !has_owner {
        return Ok(None);
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    Ok(Some((owner_id, category)))
}

/// Dereference a raw handle pointer or return `INFRASTORE_ERR_NULL_POINTER`.
///
/// `deref_handle!(ref p)` yields `&T` via `p.as_ref()`; `deref_handle!(mut p)`
/// yields `&mut T` via `p.as_mut()`. Both early-return on a null pointer, so
/// this is only usable inside functions returning `i32`.
macro_rules! deref_handle {
    (ref $ptr:expr) => {
        match unsafe { $ptr.as_ref() } {
            Some(v) => v,
            None => return INFRASTORE_ERR_NULL_POINTER,
        }
    };
    (mut $ptr:expr) => {
        match unsafe { $ptr.as_mut() } {
            Some(v) => v,
            None => return INFRASTORE_ERR_NULL_POINTER,
        }
    };
}

// ---- Logging --------------------------------------------------------------

/// Initialize the Rust tracing subscriber.
///
/// `filter` is a null-terminated UTF-8 [`EnvFilter`] directive string, e.g.
/// `"debug"` or `"infrastore_core=debug"`. Pass `NULL` to read the
/// `RUST_LOG` environment variable (or emit nothing if the variable is unset).
///
/// The subscriber is initialized at most once per process. Subsequent calls
/// are no-ops. Returns `INFRASTORE_OK` on success, `INFRASTORE_ERR_INVALID_UTF8` if `filter`
/// is not valid UTF-8, or `INFRASTORE_ERR_INVALID_PARAMETER` if `filter` contains an
/// invalid directive (e.g. an unrecognised level name).
///
/// # Safety
///
/// `filter` must be a valid null-terminated UTF-8 string or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_init_logging(filter: *const c_char) -> i32 {
    use tracing_subscriber::EnvFilter;
    let env_filter = if filter.is_null() {
        EnvFilter::from_default_env()
    } else {
        let s = match unsafe { cstr_to_str(filter) } {
            Ok(s) => s,
            Err(code) => return code,
        };
        match EnvFilter::try_new(s) {
            Ok(f) => f,
            Err(e) => {
                set_error(e.to_string());
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
    INFRASTORE_OK
}

// ---- Handles --------------------------------------------------------------

pub struct InfraStoreHandle {
    inner: core_lib::Store,
}

/// Accumulates pending add requests for a single all-or-nothing
/// `infrastore_store_add_batch` call. Building the batch performs no store I/O.
pub struct InfraStoreBatchHandle {
    items: Vec<core_lib::AddRequest>,
}

/// Owns the results of a read call (`infrastore_store_read_by_ids` or the
/// variant-general `infrastore_store_bulk_read`): the time series fetched for a batch of
/// keys, in input order. Each element's variant is discovered with
/// `infrastore_bulk_result_item_type` and read out with the matching
/// `infrastore_bulk_result_get_*`; the handle is released with `infrastore_bulk_result_free`.
pub struct InfraStoreBulkReadHandle {
    items: Vec<core_lib::TimeSeriesData>,
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(INFRASTORE_ERR_NULL_POINTER);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| INFRASTORE_ERR_INVALID_UTF8)
}

unsafe fn cstr_to_optional_string(p: *const c_char) -> Result<Option<String>, i32> {
    if p.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { cstr_to_str(p)? }.to_string()))
}

unsafe fn cstr_to_optional_path(p: *const c_char) -> Result<Option<PathBuf>, i32> {
    Ok(unsafe { cstr_to_optional_string(p)? }.map(PathBuf::from))
}

/// Parse a time reference from a C string. `null`/empty -> `None`
/// (unspecified); an unparseable spelling sets the error and returns
/// `INFRASTORE_ERR_INVALID_PARAMETER` rather than degrading to `None`, for the
/// same reason the unit system does — and one sharper: a series whose spelling
/// was silently dropped reads back as instants it never claimed.
///
/// Accepts all four spellings: `utc`, `zoneless`, a fixed offset (`-07:00`), or
/// an IANA zone name (`America/Denver`). Zone *existence* is not checked here;
/// the Julia wrapper, which has a tz database, warns on a name it does not
/// recognize and passes it through.
unsafe fn cstr_to_optional_time_reference(
    p: *const c_char,
) -> Result<Option<core_lib::TimeReference>, i32> {
    match unsafe { cstr_to_optional_string(p)? } {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => match core_lib::TimeReference::parse(&s) {
            Ok(r) => Ok(Some(r)),
            Err(e) => {
                set_error(format!("invalid time_reference {s:?}: {e}"));
                Err(INFRASTORE_ERR_INVALID_PARAMETER)
            }
        },
    }
}

/// Turn the FFI tri-state `zoneless` filter argument into
/// [`core_lib::ListFilter::zoneless`]. Negative means *no filter*, which is the
/// default a caller that does not care passes; 0 and 1 are the two coherence
/// groups. A tri-state int rather than a `bool` + `has_` pair because the
/// predicate itself is already a boolean, and two booleans at adjacent argument
/// positions is exactly the swap this file avoids elsewhere.
fn zoneless_filter(zoneless: i32) -> Result<Option<bool>, i32> {
    match zoneless {
        n if n < 0 => Ok(None),
        0 => Ok(Some(false)),
        1 => Ok(Some(true)),
        other => {
            set_error(format!(
                "invalid zoneless filter {other}; expected -1 (no filter), 0, or 1"
            ));
            Err(INFRASTORE_ERR_INVALID_PARAMETER)
        }
    }
}

/// Parse a unit system from a C string. `null`/empty -> `None` (unspecified);
/// an unrecognized spelling sets the error and returns
/// `INFRASTORE_ERR_INVALID_PARAMETER` rather than degrading to `None`, so a
/// caller that misspells the basis learns about it instead of silently writing
/// values whose basis is unrecorded.
unsafe fn cstr_to_optional_unit_system(
    p: *const c_char,
) -> Result<Option<core_lib::UnitSystem>, i32> {
    match unsafe { cstr_to_optional_string(p)? } {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => match core_lib::UnitSystem::parse(&s) {
            Some(u) => Ok(Some(u)),
            None => {
                set_error(format!(
                    "invalid unit_system {s:?}; expected natural_units or component_base"
                ));
                Err(INFRASTORE_ERR_INVALID_PARAMETER)
            }
        },
    }
}

/// Parse an ISO-8601 period from a C string. `null`/empty -> `None`; a malformed
/// string sets the error and returns `INFRASTORE_ERR_INVALID_PARAMETER`.
unsafe fn cstr_to_optional_period(p: *const c_char) -> Result<Option<core_lib::Period>, i32> {
    let s = unsafe { cstr_to_optional_string(p)? };
    match s {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => core_lib::Period::from_iso8601(&s).map(Some).map_err(|e| {
            set_error(e.to_string());
            INFRASTORE_ERR_INVALID_PARAMETER
        }),
    }
}

/// Parse a required ISO-8601 period from a C string.
unsafe fn cstr_to_period(p: *const c_char) -> Result<core_lib::Period, i32> {
    let s = unsafe { cstr_to_str(p)? };
    core_lib::Period::from_iso8601(s).map_err(|e| {
        set_error(e.to_string());
        INFRASTORE_ERR_INVALID_PARAMETER
    })
}

/// Allocate an owned C string the caller must release with [`infrastore_string_free`].
///
/// Only for strings from the library's own canonical vocabularies (ISO-8601
/// periods, `element_type` spellings), which never contain an interior NUL; a
/// NUL yields a null pointer. User-supplied attributes (`application_data`, `units`) go
/// through [`opt_attr_cstring`] instead, which reports the NUL as an error
/// rather than silently returning "unset".
fn owned_cstr(s: &str) -> *mut c_char {
    match std::ffi::CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Build the owned C string for an optional user-supplied attribute (`application_data` /
/// `units`). `None` stays `None` (emitted as a null pointer). An interior NUL
/// cannot cross the C ABI as a string; it is reported as an integrity error so
/// the caller never mistakes a present attribute for an unset one. The value is
/// kept as a `CString` (dropped on any later early return) and only released to
/// the caller with [`into_raw_or_null`] once the whole call has succeeded.
fn opt_attr_cstring(s: Option<&str>) -> Result<Option<std::ffi::CString>, i32> {
    match s {
        None => Ok(None),
        Some(s) => std::ffi::CString::new(s).map(Some).map_err(|_| {
            set_error(
                "string attribute contains an interior NUL byte and cannot be returned over \
                 the C ABI",
            );
            INFRASTORE_ERR_INTEGRITY
        }),
    }
}

/// Hand an optional `CString` to the caller: null for `None`, otherwise an
/// owned pointer released with [`infrastore_string_free`].
fn into_raw_or_null(c: Option<std::ffi::CString>) -> *mut c_char {
    c.map_or(std::ptr::null_mut(), std::ffi::CString::into_raw)
}

/// Hand a `Vec`'s contents to the caller as a heap buffer whose allocation is
/// exactly `len` elements, returning `(ptr, len)`.
///
/// The matching `infrastore_buffer_free_*`
/// reconstructs a `Box<[T]>` from `(ptr, len)`, so the handed-out allocation
/// must have capacity == length. `into_boxed_slice` guarantees that
/// (reallocating if the vector carried excess capacity), which makes the
/// contract structural instead of depending on how each producer happened to
/// build its vector. An empty vector yields a dangling (non-null, aligned)
/// pointer with length 0, which the free functions handle.
fn vec_into_raw<T>(v: Vec<T>) -> (*mut T, u64) {
    let len = v.len() as u64;
    let ptr = Box::into_raw(v.into_boxed_slice()) as *mut T;
    (ptr, len)
}

/// Reclaim and drop a buffer previously produced by [`vec_into_raw`].
///
/// # Safety
///
/// `ptr` must be null or a pointer returned by [`vec_into_raw`] with exactly
/// `len` elements, not previously freed.
unsafe fn free_raw_buffer<T>(ptr: *mut T, len: u64) {
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr,
                len as usize,
            )))
        };
    }
}

/// Owned ISO-8601 C string for a period (caller frees with [`infrastore_string_free`]).
fn period_cstr(p: core_lib::Period) -> *mut c_char {
    owned_cstr(&p.to_iso8601())
}

/// Owned ISO-8601 C string for an optional period; `None` -> null pointer.
fn opt_period_cstr(p: Option<core_lib::Period>) -> *mut c_char {
    p.map(period_cstr).unwrap_or(std::ptr::null_mut())
}

/// Free a C string returned by this library (e.g. a `*out_resolution`).
///
/// # Safety
///
/// `s` must be null or a pointer returned by this library's owned-string outputs,
/// freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(std::ffi::CString::from_raw(s)) };
    }
}

/// Hand a produced string to the caller as an owned allocation, to be released
/// with `infrastore_string_free`.
///
/// This is the convention for outputs whose size scales with the data — a
/// listing over a large catalog runs to tens of megabytes. The alternative, a
/// probe-then-fetch pair of calls with a caller-sized buffer, would execute the
/// query and serialize the result *twice*, since neither pass can retain
/// anything for the other.
///
/// # Safety
///
/// `out` and `out_len` must be valid for writing one pointer and one `u64`.
unsafe fn write_owned_str_out(s: String, out: *mut *mut c_char, out_len: *mut u64) -> i32 {
    // The payload is JSON, which never contains an interior NUL, so this only
    // fails on a genuine encoding bug.
    let len = s.len() as u64;
    match std::ffi::CString::new(s) {
        Ok(c) => unsafe {
            *out = c.into_raw();
            *out_len = len;
            INFRASTORE_OK
        },
        Err(e) => {
            set_error(format!("result contained an interior NUL: {e}"));
            INFRASTORE_ERR_INTERNAL
        }
    }
}

// ---- Store create / open / free ------------------------------------------

/// Create a time-series store and return an owning handle through `out`.
///
/// # Safety
///
/// When non-null, `path` must point to a valid, null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_create(
    path: *const c_char,
    in_memory: bool,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_optional_path(path) } {
        Ok(p) => p,
        Err(code) => {
            set_error("invalid path");
            return code;
        }
    };
    let store = match core_lib::create_store(path.as_deref(), in_memory) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Create a store with an explicit compression policy.
///
/// `compression_kind` selects the filter: `0` = none (uncompressed), `1` =
/// DEFLATE at `deflate_level` (0–9) with byte `shuffle` when non-zero. Any
/// other `compression_kind` is rejected. The policy is ignored for in-memory
/// stores and persisted so later appends reuse it. Equivalent to
/// [`infrastore_store_create`] with `compression_kind = 1`, level 3, shuffle on.
///
/// # Safety
///
/// When non-null, `path` must point to a valid, null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_create_with_compression(
    path: *const c_char,
    in_memory: bool,
    compression_kind: u8,
    deflate_level: u8,
    shuffle: bool,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_optional_path(path) } {
        Ok(p) => p,
        Err(code) => {
            set_error("invalid path");
            return code;
        }
    };
    let compression = match compression_from_code(compression_kind, deflate_level, shuffle) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store =
        match core_lib::create_store_with_compression(path.as_deref(), in_memory, compression) {
            Ok(s) => s,
            Err(e) => return map_core_error(e),
        };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Open an existing time-series store and return an owning handle through `out`.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_open(
    path: *const c_char,
    read_only: bool,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    let store = match core_lib::open_store(&path, read_only) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Translate a `compression_kind` code plus its parameters into a core
/// [`Compression`](core_lib::Compression).
///
/// `0` = none (uncompressed), `1` = DEFLATE at `deflate_level` (0–9) with byte `shuffle` when
/// non-zero. Level validation is left to the core so the message stays in one place.
fn compression_from_code(
    kind: u8,
    deflate_level: u8,
    shuffle: bool,
) -> std::result::Result<core_lib::Compression, i32> {
    match kind {
        0 => Ok(core_lib::Compression::None),
        1 => Ok(core_lib::Compression::Deflate {
            level: deflate_level,
            shuffle,
        }),
        other => {
            set_error(format!(
                "invalid compression_kind {other}, expected 0 (none) or 1 (deflate)"
            ));
            Err(INFRASTORE_ERR_INVALID_PARAMETER)
        }
    }
}

/// Translate a `catalog_mode` code into a core [`CatalogMode`](core_lib::CatalogMode).
///
/// `0` = attached (the catalog is `<path>.sqlite`), `1` = in memory (it reaches disk only through
/// `infrastore_store_persist`).
fn catalog_from_code(code: u8) -> std::result::Result<core_lib::CatalogMode, i32> {
    match code {
        0 => Ok(core_lib::CatalogMode::Attached),
        1 => Ok(core_lib::CatalogMode::InMemory),
        other => {
            set_error(format!(
                "invalid catalog_mode {other}, expected 0 (attached) or 1 (memory)"
            ));
            Err(INFRASTORE_ERR_INVALID_PARAMETER)
        }
    }
}

/// Create a store, choosing where the SQLite catalog lives.
///
/// Like `infrastore_store_create_with_compression`, but `catalog_mode` selects the catalog's
/// placement: `0` attaches it to `<path>.sqlite`, where every commit is durable; `1` holds it in
/// memory, where nothing survives a crash and only `infrastore_store_persist` writes it out.
/// Arrays stream to the HDF5 file either way. `in_memory=true` admits only `catalog_mode=1`.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_create_with_catalog(
    path: *const c_char,
    in_memory: bool,
    compression_kind: u8,
    deflate_level: u8,
    shuffle: bool,
    catalog_mode: u8,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_optional_path(path) } {
        Ok(p) => p,
        Err(code) => {
            set_error("invalid path");
            return code;
        }
    };
    let compression = match compression_from_code(compression_kind, deflate_level, shuffle) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let catalog = match catalog_from_code(catalog_mode) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store =
        match core_lib::create_store_with_catalog(path.as_deref(), in_memory, compression, catalog)
        {
            Ok(s) => s,
            Err(e) => return map_core_error(e),
        };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Create a store at `path`, discarding any artifact already there.
///
/// The destructive counterpart to `infrastore_store_create_with_catalog`, which fails with
/// `INFRASTORE_ERR_STORE_EXISTS` when either half of a store is already at the path. Both halves go
/// — the HDF5 file, `<path>.sqlite`, and the catalog's `-wal`/`-shm` sidecars — because leaving the
/// catalog would pair a fresh, empty array file with the old catalog's rows.
///
/// Not atomic: the old artifact is removed before the new one exists, so an interrupted call can
/// leave neither. For callers whose explicit intent is to discard the destination.
///
/// `compression_kind`, `deflate_level`, `shuffle`, and `catalog_mode` are as in
/// `infrastore_store_create_with_catalog`.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_create_replacing(
    path: *const c_char,
    compression_kind: u8,
    deflate_level: u8,
    shuffle: bool,
    catalog_mode: u8,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    let compression = match compression_from_code(compression_kind, deflate_level, shuffle) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let catalog = match catalog_from_code(catalog_mode) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store = match core_lib::create_store_replacing(&path, compression, catalog) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Copy the store at `src` to `dest` and open the copy read-write.
///
/// Both halves are copied, so `dest` is a complete, independent store, and `src` is never opened
/// for writing. This is the safe way to load a store and then change it: opening the original
/// read-write puts every mutation into that file, and HDF5 has no journal and no repair tool, so an
/// interrupted write there is unrecoverable. Working on the copy and calling
/// `infrastore_store_persist` back over the original replaces it with one atomic rename.
///
/// Fails with `INFRASTORE_ERR_STORE_EXISTS` if `dest` already holds either half of a store.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_open_copy(
    src: *const c_char,
    dest: *const c_char,
    catalog_mode: u8,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let src = match unsafe { cstr_to_str(src) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid src path string");
            return code;
        }
    };
    let dest = match unsafe { cstr_to_str(dest) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid dest path string");
            return code;
        }
    };
    let catalog = match catalog_from_code(catalog_mode) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store = match core_lib::open_store_copy(&src, &dest, catalog) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Open an existing store, choosing where the SQLite catalog lives.
///
/// Like `infrastore_store_open`, but `catalog_mode=1` reads `<path>.sqlite` into memory and leaves
/// the file alone; later mutations reach disk only through `infrastore_store_persist`. The HDF5
/// half is still opened in place, so a caller that means to leave the original untouched until an
/// explicit save must open a copy.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_open_with_catalog(
    path: *const c_char,
    read_only: bool,
    catalog_mode: u8,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    let catalog = match catalog_from_code(catalog_mode) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store = match core_lib::open_store_with_catalog(&path, read_only, catalog) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Open the array half of an artifact whose catalog is absent, minting an empty one.
///
/// The way in to a store shipped as arrays plus an OpenAPI document: the returned handle holds
/// every array and no rows, ready for `infrastore_store_import_time_series_associations_openapi`
/// and its supplemental-attribute counterpart to replay them. The fresh catalog inherits the array
/// file's own generation stamp, so a later `infrastore_store_open` sees a coherent pair.
///
/// Refuses (`INFRASTORE_ERR_STORE_EXISTS`) when `<path>.sqlite` is already there — that store wants
/// `infrastore_store_open`. Never read-only.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_open_without_catalog(
    path: *const c_char,
    catalog_mode: u8,
    out: *mut *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    let catalog = match catalog_from_code(catalog_mode) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let store = match core_lib::open_store_without_catalog(&path, catalog) {
        Ok(s) => s,
        Err(e) => return map_core_error(e),
    };
    let handle = Box::new(InfraStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    INFRASTORE_OK
}

/// Report where `handle`'s catalog lives through `out`: `0` attached, `1` in memory.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_catalog_mode(
    handle: *const InfraStoreHandle,
    out: *mut u8,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let Some(store) = (unsafe { handle.as_ref() }) else {
        set_error("store handle is null");
        return INFRASTORE_ERR_NULL_POINTER;
    };
    let code = match store.inner.catalog_mode() {
        core_lib::CatalogMode::Attached => 0,
        core_lib::CatalogMode::InMemory => 1,
    };
    unsafe { *out = code };
    INFRASTORE_OK
}

/// Release a store handle returned by `infrastore_store_create` or `infrastore_store_open`.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by this library that has not already been freed.
/// The handle must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_free(handle: *mut InfraStoreHandle) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle)) };
    }
}

// ---- add_single -----------------------------------------------------------

/// Parse a null-terminated `element_type` string into an [`core_lib::ElementType`].
/// Required on every write: it says both what the elements mean and, through
/// [`core_lib::ElementType::physical_dtype`], how the bytes are encoded.
unsafe fn cstr_to_element_type(
    p: *const c_char,
) -> std::result::Result<core_lib::ElementType, i32> {
    let s = match unsafe { cstr_to_str(p) } {
        Ok(s) => s,
        Err(c) => {
            set_error("element_type is null or invalid UTF-8");
            return Err(c);
        }
    };
    s.parse::<core_lib::ElementType>().map_err(|e| {
        set_error(e.to_string());
        INFRASTORE_ERR_INVALID_PARAMETER
    })
}

/// Build a [`TypedArray`] from an element type, shape (`ndims` × `dims_ptr`), and
/// raw little-endian bytes. Returns an FFI error code on failure (and sets the
/// thread-local error). The buffers are borrowed for the duration of the call.
unsafe fn build_typed_array(
    element_type: core_lib::ElementType,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
) -> std::result::Result<core_lib::TypedArray, i32> {
    let dtype = element_type.physical_dtype();
    let dims: Vec<usize> = if ndims == 0 || dims_ptr.is_null() {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(dims_ptr, ndims as usize) }
            .iter()
            .map(|&d| d as usize)
            .collect()
    };
    let bytes = if data_byte_len == 0 || data_ptr.is_null() {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(data_ptr, data_byte_len as usize) }.to_vec()
    };
    core_lib::TypedArray::new(dtype, dims, bytes).map_err(|e| {
        set_error(e);
        INFRASTORE_ERR_INVALID_PARAMETER
    })
}

/// Parse the `infrastore_store_add_single` / `infrastore_batch_add_single` argument list into
/// an [`core_lib::AddRequest`]. Shared so the one-shot and batch entry points
/// stay behaviorally identical.
#[allow(clippy::too_many_arguments)]
unsafe fn build_single_request(
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(INFRASTORE_ERR_NULL_POINTER);
    }
    let owner_type = match unsafe { cstr_to_str(owner_type) } {
        Ok(s) => s,
        Err(c) => {
            set_error("owner_type is invalid");
            return Err(c);
        }
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(c) => {
            set_error("name is invalid");
            return Err(c);
        }
    };
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let quantity_kind = unsafe { cstr_to_optional_string(quantity_kind) }?;
    let unit_system = unsafe { cstr_to_optional_unit_system(unit_system) }?;
    let time_reference = unsafe { cstr_to_optional_time_reference(time_reference) }?;
    let component_field = unsafe { cstr_to_optional_string(component_field) }?;
    let application_data = unsafe { cstr_to_optional_string(application_data) }?;
    let features = unsafe { parse_features_json(features_json) }?;

    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let resolution = unsafe { cstr_to_period(resolution)? };
    let element_type = unsafe { cstr_to_element_type(element_type) }?;
    let array =
        unsafe { build_typed_array(element_type, ndims, dims_ptr, data_ptr, data_byte_len) }?;
    let single = core_lib::SingleTimeSeries::new(initial_timestamp, resolution, array, name);

    let mut data = core_lib::TimeSeriesData::SingleTimeSeries(single);
    // The descriptors describe the series, so they travel on it rather than
    // on the request.
    data.set_descriptors(core_lib::Descriptors {
        element_type,
        units,
        quantity_kind,
        unit_system,
        time_reference,
        component_field,
        application_data,
    });
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
    })
}

/// Add a SingleTimeSeries to the store.
///
/// `features_json`, when non-null, is parsed as a JSON object whose values must be int, float,
/// bool, or string. `application_data`, `units`, `quantity_kind`, `unit_system`,
/// `time_reference`, and `component_field` are optional; `component_field` names the field on
/// the owning component whose value these values are the time-varying form of, and
/// `time_reference` records how the timestamps were spelled (`utc`, `zoneless`, a fixed
/// offset such as `-07:00`, or an IANA zone name such as `America/Denver`).
///
/// `time_range_zoneless` on the read side carries the same distinction for query bounds: a
/// bound has to be spelled the way the series is, and a mismatch is refused rather than
/// coerced.
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `dims_ptr` must reference `ndims` elements when `ndims` is nonzero;
/// `data_ptr` must reference `data_byte_len` bytes. `out_key`, when non-null, must be valid
/// for writing one pointer.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_add_single(
    handle: *mut InfraStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let req = match unsafe {
        build_single_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            initial_ts_unix_ms,
            resolution,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut added) => {
            let id = added.remove(0);
            if !out_id.is_null() {
                unsafe { *out_id = id.get() };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- add_non_sequential --------------------------------------------------

/// Parse the `infrastore_store_add_non_sequential` / `infrastore_batch_add_non_sequential`
/// argument list into an [`core_lib::AddRequest`].
#[allow(clippy::too_many_arguments)]
unsafe fn build_non_sequential_request(
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if timestamps_unix_ms.is_null() || data_ptr.is_null() {
        set_error("an input pointer is null");
        return Err(INFRASTORE_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let timestamps = match unsafe {
        slice::from_raw_parts(timestamps_unix_ms, timestamps_len as usize)
            .iter()
            .map(|&ns| unix_ms_to_datetime(ns).ok_or(ns))
            .collect::<std::result::Result<Vec<_>, _>>()
    } {
        Ok(timestamps) => timestamps,
        Err(ns) => {
            set_error(format!("invalid timestamp unix milliseconds: {ns}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let element_type = unsafe { cstr_to_element_type(element_type) }?;
    let array =
        unsafe { build_typed_array(element_type, ndims, dims_ptr, data_ptr, data_byte_len) }?;
    let series = match core_lib::NonSequentialTimeSeries::new(timestamps, array, name) {
        Ok(series) => series,
        Err(error) => {
            set_error(error);
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let features = unsafe { parse_features_json(features_json) }?;
    let units = unsafe { cstr_to_optional_string(units) }?;
    let quantity_kind = unsafe { cstr_to_optional_string(quantity_kind) }?;
    let unit_system = unsafe { cstr_to_optional_unit_system(unit_system) }?;
    let time_reference = unsafe { cstr_to_optional_time_reference(time_reference) }?;
    let component_field = unsafe { cstr_to_optional_string(component_field) }?;
    let application_data = unsafe { cstr_to_optional_string(application_data) }?;
    let mut data = core_lib::TimeSeriesData::NonSequentialTimeSeries(series);
    // The descriptors describe the series, so they travel on it rather than
    // on the request.
    data.set_descriptors(core_lib::Descriptors {
        element_type,
        units,
        quantity_kind,
        unit_system,
        time_reference,
        component_field,
        application_data,
    });
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
    })
}

/// Add a NonSequentialTimeSeries to the store.
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `timestamps_unix_ms` must reference `timestamps_len` elements,
/// `dims_ptr` must reference `ndims` elements when `ndims` is nonzero; `data_ptr` must
/// reference `data_byte_len` bytes. `out_key`, when non-null, must be valid for writing one
/// pointer.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_add_non_sequential(
    handle: *mut InfraStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let request = match unsafe {
        build_non_sequential_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            timestamps_unix_ms,
            timestamps_len,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![request]) {
        Ok(mut added) => {
            let id = added.remove(0);
            if !out_id.is_null() {
                unsafe { *out_id = id.get() };
            }
            INFRASTORE_OK
        }
        Err(error) => map_core_error(error),
    }
}

// ---- get_single -----------------------------------------------------------

// ---- remove / has / counts / verify ---------------------------------------

/// Remove many associations named by their catalog `id`, in one all-or-nothing
/// transaction. On success `*out_removed` receives the number removed.
///
/// The removal direction of the id every write hands back through its `out_id`:
/// a caller that recorded ids in its own model retires one without rebuilding
/// the key it was filed under, and an id names exactly one row where a key can
/// match a whole forecast family. Returns `INFRASTORE_ERR_NOT_FOUND` if any id
/// names no row, in which case nothing is removed — sift the set with
/// `infrastore_store_association_exists` first when some references are
/// expected to have gone. A repeated id is removed, and counted, once.
///
/// Set `has_owner` to hold every id to the owner `(owner_id, owner_category)`:
/// the row's owner is read and the row deleted by the same transaction, and a
/// row belonging to anyone else is `INFRASTORE_ERR_OWNER_MISMATCH` with the
/// whole batch rolled back. A caller that means "retire this owner's series"
/// must use the guard rather than checking the owner in a call of its own — an
/// id survives a reassignment, so a separate check has a window after it in
/// which the row can move, and the removal would then retire the new owner's
/// series. `owner_id` and `owner_category` are ignored when `has_owner` is
/// false.
///
/// # Safety
///
/// `handle` must be a live store handle created by this library and must not be
/// used concurrently from another thread for the duration of the call. `ids`
/// must point to `n` readable `i64`s (it may be null only when `n` is 0), and
/// `out_removed` must be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_remove_by_ids(
    handle: *mut InfraStoreHandle,
    ids: *const i64,
    n: u64,
    has_owner: bool,
    owner_id: i64,
    owner_category: i32,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let count = n as usize;
    // Named separately, as the id-addressed read does: one of these is the
    // caller's output buffer and the other is its input, and a C caller can
    // only act on the diagnostic if it says which.
    if out_removed.is_null() {
        set_error("out_removed pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    if ids.is_null() && count != 0 {
        set_error("ids pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let id_slice = if count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(ids, count) }
    };
    let id_slice: Vec<core_lib::TimeSeriesId> = id_slice
        .iter()
        .copied()
        .map(core_lib::TimeSeriesId)
        .collect();
    let owner = match optional_owner(has_owner, owner_id, owner_category) {
        Ok(owner) => owner,
        Err(code) => return code,
    };
    let removed = match owner {
        Some(owner) => store.inner.remove_by_ids_for_owner(&id_slice, owner),
        None => store.inner.remove_by_ids(&id_slice),
    };
    match removed {
        Ok(removed) => {
            unsafe { *out_removed = removed as u64 };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Return aggregate time-series counts.
///
/// # Safety
///
/// All output pointers must be valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_counts(
    handle: *const InfraStoreHandle,
    out_components_with_time_series: *mut i64,
    out_static_time_series: *mut i64,
    out_forecasts: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_components_with_time_series.is_null()
        || out_static_time_series.is_null()
        || out_forecasts.is_null()
    {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.get_time_series_counts() {
        Ok(c) => {
            unsafe {
                *out_components_with_time_series = c.components_with_time_series;
                *out_static_time_series = c.static_time_series;
                *out_forecasts = c.forecasts;
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Write the store's forecast parameters, optionally restricted to forecasts
/// with `filter_resolution` and/or `filter_interval` (empty/null = no filter).
///
/// `out_present` is set to `true` when a matching forecast exists, `false`
/// otherwise. `out_horizon`, `out_interval`, and `out_resolution` each receive an
/// owned ISO-8601 duration C string (e.g. `PT1H`), or null when that field is
/// absent; free each with `infrastore_string_free`. `out_count` and `out_initial_ms` (the
/// initial timestamp as unix ms) receive their value, or `-1` when absent (counts
/// and timestamps are non-negative when present, so `-1` is an unambiguous "unset"
/// sentinel).
///
/// # Safety
///
/// The filter args are plain scalars. `out_horizon`, `out_interval`, and `out_resolution` must
/// each be valid for writing one `char *`, and each non-null result freed exactly once with
/// `infrastore_string_free`. `out_count` and `out_initial_ms` must each be valid for writing
/// one `i64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_get_forecast_parameters(
    handle: *const InfraStoreHandle,
    filter_resolution: *const c_char,
    filter_interval: *const c_char,
    out_present: *mut bool,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut i64,
    out_resolution: *mut *mut c_char,
    out_initial_ms: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null()
        || out_horizon.is_null()
        || out_interval.is_null()
        || out_count.is_null()
        || out_resolution.is_null()
        || out_initial_ms.is_null()
    {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let resolution = match unsafe { cstr_to_optional_period(filter_resolution) } {
        Ok(r) => r,
        Err(c) => return c,
    };
    let interval = match unsafe { cstr_to_optional_period(filter_interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    match store.inner.get_forecast_parameters(resolution, interval) {
        Ok(p) => {
            let present = p.horizon.is_some()
                || p.interval.is_some()
                || p.count.is_some()
                || p.resolution.is_some();
            unsafe {
                *out_present = present;
                // Period out-params are owned ISO-8601 C strings (null = unset),
                // freed by the caller with `infrastore_string_free`.
                *out_horizon = opt_period_cstr(p.horizon);
                *out_interval = opt_period_cstr(p.interval);
                *out_count = p.count.map(|c| c as i64).unwrap_or(-1);
                *out_resolution = opt_period_cstr(p.resolution);
                *out_initial_ms = p.initial_timestamp.map(datetime_to_unix_ms).unwrap_or(-1);
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Verify that, per resolution, all `SingleTimeSeries` share one
/// `(initial_timestamp, length)` grid, and return the grids as a JSON array of
/// `{"resolution": <ISO-8601>, "initial_timestamp_ms": <i64>, "length": <i64>}`
/// objects, ordered by resolution (empty array = no `SingleTimeSeries`).
/// `filter_resolution` (nullable ISO-8601 duration) scopes the check to one
/// resolution. Errors when any single resolution holds more than one distinct
/// pair. Probe-then-fetch (see `infrastore_store_list_metadata`).
///
/// # Safety
///
/// `filter_resolution` must be null or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_check_static_consistency(
    handle: *const InfraStoreHandle,
    filter_resolution: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let resolution = match unsafe { cstr_to_optional_period(filter_resolution) } {
        Ok(r) => r,
        Err(c) => return c,
    };
    let grids = match store.inner.check_static_consistency(resolution) {
        Ok(g) => g,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = grids
        .iter()
        .map(|g| {
            serde_json::json!({
                "resolution": g.resolution.to_iso8601(),
                "initial_timestamp_ms": datetime_to_unix_ms(g.initial_timestamp),
                "length": g.length as i64,
            })
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// List the distinct resolutions present in the store as a JSON array of
/// ISO-8601 duration strings (e.g. `["PT1H","P1M"]`, ascending). When
/// `has_time_series_type` is true the listing is
/// restricted to that `INFRASTORE_TYPE_*` code; otherwise all types are considered.
///
/// Follows the probe-then-fetch convention: call with `buf` null and `cap` 0 to
/// learn the byte length via `out_len`, then again with a buffer of at least
/// `len + 1` bytes.
///
/// # Safety
///
/// The type filter args are plain scalars.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_resolutions(
    handle: *const InfraStoreHandle,
    has_time_series_type: bool,
    time_series_type: i32,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let ts_type = if has_time_series_type {
        match resolve_requested_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        }
    } else {
        None
    };
    let resolutions = match store.inner.get_resolutions(ts_type) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = resolutions
        .iter()
        .map(|p| Value::from(p.to_iso8601()))
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// List the distinct forecast intervals present in the store as a JSON array of
/// ISO-8601 duration strings (ascending by ISO text). The interval analog of
/// `infrastore_store_get_resolutions`; when `has_time_series_type` is true the listing is
/// restricted to that `INFRASTORE_TYPE_*` code. Non-forecast types yield `[]`.
///
/// Probe-then-fetch (see `infrastore_store_get_resolutions`).
///
/// # Safety
///
/// The type filter args are plain scalars.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_intervals(
    handle: *const InfraStoreHandle,
    has_time_series_type: bool,
    time_series_type: i32,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let ts_type = if has_time_series_type {
        match resolve_requested_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        }
    } else {
        None
    };
    let intervals = match store.inner.get_intervals(ts_type) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = intervals
        .iter()
        .map(|p| Value::from(p.to_iso8601()))
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Write whether the store was opened read-only into `*out_read_only`.
///
/// # Safety
///
/// `handle` must be a live store handle and `out_read_only` valid for writing
/// one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_read_only(
    handle: *const InfraStoreHandle,
    out_read_only: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_read_only.is_null() {
        set_error("out_read_only is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_read_only = store.inner.read_only() };
    INFRASTORE_OK
}

/// Write whether the store holds no persistent content of any kind — no time
/// series, no associations in any catalog — into `*out`. Short-circuited
/// existence probes, so it is O(1) in store size; prefer it over a
/// client-side conjunction over the count entry points, which costs a full
/// aggregation and silently goes stale as the catalog schema grows.
///
/// # Safety
///
/// `handle` must be a live store handle and `out` valid for writing one
/// `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_is_empty(
    handle: *const InfraStoreHandle,
    out: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out.is_null() {
        set_error("out is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.is_empty() {
        Ok(empty) => {
            unsafe { *out = empty };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Write the store's backing HDF5 file path into `buf` (probe-then-fetch: call with a
/// null `buf` to learn `*out_len`, then again with a buffer of that size). An
/// in-memory store has no path: `*out_has_path` is set to false and `*out_len` to 0.
///
/// # Safety
///
/// `out_has_path` and `out_len` must be valid for writing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_path(
    handle: *const InfraStoreHandle,
    out_has_path: *mut bool,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_has_path.is_null() || out_len.is_null() {
        set_error("out_has_path or out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.file_path() {
        Some(path) => {
            unsafe { *out_has_path = true };
            unsafe { write_str_out(&path.to_string_lossy(), buf, cap, out_len) };
        }
        None => unsafe {
            *out_has_path = false;
            *out_len = 0;
        },
    }
    INFRASTORE_OK
}

/// Association count grouped by time series type, as a JSON array of
/// `{"time_series_type": <name>, "count": <n>}` objects. Probe-then-fetch (see
/// `infrastore_store_list_metadata`).
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_counts_by_type(
    handle: *const InfraStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let counts = match store.inner.counts_by_type() {
        Ok(c) => c,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = counts
        .iter()
        .map(|(ts_type, n)| {
            let mut o = serde_json::Map::new();
            o.insert("time_series_type".into(), Value::from(ts_type.as_str()));
            o.insert("count".into(), Value::from(*n));
            Value::Object(o)
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Write the number of distinct stored arrays (content hashes); shared series
/// count once.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_num_distinct_arrays(
    handle: *const InfraStoreHandle,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.num_distinct_arrays() {
        Ok(n) => {
            unsafe { *out_count = n };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Write the detailed counts: distinct owners per category and distinct stored
/// arrays per kind (static vs forecast).
///
/// # Safety
///
/// Each out pointer must be valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_counts_detailed(
    handle: *const InfraStoreHandle,
    out_components: *mut i64,
    out_supplemental_attributes: *mut i64,
    out_static_time_series: *mut i64,
    out_forecasts: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_components.is_null()
        || out_supplemental_attributes.is_null()
        || out_static_time_series.is_null()
        || out_forecasts.is_null()
    {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.time_series_counts_detailed() {
        Ok(c) => {
            unsafe {
                *out_components = c.components_with_time_series;
                *out_supplemental_attributes = c.supplemental_attributes_with_time_series;
                *out_static_time_series = c.static_time_series_count;
                *out_forecasts = c.forecast_count;
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// List the distinct owner ids of `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) that have a time series, as a JSON array of integers.
/// Optionally restricted to one `time_series_type` (`INFRASTORE_TYPE_*` code, gated by
/// `has_time_series_type`) and/or `resolution` (empty/null = no filter).
/// Probe-then-fetch (see `infrastore_store_list_metadata`).
///
/// # Safety
///
/// The filter args are plain scalars.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_owner_ids(
    handle: *const InfraStoreHandle,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    resolution: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let ts_type = if has_time_series_type {
        match resolve_requested_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        }
    } else {
        None
    };
    let resolution = match unsafe { cstr_to_optional_period(resolution) } {
        Ok(r) => r,
        Err(c) => return c,
    };
    let ids = match store.inner.list_owner_ids(category, ts_type, resolution) {
        Ok(v) => v,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = ids.iter().map(|id| Value::from(*id)).collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Static-series summary as a JSON array. Each object has `owner_type`,
/// `owner_category`, `time_series_type`, `name`, `initial_timestamp_ms`,
/// `resolution`, `time_step_count`, and `count` (the number of associations in
/// the group); fields that do not apply are `null`. Probe-then-fetch.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_static_summary(
    handle: *const InfraStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let rows = match store.inner.static_summary() {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let dur = |d: Option<core_lib::Period>| {
        d.map(|x| Value::from(x.to_iso8601()))
            .unwrap_or(Value::Null)
    };
    let opt_i64 = |n: Option<i64>| n.map(Value::from).unwrap_or(Value::Null);
    let arr: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut o = serde_json::Map::new();
            o.insert("owner_type".into(), Value::from(r.owner_type.clone()));
            o.insert(
                "owner_category".into(),
                Value::from(r.owner_category.as_str()),
            );
            o.insert(
                "time_series_type".into(),
                Value::from(r.time_series_type.as_str()),
            );
            o.insert("name".into(), Value::from(r.name.clone()));
            o.insert(
                "initial_timestamp_ms".into(),
                r.initial_timestamp
                    .map(datetime_to_unix_ms)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
            o.insert("resolution".into(), dur(r.resolution));
            o.insert("time_step_count".into(), opt_i64(r.time_step_count));
            o.insert("count".into(), Value::from(r.count));
            Value::Object(o)
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Forecast summary as a JSON array. Each object has `owner_type`,
/// `owner_category`, `time_series_type`, `name`, `initial_timestamp_ms`,
/// `resolution`, `horizon`, `interval`, `window_count`, and `count` (the
/// number of associations in the group); fields that do not apply are `null`.
/// Probe-then-fetch.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_forecast_summary(
    handle: *const InfraStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let rows = match store.inner.forecast_summary() {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let dur = |d: Option<core_lib::Period>| {
        d.map(|x| Value::from(x.to_iso8601()))
            .unwrap_or(Value::Null)
    };
    let opt_i64 = |n: Option<i64>| n.map(Value::from).unwrap_or(Value::Null);
    let arr: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut o = serde_json::Map::new();
            o.insert("owner_type".into(), Value::from(r.owner_type.clone()));
            o.insert(
                "owner_category".into(),
                Value::from(r.owner_category.as_str()),
            );
            o.insert(
                "time_series_type".into(),
                Value::from(r.time_series_type.as_str()),
            );
            o.insert("name".into(), Value::from(r.name.clone()));
            o.insert(
                "initial_timestamp_ms".into(),
                r.initial_timestamp
                    .map(datetime_to_unix_ms)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
            o.insert("resolution".into(), dur(r.resolution));
            o.insert("horizon".into(), dur(r.horizon));
            o.insert("interval".into(), dur(r.interval));
            o.insert("window_count".into(), opt_i64(r.window_count));
            o.insert("count".into(), Value::from(r.count));
            Value::Object(o)
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Write the store's compression policy.
///
/// `out_kind` receives `0` (no compression) or `1` (DEFLATE). For DEFLATE,
/// `out_level` (0-9) and `out_shuffle` receive the filter parameters; for no
/// compression they are set to `0` / `false`. This reflects the policy the
/// store was created with, restored from the file when the store was opened
/// (in-memory stores report `0`).
///
/// # Safety
///
/// `out_kind` and `out_level` must each be valid for writing one `u8`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_compression(
    handle: *const InfraStoreHandle,
    out_kind: *mut u8,
    out_level: *mut u8,
    out_shuffle: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_kind.is_null() || out_level.is_null() || out_shuffle.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let (kind, level, shuffle) = match store.inner.compression() {
        core_lib::Compression::None => (0u8, 0u8, false),
        core_lib::Compression::Deflate { level, shuffle } => (1u8, level, shuffle),
    };
    unsafe {
        *out_kind = kind;
        *out_level = level;
        *out_shuffle = shuffle;
    }
    INFRASTORE_OK
}

/// Recompute each stored array's content hash and report how many disagree with
/// the hash recorded alongside them through `out_error_count`.
///
/// Covers the HDF5 half of the store only: the SQLite catalog is not inspected,
/// so a zero count does not mean the store as a whole is sound. A catalog that is
/// corrupted, truncated, or paired with the wrong HDF5 file still reports zero,
/// while every read of the affected series fails.
///
/// # Safety
///
/// `handle` must be a live store handle and `out_error_count` must be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_verify(
    handle: *const InfraStoreHandle,
    out_error_count: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_error_count.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.verify_integrity() {
        Ok(r) => {
            unsafe { *out_error_count = r.errors.len() as u64 };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Compact the store and return what it reclaimed as a JSON object
/// `{"slots_reclaimed": <u64>, "datasets_dropped": <u64>,
/// "feature_sets_reclaimed": <u64>, "timestamp_sets_reclaimed": <u64>,
/// "bytes_reclaimed": <u64>}`.
///
/// For an on-disk store this **rewrites the HDF5 file**: the arrays the catalog
/// still references are written into a sibling file which then replaces the
/// original, so removed data actually leaves the file. The store keeps working
/// across the swap (its file handle is reopened on the new file). It assumes
/// this process is the file's only user — see `Store::compact` in the Rust core.
///
/// Returns the JSON through `out_json` as an **owned** allocation the caller
/// releases with `infrastore_string_free`; `out_len` is its byte length. Unlike
/// the fixed-size read-only queries this cannot use probe-then-fetch: the call
/// mutates the store, so it must run exactly once.
///
/// # Safety
///
/// `handle` must not be used concurrently for the duration
/// of the call — the underlying file is replaced part-way through. `out_json` and `out_len` must
/// each be valid for writing one pointer / one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_compact(
    handle: *mut InfraStoreHandle,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let report = match store.inner.compact() {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let json = serde_json::json!({
        "slots_reclaimed": report.slots_reclaimed as u64,
        "datasets_dropped": report.datasets_dropped as u64,
        "feature_sets_reclaimed": report.feature_sets_reclaimed as u64,
        "timestamp_sets_reclaimed": report.timestamp_sets_reclaimed as u64,
        "bytes_reclaimed": report.bytes_reclaimed,
    })
    .to_string();
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// Begin a transaction spanning subsequent operations on `handle`, so that adds,
/// removals, and transforms either all take effect or none do. Calls nest; only
/// the outermost commit makes anything durable.
///
/// Unlike a batch, this is store state rather than a borrowed guard — nothing has
/// to survive across the ABI boundary. Pair every call with exactly one
/// `infrastore_store_commit_transaction` or
/// `infrastore_store_rollback_transaction`.
///
/// Holds the SQLite write lock until the outermost commit or rollback; another
/// writer on the same artifact will block, then fail on its busy timeout.
///
/// Returns `INFRASTORE_OK`, or an error code if the store is read-only.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_begin_transaction(handle: *mut InfraStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.begin_transaction() {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Commit the innermost open transaction on `handle`.
///
/// Returns `INFRASTORE_OK`, or an error code if no transaction is open.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_commit_transaction(handle: *mut InfraStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.commit_transaction() {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Roll back the innermost open transaction on `handle`, undoing every operation
/// it covered — including removals, which are reversible only inside a
/// transaction.
///
/// Returns `INFRASTORE_OK`, or an error code if no transaction is open.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_rollback_transaction(
    handle: *mut InfraStoreHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.rollback_transaction() {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Whether a transaction is currently open on `handle`. Writes `true`/`false`
/// through `out`.
///
/// # Safety
///
/// `out` must be a valid, writable `bool` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_in_transaction(
    handle: *mut InfraStoreHandle,
    out: *mut bool,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let store = deref_handle!(ref handle);
    unsafe { *out = store.inner.in_transaction() };
    INFRASTORE_OK
}

/// Flush pending store writes.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_flush(handle: *mut InfraStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.flush() {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Persist the store's data to `path` (HDF5 arrays) and `<path>.sqlite` (metadata),
/// materializing in-memory stores to disk. Existing target files are overwritten.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_persist(
    handle: *mut InfraStoreHandle,
    path: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    match store.inner.persist_to(&path) {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Persist only the **array half** to `path`, leaving no catalog beside it.
///
/// The mirror of `infrastore_store_persist_catalog`, and the write-side counterpart of
/// `infrastore_store_open_without_catalog`: together they let a consumer ship an artifact as arrays
/// plus a document of its own, with the catalog's rows carried in that document. Which arrays land
/// follows the backend, exactly as `infrastore_store_persist` does: an in-memory store is
/// materialized, so only the arrays the catalog still references are written, while an on-disk
/// store's file is copied whole — dead slots included, since HDF5 does not reclaim that space in
/// place. Call `infrastore_store_compact` first when the bundle's size matters.
///
/// Atomic — one file, one rename. The file still carries a fresh generation stamp, which
/// `infrastore_store_open_without_catalog` copies onto the catalog it mints.
///
/// `INFRASTORE_ERR_STORE_EXISTS` when a `<path>.sqlite` is already beside the destination: it is
/// paired with the file this would replace, so publishing new arrays under it would leave its rows
/// dangling. `INFRASTORE_ERR_INVALID_PARAMETER` for this store's own array file.
///
/// # Safety
///
/// `path` must be a valid NUL-terminated UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_persist_arrays(
    handle: *mut InfraStoreHandle,
    path: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let path = match unsafe { cstr_to_str(path) } {
        Ok(s) => PathBuf::from(s),
        Err(code) => {
            set_error("invalid path string");
            return code;
        }
    };
    match store.inner.persist_arrays_to(&path) {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Write an in-memory catalog to this store's own `<path>.sqlite`, pairing it with the HDF5 file
/// already there.
///
/// `infrastore_store_persist` aimed at another path copies the arrays; this writes only the
/// catalog, because the arrays are already where they belong. That is what makes `catalog_mode=1`
/// usable for what it is good for — skipping per-commit journaling during a bulk load — without
/// copying the array file to land the result.
///
/// A checkpoint, not a mode switch: the catalog stays in memory and later changes are again
/// RAM-only until the next call. For an attached catalog this is `infrastore_store_flush`.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_persist_catalog(handle: *mut InfraStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.persist_catalog() {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

// ---- Attribute-based metadata access --------------------------------------
//
// The Julia `RustTimeSeriesStore` works in terms of (owner_id, name,
// resolution, features) rather than opaque key handles, so these entry points
// build a `TimeSeriesKey` internally and route to the core store. v0 only
// resolves SingleTimeSeries.

/// The exact-identity `ListFilter` for a set of addressing attributes: the whole
/// feature set, matched by hash rather than as a subset. What the `has_*` probes
/// pose their question with — an existence check asks about one series, so a
/// sibling carrying an extra feature must not answer for it.
unsafe fn exact_identity_filter(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    interval: Option<core_lib::Period>,
    features_json: *const c_char,
) -> Result<core_lib::ListFilter, i32> {
    let name = unsafe { cstr_to_str(name) }.inspect_err(|_| {
        set_error("name is invalid");
    })?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let features = unsafe { parse_features_json(features_json) }?;
    let resolution = unsafe { cstr_to_optional_period(resolution)? };
    Ok(core_lib::ListFilter {
        owner_id: Some(owner_id),
        owner_category: Some(owner_category),
        name: Some(name.to_string()),
        resolution,
        interval,
        features: Some(features),
        features_exact: true,
        ..Default::default()
    })
}

/// Fetch the metadata row filed under `association_id`, as one JSON object.
///
/// The read-direction counterpart of the id every `infrastore_store_add_*`
/// hands back: a caller that recorded ids in its own model resolves them here
/// without keeping an id-to-key map beside the store. The row shape is exactly
/// `infrastore_store_get_metadata_by_id`'s, `id` included.
///
/// Writes `false` to `*out_present` and nothing to `buf` when the catalog holds
/// no such row, returning `INFRASTORE_OK` rather than
/// `INFRASTORE_ERR_NOT_FOUND`: a caller validating references it stored earlier
/// is asking whether one still resolves, and a stale reference is an answer.
///
/// Follows the two-call buffer protocol used throughout: call with `buf` null
/// (or too small) to learn the required length from `*out_len`, then call again
/// with a buffer of that size.
///
/// # Safety
///
/// `buf`, when non-null, must be valid for writing `cap` bytes. `out_len` must be valid for
/// writing one `u64` and `out_present` for writing one `bool`; both are required.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_metadata_by_id(
    handle: *const InfraStoreHandle,
    association_id: i64,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() || out_present.is_null() {
        set_error("out_len or out_present is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store
        .inner
        .get_metadata_by_id(core_lib::TimeSeriesId(association_id))
    {
        Ok(Some(meta)) => {
            let json = Value::Object(metadata_to_map(&meta)).to_string();
            unsafe {
                *out_present = true;
                write_str_out(&json, buf, cap, out_len);
            }
            INFRASTORE_OK
        }
        Ok(None) => {
            unsafe {
                *out_present = false;
                *out_len = 0;
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Whether an association is filed under `association_id`.
///
/// A primary-key probe: one statement, no row fetched, no metadata built. Cheap
/// enough to validate every reference in a model on load, rather than
/// discovering a dangling one mid-run. Use
/// `infrastore_store_get_metadata_by_id` when the row is wanted too.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_association_exists(
    handle: *const InfraStoreHandle,
    association_id: i64,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        set_error("out_present is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store
        .inner
        .association_exists(core_lib::TimeSeriesId(association_id))
    {
        Ok(found) => {
            unsafe { *out_present = found };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// True iff a SingleTimeSeries with the given attributes exists.
///
/// # Safety
///
/// `features_json` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_has_by_attrs(
    handle: *const InfraStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        exact_identity_filter(
            owner_id,
            owner_category,
            name,
            resolution,
            None,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(code) => return code,
    };
    match store.inner.has_any_time_series(filter) {
        Ok(b) => {
            unsafe { *out_present = b };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// True iff `owner_id` has any time series, optionally filtered to a single
/// time series type (`use_type` selects whether `ts_type` is applied). Answers
/// the name-less `has_time_series(owner)` / `has_time_series(owner, T)` queries.
///
/// # Safety
///
/// `owner_id` is a plain integer and `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) identifies the owner category; `out_present` valid for writing one
/// bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_has_for_owner(
    handle: *const InfraStoreHandle,
    owner_id: i64,
    owner_category: i32,
    ts_type: i32,
    use_type: bool,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let mut filter = core_lib::ListFilter::new()
        .owner_id(owner_id)
        .owner_category(category);
    if use_type {
        let t = match resolve_requested_type_from_int(ts_type) {
            Some(t) => t,
            None => {
                set_error(format!("invalid time_series_type {ts_type}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        };
        filter = filter.time_series_type(t);
    }
    match store.inner.has_any_time_series(filter) {
        Ok(present) => {
            unsafe { *out_present = present };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// True iff at least one association matches the filter — the existence probe
/// over the full `infrastore_store_list_metadata` filter surface (all-optional,
/// independent predicates; `features_json` is a subset match). Unlike
/// `infrastore_store_has_typed`, which matches one exact key identity (its
/// feature set compared by content hash), this answers "is there any series
/// like this?" without hydrating or serializing a single row, so it is safe
/// for hot per-component loops. The one exception is a non-empty
/// `features_json`: the subset match cannot be answered from an index and
/// falls back to a full listing internally, so callers testing an exact
/// feature set in a hot loop should prefer `infrastore_store_has_typed`.
///
/// # Safety
///
/// The scalar filter flags/values are plain scalars. `name`, `name_glob`, `resolution`,
/// `interval`; `features_json` must each be null or a null-terminated UTF-8 string.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_has_any_by_filter(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            name_glob,
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.has_any_time_series(filter) {
        Ok(present) => {
            unsafe { *out_present = present };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Fetch a stored array by its 32-byte content hash. On success the caller owns
/// `*out_data` and must free it with `infrastore_buffer_free_u8`.
///
/// # Safety
///
/// `handle` must be a live store handle, `data_hash` must reference 32 readable bytes, and every
/// output pointer must be valid for writing its indicated value. The returned buffer must be
/// released exactly once with `infrastore_buffer_free_u8` using the returned byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_get_array_by_hash(
    handle: *const InfraStoreHandle,
    data_hash: *const u8,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if data_hash.is_null() || out_dtype.is_null() || out_data.is_null() || out_byte_len.is_null() {
        set_error("a pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let mut hash = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(data_hash, hash.as_mut_ptr(), 32) };
    let array = match store.inner.get_array_by_hash(&hash) {
        Ok(a) => a,
        Err(e) => return map_core_error(e),
    };
    // Hand back the raw little-endian element bytes + dtype; the caller
    // interprets them according to the requested element type.
    let dtype = array.dtype.code();
    let (p, len) = vec_into_raw(array.bytes);
    unsafe {
        *out_dtype = dtype;
        *out_data = p;
        *out_byte_len = len;
    }
    INFRASTORE_OK
}

/// Count `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
/// that reference the 32-byte content hash `data_hash`, across all owners,
/// writing the counts to `*out_sts` and `*out_dst`. A binding uses these to
/// decide whether removing a `SingleTimeSeries` would orphan a DST that shares
/// its underlying array — a single catalog query rather than a full scan in the
/// caller.
///
/// # Safety
///
/// - `handle` must be a live store handle.
/// - `data_hash` must be non-null and point to at least 32 readable bytes.
/// - `out_sts` and `out_dst` must each be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_count_array_references(
    handle: *const InfraStoreHandle,
    data_hash: *const u8,
    out_sts: *mut u64,
    out_dst: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if data_hash.is_null() || out_sts.is_null() || out_dst.is_null() {
        set_error("a pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let mut hash = [0u8; 32];
    unsafe { ptr::copy_nonoverlapping(data_hash, hash.as_mut_ptr(), 32) };
    let (sts, dst) = match store.inner.count_array_references(&hash) {
        Ok(c) => c,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_sts = sts as u64;
        *out_dst = dst as u64;
    }
    INFRASTORE_OK
}

// ---- Forecasts (Deterministic / DeterministicSingleTimeSeries / ...) -------

fn time_series_type_from_int(i: i32) -> Option<core_lib::TimeSeriesType> {
    use core_lib::TimeSeriesType as T;
    Some(match i {
        0 => T::SingleTimeSeries,
        1 => T::NonSequentialTimeSeries,
        2 => T::Deterministic,
        3 => T::DeterministicSingleTimeSeries,
        4 => T::Probabilistic,
        5 => T::Scenarios,
        _ => return None,
    })
}

/// Map a *key resolution* request's type code to a [`core_lib::TimeSeriesType`].
/// Unlike [`requested_type_from_int`] this accepts every stored type, not just
/// the forecasts: resolving an identity to its key is meaningful for a
/// `SingleTimeSeries` too, and `Store::resolve_metadata` handles any type
/// (it filters candidates by the requested type, nothing more).
///
/// There is no family sentinel: requesting `INFRASTORE_TYPE_DETERMINISTIC`
/// already matches a stored `DeterministicSingleTimeSeries`, and
/// `out_matched_type` reports which form was found.
fn resolve_requested_type_from_int(i: i32) -> Option<core_lib::TimeSeriesType> {
    time_series_type_from_int(i)
}

/// Map a forecast read request's `ts_type` code to a [`core_lib::TimeSeriesType`].
/// The non-forecast types `SingleTimeSeries` (0) and `NonSequentialTimeSeries`
/// (1) are rejected here so the forecast API reports a clear "invalid
/// time_series_type" error up front rather than failing later in
/// `emit_forecast_data` after a key is resolved and data is read.
fn requested_type_from_int(i: i32) -> Option<core_lib::TimeSeriesType> {
    use core_lib::TimeSeriesType as T;
    match time_series_type_from_int(i) {
        Some(
            t @ (T::Deterministic
            | T::DeterministicSingleTimeSeries
            | T::Probabilistic
            | T::Scenarios),
        ) => Some(t),
        _ => None,
    }
}

/// Inverse of [`time_series_type_from_int`]: the integer discriminant for a
/// `TimeSeriesType` (must stay in sync with that mapping).
fn time_series_type_to_int(t: core_lib::TimeSeriesType) -> i32 {
    use core_lib::TimeSeriesType as T;
    match t {
        T::SingleTimeSeries => 0,
        T::NonSequentialTimeSeries => 1,
        T::Deterministic => 2,
        T::DeterministicSingleTimeSeries => 3,
        T::Probabilistic => 4,
        T::Scenarios => 5,
    }
}

/// Write `s` (NUL-terminated, truncated to `cap - 1` bytes) into `buf`, always
/// reporting the full byte length through `out_len`. Safe to call with a null /
/// zero-capacity buffer to probe the required length first.
///
/// # Safety
///
/// When `buf` is non-null it must be valid for writing `cap` bytes.
unsafe fn write_str_out(s: &str, buf: *mut c_char, cap: u64, out_len: *mut u64) {
    let bytes = s.as_bytes();
    unsafe {
        *out_len = bytes.len() as u64;
        if !buf.is_null() && cap > 0 {
            let n = bytes.len().min((cap - 1) as usize);
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
            *buf.add(n) = 0;
        }
    }
}

/// `interval` may be null. It only ever needs to be supplied to disambiguate a name that
/// carries several forecasts differing solely by interval (as
/// `transform_single_time_series` with `delete_existing = false` produces); a null leaves
/// the key's interval unset and matches on the other attributes alone.
unsafe fn build_typed_key_from_attrs(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
) -> Result<core_lib::ListFilter, i32> {
    let time_series_type = match time_series_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let interval = unsafe { cstr_to_optional_period(interval)? };
    let mut filter = unsafe {
        exact_identity_filter(
            owner_id,
            owner_category,
            name,
            resolution,
            interval,
            features_json,
        )
    }?;
    filter.time_series_type = Some(time_series_type);
    Ok(filter)
}

/// Add a dense forecast. `data_ptr`/`data_byte_len` is the flattened storage
/// array (Deterministic: `[H, count, *E]`; Scenarios: `[scenario_count, H,
/// count, *E]`). `ts_type` must be 2=Deterministic or 5=Scenarios;
/// `DeterministicSingleTimeSeries` is not directly addable and is derived from a
/// stored `SingleTimeSeries` via `infrastore_store_transform_single_time_series`.
///
/// # Safety
///
/// Optional strings may be null. `data_ptr` must reference `data_len` elements and `out_key`
/// must be valid for writing one pointer.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_add_forecast(
    handle: *mut InfraStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let req = match unsafe {
        build_forecast_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            ts_type,
            initial_ts_unix_ms,
            resolution,
            horizon,
            interval,
            count,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut added) => {
            let id = added.remove(0);
            if !out_id.is_null() {
                unsafe { *out_id = id.get() };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `infrastore_store_add_forecast` / `infrastore_batch_add_forecast` argument list
/// (Deterministic / Scenarios) into an [`core_lib::AddRequest`].
#[allow(clippy::too_many_arguments)]
unsafe fn build_forecast_request(
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(INFRASTORE_ERR_NULL_POINTER);
    }
    let time_series_type = match time_series_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let quantity_kind = unsafe { cstr_to_optional_string(quantity_kind) }?;
    let unit_system = unsafe { cstr_to_optional_unit_system(unit_system) }?;
    let time_reference = unsafe { cstr_to_optional_time_reference(time_reference) }?;
    let component_field = unsafe { cstr_to_optional_string(component_field) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let application_data = unsafe { cstr_to_optional_string(application_data) }?;
    let element_type = unsafe { cstr_to_element_type(element_type) }?;
    let array =
        unsafe { build_typed_array(element_type, ndims, dims_ptr, data_ptr, data_byte_len) }?;

    let resolution = unsafe { cstr_to_period(resolution)? };
    let horizon = unsafe { cstr_to_period(horizon)? };
    let interval = unsafe { cstr_to_period(interval)? };
    let data = match time_series_type {
        core_lib::TimeSeriesType::Deterministic => match core_lib::Deterministic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count as usize,
            array,
            name,
        ) {
            Ok(d) => core_lib::TimeSeriesData::Deterministic(d),
            Err(e) => {
                set_error(e);
                return Err(INFRASTORE_ERR_INVALID_PARAMETER);
            }
        },
        core_lib::TimeSeriesType::Scenarios => {
            let scenario_count = array.shape.first().copied().unwrap_or(0);
            match core_lib::Scenarios::new(
                initial_timestamp,
                resolution,
                horizon,
                interval,
                count as usize,
                scenario_count,
                array,
                name,
            ) {
                Ok(s) => core_lib::TimeSeriesData::Scenarios(s),
                Err(e) => {
                    set_error(e);
                    return Err(INFRASTORE_ERR_INVALID_PARAMETER);
                }
            }
        }
        other => {
            set_error(format!(
                "infrastore_store_add_forecast supports Deterministic and Scenarios; {other:?} \
                 is not directly addable (DeterministicSingleTimeSeries is derived via \
                 infrastore_store_transform_single_time_series)"
            ));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };

    let mut data = data;
    // The descriptors describe the series, so they travel on it rather than
    // on the request.
    data.set_descriptors(core_lib::Descriptors {
        element_type,
        units,
        quantity_kind,
        unit_system,
        time_reference,
        component_field,
        application_data,
    });
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
    })
}

/// Add a `Probabilistic` forecast. `data` is the flattened 3-D storage array
/// `(percentile_count, horizon_count, count)` column-major; `percentiles` is the
/// percentile vector.
///
/// # Safety
///
/// Optional strings may be null. `percentiles_ptr` and `data_ptr` must reference their
/// respective element counts. `out_key`, when non-null, must be valid for writing one
/// pointer.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_add_probabilistic(
    handle: *mut InfraStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    percentiles_ptr: *const f64,
    percentiles_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let req = match unsafe {
        build_probabilistic_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            initial_ts_unix_ms,
            resolution,
            horizon,
            interval,
            count,
            percentiles_ptr,
            percentiles_len,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut added) => {
            let id = added.remove(0);
            if !out_id.is_null() {
                unsafe { *out_id = id.get() };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `infrastore_store_add_probabilistic` / `infrastore_batch_add_probabilistic`
/// argument list into an [`core_lib::AddRequest`].
#[allow(clippy::too_many_arguments)]
unsafe fn build_probabilistic_request(
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    percentiles_ptr: *const f64,
    percentiles_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() || percentiles_ptr.is_null() {
        set_error("a required pointer is null");
        return Err(INFRASTORE_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let quantity_kind = unsafe { cstr_to_optional_string(quantity_kind) }?;
    let unit_system = unsafe { cstr_to_optional_unit_system(unit_system) }?;
    let time_reference = unsafe { cstr_to_optional_time_reference(time_reference) }?;
    let component_field = unsafe { cstr_to_optional_string(component_field) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let percentiles =
        unsafe { slice::from_raw_parts(percentiles_ptr, percentiles_len as usize) }.to_vec();
    let application_data = unsafe { cstr_to_optional_string(application_data) }?;
    let element_type = unsafe { cstr_to_element_type(element_type) }?;
    let array =
        unsafe { build_typed_array(element_type, ndims, dims_ptr, data_ptr, data_byte_len) }?;

    let prob = match core_lib::Probabilistic::new(
        initial_timestamp,
        unsafe { cstr_to_period(resolution)? },
        unsafe { cstr_to_period(horizon)? },
        unsafe { cstr_to_period(interval)? },
        count as usize,
        percentiles,
        array,
        name,
    ) {
        Ok(p) => p,
        Err(e) => {
            set_error(e);
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let mut data = core_lib::TimeSeriesData::Probabilistic(prob);
    // The descriptors describe the series, so they travel on it rather than
    // on the request.
    data.set_descriptors(core_lib::Descriptors {
        element_type,
        units,
        quantity_kind,
        unit_system,
        time_reference,
        component_field,
        application_data,
    });
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
    })
}

// ---- batched adds ----------------------------------------------------------
//
// A batch accumulates AddRequests client-side; `infrastore_store_add_batch` commits
// them through `Store::add_time_series_bulk` in ONE metadata transaction.
// This is the fast path for ingesting many series: per-item adds pay one
// SQLite commit each, while a batch pays a single commit for all items.

/// Create an empty add-batch. Building a batch performs no store I/O.
///
/// # Safety
///
/// The returned handle must be released exactly once with `infrastore_batch_free`
/// (regardless of whether it was submitted via `infrastore_store_add_batch`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_batch_new() -> *mut InfraStoreBatchHandle {
    Box::into_raw(Box::new(InfraStoreBatchHandle { items: Vec::new() }))
}

/// Free a batch handle created by `infrastore_batch_new`.
///
/// # Safety
///
/// `batch` must be null or a handle returned by `infrastore_batch_new` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_batch_free(batch: *mut InfraStoreBatchHandle) {
    if !batch.is_null() {
        drop(unsafe { Box::from_raw(batch) });
    }
}

/// Append a SingleTimeSeries to a batch. Arguments match
/// `infrastore_store_add_single` (minus the store handle and `out_key`); the data is
/// copied into the batch, so the caller's buffers need only stay valid for
/// this call.
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `dims_ptr` must reference `ndims` elements when `ndims` is nonzero;
/// `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_batch_add_single(
    batch: *mut InfraStoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    match unsafe {
        build_single_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            initial_ts_unix_ms,
            resolution,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            INFRASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a NonSequentialTimeSeries to a batch. Arguments match
/// `infrastore_store_add_non_sequential` (minus the store handle and `out_key`).
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `timestamps_unix_ms` must reference `timestamps_len` elements,
/// `dims_ptr` must reference `ndims` elements when `ndims` is nonzero; `data_ptr` must
/// reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_batch_add_non_sequential(
    batch: *mut InfraStoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    match unsafe {
        build_non_sequential_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            timestamps_unix_ms,
            timestamps_len,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            INFRASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a dense forecast (`ts_type` 2=Deterministic or 5=Scenarios) to a
/// batch. Arguments match `infrastore_store_add_forecast` (minus the store handle and
/// `out_key`).
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `dims_ptr` must reference `ndims` elements when `ndims` is nonzero;
/// `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_batch_add_forecast(
    batch: *mut InfraStoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    match unsafe {
        build_forecast_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            ts_type,
            initial_ts_unix_ms,
            resolution,
            horizon,
            interval,
            count,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            INFRASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a `Probabilistic` forecast to a batch. Arguments match
/// `infrastore_store_add_probabilistic` (minus the store handle and `out_key`).
///
/// # Safety
///
/// Required string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `percentiles_ptr` must reference `percentiles_len` elements,
/// `dims_ptr` must reference `ndims` elements when `ndims` is nonzero; `data_ptr` must
/// reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_batch_add_probabilistic(
    batch: *mut InfraStoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    horizon: *const c_char,
    interval: *const c_char,
    count: u64,
    percentiles_ptr: *const f64,
    percentiles_len: u64,
    element_type: *const c_char,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    application_data: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    quantity_kind: *const c_char,
    unit_system: *const c_char,
    time_reference: *const c_char,
    component_field: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    match unsafe {
        build_probabilistic_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            initial_ts_unix_ms,
            resolution,
            horizon,
            interval,
            count,
            percentiles_ptr,
            percentiles_len,
            element_type,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            application_data,
            features_json,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            INFRASTORE_OK
        }
        Err(c) => c,
    }
}

/// Submit every request in `batch` through one all-or-nothing bulk add. On
/// success, writes an array of key handles (input order) to `out_keys` /
/// `out_len`. The batch is drained by this call in all cases — on error
/// nothing was committed and the batch is left empty; rebuild it before
/// retrying.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `batch` a live batch
/// handle. `out_keys` and `out_len` must each be valid for writing one value.
/// On success the caller owns the returned array and every key handle in it:
/// release the id buffer with `infrastore_buffer_free_i64(*out_ids, *out_len)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_add_batch(
    handle: *mut InfraStoreHandle,
    batch: *mut InfraStoreBatchHandle,
    out_len: *mut u64,
    out_ids: *mut *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_ids.is_null() || out_len.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let items = std::mem::take(&mut batch.items);
    match store.inner.add_time_series_bulk(items) {
        Ok(added) => {
            let ids: Vec<i64> = added.iter().map(|i| i.get()).collect();
            let len = added.len() as u64;
            // An empty batch is reported as null (no free needed); otherwise
            // the handed-out allocation is exactly `len` elements (see
            // `vec_into_raw`), released with `infrastore_buffer_free_i64`.
            let id_ptr = if ids.is_empty() {
                ptr::null_mut()
            } else {
                vec_into_raw(ids).0
            };
            unsafe {
                *out_ids = id_ptr;
                *out_len = len;
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// A bulk read fetches many full SingleTimeSeries in one call, reading each
// packed dataset's column span once (`Store::bulk_read`) instead of re-reading
// every chunk per series. Results are held in a `InfraStoreBulkReadHandle` and read out
// element-by-element through the `infrastore_bulk_result_*` accessors.

/// Write a series' descriptive attributes (`application_data`, `element_type`,
/// `units`, `quantity_kind`, `unit_system`, `time_reference`, `component_field`)
/// into seven optional out-params.
///
/// Each pointer may be null to skip that attribute. A non-null target receives
/// either null (the attribute is unset on this series) or an owned C string the
/// caller must free exactly once with `infrastore_string_free`. `unit_system`
/// is emitted as its `natural_units` / `component_base` spelling rather than a
/// code, so an added basis reaches an older caller as a name it can report.
///
/// All seven live on the series itself, so a bulk-read result carries them just
/// as a per-key read does — the two paths must not disagree.
///
/// All-or-nothing: every attribute string is built first (an interior NUL in
/// any of them fails with `INFRASTORE_ERR_INTEGRITY` before anything is
/// written), then everything is written. Callers invoke this *before* handing
/// any other buffer to the caller, so a failure here leaks nothing.
///
/// # Safety
///
/// Each non-null pointer must be valid for writing one pointer.
#[allow(clippy::too_many_arguments)]
unsafe fn emit_descriptors(
    application_data: Option<&str>,
    element_type: core_lib::ElementType,
    units: Option<&str>,
    quantity_kind: Option<&str>,
    unit_system: Option<core_lib::UnitSystem>,
    time_reference: Option<&core_lib::TimeReference>,
    component_field: Option<&str>,
    out_application_data: *mut *mut c_char,
    out_element_type: *mut *mut c_char,
    out_units: *mut *mut c_char,
    out_quantity_kind: *mut *mut c_char,
    out_unit_system: *mut *mut c_char,
    out_time_reference: *mut *mut c_char,
    out_component_field: *mut *mut c_char,
) -> i32 {
    let application_data_c = if out_application_data.is_null() {
        None
    } else {
        match opt_attr_cstring(application_data) {
            Ok(c) => c,
            Err(code) => return code,
        }
    };
    let units_c = if out_units.is_null() {
        None
    } else {
        match opt_attr_cstring(units) {
            Ok(c) => c,
            Err(code) => return code,
        }
    };
    let quantity_kind_c = if out_quantity_kind.is_null() {
        None
    } else {
        match opt_attr_cstring(quantity_kind) {
            Ok(c) => c,
            Err(code) => return code,
        }
    };
    let component_field_c = if out_component_field.is_null() {
        None
    } else {
        match opt_attr_cstring(component_field) {
            Ok(c) => c,
            Err(code) => return code,
        }
    };
    // No fallible step: `UnitSystem::as_str` is a fixed identifier, so it can
    // never carry an interior NUL the way a user-supplied label can.
    let unit_system_c = if out_unit_system.is_null() {
        None
    } else {
        unit_system.map(|u| owned_cstr(u.as_str()))
    };
    // Same reasoning: every `TimeReference` spelling is either a fixed literal,
    // a formatted offset, or a zone name the core already refused unless it
    // matched the IANA grammar — none of which can hold an interior NUL.
    let time_reference_c = if out_time_reference.is_null() {
        None
    } else {
        time_reference.map(|r| owned_cstr(&r.as_storage_string()))
    };
    unsafe {
        if !out_application_data.is_null() {
            *out_application_data = into_raw_or_null(application_data_c);
        }
        // Unlike the optional attributes, the element type is never absent — a
        // series always carries a concrete one — so this is always a string.
        if !out_element_type.is_null() {
            *out_element_type = owned_cstr(&element_type.to_string());
        }
        if !out_units.is_null() {
            *out_units = into_raw_or_null(units_c);
        }
        if !out_quantity_kind.is_null() {
            *out_quantity_kind = into_raw_or_null(quantity_kind_c);
        }
        if !out_unit_system.is_null() {
            *out_unit_system = unit_system_c.unwrap_or(std::ptr::null_mut());
        }
        if !out_time_reference.is_null() {
            *out_time_reference = time_reference_c.unwrap_or(std::ptr::null_mut());
        }
        if !out_component_field.is_null() {
            *out_component_field = into_raw_or_null(component_field_c);
        }
    }
    INFRASTORE_OK
}

/// The number of series held by a bulk-read result handle, or `-1` if `result`
/// is null.
///
/// # Safety
///
/// `result` must be null or a live handle from a read call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_bulk_result_len(
    result: *const InfraStoreBulkReadHandle,
) -> i64 {
    match unsafe { result.as_ref() } {
        Some(r) => r.items.len() as i64,
        None => -1,
    }
}

/// Read element `index` out of a bulk-read result handle. The out parameters
/// follow the usual convention: the caller owns the `out_resolution` string and
/// the `out_shape` / `out_data` buffers and must release them with
/// `infrastore_string_free`, `infrastore_buffer_free_i64`, and `infrastore_buffer_free_u8`. The handle
/// is not consumed, so an element may be read more than once.
///
/// `out_application_data`, `out_element_type`, `out_units`, `out_quantity_kind`,
/// `out_unit_system`, and `out_component_field` each receive an owned C string
/// (null when that attribute is unset), freed with `infrastore_string_free`. Any
/// of the six may be null to skip it. They carry the same values a per-key
/// read returns: the attributes live on the series, so both paths agree.
///
/// # Safety
///
/// `result` must be a live handle from a read call and `index`
/// must be less than its length. Every output pointer except `out_application_data`,
/// `out_element_type`, `out_units`, `out_quantity_kind`, `out_unit_system`, and
/// `out_component_field` must be valid for writing its indicated value; those
/// six may be null. `*out_resolution` is an owned string too -- never null on
/// success -- and it, along with a non-null `*out_application_data` /
/// `*out_element_type` / `*out_units` / `*out_quantity_kind` /
/// `*out_unit_system` / `*out_component_field`, must be freed exactly once with
/// `infrastore_string_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_bulk_result_get_single(
    result: *const InfraStoreBulkReadHandle,
    index: u64,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_application_data: *mut *mut c_char,
    out_element_type: *mut *mut c_char,
    out_units: *mut *mut c_char,
    out_quantity_kind: *mut *mut c_char,
    out_unit_system: *mut *mut c_char,
    out_time_reference: *mut *mut c_char,
    out_component_field: *mut *mut c_char,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_dtype.is_null()
        || out_shape.is_null()
        || out_shape_len.is_null()
        || out_data.is_null()
        || out_data_byte_len.is_null()
    {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let single = match result.items.get(index as usize) {
        Some(core_lib::TimeSeriesData::SingleTimeSeries(s)) => s,
        Some(other) => {
            set_error(format!(
                "bulk-read item {index} is a {}, not a SingleTimeSeries",
                other.time_series_type().as_str()
            ));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    // Descriptors first: the only fallible step, and it writes nothing on
    // failure, so no other handed-out buffer can be orphaned by it.
    let code = unsafe {
        emit_descriptors(
            single.application_data.as_deref(),
            single.element_type,
            single.units.as_deref(),
            single.quantity_kind.as_deref(),
            single.unit_system,
            single.time_reference.as_ref(),
            single.component_field.as_deref(),
            out_application_data,
            out_element_type,
            out_units,
            out_quantity_kind,
            out_unit_system,
            out_time_reference,
            out_component_field,
        )
    };
    if code != INFRASTORE_OK {
        return code;
    }
    let resolution_cstr = period_cstr(single.resolution);
    let dtype = single.data.dtype;
    // Owned copies so the result handle stays intact for repeated reads.
    let shape: Vec<i64> = single.data.shape.iter().map(|&d| d as i64).collect();
    let (shape_ptr, shape_len) = vec_into_raw(shape);
    let (data_ptr, data_len) = vec_into_raw(single.data.bytes.clone());
    unsafe {
        *out_initial_ts_unix_ms = datetime_to_unix_ms(single.initial_timestamp);
        *out_resolution = resolution_cstr;
        *out_dtype = dtype.code();
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_len;
    }
    INFRASTORE_OK
}

/// Read many series named by their catalog association `id`, in the order the
/// ids are given. The id-addressed counterpart of `infrastore_store_bulk_read`:
/// results come back in the same `InfraStoreBulkReadHandle`, so a caller reads
/// them out with the same `infrastore_bulk_result_*` accessors, and repeats in
/// `ids` are honoured in place.
///
/// The read direction of the id every write hands back through its `out_id`: a
/// caller that recorded ids in its own model resolves them here instead of
/// keeping an id-to-key map beside the store. Returns
/// `INFRASTORE_ERR_NOT_FOUND` if any id names no row — unlike
/// `infrastore_store_association_exists`, which asks the question, this call is
/// already committed to reading and a stale reference is a failure. The error
/// does not say *which* id dangled.
///
/// # Safety
///
/// `ids` must point to `n` readable `i64`s (it may be null only when `n` is 0).
/// On `INFRASTORE_OK` the returned handle must be released exactly once with
/// `infrastore_bulk_result_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_read_by_ids(
    handle: *const InfraStoreHandle,
    ids: *const i64,
    n: u64,
    out_result: *mut *mut InfraStoreBulkReadHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_result.is_null() {
        set_error("out_result pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let count = n as usize;
    if count != 0 && ids.is_null() {
        set_error("ids pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let id_slice = if count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(ids, count) }
    };
    let id_slice: Vec<core_lib::TimeSeriesId> = id_slice
        .iter()
        .copied()
        .map(core_lib::TimeSeriesId)
        .collect();
    let items = match store
        .inner
        .read_by_ids(&id_slice, core_lib::ReadWindow::full())
    {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe { *out_result = Box::into_raw(Box::new(InfraStoreBulkReadHandle { items })) };
    INFRASTORE_OK
}

/// Read many series named by their catalog association `id`, each clipped to
/// whatever lies within the given time range.
///
/// The *bounds* read beside `infrastore_store_read_by_ids`' *window* read. A
/// window says "these exact steps" and is checked; a range says "whatever falls
/// between these instants" and clips to what is there. A caller exporting a
/// month of a store it did not write knows the bounds it wants and not how many
/// steps each series has in them.
///
/// `start_ms` / `end_ms` are Unix milliseconds, spelled zoned or zoneless by
/// `zoneless` like every other bound. A set mixing zoneless and instant-bearing
/// series has no single valid spelling and is
/// `INFRASTORE_ERR_INVALID_PARAMETER` rather than resolved per series.
/// Results come back in the same `InfraStoreBulkReadHandle` as every other bulk
/// read, in the order the ids are given.
///
/// # Safety
///
/// `ids` must point to `n` readable `i64`s (it may be null only when `n` is 0).
/// On `INFRASTORE_OK` the returned handle must be released exactly once with
/// `infrastore_bulk_result_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_read_by_ids_range(
    handle: *const InfraStoreHandle,
    ids: *const i64,
    n: u64,
    zoneless: bool,
    start_ms: i64,
    end_ms: i64,
    out_result: *mut *mut InfraStoreBulkReadHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_result.is_null() {
        set_error("out_result pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let count = n as usize;
    if count != 0 && ids.is_null() {
        set_error("ids pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let range = match build_time_range(true, zoneless, start_ms, end_ms) {
        Ok(Some(r)) => r,
        Ok(None) => unreachable!("build_time_range with present=true always yields a range"),
        Err(c) => return c,
    };
    let raw = if count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(ids, count) }
    };
    let id_slice: Vec<core_lib::TimeSeriesId> =
        raw.iter().copied().map(core_lib::TimeSeriesId).collect();
    let items = match store.inner.read_by_ids_range(&id_slice, range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe { *out_result = Box::into_raw(Box::new(InfraStoreBulkReadHandle { items })) };
    INFRASTORE_OK
}

/// Read the series filed under `id`, or the window of it the arguments name, in
/// one call. The result comes back in the same `InfraStoreBulkReadHandle` as the
/// bulk reads, holding exactly one item, so a caller decodes it with the same
/// `infrastore_bulk_result_*` accessors.
///
/// The id is a primary-key lookup and its row carries the grid, so both halves
/// of a sliced read happen here: a caller holding an id needs no metadata call
/// to ask for the second day of a series. Each optional argument is a
/// `*_present` flag beside its value; with none present this reads the whole
/// series, exactly as `infrastore_store_read_by_ids` would for one id.
///
/// `start_ms` is Unix milliseconds, spelled zoned or zoneless by
/// `start_zoneless` like every other bound. `len` counts timesteps and applies
/// to the static types; `count` counts windows and applies to the forecasts.
/// Supplying the one that does not apply is `INFRASTORE_ERR_INVALID_PARAMETER`,
/// not an argument the store drops -- as is a start off the series' grid or an
/// extent running past its end, which a raw time range would instead clamp.
/// `INFRASTORE_ERR_NOT_FOUND` if the id names no row.
///
/// Set `has_owner` to hold the row to the owner `(owner_id, owner_category)`,
/// and get `INFRASTORE_ERR_OWNER_MISMATCH` when it belongs to someone else. The
/// owner is taken off the same row the values are materialized from, so the
/// guarded read costs exactly what the unguarded one does — where confirming
/// the owner in a call of its own would be a second round trip whose answer
/// describes the row as it was rather than the row being read. `owner_id` and
/// `owner_category` are ignored when `has_owner` is false.
///
/// # Safety
///
/// `out_result` must be valid for writing one pointer. On `INFRASTORE_OK` the
/// returned handle must be released exactly once with
/// `infrastore_bulk_result_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_read_by_id(
    handle: *const InfraStoreHandle,
    id: i64,
    start_present: bool,
    start_zoneless: bool,
    start_ms: i64,
    len_present: bool,
    len: u64,
    count_present: bool,
    count: u64,
    has_owner: bool,
    owner_id: i64,
    owner_category: i32,
    out_result: *mut *mut InfraStoreBulkReadHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_result.is_null() {
        set_error("out_result pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let start = if start_present {
        match unix_ms_to_datetime(start_ms) {
            Some(dt) => Some(dt),
            None => {
                set_error(format!("invalid start_ms: {start_ms}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        }
    } else {
        None
    };
    // The ABI carries both extents as `u64` because that is what the wrapper
    // languages spell; on a target where `usize` is narrower, an `as` cast
    // would quietly hand the core a different window than the caller asked
    // for.
    let extent = |present: bool, value: u64, name: &str| match present {
        false => Ok(None),
        true => usize::try_from(value).map(Some).map_err(|_| {
            set_error(format!("{name} {value} is too large for this platform"));
            INFRASTORE_ERR_INVALID_PARAMETER
        }),
    };
    let len = match extent(len_present, len, "len") {
        Ok(v) => v,
        Err(code) => return code,
    };
    let count = match extent(count_present, count, "count") {
        Ok(v) => v,
        Err(code) => return code,
    };
    let window = core_lib::ReadWindow {
        start,
        zoneless: start_zoneless,
        len,
        count,
    };
    let owner = match optional_owner(has_owner, owner_id, owner_category) {
        Ok(owner) => owner,
        Err(code) => return code,
    };
    let read = match owner {
        Some(owner) => store
            .inner
            .read_by_id_for_owner(core_lib::TimeSeriesId(id), owner, window),
        None => store.inner.read_by_id(core_lib::TimeSeriesId(id), window),
    };
    let item = match read {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_result = Box::into_raw(Box::new(InfraStoreBulkReadHandle { items: vec![item] }))
    };
    INFRASTORE_OK
}

/// Write the name of bulk-read item `index` into `out_name` as an owned C
/// string. The result handle carries each item's name whichever way the read
/// was addressed, so this is how both `infrastore_store_bulk_read` and
/// `infrastore_store_read_by_ids` label what they got back — the companion to
/// `infrastore_bulk_result_item_type` beside it.
///
/// # Safety
///
/// `result` must be a live bulk-read handle, `index` less than its length, and
/// `out_name` valid for writing one pointer. `*out_name` is an owned string --
/// never null on success -- and must be freed exactly once with
/// `infrastore_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_bulk_result_item_name(
    result: *const InfraStoreBulkReadHandle,
    index: u64,
    out_name: *mut *mut c_char,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_name.is_null() {
        set_error("out_name pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let item = match result.items.get(index as usize) {
        Some(d) => d,
        None => {
            set_error("bulk-read index out of bounds");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match std::ffi::CString::new(item.name()) {
        Ok(c) => {
            unsafe { *out_name = c.into_raw() };
            INFRASTORE_OK
        }
        Err(e) => {
            set_error(format!("series name contained an interior NUL: {e}"));
            INFRASTORE_ERR_INTERNAL
        }
    }
}

/// Write the [`time_series_type_to_int`] discriminant of bulk-read item `index` into
/// `out_type` (`0`=SingleTimeSeries, `1`=NonSequentialTimeSeries,
/// `2`=Deterministic, `4`=Probabilistic, `5`=Scenarios — a bulk read never
/// returns the synthesized `DeterministicSingleTimeSeries`). Lets a caller pick
/// the right `infrastore_bulk_result_get_*` before reading.
///
/// # Safety
///
/// `result` must be a live bulk-read handle, `index` less than its length, and
/// `out_type` valid for writing one `i32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_bulk_result_item_type(
    result: *const InfraStoreBulkReadHandle,
    index: u64,
    out_type: *mut i32,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_type.is_null() {
        set_error("out_type pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let item = match result.items.get(index as usize) {
        Some(d) => d,
        None => {
            set_error("bulk-read index out of bounds");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_type = time_series_type_to_int(item.time_series_type()) };
    INFRASTORE_OK
}

/// Read a `NonSequentialTimeSeries` element out of a bulk-read result. The
/// out-params mirror `infrastore_bulk_result_get_single` except there is no
/// `application_data` (a bulk read carries the array data, not the metadata row;
/// fetch it per-key with `infrastore_store_get_metadata` if needed). The caller owns the
/// `out_timestamps`, `out_shape`, and `out_data` buffers.
///
///
/// `out_application_data`, `out_element_type`, `out_units`, `out_quantity_kind`,
/// `out_unit_system`, and `out_component_field` behave as in
/// `infrastore_bulk_result_get_single`: owned C strings (null when unset), any of
/// them nullable to skip, freed with `infrastore_string_free`.
/// # Safety
///
/// `result` must be a live bulk-read handle and `index` less than its length.
/// Every output pointer must be valid for writing its indicated value. The
/// returned buffers must each be released with the matching free function.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_bulk_result_get_non_sequential(
    result: *const InfraStoreBulkReadHandle,
    index: u64,
    out_timestamps: *mut *mut i64,
    out_timestamps_len: *mut u64,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_application_data: *mut *mut c_char,
    out_element_type: *mut *mut c_char,
    out_units: *mut *mut c_char,
    out_quantity_kind: *mut *mut c_char,
    out_unit_system: *mut *mut c_char,
    out_time_reference: *mut *mut c_char,
    out_component_field: *mut *mut c_char,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_timestamps.is_null()
        || out_timestamps_len.is_null()
        || out_dtype.is_null()
        || out_shape.is_null()
        || out_shape_len.is_null()
        || out_data.is_null()
        || out_data_byte_len.is_null()
    {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let series = match result.items.get(index as usize) {
        Some(core_lib::TimeSeriesData::NonSequentialTimeSeries(s)) => s,
        Some(other) => {
            set_error(format!(
                "bulk-read item {index} is a {}, not a NonSequentialTimeSeries",
                other.time_series_type().as_str()
            ));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    // Descriptors first: the only fallible step, and it writes nothing on
    // failure, so no other handed-out buffer can be orphaned by it.
    let code = unsafe {
        emit_descriptors(
            series.application_data.as_deref(),
            series.element_type,
            series.units.as_deref(),
            series.quantity_kind.as_deref(),
            series.unit_system,
            series.time_reference.as_ref(),
            series.component_field.as_deref(),
            out_application_data,
            out_element_type,
            out_units,
            out_quantity_kind,
            out_unit_system,
            out_time_reference,
            out_component_field,
        )
    };
    if code != INFRASTORE_OK {
        return code;
    }
    let timestamps: Vec<i64> = series
        .timestamps
        .iter()
        .map(|t| datetime_to_unix_ms(*t))
        .collect();
    let (timestamps_ptr, timestamps_len) = vec_into_raw(timestamps);
    let shape: Vec<i64> = series.data.shape.iter().map(|&d| d as i64).collect();
    let (shape_ptr, shape_len) = vec_into_raw(shape);
    let dtype = series.data.dtype.code();
    let (data_ptr, data_byte_len) = vec_into_raw(series.data.bytes.clone());
    unsafe {
        *out_timestamps = timestamps_ptr;
        *out_timestamps_len = timestamps_len;
        *out_dtype = dtype;
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_byte_len;
    }
    INFRASTORE_OK
}

/// Read a forecast element (`Deterministic`, `Probabilistic`, or `Scenarios`)
/// out of a read result. The out-params carry the forecast window parameters
/// (`out_scenario_count` is nonzero only for `Scenarios`; `out_percentiles` is
/// non-null only for `Probabilistic`). The caller owns the `out_dims`,
/// `out_data`, and `out_percentiles` buffers.
///
///
/// `out_application_data`, `out_element_type`, `out_units`, `out_quantity_kind`,
/// `out_unit_system`, and `out_component_field` behave as in
/// `infrastore_bulk_result_get_single`: owned C strings (null when unset), any of
/// them nullable to skip, freed with `infrastore_string_free`.
/// # Safety
///
/// `result` must be a live bulk-read handle and `index` less than its length.
/// Every output pointer must be valid for writing its indicated value. The
/// returned buffers must each be released with the matching `infrastore_buffer_free_*`,
/// and the owned strings -- `*out_resolution`, `*out_horizon`, `*out_interval`,
/// and any non-null one of the six optional attributes -- exactly once each with
/// `infrastore_string_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_bulk_result_get_forecast(
    result: *const InfraStoreBulkReadHandle,
    index: u64,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_scenario_count: *mut u64,
    out_ndims: *mut u64,
    out_dims: *mut *mut u64,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_percentiles: *mut *mut f64,
    out_percentiles_len: *mut u64,
    out_application_data: *mut *mut c_char,
    out_element_type: *mut *mut c_char,
    out_units: *mut *mut c_char,
    out_quantity_kind: *mut *mut c_char,
    out_unit_system: *mut *mut c_char,
    out_time_reference: *mut *mut c_char,
    out_component_field: *mut *mut c_char,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_horizon.is_null()
        || out_interval.is_null()
        || out_count.is_null()
        || out_scenario_count.is_null()
        || out_ndims.is_null()
        || out_dims.is_null()
        || out_dtype.is_null()
        || out_data.is_null()
        || out_data_byte_len.is_null()
        || out_percentiles.is_null()
        || out_percentiles_len.is_null()
    {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let data = match result.items.get(index as usize) {
        Some(
            d @ (core_lib::TimeSeriesData::Deterministic(_)
            | core_lib::TimeSeriesData::Probabilistic(_)
            | core_lib::TimeSeriesData::Scenarios(_)),
        ) => d.clone(),
        Some(other) => {
            set_error(format!(
                "bulk-read item {index} is a {}, not a forecast",
                other.time_series_type().as_str()
            ));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    // Descriptors first: the only fallible step left (the item was verified to
    // be a forecast variant above, and `emit_forecast_data` is infallible for
    // those), and it writes nothing on failure, so no handed-out buffer can be
    // orphaned by it.
    let code = unsafe {
        emit_descriptors(
            data.application_data(),
            data.element_type(),
            data.units(),
            data.quantity_kind(),
            data.unit_system(),
            data.time_reference(),
            data.component_field(),
            out_application_data,
            out_element_type,
            out_units,
            out_quantity_kind,
            out_unit_system,
            out_time_reference,
            out_component_field,
        )
    };
    if code != INFRASTORE_OK {
        return code;
    }
    unsafe {
        emit_forecast_data(
            data,
            out_initial_ts_unix_ms,
            out_resolution,
            out_horizon,
            out_interval,
            out_count,
            out_scenario_count,
            out_ndims,
            out_dims,
            out_dtype,
            out_data,
            out_data_byte_len,
            out_percentiles,
            out_percentiles_len,
        )
    }
}

/// Free a read result handle created by `infrastore_store_read_by_ids` or
/// `infrastore_store_bulk_read`.
///
/// # Safety
///
/// `result` must be null or a handle returned by a bulk-read function
/// that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_bulk_result_free(result: *mut InfraStoreBulkReadHandle) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

/// Buffer size a caller must provide for
/// `infrastore_store_transform_single_time_series`'s `out_interval`. An
/// ISO-8601 period is far shorter than this; the slack is deliberate.
pub const INTERVAL_BUF_LEN: u64 = 64;

/// Derive `DeterministicSingleTimeSeries` forecasts from the stored
/// `SingleTimeSeries` associations (see `Store::transform_single_time_series`,
/// which performs the whole of the eligibility validation).
///
/// Writes the number of series transformed to `*out_count`. The remaining
/// out-parameters report the rest of the `TransformOutcome` and may each be
/// null if the caller does not need them:
///
/// - `out_sources` — `SingleTimeSeries` matched before idempotent skips; zero
///   means there was nothing to transform.
/// - `out_interval` — the ISO-8601 interval actually stored, NUL-terminated.
///   Unlike the listing exports this takes no probe pass: an ISO period is
///   bounded, so the caller passes a fixed buffer of `INTERVAL_BUF_LEN` bytes.
/// - `out_interval_normalized` — non-zero when the request described a single
///   window (see `normalize_single_window`).
/// - `out_ids` — the catalog id of each view written, in the order they were
///   written; free with `infrastore_buffer_free_i64(ptr, *out_count)`. Set to
///   null when nothing was written — a dry run or an idempotent re-run — and a
///   null pointer must never be indexed, whatever `*out_count` says: on a dry
///   run `*out_count` is the count a committing run *would* produce, and no
///   ids exist for it yet. Check the pointer, not the count.
///
/// The two policy flags are `TransformPolicy` (see the core docs); both false
/// is the permissive default, and InfrastructureSystems.jl passes both true:
///
/// - `normalize_single_window` selects the single-window encoding: non-zero
///   stores the zero interval (what InfrastructureSystems.jl looks up by), zero
///   stores the requested interval verbatim.
/// - `require_uniform_forecast_grid` requires every resolution in scope, and
///   any forecast already stored at the same `(resolution, interval)`, to agree
///   on the derived window `count` and `initial_timestamp`.
/// - `dry_run` runs every check and reports what would happen without writing.
///   `*out_count` is then the count a committing run would produce. Legal
///   against a read-only store.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `out_count` must be valid
/// for writing one `u64`. Each non-null optional out-parameter must be valid
/// for writing its type.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_transform_single_time_series(
    handle: *mut InfraStoreHandle,
    horizon: *const c_char,
    interval: *const c_char,
    owner_category: i32,
    resolution: *const c_char,
    normalize_single_window: bool,
    require_uniform_forecast_grid: bool,
    dry_run: bool,
    out_count: *mut u64,
    out_sources: *mut u64,
    out_interval: *mut c_char,
    out_interval_normalized: *mut bool,
    out_ids: *mut *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_count.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    // `owner_category < 0` means "all categories"; an empty `resolution` means
    // "all resolutions".
    let category = match owner_category {
        c if c < 0 => None,
        0 => Some(core_lib::OwnerCategory::Component),
        1 => Some(core_lib::OwnerCategory::SupplementalAttribute),
        other => {
            set_error(format!("invalid owner_category {other}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let resolution = match unsafe { cstr_to_optional_period(resolution) } {
        Ok(r) => r,
        Err(c) => return c,
    };
    let horizon = match unsafe { cstr_to_period(horizon) } {
        Ok(h) => h,
        Err(c) => return c,
    };
    let interval = match unsafe { cstr_to_period(interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    let policy = core_lib::TransformPolicy {
        dry_run,
        normalize_single_window,
        require_uniform_forecast_grid,
    };
    match store
        .inner
        .transform_single_time_series(horizon, interval, category, resolution, policy)
    {
        Ok(outcome) => {
            unsafe {
                *out_count = outcome.transformed as u64;
                if !out_sources.is_null() {
                    *out_sources = outcome.sources as u64;
                }
                if !out_interval_normalized.is_null() {
                    *out_interval_normalized = outcome.interval_normalized;
                }
                if !out_interval.is_null() {
                    let mut written = 0u64;
                    write_str_out(
                        &outcome.interval.to_iso8601(),
                        out_interval,
                        INTERVAL_BUF_LEN,
                        &raw mut written,
                    );
                }
                if !out_ids.is_null() {
                    // `*out_count` elements, in the order they were written,
                    // or null when nothing was. A dry run is the case to watch:
                    // it reports the *planned* count in `*out_count` while
                    // writing nothing, so the pointer, not the count, is the
                    // caller's signal (documented above).
                    let ids: Vec<i64> = outcome.written.iter().map(|i| i.get()).collect();
                    *out_ids = if ids.is_empty() {
                        ptr::null_mut()
                    } else {
                        vec_into_raw(ids).0
                    };
                }
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Shared emitter: write a forecast `TimeSeriesData` value into the C out-params
/// used by [`infrastore_bulk_result_get_forecast`].
///
/// # Safety
///
/// All out pointers must be non-null and valid for writing their indicated
/// values (the callers null-check them). The returned `out_dims`, `out_data`,
/// and (for `Probabilistic`) `out_percentiles` buffers are heap-allocated and
/// must be released by the caller with the matching `infrastore_buffer_free_*` function.
#[allow(clippy::too_many_arguments)]
unsafe fn emit_forecast_data(
    data: core_lib::TimeSeriesData,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_scenario_count: *mut u64,
    out_ndims: *mut u64,
    out_dims: *mut *mut u64,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_percentiles: *mut *mut f64,
    out_percentiles_len: *mut u64,
) -> i32 {
    // Helper: convert Duration to milliseconds.

    match data {
        core_lib::TimeSeriesData::Deterministic(det) => {
            let dims: Vec<u64> = det.data.shape.iter().map(|&d| d as u64).collect();
            let (dims_ptr, ndims) = vec_into_raw(dims);

            let dtype = det.data.dtype.code();
            let (data_ptr, byte_len) = vec_into_raw(det.data.bytes);

            unsafe {
                *out_initial_ts_unix_ms = datetime_to_unix_ms(det.initial_timestamp);
                *out_resolution = period_cstr(det.resolution);
                *out_horizon = period_cstr(det.horizon);
                *out_interval = period_cstr(det.interval);
                *out_count = det.count as u64;
                *out_scenario_count = 0;
                *out_ndims = ndims;
                *out_dims = dims_ptr;
                *out_dtype = dtype;
                *out_data = data_ptr;
                *out_data_byte_len = byte_len;
                *out_percentiles = std::ptr::null_mut();
                *out_percentiles_len = 0;
            }
            INFRASTORE_OK
        }
        core_lib::TimeSeriesData::Probabilistic(prob) => {
            let dims: Vec<u64> = prob.data.shape.iter().map(|&d| d as u64).collect();
            let (dims_ptr, ndims) = vec_into_raw(dims);

            let dtype = prob.data.dtype.code();
            let (data_ptr, byte_len) = vec_into_raw(prob.data.bytes);

            let (pct_ptr, pct_len) = vec_into_raw(prob.percentiles);

            unsafe {
                *out_initial_ts_unix_ms = datetime_to_unix_ms(prob.initial_timestamp);
                *out_resolution = period_cstr(prob.resolution);
                *out_horizon = period_cstr(prob.horizon);
                *out_interval = period_cstr(prob.interval);
                *out_count = prob.count as u64;
                *out_scenario_count = 0;
                *out_ndims = ndims;
                *out_dims = dims_ptr;
                *out_dtype = dtype;
                *out_data = data_ptr;
                *out_data_byte_len = byte_len;
                *out_percentiles = pct_ptr;
                *out_percentiles_len = pct_len;
            }
            INFRASTORE_OK
        }
        core_lib::TimeSeriesData::Scenarios(scen) => {
            let scenario_count = scen.scenario_count;

            let dims: Vec<u64> = scen.data.shape.iter().map(|&d| d as u64).collect();
            let (dims_ptr, ndims) = vec_into_raw(dims);

            let dtype = scen.data.dtype.code();
            let (data_ptr, byte_len) = vec_into_raw(scen.data.bytes);

            unsafe {
                *out_initial_ts_unix_ms = datetime_to_unix_ms(scen.initial_timestamp);
                *out_resolution = period_cstr(scen.resolution);
                *out_horizon = period_cstr(scen.horizon);
                *out_interval = period_cstr(scen.interval);
                *out_count = scen.count as u64;
                *out_scenario_count = scenario_count as u64;
                *out_ndims = ndims;
                *out_dims = dims_ptr;
                *out_dtype = dtype;
                *out_data = data_ptr;
                *out_data_byte_len = byte_len;
                *out_percentiles = std::ptr::null_mut();
                *out_percentiles_len = 0;
            }
            INFRASTORE_OK
        }
        other => {
            set_error(format!(
                "key identifies a {} time series; use the matching read function",
                other.time_series_type().as_str()
            ));
            INFRASTORE_ERR_INVALID_PARAMETER
        }
    }
}

/// Lowercase hex of a 32-byte content hash (64 chars).
fn hash_to_hex(hash: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in hash {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Full-metadata JSON object for one association row: the identity/descriptive
/// key fields plus the storage columns a key row omits (`data_hash` hex,
/// `element_type`, `element_shape`, `percentiles`, `units`, `application_data`).
/// Periods are ISO-8601 strings; `initial_timestamp_ms` is Unix milliseconds.
/// One `metadata_to_map` object per row, as a JSON array — what every listing
/// serves now that a listing is rows rather than keys.
fn metadata_rows_to_json(rows: &[core_lib::TimeSeriesMetadata]) -> String {
    Value::Array(
        rows.iter()
            .map(|m| Value::Object(metadata_to_map(m)))
            .collect(),
    )
    .to_string()
}

fn metadata_to_map(m: &core_lib::TimeSeriesMetadata) -> serde_json::Map<String, Value> {
    let iso = |p: Option<core_lib::Period>| -> Value {
        p.map(|x| Value::from(x.to_iso8601()))
            .unwrap_or(Value::Null)
    };
    let mut o = serde_json::Map::new();
    o.insert("owner_id".into(), Value::from(m.owner_id));
    o.insert("owner_type".into(), Value::from(m.owner_type.clone()));
    o.insert(
        "owner_category".into(),
        Value::from(m.owner_category.as_str()),
    );
    o.insert(
        "time_series_type".into(),
        Value::from(m.time_series_type.as_str()),
    );
    o.insert("name".into(), Value::from(m.name.clone()));
    o.insert("data_hash".into(), Value::from(hash_to_hex(&m.data_hash)));
    // Always present on a row read out of the catalog; `null` only if a caller
    // built the metadata itself without one.
    o.insert(
        "id".into(),
        m.id.map(|i| Value::from(i.get())).unwrap_or(Value::Null),
    );
    o.insert(
        "initial_timestamp_ms".into(),
        m.initial_timestamp
            .map(datetime_to_unix_ms)
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    o.insert("resolution".into(), iso(m.resolution));
    o.insert("horizon".into(), iso(m.horizon));
    o.insert("interval".into(), iso(m.interval));
    o.insert(
        "count".into(),
        m.count
            .map(|c| Value::from(c as u64))
            .unwrap_or(Value::Null),
    );
    o.insert(
        "length".into(),
        m.length
            .map(|l| Value::from(l as u64))
            .unwrap_or(Value::Null),
    );
    o.insert(
        "percentiles".into(),
        match &m.percentiles {
            Some(p) => Value::Array(p.iter().map(|&x| Value::from(x)).collect()),
            None => Value::Null,
        },
    );
    o.insert(
        "element_type".into(),
        Value::from(m.element_type.to_string()),
    );
    o.insert(
        "element_shape".into(),
        Value::Array(
            m.element_shape
                .iter()
                .map(|&d| Value::from(d as u64))
                .collect(),
        ),
    );
    o.insert(
        "features".into(),
        serde_json::from_str(&features_to_json(&m.features))
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
    );
    o.insert(
        "units".into(),
        m.units.clone().map(Value::from).unwrap_or(Value::Null),
    );
    o.insert(
        "quantity_kind".into(),
        m.quantity_kind
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    o.insert(
        "unit_system".into(),
        m.unit_system
            .map(|u| Value::from(u.as_str()))
            .unwrap_or(Value::Null),
    );
    o.insert(
        "time_reference".into(),
        m.time_reference
            .as_ref()
            .map(|r| Value::from(r.as_storage_string()))
            .unwrap_or(Value::Null),
    );
    o.insert(
        "component_field".into(),
        m.component_field
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    o.insert(
        "application_data".into(),
        m.application_data
            .clone()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    o
}

/// List the catalog metadata rows named by `ids`, in the order the ids are
/// given, as a JSON array string (the per-row shape of
/// `infrastore_store_list_metadata`).
///
/// `infrastore_store_list_metadata` addressed by id instead of by attributes —
/// the bulk companion to `infrastore_store_get_metadata_by_id`, and what a
/// consumer hydrating a model full of recorded ids wants: one catalog query for
/// the whole set rather than one call per reference.
///
/// `INFRASTORE_ERR_NOT_FOUND` if any id names no row: a caller naming ids is
/// asserting they exist, and a silently short array would let a stale reference
/// pass as an absent match. Sift the set with
/// `infrastore_store_association_exists` first when some are expected to have
/// gone. Repeats are returned once each, in place.
///
/// Returns the JSON through `out_json` as an **owned** allocation the caller
/// releases with `infrastore_string_free`; `out_len` is its byte length.
///
/// # Safety
///
/// `ids` must point to `n` readable `i64`s (it may be null only when `n` is 0).
/// `out_json` must be valid for writing one pointer and `out_len` for writing
/// one `u64`; on success `*out_json` must be released exactly once with
/// `infrastore_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_metadata_by_ids(
    handle: *const InfraStoreHandle,
    ids: *const i64,
    n: u64,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let count = n as usize;
    if count != 0 && ids.is_null() {
        set_error("ids pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let raw = if count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(ids, count) }
    };
    let id_slice: Vec<core_lib::TimeSeriesId> =
        raw.iter().copied().map(core_lib::TimeSeriesId).collect();
    let rows = match store.inner.list_metadata_by_ids(&id_slice) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let json = metadata_rows_to_json(&rows);
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// List catalog metadata rows as a JSON array string (see `metadata_to_map` for
/// the per-row shape). Every filter is optional and independent; with none set
/// the whole store is listed. A `has_*` flag of `false` (or a null string pointer)
/// disables that filter:
/// - `owner_id` / `owner_category` (`0` = Component, `1` = SupplementalAttribute)
/// - `time_series_type` (the `INFRASTORE_TYPE_*` code)
/// - `name` (null = no name filter)
/// - `name_glob` (null = no glob filter; a SQLite `GLOB` pattern over the name,
///   e.g. `wind_*`. Case-sensitive, and composes with `name` rather than
///   replacing it — set both and a row must satisfy both.)
/// - `resolution` (empty/null = no resolution filter)
/// - `interval` (empty/null = no interval filter; forecasts only — static rows
///   have no interval and never match an interval filter)
/// - `features_json` (a JSON object; null or empty = no feature filter; matches as
///   a subset, i.e. a row whose features include all the given ones)
/// - `component_field` (null = no filter; exact, case-sensitive match on the
///   owning component's field. A row that declares none matches no value, so
///   this cannot select the rows that left it unset.)
///
/// Each row carries `id`, the association id the catalog filed it under (the
/// same id `infrastore_store_add_batch` returns) — the address every read,
/// removal and rename takes. A row carries no timestamp vector: an irregular
/// series' time axis is the one part of a row that costs a read per row, so a
/// listing omits it and a caller that needs it reads the series.
///
/// Returns the JSON through `out_json` as an **owned** allocation the caller
/// releases with `infrastore_string_free`; `out_len` is its byte length. A
/// listing's size scales with the catalog, so this deliberately does not use the
/// probe-then-fetch convention the fixed-size outputs use — that would run the
/// query and serialize the rows twice.
///
/// # Safety
///
/// The scalar filter flags/values are plain scalars. `name`, `name_glob`, `component_field`;
/// `features_json` must each be null or a null-terminated UTF-8 string. `out_json` must be
/// valid for writing one pointer and `out_len` for writing one `u64`; on success `*out_json`
/// must be released exactly once with `infrastore_string_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_list_metadata(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            name_glob,
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let rows = match store.inner.list_metadata(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let json = metadata_rows_to_json(&rows);
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// List the distinct series names matching the filter as a JSON array of strings
/// (sorted). Filters and the owned-string return match
/// `infrastore_store_list_metadata`.
///
/// # Safety
///
/// Identical to `infrastore_store_list_metadata`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_list_names(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            name_glob,
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let names = match store.inner.list_names(filter) {
        Ok(n) => n,
        Err(e) => return map_core_error(e),
    };
    let json = Value::Array(names.into_iter().map(Value::from).collect()).to_string();
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// List the distinct owner types matching the filter as a JSON array of strings
/// (sorted). Filters and the owned-string return match
/// `infrastore_store_list_metadata`.
///
/// # Safety
///
/// Identical to `infrastore_store_list_metadata`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_list_owner_types(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            name_glob,
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let types = match store.inner.list_owner_types(filter) {
        Ok(t) => t,
        Err(e) => return map_core_error(e),
    };
    let json = Value::Array(types.into_iter().map(Value::from).collect()).to_string();
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// Remove every time series matching the filter in one all-or-nothing
/// transaction, writing the number removed into `*out_removed`. Filters match
/// `infrastore_store_list_metadata`; an empty match removes nothing (`0`).
///
/// # Safety
///
/// The filter args match `infrastore_store_list_metadata`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_remove_by_filter(
    handle: *mut InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_removed.is_null() {
        set_error("out_removed is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            name_glob,
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.remove_by_filter(filter) {
        Ok(n) => {
            unsafe { *out_removed = n as u64 };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Rename the association filed under `id` to `new_name`. Only the catalog name
/// changes; the array is untouched, and the id is the same afterwards — a rename
/// moves the name, not the reference. `INFRASTORE_ERR_NOT_FOUND` if the id names
/// no row, or a duplicate error if the new identity already exists.
///
/// # Safety
///
/// `handle` must be a live read-write store handle. `new_name` must be
/// null-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_rename(
    handle: *mut InfraStoreHandle,
    id: i64,
    new_name: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let new_name = match unsafe { cstr_to_str(new_name) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    match store
        .inner
        .rename_time_series(core_lib::TimeSeriesId(id), new_name)
    {
        Ok(_) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Build a [`core_lib::ListFilter`] from the optional scalar/string filter args
/// shared by `infrastore_store_list_metadata` and `infrastore_store_list_array_groups`. On a bad
/// argument it sets the thread-local error (where appropriate) and returns the
/// error code to propagate.
///
/// # Safety
///
/// `name`, `name_glob`, `component_field`, and `features_json` must each be null
/// or a null-terminated UTF-8 string; `resolution` and `interval` must each be
/// null or a null-terminated ISO-8601 period.
#[allow(clippy::too_many_arguments)]
unsafe fn build_list_filter(
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
) -> std::result::Result<core_lib::ListFilter, i32> {
    let mut filter = core_lib::ListFilter::new();
    if has_owner {
        filter = filter.owner_id(owner_id);
    }
    if has_owner_category {
        let category = match owner_category {
            0 => core_lib::OwnerCategory::Component,
            1 => core_lib::OwnerCategory::SupplementalAttribute,
            other => {
                set_error(format!("invalid owner_category {other}"));
                return Err(INFRASTORE_ERR_INVALID_PARAMETER);
            }
        };
        filter = filter.owner_category(category);
    }
    if has_time_series_type {
        match resolve_requested_type_from_int(time_series_type) {
            Some(t) => filter = filter.time_series_type(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return Err(INFRASTORE_ERR_INVALID_PARAMETER);
            }
        }
    }
    match unsafe { cstr_to_optional_string(name) } {
        Ok(Some(n)) => filter = filter.name(n),
        Ok(None) => {}
        Err(c) => {
            set_error("name is not valid UTF-8");
            return Err(c);
        }
    }
    match unsafe { cstr_to_optional_string(name_glob) } {
        Ok(Some(g)) => filter = filter.name_glob(g),
        Ok(None) => {}
        Err(c) => {
            set_error("name_glob is not valid UTF-8");
            return Err(c);
        }
    }
    match unsafe { cstr_to_optional_period(resolution) } {
        Ok(Some(p)) => filter = filter.resolution(p),
        Ok(None) => {}
        Err(c) => return Err(c),
    }
    match unsafe { cstr_to_optional_period(interval) } {
        Ok(Some(p)) => filter = filter.interval(p),
        Ok(None) => {}
        Err(c) => return Err(c),
    }
    match unsafe { cstr_to_optional_string(component_field) } {
        Ok(Some(f)) => filter = filter.component_field(f),
        Ok(None) => {}
        Err(c) => {
            set_error("component_field is not valid UTF-8");
            return Err(c);
        }
    }
    if let Some(zoneless) = zoneless_filter(zoneless)? {
        filter = filter.zoneless(zoneless);
    }
    let features = unsafe { parse_features_json(features_json) }?;
    if !features.is_empty() {
        filter = filter.features(features);
    }
    Ok(filter)
}

/// Serialize a key's `Features` map to a JSON object string of plain scalar
/// values (the same shape `parse_features_json` accepts), so it round-trips back
/// through the attribute-addressed entry points. An empty map serializes to
/// `"{}"`.
fn features_to_json(features: &core_lib::Features) -> String {
    let mut map = serde_json::Map::with_capacity(features.len());
    for (k, v) in features {
        let jv = match v {
            core_lib::FeatureValue::Int(i) => Value::from(*i),
            core_lib::FeatureValue::Float(f) => Value::from(*f),
            core_lib::FeatureValue::Bool(b) => Value::from(*b),
            core_lib::FeatureValue::Str(s) => Value::from(s.clone()),
        };
        map.insert(k.clone(), jv);
    }
    Value::Object(map).to_string()
}

/// Release a `u64` dims buffer returned by `infrastore_bulk_result_get_forecast`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_buffer_free_u64(ptr: *mut u64, len: u64) {
    unsafe { free_raw_buffer(ptr, len) };
}

/// True iff a time series of `ts_type` with the given attributes exists.
///
/// # Safety
///
/// `features_json` may be null.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_has_typed(
    handle: *const InfraStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            ts_type,
            resolution,
            interval,
            features_json,
        )
    } {
        Ok(filter) => filter,
        Err(c) => return c,
    };
    match store.inner.has_any_time_series(filter) {
        Ok(b) => {
            unsafe { *out_present = b };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Copy the association filed under `src_id` onto another owner, optionally
/// under a new name, and report the id of the new row through `out_id`.
///
/// Arrays are content-addressed, so only a new association row is written — no
/// array data is duplicated and the stored time series type is preserved (a
/// `DeterministicSingleTimeSeries` stays one rather than being materialized into
/// a dense `Deterministic`). The copy keeps the source's owner category.
///
/// A copy is its own row with its own id; `src_id` is untouched and still
/// resolves afterwards. The new id is reported rather than left for the caller
/// to find, because a listing is the only other way to recover it and the caller
/// usually wants to reference the copy straight away.
///
/// # Safety
///
/// `src_id` names the SOURCE association. `dst_owner_type` must be a
/// null-terminated UTF-8 string; `new_name` may be null (which keeps the source
/// name). `out_id` may be null to discard the new id, and is otherwise valid for
/// writing one `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_copy_time_series(
    handle: *mut InfraStoreHandle,
    src_id: i64,
    dst_owner_id: i64,
    dst_owner_type: *const c_char,
    new_name: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let dst_type = match unsafe { cstr_to_str(dst_owner_type) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let renamed = if new_name.is_null() {
        None
    } else {
        match unsafe { cstr_to_str(new_name) } {
            Ok(s) => Some(s),
            Err(c) => return c,
        }
    };
    match store.inner.copy_time_series(
        core_lib::TimeSeriesId(src_id),
        dst_owner_id,
        dst_type,
        renamed,
    ) {
        Ok(id) => {
            if !out_id.is_null() {
                unsafe { *out_id = id.get() };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Remove all time series, or all for a single owner when `has_owner` is true.
/// Returns `INFRASTORE_OK` on success.
///
/// # Safety
///
/// `has_owner`, `owner_id`, and `owner_category` are plain scalars; when `has_owner` is true
/// `owner_category` (`0` = Component, `1` = SupplementalAttribute) scopes the clear to one
/// owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_clear(
    handle: *mut InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    owner_category: i32,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let owner = if has_owner {
        let category = match owner_category {
            0 => core_lib::OwnerCategory::Component,
            1 => core_lib::OwnerCategory::SupplementalAttribute,
            other => {
                set_error(format!("invalid owner_category {other}"));
                return INFRASTORE_ERR_INVALID_PARAMETER;
            }
        };
        Some((owner_id, category))
    } else {
        None
    };
    match store.inner.clear_time_series(owner) {
        Ok(_) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Reassign every time series owned by `old_owner_id` to `new_owner_id`.
/// When `out_updated` is non-null it receives the number of associations
/// changed. Returns `INFRASTORE_OK` on success.
///
/// # Safety
///
/// `old_owner_id` and `new_owner_id` are plain integers; `owner_category` (`0` = Component, `1`
/// = SupplementalAttribute) identifies the owner category.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_replace_owner(
    handle: *mut InfraStoreHandle,
    old_owner_id: i64,
    new_owner_id: i64,
    owner_category: i32,
    out_updated: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store
        .inner
        .replace_owner(old_owner_id, new_owner_id, category)
    {
        Ok(updated) => {
            if !out_updated.is_null() {
                unsafe { *out_updated = updated as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- Association catalogs -------------------------------------------------
//
// Two catalogs, kept apart on purpose: `supplemental_attribute_*` records which
// attributes are attached to which components; `parent_child_*` records directed
// edges between components (a generator connected to a bus, say). Neither has
// anything to do with time series.
//
// Predicates cross the boundary as one JSON object rather than positional
// arguments, the same way `features_json` already does: a filter has four
// optional fields, two of which are string lists, and spreading that over eight
// arguments is unreadable from the caller side. Result sets come back through
// the probe-then-fetch JSON convention. These association listings are bounded
// by one owner's edges rather than by the whole catalog, so the convention's
// double execution is not the cost here that it is for the time-series listings
// (`infrastore_store_list_metadata` and friends), which return owned strings.

/// Parse a filter from a JSON object, or the default (match-everything) filter
/// from a null or empty string. Unknown fields are rejected so a typo in a
/// binding surfaces as an error instead of silently widening the query.
unsafe fn assoc_filter_from_json<T: serde::de::DeserializeOwned + Default>(
    p: *const c_char,
) -> Result<T, i32> {
    let s = unsafe { cstr_to_optional_string(p)? };
    match s {
        None => Ok(T::default()),
        Some(s) if s.trim().is_empty() => Ok(T::default()),
        Some(s) => serde_json::from_str(&s).map_err(|e| {
            set_error(format!("invalid association filter JSON: {e}"));
            INFRASTORE_ERR_INVALID_PARAMETER
        }),
    }
}

/// Parse a JSON array of association rows for the bulk-add exports.
unsafe fn assoc_rows_from_json<T: serde::de::DeserializeOwned>(
    p: *const c_char,
) -> Result<Vec<T>, i32> {
    let json = unsafe { cstr_to_str(p)? };
    serde_json::from_str(json).map_err(|e| {
        set_error(format!("invalid associations JSON: {e}"));
        INFRASTORE_ERR_INVALID_PARAMETER
    })
}

/// Hand a bulk write's assigned ids back through its optional out-parameters.
///
/// The same shape `infrastore_store_add_batch` uses: `out_added` takes the
/// count, `out_ids` an owned buffer of exactly that many ids in input order,
/// released with `infrastore_buffer_free_i64`. An empty batch writes null,
/// so there is nothing to free.
///
/// # Safety
///
/// Each of `out_added` and `out_ids` must be null or valid for writing one
/// value of its type.
unsafe fn write_assigned_ids(ids: Vec<i64>, out_added: *mut u64, out_ids: *mut *mut i64) {
    let len = ids.len() as u64;
    if !out_added.is_null() {
        unsafe { *out_added = len };
    }
    if !out_ids.is_null() {
        let ptr = if ids.is_empty() {
            ptr::null_mut()
        } else {
            vec_into_raw(ids).0
        };
        unsafe { *out_ids = ptr };
    }
}

/// Serialize a result set and write it into the caller's buffer.
unsafe fn write_json_out<T: serde::Serialize>(
    value: &T,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    match serde_json::to_string(value) {
        Ok(json) => {
            unsafe { write_str_out(&json, buf, cap, out_len) };
            INFRASTORE_OK
        }
        Err(e) => {
            set_error(e.to_string());
            INFRASTORE_ERR_INTERNAL
        }
    }
}

// ---- Supplemental-attribute associations ----------------------------------

/// Attach supplemental attribute `(attribute_id, attribute_type)` to component
/// `(component_id, component_type)`. Returns `INFRASTORE_ERR_DUPLICATE_ASSOCIATION` if
/// that component already carries that attribute, whatever type names are
/// supplied.
///
/// # Safety
///
/// `component_type` and `attribute_type` must point to valid, null-terminated UTF-8 strings
/// that stay valid for the call.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_add_supplemental_attribute_association(
    handle: *mut InfraStoreHandle,
    component_id: i64,
    component_type: *const c_char,
    attribute_id: i64,
    attribute_type: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let component_type = match unsafe { cstr_to_str(component_type) } {
        Ok(s) => s.to_string(),
        Err(c) => return c,
    };
    let attribute_type = match unsafe { cstr_to_str(attribute_type) } {
        Ok(s) => s.to_string(),
        Err(c) => return c,
    };
    match store.inner.add_supplemental_attribute_association(
        core_lib::SupplementalAttributeAssociation {
            component_id,
            component_type,
            attribute_id,
            attribute_type,
            // The catalog assigns; this table's wire form carries no id.
            id: None,
        },
    ) {
        Ok(id) => {
            if !out_id.is_null() {
                unsafe { *out_id = id };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attach many in one all-or-nothing transaction, from a JSON array of objects
/// with `component_id`, `component_type`, `attribute_id`, and `attribute_type`.
/// This is the import half of the bulk round trip whose export is
/// `infrastore_store_list_supplemental_attribute_associations` with a null filter.
///
/// `out_added` receives the number inserted and `out_ids` the catalog id of
/// each, in input order — the ids are the durable handles this write creates,
/// so returning only a count would leave a caller re-listing the table to find
/// what it just wrote. Either may be null to skip it.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `associations_json` a valid, null-
/// terminated UTF-8 string. `out_added`, when non-null, must be valid for writing one
/// `uint64_t`. `out_ids`, when non-null, must be valid for writing one pointer; on
/// `INFRASTORE_OK` it receives an array of `*out_added` ids that the caller owns and must
/// release with `infrastore_buffer_free_i64(*out_ids, *out_added)`. An empty batch writes
/// null there, which needs no release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_add_supplemental_attribute_associations(
    handle: *mut InfraStoreHandle,
    associations_json: *const c_char,
    out_added: *mut u64,
    out_ids: *mut *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let assocs: Vec<core_lib::SupplementalAttributeAssociation> =
        match unsafe { assoc_rows_from_json(associations_json) } {
            Ok(v) => v,
            Err(c) => return c,
        };
    match store.inner.add_supplemental_attribute_associations(assocs) {
        Ok(ids) => {
            unsafe { write_assigned_ids(ids, out_added, out_ids) };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Whether any attachment matches `filter_json`. Recognized filter fields, all
/// optional: `component_id`, `component_types`, `attribute_id`,
/// `attribute_types`. A null or empty string matches everything; an empty type
/// list matches nothing.
///
/// # Safety
///
/// `handle` must be a live store handle, `filter_json` null or valid
/// null-terminated UTF-8, and `out_found` valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_has_supplemental_attribute_association(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    out_found: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_found.is_null() {
        set_error("out_found is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store.inner.has_supplemental_attribute_association(&filter) {
        Ok(found) => {
            unsafe { *out_found = found };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attachments matching `filter_json` as a JSON array, in insertion order. Each
/// object carries `component_id`, `component_type`, `attribute_id`, and
/// `attribute_type`. Returns the JSON through `out_json` as an **owned**
/// allocation the caller releases with `infrastore_string_free`; `out_len` is
/// its byte length.
///
/// This listing is catalog-scaled (a no-filter call exports the whole table),
/// so — unlike the other `list_supplemental_attribute_*` exports in this
/// section, which stay probe-then-fetch because they are bounded by one
/// owner's edges — it follows the owned-string convention `infrastore_store_list_metadata`
/// and friends use, to avoid running the query and serializing every row twice.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_json` must be valid for writing one pointer and
/// `out_len` for writing one `u64`; on success `*out_json` must be released
/// exactly once with `infrastore_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_supplemental_attribute_associations(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    let rows = match store
        .inner
        .list_supplemental_attribute_associations(&filter)
    {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let json = match serde_json::to_string(&rows) {
        Ok(j) => j,
        Err(e) => {
            set_error(e.to_string());
            return INFRASTORE_ERR_INTERNAL;
        }
    };
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// Distinct attribute ids matching `filter_json`, ascending, as a JSON array —
/// the attributes attached to a component when `component_id` is set.
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid null-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_supplemental_attribute_ids(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store.inner.list_supplemental_attribute_ids(&filter) {
        Ok(ids) => unsafe { write_json_out(&ids, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

/// Distinct component ids matching `filter_json`, ascending, as a JSON array —
/// the components carrying an attribute when `attribute_id` is set.
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid null-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_components_with_attributes(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store.inner.list_components_with_attributes(&filter) {
        Ok(ids) => unsafe { write_json_out(&ids, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

/// Remove every attachment matching `filter_json`. When non-null, `out_removed`
/// receives the number removed; removing nothing is success, not an error.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `filter_json` null or valid null-
/// terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_remove_supplemental_attribute_associations(
    handle: *mut InfraStoreHandle,
    filter_json: *const c_char,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store
        .inner
        .remove_supplemental_attribute_associations(&filter)
    {
        Ok(n) => {
            if !out_removed.is_null() {
                unsafe { *out_removed = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Move every attachment from component `old_id` to `new_id`. When non-null,
/// `out_updated` receives the rows changed. Returns
/// `INFRASTORE_ERR_DUPLICATE_ASSOCIATION` if `new_id` already carries one of the
/// attributes being moved.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_replace_supplemental_attribute_component_id(
    handle: *mut InfraStoreHandle,
    old_id: i64,
    new_id: i64,
    out_updated: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store
        .inner
        .replace_supplemental_attribute_component_id(old_id, new_id)
    {
        Ok(n) => {
            if !out_updated.is_null() {
                unsafe { *out_updated = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attachment counts through `out_count`. `kind` selects what is counted: `0` =
/// rows matching the filter, `1` = distinct attributes among them, `2` =
/// distinct components among them.
///
/// # Safety
///
/// `handle` must be a live store handle, `filter_json` null or valid
/// null-terminated UTF-8, and `out_count` valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_count_supplemental_attribute_associations(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    kind: i32,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    let counted = match kind {
        0 => store
            .inner
            .count_supplemental_attribute_associations(&filter),
        1 => store.inner.count_supplemental_attributes(&filter),
        2 => store.inner.count_components_with_attributes(&filter),
        other => {
            set_error(format!("invalid count kind {other}, expected 0, 1, or 2"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match counted {
        Ok(n) => {
            unsafe { *out_count = n };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attachment counts grouped by attribute type as a JSON array of
/// `{"type": …, "count": …}` objects, ordered by type. Probe-then-fetch.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_supplemental_attribute_counts_by_type(
    handle: *const InfraStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let counts = match store.inner.supplemental_attribute_counts_by_type() {
        Ok(c) => c,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = counts
        .into_iter()
        .map(|(ty, count)| {
            let mut o = serde_json::Map::new();
            o.insert("type".into(), Value::from(ty));
            o.insert("count".into(), Value::from(count));
            Value::Object(o)
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Attachment counts grouped by both type names as a JSON array of
/// `{"component_type": …, "attribute_type": …, "count": …}` objects, ordered by
/// attribute type then component type. Probe-then-fetch.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_supplemental_attribute_summary(
    handle: *const InfraStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    match store.inner.supplemental_attribute_summary() {
        Ok(rows) => unsafe { write_json_out(&rows, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

// ---- Parent/child associations --------------------------------------------

/// Record a directed edge from component `(parent_id, parent_type)` to component
/// `(child_id, child_type)`. Returns `INFRASTORE_ERR_DUPLICATE_ASSOCIATION` if that
/// ordered pair is already related; the reversed pair is a different edge.
///
/// # Safety
///
/// `parent_type` and `child_type` must point to valid, null-terminated UTF-8 strings that stay
/// valid for the call.
/// `out_id`, when non-null, must be valid for writing one `i64`, and receives the catalog
/// id the row was filed under.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_add_parent_child_association(
    handle: *mut InfraStoreHandle,
    parent_id: i64,
    parent_type: *const c_char,
    child_id: i64,
    child_type: *const c_char,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let parent_type = match unsafe { cstr_to_str(parent_type) } {
        Ok(s) => s.to_string(),
        Err(c) => return c,
    };
    let child_type = match unsafe { cstr_to_str(child_type) } {
        Ok(s) => s.to_string(),
        Err(c) => return c,
    };
    match store
        .inner
        .add_parent_child_association(core_lib::ParentChildAssociation {
            parent_id,
            parent_type,
            child_id,
            child_type,
            // The catalog assigns; this table's wire form carries no id.
            id: None,
        }) {
        Ok(id) => {
            if !out_id.is_null() {
                unsafe { *out_id = id };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Record many edges in one all-or-nothing transaction, from a JSON array of
/// objects with `parent_id`, `parent_type`, `child_id`, and `child_type`.
///
/// `out_added` and `out_ids` mean exactly what they mean on
/// `infrastore_store_add_supplemental_attribute_associations`, over this
/// table's own id stream.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `associations_json` a valid, null-
/// terminated UTF-8 string. `out_added`, when non-null, must be valid for writing one
/// `uint64_t`. `out_ids`, when non-null, must be valid for writing one pointer; on
/// `INFRASTORE_OK` it receives an array of `*out_added` ids that the caller owns and must
/// release with `infrastore_buffer_free_i64(*out_ids, *out_added)`. An empty batch writes
/// null there, which needs no release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_add_parent_child_associations(
    handle: *mut InfraStoreHandle,
    associations_json: *const c_char,
    out_added: *mut u64,
    out_ids: *mut *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let assocs: Vec<core_lib::ParentChildAssociation> =
        match unsafe { assoc_rows_from_json(associations_json) } {
            Ok(v) => v,
            Err(c) => return c,
        };
    match store.inner.add_parent_child_associations(assocs) {
        Ok(ids) => {
            unsafe { write_assigned_ids(ids, out_added, out_ids) };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Whether any edge matches `filter_json`. Recognized filter fields, all
/// optional: `parent_id`, `parent_types`, `child_id`, `child_types`. A null or
/// empty string matches everything; an empty type list matches nothing.
///
/// # Safety
///
/// `handle` must be a live store handle, `filter_json` null or valid
/// null-terminated UTF-8, and `out_found` valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_has_parent_child_association(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    out_found: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_found.is_null() {
        set_error("out_found is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.has_parent_child_association(&filter) {
        Ok(found) => {
            unsafe { *out_found = found };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Edges matching `filter_json` as a JSON array, in insertion order. Each object
/// carries `parent_id`, `parent_type`, `child_id`, and `child_type`.
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid null-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_parent_child_associations(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.list_parent_child_associations(&filter) {
        Ok(rows) => unsafe { write_json_out(&rows, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

/// Distinct ids on one end of the edges matching `filter_json`, ascending, as a
/// JSON array. `endpoint` is `0` for parents and `1` for children — so
/// `endpoint = 1` with `parent_id` set is "the children of this component".
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid null-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_list_parent_child_ids(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    endpoint: i32,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let ids = match endpoint {
        0 => store.inner.list_parents(&filter),
        1 => store.inner.list_children(&filter),
        other => {
            set_error(format!(
                "invalid endpoint {other}, expected 0 (parent) or 1 (child)"
            ));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match ids {
        Ok(ids) => unsafe { write_json_out(&ids, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

/// Remove every edge matching `filter_json`. When non-null, `out_removed`
/// receives the number removed; removing nothing is success, not an error.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `filter_json` null or valid null-
/// terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_remove_parent_child_associations(
    handle: *mut InfraStoreHandle,
    filter_json: *const c_char,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.remove_parent_child_associations(&filter) {
        Ok(n) => {
            if !out_removed.is_null() {
                unsafe { *out_removed = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Rewrite component `old_id` to `new_id` on both ends of every edge. When
/// non-null, `out_updated` receives the rows changed. Returns
/// `INFRASTORE_ERR_DUPLICATE_ASSOCIATION` if the rewrite would duplicate an edge
/// `new_id` already has.
///
/// # Safety
///
/// Standard: see the crate-level ABI conventions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_replace_parent_child_component_id(
    handle: *mut InfraStoreHandle,
    old_id: i64,
    new_id: i64,
    out_updated: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store
        .inner
        .replace_parent_child_component_id(old_id, new_id)
    {
        Ok(n) => {
            if !out_updated.is_null() {
                unsafe { *out_updated = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Number of edges matching `filter_json`, through `out_count`.
///
/// # Safety
///
/// `handle` must be a live store handle, `filter_json` null or valid
/// null-terminated UTF-8, and `out_count` valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_count_parent_child_associations(
    handle: *const InfraStoreHandle,
    filter_json: *const c_char,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.count_parent_child_associations(&filter) {
        Ok(n) => {
            unsafe { *out_count = n };
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- OpenAPI-row association serde -----------------------------------------
//
// Three exports over `infrastore_core::openapi` (crate-private there; `Store`
// inherent methods are the public surface): the two exports use the
// owned-string convention (catalog-scaled output), and import returns its row
// count through an out-param, matching
// `infrastore_store_add_supplemental_attribute_associations` above.

/// Export `time_series_associations` matching the filter as a sorted
/// OpenAPI-row JSON array. Each row's `uri` and `data_hash` are the
/// hex-encoded content hash the store already has for that row — never a
/// caller-supplied locator. Filters match `infrastore_store_list_metadata`.
/// Returns the JSON through `out_json` as an **owned** allocation the caller
/// releases with `infrastore_string_free`; `out_len` is its byte length.
///
/// # Safety
///
/// The scalar filter flags/values are plain scalars; `name`, `resolution`, `interval`,
/// `features_json`; `component_field` must each be null or a null-terminated UTF-8 string.
/// `out_json` must be valid for writing one pointer and `out_len` for writing one `u64`; on
/// success `*out_json` must be released exactly once with `infrastore_string_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_export_time_series_associations_openapi(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        build_list_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            has_time_series_type,
            time_series_type,
            name,
            std::ptr::null(),
            resolution,
            interval,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let json = match store.inner.export_time_series_associations_openapi(&filter) {
        Ok(j) => j,
        Err(e) => return map_core_error(e),
    };
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// Export the whole `supplemental_attribute_associations` table as an
/// OpenAPI-row JSON array, sorted by `(component_id, attribute_id)`. Returns
/// the JSON through `out_json` as an **owned** allocation the caller releases
/// with `infrastore_string_free`; `out_len` is its byte length.
///
/// # Safety
///
/// `out_json` must be valid for writing one pointer and `out_len` for writing one `u64`; on
/// success `*out_json` must be released exactly once with `infrastore_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_export_supplemental_attribute_associations_openapi(
    handle: *const InfraStoreHandle,
    out_json: *mut *mut c_char,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_json.is_null() || out_len.is_null() {
        set_error("a required pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let json = match store
        .inner
        .export_supplemental_attribute_associations_openapi()
    {
        Ok(j) => j,
        Err(e) => return map_core_error(e),
    };
    unsafe { write_owned_str_out(json, out_json, out_len) }
}

/// Bulk-ingest a JSON array of time-series association OpenAPI rows in one
/// all-or-nothing transaction — the import half of the round trip whose export
/// is `infrastore_store_export_time_series_associations_openapi`. When non-null,
/// `out_added` receives the number inserted.
///
/// Rows only: the document carries locators, never values, so every row must
/// name an array this store already holds, and each row keeps the
/// `association_id` it carries. A row whose array is absent, or a
/// `NonSequentialTimeSeries` row (whose timestamp vector is not on the wire),
/// is refused with `INFRASTORE_ERR_INVALID_PARAMETER`; an `association_id`
/// already in use is `INFRASTORE_ERR_DUPLICATE_ASSOCIATION_ID`.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `json` a valid, null-terminated UTF-8
/// string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_import_time_series_associations_openapi(
    handle: *mut InfraStoreHandle,
    json: *const c_char,
    out_added: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let json = match unsafe { cstr_to_str(json) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    match store.inner.import_time_series_associations_openapi(json) {
        Ok(n) => {
            if !out_added.is_null() {
                unsafe { *out_added = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Bulk-ingest a JSON array of supplemental-attribute association OpenAPI rows
/// in one all-or-nothing transaction — the import half of the round trip whose
/// export is `infrastore_store_export_supplemental_attribute_associations_openapi`.
/// When non-null, `out_added` receives the number inserted.
///
/// # Safety
///
/// `handle` must be a live read-write store handle and `json` a valid, null-terminated UTF-8
/// string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_store_import_supplemental_attribute_associations_openapi(
    handle: *mut InfraStoreHandle,
    json: *const c_char,
    out_added: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let json = match unsafe { cstr_to_str(json) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    match store
        .inner
        .import_supplemental_attribute_associations_openapi(json)
    {
        Ok(n) => {
            if !out_added.is_null() {
                unsafe { *out_added = n as u64 };
            }
            INFRASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- Free helpers ---------------------------------------------------------

/// Release an `f64` buffer returned by this library.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_buffer_free_f64(ptr: *mut f64, len: u64) {
    unsafe { free_raw_buffer(ptr, len) };
}

/// Free a `u8` buffer returned by `infrastore_store_get_array_by_hash`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` bytes. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_buffer_free_u8(ptr: *mut u8, len: u64) {
    unsafe { free_raw_buffer(ptr, len) };
}

/// Free an `i64` buffer returned by `infrastore_bulk_result_get_non_sequential`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_buffer_free_i64(ptr: *mut i64, len: u64) {
    unsafe { free_raw_buffer(ptr, len) };
}

// ---- Error message --------------------------------------------------------

/// Copy the thread-local error message into `buf` (UTF-8, null-terminated).
/// Returns the number of bytes that would have been written (excluding the NUL)
/// in `*needed`. If `buf_len` is too small, `buf` is filled up to its length
/// and truncated; the function still returns `INFRASTORE_OK` and the caller can decide
/// whether to retry with a larger buffer.
///
/// # Safety
///
/// `needed` may be null; otherwise it must be valid for writing one `u64`. `buf` may be null when
/// `buf_len` is zero; otherwise it must reference at least `buf_len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_last_error_message(
    buf: *mut c_char,
    buf_len: u64,
    needed: *mut u64,
) -> i32 {
    let msg = LAST_ERROR.with(|e| e.borrow().clone()).unwrap_or_default();
    let bytes = msg.as_bytes();
    let needed_len = bytes.len() as u64;
    if !needed.is_null() {
        unsafe { *needed = needed_len };
    }
    if buf.is_null() || buf_len == 0 {
        return INFRASTORE_OK;
    }
    let max_copy = std::cmp::min(buf_len.saturating_sub(1) as usize, bytes.len());
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, max_copy);
        *buf.add(max_copy) = 0;
    }
    INFRASTORE_OK
}

// ---- helpers --------------------------------------------------------------

unsafe fn parse_features_json(json: *const c_char) -> Result<core_lib::Features, i32> {
    let mut features: core_lib::Features = BTreeMap::new();
    if json.is_null() {
        return Ok(features);
    }
    let s = match unsafe { cstr_to_str(json) } {
        Ok(s) => s,
        Err(c) => {
            set_error("features_json is not valid UTF-8");
            return Err(c);
        }
    };
    if s.trim().is_empty() {
        return Ok(features);
    }
    let value: Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => {
            set_error(format!("features_json: {e}"));
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            set_error("features_json must be an object");
            return Err(INFRASTORE_ERR_INVALID_PARAMETER);
        }
    };
    for (k, v) in obj {
        let fv = match v {
            Value::Bool(b) => core_lib::FeatureValue::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    core_lib::FeatureValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    core_lib::FeatureValue::Float(f)
                } else {
                    set_error(format!("feature {k}: number out of range"));
                    return Err(INFRASTORE_ERR_INVALID_PARAMETER);
                }
            }
            Value::String(s) => core_lib::FeatureValue::Str(s.clone()),
            other => {
                set_error(format!(
                    "feature {k}: must be int/float/bool/string, got {}",
                    type_name(other)
                ));
                return Err(INFRASTORE_ERR_INVALID_PARAMETER);
            }
        };
        features.insert(k.clone(), fv);
    }
    Ok(features)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn unix_ms_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// Build an optional `(start, end)` time range from the FFI convention shared by
/// the get paths: `present = false` yields `None`; otherwise both millisecond
/// bounds are converted to UTC. Returns `Err(code)` (after setting the
/// thread-local error) if either bound is out of range.
///
/// `zoneless` carries how the caller *spelled* those bounds. The wire form is
/// Unix milliseconds either way — a zoneless caller sends its wall clock read as
/// if UTC, exactly as the store holds one — so this flag is the only thing that
/// tells the two apart, and the core refuses a bound whose spelling the series
/// cannot answer.
fn build_time_range(
    present: bool,
    zoneless: bool,
    start_ms: i64,
    end_ms: i64,
) -> Result<Option<core_lib::TimeRange>, i32> {
    if !present {
        return Ok(None);
    }
    let start = unix_ms_to_datetime(start_ms).ok_or_else(|| {
        set_error(format!("invalid time_range_start_ms: {start_ms}"));
        INFRASTORE_ERR_INVALID_PARAMETER
    })?;
    let end = unix_ms_to_datetime(end_ms).ok_or_else(|| {
        set_error(format!("invalid time_range_end_ms: {end_ms}"));
        INFRASTORE_ERR_INVALID_PARAMETER
    })?;
    Ok(Some(core_lib::TimeRange::spelled(start, end, zoneless)))
}

/// Unix milliseconds for a UTC datetime. Infallible: chrono's `DateTime<Utc>`
/// range (about ±262,000 years) is far inside what an `i64` of milliseconds
/// represents, so `timestamp_millis` cannot overflow.
fn datetime_to_unix_ms(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

use chrono::TimeZone;

// ---- Timestamp readers (StaticReader / ForecastReader) --------------------
//
// Stateful readers for the simulation access pattern: a loop over every
// timestamp wants the value of every series at that instant. Build a reader
// once (it resolves the catalog layout and owns reusable buffers), then call
// the read function per timestamp and read each group/entry's values pointer,
// which is valid until the next read on that reader or until it is freed.

/// Opaque handle wrapping a core `StaticReader` (SingleTimeSeries, columnar).
pub struct InfraStoreStaticReaderHandle {
    inner: core_lib::StaticReader,
}

/// Opaque handle wrapping a core `ForecastReader` (one forecast type, per-key
/// windows).
pub struct InfraStoreForecastReaderHandle {
    inner: core_lib::ForecastReader,
}

/// Build a [`core_lib::ListFilter`] from the reader build arguments shared by
/// both readers (owner / category / name / name_glob / resolution / features /
/// component_field). The time-series type is set by the caller, not here.
///
/// # Safety
///
/// `name`, `name_glob`, `component_field`, and `features_json` must each be null
/// or a null-terminated UTF-8 string; `resolution` must be null or a
/// null-terminated ISO-8601 period.
#[allow(clippy::too_many_arguments)]
unsafe fn reader_filter(
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
) -> Result<core_lib::ListFilter, i32> {
    let mut filter = core_lib::ListFilter::new();
    if has_owner {
        filter = filter.owner_id(owner_id);
    }
    if has_owner_category {
        let category = match owner_category {
            0 => core_lib::OwnerCategory::Component,
            1 => core_lib::OwnerCategory::SupplementalAttribute,
            other => {
                set_error(format!("invalid owner_category {other}"));
                return Err(INFRASTORE_ERR_INVALID_PARAMETER);
            }
        };
        filter = filter.owner_category(category);
    }
    match unsafe { cstr_to_optional_string(name) } {
        Ok(Some(n)) => filter = filter.name(n),
        Ok(None) => {}
        Err(c) => {
            set_error("name is not valid UTF-8");
            return Err(c);
        }
    }
    match unsafe { cstr_to_optional_string(name_glob) } {
        Ok(Some(g)) => filter = filter.name_glob(g),
        Ok(None) => {}
        Err(c) => {
            set_error("name_glob is not valid UTF-8");
            return Err(c);
        }
    }
    if let Some(p) = unsafe { cstr_to_optional_period(resolution)? } {
        filter = filter.resolution(p);
    }
    match unsafe { cstr_to_optional_string(component_field) } {
        Ok(Some(f)) => filter = filter.component_field(f),
        Ok(None) => {}
        Err(c) => {
            set_error("component_field is not valid UTF-8");
            return Err(c);
        }
    }
    if let Some(zoneless) = zoneless_filter(zoneless)? {
        filter = filter.zoneless(zoneless);
    }
    let features = unsafe { parse_features_json(features_json) }?;
    if !features.is_empty() {
        filter = filter.features(features);
    }
    Ok(filter)
}

/// Write `values` into `buf` (truncated to `cap` elements), always reporting the
/// full length through `out_len`. Probe-then-fetch: call with `buf` null and
/// `cap` 0 to learn the length first. Used for the small shape arrays.
///
/// # Safety
///
/// When `buf` is non-null it must be valid for writing `cap` `i64` values.
unsafe fn write_i64_slice_out(values: &[i64], buf: *mut i64, cap: u64, out_len: *mut u64) {
    unsafe {
        *out_len = values.len() as u64;
        if !buf.is_null() && cap > 0 {
            let n = values.len().min(cap as usize);
            ptr::copy_nonoverlapping(values.as_ptr(), buf, n);
        }
    }
}

// ---- StaticReader ---------------------------------------------------------

/// Build a [`InfraStoreStaticReaderHandle`] over the static series matching the
/// filter. The filter arguments are `infrastore_store_list_metadata`'s, minus the
/// interval (a static series has none) -- `name_glob` included.
///
/// `time_series_type` is a `TimeSeriesType` discriminant and selects the two
/// shapes a reader can take:
///
/// * `SingleTimeSeries` (0): `resolution` must be a non-empty ISO-8601 period —
///   one resolution per reader — and the matched series must share one grid
///   (`initial_timestamp` + `length`).
/// * `NonSequentialTimeSeries` (1): `resolution` must be null (an irregular
///   series has none); the matched series must instead share one timestamp
///   vector, which is also what pools their arrays on disk. Read that timeline
///   with `infrastore_static_reader_timestamps`.
///
/// Any other discriminant is rejected.
///
/// # Safety
///
/// `name` / `name_glob` / `resolution` / `features_json` / `component_field` -- every string
/// argument -- must be null or valid null-terminated UTF-8; must stay readable for the duration
/// of the call.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_build_static_reader(
    handle: *const InfraStoreHandle,
    time_series_type: i32,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_reader: *mut *mut InfraStoreStaticReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let ts_type = match resolve_requested_type_from_int(time_series_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {time_series_type}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let filter = match unsafe {
        reader_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            name,
            name_glob,
            resolution,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let filter = filter.time_series_type(ts_type);
    let reader = match store.inner.build_static_reader(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_reader = Box::into_raw(Box::new(InfraStoreStaticReaderHandle { inner: reader }))
    };
    INFRASTORE_OK
}

/// Read the reader's timeline: `initial_timestamp` (unix ms), `resolution` (an
/// owned ISO-8601 duration string, e.g. `PT1H` / `P1M`), and the number of
/// timestamps on it.
///
/// `*out_resolution` is **null** for a `NonSequentialTimeSeries` reader: an
/// irregular timeline has no constant step, so read it with
/// `infrastore_static_reader_timestamps` instead.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. Each out pointer must be valid
/// for writing one value. On success `*out_resolution` is either null or an
/// owned C string the caller must free exactly once with
/// [`infrastore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_grid(
    reader: *const InfraStoreStaticReaderHandle,
    out_initial_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_length: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null() || out_resolution.is_null() || out_length.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe {
        *out_initial_ms = datetime_to_unix_ms(reader.inner.initial_timestamp());
        *out_resolution = opt_period_cstr(reader.inner.resolution());
        *out_length = reader.inner.length() as u64;
    }
    INFRASTORE_OK
}

/// The one spelling the reader's timeline carries, as an owned C string
/// (`"utc"`, `"zoneless"`, `"-07:00"`, or an IANA zone name), or **null** when
/// the cohort records no reference.
///
/// A reader spans one timeline, so it carries one spelling: a cohort whose
/// columns agree reports their reference, one whose columns merely agree on
/// naming instants reports `"utc"`, and a cohort mixing zoneless with the rest
/// never builds at all. Without this a caller can read the axis but not say how
/// it was written -- it cannot tell a wall-clock axis from an unspecified or UTC
/// one, which is exactly the distinction the axis exists to preserve.
///
/// Separate from [`infrastore_static_reader_grid`] rather than another out
/// parameter on it: adding one there would shift every following argument for
/// callers already compiled against the existing declaration.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. On success it is either null or an owned C
/// string the caller must free exactly once with [`infrastore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_time_reference(
    reader: *const InfraStoreStaticReaderHandle,
    out_time_reference: *mut *mut c_char,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_time_reference.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe {
        *out_time_reference = reader
            .inner
            .time_reference()
            .map(|r| owned_cstr(&r.as_storage_string()))
            .unwrap_or(std::ptr::null_mut());
    }
    INFRASTORE_OK
}

/// Every timestamp on the reader's timeline, in order, as unix milliseconds.
///
/// Probe-then-fetch: call with `buf` null and `cap` 0 to learn the length
/// (always reported through `out_len`), then again with a buffer that size. This
/// is how a caller reads an irregular timeline, whose instants
/// `infrastore_static_reader_grid` cannot describe; it works for a regular grid
/// too, where they are `initial_timestamp + k · resolution`.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. When non-null, `buf` must be valid for writing
/// `cap` `i64` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_timestamps(
    reader: *const InfraStoreStaticReaderHandle,
    buf: *mut i64,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let millis: Vec<i64> = reader.inner.timestamps().map(datetime_to_unix_ms).collect();
    unsafe { write_i64_slice_out(&millis, buf, cap, out_len) };
    INFRASTORE_OK
}

/// Number of columnar groups in the reader.
///
/// # Safety
///
/// `reader` must be a live static-reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_num_groups(
    reader: *const InfraStoreStaticReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.groups().len() as u64 };
    INFRASTORE_OK
}

/// Read group `group_idx`'s layout: its dtype code, column count, and per-step
/// element shape. The shape follows the probe-then-fetch convention: call with
/// `shape_buf` null / `shape_cap` 0 to learn `out_shape_len`, then call again
/// with a buffer of at least that many `i64` values. An empty shape (scalar per
/// step) reports length 0.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_dtype`, `out_num_columns`,
/// and `out_shape_len` must be valid for writing one value each. When non-null,
/// `shape_buf` must be valid for writing `shape_cap` `i64` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_group_info(
    reader: *const InfraStoreStaticReaderHandle,
    group_idx: u64,
    out_dtype: *mut i32,
    out_num_columns: *mut u64,
    shape_buf: *mut i64,
    shape_cap: u64,
    out_shape_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_num_columns.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let shape: Vec<i64> = group.element_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = group.dtype().code();
        *out_num_columns = group.num_columns() as u64;
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    INFRASTORE_OK
}

/// Write the association id of column `col_idx` of group `group_idx` to
/// `out_id`.
///
/// The reader's columns are in buffer order, so this is how a caller maps a
/// column of values back to the series it came from: take the id, then
/// `infrastore_store_get_metadata_by_id` for its owner, name, or descriptors.
/// Nothing is allocated and nothing needs freeing.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_id` must be valid for
/// writing one `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_group_id(
    reader: *const InfraStoreStaticReaderHandle,
    group_idx: u64,
    col_idx: u64,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    if out_id.is_null() {
        set_error("out_id pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let id = match group.ids().get(col_idx as usize) {
        Some(id) => *id,
        None => {
            set_error(format!("column index {col_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_id = id.get() };
    INFRASTORE_OK
}

/// Read the value of every series at `at_unix_ms`, filling the reader's reusable
/// buffers. After this, `infrastore_static_reader_group_values` exposes each group's
/// bytes. Errors if `at_unix_ms` is off the reader's grid.
///
/// # Safety
///
/// `reader` must be a live static-reader handle and `store` a live store handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_read(
    reader: *mut InfraStoreStaticReaderHandle,
    store: *const InfraStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.static_read(&mut reader.inner, at) {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Expose group `group_idx`'s value bytes from the most recent read. The pointer
/// is into reader-owned memory and is valid until the next read on this reader
/// or until it is freed; do not free it. Before any read the length is 0.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_ptr` and `out_byte_len`
/// must be valid for writing one value each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_group_values(
    reader: *const InfraStoreStaticReaderHandle,
    group_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let bytes = group.values();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    INFRASTORE_OK
}

/// Free a static-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `infrastore_store_build_static_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_static_reader_free(reader: *mut InfraStoreStaticReaderHandle) {
    if !reader.is_null() {
        unsafe { drop(Box::from_raw(reader)) };
    }
}

// ---- ForecastReader -------------------------------------------------------

/// Build a [`InfraStoreForecastReaderHandle`] over the forecasts matching the filter.
/// The filter arguments are `infrastore_store_list_metadata`'s, minus the interval --
/// `name_glob` included.
/// `time_series_type` must be a forecast type; a `Deterministic` reader also
/// includes `DeterministicSingleTimeSeries`, matching the read request rule.
/// `resolution` must be positive; matched forecasts must share one window
/// timeline.
///
/// # Safety
///
/// `name` / `name_glob` / `resolution` / `features_json` / `component_field` -- every string
/// argument -- must be null or valid null-terminated UTF-8; must stay readable for the duration
/// of the call. Free the result with `infrastore_forecast_reader_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn infrastore_store_build_forecast_reader(
    handle: *const InfraStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    time_series_type: i32,
    name: *const c_char,
    name_glob: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    component_field: *const c_char,
    zoneless: i32,
    out_reader: *mut *mut InfraStoreForecastReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let ts_type = match requested_type_from_int(time_series_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {time_series_type}"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let mut filter = match unsafe {
        reader_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            name,
            name_glob,
            resolution,
            features_json,
            component_field,
            zoneless,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    filter = filter.time_series_type(ts_type);
    let reader = match store.inner.build_forecast_reader(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_reader = Box::into_raw(Box::new(InfraStoreForecastReaderHandle { inner: reader }))
    };
    INFRASTORE_OK
}

/// The one spelling the forecast reader's window timeline carries, as an owned C
/// string, or **null** when the cohort records no reference. The forecast
/// counterpart of [`infrastore_static_reader_time_reference`]; same rules.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. On success it is either null or an owned C
/// string the caller must free exactly once with [`infrastore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_time_reference(
    reader: *const InfraStoreForecastReaderHandle,
    out_time_reference: *mut *mut c_char,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_time_reference.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe {
        *out_time_reference = reader
            .inner
            .time_reference()
            .map(|r| owned_cstr(&r.as_storage_string()))
            .unwrap_or(std::ptr::null_mut());
    }
    INFRASTORE_OK
}

/// Read the reader's window timeline: `initial_timestamp` (unix ms),
/// `resolution` and `interval` (each an owned ISO-8601 duration string, e.g.
/// `PT1H` / `P1M`), and the window count.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. Each out pointer must be
/// valid for writing one value. On success `*out_resolution` and `*out_interval`
/// are owned C strings the caller must each free exactly once with
/// [`infrastore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_timeline(
    reader: *const InfraStoreForecastReaderHandle,
    out_initial_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null()
        || out_resolution.is_null()
        || out_interval.is_null()
        || out_count.is_null()
    {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe {
        *out_initial_ms = datetime_to_unix_ms(reader.inner.initial_timestamp());
        *out_resolution = period_cstr(reader.inner.resolution());
        *out_interval = period_cstr(reader.inner.interval());
        *out_count = reader.inner.count() as u64;
    }
    INFRASTORE_OK
}

/// Number of per-key window entries in the reader.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_num_entries(
    reader: *const InfraStoreForecastReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.entries().len() as u64 };
    INFRASTORE_OK
}

/// Number of deduplicated window slots: the count of physical backend reads per
/// [`infrastore_forecast_reader_read`]. Entries that share an array and read plan
/// (e.g. components referencing one shared forecast) collapse to one slot.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_num_slots(
    reader: *const InfraStoreForecastReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.slots().len() as u64 };
    INFRASTORE_OK
}

/// The 0-based slot index backing entry `entry_idx`. Entries sharing an array
/// and read plan return the same slot, letting a caller group components that
/// resolve to one window and materialize it once.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_entry_slot(
    reader: *const InfraStoreForecastReaderHandle,
    entry_idx: u64,
    out_slot: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_slot.is_null() {
        set_error("out_slot is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_slot = entry.slot() as u64 };
    INFRASTORE_OK
}

/// Read entry `entry_idx`'s layout: its dtype code and window shape. The shape
/// follows the probe-then-fetch convention (call with `shape_buf` null /
/// `shape_cap` 0 to learn `out_shape_len`).
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_dtype` and
/// `out_shape_len` must be valid for writing one value each. When non-null,
/// `shape_buf` must be valid for writing `shape_cap` `i64` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_entry_info(
    reader: *const InfraStoreForecastReaderHandle,
    entry_idx: u64,
    out_dtype: *mut i32,
    shape_buf: *mut i64,
    shape_cap: u64,
    out_shape_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    if entry_idx as usize >= reader.inner.entries().len() {
        set_error(format!("entry index {entry_idx} out of bounds"));
        return INFRASTORE_ERR_INVALID_PARAMETER;
    }
    let slot = reader.inner.entry_slot(entry_idx as usize);
    let shape: Vec<i64> = slot.window_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = slot.dtype().code();
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    INFRASTORE_OK
}

/// Write the association id of entry `entry_idx` to `out_id`.
///
/// The forecast counterpart of `infrastore_static_reader_group_id`: entries are
/// in reader order, and the id maps one back to its series through
/// `infrastore_store_get_metadata_by_id`. Nothing is allocated.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_id` must be valid for
/// writing one `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_entry_id(
    reader: *const InfraStoreForecastReaderHandle,
    entry_idx: u64,
    out_id: *mut i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_id.is_null() {
        set_error("out_id pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_id = entry.id().get() };
    INFRASTORE_OK
}

/// Read the forecast window at `at_unix_ms` for every entry, filling the
/// reader's reusable buffers. Errors if `at_unix_ms` is off the window timeline.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle and `store` a live store
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_read(
    reader: *mut InfraStoreForecastReaderHandle,
    store: *const InfraStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return INFRASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.forecast_read(&mut reader.inner, at) {
        Ok(()) => INFRASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Expose entry `entry_idx`'s window bytes from the most recent read. The
/// pointer is into reader-owned memory, valid until the next read or free; do
/// not free it. Before any read the length is 0.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_ptr` and `out_byte_len`
/// must be valid for writing one value each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_entry_values(
    reader: *const InfraStoreForecastReaderHandle,
    entry_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return INFRASTORE_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return INFRASTORE_ERR_NULL_POINTER;
    }
    if entry_idx as usize >= reader.inner.entries().len() {
        set_error(format!("entry index {entry_idx} out of bounds"));
        return INFRASTORE_ERR_INVALID_PARAMETER;
    }
    let bytes = reader.inner.entry_slot(entry_idx as usize).window();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    INFRASTORE_OK
}

/// Free a forecast-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `infrastore_store_build_forecast_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn infrastore_forecast_reader_free(
    reader: *mut InfraStoreForecastReaderHandle,
) {
    if !reader.is_null() {
        unsafe { drop(Box::from_raw(reader)) };
    }
}

#[cfg(test)]
mod reader_ffi_tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use core_lib::{OwnerCategory, SingleTimeSeries, Store, TimeSeriesData, TypedArray};

    const T0_MS: i64 = 1_700_000_000_000;
    const HOUR_MS: i64 = 3_600_000;

    fn t0() -> DateTime<Utc> {
        Utc.timestamp_millis_opt(T0_MS).single().unwrap()
    }

    fn add_sts_f64(store: &mut Store, owner_id: i64, name: &str, vals: &[f64]) {
        let data = TypedArray::from_f64(vec![vals.len()], vals);
        let ts = SingleTimeSeries::new(t0(), ChronoDuration::hours(1), data, name);
        store
            .add_time_series(
                owner_id,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(ts),
                Default::default(),
            )
            .unwrap();
    }

    #[test]
    fn static_reader_ffi_roundtrip() {
        let mut store = Store::create(None, true).unwrap();
        add_sts_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        add_sts_f64(&mut store, 2, "load", &[20.0, 21.0, 22.0, 23.0]);
        let handle = InfraStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_build_static_reader(
                &handle,
                0,
                // SingleTimeSeries
                false,
                0,
                false,
                0,
                ptr::null(),
                ptr::null(),
                // name_glob
                hour.as_ptr(),
                ptr::null(),
                ptr::null(),
                -1,
                &mut reader,
            )
        };
        assert_eq!(rc, INFRASTORE_OK);
        assert!(!reader.is_null());

        // Grid. Resolution is an owned ISO-8601 C string.
        let (mut initial, mut len) = (0i64, 0u64);
        let mut res: *mut c_char = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_static_reader_grid(reader, &mut initial, &mut res, &mut len) },
            INFRASTORE_OK
        );
        assert_eq!((initial, len), (T0_MS, 4));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        unsafe { infrastore_string_free(res) };

        // One f64 group, 2 columns, scalar shape.
        let mut n = 0u64;
        assert_eq!(
            unsafe { infrastore_static_reader_num_groups(reader, &mut n) },
            INFRASTORE_OK
        );
        assert_eq!(n, 1);
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 99u64);
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!((dtype, ncols, shape_len), (0, 2, 0)); // F64 code 0

        // Column ids, in buffer order (owners 1 then 2). A group carries ids,
        // not keys, so a column maps back to its series through the catalog --
        // and nothing has to be freed.
        for (col, owner) in [(0u64, 1i64), (1, 2)] {
            let mut id = 0i64;
            assert_eq!(
                unsafe { infrastore_static_reader_group_id(reader, 0, col, &mut id) },
                INFRASTORE_OK
            );
            let row = handle
                .inner
                .get_metadata_by_id(core_lib::TimeSeriesId(id))
                .unwrap()
                .expect("a reader column resolves to a live row");
            assert_eq!(row.owner_id, owner);
        }

        // Read at t0 + 2h -> [12, 22].
        let at = T0_MS + 2 * HOUR_MS;
        assert_eq!(
            unsafe { infrastore_static_reader_read(reader, &handle, at) },
            INFRASTORE_OK
        );
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { infrastore_static_reader_group_values(reader, 0, &mut p, &mut blen) },
            INFRASTORE_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[12.0, 22.0]);

        // Off-grid read errors.
        assert_ne!(
            unsafe { infrastore_static_reader_read(reader, &handle, T0_MS + HOUR_MS / 2) },
            INFRASTORE_OK
        );

        unsafe { infrastore_static_reader_free(reader) };
    }

    #[test]
    fn forecast_reader_ffi_roundtrip() {
        use core_lib::Deterministic;
        let mut store = Store::create(None, true).unwrap();
        // Deterministic H=2, count=3, scalar. Row-major [s, k]; value = k*10 + s.
        let data = TypedArray::from_f64(vec![2, 3], &[0.0, 10.0, 20.0, 1.0, 11.0, 21.0]);
        let det = Deterministic::new(
            t0(),
            ChronoDuration::hours(1),
            ChronoDuration::hours(2),
            ChronoDuration::hours(1),
            3,
            data,
            "gen",
        )
        .unwrap();
        store
            .add_time_series(
                7,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::Deterministic(det),
                Default::default(),
            )
            .unwrap();
        let handle = InfraStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut InfraStoreForecastReaderHandle = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_build_forecast_reader(
                &handle,
                false,
                0,
                false,
                0,
                2,
                // Deterministic
                ptr::null(),
                ptr::null(),
                // name_glob
                hour.as_ptr(),
                ptr::null(),
                ptr::null(),
                -1,
                &mut reader,
            )
        };
        assert_eq!(rc, INFRASTORE_OK);

        let (mut initial, mut count) = (0i64, 0u64);
        let (mut res, mut interval): (*mut c_char, *mut c_char) =
            (ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            unsafe {
                infrastore_forecast_reader_timeline(
                    reader,
                    &mut initial,
                    &mut res,
                    &mut interval,
                    &mut count,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!((initial, count), (T0_MS, 3));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        assert_eq!(
            unsafe { CStr::from_ptr(interval) }.to_str().unwrap(),
            "PT1H"
        );
        unsafe {
            infrastore_string_free(res);
            infrastore_string_free(interval);
        }

        let mut n = 0u64;
        assert_eq!(
            unsafe { infrastore_forecast_reader_num_entries(reader, &mut n) },
            INFRASTORE_OK
        );
        assert_eq!(n, 1);

        // Window shape [H] = [2].
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                infrastore_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!((dtype, shape_len), (0, 1));
        let mut shape = [0i64; 1];
        let mut got = 0u64;
        assert_eq!(
            unsafe {
                infrastore_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    shape.as_mut_ptr(),
                    1,
                    &mut got,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(shape, [2]);

        // Window at index 1 (t0 + 1h) -> [10, 11].
        assert_eq!(
            unsafe { infrastore_forecast_reader_read(reader, &handle, T0_MS + HOUR_MS) },
            INFRASTORE_OK
        );
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { infrastore_forecast_reader_entry_values(reader, 0, &mut p, &mut blen) },
            INFRASTORE_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[10.0, 11.0]);

        unsafe { infrastore_forecast_reader_free(reader) };
    }

    #[test]
    fn get_single_returns_native_dtype_and_shape() {
        use core_lib::Dtype;

        let mut store = Store::create(None, true).unwrap();
        // Int64 with a 2-element shape: stored array shape [3, 2].
        let mut bytes = Vec::new();
        for v in [10i64, 11, 20, 21, 30, 31] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let data = TypedArray::new(Dtype::I64, vec![3, 2], bytes).unwrap();
        let ts = SingleTimeSeries::new(t0(), ChronoDuration::hours(1), data, "im");
        store
            .add_time_series(
                5,
                "Gen",
                OwnerCategory::Component,
                TimeSeriesData::SingleTimeSeries(ts),
                Default::default(),
            )
            .unwrap();
        let handle = InfraStoreHandle { inner: store };

        // The wire carries no key: list for the series, then read the id the
        // row hands back.
        let id = handle
            .inner
            .list_metadata(core_lib::ListFilter::new().owner_id(5).name("im"))
            .unwrap()[0]
            .id
            .expect("a stored row carries its id");

        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    &handle,
                    id.get(),
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    0,
                    &mut result,
                )
            },
            INFRASTORE_OK
        );

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_bulk_result_get_single(
                    result,
                    0,
                    &mut initial,
                    &mut res,
                    &mut dtype,
                    &mut shape_ptr,
                    &mut shape_len,
                    &mut data_ptr,
                    &mut data_len,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(dtype, 2); // I64
        assert_eq!(
            unsafe { slice::from_raw_parts(shape_ptr, shape_len as usize) },
            &[3, 2]
        );
        let vals: Vec<i64> = unsafe { slice::from_raw_parts(data_ptr, data_len as usize) }
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| i64::from_le_bytes(*c))
            .collect();
        assert_eq!(vals, vec![10, 11, 20, 21, 30, 31]);

        unsafe {
            infrastore_string_free(res);
            infrastore_buffer_free_i64(shape_ptr, shape_len);
            infrastore_buffer_free_u8(data_ptr, data_len);
            infrastore_bulk_result_free(result);
        }
    }
}

#[cfg(test)]
mod abi_tests {
    //! Tests that drive the C ABI the way a foreign caller does.
    //!
    //! The `reader_ffi_tests` module above constructs `InfraStoreHandle`
    //! directly and never calls `infrastore_store_create` / `_open` / `_persist` /
    //! `_free`, so the lifecycle exports had no coverage at all. Nor did any test
    //! assert an **error code by value** — a change that returned
    //! `INFRASTORE_ERR_INTERNAL` where a caller expects `INFRASTORE_ERR_NOT_FOUND`
    //! would have gone unnoticed, and the numeric codes are the ABI contract
    //! every binding switches on.
    //!
    //! Deliberately not covered: double-free and use-after-free. Those are
    //! documented undefined behavior, not defined behavior worth pinning.

    use super::*;
    use std::ffi::CString;

    const T0_MS: i64 = 1_700_000_000_000;
    const HOUR: &str = "PT1H";
    /// The `element_type` a plain `f64` series is written with.
    const F64_ET: &std::ffi::CStr = c"f64";

    /// `infrastore_last_error_message`'s current value.
    fn last_error() -> String {
        let mut needed = 0u64;
        assert_eq!(
            unsafe { infrastore_last_error_message(ptr::null_mut(), 0, &mut needed) },
            INFRASTORE_OK
        );
        if needed == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                infrastore_last_error_message(
                    buf.as_mut_ptr() as *mut c_char,
                    buf.len() as u64,
                    &mut needed,
                )
            },
            INFRASTORE_OK
        );
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..nul]).into_owned()
    }

    /// Add one f64 SingleTimeSeries through the ABI; returns its catalog id.
    #[allow(clippy::too_many_arguments)]
    fn abi_add_f64(store: *mut InfraStoreHandle, owner: i64, name: &str, vals: &[f64]) -> i64 {
        let (rc, id) = abi_try_add(
            store,
            owner,
            name,
            F64_ET.as_ptr(),
            &to_le(vals),
            vals.len(),
        );
        assert_eq!(rc, INFRASTORE_OK, "add failed: {}", last_error());
        id
    }

    fn to_le(vals: &[f64]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Add through the ABI without asserting success. Returns the catalog id
    /// the write was filed under, which is how every read addresses it.
    fn abi_try_add(
        store: *mut InfraStoreHandle,
        owner: i64,
        name: &str,
        element_type: *const c_char,
        bytes: &[u8],
        length: usize,
    ) -> (i32, i64) {
        let owner_type = CString::new("Generator").unwrap();
        let name_c = CString::new(name).unwrap();
        let res = CString::new(HOUR).unwrap();
        let dims = [length as u64];
        let mut id = 0i64;
        let rc = unsafe {
            infrastore_store_add_single(
                store,
                owner,
                owner_type.as_ptr(),
                0,
                name_c.as_ptr(),
                T0_MS,
                res.as_ptr(),
                element_type,
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id,
            )
        };
        (rc, id)
    }

    /// The catalog id of the single series `name` on `owner`, through the ABI.
    ///
    /// The identify half: the wire carries no key, so a test that knows a series
    /// by its attributes lists for it and takes the `id` off the row. Asserts
    /// exactly one match, so a fixture that grows a sibling fails here rather
    /// than silently addressing whichever row came back first.
    fn abi_resolve_id(store: *mut InfraStoreHandle, owner: i64, name: &str) -> i64 {
        let name_c = CString::new(name).unwrap();
        let mut out: *mut c_char = ptr::null_mut();
        let mut len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_list_metadata(
                    store,
                    true,
                    owner,
                    true,
                    0,
                    false,
                    0,
                    name_c.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut out,
                    &mut len,
                )
            },
            INFRASTORE_OK,
            "list failed: {}",
            last_error()
        );
        let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { infrastore_string_free(out) };
        let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = rows.as_array().expect("a JSON array of rows");
        assert_eq!(rows.len(), 1, "expected one row named {name}: {json}");
        rows[0]["id"].as_i64().expect("a served row carries its id")
    }

    fn abi_create_in_memory() -> *mut InfraStoreHandle {
        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_create(ptr::null(), true, &mut store) },
            INFRASTORE_OK
        );
        assert!(!store.is_null());
        store
    }

    /// A NonSequentialTimeSeries `application_data` far larger than any fixed buffer
    /// round-trips exactly through `infrastore_bulk_result_get_non_sequential`, which
    /// returns it as an owned string (the earlier caller-sized-buffer form
    /// invited silent truncation).
    #[test]
    fn non_sequential_ext_round_trips_untruncated() {
        let store = abi_create_in_memory();
        let long_application_data: String =
            "{\"payload\":\"".to_string() + &"x".repeat(4096) + "\"}";

        let owner_type = CString::new("Gen").unwrap();
        let name = CString::new("irregular").unwrap();
        let et = CString::new("f64").unwrap();
        let application_data_c = CString::new(long_application_data.clone()).unwrap();
        let timestamps: Vec<i64> = vec![0, 3_600_000, 10_800_000];
        let bytes = to_le(&[1.0, 2.0, 3.0]);
        let dims = [3u64];
        let mut id = 0i64;
        assert_eq!(
            unsafe {
                infrastore_store_add_non_sequential(
                    store,
                    7,
                    owner_type.as_ptr(),
                    0,
                    name.as_ptr(),
                    timestamps.as_ptr(),
                    timestamps.len() as u64,
                    et.as_ptr(),
                    1,
                    dims.as_ptr(),
                    bytes.as_ptr(),
                    bytes.len() as u64,
                    application_data_c.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    &mut id,
                )
            },
            INFRASTORE_OK
        );

        let mut ts_ptr: *mut i64 = ptr::null_mut();
        let mut ts_len = 0u64;
        let mut dtype = -1i32;
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        let mut application_data_out: *mut c_char = ptr::null_mut();
        let mut units_out: *mut c_char = ptr::null_mut();
        // Read by the id the add handed back; the result decodes through the
        // same accessors every bulk read uses.
        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    id,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    0,
                    &mut result,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(
            unsafe {
                infrastore_bulk_result_get_non_sequential(
                    result,
                    0,
                    &mut ts_ptr,
                    &mut ts_len,
                    &mut dtype,
                    &mut shape_ptr,
                    &mut shape_len,
                    &mut data_ptr,
                    &mut data_len,
                    &mut application_data_out,
                    ptr::null_mut(),
                    // skip element_type
                    &mut units_out,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(ts_len, 3);
        assert!(!application_data_out.is_null());
        assert_eq!(
            unsafe { CStr::from_ptr(application_data_out) }
                .to_str()
                .unwrap(),
            long_application_data
        );
        // No units were set, so the out-param reports "unset" as null.
        assert!(units_out.is_null());

        unsafe {
            infrastore_buffer_free_i64(ts_ptr, ts_len);
            infrastore_buffer_free_i64(shape_ptr, shape_len);
            infrastore_buffer_free_u8(data_ptr, data_len);
            infrastore_string_free(application_data_out);
            infrastore_bulk_result_free(result);
            infrastore_store_free(store);
        }
    }

    // ---- Store lifecycle through the ABI ----------------------------------

    #[test]
    fn create_add_flush_free_then_reopen_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abi.h5");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();

        // create on a real path
        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_create(path_c.as_ptr(), false, &mut store) },
            INFRASTORE_OK
        );
        assert!(!store.is_null());

        // the handle reports the path it was created at, and is writable
        let mut read_only = true;
        assert_eq!(
            unsafe { infrastore_store_read_only(store, &mut read_only) },
            INFRASTORE_OK
        );
        assert!(!read_only);

        let mut has_path = false;
        let mut needed = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_get_path(store, &mut has_path, ptr::null_mut(), 0, &mut needed)
            },
            INFRASTORE_OK
        );
        assert!(has_path);
        let mut buf = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                infrastore_store_get_path(
                    store,
                    &mut has_path,
                    buf.as_mut_ptr() as *mut c_char,
                    buf.len() as u64,
                    &mut needed,
                )
            },
            INFRASTORE_OK
        );
        let nul = buf.iter().position(|&b| b == 0).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..nul]).unwrap(),
            path.to_str().unwrap()
        );

        // add, then flush and free
        let id = abi_add_f64(store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        let mut present = false;
        assert_eq!(
            unsafe { infrastore_store_association_exists(store, id, &mut present) },
            INFRASTORE_OK
        );
        assert!(present);
        assert_eq!(unsafe { infrastore_store_flush(store) }, INFRASTORE_OK);
        unsafe { infrastore_store_free(store) };

        // reopen read-only through the ABI and read the values back
        let mut ro: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open(path_c.as_ptr(), true, &mut ro) },
            INFRASTORE_OK
        );
        let mut read_only = false;
        assert_eq!(
            unsafe { infrastore_store_read_only(ro, &mut read_only) },
            INFRASTORE_OK
        );
        assert!(read_only);

        let vals = abi_read_f64(ro, 1, "load");
        assert_eq!(vals, vec![10.0, 11.0, 12.0, 13.0]);

        let mut errors = u64::MAX;
        assert_eq!(
            unsafe { infrastore_store_verify(ro, &mut errors) },
            INFRASTORE_OK
        );
        assert_eq!(errors, 0);

        unsafe { infrastore_store_free(ro) };
    }

    /// Read one f64 SingleTimeSeries by attributes, through the ABI.
    fn abi_read_f64(store: *mut InfraStoreHandle, owner: i64, name: &str) -> Vec<f64> {
        let (dtype, shape, bytes) = abi_get_single(store, owner, name);
        assert_eq!(dtype, core_lib::Dtype::F64.code());
        assert_eq!(shape.len(), 1);
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| f64::from_le_bytes(*c))
            .collect()
    }

    /// a read through the ABI, returning
    /// `(dtype_code, shape, raw bytes)` with every out-buffer freed.
    /// Read owner's series `name` whole, as `(dtype, shape, bytes)`.
    ///
    /// Identify, then act: resolve the attributes to an id, then read it. A
    /// single read comes back in the same result handle as a bulk one, holding
    /// exactly one item, so it decodes through the same accessors.
    fn abi_get_single(
        store: *mut InfraStoreHandle,
        owner: i64,
        name: &str,
    ) -> (i32, Vec<i64>, Vec<u8>) {
        let id = abi_resolve_id(store, owner, name);
        abi_read_by_id(store, id)
    }

    /// [`abi_get_single`] for a caller that already holds the id.
    fn abi_read_by_id(store: *mut InfraStoreHandle, id: i64) -> (i32, Vec<i64>, Vec<u8>) {
        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_read_by_id(
                store,
                id,
                false,
                false,
                0,
                false,
                0,
                false,
                0,
                false,
                0,
                0,
                &mut result,
            )
        };
        assert_eq!(rc, INFRASTORE_OK, "read_by_id failed: {}", last_error());

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res_out: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_bulk_result_get_single(
                    result,
                    0,
                    &mut initial,
                    &mut res_out,
                    &mut dtype,
                    &mut shape_ptr,
                    &mut shape_len,
                    &mut data_ptr,
                    &mut data_len,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            INFRASTORE_OK,
            "decode failed: {}",
            last_error()
        );
        assert_eq!(initial, T0_MS);

        let shape = unsafe { slice::from_raw_parts(shape_ptr, shape_len as usize) }.to_vec();
        let bytes = unsafe { slice::from_raw_parts(data_ptr, data_len as usize) }.to_vec();
        unsafe {
            infrastore_string_free(res_out);
            infrastore_buffer_free_i64(shape_ptr, shape_len);
            infrastore_buffer_free_u8(data_ptr, data_len);
            infrastore_bulk_result_free(result);
        }
        (dtype, shape, bytes)
    }

    #[test]
    fn persist_materializes_an_in_memory_store_and_reopens() {
        let store = abi_create_in_memory();
        let _key = abi_add_f64(store, 3, "load", &[1.5, 2.5, 3.5]);

        // An in-memory store reports no path.
        let (mut has_path, mut len) = (true, 99u64);
        assert_eq!(
            unsafe {
                infrastore_store_get_path(store, &mut has_path, ptr::null_mut(), 0, &mut len)
            },
            INFRASTORE_OK
        );
        assert!(!has_path);
        assert_eq!(len, 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persisted.h5");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { infrastore_store_persist(store, path_c.as_ptr()) },
            INFRASTORE_OK
        );
        unsafe { infrastore_store_free(store) };

        let mut reopened: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open(path_c.as_ptr(), true, &mut reopened) },
            INFRASTORE_OK
        );
        assert_eq!(abi_read_f64(reopened, 3, "load"), vec![1.5, 2.5, 3.5]);
        unsafe { infrastore_store_free(reopened) };
    }

    #[test]
    fn opening_a_missing_path_reports_an_error_and_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.h5");
        let path_c = CString::new(missing.to_str().unwrap()).unwrap();
        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        let rc = unsafe { infrastore_store_open(path_c.as_ptr(), true, &mut store) };
        assert_ne!(rc, INFRASTORE_OK);
        assert!(store.is_null(), "no handle may be produced on failure");
        assert!(
            !last_error().is_empty(),
            "a failed open must leave a diagnostic"
        );
    }

    #[test]
    fn freeing_a_null_store_handle_is_a_no_op() {
        // Documented: `infrastore_store_free` accepts null. Every `*_free` export
        // does, so bindings need no null guard of their own.
        unsafe {
            infrastore_store_free(ptr::null_mut());
            infrastore_string_free(ptr::null_mut());
            infrastore_buffer_free_u8(ptr::null_mut(), 0);
            infrastore_buffer_free_i64(ptr::null_mut(), 0);
            infrastore_buffer_free_f64(ptr::null_mut(), 0);
            infrastore_buffer_free_u64(ptr::null_mut(), 0);
            infrastore_static_reader_free(ptr::null_mut());
            infrastore_forecast_reader_free(ptr::null_mut());
            infrastore_bulk_result_free(ptr::null_mut());
            infrastore_batch_free(ptr::null_mut());
        }
    }

    // ---- Null-pointer sweep -----------------------------------------------

    #[test]
    fn null_handles_and_out_params_return_err_null_pointer() {
        // One representative export per family. The code must be exactly 1:
        // bindings map it to their own "null argument" exception.
        assert_eq!(
            INFRASTORE_ERR_NULL_POINTER, 1,
            "the ABI value is the contract"
        );

        let store = abi_create_in_memory();
        let id = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        // -- store op: null handle, then null out-param. There is no third case
        // any more: the request carries an id, and an `int64` cannot be null.
        let mut present = false;
        assert_eq!(
            unsafe { infrastore_store_association_exists(ptr::null(), id, &mut present) },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe { infrastore_store_association_exists(store, id, ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );
        // create with a null out pointer
        assert_eq!(
            unsafe { infrastore_store_create(ptr::null(), true, ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );
        // probe-style store op with a null out_len
        assert_eq!(
            unsafe { infrastore_store_counts_by_type(store, ptr::null_mut(), 0, ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );

        // -- reader op
        let res = CString::new(HOUR).unwrap();
        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_build_static_reader(
                    ptr::null(),
                    0,
                    // SingleTimeSeries
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut reader,
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        let mut n = 0u64;
        assert_eq!(
            unsafe { infrastore_static_reader_num_groups(ptr::null(), &mut n) },
            INFRASTORE_ERR_NULL_POINTER
        );
        // build a real reader, then pass a null out-param to it
        assert_eq!(
            unsafe {
                infrastore_store_build_static_reader(
                    store,
                    0,
                    // SingleTimeSeries
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut reader,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(
            unsafe { infrastore_static_reader_num_groups(reader, ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );
        let (mut dtype, mut ncols) = (0i32, 0u64);
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        // reader read against a null store handle
        assert_eq!(
            unsafe { infrastore_static_reader_read(reader, ptr::null(), T0_MS) },
            INFRASTORE_ERR_NULL_POINTER
        );
        unsafe { infrastore_static_reader_free(reader) };

        // -- id ops
        //
        // An id-addressed call carries a bare `int64`, so there is no key
        // message to be null. What still has to be checked is every out-param.
        let mut out_result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    ptr::null(),
                    1,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    0,
                    &mut out_result,
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    1,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    0,
                    ptr::null_mut(),
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        let mut present = false;
        assert_eq!(
            unsafe { infrastore_store_association_exists(ptr::null(), 1, &mut present) },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe { infrastore_store_association_exists(store, 1, ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );
        // A non-null `ids` is only required when the count is non-zero.
        let mut removed = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, ptr::null(), 1, false, 0, 0, &mut removed)
            },
            INFRASTORE_ERR_NULL_POINTER
        );

        // -- listing op (owned-string return with a null out_len)
        assert_eq!(
            unsafe {
                infrastore_store_list_metadata(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );

        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn compact_returns_a_report_and_leaves_the_store_usable() {
        let store = abi_create_in_memory();
        let (rc, _id) = abi_try_add(store, 1, "load", F64_ET.as_ptr(), &to_le(&[1.0, 2.0]), 2);
        assert_eq!(rc, INFRASTORE_OK);

        let mut out: *mut c_char = ptr::null_mut();
        let mut len: u64 = 0;
        assert_eq!(
            unsafe { infrastore_store_compact(store, &mut out, &mut len) },
            INFRASTORE_OK
        );
        assert!(!out.is_null());
        let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        assert_eq!(json.len() as u64, len);
        unsafe { infrastore_string_free(out) };

        let report: Value = serde_json::from_str(&json).unwrap();
        for field in [
            "slots_reclaimed",
            "datasets_dropped",
            "feature_sets_reclaimed",
            "timestamp_sets_reclaimed",
            "bytes_reclaimed",
        ] {
            assert!(
                report.get(field).and_then(Value::as_u64).is_some(),
                "missing {field} in {json}"
            );
        }
        // An in-memory store has no file to shrink.
        assert_eq!(report["bytes_reclaimed"].as_u64(), Some(0));

        // The handle still works after the call.
        let mut n = 0i64;
        assert_eq!(
            unsafe { infrastore_store_num_distinct_arrays(store, &mut n) },
            INFRASTORE_OK
        );
        assert_eq!(n, 1);
        unsafe { infrastore_store_free(store) };
    }

    // ---- Invalid UTF-8 -----------------------------------------------------

    /// `name_glob` reaches every filter surface the C ABI exposes.
    ///
    /// The Julia suite covers this end to end, but through the dylib; this pins
    /// it in the crate's own tests, where a signature change is a compile error
    /// rather than a runtime one in another language. The five JSON-returning
    /// listings share one C signature, so they are driven through a function
    /// pointer -- the same way the Julia wrapper resolves them by symbol.
    #[test]
    fn name_glob_filters_every_c_abi_surface() {
        type ListFn = unsafe extern "C" fn(
            *const InfraStoreHandle,
            bool,
            i64,
            bool,
            i32,
            bool,
            i32,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            i32,
            *mut *mut c_char,
            *mut u64,
        ) -> i32;

        let store = abi_create_in_memory();
        for name in ["wind_speed", "wind_dir", "solar_ghi"] {
            let _ = abi_add_f64(store, 1, name, &[1.0, 2.0]);
        }
        let glob = CString::new("wind_*").unwrap();

        let listed = |f: ListFn, pattern: &CStr| -> String {
            let mut out: *mut c_char = ptr::null_mut();
            let mut len = 0u64;
            let rc = unsafe {
                f(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    pattern.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut out,
                    &mut len,
                )
            };
            assert_eq!(rc, INFRASTORE_OK);
            let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
            unsafe { infrastore_string_free(out) };
            assert_eq!(len as usize, json.len(), "out_len is the byte length");
            json
        };

        // The two that carry the matched names. The three key-shaped listings
        // that used to sit beside `list_metadata` were the same query projected
        // differently; one listing carries what all of them projected.
        let by_name: [(&str, ListFn); 2] = [
            ("list_metadata", infrastore_store_list_metadata),
            ("list_names", infrastore_store_list_names),
        ];
        for (label, f) in by_name {
            let json = listed(f, &glob);
            assert!(json.contains("wind_speed"), "{label}: {json}");
            assert!(json.contains("wind_dir"), "{label}: {json}");
            assert!(
                !json.contains("solar_ghi"),
                "{label} must exclude the non-matching row: {json}"
            );
        }

        // `list_owner_types` reports the owners of the matched rows rather than
        // the rows, so the filter shows up as which owners survive it.
        let nothing = CString::new("hydro_*").unwrap();
        assert_eq!(
            listed(infrastore_store_list_owner_types, &glob),
            "[\"Generator\"]"
        );
        assert_eq!(
            listed(infrastore_store_list_owner_types, &nothing),
            "[]",
            "a glob matching no row leaves no owner type"
        );

        // The existence probe.
        let mut present = false;
        assert_eq!(
            unsafe {
                infrastore_store_has_any_by_filter(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    glob.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut present,
                )
            },
            INFRASTORE_OK
        );
        assert!(present);

        // The reader builder, whose filter is built by `reader_filter` rather
        // than `build_list_filter` -- a second decode of the same argument.
        let hour = CString::new(HOUR).unwrap();
        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_build_static_reader(
                    store,
                    0,
                    // SingleTimeSeries
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    glob.as_ptr(),
                    hour.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut reader,
                )
            },
            INFRASTORE_OK
        );
        let mut groups = 0u64;
        assert_eq!(
            unsafe { infrastore_static_reader_num_groups(reader, &mut groups) },
            INFRASTORE_OK
        );
        let mut columns = 0u64;
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut columns,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(
            (groups, columns),
            (1, 2),
            "the two wind series, not the sun"
        );
        unsafe { infrastore_static_reader_free(reader) };

        // And the removal, which is the one that changes the store.
        let mut removed = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_filter(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    glob.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut removed,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(removed, 2);
        let (mut components, mut total, mut arrays) = (0i64, 0i64, 0i64);
        assert_eq!(
            unsafe { infrastore_store_counts(store, &mut components, &mut total, &mut arrays) },
            INFRASTORE_OK
        );
        assert_eq!(total, 1, "solar_ghi is what is left");

        unsafe { infrastore_store_free(store) };
    }

    /// The metadata getter is probe-then-fetch: a call with a null buffer
    /// allocates nothing and reports the length the caller must supply.
    ///
    /// This is what replaced `infrastore_key_attributes`. The attributes used to
    /// come off a key handle the caller was holding; a caller holds an id now,
    /// and the row it names carries every attribute plus the descriptors a key
    /// never did.
    #[test]
    fn get_metadata_by_id_probes_before_it_fetches() {
        let store = abi_create_in_memory();
        let id = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        // The probe: null buffer, zero cap. `out_len` reports what is needed and
        // nothing is written.
        let (mut needed, mut present) = (0u64, false);
        assert_eq!(
            unsafe {
                infrastore_store_get_metadata_by_id(
                    store,
                    id,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    &mut present,
                )
            },
            INFRASTORE_OK
        );
        assert!(present);
        assert!(needed > 0);

        // The fetch: the same call with a buffer the probe sized.
        let mut buf: Vec<c_char> = vec![0; needed as usize + 1];
        let mut got = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_get_metadata_by_id(
                    store,
                    id,
                    buf.as_mut_ptr(),
                    buf.len() as u64,
                    &mut got,
                    &mut present,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(got, needed);
        let json = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        let row: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(row["owner_id"].as_i64(), Some(1));
        assert_eq!(row["name"].as_str(), Some("load"));
        assert_eq!(row["resolution"].as_str(), Some(HOUR));
        assert_eq!(row["id"].as_i64(), Some(id));

        // A stale id is an answer, not an error: `present` says no and the row
        // is not built.
        assert_eq!(
            unsafe {
                infrastore_store_get_metadata_by_id(
                    store,
                    9_999,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    &mut present,
                )
            },
            INFRASTORE_OK
        );
        assert!(!present);

        unsafe { infrastore_store_free(store) };
    }

    /// Both filter builders reject an invalid-UTF-8 `name_glob`, and say which
    /// argument was at fault.
    ///
    /// The catalog filters and the reader builders each build their own
    /// `ListFilter`, so each has its own decode of this argument. A Julia or
    /// Python caller cannot construct the input -- both hand the ABI a string
    /// that is UTF-8 by construction -- so a direct C caller is the only one who
    /// can reach it, and this is the only place it gets exercised.
    #[test]
    fn invalid_utf8_name_glob_is_reported_by_both_filter_builders() {
        let store = abi_create_in_memory();
        let _ = abi_add_f64(store, 1, "load", &[1.0, 2.0]);
        // `wind\xff*` is not valid UTF-8; the trailing NUL terminates the C string.
        let bad_glob: &[u8] = b"wind\xff*\x00";
        let glob_ptr = bad_glob.as_ptr() as *const c_char;
        let res = CString::new(HOUR).unwrap();

        // A catalog filter, via `build_list_filter`.
        let mut out: *mut c_char = ptr::null_mut();
        let mut len = 0u64;
        let rc = unsafe {
            infrastore_store_list_metadata(
                store,
                false,
                0,
                false,
                0,
                false,
                0,
                ptr::null(),
                glob_ptr,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                -1,
                &mut out,
                &mut len,
            )
        };
        assert_eq!(rc, INFRASTORE_ERR_INVALID_UTF8);
        assert!(out.is_null(), "nothing is allocated on the error path");
        assert!(
            last_error().contains("name_glob"),
            "the message must name the argument: {}",
            last_error()
        );

        // A reader builder, via `reader_filter`.
        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_build_static_reader(
                store,
                0,
                // SingleTimeSeries
                false,
                0,
                false,
                0,
                ptr::null(),
                glob_ptr,
                res.as_ptr(),
                ptr::null(),
                ptr::null(),
                -1,
                &mut reader,
            )
        };
        assert_eq!(rc, INFRASTORE_ERR_INVALID_UTF8);
        assert!(reader.is_null(), "no handle is produced on the error path");
        assert!(
            last_error().contains("name_glob"),
            "the message must name the argument: {}",
            last_error()
        );

        // A valid glob still works, so the guard is the only thing rejecting.
        let good = CString::new("lo*").unwrap();
        let rc = unsafe {
            infrastore_store_list_metadata(
                store,
                false,
                0,
                false,
                0,
                false,
                0,
                ptr::null(),
                good.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                -1,
                &mut out,
                &mut len,
            )
        };
        assert_eq!(rc, INFRASTORE_OK);
        assert!(!out.is_null());
        let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { infrastore_string_free(out) };
        assert!(json.contains("\"load\""), "{json}");

        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn invalid_utf8_name_returns_err_invalid_utf8_with_a_message() {
        assert_eq!(
            INFRASTORE_ERR_INVALID_UTF8, 2,
            "the ABI value is the contract"
        );
        let store = abi_create_in_memory();

        // `wind\xff` is not valid UTF-8; the trailing NUL terminates the C string.
        let bad_name: &[u8] = b"wind\xff\x00";
        let owner_type = CString::new("Generator").unwrap();
        let res = CString::new(HOUR).unwrap();
        let vals = [1.0f64, 2.0];
        let bytes = to_le(&vals);
        let dims = [2u64];
        let mut id = 0i64;
        let rc = unsafe {
            infrastore_store_add_single(
                store,
                1,
                owner_type.as_ptr(),
                0,
                bad_name.as_ptr() as *const c_char,
                T0_MS,
                res.as_ptr(),
                F64_ET.as_ptr(),
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id,
            )
        };
        assert_eq!(rc, INFRASTORE_ERR_INVALID_UTF8);
        assert_eq!(id, 0, "a refused add mints no id");
        let msg = last_error();
        assert!(
            !msg.is_empty(),
            "infrastore_last_error_message must describe the failure"
        );

        // Nothing was added.
        let (mut components, mut total, mut arrays) = (0i64, 0i64, 0i64);
        assert_eq!(
            unsafe { infrastore_store_counts(store, &mut components, &mut total, &mut arrays) },
            INFRASTORE_OK
        );
        assert_eq!(total, 0);

        // A successful call clears the message.
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);
        assert!(
            last_error().is_empty(),
            "a successful call must clear the thread-local error"
        );
        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn an_invalid_period_string_is_an_invalid_parameter() {
        assert_eq!(INFRASTORE_ERR_INVALID_PARAMETER, 3);
        let store = abi_create_in_memory();
        let owner_type = CString::new("Generator").unwrap();
        let name = CString::new("load").unwrap();
        let bad_res = CString::new("not-a-period").unwrap();
        let bytes = to_le(&[1.0f64, 2.0]);
        let dims = [2u64];
        let mut id = 0i64;
        let rc = unsafe {
            infrastore_store_add_single(
                store,
                1,
                owner_type.as_ptr(),
                0,
                name.as_ptr(),
                T0_MS,
                bad_res.as_ptr(),
                F64_ET.as_ptr(),
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut id,
            )
        };
        assert_eq!(rc, INFRASTORE_ERR_INVALID_PARAMETER);
        assert!(!last_error().is_empty());
        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn an_unknown_element_type_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        let (rc, id) = abi_try_add(
            store,
            1,
            "load",
            c"float64".as_ptr(),
            &to_le(&[1.0, 2.0]),
            2,
        );
        assert_eq!(rc, INFRASTORE_ERR_INVALID_PARAMETER);
        assert_eq!(id, 0, "a refused add mints no id");
        assert!(!last_error().is_empty());
        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn a_byte_length_that_contradicts_the_shape_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        // Shape says 4 elements, only 2 f64s of bytes are supplied.
        let (rc, id) = abi_try_add(store, 1, "load", F64_ET.as_ptr(), &to_le(&[1.0, 2.0]), 4);
        assert_eq!(rc, INFRASTORE_ERR_INVALID_PARAMETER);
        assert_eq!(id, 0, "a refused add mints no id");
        unsafe { infrastore_store_free(store) };
    }

    // ---- Error codes by value ---------------------------------------------

    #[test]
    fn not_found_duplicate_and_read_only_codes_come_back_by_value() {
        assert_eq!(
            (
                INFRASTORE_ERR_NOT_FOUND,
                INFRASTORE_ERR_DUPLICATE,
                INFRASTORE_ERR_READ_ONLY
            ),
            (4, 5, 7),
            "the ABI values are the contract"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codes.h5");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();

        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_create(path_c.as_ptr(), false, &mut store) },
            INFRASTORE_OK
        );
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        // NOT_FOUND: remove an id the catalog never minted. Ids are never
        // reissued, so a stale one stays stale.
        let missing = 9_999i64;
        let mut removed = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, &missing, 1, false, 0, 0, &mut removed)
            },
            INFRASTORE_ERR_NOT_FOUND
        );
        assert!(!last_error().is_empty());

        // DUPLICATE: add the same identity twice.
        let (rc, dup) = abi_try_add(store, 1, "load", F64_ET.as_ptr(), &to_le(&[1.0, 2.0]), 2);
        assert_eq!(rc, INFRASTORE_ERR_DUPLICATE);
        assert_eq!(dup, 0, "a refused add mints no id");

        assert_eq!(unsafe { infrastore_store_flush(store) }, INFRASTORE_OK);
        unsafe { infrastore_store_free(store) };

        // READ_ONLY: every write through a read-only handle.
        let mut ro: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open(path_c.as_ptr(), true, &mut ro) },
            INFRASTORE_OK
        );
        let (rc, k) = abi_try_add(ro, 2, "new", F64_ET.as_ptr(), &to_le(&[1.0, 2.0]), 2);
        assert_eq!(rc, INFRASTORE_ERR_READ_ONLY);
        assert_eq!(k, 0, "a refused add mints no id");
        let mut removed = 0u64;
        assert_eq!(
            unsafe { infrastore_store_remove_by_ids(ro, &missing, 1, false, 0, 0, &mut removed) },
            INFRASTORE_ERR_READ_ONLY
        );
        let mut report: *mut c_char = ptr::null_mut();
        let mut report_len: u64 = 0;
        assert_eq!(
            unsafe { infrastore_store_compact(ro, &mut report, &mut report_len) },
            INFRASTORE_ERR_READ_ONLY
        );
        assert!(report.is_null());

        unsafe { infrastore_store_free(ro) };
    }

    #[test]
    fn reading_a_stale_id_is_not_found() {
        let store = abi_create_in_memory();
        // An id the catalog never minted. Nothing else can be malformed about an
        // id request, so this is the whole failure surface a read has.
        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_read_by_id(
                store,
                9_999,
                false,
                false,
                0,
                false,
                0,
                false,
                0,
                false,
                0,
                0,
                &mut result,
            )
        };
        assert_eq!(rc, INFRASTORE_ERR_NOT_FOUND);
        // No handle was produced, so there is nothing to free.
        assert!(result.is_null());
        assert!(!last_error().is_empty());

        // The probe that asks instead of failing answers `false`.
        let mut present = true;
        assert_eq!(
            unsafe { infrastore_store_association_exists(store, 9_999, &mut present) },
            INFRASTORE_OK
        );
        assert!(!present);

        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn get_array_by_hash_with_an_unknown_hash_errors() {
        let store = abi_create_in_memory();
        // `data_hash` is 32 raw bytes, not hex. An all-zero hash never exists.
        let zero = [0u8; 32];
        let (mut dtype, mut data_len) = (-1i32, 0u64);
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let rc = unsafe {
            infrastore_store_get_array_by_hash(
                store,
                zero.as_ptr(),
                &mut dtype,
                &mut data_ptr,
                &mut data_len,
            )
        };
        assert_ne!(rc, INFRASTORE_OK);
        assert!(data_ptr.is_null(), "no buffer may be handed out on failure");
        assert!(!last_error().is_empty());

        // A null hash pointer is a null-pointer error, not an integrity error.
        assert_eq!(
            unsafe {
                infrastore_store_get_array_by_hash(
                    store,
                    ptr::null(),
                    &mut dtype,
                    &mut data_ptr,
                    &mut data_len,
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        unsafe { infrastore_store_free(store) };
    }

    // ---- Buffer-probe edges ------------------------------------------------

    #[test]
    fn a_string_probe_buffer_smaller_than_needed_truncates_and_still_reports_the_full_length() {
        // Probe-then-fetch contract: `out_len` is always the full byte length,
        // whatever `cap` was, and a non-zero `cap` yields a NUL-terminated
        // prefix. A binding that trusted `cap` instead of `out_len` would
        // silently read a truncated JSON document.
        let store = abi_create_in_memory();
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        let mut needed = 0u64;
        assert_eq!(
            unsafe { infrastore_store_counts_by_type(store, ptr::null_mut(), 0, &mut needed) },
            INFRASTORE_OK
        );
        assert!(needed > 8, "the JSON must be longer than the tiny buffer");

        // cap = 8: 7 bytes of payload plus the NUL.
        let mut small = vec![0xAAu8; 8];
        let mut reported = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_counts_by_type(
                    store,
                    small.as_mut_ptr() as *mut c_char,
                    small.len() as u64,
                    &mut reported,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(reported, needed, "out_len must report the full length");
        assert_eq!(small[7], 0, "the buffer must stay NUL-terminated");

        // The full read agrees with the truncated prefix.
        let mut full = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                infrastore_store_counts_by_type(
                    store,
                    full.as_mut_ptr() as *mut c_char,
                    full.len() as u64,
                    &mut reported,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(reported, needed);
        assert_eq!(&small[..7], &full[..7]);

        // cap = 1 leaves room only for the terminator.
        let mut one = vec![0xAAu8; 1];
        assert_eq!(
            unsafe {
                infrastore_store_counts_by_type(
                    store,
                    one.as_mut_ptr() as *mut c_char,
                    1,
                    &mut reported,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(reported, needed);
        assert_eq!(one[0], 0);

        unsafe { infrastore_store_free(store) };
    }

    #[test]
    fn a_shape_probe_buffer_smaller_than_needed_truncates() {
        let store = abi_create_in_memory();
        // element shape [2, 3] -> stored array shape [2, 2, 3].
        let owner_type = CString::new("Generator").unwrap();
        let name = CString::new("multi").unwrap();
        let res = CString::new(HOUR).unwrap();
        let vals: Vec<f64> = (0..12).map(|i| i as f64).collect();
        let bytes = to_le(&vals);
        let dims = [2u64, 2, 3];
        let mut id = 0i64;
        assert_eq!(
            unsafe {
                infrastore_store_add_single(
                    store,
                    1,
                    owner_type.as_ptr(),
                    0,
                    name.as_ptr(),
                    T0_MS,
                    res.as_ptr(),
                    F64_ET.as_ptr(),
                    3,
                    dims.as_ptr(),
                    bytes.as_ptr(),
                    bytes.len() as u64,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    &mut id,
                )
            },
            INFRASTORE_OK
        );

        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_build_static_reader(
                    store,
                    0,
                    // SingleTimeSeries
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut reader,
                )
            },
            INFRASTORE_OK
        );

        // Probe: element shape is [2, 3], so 2 entries.
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 0u64);
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(shape_len, 2);

        // cap = 1: only the first dim is written, but the length still reports 2.
        let mut one = [-1i64; 2];
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    one.as_mut_ptr(),
                    1,
                    &mut shape_len,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(shape_len, 2, "out_len reports the full rank");
        assert_eq!(one[0], 2, "the first dim was written");
        assert_eq!(one[1], -1, "the caller's second slot was left untouched");

        unsafe {
            infrastore_static_reader_free(reader);
            infrastore_store_free(store);
        }
    }

    #[test]
    fn an_out_of_range_group_or_entry_index_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);
        let res = CString::new(HOUR).unwrap();

        let mut reader: *mut InfraStoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_build_static_reader(
                    store,
                    0,
                    // SingleTimeSeries
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut reader,
                )
            },
            INFRASTORE_OK
        );
        let mut num_groups = 0u64;
        assert_eq!(
            unsafe { infrastore_static_reader_num_groups(reader, &mut num_groups) },
            INFRASTORE_OK
        );
        assert_eq!(num_groups, 1);

        // group_idx == num_groups is one past the end.
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 0u64);
        assert_eq!(
            unsafe {
                infrastore_static_reader_group_info(
                    reader,
                    num_groups,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert!(!last_error().is_empty());

        // A column index past the group's width, and the group index again.
        let mut out_id = -1i64;
        assert_eq!(
            unsafe { infrastore_static_reader_group_id(reader, 0, 99, &mut out_id) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert_eq!(out_id, -1, "a refused lookup writes nothing");
        assert_eq!(
            unsafe { infrastore_static_reader_group_id(reader, 99, 0, &mut out_id) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );

        // Values before any read, and for an out-of-range group.
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { infrastore_static_reader_group_values(reader, 99, &mut p, &mut blen) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        unsafe { infrastore_static_reader_free(reader) };

        // The forecast reader's entry index behaves the same way.
        let det_store = abi_create_in_memory();
        abi_add_deterministic(det_store, 7, "gen");
        let mut freader: *mut InfraStoreForecastReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_build_forecast_reader(
                    det_store,
                    false,
                    0,
                    false,
                    0,
                    2,
                    // Deterministic
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut freader,
                )
            },
            INFRASTORE_OK
        );
        let mut num_entries = 0u64;
        assert_eq!(
            unsafe { infrastore_forecast_reader_num_entries(freader, &mut num_entries) },
            INFRASTORE_OK
        );
        assert_eq!(num_entries, 1);
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                infrastore_forecast_reader_entry_info(
                    freader,
                    num_entries,
                    &mut dtype,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        let mut slot = 0u64;
        assert_eq!(
            unsafe { infrastore_forecast_reader_entry_slot(freader, 99, &mut slot) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        unsafe {
            infrastore_forecast_reader_free(freader);
            infrastore_store_free(det_store);
            infrastore_store_free(store);
        }
    }

    /// A Deterministic H=2, count=3 scalar forecast, added through the core API
    /// (the ABI's forecast add takes a much wider argument list and is exercised
    /// by the Julia suite).
    fn abi_add_deterministic(store: *mut InfraStoreHandle, owner: i64, name: &str) {
        use chrono::{Duration as ChronoDuration, TimeZone, Utc};
        let initial = Utc.timestamp_millis_opt(T0_MS).single().unwrap();
        let data = core_lib::TypedArray::from_f64(vec![2, 3], &[0.0, 10.0, 20.0, 1.0, 11.0, 21.0]);
        let det = core_lib::Deterministic::new(
            initial,
            ChronoDuration::hours(1),
            ChronoDuration::hours(2),
            ChronoDuration::hours(1),
            3,
            data,
            name,
        )
        .unwrap();
        let store = unsafe { store.as_mut() }.unwrap();
        store
            .inner
            .add_time_series(
                owner,
                "Generator",
                core_lib::OwnerCategory::Component,
                core_lib::TimeSeriesData::Deterministic(det),
                Default::default(),
            )
            .unwrap();
    }

    // ---- Element types -----------------------------------------------------

    #[test]
    fn every_scalar_element_type_round_trips_through_get_single() {
        // The write side names the element type; the read side still reports the
        // physical dtype code, and both are the ABI contract each binding maps to
        // its own element type.
        assert_eq!(
            [
                core_lib::Dtype::F64.code(),
                core_lib::Dtype::F32.code(),
                core_lib::Dtype::I64.code(),
                core_lib::Dtype::I32.code(),
                core_lib::Dtype::U64.code(),
                core_lib::Dtype::Bool.code(),
            ],
            [0, 1, 2, 3, 4, 5],
            "the ABI dtype codes are the contract"
        );

        let store = abi_create_in_memory();

        // (name, dtype code, raw little-endian bytes, element count)
        let f32_bytes: Vec<u8> = [1.5f32, -2.5, f32::INFINITY]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let i32_bytes: Vec<u8> = [i32::MIN, 0, i32::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let u64_bytes: Vec<u8> = [0u64, 1, u64::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let bool_bytes: Vec<u8> = vec![1u8, 0, 1];
        let i64_bytes: Vec<u8> = [i64::MIN, 0, i64::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let f64_bytes = to_le(&[1.0, 2.0, 3.0]);

        // (element_type name, expected dtype code on read, raw little-endian bytes)
        let cases: Vec<(&std::ffi::CStr, i32, Vec<u8>)> = vec![
            (c"f64", 0, f64_bytes),
            (c"f32", 1, f32_bytes),
            (c"i64", 2, i64_bytes),
            (c"i32", 3, i32_bytes),
            (c"u64", 4, u64_bytes),
            (c"bool", 5, bool_bytes),
        ];

        for (i, (element_type, code, bytes)) in cases.iter().enumerate() {
            let name = element_type.to_str().unwrap();
            let (rc, _id) = abi_try_add(store, i as i64 + 1, name, element_type.as_ptr(), bytes, 3);
            assert_eq!(rc, INFRASTORE_OK, "adding {name}: {}", last_error());

            let (got_dtype, shape, got_bytes) = abi_get_single(store, i as i64 + 1, name);
            assert_eq!(got_dtype, *code, "{name}: dtype code");
            assert_eq!(shape, vec![3], "{name}: shape");
            assert_eq!(&got_bytes, bytes, "{name}: bytes are not byte-exact");
        }

        unsafe { infrastore_store_free(store) };
    }

    // ---- Artifact-safety exports ------------------------------------------
    //
    // `infrastore_store_create_replacing`, `_open_copy`, and `_persist_catalog`
    // are reachable from the Julia wrapper, but the wrapper validates its own
    // arguments before it calls, so nothing there can drive a null `out`, an
    // out-of-range `compression_kind`/`catalog_mode`, or the two error codes
    // these guards return. Those are the ABI contract every binding switches on,
    // so they are asserted by value here.

    /// A store at `path` holding one f64 series for owner 1.
    fn abi_store_with_one_series(path: &std::path::Path) -> CString {
        let path_c = CString::new(path.to_str().unwrap()).unwrap();
        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_create(path_c.as_ptr(), false, &mut store) },
            INFRASTORE_OK,
            "create failed: {}",
            last_error()
        );
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0, 3.0]);
        unsafe {
            assert_eq!(infrastore_store_flush(store), INFRASTORE_OK);
            infrastore_store_free(store);
        }
        path_c
    }

    /// How many keys `store` lists, via the JSON listing export.
    fn abi_key_count(store: *mut InfraStoreHandle) -> usize {
        let mut out: *mut c_char = ptr::null_mut();
        let mut len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_list_metadata(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    // name_glob
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut out,
                    &mut len,
                )
            },
            INFRASTORE_OK,
            "list failed: {}",
            last_error()
        );
        let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { infrastore_string_free(out) };
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_array().expect("a JSON array of keys").len()
    }

    /// Creating where a store already lives is refused, by code, through the ABI.
    #[test]
    fn abi_create_over_an_existing_store_reports_store_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path_c = abi_store_with_one_series(&dir.path().join("abi.h5"));

        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_create(path_c.as_ptr(), false, &mut store) },
            INFRASTORE_ERR_STORE_EXISTS
        );
        assert!(
            store.is_null(),
            "a refused create must not hand back a handle"
        );
        assert!(
            last_error().contains("already exists"),
            "unhelpful message: {}",
            last_error()
        );

        // ...and the explicit replacing form goes through, dropping the old rows.
        let mut replaced: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_create_replacing(path_c.as_ptr(), 1, 3, true, 0, &mut replaced)
            },
            INFRASTORE_OK,
            "replacing create failed: {}",
            last_error()
        );
        assert_eq!(abi_key_count(replaced), 0, "the replaced catalog survived");
        unsafe { infrastore_store_free(replaced) };
    }

    /// The argument guards on the two new constructors, by code.
    #[test]
    fn abi_new_constructors_reject_bad_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let path_c = abi_store_with_one_series(&dir.path().join("abi.h5"));
        let dest = CString::new(dir.path().join("copy.h5").to_str().unwrap()).unwrap();
        let mut out: *mut InfraStoreHandle = ptr::null_mut();

        // A null `out` has nowhere to put the handle.
        assert_eq!(
            unsafe {
                infrastore_store_create_replacing(path_c.as_ptr(), 1, 3, true, 0, ptr::null_mut())
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe {
                infrastore_store_open_copy(path_c.as_ptr(), dest.as_ptr(), 0, ptr::null_mut())
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        // A null path is a null pointer, not invalid UTF-8. Unlike
        // `infrastore_store_create`, neither of these takes an optional path:
        // there is no in-memory form of replacing or copying an artifact.
        assert_eq!(
            unsafe { infrastore_store_create_replacing(ptr::null(), 1, 3, true, 0, &mut out) },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe { infrastore_store_open_copy(ptr::null(), dest.as_ptr(), 0, &mut out) },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe { infrastore_store_open_copy(path_c.as_ptr(), ptr::null(), 0, &mut out) },
            INFRASTORE_ERR_NULL_POINTER
        );
        // Out-of-range enum codes. The Julia wrapper maps its own symbols and so
        // can never send these, which is exactly why they are pinned here.
        assert_eq!(
            unsafe { infrastore_store_create_replacing(path_c.as_ptr(), 7, 3, true, 0, &mut out) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert!(
            last_error().contains("compression_kind"),
            "{}",
            last_error()
        );
        assert_eq!(
            unsafe { infrastore_store_create_replacing(path_c.as_ptr(), 1, 3, true, 9, &mut out) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert!(last_error().contains("catalog_mode"), "{}", last_error());
        assert_eq!(
            unsafe { infrastore_store_open_copy(path_c.as_ptr(), dest.as_ptr(), 9, &mut out) },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert!(out.is_null(), "a rejected call must not hand back a handle");
    }

    /// `open_copy` carries the data, leaves the source alone, and refuses a
    /// destination that already holds a store.
    #[test]
    fn abi_open_copy_copies_and_refuses_a_live_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("abi.h5");
        let src_c = abi_store_with_one_series(&src);
        let src_bytes = std::fs::read(&src).unwrap();
        let dest = dir.path().join("copy.h5");
        let dest_c = CString::new(dest.to_str().unwrap()).unwrap();

        let mut copy: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open_copy(src_c.as_ptr(), dest_c.as_ptr(), 0, &mut copy) },
            INFRASTORE_OK,
            "open_copy failed: {}",
            last_error()
        );
        assert_eq!(abi_key_count(copy), 1, "the copy lost the source's series");
        unsafe { infrastore_store_free(copy) };
        assert_eq!(
            std::fs::read(&src).unwrap(),
            src_bytes,
            "open_copy wrote to the source"
        );

        // The destination now holds a store, so a second copy onto it is refused
        // for the same reason a create would be.
        let mut again: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open_copy(src_c.as_ptr(), dest_c.as_ptr(), 0, &mut again) },
            INFRASTORE_ERR_STORE_EXISTS
        );
        assert!(again.is_null());
    }

    /// `persist_catalog` lands an in-memory catalog beside the arrays already
    /// written, without copying them.
    #[test]
    fn abi_persist_catalog_writes_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scratch.h5");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();
        let sqlite = std::path::PathBuf::from(format!("{}.sqlite", path.display()));

        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                // catalog_mode = 1: the catalog stays in RAM.
                infrastore_store_create_with_catalog(
                    path_c.as_ptr(),
                    false,
                    1,
                    3,
                    true,
                    1,
                    &mut store,
                )
            },
            INFRASTORE_OK,
            "create failed: {}",
            last_error()
        );
        let _key = abi_add_f64(store, 1, "load", &[1.0, 2.0, 3.0]);
        assert!(!sqlite.exists(), "an in-memory catalog writes nothing yet");

        assert_eq!(
            unsafe { infrastore_store_persist_catalog(store) },
            INFRASTORE_OK,
            "persist_catalog failed: {}",
            last_error()
        );
        assert!(sqlite.exists(), "the catalog did not reach disk");
        unsafe { infrastore_store_free(store) };

        // The pair opens, which is the whole point: a stamped HDF5 file beside a
        // catalog stamped to match it.
        let mut reopened: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open(path_c.as_ptr(), true, &mut reopened) },
            INFRASTORE_OK,
            "reopen failed: {}",
            last_error()
        );
        assert_eq!(abi_key_count(reopened), 1);
        unsafe { infrastore_store_free(reopened) };

        assert_eq!(
            unsafe { infrastore_store_persist_catalog(ptr::null_mut()) },
            INFRASTORE_ERR_NULL_POINTER
        );
    }

    /// Half an artifact does not open, and says so with its own code.
    #[test]
    fn abi_a_half_artifact_reports_mismatched_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abi.h5");
        let path_c = abi_store_with_one_series(&path);
        std::fs::remove_file(format!("{}.sqlite", path.display())).unwrap();

        let mut store: *mut InfraStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_open(path_c.as_ptr(), false, &mut store) },
            INFRASTORE_ERR_MISMATCHED_ARTIFACT
        );
        assert!(store.is_null());
    }

    /// Add and drop `n` throwaway rows, so the next id `store` assigns clears
    /// `n`. Ids are assigned and never chosen, so a document whose ids must sit
    /// above an importing store's high-water mark is arranged this way.
    fn abi_advance_ids(store: *mut InfraStoreHandle, n: i64) {
        for i in 0..n {
            let name = format!("__spacer{i}");
            let id = abi_add_f64(store, -1, &name, &[i as f64, 0.0, 0.0]);
            let mut removed: u64 = 0;
            assert_eq!(
                unsafe { infrastore_store_remove_by_ids(store, &id, 1, false, 0, 0, &mut removed) },
                INFRASTORE_OK,
                "spacer remove failed: {}",
                last_error()
            );
        }
    }

    /// Item `index`'s name, through the getter `read_by_ids` callers rely on.
    fn abi_bulk_item_name(result: *mut InfraStoreBulkReadHandle, index: u64) -> String {
        let mut out: *mut c_char = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_bulk_result_item_name(result, index, &mut out) },
            INFRASTORE_OK,
            "item_name failed: {}",
            last_error()
        );
        assert!(!out.is_null());
        let s = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { infrastore_string_free(out) };
        s
    }

    /// The first f64 value of bulk item `index`.
    fn abi_bulk_first_value(result: *mut InfraStoreBulkReadHandle, index: u64) -> f64 {
        let mut initial = 0i64;
        let mut resolution: *mut c_char = ptr::null_mut();
        let mut dtype = -1i32;
        let mut shape: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_bulk_result_get_single(
                    result,
                    index,
                    &mut initial,
                    &mut resolution,
                    &mut dtype,
                    &mut shape,
                    &mut shape_len,
                    &mut data,
                    &mut data_len,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            INFRASTORE_OK,
            "get_single failed: {}",
            last_error()
        );
        let bytes = unsafe { slice::from_raw_parts(data, data_len as usize) };
        let value = f64::from_le_bytes(bytes[..8].try_into().unwrap());
        unsafe {
            infrastore_string_free(resolution);
            infrastore_buffer_free_i64(shape, shape_len);
            infrastore_buffer_free_u8(data, data_len);
        }
        value
    }

    /// `infrastore_store_read_by_ids` follows the order it was given, repeats
    /// included, and labels each item with its own name — the id-addressed
    /// counterpart of the keyed bulk read, which reads names off its keys.
    #[test]
    fn abi_read_by_ids_follows_the_order_it_was_given() {
        let store = abi_create_in_memory();
        let a = abi_add_f64(store, 1, "a", &[1.0, 2.0, 3.0]);
        let b = abi_add_f64(store, 2, "b", &[10.0, 11.0, 12.0]);
        let c = abi_add_f64(store, 3, "c", &[100.0, 101.0, 102.0]);

        let asked = [c, a, c, b];
        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_ids(store, asked.as_ptr(), asked.len() as u64, &mut result)
            },
            INFRASTORE_OK,
            "read_by_ids failed: {}",
            last_error()
        );
        assert_eq!(unsafe { infrastore_bulk_result_len(result) }, 4);
        let names: Vec<String> = (0..4).map(|i| abi_bulk_item_name(result, i)).collect();
        assert_eq!(names, vec!["c", "a", "c", "b"]);
        let firsts: Vec<f64> = (0..4).map(|i| abi_bulk_first_value(result, i)).collect();
        assert_eq!(firsts, vec![100.0, 1.0, 100.0, 10.0]);
        unsafe { infrastore_bulk_result_free(result) };

        // An id naming no row fails the whole call with the code a caller
        // switches on, and hands back no handle.
        let missing = [a, 9_999];
        let mut bad: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_read_by_ids(store, missing.as_ptr(), 2, &mut bad) },
            INFRASTORE_ERR_NOT_FOUND
        );
        assert!(bad.is_null());

        // An empty request is a valid one: an empty handle, not an error.
        let mut empty: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe { infrastore_store_read_by_ids(store, ptr::null(), 0, &mut empty) },
            INFRASTORE_OK
        );
        assert_eq!(unsafe { infrastore_bulk_result_len(empty) }, 0);
        unsafe { infrastore_bulk_result_free(empty) };

        assert_eq!(
            unsafe { infrastore_store_read_by_ids(store, ptr::null(), 1, &mut empty) },
            INFRASTORE_ERR_NULL_POINTER
        );

        unsafe { infrastore_store_free(store) };
    }

    /// The owner guard crosses the ABI on both id-addressed calls: it holds the
    /// row to the owner the caller names, and reports a mismatch by its own
    /// code rather than as a missing row.
    #[test]
    fn abi_owner_guarded_read_and_removal_hold_a_row_to_its_owner() {
        let store = abi_create_in_memory();
        let a = abi_add_f64(store, 1, "a", &[1.0, 2.0, 3.0]);

        // Guarded read: the owner that holds it is served, another is refused.
        let mut result: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    a,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    true,
                    1,
                    0,
                    &mut result,
                )
            },
            INFRASTORE_OK
        );
        assert_eq!(unsafe { infrastore_bulk_result_len(result) }, 1);
        unsafe { infrastore_bulk_result_free(result) };

        let mut refused: *mut InfraStoreBulkReadHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    a,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    true,
                    2,
                    0,
                    &mut refused,
                )
            },
            INFRASTORE_ERR_OWNER_MISMATCH
        );
        assert!(refused.is_null());

        // The same id under the other owner category is a different owner.
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    a,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    true,
                    1,
                    1,
                    &mut refused,
                )
            },
            INFRASTORE_ERR_OWNER_MISMATCH
        );

        // An owner_category outside the enum is a bad argument, not a mismatch.
        assert_eq!(
            unsafe {
                infrastore_store_read_by_id(
                    store,
                    a,
                    false,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    true,
                    1,
                    7,
                    &mut refused,
                )
            },
            INFRASTORE_ERR_INVALID_PARAMETER
        );

        // Guarded removal: refused for the wrong owner, and the row survives.
        let mut removed = 0u64;
        assert_eq!(
            unsafe { infrastore_store_remove_by_ids(store, &a, 1, true, 2, 0, &mut removed) },
            INFRASTORE_ERR_OWNER_MISMATCH
        );
        let mut present = false;
        assert_eq!(
            unsafe { infrastore_store_association_exists(store, a, &mut present) },
            INFRASTORE_OK
        );
        assert!(present, "a refused removal must leave the row in place");

        assert_eq!(
            unsafe { infrastore_store_remove_by_ids(store, &a, 1, true, 1, 0, &mut removed) },
            INFRASTORE_OK
        );
        assert_eq!(removed, 1);

        unsafe { infrastore_store_free(store) };
    }

    /// `infrastore_store_remove_by_ids` removes exactly the rows its ids name,
    /// is all-or-nothing when one of them dangles, and reports the count.
    #[test]
    fn abi_remove_by_ids_is_all_or_nothing() {
        let store = abi_create_in_memory();
        let a = abi_add_f64(store, 1, "a", &[1.0, 2.0, 3.0]);
        let b = abi_add_f64(store, 2, "b", &[10.0, 11.0, 12.0]);
        let c = abi_add_f64(store, 3, "c", &[100.0, 101.0, 102.0]);

        // One dangling id fails the batch, and leaves every row in place.
        let doomed = [a, 9_999, b];
        let mut removed = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, doomed.as_ptr(), 3, false, 0, 0, &mut removed)
            },
            INFRASTORE_ERR_NOT_FOUND
        );
        for id in [a, b, c] {
            let mut present = false;
            assert_eq!(
                unsafe { infrastore_store_association_exists(store, id, &mut present) },
                INFRASTORE_OK
            );
            assert!(present, "id {id} must survive the rolled-back batch");
        }

        // The good pair goes together; a repeated id counts once.
        let asked = [a, b, a];
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, asked.as_ptr(), 3, false, 0, 0, &mut removed)
            },
            INFRASTORE_OK,
            "remove_by_ids failed: {}",
            last_error()
        );
        assert_eq!(removed, 2);
        for (id, expected) in [(a, false), (b, false), (c, true)] {
            let mut present = false;
            assert_eq!(
                unsafe { infrastore_store_association_exists(store, id, &mut present) },
                INFRASTORE_OK
            );
            assert_eq!(present, expected, "id {id}");
        }

        // An empty request is valid; a null `ids` with a non-zero length is not,
        // and neither is a null out pointer.
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, ptr::null(), 0, false, 0, 0, &mut removed)
            },
            INFRASTORE_OK
        );
        assert_eq!(removed, 0);
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(store, ptr::null(), 1, false, 0, 0, &mut removed)
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert!(
            last_error().contains("ids"),
            "the diagnostic must name the null input, got: {}",
            last_error()
        );
        assert_eq!(
            unsafe {
                infrastore_store_remove_by_ids(
                    store,
                    asked.as_ptr(),
                    1,
                    false,
                    0,
                    0,
                    ptr::null_mut(),
                )
            },
            INFRASTORE_ERR_NULL_POINTER
        );
        assert!(
            last_error().contains("out_removed"),
            "the diagnostic must name the null output, got: {}",
            last_error()
        );

        unsafe { infrastore_store_free(store) };
    }

    /// Export one store's time-series rows and import them into another that
    /// already holds the arrays: the ids survive, which is the point of putting
    /// them on the wire. A store holding no such array refuses the document.
    #[test]
    fn abi_time_series_openapi_rows_round_trip_with_their_ids() {
        let source = abi_create_in_memory();
        // Above whatever the target will have assigned its own anchor row.
        abi_advance_ids(source, 699);
        let id = abi_add_f64(source, 1, "load", &[1.0, 2.0, 3.0]);
        assert_eq!(id, 700);

        let mut json: *mut c_char = ptr::null_mut();
        let mut json_len = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_export_time_series_associations_openapi(
                    source,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    -1,
                    &mut json,
                    &mut json_len,
                )
            },
            INFRASTORE_OK,
            "export failed: {}",
            last_error()
        );

        // Arrays are content-addressed, so "the artifact brought the values" is
        // a store already holding the same bytes under an identity of its own.
        let target = abi_create_in_memory();
        abi_add_f64(target, 9, "anchor", &[1.0, 2.0, 3.0]);
        let mut added = 0u64;
        assert_eq!(
            unsafe {
                infrastore_store_import_time_series_associations_openapi(target, json, &mut added)
            },
            INFRASTORE_OK,
            "import failed: {}",
            last_error()
        );
        assert_eq!(added, 1);
        let mut present = false;
        assert_eq!(
            unsafe { infrastore_store_association_exists(target, 700, &mut present) },
            INFRASTORE_OK
        );
        assert!(present, "the id did not survive the import");

        // A store holding none of the arrays refuses the rows rather than
        // writing dangling references.
        let empty = abi_create_in_memory();
        assert_eq!(
            unsafe {
                infrastore_store_import_time_series_associations_openapi(
                    empty,
                    json,
                    ptr::null_mut(),
                )
            },
            INFRASTORE_ERR_INVALID_PARAMETER
        );
        assert!(last_error().contains("does not hold"), "{}", last_error());

        unsafe {
            infrastore_string_free(json);
            infrastore_store_free(source);
            infrastore_store_free(target);
            infrastore_store_free(empty);
        }
    }
}
