//! C ABI for `castore`. Used by the Julia binding (`TimeSeries.jl`)
//! and any other language that can call C.
//!
//! v0 surface — read/write SingleTimeSeries with optional features (passed as
//! a JSON object). Errors are reported via int32 status codes and a thread-
//! local message accessed through [`castore_last_error_message`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::ptr;
use std::slice;

use castore_core as core_lib;
use chrono::{DateTime, Utc};
use serde_json::Value;

// ---- Status codes ---------------------------------------------------------

pub const CASTORE_OK: i32 = 0;
pub const CASTORE_ERR_NULL_POINTER: i32 = 1;
pub const CASTORE_ERR_INVALID_UTF8: i32 = 2;
pub const CASTORE_ERR_INVALID_PARAMETER: i32 = 3;
pub const CASTORE_ERR_NOT_FOUND: i32 = 4;
pub const CASTORE_ERR_DUPLICATE: i32 = 5;
pub const CASTORE_ERR_INTEGRITY: i32 = 6;
pub const CASTORE_ERR_READ_ONLY: i32 = 7;
pub const CASTORE_ERR_IO: i32 = 8;
/// The store on disk was written in a different, incompatible on-disk format
/// than this build reads. There is no in-place upgrade.
pub const CASTORE_ERR_INCOMPATIBLE_FORMAT: i32 = 9;
/// The endpoint pair of an association is already associated. Distinct from
/// `CASTORE_ERR_DUPLICATE`, which is about time-series identity.
pub const CASTORE_ERR_DUPLICATE_ASSOCIATION: i32 = 10;
pub const CASTORE_ERR_INTERNAL: i32 = 99;

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
        E::NotFound => CASTORE_ERR_NOT_FOUND,
        E::DuplicateTimeSeries => CASTORE_ERR_DUPLICATE,
        E::DuplicateAssociation(_) => CASTORE_ERR_DUPLICATE_ASSOCIATION,
        E::InvalidParameter(_) => CASTORE_ERR_INVALID_PARAMETER,
        E::IntegrityError(_) => CASTORE_ERR_INTEGRITY,
        E::ReadOnlyStore => CASTORE_ERR_READ_ONLY,
        E::Io(_) => CASTORE_ERR_IO,
        E::IncompatibleFormat { .. } => CASTORE_ERR_INCOMPATIBLE_FORMAT,
        _ => CASTORE_ERR_INTERNAL,
    };
    set_error(e.to_string());
    code
}

/// Dereference a raw handle pointer or return `CASTORE_ERR_NULL_POINTER`.
///
/// `deref_handle!(ref p)` yields `&T` via `p.as_ref()`; `deref_handle!(mut p)`
/// yields `&mut T` via `p.as_mut()`. Both early-return on a null pointer, so
/// this is only usable inside functions returning `i32`.
macro_rules! deref_handle {
    (ref $ptr:expr) => {
        match unsafe { $ptr.as_ref() } {
            Some(v) => v,
            None => return CASTORE_ERR_NULL_POINTER,
        }
    };
    (mut $ptr:expr) => {
        match unsafe { $ptr.as_mut() } {
            Some(v) => v,
            None => return CASTORE_ERR_NULL_POINTER,
        }
    };
}

// ---- Logging --------------------------------------------------------------

/// Initialize the Rust tracing subscriber.
///
/// `filter` is a null-terminated UTF-8 [`EnvFilter`] directive string, e.g.
/// `"debug"` or `"castore_core=debug"`. Pass `NULL` to read the
/// `RUST_LOG` environment variable (or emit nothing if the variable is unset).
///
/// The subscriber is initialized at most once per process. Subsequent calls
/// are no-ops. Returns `CASTORE_OK` on success, `CASTORE_ERR_INVALID_UTF8` if `filter`
/// is not valid UTF-8, or `CASTORE_ERR_INVALID_PARAMETER` if `filter` contains an
/// invalid directive (e.g. an unrecognised level name).
///
/// # Safety
///
/// `filter` must be a valid null-terminated UTF-8 string or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_init_logging(filter: *const c_char) -> i32 {
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
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
    CASTORE_OK
}

// ---- Handles --------------------------------------------------------------

pub struct CastoreStoreHandle {
    inner: core_lib::Store,
}

pub struct CastoreKeyHandle {
    // A key handle is a lookup handle: it carries the identity tuple the catalog
    // resolves. Descriptive window fields (only known for a fully-described key
    // from add/list) are not carried here.
    inner: core_lib::KeyIdentity,
}

/// Accumulates pending add requests for a single all-or-nothing
/// `castore_store_add_batch` call. Building the batch performs no store I/O.
pub struct CastoreBatchHandle {
    items: Vec<core_lib::AddRequest>,
}

/// Owns the results of a bulk-read call (`castore_store_bulk_read_single` or the
/// variant-general `castore_store_bulk_read`): the time series fetched for a batch of
/// keys, in input order. Each element's variant is discovered with
/// `castore_bulk_result_item_type` and read out with the matching
/// `castore_bulk_result_get_*`; the handle is released with `castore_bulk_result_free`.
pub struct CastoreBulkReadHandle {
    items: Vec<core_lib::TimeSeriesData>,
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(CASTORE_ERR_NULL_POINTER);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| CASTORE_ERR_INVALID_UTF8)
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

/// Parse an ISO-8601 period from a C string. `null`/empty -> `None`; a malformed
/// string sets the error and returns `CASTORE_ERR_INVALID_PARAMETER`.
unsafe fn cstr_to_optional_period(p: *const c_char) -> Result<Option<core_lib::Period>, i32> {
    let s = unsafe { cstr_to_optional_string(p)? };
    match s {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => core_lib::Period::from_iso8601(&s).map(Some).map_err(|e| {
            set_error(e.to_string());
            CASTORE_ERR_INVALID_PARAMETER
        }),
    }
}

/// Parse a required ISO-8601 period from a C string.
unsafe fn cstr_to_period(p: *const c_char) -> Result<core_lib::Period, i32> {
    let s = unsafe { cstr_to_str(p)? };
    core_lib::Period::from_iso8601(s).map_err(|e| {
        set_error(e.to_string());
        CASTORE_ERR_INVALID_PARAMETER
    })
}

/// Allocate an owned C string the caller must release with [`castore_string_free`].
/// An interior NUL (never present in an ISO-8601 period) yields a null pointer.
fn owned_cstr(s: &str) -> *mut c_char {
    match std::ffi::CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Owned ISO-8601 C string for a period (caller frees with [`castore_string_free`]).
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
pub unsafe extern "C" fn castore_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(std::ffi::CString::from_raw(s)) };
    }
}

// ---- Store create / open / free ------------------------------------------

/// Create a time-series store and return an owning handle through `out`.
///
/// # Safety
///
/// `out` must be valid for writing one pointer. When non-null, `path` must point to a valid,
/// null-terminated UTF-8 string. The returned handle must be released exactly once with
/// `castore_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_create(
    path: *const c_char,
    in_memory: bool,
    out: *mut *mut CastoreStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
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
    let handle = Box::new(CastoreStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    CASTORE_OK
}

/// Create a store with an explicit NetCDF compression policy.
///
/// `compression_kind` selects the filter: `0` = none (uncompressed), `1` =
/// DEFLATE at `deflate_level` (0–9) with byte `shuffle` when non-zero. Any
/// other `compression_kind` is rejected. The policy is ignored for in-memory
/// stores and persisted so later appends reuse it. Equivalent to
/// [`castore_store_create`] with `compression_kind = 1`, level 3, shuffle on.
///
/// # Safety
///
/// `out` must be valid for writing one pointer. When non-null, `path` must point to a valid,
/// null-terminated UTF-8 string. The returned handle must be released exactly once with
/// `castore_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_create_with_compression(
    path: *const c_char,
    in_memory: bool,
    compression_kind: u8,
    deflate_level: u8,
    shuffle: bool,
    out: *mut *mut CastoreStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let path = match unsafe { cstr_to_optional_path(path) } {
        Ok(p) => p,
        Err(code) => {
            set_error("invalid path");
            return code;
        }
    };
    let compression = match compression_kind {
        0 => core_lib::Compression::None,
        1 => core_lib::Compression::Deflate {
            level: deflate_level,
            shuffle,
        },
        other => {
            set_error(format!(
                "invalid compression_kind {other}, expected 0 (none) or 1 (deflate)"
            ));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let store =
        match core_lib::create_store_with_compression(path.as_deref(), in_memory, compression) {
            Ok(s) => s,
            Err(e) => return map_core_error(e),
        };
    let handle = Box::new(CastoreStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    CASTORE_OK
}

/// Open an existing time-series store and return an owning handle through `out`.
///
/// # Safety
///
/// `path` must point to a valid, null-terminated UTF-8 string, and `out` must be valid for writing
/// one pointer. The returned handle must be released exactly once with `castore_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_open(
    path: *const c_char,
    read_only: bool,
    out: *mut *mut CastoreStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
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
    let handle = Box::new(CastoreStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    CASTORE_OK
}

/// Release a store handle returned by `castore_store_create` or `castore_store_open`.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by this library that has not already been freed.
/// The handle must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_free(handle: *mut CastoreStoreHandle) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle)) };
    }
}

// ---- add_single -----------------------------------------------------------

/// Build a [`TypedArray`] from a dtype code, shape (`ndims` × `dims_ptr`), and
/// raw little-endian bytes. Returns an FFI error code on failure (and sets the
/// thread-local error). The buffers are borrowed for the duration of the call.
unsafe fn build_typed_array(
    dtype_code: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
) -> std::result::Result<core_lib::TypedArray, i32> {
    let dtype = match core_lib::Dtype::from_code(dtype_code) {
        Some(d) => d,
        None => {
            set_error(format!("invalid dtype code {dtype_code}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
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
        CASTORE_ERR_INVALID_PARAMETER
    })
}

/// Parse the `castore_store_add_single` / `castore_batch_add_single` argument list into
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(CASTORE_ERR_NULL_POINTER);
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
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let ext = unsafe { cstr_to_optional_string(ext) }?;
    let features = unsafe { parse_features_json(features_json) }?;

    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let resolution = unsafe { cstr_to_period(resolution)? };
    let array = unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }?;
    let single = core_lib::SingleTimeSeries::new(initial_timestamp, resolution, array, name);

    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data: core_lib::TimeSeriesData::SingleTimeSeries(single),
        features,
        units,

        ext,
    })
}

/// Add a SingleTimeSeries to the store.
///
/// `features_json`, when non-null, is parsed as a JSON object whose values must be int, float,
/// bool, or string. `ext` and `units` are optional.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required string
/// pointers must reference null-terminated UTF-8 strings; optional string pointers may be null.
/// `dims_ptr` must reference `ndims` elements when `ndims` is nonzero, and `data_ptr` must reference
/// `data_byte_len` bytes. `out_key` must be valid for writing one pointer. The returned key must be
/// released with `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_add_single(
    handle: *mut CastoreStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let req = match unsafe {
        build_single_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            initial_ts_unix_ms,
            resolution,
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(CastoreKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- add_non_sequential --------------------------------------------------

/// Parse the `castore_store_add_non_sequential` / `castore_batch_add_non_sequential`
/// argument list into an [`core_lib::AddRequest`].
#[allow(clippy::too_many_arguments)]
unsafe fn build_non_sequential_request(
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if timestamps_unix_ms.is_null() || data_ptr.is_null() {
        set_error("an input pointer is null");
        return Err(CASTORE_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
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
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let array = unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }?;
    let series = match core_lib::NonSequentialTimeSeries::new(timestamps, array, name) {
        Ok(series) => series,
        Err(error) => {
            set_error(error);
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let features = unsafe { parse_features_json(features_json) }?;
    let units = unsafe { cstr_to_optional_string(units) }?;
    let ext = unsafe { cstr_to_optional_string(ext) }?;
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data: core_lib::TimeSeriesData::NonSequentialTimeSeries(series),
        features,
        units,

        ext,
    })
}

/// Add a NonSequentialTimeSeries to the store.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required string
/// pointers must reference null-terminated UTF-8 strings; optional string pointers may be null.
/// `timestamps_unix_ms` must reference `timestamps_len` elements, `dims_ptr` must reference `ndims`
/// elements when `ndims` is nonzero, and `data_ptr` must reference `data_byte_len` bytes. `out_key`
/// must be valid for writing one pointer. The returned key must be released with `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_add_non_sequential(
    handle: *mut CastoreStoreHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let request = match unsafe {
        build_non_sequential_request(
            owner_id,
            owner_type,
            owner_category,
            name,
            timestamps_unix_ms,
            timestamps_len,
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![request]) {
        Ok(mut keys) => {
            unsafe {
                *out_key = Box::into_raw(Box::new(CastoreKeyHandle {
                    inner: keys.remove(0).identity().clone(),
                }))
            };
            CASTORE_OK
        }
        Err(error) => map_core_error(error),
    }
}

// ---- get_single -----------------------------------------------------------

/// Fetch a SingleTimeSeries by key in its native dtype and shape.
///
/// When `time_range_present` is `true`, only the steps whose timestamp falls in
/// `[time_range_start_ms, time_range_end_ms)` are returned (the returned
/// `out_initial_ts_unix_ms` / shape reflect the slice); pass `false` (with any
/// millisecond values) to retrieve the whole series.
///
/// `out_dtype` receives the element dtype code (see [`time_series_type_from_int`]'s dtype
/// siblings: f64=0, f32=1, i64=2, i32=3, u64=4, bool=5). `out_shape` /
/// `out_shape_len` return the full array shape `[length, *element_shape]` (the
/// first dim is time); `out_data` / `out_data_byte_len` return the raw
/// little-endian element bytes. The caller owns both buffers and must free
/// `*out_shape` with `castore_buffer_free_i64` and `*out_data` with
/// `castore_buffer_free_u8`, each using its returned length.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. The returned shape and data buffers must each be released
/// exactly once with the matching free function and returned length.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_single(
    handle: *const CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let time_range =
        match build_time_range(time_range_present, time_range_start_ms, time_range_end_ms) {
            Ok(r) => r,
            Err(c) => return c,
        };
    let data = match store.inner.get_time_series(&key.inner, time_range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    let single = match data {
        core_lib::TimeSeriesData::SingleTimeSeries(single) => single,
        core_lib::TimeSeriesData::NonSequentialTimeSeries(_) => {
            set_error("key does not identify a SingleTimeSeries");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        // Forecast types are not yet exposed through this FFI entry point.
        core_lib::TimeSeriesData::Deterministic(_)
        | core_lib::TimeSeriesData::Probabilistic(_)
        | core_lib::TimeSeriesData::Scenarios(_) => {
            set_error("key identifies a forecast type; use the forecast FFI");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let initial_ms = match datetime_to_unix_ms(single.initial_timestamp) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    let resolution_cstr = period_cstr(single.resolution);
    let dtype = single.data.dtype;
    // Full array shape `[length, *element_shape]`, returned as an owned i64 buffer.
    let mut shape: Vec<i64> = single.data.shape.iter().map(|&d| d as i64).collect();
    let shape_len = shape.len() as u64;
    let shape_ptr = shape.as_mut_ptr();
    std::mem::forget(shape);
    // Native little-endian element bytes, returned as an owned u8 buffer.
    let mut bytes = single.data.bytes;
    let data_len = bytes.len() as u64;
    let data_ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution = resolution_cstr;
        *out_dtype = dtype.code();
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_len;
    }
    CASTORE_OK
}

/// Fetch a NonSequentialTimeSeries by key.
///
/// When `time_range_present` is `true`, only the points whose timestamp falls in
/// `[time_range_start_ms, time_range_end_ms)` are returned; pass `false` to
/// retrieve every point.
///
/// `out_shape` returns the full array shape `[length, *element_shape]` (so callers can recover an
/// N-dimensional per-step element shape, e.g. a `(length, k)` FunctionData encoding); `out_dtype`
/// and `out_data` carry the row-major element bytes. `out_ext` is an optional opaque
/// element-typing tag (e.g. `"QuadraticFunctionData"`) copied into a caller-allocated buffer of
/// `ext_cap` bytes; the full length is reported in `out_ext_len` so the caller can
/// probe with a null/zero-capacity buffer first.
///
/// The caller owns the `out_timestamps`, `out_shape`, and `out_data` buffers and must release them
/// with `castore_buffer_free_i64`, `castore_buffer_free_i64`, and `castore_buffer_free_u8` respectively.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. Returned buffers must each be released exactly once with
/// the matching free function and returned length.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_non_sequential(
    handle: *const CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
    out_timestamps: *mut *mut i64,
    out_timestamps_len: *mut u64,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_ext: *mut c_char,
    ext_cap: u64,
    out_ext_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    let key = deref_handle!(ref key);
    if out_timestamps.is_null()
        || out_timestamps_len.is_null()
        || out_dtype.is_null()
        || out_shape.is_null()
        || out_shape_len.is_null()
        || out_data.is_null()
        || out_data_byte_len.is_null()
        || out_ext_len.is_null()
    {
        return CASTORE_ERR_NULL_POINTER;
    }
    let time_range =
        match build_time_range(time_range_present, time_range_start_ms, time_range_end_ms) {
            Ok(r) => r,
            Err(c) => return c,
        };
    let series = match store.inner.get_time_series(&key.inner, time_range) {
        Ok(core_lib::TimeSeriesData::NonSequentialTimeSeries(series)) => series,
        Ok(core_lib::TimeSeriesData::SingleTimeSeries(_)) => {
            set_error("key does not identify a NonSequentialTimeSeries");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        // Forecast types are not yet exposed through this FFI entry point.
        Ok(
            core_lib::TimeSeriesData::Deterministic(_)
            | core_lib::TimeSeriesData::Probabilistic(_)
            | core_lib::TimeSeriesData::Scenarios(_),
        ) => {
            set_error("key identifies a forecast type; use the forecast FFI");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        Err(error) => return map_core_error(error),
    };
    // The extension payload lives on the metadata row, not on the reconstructed series.
    let ext = match store.inner.get_metadata(&key.inner) {
        Ok(meta) => meta.ext.unwrap_or_default(),
        Err(error) => return map_core_error(error),
    };
    let mut timestamps = match series
        .timestamps
        .iter()
        .map(|timestamp| datetime_to_unix_ms(*timestamp).ok_or(CASTORE_ERR_INTEGRITY))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(timestamps) => timestamps,
        Err(code) => {
            set_error("timestamp out of i64 millisecond range");
            return code;
        }
    };
    let timestamps_len = timestamps.len() as u64;
    let timestamps_ptr = timestamps.as_mut_ptr();
    std::mem::forget(timestamps);

    // Full array shape `[length, *element_shape]`, returned as an owned i64 buffer.
    let mut shape: Vec<i64> = series.data.shape.iter().map(|&d| d as i64).collect();
    let shape_len = shape.len() as u64;
    let shape_ptr = shape.as_mut_ptr();
    std::mem::forget(shape);

    let dtype = series.data.dtype.code();
    let mut bytes = series.data.bytes;
    let data_byte_len = bytes.len() as u64;
    let data_ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        *out_timestamps = timestamps_ptr;
        *out_timestamps_len = timestamps_len;
        *out_dtype = dtype;
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_byte_len;
        write_str_out(&ext, out_ext, ext_cap, out_ext_len);
    }
    CASTORE_OK
}

// ---- remove / has / counts / verify ---------------------------------------

/// Remove the time series identified by `key`.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `key` must be a live key handle created by this
/// library. Neither handle may be concurrently mutated for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_remove(
    handle: *mut CastoreStoreHandle,
    key: *const CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let key = deref_handle!(ref key);
    match store.inner.remove_time_series(&key.inner) {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Remove several time series in one all-or-nothing transaction. On success
/// `*out_removed` receives the number of removed associations; on any error
/// (including a single missing key) nothing is removed.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `keys` must point to `len`
/// valid, non-null key-handle pointers created by this library. `out_removed`
/// must be valid for writing one `u64`. No handle may be concurrently mutated
/// for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_remove_bulk(
    handle: *mut CastoreStoreHandle,
    keys: *const *const CastoreKeyHandle,
    len: u64,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_removed.is_null() || (keys.is_null() && len > 0) {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let mut identities: Vec<&core_lib::KeyIdentity> = Vec::with_capacity(len as usize);
    for i in 0..len as usize {
        match unsafe { (*keys.add(i)).as_ref() } {
            Some(k) => identities.push(&k.inner),
            None => {
                set_error(format!("key {i} is null"));
                return CASTORE_ERR_NULL_POINTER;
            }
        }
    }
    match store.inner.remove_time_series_bulk(&identities) {
        Ok(n) => {
            unsafe { *out_removed = n as u64 };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Report whether the store contains the time series identified by `key`.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library, and `out_present` must be valid
/// for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_has(
    handle: *const CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    let key = deref_handle!(ref key);
    if out_present.is_null() {
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.has_time_series(&key.inner) {
        Ok(b) => {
            unsafe { *out_present = b };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Return aggregate time-series counts.
///
/// # Safety
///
/// `handle` must be a live store handle. All output pointers must be valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_counts(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.get_time_series_counts() {
        Ok(c) => {
            unsafe {
                *out_components_with_time_series = c.components_with_time_series;
                *out_static_time_series = c.static_time_series;
                *out_forecasts = c.forecasts;
            }
            CASTORE_OK
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
/// absent; free each with `castore_string_free`. `out_count` and `out_initial_ms` (the
/// initial timestamp as unix ms) receive their value, or `-1` when absent (counts
/// and timestamps are non-negative when present, so `-1` is an unambiguous "unset"
/// sentinel).
///
/// # Safety
///
/// `handle` must be a live store handle; the filter args are plain scalars.
/// `out_present` must be valid for writing one `bool`; `out_horizon`,
/// `out_interval`, and `out_resolution` must each be valid for writing one
/// `char *`; `out_count` and `out_initial_ms` must each be valid for writing one
/// `i64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_forecast_parameters(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
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
                // freed by the caller with `castore_string_free`.
                *out_horizon = opt_period_cstr(p.horizon);
                *out_interval = opt_period_cstr(p.interval);
                *out_count = p.count.map(|c| c as i64).unwrap_or(-1);
                *out_resolution = opt_period_cstr(p.resolution);
                *out_initial_ms = p
                    .initial_timestamp
                    .and_then(datetime_to_unix_ms)
                    .unwrap_or(-1);
            }
            CASTORE_OK
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
/// pair. Probe-then-fetch (see `castore_store_list_keys`).
///
/// # Safety
///
/// `handle` must be a live store handle. `filter_resolution` must be null or a
/// valid NUL-terminated string. `out_len` must be writable; `buf` must be null
/// or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_check_static_consistency(
    handle: *const CastoreStoreHandle,
    filter_resolution: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
                "initial_timestamp_ms": datetime_to_unix_ms(g.initial_timestamp).unwrap_or(0),
                "length": g.length as i64,
            })
        })
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// List the distinct resolutions present in the store as a JSON array of
/// ISO-8601 duration strings (e.g. `["PT1H","P1M"]`, ascending). When
/// `has_time_series_type` is true the listing is
/// restricted to that `CASTORE_TYPE_*` code; otherwise all types are considered.
///
/// Follows the probe-then-fetch convention: call with `buf` null and `cap` 0 to
/// learn the byte length via `out_len`, then again with a buffer of at least
/// `len + 1` bytes.
///
/// # Safety
///
/// `handle` must be a live store handle; the type filter args are plain scalars.
/// `out_len` must be writable; `buf` must be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_resolutions(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let ts_type = if has_time_series_type {
        match time_series_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return CASTORE_ERR_INVALID_PARAMETER;
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
    CASTORE_OK
}

/// List the distinct forecast intervals present in the store as a JSON array of
/// ISO-8601 duration strings (ascending by ISO text). The interval analog of
/// `castore_store_get_resolutions`; when `has_time_series_type` is true the listing is
/// restricted to that `CASTORE_TYPE_*` code. Non-forecast types yield `[]`.
///
/// Probe-then-fetch (see `castore_store_get_resolutions`).
///
/// # Safety
///
/// `handle` must be a live store handle; the type filter args are plain scalars.
/// `out_len` must be writable; `buf` must be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_intervals(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let ts_type = if has_time_series_type {
        match time_series_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return CASTORE_ERR_INVALID_PARAMETER;
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
    CASTORE_OK
}

/// Write whether the store was opened read-only into `*out_read_only`.
///
/// # Safety
///
/// `handle` must be a live store handle and `out_read_only` valid for writing
/// one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_read_only(
    handle: *const CastoreStoreHandle,
    out_read_only: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_read_only.is_null() {
        set_error("out_read_only is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_read_only = store.inner.read_only() };
    CASTORE_OK
}

/// Write the store's backing NetCDF path into `buf` (probe-then-fetch: call with a
/// null `buf` to learn `*out_len`, then again with a buffer of that size). An
/// in-memory store has no path: `*out_has_path` is set to false and `*out_len` to 0.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_has_path` and `out_len` must be valid
/// for writing; `buf` must be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_path(
    handle: *const CastoreStoreHandle,
    out_has_path: *mut bool,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_has_path.is_null() || out_len.is_null() {
        set_error("out_has_path or out_len is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.netcdf_path() {
        Some(path) => {
            unsafe { *out_has_path = true };
            unsafe { write_str_out(&path.to_string_lossy(), buf, cap, out_len) };
        }
        None => unsafe {
            *out_has_path = false;
            *out_len = 0;
        },
    }
    CASTORE_OK
}

/// Association count grouped by time series type, as a JSON array of
/// `{"time_series_type": <name>, "count": <n>}` objects. Probe-then-fetch (see
/// `castore_store_list_keys`).
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must be
/// null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_counts_by_type(
    handle: *const CastoreStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
    CASTORE_OK
}

/// Write the number of distinct stored arrays (content hashes); shared series
/// count once.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_count` must be valid for writing one
/// `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_num_distinct_arrays(
    handle: *const CastoreStoreHandle,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.num_distinct_arrays() {
        Ok(n) => {
            unsafe { *out_count = n };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Write the detailed counts: distinct owners per category and distinct stored
/// arrays per kind (static vs forecast).
///
/// # Safety
///
/// `handle` must be a live store handle. Each out pointer must be valid for
/// writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_counts_detailed(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.time_series_counts_detailed() {
        Ok(c) => {
            unsafe {
                *out_components = c.components_with_time_series;
                *out_supplemental_attributes = c.supplemental_attributes_with_time_series;
                *out_static_time_series = c.static_time_series_count;
                *out_forecasts = c.forecast_count;
            }
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// List the distinct owner ids of `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) that have a time series, as a JSON array of integers.
/// Optionally restricted to one `time_series_type` (`CASTORE_TYPE_*` code, gated by
/// `has_time_series_type`) and/or `resolution` (empty/null = no filter).
/// Probe-then-fetch (see `castore_store_list_keys`).
///
/// # Safety
///
/// `handle` must be a live store handle; the filter args are plain scalars.
/// `out_len` must be writable; `buf` must be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_owner_ids(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let ts_type = if has_time_series_type {
        match time_series_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return CASTORE_ERR_INVALID_PARAMETER;
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
    CASTORE_OK
}

/// Static-series summary as a JSON array. Each object has `owner_type`,
/// `owner_category`, `time_series_type`, `name`, `initial_timestamp_ms`,
/// `resolution`, `time_step_count`, and `count` (the number of associations in
/// the group); fields that do not apply are `null`. Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must be
/// null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_static_summary(
    handle: *const CastoreStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
                    .and_then(datetime_to_unix_ms)
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
    CASTORE_OK
}

/// Forecast summary as a JSON array. Each object has `owner_type`,
/// `owner_category`, `time_series_type`, `name`, `initial_timestamp_ms`,
/// `resolution`, `horizon`, `interval`, `window_count`, and `count` (the
/// number of associations in the group); fields that do not apply are `null`.
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must be
/// null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_forecast_summary(
    handle: *const CastoreStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
                    .and_then(datetime_to_unix_ms)
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
    CASTORE_OK
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
/// `handle` must be a live store handle. `out_kind` and `out_level` must each be
/// valid for writing one `u8`; `out_shuffle` must be valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_compression(
    handle: *const CastoreStoreHandle,
    out_kind: *mut u8,
    out_level: *mut u8,
    out_shuffle: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_kind.is_null() || out_level.is_null() || out_shuffle.is_null() {
        return CASTORE_ERR_NULL_POINTER;
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
    CASTORE_OK
}

/// Verify store integrity and return the number of detected errors.
///
/// # Safety
///
/// `handle` must be a live store handle and `out_error_count` must be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_verify(
    handle: *const CastoreStoreHandle,
    out_error_count: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_error_count.is_null() {
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.verify_integrity() {
        Ok(r) => {
            unsafe { *out_error_count = r.errors.len() as u64 };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Compact the store.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and must not be used concurrently for the duration
/// of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_compact(handle: *mut CastoreStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.compact() {
        Ok(_) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Flush pending store writes.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and must not be used concurrently for the duration
/// of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_flush(handle: *mut CastoreStoreHandle) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    match store.inner.flush() {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Persist the store's data to `path` (NetCDF) and `<path>.sqlite` (metadata),
/// materializing in-memory stores to disk. Existing target files are overwritten.
///
/// # Safety
///
/// `handle` must be a live store handle; `path` must be a valid NUL-terminated
/// UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_persist(
    handle: *mut CastoreStoreHandle,
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
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

// ---- Attribute-based metadata access --------------------------------------
//
// The Julia `RustTimeSeriesStore` works in terms of (owner_id, name,
// resolution, features) rather than opaque key handles, so these entry points
// build a `TimeSeriesKey` internally and route to the core store. v0 only
// resolves SingleTimeSeries.

unsafe fn build_key_from_attrs(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
) -> Result<core_lib::KeyIdentity, i32> {
    let name = unsafe { cstr_to_str(name) }.inspect_err(|_| {
        set_error("name is invalid");
    })?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let features = unsafe { parse_features_json(features_json) }?;
    let resolution = unsafe { cstr_to_optional_period(resolution)? };
    Ok(core_lib::KeyIdentity {
        owner_id,
        owner_category,
        time_series_type: core_lib::TimeSeriesType::SingleTimeSeries,
        name: name.to_string(),
        resolution,
        interval: None,
        features,
    })
}

/// Look up a SingleTimeSeries metadata record by attributes. On success the
/// caller's out-params receive the initial timestamp, resolution, length, the
/// 32-byte content hash (written into the `out_data_hash` buffer, which must
/// have room for 32 bytes), the dtype code (`out_dtype`), the extension
/// payload and units string via probe-then-fetch (`out_ext` /
/// `out_ext_len` and `out_units` / `out_units_len`; an empty string
/// means the field is unset), the per-timestep element shape via
/// probe-then-fetch (`out_element_shape` / `out_element_shape_len`; length 0
/// means scalar elements), and the features as a JSON object string via
/// probe-then-fetch (`out_features_json` / `out_features_json_len`; `{}` means
/// no features). Returns `CASTORE_ERR_NOT_FOUND` if absent.
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. Scalar output pointers must be valid for one
/// value and `out_data_hash` must be valid for 32 bytes. The `out_ext`, `out_units`, and
/// `out_features_json` caller buffers, when non-null, must be valid for `ext_cap`,
/// `units_cap`, and `features_json_cap` bytes respectively; the `out_element_shape` buffer, when
/// non-null, must be valid for `element_shape_cap` `u64` values; every `*_len` out-pointer must be
/// valid for one `u64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_metadata(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_length: *mut u64,
    out_data_hash: *mut u8,
    out_dtype: *mut i32,
    out_ext: *mut c_char,
    ext_cap: u64,
    out_ext_len: *mut u64,
    out_units: *mut c_char,
    units_cap: u64,
    out_units_len: *mut u64,
    out_element_shape: *mut u64,
    element_shape_cap: u64,
    out_element_shape_len: *mut u64,
    out_features_json: *mut c_char,
    features_json_cap: u64,
    out_features_json_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_dtype.is_null()
        || out_ext_len.is_null()
        || out_units_len.is_null()
        || out_element_shape_len.is_null()
        || out_features_json_len.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_key_from_attrs(owner_id, owner_category, name, resolution, features_json)
    } {
        Ok(k) => k,
        Err(code) => return code,
    };
    let meta = match store.inner.get_metadata(&key) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let initial_ms = match meta.initial_timestamp.and_then(datetime_to_unix_ms) {
        Some(n) => n,
        None => {
            set_error("metadata missing or out-of-range initial_timestamp");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    let resolution_cstr = opt_period_cstr(meta.resolution);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution = resolution_cstr;
        *out_length = meta.length.unwrap_or(0) as u64;
        ptr::copy_nonoverlapping(meta.data_hash.as_ptr(), out_data_hash, 32);
        *out_dtype = meta.dtype.code();
    }
    // ext and units (optional): probe-then-fetch caller buffers.
    unsafe {
        write_str_out(
            meta.ext.as_deref().unwrap_or(""),
            out_ext,
            ext_cap,
            out_ext_len,
        );
        write_str_out(
            meta.units.as_deref().unwrap_or(""),
            out_units,
            units_cap,
            out_units_len,
        );
        let shape: Vec<u64> = meta.element_shape.iter().map(|&d| d as u64).collect();
        write_u64s_out(
            &shape,
            out_element_shape,
            element_shape_cap,
            out_element_shape_len,
        );
        write_str_out(
            &features_to_json(&meta.features),
            out_features_json,
            features_json_cap,
            out_features_json_len,
        );
    }
    CASTORE_OK
}

/// True iff a SingleTimeSeries with the given attributes exists.
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. `out_present` must be valid for writing one
/// `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_has_by_attrs(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_key_from_attrs(owner_id, owner_category, name, resolution, features_json)
    } {
        Ok(k) => k,
        Err(code) => return code,
    };
    match store.inner.has_time_series(&key) {
        Ok(b) => {
            unsafe { *out_present = b };
            CASTORE_OK
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
/// `handle` must be a live store handle; `owner_id` is a plain integer and
/// `owner_category` (`0` = Component, `1` = SupplementalAttribute) identifies the
/// owner category; `out_present` valid for writing one bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_has_for_owner(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    ts_type: i32,
    use_type: bool,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_present.is_null() {
        return CASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let mut filter = core_lib::ListFilter::new()
        .owner_id(owner_id)
        .owner_category(category);
    if use_type {
        let t = match time_series_type_from_int(ts_type) {
            Some(t) => t,
            None => {
                set_error(format!("invalid time_series_type {ts_type}"));
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        filter = filter.time_series_type(t);
    }
    match store.inner.list_time_series(filter) {
        Ok(list) => {
            unsafe { *out_present = !list.is_empty() };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Remove a SingleTimeSeries by attributes. Drops the underlying array iff no
/// other association still references its content hash.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` and `owner_category`
/// (`0` = Component, `1` = SupplementalAttribute) identify the owner. Required strings
/// must be null-terminated UTF-8, and `features_json` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_remove_by_attrs(
    handle: *mut CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let key = match unsafe {
        build_key_from_attrs(owner_id, owner_category, name, resolution, features_json)
    } {
        Ok(k) => k,
        Err(code) => return code,
    };
    match store.inner.remove_time_series(&key) {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Fetch a stored array by its 32-byte content hash. On success the caller owns
/// `*out_data` and must free it with `castore_buffer_free_u8`.
///
/// # Safety
///
/// `handle` must be a live store handle, `data_hash` must reference 32 readable bytes, and every
/// output pointer must be valid for writing its indicated value. The returned buffer must be
/// released exactly once with `castore_buffer_free_u8` using the returned byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_array_by_hash(
    handle: *const CastoreStoreHandle,
    data_hash: *const u8,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if data_hash.is_null() || out_dtype.is_null() || out_data.is_null() || out_byte_len.is_null() {
        set_error("a pointer is null");
        return CASTORE_ERR_NULL_POINTER;
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
    let mut buf: Vec<u8> = array.bytes;
    let len = buf.len() as u64;
    let p = buf.as_mut_ptr();
    std::mem::forget(buf);
    unsafe {
        *out_dtype = dtype;
        *out_data = p;
        *out_byte_len = len;
    }
    CASTORE_OK
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
/// - `handle` must be a live, non-null store handle created by this library; no
///   concurrent mutation is permitted for the duration of the call.
/// - `data_hash` must be non-null and point to at least 32 readable bytes.
/// - `out_sts` and `out_dst` must each be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_count_array_references(
    handle: *const CastoreStoreHandle,
    data_hash: *const u8,
    out_sts: *mut u64,
    out_dst: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if data_hash.is_null() || out_sts.is_null() || out_dst.is_null() {
        set_error("a pointer is null");
        return CASTORE_ERR_NULL_POINTER;
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
    CASTORE_OK
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

/// Request-only `ts_type` sentinel for the `AbstractDeterministic` family. It is
/// never a stored type and never returned through `out_matched_type`; it only
/// addresses a forecast whose concrete type (`Deterministic` or
/// `DeterministicSingleTimeSeries`) the caller does not need to know in advance.
pub const CASTORE_TYPE_ABSTRACT_DETERMINISTIC: i32 = 100;

/// Map a forecast read request's `ts_type` code to a [`core_lib::RequestedType`]:
/// a concrete forecast type (2..=5) or the [`CASTORE_TYPE_ABSTRACT_DETERMINISTIC`]
/// family. The non-forecast types `SingleTimeSeries` (0) and
/// `NonSequentialTimeSeries` (1) are rejected here so the forecast API reports a
/// clear "invalid time_series_type" error up front rather than failing later in
/// `emit_forecast_data` after a key is resolved and data is read.
fn requested_type_from_int(i: i32) -> Option<core_lib::RequestedType> {
    use core_lib::TimeSeriesType as T;
    if i == CASTORE_TYPE_ABSTRACT_DETERMINISTIC {
        return Some(core_lib::RequestedType::AbstractDeterministic);
    }
    match time_series_type_from_int(i) {
        Some(
            t @ (T::Deterministic
            | T::DeterministicSingleTimeSeries
            | T::Probabilistic
            | T::Scenarios),
        ) => Some(core_lib::RequestedType::Concrete(t)),
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
/// `out_len` must be valid for writing one `u64`. When `buf` is non-null it must
/// be valid for writing `cap` bytes.
unsafe fn write_str_out(s: &str, buf: *mut c_char, cap: u64, out_len: *mut u64) {
    let bytes = s.as_bytes();
    unsafe {
        *out_len = bytes.len() as u64;
        if !buf.is_null() && cap > 0 {
            let n = bytes.len().min((cap - 1) as usize);
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
    }
}

/// Write `vals` into the caller buffer `buf` (truncated to `cap` entries),
/// always reporting the full entry count through `out_len`. Safe to call with a
/// null / zero-capacity buffer to probe the required length first.
///
/// # Safety
///
/// `out_len` must be valid for writing one `u64`. When `buf` is non-null it must
/// be valid for writing `cap` `u64` values.
unsafe fn write_u64s_out(vals: &[u64], buf: *mut u64, cap: u64, out_len: *mut u64) {
    unsafe {
        *out_len = vals.len() as u64;
        if !buf.is_null() && cap > 0 {
            let n = vals.len().min(cap as usize);
            ptr::copy_nonoverlapping(vals.as_ptr(), buf, n);
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
) -> Result<core_lib::KeyIdentity, i32> {
    let time_series_type = match time_series_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let mut key =
        unsafe { build_key_from_attrs(owner_id, owner_category, name, resolution, features_json) }?;
    key.time_series_type = time_series_type;
    key.interval = unsafe { cstr_to_optional_period(interval)? };
    Ok(key)
}

/// Add a dense forecast. `data_ptr`/`data_byte_len` is the flattened storage
/// array (Deterministic: `[H, count, *E]`; Scenarios: `[scenario_count, H,
/// count, *E]`). `ts_type` must be 2=Deterministic or 5=Scenarios;
/// `DeterministicSingleTimeSeries` is not directly addable and is derived from a
/// stored `SingleTimeSeries` via `castore_store_transform_single_time_series`.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required strings
/// must be null-terminated UTF-8; optional strings may be null. `data_ptr` must reference `data_len`
/// elements and `out_key` must be valid for writing one pointer. The returned key must be released
/// with `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_add_forecast(
    handle: *mut CastoreStoreHandle,
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(CastoreKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `castore_store_add_forecast` / `castore_batch_add_forecast` argument list
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(CASTORE_ERR_NULL_POINTER);
    }
    let time_series_type = match time_series_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let ext = unsafe { cstr_to_optional_string(ext) }?;
    let array = unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }?;

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
                return Err(CASTORE_ERR_INVALID_PARAMETER);
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
                    return Err(CASTORE_ERR_INVALID_PARAMETER);
                }
            }
        }
        other => {
            set_error(format!(
                "castore_store_add_forecast supports Deterministic and Scenarios; {other:?} \
                 is not directly addable (DeterministicSingleTimeSeries is derived via \
                 castore_store_transform_single_time_series)"
            ));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };

    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
        units,

        ext,
    })
}

/// Add a `Probabilistic` forecast. `data` is the flattened 3-D storage array
/// `(percentile_count, horizon_count, count)` column-major; `percentiles` is the
/// percentile vector.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required strings
/// must be null-terminated UTF-8; optional strings may be null. `percentiles_ptr` and `data_ptr`
/// must reference their respective element counts, and `out_key` must be valid for writing one
/// pointer. The returned key must be released with `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_add_probabilistic(
    handle: *mut CastoreStoreHandle,
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(CastoreKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `castore_store_add_probabilistic` / `castore_batch_add_probabilistic`
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() || percentiles_ptr.is_null() {
        set_error("a required pointer is null");
        return Err(CASTORE_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let percentiles =
        unsafe { slice::from_raw_parts(percentiles_ptr, percentiles_len as usize) }.to_vec();
    let ext = unsafe { cstr_to_optional_string(ext) }?;
    let array = unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }?;

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
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data: core_lib::TimeSeriesData::Probabilistic(prob),
        features,
        units,

        ext,
    })
}

// ---- batched adds ----------------------------------------------------------
//
// A batch accumulates AddRequests client-side; `castore_store_add_batch` commits
// them through `Store::add_time_series_bulk` in ONE metadata transaction.
// This is the fast path for ingesting many series: per-item adds pay one
// SQLite commit each, while a batch pays a single commit for all items.

/// Create an empty add-batch. Building a batch performs no store I/O.
///
/// # Safety
///
/// The returned handle must be released exactly once with `castore_batch_free`
/// (regardless of whether it was submitted via `castore_store_add_batch`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_batch_new() -> *mut CastoreBatchHandle {
    Box::into_raw(Box::new(CastoreBatchHandle { items: Vec::new() }))
}

/// Free a batch handle created by `castore_batch_new`.
///
/// # Safety
///
/// `batch` must be null or a handle returned by `castore_batch_new` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_batch_free(batch: *mut CastoreBatchHandle) {
    if !batch.is_null() {
        drop(unsafe { Box::from_raw(batch) });
    }
}

/// Append a SingleTimeSeries to a batch. Arguments match
/// `castore_store_add_single` (minus the store handle and `out_key`); the data is
/// copied into the batch, so the caller's buffers need only stay valid for
/// this call.
///
/// # Safety
///
/// `batch` must be a live batch handle. `owner_id` is a plain integer. Required
/// string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `dims_ptr` must reference `ndims` elements when `ndims`
/// is nonzero, and `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_batch_add_single(
    batch: *mut CastoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution: *const c_char,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            CASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a NonSequentialTimeSeries to a batch. Arguments match
/// `castore_store_add_non_sequential` (minus the store handle and `out_key`).
///
/// # Safety
///
/// `batch` must be a live batch handle. `owner_id` is a plain integer. Required
/// string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `timestamps_unix_ms` must reference `timestamps_len`
/// elements, `dims_ptr` must reference `ndims` elements when `ndims` is nonzero,
/// and `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_batch_add_non_sequential(
    batch: *mut CastoreBatchHandle,
    owner_id: i64,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    timestamps_unix_ms: *const i64,
    timestamps_len: u64,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            CASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a dense forecast (`ts_type` 2=Deterministic or 5=Scenarios) to a
/// batch. Arguments match `castore_store_add_forecast` (minus the store handle and
/// `out_key`).
///
/// # Safety
///
/// `batch` must be a live batch handle. `owner_id` is a plain integer. Required
/// string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `dims_ptr` must reference `ndims` elements when `ndims`
/// is nonzero, and `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_batch_add_forecast(
    batch: *mut CastoreBatchHandle,
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            CASTORE_OK
        }
        Err(c) => c,
    }
}

/// Append a `Probabilistic` forecast to a batch. Arguments match
/// `castore_store_add_probabilistic` (minus the store handle and `out_key`).
///
/// # Safety
///
/// `batch` must be a live batch handle. `owner_id` is a plain integer. Required
/// string pointers must reference null-terminated UTF-8 strings; optional string
/// pointers may be null. `percentiles_ptr` must reference `percentiles_len`
/// elements, `dims_ptr` must reference `ndims` elements when `ndims` is nonzero,
/// and `data_ptr` must reference `data_byte_len` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_batch_add_probabilistic(
    batch: *mut CastoreBatchHandle,
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
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    ext: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
            dtype,
            ndims,
            dims_ptr,
            data_ptr,
            data_byte_len,
            ext,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            CASTORE_OK
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
/// `handle` must be a live mutable store handle and `batch` a live batch
/// handle. `out_keys` and `out_len` must each be valid for writing one value.
/// On success the caller owns the returned array and every key handle in it:
/// release each key with `castore_key_free`, then the array buffer itself with
/// `castore_keys_buffer_free(*out_keys, *out_len)` (the same contract as
/// `castore_store_get_time_series_keys`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_add_batch(
    handle: *mut CastoreStoreHandle,
    batch: *mut CastoreBatchHandle,
    out_keys: *mut *mut *mut CastoreKeyHandle,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_keys.is_null() || out_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let items = std::mem::take(&mut batch.items);
    match store.inner.add_time_series_bulk(items) {
        Ok(keys) => {
            let mut handles: Vec<*mut CastoreKeyHandle> = keys
                .into_iter()
                .map(|k| {
                    Box::into_raw(Box::new(CastoreKeyHandle {
                        inner: k.identity().clone(),
                    }))
                })
                .collect();
            // Keep capacity == length so `castore_keys_buffer_free` can reconstruct the Vec.
            handles.shrink_to_fit();
            let len = handles.len() as u64;
            let ptr = if handles.is_empty() {
                ptr::null_mut()
            } else {
                let p = handles.as_mut_ptr();
                std::mem::forget(handles);
                p
            };
            unsafe {
                *out_keys = ptr;
                *out_len = len;
            }
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// A bulk read fetches many full SingleTimeSeries in one call, reading each
// packed dataset's column span once (`Store::bulk_read`) instead of re-reading
// every chunk per series. Results are held in a `CastoreBulkReadHandle` and read out
// element-by-element with the same out-parameter shape as `castore_store_get_single`.

/// Read many full `SingleTimeSeries` at once. `keys` points to `n` live key
/// handles; the results are returned through `out_result` as a handle whose
/// elements line up with `keys` in order. Every key must identify a
/// `SingleTimeSeries`; a forecast or non-sequential key makes the whole call
/// fail with `CASTORE_ERR_INVALID_PARAMETER`.
///
/// Soft-deprecated: prefer `castore_store_bulk_read`, which handles every variant
/// (and optional time-range slicing) through the same result handle. This
/// entry point is kept for ABI compatibility with single-type callers.
///
/// # Safety
///
/// `handle` must be a live store handle. `keys` must point to `n` live key
/// handles created by this library (it may be null only when `n` is 0).
/// `out_result` must be valid for writing one pointer. On `CASTORE_OK` the returned
/// handle must be released exactly once with `castore_bulk_result_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_bulk_read_single(
    handle: *const CastoreStoreHandle,
    keys: *const *const CastoreKeyHandle,
    n: u64,
    out_result: *mut *mut CastoreBulkReadHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_result.is_null() {
        set_error("out_result pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let count = n as usize;
    if count != 0 && keys.is_null() {
        set_error("keys pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let key_ptrs = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(keys, count) }
    };
    let mut identities: Vec<&core_lib::KeyIdentity> = Vec::with_capacity(count);
    for &kp in key_ptrs {
        match unsafe { kp.as_ref() } {
            Some(k) => identities.push(&k.inner),
            None => {
                set_error("a key handle is null");
                return CASTORE_ERR_NULL_POINTER;
            }
        }
    }
    let items = match store.inner.bulk_read(&identities) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    // Preserve the single-only contract: reject non-Single results up front.
    if let Some(bad) = items
        .iter()
        .find(|d| !matches!(d, core_lib::TimeSeriesData::SingleTimeSeries(_)))
    {
        set_error(format!(
            "castore_store_bulk_read_single requires every key to identify a SingleTimeSeries; \
             got {}",
            bad.time_series_type().as_str()
        ));
        return CASTORE_ERR_INVALID_PARAMETER;
    }
    unsafe { *out_result = Box::into_raw(Box::new(CastoreBulkReadHandle { items })) };
    CASTORE_OK
}

/// The number of series held by a bulk-read result handle, or `-1` if `result`
/// is null.
///
/// # Safety
///
/// `result` must be null or a live handle from `castore_store_bulk_read_single`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_bulk_result_len(result: *const CastoreBulkReadHandle) -> i64 {
    match unsafe { result.as_ref() } {
        Some(r) => r.items.len() as i64,
        None => -1,
    }
}

/// Read element `index` out of a bulk-read result handle. The out parameters
/// match `castore_store_get_single`: the caller owns the `out_resolution` string and
/// the `out_shape` / `out_data` buffers and must release them with
/// `castore_string_free`, `castore_buffer_free_i64`, and `castore_buffer_free_u8`. The handle
/// is not consumed, so an element may be read more than once.
///
/// # Safety
///
/// `result` must be a live handle from `castore_store_bulk_read_single` and `index`
/// must be less than its length. Every output pointer must be valid for writing
/// its indicated value.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_bulk_result_get_single(
    result: *const CastoreBulkReadHandle,
    index: u64,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let single = match result.items.get(index as usize) {
        Some(core_lib::TimeSeriesData::SingleTimeSeries(s)) => s,
        Some(other) => {
            set_error(format!(
                "bulk-read item {index} is a {}, not a SingleTimeSeries",
                other.time_series_type().as_str()
            ));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let initial_ms = match datetime_to_unix_ms(single.initial_timestamp) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    let resolution_cstr = period_cstr(single.resolution);
    let dtype = single.data.dtype;
    // Owned copies so the result handle stays intact for repeated reads.
    let mut shape: Vec<i64> = single.data.shape.iter().map(|&d| d as i64).collect();
    let shape_len = shape.len() as u64;
    let shape_ptr = shape.as_mut_ptr();
    std::mem::forget(shape);
    let mut bytes = single.data.bytes.clone();
    let data_len = bytes.len() as u64;
    let data_ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution = resolution_cstr;
        *out_dtype = dtype.code();
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_len;
    }
    CASTORE_OK
}

/// Read many series of *any* variant at once, optionally sliced to a time range.
/// The variant-general counterpart of `castore_store_bulk_read_single`: results line
/// up with `keys` in order, and each element's variant is discovered with
/// `castore_bulk_result_item_type` then read with the matching `castore_bulk_result_get_*`.
///
/// When `time_range_present` is `true`, every series is sliced to
/// `[time_range_start_ms, time_range_end_ms)`; pass `false` for whole series.
///
/// # Safety
///
/// `handle` must be a live store handle. `keys` must point to `n` live key
/// handles created by this library (it may be null only when `n` is 0).
/// `out_result` must be valid for writing one pointer. On `CASTORE_OK` the returned
/// handle must be released exactly once with `castore_bulk_result_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_bulk_read(
    handle: *const CastoreStoreHandle,
    keys: *const *const CastoreKeyHandle,
    n: u64,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
    out_result: *mut *mut CastoreBulkReadHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_result.is_null() {
        set_error("out_result pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let count = n as usize;
    if count != 0 && keys.is_null() {
        set_error("keys pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let key_ptrs = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(keys, count) }
    };
    let mut identities: Vec<&core_lib::KeyIdentity> = Vec::with_capacity(count);
    for &kp in key_ptrs {
        match unsafe { kp.as_ref() } {
            Some(k) => identities.push(&k.inner),
            None => {
                set_error("a key handle is null");
                return CASTORE_ERR_NULL_POINTER;
            }
        }
    }
    let time_range =
        match build_time_range(time_range_present, time_range_start_ms, time_range_end_ms) {
            Ok(r) => r,
            Err(c) => return c,
        };
    let items = match store.inner.bulk_read_range(&identities, time_range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe { *out_result = Box::into_raw(Box::new(CastoreBulkReadHandle { items })) };
    CASTORE_OK
}

/// Write the [`time_series_type_to_int`] discriminant of bulk-read item `index` into
/// `out_type` (`0`=SingleTimeSeries, `1`=NonSequentialTimeSeries,
/// `2`=Deterministic, `4`=Probabilistic, `5`=Scenarios — a bulk read never
/// returns the synthesized `DeterministicSingleTimeSeries`). Lets a caller pick
/// the right `castore_bulk_result_get_*` before reading.
///
/// # Safety
///
/// `result` must be a live bulk-read handle, `index` less than its length, and
/// `out_type` valid for writing one `i32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_bulk_result_item_type(
    result: *const CastoreBulkReadHandle,
    index: u64,
    out_type: *mut i32,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_type.is_null() {
        set_error("out_type pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let item = match result.items.get(index as usize) {
        Some(d) => d,
        None => {
            set_error("bulk-read index out of bounds");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_type = time_series_type_to_int(item.time_series_type()) };
    CASTORE_OK
}

/// Read a `NonSequentialTimeSeries` element out of a bulk-read result. The
/// out-params mirror `castore_store_get_non_sequential` except there is no
/// `ext` (a bulk read carries the array data, not the metadata row;
/// fetch it per-key with `castore_store_get_metadata` if needed). The caller owns the
/// `out_timestamps`, `out_shape`, and `out_data` buffers.
///
/// # Safety
///
/// `result` must be a live bulk-read handle and `index` less than its length.
/// Every output pointer must be valid for writing its indicated value. The
/// returned buffers must each be released with the matching free function.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_bulk_result_get_non_sequential(
    result: *const CastoreBulkReadHandle,
    index: u64,
    out_timestamps: *mut *mut i64,
    out_timestamps_len: *mut u64,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let series = match result.items.get(index as usize) {
        Some(core_lib::TimeSeriesData::NonSequentialTimeSeries(s)) => s,
        Some(other) => {
            set_error(format!(
                "bulk-read item {index} is a {}, not a NonSequentialTimeSeries",
                other.time_series_type().as_str()
            ));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let mut timestamps = match series
        .timestamps
        .iter()
        .map(|t| datetime_to_unix_ms(*t).ok_or(CASTORE_ERR_INTEGRITY))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(t) => t,
        Err(code) => {
            set_error("timestamp out of i64 millisecond range");
            return code;
        }
    };
    let timestamps_len = timestamps.len() as u64;
    let timestamps_ptr = timestamps.as_mut_ptr();
    std::mem::forget(timestamps);
    let mut shape: Vec<i64> = series.data.shape.iter().map(|&d| d as i64).collect();
    let shape_len = shape.len() as u64;
    let shape_ptr = shape.as_mut_ptr();
    std::mem::forget(shape);
    let dtype = series.data.dtype.code();
    let mut bytes = series.data.bytes.clone();
    let data_byte_len = bytes.len() as u64;
    let data_ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        *out_timestamps = timestamps_ptr;
        *out_timestamps_len = timestamps_len;
        *out_dtype = dtype;
        *out_shape = shape_ptr;
        *out_shape_len = shape_len;
        *out_data = data_ptr;
        *out_data_byte_len = data_byte_len;
    }
    CASTORE_OK
}

/// Read a forecast element (`Deterministic`, `Probabilistic`, or `Scenarios`)
/// out of a bulk-read result. The out-params mirror `castore_store_get_forecast`
/// (`out_scenario_count` is nonzero only for `Scenarios`; `out_percentiles` is
/// non-null only for `Probabilistic`). The caller owns the `out_dims`,
/// `out_data`, and `out_percentiles` buffers.
///
/// # Safety
///
/// `result` must be a live bulk-read handle and `index` less than its length.
/// Every output pointer must be valid for writing its indicated value. The
/// returned buffers must each be released with the matching `castore_buffer_free_*`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_bulk_result_get_forecast(
    result: *const CastoreBulkReadHandle,
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
) -> i32 {
    clear_error();
    let result = match unsafe { result.as_ref() } {
        Some(r) => r,
        None => {
            set_error("bulk-read result handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
        return CASTORE_ERR_NULL_POINTER;
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
            return CASTORE_ERR_INVALID_PARAMETER;
        }
        None => {
            set_error("bulk-read index out of bounds");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
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

/// Free a bulk-read result handle created by `castore_store_bulk_read_single` or
/// `castore_store_bulk_read`.
///
/// # Safety
///
/// `result` must be null or a handle returned by a bulk-read function
/// that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_bulk_result_free(result: *mut CastoreBulkReadHandle) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

/// Derive `DeterministicSingleTimeSeries` forecasts from the stored
/// `SingleTimeSeries` associations (see `Store::transform_single_time_series`).
/// Writes the number of series transformed to `*out_count`.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `out_count` must be valid
/// for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_transform_single_time_series(
    handle: *mut CastoreStoreHandle,
    horizon: *const c_char,
    interval: *const c_char,
    owner_category: i32,
    resolution: *const c_char,
    out_count: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_count.is_null() {
        set_error("a required pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    // `owner_category < 0` means "all categories"; an empty `resolution` means
    // "all resolutions".
    let category = match owner_category {
        c if c < 0 => None,
        0 => Some(core_lib::OwnerCategory::Component),
        1 => Some(core_lib::OwnerCategory::SupplementalAttribute),
        other => {
            set_error(format!("invalid owner_category {other}"));
            return CASTORE_ERR_INVALID_PARAMETER;
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
    match store
        .inner
        .transform_single_time_series(horizon, interval, category, resolution)
    {
        Ok(n) => {
            unsafe { *out_count = n as u64 };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Read `Probabilistic` metadata. Like `castore_store_get_forecast_metadata` but also
/// returns the percentiles vector in `*out_percentiles` (caller frees with
/// `castore_buffer_free_f64`), the units string via probe-then-fetch
/// (`out_units` / `out_units_len`; an empty string means unset), the
/// per-timestep element shape via probe-then-fetch (`out_element_shape` /
/// `out_element_shape_len`; length 0 means scalar elements), and the features
/// as a JSON object string via probe-then-fetch (`out_features_json` /
/// `out_features_json_len`; `{}` means no features).
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. Scalar output pointers must each be valid for
/// one value, `out_data_hash` must be valid for 32 bytes, and `out_percentiles` must be valid for
/// writing one pointer. The returned percentile buffer must be released exactly once with
/// `castore_buffer_free_f64` using the returned length. The `out_units` and `out_features_json` caller
/// buffers, when non-null, must be valid for `units_cap` and `features_json_cap` bytes; the
/// `out_element_shape` buffer, when non-null, must be valid for `element_shape_cap` `u64` values;
/// every `*_len` out-pointer must be valid for one `u64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_probabilistic_metadata(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_length: *mut u64,
    out_data_hash: *mut u8,
    out_percentiles: *mut *mut f64,
    out_percentiles_len: *mut u64,
    out_units: *mut c_char,
    units_cap: u64,
    out_units_len: *mut u64,
    out_element_shape: *mut u64,
    element_shape_cap: u64,
    out_element_shape_len: *mut u64,
    out_features_json: *mut c_char,
    features_json_cap: u64,
    out_features_json_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_percentiles.is_null()
        || out_percentiles_len.is_null()
        || out_units_len.is_null()
        || out_element_shape_len.is_null()
        || out_features_json_len.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            4, // Probabilistic
            resolution,
            interval,
            features_json,
        )
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    let meta = match store.inner.get_metadata(&key) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let initial_ms = match meta.initial_timestamp.and_then(datetime_to_unix_ms) {
        Some(n) => n,
        None => {
            set_error("forecast metadata missing initial_timestamp");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    let mut pct: Vec<f64> = meta.percentiles.unwrap_or_default();
    let pct_len = pct.len() as u64;
    let pct_ptr = pct.as_mut_ptr();
    std::mem::forget(pct);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution = opt_period_cstr(meta.resolution);
        *out_horizon = opt_period_cstr(meta.horizon);
        *out_interval = opt_period_cstr(meta.interval);
        *out_count = meta.count.unwrap_or(0) as u64;
        *out_length = meta.length.unwrap_or(0) as u64;
        ptr::copy_nonoverlapping(meta.data_hash.as_ptr(), out_data_hash, 32);
        *out_percentiles = pct_ptr;
        *out_percentiles_len = pct_len;
        write_str_out(
            meta.units.as_deref().unwrap_or(""),
            out_units,
            units_cap,
            out_units_len,
        );
        let shape: Vec<u64> = meta.element_shape.iter().map(|&d| d as u64).collect();
        write_u64s_out(
            &shape,
            out_element_shape,
            element_shape_cap,
            out_element_shape_len,
        );
        write_str_out(
            &features_to_json(&meta.features),
            out_features_json,
            features_json_cap,
            out_features_json_len,
        );
    }
    CASTORE_OK
}

/// Read forecast metadata by attributes. Out-params receive initial timestamp,
/// resolution, horizon, interval, count, the stored array length, the 32-byte
/// content hash (into `out_data_hash`), the extension payload and units
/// string via probe-then-fetch (`ext_buf` / `out_ext_len` and
/// `out_units` / `out_units_len`; an empty string means the field is unset),
/// the per-timestep element shape via probe-then-fetch (`out_element_shape` /
/// `out_element_shape_len`; length 0 means scalar elements), and the features
/// as a JSON object string via probe-then-fetch (`out_features_json` /
/// `out_features_json_len`; `{}` means no features).
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. `interval`, when non-null, is the
/// ISO-8601 forecast interval (part of the identity); pass null to leave it unconstrained.
/// Scalar output pointers must each be valid for
/// one value and `out_data_hash` must be valid for 32 bytes. The `ext_buf`, `out_units`,
/// and `out_features_json` caller buffers, when non-null, must be valid for `ext_cap`,
/// `units_cap`, and `features_json_cap` bytes; the `out_element_shape` buffer, when non-null, must
/// be valid for `element_shape_cap` `u64` values; every `*_len` out-pointer must be valid for one
/// `u64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_forecast_metadata(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_length: *mut u64,
    out_data_hash: *mut u8,
    ext_buf: *mut c_char,
    ext_cap: u64,
    out_ext_len: *mut u64,
    out_units: *mut c_char,
    units_cap: u64,
    out_units_len: *mut u64,
    out_element_shape: *mut u64,
    element_shape_cap: u64,
    out_element_shape_len: *mut u64,
    out_features_json: *mut c_char,
    features_json_cap: u64,
    out_features_json_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_horizon.is_null()
        || out_interval.is_null()
        || out_count.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_ext_len.is_null()
        || out_units_len.is_null()
        || out_element_shape_len.is_null()
        || out_features_json_len.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let mut key = match unsafe {
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
        Ok(k) => k,
        Err(c) => return c,
    };
    key.interval = match unsafe { cstr_to_optional_period(interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    let meta = match store.inner.get_metadata(&key) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let initial_ms = match meta.initial_timestamp.and_then(datetime_to_unix_ms) {
        Some(n) => n,
        None => {
            set_error("forecast metadata missing initial_timestamp");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution = opt_period_cstr(meta.resolution);
        *out_horizon = opt_period_cstr(meta.horizon);
        *out_interval = opt_period_cstr(meta.interval);
        *out_count = meta.count.unwrap_or(0) as u64;
        *out_length = meta.length.unwrap_or(0) as u64;
        ptr::copy_nonoverlapping(meta.data_hash.as_ptr(), out_data_hash, 32);
        write_str_out(
            meta.ext.as_deref().unwrap_or(""),
            ext_buf,
            ext_cap,
            out_ext_len,
        );
        write_str_out(
            meta.units.as_deref().unwrap_or(""),
            out_units,
            units_cap,
            out_units_len,
        );
        let shape: Vec<u64> = meta.element_shape.iter().map(|&d| d as u64).collect();
        write_u64s_out(
            &shape,
            out_element_shape,
            element_shape_cap,
            out_element_shape_len,
        );
        write_str_out(
            &features_to_json(&meta.features),
            out_features_json,
            features_json_cap,
            out_features_json_len,
        );
    }
    CASTORE_OK
}

/// Fetch a forecast by attributes and return the full data array plus metadata.
///
/// `ts_type` is a read request: a concrete type (`2`=Deterministic,
/// `3`=DeterministicSingleTimeSeries, `4`=Probabilistic, `5`=Scenarios) or the
/// `CASTORE_TYPE_ABSTRACT_DETERMINISTIC` (`100`) family, which matches a stored
/// `Deterministic` *or* `DeterministicSingleTimeSeries`. The catalog resolves the
/// family authoritatively — no client-side guess-and-retry — and writes the
/// concrete type that matched to `*out_matched_type`. An ambiguous family request
/// (both concrete types share the identity) returns `CASTORE_ERR_INVALID_PARAMETER`;
/// a genuine miss returns the unmasked not-found error.
///
/// Reads a `Deterministic`, `Probabilistic`, or `Scenarios` forecast (DST is
/// synthesized into `Deterministic`). On success, the caller owns two heap
/// buffers and must free them with the matching deallocators:
///
/// - `*out_data` (byte buffer, `*out_data_byte_len` bytes) —
///   free with `castore_buffer_free_u8(*out_data, *out_data_byte_len)`.
/// - `*out_dims` (array of `u64`, `*out_ndims` elements) —
///   free with `castore_buffer_free_u64(*out_dims, *out_ndims)`.
/// - `*out_percentiles` (`f64` array, `*out_percentiles_len` elements) —
///   non-NULL only for `Probabilistic`; free with
///   `castore_buffer_free_f64(*out_percentiles, *out_percentiles_len)`.
///
/// **Optional time-range / window selection:** when `time_range_present` is
/// `true`, only the windows whose start timestamp falls in
/// `[time_range_start_ms, time_range_end_ms)` are returned. Pass
/// `time_range_present = false` to retrieve all windows.
///
/// # Safety
///
/// - `handle` must be a live, non-null store handle created by this library.
///   No concurrent mutation is permitted for the duration of the call.
/// - `owner_id` and `owner_category` (`0` = Component, `1` = SupplementalAttribute)
///   identify the owner. `name` must point to a valid, null-terminated
///   UTF-8 string for the duration of the call; `features_json` may be null.
/// - `resolution` and `interval`, when non-null, must be valid null-terminated
///   UTF-8 ISO-8601 durations; either may be null to leave that part of the
///   identity unconstrained (the catalog reports an error if the request is
///   then ambiguous).
/// - All `out_*` scalar pointers, including `out_matched_type`, must be valid
///   for writing one value each.
/// - `out_dims` must be valid for writing one pointer; the returned pointer
///   must be freed exactly once with `castore_buffer_free_u64` using `*out_ndims`.
/// - `out_data` must be valid for writing one pointer; the returned pointer
///   must be freed exactly once with `castore_buffer_free_u8` using
///   `*out_data_byte_len`.
/// - `out_percentiles` must be valid for writing one pointer; when the result
///   is not `Probabilistic` the pointer is set to null and `*out_percentiles_len`
///   to 0, so no free is needed. When non-null it must be freed exactly once
///   with `castore_buffer_free_f64` using `*out_percentiles_len`.
/// - All returned heap buffers are invalidated after their matching free call
///   and must not be used afterwards.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_forecast(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
    // scalar metadata outputs
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_scenario_count: *mut u64, // Scenarios only; 0 for other types
    // array shape outputs (dims buffer)
    out_ndims: *mut u64,
    out_dims: *mut *mut u64,
    // raw byte output
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    // percentiles (Probabilistic only; null + 0 for other types)
    out_percentiles: *mut *mut f64,
    out_percentiles_len: *mut u64,
    // the concrete type that was matched (e.g. an AbstractDeterministic request
    // resolves to 2=Deterministic or 3=DeterministicSingleTimeSeries)
    out_matched_type: *mut i32,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    // Null-check all output pointers.
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
        || out_matched_type.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let requested = match requested_type_from_int(ts_type) {
        Some(r) => r,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    // Parse the addressing attributes (the key's type field is unused here; the
    // catalog decides the concrete type via `resolve_forecast_key`).
    let attrs = match unsafe {
        build_key_from_attrs(owner_id, owner_category, name, resolution, features_json)
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    let interval = match unsafe { cstr_to_optional_period(interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    let key = match store.inner.resolve_forecast_key(
        attrs.owner_id,
        attrs.owner_category,
        &attrs.name,
        attrs.resolution,
        interval,
        attrs.features,
        requested,
    ) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let matched_type = time_series_type_to_int(key.time_series_type());
    let time_range = if time_range_present {
        let start = match unix_ms_to_datetime(time_range_start_ms) {
            Some(d) => d,
            None => {
                set_error(format!(
                    "invalid time_range_start_ms: {time_range_start_ms}"
                ));
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        let end = match unix_ms_to_datetime(time_range_end_ms) {
            Some(d) => d,
            None => {
                set_error(format!("invalid time_range_end_ms: {time_range_end_ms}"));
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        Some((start, end))
    } else {
        None
    };
    let data = match store.inner.get_time_series(key.identity(), time_range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_matched_type = matched_type;
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

/// Shared emitter: write a forecast `TimeSeriesData` value into the C out-params
/// used by [`castore_store_get_forecast`] and [`castore_store_get_forecast_by_key`].
///
/// # Safety
///
/// All out pointers must be non-null and valid for writing their indicated
/// values (the callers null-check them). The returned `out_dims`, `out_data`,
/// and (for `Probabilistic`) `out_percentiles` buffers are heap-allocated and
/// must be released by the caller with the matching `castore_buffer_free_*` function.
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
            let initial_ms = match datetime_to_unix_ms(det.initial_timestamp) {
                Some(n) => n,
                None => {
                    set_error("initial_timestamp out of i64 millisecond range");
                    return CASTORE_ERR_INTEGRITY;
                }
            };
            let mut dims: Vec<u64> = det.data.shape.iter().map(|&d| d as u64).collect();
            let ndims = dims.len() as u64;
            let dims_ptr = dims.as_mut_ptr();
            std::mem::forget(dims);

            let dtype = det.data.dtype.code();
            let mut bytes = det.data.bytes;
            let byte_len = bytes.len() as u64;
            let data_ptr = bytes.as_mut_ptr();
            std::mem::forget(bytes);

            unsafe {
                *out_initial_ts_unix_ms = initial_ms;
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
            CASTORE_OK
        }
        core_lib::TimeSeriesData::Probabilistic(prob) => {
            let initial_ms = match datetime_to_unix_ms(prob.initial_timestamp) {
                Some(n) => n,
                None => {
                    set_error("initial_timestamp out of i64 millisecond range");
                    return CASTORE_ERR_INTEGRITY;
                }
            };
            let mut dims: Vec<u64> = prob.data.shape.iter().map(|&d| d as u64).collect();
            let ndims = dims.len() as u64;
            let dims_ptr = dims.as_mut_ptr();
            std::mem::forget(dims);

            let dtype = prob.data.dtype.code();
            let mut bytes = prob.data.bytes;
            let byte_len = bytes.len() as u64;
            let data_ptr = bytes.as_mut_ptr();
            std::mem::forget(bytes);

            let mut pct = prob.percentiles;
            let pct_len = pct.len() as u64;
            let pct_ptr = pct.as_mut_ptr();
            std::mem::forget(pct);

            unsafe {
                *out_initial_ts_unix_ms = initial_ms;
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
            CASTORE_OK
        }
        core_lib::TimeSeriesData::Scenarios(scen) => {
            let initial_ms = match datetime_to_unix_ms(scen.initial_timestamp) {
                Some(n) => n,
                None => {
                    set_error("initial_timestamp out of i64 millisecond range");
                    return CASTORE_ERR_INTEGRITY;
                }
            };
            let scenario_count = scen.scenario_count;

            let mut dims: Vec<u64> = scen.data.shape.iter().map(|&d| d as u64).collect();
            let ndims = dims.len() as u64;
            let dims_ptr = dims.as_mut_ptr();
            std::mem::forget(dims);

            let dtype = scen.data.dtype.code();
            let mut bytes = scen.data.bytes;
            let byte_len = bytes.len() as u64;
            let data_ptr = bytes.as_mut_ptr();
            std::mem::forget(bytes);

            unsafe {
                *out_initial_ts_unix_ms = initial_ms;
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
            CASTORE_OK
        }
        other => {
            set_error(format!(
                "key identifies a {} time series; use the matching read function",
                other.time_series_type().as_str()
            ));
            CASTORE_ERR_INVALID_PARAMETER
        }
    }
}

/// Fetch a forecast (`Deterministic` / `Probabilistic` / `Scenarios`, or a
/// `DeterministicSingleTimeSeries` synthesized into a `Deterministic`) by key.
///
/// This is the key-based counterpart to [`castore_store_get_forecast`]: the time
/// series type comes from `key` rather than an explicit `ts_type` argument. The
/// outputs and buffer-ownership rules are identical to [`castore_store_get_forecast`];
/// `*out_matched_type` is set from the key's type (no family resolution needed
/// because the key already names the concrete type).
///
/// # Safety
///
/// - `handle` and `key` must be live handles created by this library; no
///   concurrent mutation is permitted for the duration of the call.
/// - All `out_*` scalar pointers, including `out_matched_type`, must be valid
///   for writing one value each.
/// - `out_dims` must be valid for writing one pointer; the returned pointer must
///   be freed exactly once with `castore_buffer_free_u64` using `*out_ndims`.
/// - `out_data` must be valid for writing one pointer; the returned pointer must
///   be freed exactly once with `castore_buffer_free_u8` using `*out_data_byte_len`.
/// - `out_percentiles` must be valid for writing one pointer; when the result is
///   not `Probabilistic` the pointer is set to null and `*out_percentiles_len`
///   to 0, so no free is needed. When non-null it must be freed exactly once
///   with `castore_buffer_free_f64` using `*out_percentiles_len`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_get_forecast_by_key(
    handle: *const CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
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
    // the concrete type read (taken from the key; provided for symmetry with
    // `castore_store_get_forecast`)
    out_matched_type: *mut i32,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return CASTORE_ERR_NULL_POINTER;
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
        || out_matched_type.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let matched_type = time_series_type_to_int(key.inner.time_series_type);
    let time_range = if time_range_present {
        let start = match unix_ms_to_datetime(time_range_start_ms) {
            Some(d) => d,
            None => {
                set_error(format!(
                    "invalid time_range_start_ms: {time_range_start_ms}"
                ));
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        let end = match unix_ms_to_datetime(time_range_end_ms) {
            Some(d) => d,
            None => {
                set_error(format!("invalid time_range_end_ms: {time_range_end_ms}"));
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        Some((start, end))
    } else {
        None
    };
    let data = match store.inner.get_time_series(&key.inner, time_range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        *out_matched_type = matched_type;
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

/// Construct a `TimeSeriesKey` handle from attributes `(owner_id, name,
/// ts_type, resolution, features)`.
///
/// The returned key can be passed to the key-based read functions (e.g.
/// [`castore_store_get_single`], [`castore_store_get_non_sequential`],
/// [`castore_store_get_forecast_by_key`]); it lets an attribute-addressed caller
/// reuse the key-based read path without an `add`/lookup round trip.
/// an empty `resolution` means "unspecified"; likewise an empty/null `interval`
/// leaves the forecast interval (part of the identity) unconstrained.
///
/// # Safety
///
/// `owner_id` and `owner_category` (`0` = Component, `1` = SupplementalAttribute)
/// identify the owner. `name` must point to a valid, null-terminated
/// UTF-8 string. `features_json`, when non-null, must be a null-terminated UTF-8
/// JSON object. `out_key` must be valid for writing one pointer. The returned key
/// must be released exactly once with `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_make_key_from_attrs(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let mut key = match unsafe {
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
        Ok(k) => k,
        Err(c) => return c,
    };
    key.interval = match unsafe { cstr_to_optional_period(interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    let handle = Box::new(CastoreKeyHandle { inner: key });
    unsafe { *out_key = Box::into_raw(handle) };
    CASTORE_OK
}

/// List every time series key associated with `owner_id`. On success
/// `*out_keys` points to an array of `*out_len` owned key handles (one per
/// association, including derived `DeterministicSingleTimeSeries` rows), each
/// usable with the key-based read functions.
///
/// Ownership is two-tiered: free every individual `CastoreKey` with `castore_key_free`,
/// then free the array buffer itself with `castore_keys_buffer_free`. When the owner
/// has no series, `*out_keys` is set to null and `*out_len` to 0 (no free
/// needed).
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner.
/// `out_keys` must be valid for writing one pointer and `out_len` for writing one
/// `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_time_series_keys(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    out_keys: *mut *mut *mut CastoreKeyHandle,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_keys.is_null() || out_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let keys = match store.inner.get_time_series_keys(owner_id, category) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let mut handles: Vec<*mut CastoreKeyHandle> = keys
        .into_iter()
        .map(|k| {
            Box::into_raw(Box::new(CastoreKeyHandle {
                inner: k.identity().clone(),
            }))
        })
        .collect();
    // Keep capacity == length so `castore_keys_buffer_free` can reconstruct the Vec.
    handles.shrink_to_fit();
    let len = handles.len() as u64;
    let ptr = if handles.is_empty() {
        ptr::null_mut()
    } else {
        let p = handles.as_mut_ptr();
        std::mem::forget(handles);
        p
    };
    unsafe {
        *out_keys = ptr;
        *out_len = len;
    }
    CASTORE_OK
}

/// Encode metadata rows as a JSON array string. Each element carries the
/// association's owner + addressing fields and the temporal parameters the
/// binding needs to reconstruct a `TimeSeriesMetadata`. Durations are emitted
/// as ISO-8601 duration strings (e.g. `PT1H`), `initial_timestamp_ms` as Unix
/// epoch milliseconds, and `data_hash` as a byte array; absent optionals are
/// `null`.
// Serialize keys to a JSON array. Each object carries the identity tuple
// (`owner_id`, `owner_category`, `time_series_type`, `name`, `resolution`,
// `features`) plus the per-variant descriptive snapshot. Physical storage detail
// (`data_hash`, `dtype`, `ext`, `percentiles`) is deliberately absent —
// it is read on demand via the metadata read descriptors.
/// Build the JSON object for one key (the per-row shape shared by
/// `keys_to_json` and `keys_with_hash_to_json`).
fn key_to_map(k: &core_lib::TimeSeriesKey) -> serde_json::Map<String, Value> {
    let dur_ms = |d: Option<core_lib::Period>| -> Value {
        d.map(|x| Value::from(x.to_iso8601()))
            .unwrap_or(Value::Null)
    };
    let id = k.identity();
    let mut o = serde_json::Map::new();
    o.insert("owner_id".into(), Value::from(id.owner_id));
    o.insert(
        "owner_category".into(),
        Value::from(id.owner_category.as_str()),
    );
    o.insert(
        "time_series_type".into(),
        Value::from(id.time_series_type.as_str()),
    );
    o.insert("name".into(), Value::from(id.name.clone()));
    o.insert("resolution".into(), dur_ms(id.resolution));
    o.insert(
        "features".into(),
        serde_json::from_str(&features_to_json(&id.features))
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
    );
    // Per-variant descriptive snapshot.
    let (initial_timestamp, length, horizon, interval, count) = match k {
        core_lib::TimeSeriesKey::Single(s) => {
            (Some(s.initial_timestamp), Some(s.length), None, None, None)
        }
        core_lib::TimeSeriesKey::NonSequential(s) => (None, Some(s.length), None, None, None),
        core_lib::TimeSeriesKey::Forecast(f) => (
            Some(f.initial_timestamp),
            None,
            Some(f.horizon),
            Some(f.interval()),
            Some(f.count),
        ),
    };
    o.insert(
        "initial_timestamp_ms".into(),
        initial_timestamp
            .and_then(datetime_to_unix_ms)
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    o.insert(
        "length".into(),
        length.map(|l| Value::from(l as u64)).unwrap_or(Value::Null),
    );
    o.insert("horizon".into(), dur_ms(horizon));
    o.insert("interval".into(), dur_ms(interval));
    o.insert(
        "count".into(),
        count.map(|c| Value::from(c as u64)).unwrap_or(Value::Null),
    );
    o
}

fn keys_to_json(keys: &[core_lib::TimeSeriesKey]) -> String {
    let arr: Vec<Value> = keys.iter().map(|k| Value::Object(key_to_map(k))).collect();
    Value::Array(arr).to_string()
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

/// Like `keys_to_json`, but each row carries an extra `data_hash` field (the
/// lowercase hex content hash). Rows that share a stored array share the hash.
fn keys_with_hash_to_json(rows: &[(core_lib::TimeSeriesKey, [u8; 32])]) -> String {
    let arr: Vec<Value> = rows
        .iter()
        .map(|(k, h)| {
            let mut o = key_to_map(k);
            o.insert("data_hash".into(), Value::from(hash_to_hex(h)));
            Value::Object(o)
        })
        .collect();
    Value::Array(arr).to_string()
}

/// Full-metadata JSON object for one association row: the identity/descriptive
/// key fields plus the physical-storage columns a key row omits (`data_hash`
/// hex, `dtype`, `element_shape`, `percentiles`, `units`, `ext`).
/// Periods are ISO-8601 strings; `initial_timestamp_ms` is Unix milliseconds.
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
    o.insert(
        "initial_timestamp_ms".into(),
        m.initial_timestamp
            .and_then(datetime_to_unix_ms)
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
    o.insert("dtype".into(), Value::from(m.dtype.as_str()));
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
        "ext".into(),
        m.ext.clone().map(Value::from).unwrap_or(Value::Null),
    );
    o
}

/// List time series keys as a JSON array string (see `keys_to_json` for the
/// per-key shape). Every filter is optional and independent; with none set the
/// whole store is listed. A `has_*` flag of `false` (or a null string pointer)
/// disables that filter:
/// - `owner_id` / `owner_category` (`0` = Component, `1` = SupplementalAttribute)
/// - `time_series_type` (the `CASTORE_TYPE_*` code)
/// - `name` (null = no name filter)
/// - `resolution` (empty/null = no resolution filter)
/// - `features_json` (a JSON object; null or empty = no feature filter; matches as
///   a subset, i.e. a key whose features include all the given ones)
///
/// Follows the probe-then-fetch convention: call with `buf` null and `cap` 0 to
/// learn the byte length via `out_len`, then call again with a buffer of at
/// least `len + 1` bytes. The string is NUL-terminated and truncated to `cap`;
/// `out_len` is always the untruncated byte length.
///
/// # Safety
///
/// `handle` must be a live store handle. The scalar filter flags/values are plain
/// scalars. `name` and `features_json` must each be null or a null-terminated
/// UTF-8 string. `out_len` must be writable; `buf` must be null or valid for
/// `cap` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_list_keys(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let keys = match store.inner.list_keys(filter) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let json = keys_to_json(&keys);
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// List full time-series metadata rows as a JSON array (see `metadata_to_map`
/// for the per-row shape: the key fields plus `data_hash`, `dtype`,
/// `element_shape`, `percentiles`, `units`, and `ext`). Filters and the
/// probe-then-fetch buffer convention match `castore_store_list_keys`.
///
/// # Safety
///
/// Identical to `castore_store_list_keys`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_list_time_series(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let rows = match store.inner.list_time_series(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let arr: Vec<Value> = rows
        .iter()
        .map(|m| Value::Object(metadata_to_map(m)))
        .collect();
    let json = Value::Array(arr).to_string();
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// List the distinct series names matching the filter as a JSON array of strings
/// (sorted). Filters and the probe-then-fetch convention match
/// `castore_store_list_keys`.
///
/// # Safety
///
/// Identical to `castore_store_list_keys`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_list_names(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
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
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// List the distinct owner types matching the filter as a JSON array of strings
/// (sorted). Filters and the probe-then-fetch convention match
/// `castore_store_list_keys`.
///
/// # Safety
///
/// Identical to `castore_store_list_keys`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_list_owner_types(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
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
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// Remove every time series matching the filter in one all-or-nothing
/// transaction, writing the number removed into `*out_removed`. Filters match
/// `castore_store_list_keys`; an empty match removes nothing (`0`).
///
/// # Safety
///
/// `handle` must be a live mutable store handle; the filter args match
/// `castore_store_list_keys`. `out_removed` must be valid for writing one `u64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_remove_by_filter(
    handle: *mut CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_removed: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    if out_removed.is_null() {
        set_error("out_removed is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.remove_by_filter(filter) {
        Ok(n) => {
            unsafe { *out_removed = n as u64 };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Rename the series identified by `key` to `new_name`, returning the renamed
/// key through `out_key` (same identity, new name). Only the catalog name
/// changes; the array is untouched. `CASTORE_ERR_NOT_FOUND` if the key matches
/// nothing, or a duplicate error if the new identity already exists.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `key` a live key handle.
/// `new_name` must be null-terminated UTF-8. `out_key` must be valid for writing
/// one pointer; the returned key must be released with `castore_key_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_rename(
    handle: *mut CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    new_name: *const c_char,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let new_name = match unsafe { cstr_to_str(new_name) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    match store.inner.rename_time_series(&key.inner, new_name) {
        Ok(new_key) => {
            unsafe {
                *out_key = Box::into_raw(Box::new(CastoreKeyHandle {
                    inner: new_key.identity().clone(),
                }))
            };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Resolve a forecast addressed by attributes plus a requested type to its
/// concrete key, returned through `out_key`. `requested_type` is a concrete
/// forecast code (`2`=Deterministic, `3`=DeterministicSingleTimeSeries,
/// `4`=Probabilistic, `5`=Scenarios) or `CASTORE_TYPE_ABSTRACT_DETERMINISTIC` (`100`),
/// which matches a stored `Deterministic` *or* `DeterministicSingleTimeSeries`.
/// `resolution` / `interval`, when non-null, narrow the identity. An ambiguous
/// request returns `CASTORE_ERR_INVALID_PARAMETER`; a miss returns `CASTORE_ERR_NOT_FOUND`.
///
/// # Safety
///
/// `handle` must be a live store handle. `name` must be null-terminated UTF-8;
/// `resolution`, `interval`, and `features_json` may be null. `out_key` must be
/// valid for writing one pointer; the returned key must be released with
/// `castore_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_resolve_forecast_key(
    handle: *const CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    requested_type: i32,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let resolution = match unsafe { cstr_to_optional_period(resolution) } {
        Ok(r) => r,
        Err(c) => return c,
    };
    let interval = match unsafe { cstr_to_optional_period(interval) } {
        Ok(i) => i,
        Err(c) => return c,
    };
    let features = match unsafe { parse_features_json(features_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let requested = match requested_type_from_int(requested_type) {
        Some(r) => r,
        None => {
            set_error(format!("invalid requested forecast type {requested_type}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.resolve_forecast_key(
        owner_id, category, name, resolution, interval, features, requested,
    ) {
        Ok(key) => {
            unsafe {
                *out_key = Box::into_raw(Box::new(CastoreKeyHandle {
                    inner: key.identity().clone(),
                }))
            };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Build a [`core_lib::ListFilter`] from the optional scalar/string filter args
/// shared by `castore_store_list_keys` and `castore_store_list_array_groups`. On a bad
/// argument it sets the thread-local error (where appropriate) and returns the
/// error code to propagate.
///
/// # Safety
///
/// `name` and `features_json` must each be null or a null-terminated UTF-8
/// string; `resolution` must be null or a null-terminated ISO-8601 period.
#[allow(clippy::too_many_arguments)]
unsafe fn build_list_filter(
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
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
                return Err(CASTORE_ERR_INVALID_PARAMETER);
            }
        };
        filter = filter.owner_category(category);
    }
    if has_time_series_type {
        match time_series_type_from_int(time_series_type) {
            Some(t) => filter = filter.time_series_type(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return Err(CASTORE_ERR_INVALID_PARAMETER);
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
    match unsafe { cstr_to_optional_period(resolution) } {
        Ok(Some(p)) => filter = filter.resolution(p),
        Ok(None) => {}
        Err(c) => return Err(c),
    }
    let features = unsafe { parse_features_json(features_json) }?;
    if !features.is_empty() {
        filter = filter.features(features);
    }
    Ok(filter)
}

/// List time series keys, each annotated with the hex content hash of the array
/// it resolves to, as a JSON array string (see `keys_with_hash_to_json` for the
/// per-row shape — `keys_to_json`'s shape plus a `data_hash` field). Rows that
/// share a stored array share their `data_hash`, so a caller can group time
/// series by their underlying data in one query (no per-row metadata fetch).
///
/// Filters and the probe-then-fetch buffer convention are identical to
/// `castore_store_list_keys`.
///
/// # Safety
///
/// Identical to `castore_store_list_keys`: `handle` must be a live store handle;
/// `name` / `features_json` / `resolution` must each be null or a
/// null-terminated UTF-8 string; `out_len` must be writable; `buf` must be null
/// or valid for `cap` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_list_array_groups(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
            resolution,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let rows = match store.inner.list_keys_with_hash(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    let json = keys_with_hash_to_json(&rows);
    unsafe { write_str_out(&json, buf, cap, out_len) };
    CASTORE_OK
}

/// Free the key-handle array returned by `castore_store_get_time_series_keys`.
///
/// This releases only the array buffer, not the keys it held: transfer each
/// `CastoreKey` out first (the Julia binding wraps each in a finalized object) and
/// release them individually with `castore_key_free`.
///
/// # Safety
///
/// `ptr` must be null or an array returned by `castore_store_get_time_series_keys`
/// with exactly `len` elements, not previously freed. It must not be used after
/// this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_keys_buffer_free(ptr: *mut *mut CastoreKeyHandle, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
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

/// Read the attributes of a key handle: its time series type code (see
/// `time_series_type_from_int`), resolution as an owned ISO-8601 duration C string (null
/// when unset; free with `castore_string_free`), the owner
/// id (an integer), the name string, and the features as a JSON object string
/// (`"{}"` when empty — the same shape the attribute-addressed entry points
/// accept).
///
/// `out_owner_id` receives the owner id directly. The `name` and `features`
/// strings follow the probe-then-fetch convention: call with `name_buf` /
/// `features_buf` null (and capacities `0`) to learn the required lengths via the
/// matching `out_*_len`, then call again with buffers of at least `len + 1`
/// bytes. Each returned string is NUL-terminated and truncated to its capacity;
/// the reported length is always the untruncated byte length.
///
/// # Safety
///
/// `key` must be a live key handle created by this library. `out_type`,
/// `out_resolution`, `out_owner_id`, `out_owner_category`, `out_name_len`, and
/// `out_features_len` must each be valid for writing one value. `out_owner_category`
/// receives `0` (Component) or `1` (SupplementalAttribute). `name_buf` /
/// `features_buf` may be null; when non-null they must be valid for writing
/// `name_cap` / `features_cap` bytes respectively.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_key_attributes(
    key: *const CastoreKeyHandle,
    out_type: *mut i32,
    out_resolution: *mut *mut c_char,
    out_owner_id: *mut i64,
    out_owner_category: *mut i32,
    name_buf: *mut c_char,
    name_cap: u64,
    out_name_len: *mut u64,
    features_buf: *mut c_char,
    features_cap: u64,
    out_features_len: *mut u64,
) -> i32 {
    clear_error();
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_type.is_null()
        || out_resolution.is_null()
        || out_owner_id.is_null()
        || out_owner_category.is_null()
        || out_name_len.is_null()
        || out_features_len.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let k = &key.inner;
    let category_code = match k.owner_category {
        core_lib::OwnerCategory::Component => 0,
        core_lib::OwnerCategory::SupplementalAttribute => 1,
    };
    unsafe {
        *out_type = time_series_type_to_int(k.time_series_type);
        *out_resolution = opt_period_cstr(k.resolution);
        *out_owner_id = k.owner_id;
        *out_owner_category = category_code;
        write_str_out(&k.name, name_buf, name_cap, out_name_len);
        write_str_out(
            &features_to_json(&k.features),
            features_buf,
            features_cap,
            out_features_len,
        );
    }
    CASTORE_OK
}

/// Read an association's `name` by key, resolved through the stored metadata
/// (`Store::get_metadata`). This surfaces the per-association `name` that is not
/// carried on the key itself — the read path uses it to populate the returned
/// time series object.
///
/// `name` uses the probe-then-fetch convention (see [`castore_key_attributes`]).
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library.
/// `out_name_len` must be valid for writing one `u64`. `name_buf` may be null;
/// when non-null it must be valid for writing `name_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_get_association(
    handle: *const CastoreStoreHandle,
    key: *const CastoreKeyHandle,
    name_buf: *mut c_char,
    name_cap: u64,
    out_name_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_name_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let meta = match store.inner.get_metadata(&key.inner) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        write_str_out(&meta.name, name_buf, name_cap, out_name_len);
    }
    CASTORE_OK
}

/// Release a `u64` dims buffer returned by `castore_store_get_forecast`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_buffer_free_u64(ptr: *mut u64, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

/// True iff a time series of `ts_type` with the given attributes exists.
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. `out_present` must be valid for writing one
/// `bool`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_has_typed(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
    }
    let key = match unsafe {
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
        Ok(k) => k,
        Err(c) => return c,
    };
    match store.inner.has_time_series(&key) {
        Ok(b) => {
            unsafe { *out_present = b };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Remove a time series of `ts_type` by attributes.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` and `owner_category`
/// (`0` = Component, `1` = SupplementalAttribute) identify the owner. Required strings
/// must be null-terminated UTF-8, and `features_json` may be null.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_remove_typed(
    handle: *mut CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let key = match unsafe {
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
        Ok(k) => k,
        Err(c) => return c,
    };
    match store.inner.remove_time_series(&key) {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Copy the time series identified by the source attributes onto another owner,
/// optionally under a new name.
///
/// Arrays are content-addressed, so only a new association row is written — no
/// array data is duplicated and the stored time series type is preserved (a
/// `DeterministicSingleTimeSeries` stays one rather than being materialized into
/// a dense `Deterministic`). The copy keeps the source's owner category.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. The `owner_id` / `owner_category`
/// (`0` = Component, `1` = SupplementalAttribute) / `name` / `ts_type` /
/// `resolution` / `features_json` arguments identify the SOURCE series, exactly as
/// for `castore_store_remove_typed`. Required strings must be null-terminated UTF-8;
/// `resolution`, `features_json`, and `new_name` may be null (a null `new_name`
/// keeps the source name).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_copy_time_series(
    handle: *mut CastoreStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    interval: *const c_char,
    features_json: *const c_char,
    dst_owner_id: i64,
    dst_owner_type: *const c_char,
    new_name: *const c_char,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let key = match unsafe {
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
        Ok(k) => k,
        Err(c) => return c,
    };
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
    match store
        .inner
        .copy_time_series(&key, dst_owner_id, dst_type, renamed)
    {
        Ok(_) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Remove all time series, or all for a single owner when `has_owner` is true.
/// Returns `CASTORE_OK` on success.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `has_owner`, `owner_id`, and
/// `owner_category` are plain scalars; when `has_owner` is true `owner_category`
/// (`0` = Component, `1` = SupplementalAttribute) scopes the clear to one owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_clear(
    handle: *mut CastoreStoreHandle,
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
                return CASTORE_ERR_INVALID_PARAMETER;
            }
        };
        Some((owner_id, category))
    } else {
        None
    };
    match store.inner.clear_time_series(owner) {
        Ok(_) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Reassign every time series owned by `old_owner_id` to `new_owner_id`.
/// When `out_updated` is non-null it receives the number of associations
/// changed. Returns `CASTORE_OK` on success.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `old_owner_id` and
/// `new_owner_id` are plain integers; `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) identifies the owner category. When non-null,
/// `out_updated` must point to writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_replace_owner(
    handle: *mut CastoreStoreHandle,
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
            return CASTORE_ERR_INVALID_PARAMETER;
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
            CASTORE_OK
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
// the probe-then-fetch JSON convention used by the other list-returning exports,
// so no new deallocator is introduced.

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
            CASTORE_ERR_INVALID_PARAMETER
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
        CASTORE_ERR_INVALID_PARAMETER
    })
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
            CASTORE_OK
        }
        Err(e) => {
            set_error(e.to_string());
            CASTORE_ERR_INTERNAL
        }
    }
}

// ---- Supplemental-attribute associations ----------------------------------

/// Attach supplemental attribute `(attribute_id, attribute_type)` to component
/// `(component_id, component_type)`. Returns `CASTORE_ERR_DUPLICATE_ASSOCIATION` if
/// that component already carries that attribute, whatever type names are
/// supplied.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `component_type` and
/// `attribute_type` must point to valid, null-terminated UTF-8 strings that stay
/// valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_add_supplemental_attribute_association(
    handle: *mut CastoreStoreHandle,
    component_id: i64,
    component_type: *const c_char,
    attribute_id: i64,
    attribute_type: *const c_char,
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
        },
    ) {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Attach many in one all-or-nothing transaction, from a JSON array of objects
/// with `component_id`, `component_type`, `attribute_id`, and `attribute_type`.
/// This is the import half of the bulk round trip whose export is
/// `castore_store_list_supplemental_attribute_associations` with a null filter. When
/// non-null, `out_added` receives the number inserted.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `associations_json` a valid,
/// null-terminated UTF-8 string. When non-null, `out_added` must point to
/// writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_add_supplemental_attribute_associations(
    handle: *mut CastoreStoreHandle,
    associations_json: *const c_char,
    out_added: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let assocs: Vec<core_lib::SupplementalAttributeAssociation> =
        match unsafe { assoc_rows_from_json(associations_json) } {
            Ok(v) => v,
            Err(c) => return c,
        };
    match store.inner.add_supplemental_attribute_associations(assocs) {
        Ok(n) => {
            if !out_added.is_null() {
                unsafe { *out_added = n as u64 };
            }
            CASTORE_OK
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
pub unsafe extern "C" fn castore_store_has_supplemental_attribute_association(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    out_found: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_found.is_null() {
        set_error("out_found is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store.inner.has_supplemental_attribute_association(&filter) {
        Ok(found) => {
            unsafe { *out_found = found };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attachments matching `filter_json` as a JSON array, in insertion order. Each
/// object carries `component_id`, `component_type`, `attribute_id`, and
/// `attribute_type`. Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_len` must be writable; `buf` must be null or
/// valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_supplemental_attribute_associations(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::SupplementalAttributeFilter =
        match unsafe { assoc_filter_from_json(filter_json) } {
            Ok(f) => f,
            Err(c) => return c,
        };
    match store
        .inner
        .list_supplemental_attribute_associations(&filter)
    {
        Ok(rows) => unsafe { write_json_out(&rows, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

/// Distinct attribute ids matching `filter_json`, ascending, as a JSON array —
/// the attributes attached to a component when `component_id` is set.
/// Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_len` must be writable; `buf` must be null or
/// valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_supplemental_attribute_ids(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_len` must be writable; `buf` must be null or
/// valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_components_with_attributes(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
/// `handle` must be a live mutable store handle and `filter_json` null or valid
/// null-terminated UTF-8. When non-null, `out_removed` must point to writable
/// `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_remove_supplemental_attribute_associations(
    handle: *mut CastoreStoreHandle,
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
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Move every attachment from component `old_id` to `new_id`. When non-null,
/// `out_updated` receives the rows changed. Returns
/// `CASTORE_ERR_DUPLICATE_ASSOCIATION` if `new_id` already carries one of the
/// attributes being moved.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. When non-null, `out_updated`
/// must point to writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_replace_supplemental_attribute_component_id(
    handle: *mut CastoreStoreHandle,
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
            CASTORE_OK
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
pub unsafe extern "C" fn castore_store_count_supplemental_attribute_associations(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    kind: i32,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return CASTORE_ERR_NULL_POINTER;
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
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match counted {
        Ok(n) => {
            unsafe { *out_count = n };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Attachment counts grouped by attribute type as a JSON array of
/// `{"type": …, "count": …}` objects, ordered by type. Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must
/// be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_supplemental_attribute_counts_by_type(
    handle: *const CastoreStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
    CASTORE_OK
}

/// Attachment counts grouped by both type names as a JSON array of
/// `{"component_type": …, "attribute_type": …, "count": …}` objects, ordered by
/// attribute type then component type. Probe-then-fetch.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must
/// be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_supplemental_attribute_summary(
    handle: *const CastoreStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    match store.inner.supplemental_attribute_summary() {
        Ok(rows) => unsafe { write_json_out(&rows, buf, cap, out_len) },
        Err(e) => map_core_error(e),
    }
}

// ---- Parent/child associations --------------------------------------------

/// Record a directed edge from component `(parent_id, parent_type)` to component
/// `(child_id, child_type)`. Returns `CASTORE_ERR_DUPLICATE_ASSOCIATION` if that
/// ordered pair is already related; the reversed pair is a different edge.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `parent_type` and `child_type`
/// must point to valid, null-terminated UTF-8 strings that stay valid for the
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_add_parent_child_association(
    handle: *mut CastoreStoreHandle,
    parent_id: i64,
    parent_type: *const c_char,
    child_id: i64,
    child_type: *const c_char,
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
        }) {
        Ok(()) => CASTORE_OK,
        Err(e) => map_core_error(e),
    }
}

/// Record many edges in one all-or-nothing transaction, from a JSON array of
/// objects with `parent_id`, `parent_type`, `child_id`, and `child_type`. When
/// non-null, `out_added` receives the number inserted.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `associations_json` a valid,
/// null-terminated UTF-8 string. When non-null, `out_added` must point to
/// writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_add_parent_child_associations(
    handle: *mut CastoreStoreHandle,
    associations_json: *const c_char,
    out_added: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(mut handle);
    let assocs: Vec<core_lib::ParentChildAssociation> =
        match unsafe { assoc_rows_from_json(associations_json) } {
            Ok(v) => v,
            Err(c) => return c,
        };
    match store.inner.add_parent_child_associations(assocs) {
        Ok(n) => {
            if !out_added.is_null() {
                unsafe { *out_added = n as u64 };
            }
            CASTORE_OK
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
pub unsafe extern "C" fn castore_store_has_parent_child_association(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    out_found: *mut bool,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_found.is_null() {
        set_error("out_found is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.has_parent_child_association(&filter) {
        Ok(found) => {
            unsafe { *out_found = found };
            CASTORE_OK
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
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_len` must be writable; `buf` must be null or
/// valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_parent_child_associations(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_len.is_null() {
        set_error("out_len is null");
        return CASTORE_ERR_NULL_POINTER;
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
/// `handle` must be a live store handle and `filter_json` null or valid
/// null-terminated UTF-8. `out_len` must be writable; `buf` must be null or
/// valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_list_parent_child_ids(
    handle: *const CastoreStoreHandle,
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
        return CASTORE_ERR_NULL_POINTER;
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
            return CASTORE_ERR_INVALID_PARAMETER;
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
/// `handle` must be a live mutable store handle and `filter_json` null or valid
/// null-terminated UTF-8. When non-null, `out_removed` must point to writable
/// `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_remove_parent_child_associations(
    handle: *mut CastoreStoreHandle,
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
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Rewrite component `old_id` to `new_id` on both ends of every edge. When
/// non-null, `out_updated` receives the rows changed. Returns
/// `CASTORE_ERR_DUPLICATE_ASSOCIATION` if the rewrite would duplicate an edge
/// `new_id` already has.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. When non-null, `out_updated`
/// must point to writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_store_replace_parent_child_component_id(
    handle: *mut CastoreStoreHandle,
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
            CASTORE_OK
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
pub unsafe extern "C" fn castore_store_count_parent_child_associations(
    handle: *const CastoreStoreHandle,
    filter_json: *const c_char,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = deref_handle!(ref handle);
    if out_count.is_null() {
        set_error("out_count is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let filter: core_lib::ParentChildFilter = match unsafe { assoc_filter_from_json(filter_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    match store.inner.count_parent_child_associations(&filter) {
        Ok(n) => {
            unsafe { *out_count = n };
            CASTORE_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- Free helpers ---------------------------------------------------------

/// Release a key handle returned by this library.
///
/// # Safety
///
/// `key` must be null or a live key handle returned by this library that has not already been
/// freed. The key must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_key_free(key: *mut CastoreKeyHandle) {
    if !key.is_null() {
        unsafe { drop(Box::from_raw(key)) };
    }
}

/// Compare two key handles by identity (owner, category, type, name,
/// resolution, interval, features). `*out_eq` receives the result.
///
/// # Safety
///
/// `a` and `b` must be live key handles created by this library and `out_eq`
/// must be valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_key_eq(
    a: *const CastoreKeyHandle,
    b: *const CastoreKeyHandle,
    out_eq: *mut bool,
) -> i32 {
    clear_error();
    let (a, b) = match (unsafe { a.as_ref() }, unsafe { b.as_ref() }) {
        (Some(a), Some(b)) => (a, b),
        _ => return CASTORE_ERR_NULL_POINTER,
    };
    if out_eq.is_null() {
        set_error("out_eq is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_eq = a.inner == b.inner };
    CASTORE_OK
}

/// Hash a key handle's identity into `*out_hash`, consistent with `castore_key_eq`
/// (equal keys hash equal). The value is stable only within one process — do
/// not persist it or compare it across library versions.
///
/// # Safety
///
/// `key` must be a live key handle created by this library and `out_hash` must
/// be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_key_identity_hash(
    key: *const CastoreKeyHandle,
    out_hash: *mut u64,
) -> i32 {
    clear_error();
    let key = deref_handle!(ref key);
    if out_hash.is_null() {
        set_error("out_hash is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.inner.hash(&mut hasher);
    unsafe { *out_hash = hasher.finish() };
    CASTORE_OK
}

/// Release an `f64` buffer returned by this library.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_buffer_free_f64(ptr: *mut f64, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

/// Free a `u8` buffer returned by `castore_store_get_array_by_hash`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` bytes. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_buffer_free_u8(ptr: *mut u8, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

/// Free an `i64` buffer returned by `castore_store_get_non_sequential`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_buffer_free_i64(ptr: *mut i64, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

// ---- Error message --------------------------------------------------------

/// Copy the thread-local error message into `buf` (UTF-8, null-terminated).
/// Returns the number of bytes that would have been written (excluding the NUL)
/// in `*needed`. If `buf_len` is too small, `buf` is filled up to its length
/// and truncated; the function still returns `CASTORE_OK` and the caller can decide
/// whether to retry with a larger buffer.
///
/// # Safety
///
/// `needed` may be null; otherwise it must be valid for writing one `u64`. `buf` may be null when
/// `buf_len` is zero; otherwise it must reference at least `buf_len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_last_error_message(
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
        return CASTORE_OK;
    }
    let max_copy = std::cmp::min(buf_len.saturating_sub(1) as usize, bytes.len());
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, max_copy);
        *buf.add(max_copy) = 0;
    }
    CASTORE_OK
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
            return Err(CASTORE_ERR_INVALID_PARAMETER);
        }
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            set_error("features_json must be an object");
            return Err(CASTORE_ERR_INVALID_PARAMETER);
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
                    return Err(CASTORE_ERR_INVALID_PARAMETER);
                }
            }
            Value::String(s) => core_lib::FeatureValue::Str(s.clone()),
            other => {
                set_error(format!(
                    "feature {k}: must be int/float/bool/string, got {}",
                    type_name(other)
                ));
                return Err(CASTORE_ERR_INVALID_PARAMETER);
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

/// An inclusive-start, exclusive-end UTC time range for the get paths.
type TimeRange = (DateTime<Utc>, DateTime<Utc>);

/// Build an optional `(start, end)` time range from the FFI convention shared by
/// the get paths: `present = false` yields `None`; otherwise both millisecond
/// bounds are converted to UTC. Returns `Err(code)` (after setting the
/// thread-local error) if either bound is out of range.
fn build_time_range(present: bool, start_ms: i64, end_ms: i64) -> Result<Option<TimeRange>, i32> {
    if !present {
        return Ok(None);
    }
    let start = unix_ms_to_datetime(start_ms).ok_or_else(|| {
        set_error(format!("invalid time_range_start_ms: {start_ms}"));
        CASTORE_ERR_INVALID_PARAMETER
    })?;
    let end = unix_ms_to_datetime(end_ms).ok_or_else(|| {
        set_error(format!("invalid time_range_end_ms: {end_ms}"));
        CASTORE_ERR_INVALID_PARAMETER
    })?;
    Ok(Some((start, end)))
}

fn datetime_to_unix_ms(dt: DateTime<Utc>) -> Option<i64> {
    Some(dt.timestamp_millis())
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
pub struct CastoreStaticReaderHandle {
    inner: core_lib::StaticReader,
}

/// Opaque handle wrapping a core `ForecastReader` (one forecast type, per-key
/// windows).
pub struct CastoreForecastReaderHandle {
    inner: core_lib::ForecastReader,
}

/// Build a [`core_lib::ListFilter`] from the reader build arguments shared by
/// both readers (owner / category / name / resolution / features). The
/// time-series type is set by the caller, not here.
unsafe fn reader_filter(
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
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
                return Err(CASTORE_ERR_INVALID_PARAMETER);
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
    if let Some(p) = unsafe { cstr_to_optional_period(resolution)? } {
        filter = filter.resolution(p);
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
/// `out_len` must be valid for writing one `u64`. When `buf` is non-null it must
/// be valid for writing `cap` `i64` values.
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

/// Build a [`CastoreStaticReaderHandle`] over the SingleTimeSeries matching the
/// filter. `resolution` must be a non-empty ISO-8601 period (one resolution per reader); the
/// matched series must share one grid (`initial_timestamp` + `length`).
///
/// # Safety
///
/// `handle` must be a live store handle. `name` / `features_json` must be null
/// or valid null-terminated UTF-8. `out_reader` must be valid for writing one
/// pointer; the returned handle must be freed exactly once with
/// `castore_static_reader_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_build_static_reader(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_reader: *mut *mut CastoreStaticReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let filter = match unsafe {
        reader_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            name,
            resolution,
            features_json,
        )
    } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let reader = match store.inner.build_static_reader(filter) {
        Ok(r) => r,
        Err(e) => return map_core_error(e),
    };
    unsafe { *out_reader = Box::into_raw(Box::new(CastoreStaticReaderHandle { inner: reader })) };
    CASTORE_OK
}

/// Read the reader's master grid: `initial_timestamp` (unix ms), `resolution`
/// (an owned ISO-8601 duration string, e.g. `PT1H` / `P1M`), and the number of
/// timestamps on the grid.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. Each out pointer must be valid
/// for writing one value. On success `*out_resolution` is an owned C string the
/// caller must free exactly once with [`castore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_static_reader_grid(
    reader: *const CastoreStaticReaderHandle,
    out_initial_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_length: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null() || out_resolution.is_null() || out_length.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let initial = match datetime_to_unix_ms(reader.inner.initial_timestamp()) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    unsafe {
        *out_initial_ms = initial;
        *out_resolution = period_cstr(reader.inner.resolution());
        *out_length = reader.inner.length() as u64;
    }
    CASTORE_OK
}

/// Number of columnar groups in the reader.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_n` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_static_reader_num_groups(
    reader: *const CastoreStaticReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.groups().len() as u64 };
    CASTORE_OK
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
pub unsafe extern "C" fn castore_static_reader_group_info(
    reader: *const CastoreStaticReaderHandle,
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
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_num_columns.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let shape: Vec<i64> = group.element_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = group.dtype().code();
        *out_num_columns = group.num_columns() as u64;
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    CASTORE_OK
}

/// Return an owned key handle for column `col_idx` of group `group_idx`. The
/// handle carries the column's identity and must be freed with `castore_key_free`.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_key` must be valid for
/// writing one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_static_reader_group_key(
    reader: *const CastoreStaticReaderHandle,
    group_idx: u64,
    col_idx: u64,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let key = match group.keys().get(col_idx as usize) {
        Some(k) => k,
        None => {
            set_error(format!("column index {col_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let handle = Box::new(CastoreKeyHandle {
        inner: key.identity().clone(),
    });
    unsafe { *out_key = Box::into_raw(handle) };
    CASTORE_OK
}

/// Read the value of every series at `at_unix_ms`, filling the reader's reusable
/// buffers. After this, `castore_static_reader_group_values` exposes each group's
/// bytes. Errors if `at_unix_ms` is off the reader's grid.
///
/// # Safety
///
/// `reader` must be a live static-reader handle and `store` a live store handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_static_reader_read(
    reader: *mut CastoreStaticReaderHandle,
    store: *const CastoreStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.static_read(&mut reader.inner, at) {
        Ok(()) => CASTORE_OK,
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
pub unsafe extern "C" fn castore_static_reader_group_values(
    reader: *const CastoreStaticReaderHandle,
    group_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let bytes = group.values();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    CASTORE_OK
}

/// Free a static-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `castore_store_build_static_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_static_reader_free(reader: *mut CastoreStaticReaderHandle) {
    if !reader.is_null() {
        unsafe { drop(Box::from_raw(reader)) };
    }
}

// ---- ForecastReader -------------------------------------------------------

/// Build a [`CastoreForecastReaderHandle`] over the forecasts matching the filter.
/// `time_series_type` must be a forecast type; a `Deterministic` reader is
/// abstract and also includes `DeterministicSingleTimeSeries`. `resolution`
/// must be positive; matched forecasts must share one window timeline.
///
/// # Safety
///
/// `handle` must be a live store handle. `name` / `features_json` must be null
/// or valid null-terminated UTF-8. `out_reader` must be valid for writing one
/// pointer; free the result with `castore_forecast_reader_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn castore_store_build_forecast_reader(
    handle: *const CastoreStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_reader: *mut *mut CastoreForecastReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let ts_type = match time_series_type_from_int(time_series_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {time_series_type}"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let mut filter = match unsafe {
        reader_filter(
            has_owner,
            owner_id,
            has_owner_category,
            owner_category,
            name,
            resolution,
            features_json,
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
    unsafe { *out_reader = Box::into_raw(Box::new(CastoreForecastReaderHandle { inner: reader })) };
    CASTORE_OK
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
/// [`castore_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_timeline(
    reader: *const CastoreForecastReaderHandle,
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
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null()
        || out_resolution.is_null()
        || out_interval.is_null()
        || out_count.is_null()
    {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let initial = match datetime_to_unix_ms(reader.inner.initial_timestamp()) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return CASTORE_ERR_INTEGRITY;
        }
    };
    unsafe {
        *out_initial_ms = initial;
        *out_resolution = period_cstr(reader.inner.resolution());
        *out_interval = period_cstr(reader.inner.interval());
        *out_count = reader.inner.count() as u64;
    }
    CASTORE_OK
}

/// Number of per-key window entries in the reader.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_n` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_num_entries(
    reader: *const CastoreForecastReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.entries().len() as u64 };
    CASTORE_OK
}

/// Number of deduplicated window slots: the count of physical backend reads per
/// [`castore_forecast_reader_read`]. Entries that share an array and read plan
/// (e.g. components referencing one shared forecast) collapse to one slot.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_n` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_num_slots(
    reader: *const CastoreForecastReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.slots().len() as u64 };
    CASTORE_OK
}

/// The 0-based slot index backing entry `entry_idx`. Entries sharing an array
/// and read plan return the same slot, letting a caller group components that
/// resolve to one window and materialize it once.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_slot` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_entry_slot(
    reader: *const CastoreForecastReaderHandle,
    entry_idx: u64,
    out_slot: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_slot.is_null() {
        set_error("out_slot is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    unsafe { *out_slot = entry.slot() as u64 };
    CASTORE_OK
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
pub unsafe extern "C" fn castore_forecast_reader_entry_info(
    reader: *const CastoreForecastReaderHandle,
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
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    if entry_idx as usize >= reader.inner.entries().len() {
        set_error(format!("entry index {entry_idx} out of bounds"));
        return CASTORE_ERR_INVALID_PARAMETER;
    }
    let slot = reader.inner.entry_slot(entry_idx as usize);
    let shape: Vec<i64> = slot.window_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = slot.dtype().code();
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    CASTORE_OK
}

/// Return an owned key handle for entry `entry_idx`, freed with `castore_key_free`.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_key` must be valid for
/// writing one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_entry_key(
    reader: *const CastoreForecastReaderHandle,
    entry_idx: u64,
    out_key: *mut *mut CastoreKeyHandle,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    let handle = Box::new(CastoreKeyHandle {
        inner: entry.key().identity().clone(),
    });
    unsafe { *out_key = Box::into_raw(handle) };
    CASTORE_OK
}

/// Read the forecast window at `at_unix_ms` for every entry, filling the
/// reader's reusable buffers. Errors if `at_unix_ms` is off the window timeline.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle and `store` a live store
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_read(
    reader: *mut CastoreForecastReaderHandle,
    store: *const CastoreStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return CASTORE_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.forecast_read(&mut reader.inner, at) {
        Ok(()) => CASTORE_OK,
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
pub unsafe extern "C" fn castore_forecast_reader_entry_values(
    reader: *const CastoreForecastReaderHandle,
    entry_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return CASTORE_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return CASTORE_ERR_NULL_POINTER;
    }
    if entry_idx as usize >= reader.inner.entries().len() {
        set_error(format!("entry index {entry_idx} out of bounds"));
        return CASTORE_ERR_INVALID_PARAMETER;
    }
    let bytes = reader.inner.entry_slot(entry_idx as usize).window();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    CASTORE_OK
}

/// Free a forecast-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `castore_store_build_forecast_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn castore_forecast_reader_free(reader: *mut CastoreForecastReaderHandle) {
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
                None,
            )
            .unwrap();
    }

    #[test]
    fn static_reader_ffi_roundtrip() {
        let mut store = Store::create(None, true).unwrap();
        add_sts_f64(&mut store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        add_sts_f64(&mut store, 2, "load", &[20.0, 21.0, 22.0, 23.0]);
        let handle = CastoreStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut CastoreStaticReaderHandle = ptr::null_mut();
        let rc = unsafe {
            castore_store_build_static_reader(
                &handle,
                false,
                0,
                false,
                0,
                ptr::null(),
                hour.as_ptr(),
                ptr::null(),
                &mut reader,
            )
        };
        assert_eq!(rc, CASTORE_OK);
        assert!(!reader.is_null());

        // Grid. Resolution is an owned ISO-8601 C string.
        let (mut initial, mut len) = (0i64, 0u64);
        let mut res: *mut c_char = ptr::null_mut();
        assert_eq!(
            unsafe { castore_static_reader_grid(reader, &mut initial, &mut res, &mut len) },
            CASTORE_OK
        );
        assert_eq!((initial, len), (T0_MS, 4));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        unsafe { castore_string_free(res) };

        // One f64 group, 2 columns, scalar shape.
        let mut n = 0u64;
        assert_eq!(
            unsafe { castore_static_reader_num_groups(reader, &mut n) },
            CASTORE_OK
        );
        assert_eq!(n, 1);
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 99u64);
        assert_eq!(
            unsafe {
                castore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            CASTORE_OK
        );
        assert_eq!((dtype, ncols, shape_len), (0, 2, 0)); // F64 code 0

        // Column keys (owners 1 then 2).
        for (col, owner) in [(0u64, 1i64), (1, 2)] {
            let mut key: *mut CastoreKeyHandle = ptr::null_mut();
            assert_eq!(
                unsafe { castore_static_reader_group_key(reader, 0, col, &mut key) },
                CASTORE_OK
            );
            assert_eq!(unsafe { (*key).inner.owner_id }, owner);
            unsafe { castore_key_free(key) };
        }

        // Read at t0 + 2h -> [12, 22].
        let at = T0_MS + 2 * HOUR_MS;
        assert_eq!(
            unsafe { castore_static_reader_read(reader, &handle, at) },
            CASTORE_OK
        );
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { castore_static_reader_group_values(reader, 0, &mut p, &mut blen) },
            CASTORE_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[12.0, 22.0]);

        // Off-grid read errors.
        assert_ne!(
            unsafe { castore_static_reader_read(reader, &handle, T0_MS + HOUR_MS / 2) },
            CASTORE_OK
        );

        unsafe { castore_static_reader_free(reader) };
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
                None,
            )
            .unwrap();
        let handle = CastoreStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut CastoreForecastReaderHandle = ptr::null_mut();
        let rc = unsafe {
            castore_store_build_forecast_reader(
                &handle,
                false,
                0,
                false,
                0,
                2, // Deterministic
                ptr::null(),
                hour.as_ptr(),
                ptr::null(),
                &mut reader,
            )
        };
        assert_eq!(rc, CASTORE_OK);

        let (mut initial, mut count) = (0i64, 0u64);
        let (mut res, mut interval): (*mut c_char, *mut c_char) =
            (ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            unsafe {
                castore_forecast_reader_timeline(
                    reader,
                    &mut initial,
                    &mut res,
                    &mut interval,
                    &mut count,
                )
            },
            CASTORE_OK
        );
        assert_eq!((initial, count), (T0_MS, 3));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        assert_eq!(
            unsafe { CStr::from_ptr(interval) }.to_str().unwrap(),
            "PT1H"
        );
        unsafe {
            castore_string_free(res);
            castore_string_free(interval);
        }

        let mut n = 0u64;
        assert_eq!(
            unsafe { castore_forecast_reader_num_entries(reader, &mut n) },
            CASTORE_OK
        );
        assert_eq!(n, 1);

        // Window shape [H] = [2].
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                castore_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            CASTORE_OK
        );
        assert_eq!((dtype, shape_len), (0, 1));
        let mut shape = [0i64; 1];
        let mut got = 0u64;
        assert_eq!(
            unsafe {
                castore_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    shape.as_mut_ptr(),
                    1,
                    &mut got,
                )
            },
            CASTORE_OK
        );
        assert_eq!(shape, [2]);

        // Window at index 1 (t0 + 1h) -> [10, 11].
        assert_eq!(
            unsafe { castore_forecast_reader_read(reader, &handle, T0_MS + HOUR_MS) },
            CASTORE_OK
        );
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { castore_forecast_reader_entry_values(reader, 0, &mut p, &mut blen) },
            CASTORE_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[10.0, 11.0]);

        unsafe { castore_forecast_reader_free(reader) };
    }

    #[test]
    fn get_single_returns_native_dtype_and_shape() {
        use core_lib::Dtype;
        use std::ffi::CString;

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
                None,
            )
            .unwrap();
        let handle = CastoreStoreHandle { inner: store };

        let name = CString::new("im").unwrap();
        let hour = CString::new("PT1H").unwrap();
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    5,
                    0,
                    name.as_ptr(),
                    0,
                    hour.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut key,
                )
            },
            CASTORE_OK
        );

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        assert_eq!(
            unsafe {
                castore_store_get_single(
                    &handle,
                    key,
                    false,
                    0,
                    0,
                    &mut initial,
                    &mut res,
                    &mut dtype,
                    &mut shape_ptr,
                    &mut shape_len,
                    &mut data_ptr,
                    &mut data_len,
                )
            },
            CASTORE_OK
        );
        assert_eq!(dtype, 2); // I64
        assert_eq!(
            unsafe { slice::from_raw_parts(shape_ptr, shape_len as usize) },
            &[3, 2]
        );
        let vals: Vec<i64> = unsafe { slice::from_raw_parts(data_ptr, data_len as usize) }
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(vals, vec![10, 11, 20, 21, 30, 31]);

        unsafe {
            castore_string_free(res);
            castore_buffer_free_i64(shape_ptr, shape_len);
            castore_buffer_free_u8(data_ptr, data_len);
            castore_key_free(key);
        }
    }
}

#[cfg(test)]
mod abi_tests {
    //! Tests that drive the C ABI the way a foreign caller does.
    //!
    //! The `reader_ffi_tests` module above constructs `CastoreStoreHandle`
    //! directly and never calls `castore_store_create` / `_open` / `_persist` /
    //! `_free`, so the lifecycle exports had no coverage at all. Nor did any test
    //! assert an **error code by value** — a change that returned
    //! `CASTORE_ERR_INTERNAL` where a caller expects `CASTORE_ERR_NOT_FOUND`
    //! would have gone unnoticed, and the numeric codes are the ABI contract
    //! every binding switches on.
    //!
    //! Deliberately not covered: double-free and use-after-free. Those are
    //! documented undefined behavior, not defined behavior worth pinning.

    use super::*;
    use std::ffi::CString;

    const T0_MS: i64 = 1_700_000_000_000;
    const HOUR: &str = "PT1H";

    /// `castore_last_error_message`'s current value.
    fn last_error() -> String {
        let mut needed = 0u64;
        assert_eq!(
            unsafe { castore_last_error_message(ptr::null_mut(), 0, &mut needed) },
            CASTORE_OK
        );
        if needed == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                castore_last_error_message(
                    buf.as_mut_ptr() as *mut c_char,
                    buf.len() as u64,
                    &mut needed,
                )
            },
            CASTORE_OK
        );
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..nul]).into_owned()
    }

    /// Add one f64 SingleTimeSeries through the ABI; returns the key handle.
    #[allow(clippy::too_many_arguments)]
    fn abi_add_f64(
        store: *mut CastoreStoreHandle,
        owner: i64,
        name: &str,
        vals: &[f64],
    ) -> *mut CastoreKeyHandle {
        let (rc, key) = abi_try_add(
            store,
            owner,
            name,
            core_lib::Dtype::F64.code(),
            &to_le(vals),
            vals.len(),
        );
        assert_eq!(rc, CASTORE_OK, "add failed: {}", last_error());
        key
    }

    fn to_le(vals: &[f64]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Add through the ABI without asserting success.
    fn abi_try_add(
        store: *mut CastoreStoreHandle,
        owner: i64,
        name: &str,
        dtype: i32,
        bytes: &[u8],
        length: usize,
    ) -> (i32, *mut CastoreKeyHandle) {
        let owner_type = CString::new("Generator").unwrap();
        let name_c = CString::new(name).unwrap();
        let res = CString::new(HOUR).unwrap();
        let dims = [length as u64];
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        let rc = unsafe {
            castore_store_add_single(
                store,
                owner,
                owner_type.as_ptr(),
                0,
                name_c.as_ptr(),
                T0_MS,
                res.as_ptr(),
                dtype,
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut key,
            )
        };
        (rc, key)
    }

    fn abi_create_in_memory() -> *mut CastoreStoreHandle {
        let mut store: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_create(ptr::null(), true, &mut store) },
            CASTORE_OK
        );
        assert!(!store.is_null());
        store
    }

    // ---- Store lifecycle through the ABI ----------------------------------

    #[test]
    fn create_add_flush_free_then_reopen_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abi.nc");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();

        // create on a real path
        let mut store: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_create(path_c.as_ptr(), false, &mut store) },
            CASTORE_OK
        );
        assert!(!store.is_null());

        // the handle reports the path it was created at, and is writable
        let mut read_only = true;
        assert_eq!(
            unsafe { castore_store_read_only(store, &mut read_only) },
            CASTORE_OK
        );
        assert!(!read_only);

        let mut has_path = false;
        let mut needed = 0u64;
        assert_eq!(
            unsafe {
                castore_store_get_path(store, &mut has_path, ptr::null_mut(), 0, &mut needed)
            },
            CASTORE_OK
        );
        assert!(has_path);
        let mut buf = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                castore_store_get_path(
                    store,
                    &mut has_path,
                    buf.as_mut_ptr() as *mut c_char,
                    buf.len() as u64,
                    &mut needed,
                )
            },
            CASTORE_OK
        );
        let nul = buf.iter().position(|&b| b == 0).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..nul]).unwrap(),
            path.to_str().unwrap()
        );

        // add, then flush and free
        let key = abi_add_f64(store, 1, "load", &[10.0, 11.0, 12.0, 13.0]);
        let mut present = false;
        assert_eq!(
            unsafe { castore_store_has(store, key, &mut present) },
            CASTORE_OK
        );
        assert!(present);
        assert_eq!(unsafe { castore_store_flush(store) }, CASTORE_OK);
        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }

        // reopen read-only through the ABI and read the values back
        let mut ro: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_open(path_c.as_ptr(), true, &mut ro) },
            CASTORE_OK
        );
        let mut read_only = false;
        assert_eq!(
            unsafe { castore_store_read_only(ro, &mut read_only) },
            CASTORE_OK
        );
        assert!(read_only);

        let vals = abi_read_f64(ro, 1, "load");
        assert_eq!(vals, vec![10.0, 11.0, 12.0, 13.0]);

        let mut errors = u64::MAX;
        assert_eq!(unsafe { castore_store_verify(ro, &mut errors) }, CASTORE_OK);
        assert_eq!(errors, 0);

        unsafe { castore_store_free(ro) };
    }

    /// Read one f64 SingleTimeSeries by attributes, through the ABI.
    fn abi_read_f64(store: *mut CastoreStoreHandle, owner: i64, name: &str) -> Vec<f64> {
        let (dtype, shape, bytes) = abi_get_single(store, owner, name);
        assert_eq!(dtype, core_lib::Dtype::F64.code());
        assert_eq!(shape.len(), 1);
        bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// `castore_store_get_single` through the ABI, returning
    /// `(dtype_code, shape, raw bytes)` with every out-buffer freed.
    fn abi_get_single(
        store: *mut CastoreStoreHandle,
        owner: i64,
        name: &str,
    ) -> (i32, Vec<i64>, Vec<u8>) {
        let name_c = CString::new(name).unwrap();
        let res = CString::new(HOUR).unwrap();
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    owner,
                    0,
                    name_c.as_ptr(),
                    0,
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut key,
                )
            },
            CASTORE_OK
        );

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res_out: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        let rc = unsafe {
            castore_store_get_single(
                store,
                key,
                false,
                0,
                0,
                &mut initial,
                &mut res_out,
                &mut dtype,
                &mut shape_ptr,
                &mut shape_len,
                &mut data_ptr,
                &mut data_len,
            )
        };
        assert_eq!(rc, CASTORE_OK, "get_single failed: {}", last_error());
        assert_eq!(initial, T0_MS);

        let shape = unsafe { slice::from_raw_parts(shape_ptr, shape_len as usize) }.to_vec();
        let bytes = unsafe { slice::from_raw_parts(data_ptr, data_len as usize) }.to_vec();
        unsafe {
            castore_string_free(res_out);
            castore_buffer_free_i64(shape_ptr, shape_len);
            castore_buffer_free_u8(data_ptr, data_len);
            castore_key_free(key);
        }
        (dtype, shape, bytes)
    }

    #[test]
    fn persist_materializes_an_in_memory_store_and_reopens() {
        let store = abi_create_in_memory();
        let key = abi_add_f64(store, 3, "load", &[1.5, 2.5, 3.5]);

        // An in-memory store reports no path.
        let (mut has_path, mut len) = (true, 99u64);
        assert_eq!(
            unsafe { castore_store_get_path(store, &mut has_path, ptr::null_mut(), 0, &mut len) },
            CASTORE_OK
        );
        assert!(!has_path);
        assert_eq!(len, 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persisted.nc");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { castore_store_persist(store, path_c.as_ptr()) },
            CASTORE_OK
        );
        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }

        let mut reopened: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_open(path_c.as_ptr(), true, &mut reopened) },
            CASTORE_OK
        );
        assert_eq!(abi_read_f64(reopened, 3, "load"), vec![1.5, 2.5, 3.5]);
        unsafe { castore_store_free(reopened) };
    }

    #[test]
    fn opening_a_missing_path_reports_an_error_and_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.nc");
        let path_c = CString::new(missing.to_str().unwrap()).unwrap();
        let mut store: *mut CastoreStoreHandle = ptr::null_mut();
        let rc = unsafe { castore_store_open(path_c.as_ptr(), true, &mut store) };
        assert_ne!(rc, CASTORE_OK);
        assert!(store.is_null(), "no handle may be produced on failure");
        assert!(
            !last_error().is_empty(),
            "a failed open must leave a diagnostic"
        );
    }

    #[test]
    fn freeing_a_null_store_handle_is_a_no_op() {
        // Documented: `castore_store_free` accepts null. Every `*_free` export
        // does, so bindings need no null guard of their own.
        unsafe {
            castore_store_free(ptr::null_mut());
            castore_key_free(ptr::null_mut());
            castore_string_free(ptr::null_mut());
            castore_buffer_free_u8(ptr::null_mut(), 0);
            castore_buffer_free_i64(ptr::null_mut(), 0);
            castore_buffer_free_f64(ptr::null_mut(), 0);
            castore_buffer_free_u64(ptr::null_mut(), 0);
            castore_static_reader_free(ptr::null_mut());
            castore_forecast_reader_free(ptr::null_mut());
            castore_bulk_result_free(ptr::null_mut());
            castore_batch_free(ptr::null_mut());
        }
    }

    // ---- Null-pointer sweep -----------------------------------------------

    #[test]
    fn null_handles_and_out_params_return_err_null_pointer() {
        // One representative export per family. The code must be exactly 1:
        // bindings map it to their own "null argument" exception.
        assert_eq!(CASTORE_ERR_NULL_POINTER, 1, "the ABI value is the contract");

        let store = abi_create_in_memory();
        let key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        // -- store op: null handle, then null out-param
        let mut present = false;
        assert_eq!(
            unsafe { castore_store_has(ptr::null(), key, &mut present) },
            CASTORE_ERR_NULL_POINTER
        );
        assert_eq!(
            unsafe { castore_store_has(store, key, ptr::null_mut()) },
            CASTORE_ERR_NULL_POINTER
        );
        // null key handle
        assert_eq!(
            unsafe { castore_store_has(store, ptr::null(), &mut present) },
            CASTORE_ERR_NULL_POINTER
        );
        // create with a null out pointer
        assert_eq!(
            unsafe { castore_store_create(ptr::null(), true, ptr::null_mut()) },
            CASTORE_ERR_NULL_POINTER
        );
        // probe-style store op with a null out_len
        assert_eq!(
            unsafe { castore_store_counts_by_type(store, ptr::null_mut(), 0, ptr::null_mut()) },
            CASTORE_ERR_NULL_POINTER
        );

        // -- reader op
        let res = CString::new(HOUR).unwrap();
        let mut reader: *mut CastoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_store_build_static_reader(
                    ptr::null(),
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    res.as_ptr(),
                    ptr::null(),
                    &mut reader,
                )
            },
            CASTORE_ERR_NULL_POINTER
        );
        let mut n = 0u64;
        assert_eq!(
            unsafe { castore_static_reader_num_groups(ptr::null(), &mut n) },
            CASTORE_ERR_NULL_POINTER
        );
        // build a real reader, then pass a null out-param to it
        assert_eq!(
            unsafe {
                castore_store_build_static_reader(
                    store,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    res.as_ptr(),
                    ptr::null(),
                    &mut reader,
                )
            },
            CASTORE_OK
        );
        assert_eq!(
            unsafe { castore_static_reader_num_groups(reader, ptr::null_mut()) },
            CASTORE_ERR_NULL_POINTER
        );
        let (mut dtype, mut ncols) = (0i32, 0u64);
        assert_eq!(
            unsafe {
                castore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                )
            },
            CASTORE_ERR_NULL_POINTER
        );
        // reader read against a null store handle
        assert_eq!(
            unsafe { castore_static_reader_read(reader, ptr::null(), T0_MS) },
            CASTORE_ERR_NULL_POINTER
        );
        unsafe { castore_static_reader_free(reader) };

        // -- key op
        let name = CString::new("load").unwrap();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    1,
                    0,
                    name.as_ptr(),
                    0,
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                )
            },
            CASTORE_ERR_NULL_POINTER
        );
        // a null name string is a null pointer, not invalid UTF-8
        let mut out_key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    1,
                    0,
                    ptr::null(),
                    0,
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut out_key,
                )
            },
            CASTORE_ERR_NULL_POINTER
        );
        let mut eq = false;
        assert_eq!(
            unsafe { castore_key_eq(ptr::null(), key, &mut eq) },
            CASTORE_ERR_NULL_POINTER
        );
        let mut hash = 0u64;
        assert_eq!(
            unsafe { castore_key_identity_hash(ptr::null(), &mut hash) },
            CASTORE_ERR_NULL_POINTER
        );

        // -- buffer op (probe-then-fetch with a null out_len)
        assert_eq!(
            unsafe {
                castore_store_list_keys(
                    store,
                    false,
                    0,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                )
            },
            CASTORE_ERR_NULL_POINTER
        );

        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }
    }

    // ---- Invalid UTF-8 -----------------------------------------------------

    #[test]
    fn invalid_utf8_name_returns_err_invalid_utf8_with_a_message() {
        assert_eq!(CASTORE_ERR_INVALID_UTF8, 2, "the ABI value is the contract");
        let store = abi_create_in_memory();

        // `wind\xff` is not valid UTF-8; the trailing NUL terminates the C string.
        let bad_name: &[u8] = b"wind\xff\x00";
        let owner_type = CString::new("Generator").unwrap();
        let res = CString::new(HOUR).unwrap();
        let vals = [1.0f64, 2.0];
        let bytes = to_le(&vals);
        let dims = [2u64];
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        let rc = unsafe {
            castore_store_add_single(
                store,
                1,
                owner_type.as_ptr(),
                0,
                bad_name.as_ptr() as *const c_char,
                T0_MS,
                res.as_ptr(),
                core_lib::Dtype::F64.code(),
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut key,
            )
        };
        assert_eq!(rc, CASTORE_ERR_INVALID_UTF8);
        assert!(key.is_null());
        let msg = last_error();
        assert!(
            !msg.is_empty(),
            "castore_last_error_message must describe the failure"
        );

        // Nothing was added.
        let (mut components, mut total, mut arrays) = (0i64, 0i64, 0i64);
        assert_eq!(
            unsafe { castore_store_counts(store, &mut components, &mut total, &mut arrays) },
            CASTORE_OK
        );
        assert_eq!(total, 0);

        // A successful call clears the message.
        let key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);
        assert!(
            last_error().is_empty(),
            "a successful call must clear the thread-local error"
        );
        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }
    }

    #[test]
    fn an_invalid_period_string_is_an_invalid_parameter() {
        assert_eq!(CASTORE_ERR_INVALID_PARAMETER, 3);
        let store = abi_create_in_memory();
        let owner_type = CString::new("Generator").unwrap();
        let name = CString::new("load").unwrap();
        let bad_res = CString::new("not-a-period").unwrap();
        let bytes = to_le(&[1.0f64, 2.0]);
        let dims = [2u64];
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        let rc = unsafe {
            castore_store_add_single(
                store,
                1,
                owner_type.as_ptr(),
                0,
                name.as_ptr(),
                T0_MS,
                bad_res.as_ptr(),
                core_lib::Dtype::F64.code(),
                1,
                dims.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as u64,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                &mut key,
            )
        };
        assert_eq!(rc, CASTORE_ERR_INVALID_PARAMETER);
        assert!(!last_error().is_empty());
        unsafe { castore_store_free(store) };
    }

    #[test]
    fn an_unknown_dtype_code_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        let (rc, key) = abi_try_add(store, 1, "load", 99, &to_le(&[1.0, 2.0]), 2);
        assert_eq!(rc, CASTORE_ERR_INVALID_PARAMETER);
        assert!(key.is_null());
        assert!(!last_error().is_empty());
        unsafe { castore_store_free(store) };
    }

    #[test]
    fn a_byte_length_that_contradicts_the_shape_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        // Shape says 4 elements, only 2 f64s of bytes are supplied.
        let (rc, key) = abi_try_add(
            store,
            1,
            "load",
            core_lib::Dtype::F64.code(),
            &to_le(&[1.0, 2.0]),
            4,
        );
        assert_eq!(rc, CASTORE_ERR_INVALID_PARAMETER);
        assert!(key.is_null());
        unsafe { castore_store_free(store) };
    }

    // ---- Error codes by value ---------------------------------------------

    #[test]
    fn not_found_duplicate_and_read_only_codes_come_back_by_value() {
        assert_eq!(
            (
                CASTORE_ERR_NOT_FOUND,
                CASTORE_ERR_DUPLICATE,
                CASTORE_ERR_READ_ONLY
            ),
            (4, 5, 7),
            "the ABI values are the contract"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codes.nc");
        let path_c = CString::new(path.to_str().unwrap()).unwrap();

        let mut store: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_create(path_c.as_ptr(), false, &mut store) },
            CASTORE_OK
        );
        let key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        // NOT_FOUND: remove a key that names a series that does not exist.
        let absent = CString::new("absent").unwrap();
        let res = CString::new(HOUR).unwrap();
        let mut missing: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    1,
                    0,
                    absent.as_ptr(),
                    0,
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut missing,
                )
            },
            CASTORE_OK
        );
        assert_eq!(
            unsafe { castore_store_remove(store, missing) },
            CASTORE_ERR_NOT_FOUND
        );
        assert!(!last_error().is_empty());

        // DUPLICATE: add the same identity twice.
        let (rc, dup) = abi_try_add(
            store,
            1,
            "load",
            core_lib::Dtype::F64.code(),
            &to_le(&[1.0, 2.0]),
            2,
        );
        assert_eq!(rc, CASTORE_ERR_DUPLICATE);
        assert!(dup.is_null());

        assert_eq!(unsafe { castore_store_flush(store) }, CASTORE_OK);
        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }

        // READ_ONLY: every write through a read-only handle.
        let mut ro: *mut CastoreStoreHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_store_open(path_c.as_ptr(), true, &mut ro) },
            CASTORE_OK
        );
        let (rc, k) = abi_try_add(
            ro,
            2,
            "new",
            core_lib::Dtype::F64.code(),
            &to_le(&[1.0, 2.0]),
            2,
        );
        assert_eq!(rc, CASTORE_ERR_READ_ONLY);
        assert!(k.is_null());
        assert_eq!(
            unsafe { castore_store_remove(ro, missing) },
            CASTORE_ERR_READ_ONLY
        );
        assert_eq!(unsafe { castore_store_compact(ro) }, CASTORE_ERR_READ_ONLY);

        unsafe {
            castore_key_free(missing);
            castore_store_free(ro);
        }
    }

    #[test]
    fn get_single_on_a_missing_key_is_not_found() {
        let store = abi_create_in_memory();
        let name = CString::new("absent").unwrap();
        let res = CString::new(HOUR).unwrap();
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_make_key_from_attrs(
                    1,
                    0,
                    name.as_ptr(),
                    0,
                    res.as_ptr(),
                    ptr::null(),
                    ptr::null(),
                    &mut key,
                )
            },
            CASTORE_OK
        );

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res_out: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        let rc = unsafe {
            castore_store_get_single(
                store,
                key,
                false,
                0,
                0,
                &mut initial,
                &mut res_out,
                &mut dtype,
                &mut shape_ptr,
                &mut shape_len,
                &mut data_ptr,
                &mut data_len,
            )
        };
        assert_eq!(rc, CASTORE_ERR_NOT_FOUND);
        // No buffers were handed out, so there is nothing to free.
        assert!(res_out.is_null() && shape_ptr.is_null() && data_ptr.is_null());

        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }
    }

    #[test]
    fn get_array_by_hash_with_an_unknown_hash_errors() {
        let store = abi_create_in_memory();
        // `data_hash` is 32 raw bytes, not hex. An all-zero hash never exists.
        let zero = [0u8; 32];
        let (mut dtype, mut data_len) = (-1i32, 0u64);
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let rc = unsafe {
            castore_store_get_array_by_hash(
                store,
                zero.as_ptr(),
                &mut dtype,
                &mut data_ptr,
                &mut data_len,
            )
        };
        assert_ne!(rc, CASTORE_OK);
        assert!(data_ptr.is_null(), "no buffer may be handed out on failure");
        assert!(!last_error().is_empty());

        // A null hash pointer is a null-pointer error, not an integrity error.
        assert_eq!(
            unsafe {
                castore_store_get_array_by_hash(
                    store,
                    ptr::null(),
                    &mut dtype,
                    &mut data_ptr,
                    &mut data_len,
                )
            },
            CASTORE_ERR_NULL_POINTER
        );
        unsafe { castore_store_free(store) };
    }

    // ---- Buffer-probe edges ------------------------------------------------

    #[test]
    fn a_string_probe_buffer_smaller_than_needed_truncates_and_still_reports_the_full_length() {
        // Probe-then-fetch contract: `out_len` is always the full byte length,
        // whatever `cap` was, and a non-zero `cap` yields a NUL-terminated
        // prefix. A binding that trusted `cap` instead of `out_len` would
        // silently read a truncated JSON document.
        let store = abi_create_in_memory();
        let key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);

        let mut needed = 0u64;
        assert_eq!(
            unsafe { castore_store_counts_by_type(store, ptr::null_mut(), 0, &mut needed) },
            CASTORE_OK
        );
        assert!(needed > 8, "the JSON must be longer than the tiny buffer");

        // cap = 8: 7 bytes of payload plus the NUL.
        let mut small = vec![0xAAu8; 8];
        let mut reported = 0u64;
        assert_eq!(
            unsafe {
                castore_store_counts_by_type(
                    store,
                    small.as_mut_ptr() as *mut c_char,
                    small.len() as u64,
                    &mut reported,
                )
            },
            CASTORE_OK
        );
        assert_eq!(reported, needed, "out_len must report the full length");
        assert_eq!(small[7], 0, "the buffer must stay NUL-terminated");

        // The full read agrees with the truncated prefix.
        let mut full = vec![0u8; needed as usize + 1];
        assert_eq!(
            unsafe {
                castore_store_counts_by_type(
                    store,
                    full.as_mut_ptr() as *mut c_char,
                    full.len() as u64,
                    &mut reported,
                )
            },
            CASTORE_OK
        );
        assert_eq!(reported, needed);
        assert_eq!(&small[..7], &full[..7]);

        // cap = 1 leaves room only for the terminator.
        let mut one = vec![0xAAu8; 1];
        assert_eq!(
            unsafe {
                castore_store_counts_by_type(
                    store,
                    one.as_mut_ptr() as *mut c_char,
                    1,
                    &mut reported,
                )
            },
            CASTORE_OK
        );
        assert_eq!(reported, needed);
        assert_eq!(one[0], 0);

        unsafe {
            castore_key_free(key);
            castore_store_free(store);
        }
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
        let mut key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_store_add_single(
                    store,
                    1,
                    owner_type.as_ptr(),
                    0,
                    name.as_ptr(),
                    T0_MS,
                    res.as_ptr(),
                    core_lib::Dtype::F64.code(),
                    3,
                    dims.as_ptr(),
                    bytes.as_ptr(),
                    bytes.len() as u64,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    &mut key,
                )
            },
            CASTORE_OK
        );

        let mut reader: *mut CastoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_store_build_static_reader(
                    store,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    res.as_ptr(),
                    ptr::null(),
                    &mut reader,
                )
            },
            CASTORE_OK
        );

        // Probe: element shape is [2, 3], so 2 entries.
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 0u64);
        assert_eq!(
            unsafe {
                castore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            CASTORE_OK
        );
        assert_eq!(shape_len, 2);

        // cap = 1: only the first dim is written, but the length still reports 2.
        let mut one = [-1i64; 2];
        assert_eq!(
            unsafe {
                castore_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    one.as_mut_ptr(),
                    1,
                    &mut shape_len,
                )
            },
            CASTORE_OK
        );
        assert_eq!(shape_len, 2, "out_len reports the full rank");
        assert_eq!(one[0], 2, "the first dim was written");
        assert_eq!(one[1], -1, "the caller's second slot was left untouched");

        unsafe {
            castore_static_reader_free(reader);
            castore_key_free(key);
            castore_store_free(store);
        }
    }

    #[test]
    fn an_out_of_range_group_or_entry_index_is_an_invalid_parameter() {
        let store = abi_create_in_memory();
        let key = abi_add_f64(store, 1, "load", &[1.0, 2.0]);
        let res = CString::new(HOUR).unwrap();

        let mut reader: *mut CastoreStaticReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_store_build_static_reader(
                    store,
                    false,
                    0,
                    false,
                    0,
                    ptr::null(),
                    res.as_ptr(),
                    ptr::null(),
                    &mut reader,
                )
            },
            CASTORE_OK
        );
        let mut num_groups = 0u64;
        assert_eq!(
            unsafe { castore_static_reader_num_groups(reader, &mut num_groups) },
            CASTORE_OK
        );
        assert_eq!(num_groups, 1);

        // group_idx == num_groups is one past the end.
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 0u64);
        assert_eq!(
            unsafe {
                castore_static_reader_group_info(
                    reader,
                    num_groups,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            CASTORE_ERR_INVALID_PARAMETER
        );
        assert!(!last_error().is_empty());

        // A column index past the group's width, and the group index again.
        let mut out_key: *mut CastoreKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe { castore_static_reader_group_key(reader, 0, 99, &mut out_key) },
            CASTORE_ERR_INVALID_PARAMETER
        );
        assert!(out_key.is_null());
        assert_eq!(
            unsafe { castore_static_reader_group_key(reader, 99, 0, &mut out_key) },
            CASTORE_ERR_INVALID_PARAMETER
        );

        // Values before any read, and for an out-of-range group.
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { castore_static_reader_group_values(reader, 99, &mut p, &mut blen) },
            CASTORE_ERR_INVALID_PARAMETER
        );
        unsafe { castore_static_reader_free(reader) };

        // The forecast reader's entry index behaves the same way.
        let det_store = abi_create_in_memory();
        abi_add_deterministic(det_store, 7, "gen");
        let mut freader: *mut CastoreForecastReaderHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                castore_store_build_forecast_reader(
                    det_store,
                    false,
                    0,
                    false,
                    0,
                    2, // Deterministic
                    ptr::null(),
                    res.as_ptr(),
                    ptr::null(),
                    &mut freader,
                )
            },
            CASTORE_OK
        );
        let mut num_entries = 0u64;
        assert_eq!(
            unsafe { castore_forecast_reader_num_entries(freader, &mut num_entries) },
            CASTORE_OK
        );
        assert_eq!(num_entries, 1);
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                castore_forecast_reader_entry_info(
                    freader,
                    num_entries,
                    &mut dtype,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            CASTORE_ERR_INVALID_PARAMETER
        );
        let mut slot = 0u64;
        assert_eq!(
            unsafe { castore_forecast_reader_entry_slot(freader, 99, &mut slot) },
            CASTORE_ERR_INVALID_PARAMETER
        );
        unsafe {
            castore_forecast_reader_free(freader);
            castore_store_free(det_store);
            castore_key_free(key);
            castore_store_free(store);
        }
    }

    /// A Deterministic H=2, count=3 scalar forecast, added through the core API
    /// (the ABI's forecast add takes a much wider argument list and is exercised
    /// by the Julia suite).
    fn abi_add_deterministic(store: *mut CastoreStoreHandle, owner: i64, name: &str) {
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
                None,
            )
            .unwrap();
    }

    // ---- Dtype codes -------------------------------------------------------

    #[test]
    fn every_dtype_code_round_trips_through_get_single() {
        // Only F64 (0) and I64 (2) were asserted before. The codes are the ABI
        // contract each binding maps to its own element type.
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

        let cases: Vec<(&str, i32, Vec<u8>)> = vec![
            ("f64", 0, f64_bytes),
            ("f32", 1, f32_bytes),
            ("i64", 2, i64_bytes),
            ("i32", 3, i32_bytes),
            ("u64", 4, u64_bytes),
            ("bool", 5, bool_bytes),
        ];

        for (i, (name, code, bytes)) in cases.iter().enumerate() {
            let (rc, key) = abi_try_add(store, i as i64 + 1, name, *code, bytes, 3);
            assert_eq!(rc, CASTORE_OK, "adding {name}: {}", last_error());
            unsafe { castore_key_free(key) };

            let (got_dtype, shape, got_bytes) = abi_get_single(store, i as i64 + 1, name);
            assert_eq!(got_dtype, *code, "{name}: dtype code");
            assert_eq!(shape, vec![3], "{name}: shape");
            assert_eq!(&got_bytes, bytes, "{name}: bytes are not byte-exact");
        }

        unsafe { castore_store_free(store) };
    }
}
