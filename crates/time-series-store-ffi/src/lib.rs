//! C ABI for `time-series-store`. Used by the Julia binding (`TimeSeries.jl`)
//! and any other language that can call C.
//!
//! v0 surface — read/write SingleTimeSeries with optional features (passed as
//! a JSON object). Errors are reported via int32 status codes and a thread-
//! local message accessed through [`ts_last_error_message`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::ptr;
use std::slice;

use chrono::{DateTime, Utc};
use serde_json::Value;
use time_series_store_core as core_lib;

// ---- Status codes ---------------------------------------------------------

pub const TS_OK: i32 = 0;
pub const TS_ERR_NULL_POINTER: i32 = 1;
pub const TS_ERR_INVALID_UTF8: i32 = 2;
pub const TS_ERR_INVALID_PARAMETER: i32 = 3;
pub const TS_ERR_NOT_FOUND: i32 = 4;
pub const TS_ERR_DUPLICATE: i32 = 5;
pub const TS_ERR_INTEGRITY: i32 = 6;
pub const TS_ERR_READ_ONLY: i32 = 7;
pub const TS_ERR_IO: i32 = 8;
pub const TS_ERR_INTERNAL: i32 = 99;

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
        E::NotFound => TS_ERR_NOT_FOUND,
        E::DuplicateTimeSeries => TS_ERR_DUPLICATE,
        E::InvalidParameter(_) => TS_ERR_INVALID_PARAMETER,
        E::IntegrityError(_) => TS_ERR_INTEGRITY,
        E::ReadOnlyStore => TS_ERR_READ_ONLY,
        E::Io(_) => TS_ERR_IO,
        _ => TS_ERR_INTERNAL,
    };
    set_error(e.to_string());
    code
}

// ---- Logging --------------------------------------------------------------

/// Initialize the Rust tracing subscriber.
///
/// `filter` is a null-terminated UTF-8 [`EnvFilter`] directive string, e.g.
/// `"debug"` or `"time_series_store_core=debug"`. Pass `NULL` to read the
/// `RUST_LOG` environment variable (or emit nothing if the variable is unset).
///
/// The subscriber is initialized at most once per process. Subsequent calls
/// are no-ops. Returns `TS_OK` on success, `TS_ERR_INVALID_UTF8` if `filter`
/// is not valid UTF-8, or `TS_ERR_INVALID_PARAMETER` if `filter` contains an
/// invalid directive (e.g. an unrecognised level name).
///
/// # Safety
///
/// `filter` must be a valid null-terminated UTF-8 string or `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_init_logging(filter: *const c_char) -> i32 {
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
                return TS_ERR_INVALID_PARAMETER;
            }
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
    TS_OK
}

// ---- Handles --------------------------------------------------------------

pub struct TsStoreHandle {
    inner: core_lib::Store,
}

pub struct TsKeyHandle {
    // A key handle is a lookup handle: it carries the identity tuple the catalog
    // resolves. Descriptive window fields (only known for a fully-described key
    // from add/list) are not carried here.
    inner: core_lib::KeyIdentity,
}

/// Accumulates pending add requests for a single all-or-nothing
/// `ts_store_add_batch` call. Building the batch performs no store I/O.
pub struct TsBatchHandle {
    items: Vec<core_lib::AddRequest>,
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> Result<&'a str, i32> {
    if p.is_null() {
        return Err(TS_ERR_NULL_POINTER);
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| TS_ERR_INVALID_UTF8)
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
/// string sets the error and returns `TS_ERR_INVALID_PARAMETER`.
unsafe fn cstr_to_optional_period(p: *const c_char) -> Result<Option<core_lib::Period>, i32> {
    let s = unsafe { cstr_to_optional_string(p)? };
    match s {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => core_lib::Period::from_iso8601(&s).map(Some).map_err(|e| {
            set_error(e.to_string());
            TS_ERR_INVALID_PARAMETER
        }),
    }
}

/// Parse a required ISO-8601 period from a C string.
unsafe fn cstr_to_period(p: *const c_char) -> Result<core_lib::Period, i32> {
    let s = unsafe { cstr_to_str(p)? };
    core_lib::Period::from_iso8601(s).map_err(|e| {
        set_error(e.to_string());
        TS_ERR_INVALID_PARAMETER
    })
}

/// Allocate an owned C string the caller must release with [`ts_string_free`].
/// An interior NUL (never present in an ISO-8601 period) yields a null pointer.
fn owned_cstr(s: &str) -> *mut c_char {
    match std::ffi::CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Owned ISO-8601 C string for a period (caller frees with [`ts_string_free`]).
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
pub unsafe extern "C" fn ts_string_free(s: *mut c_char) {
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
/// `ts_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_create(
    path: *const c_char,
    in_memory: bool,
    out: *mut *mut TsStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return TS_ERR_NULL_POINTER;
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
    let handle = Box::new(TsStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    TS_OK
}

/// Create a store with an explicit NetCDF compression policy.
///
/// `compression_kind` selects the filter: `0` = none (uncompressed), `1` =
/// DEFLATE at `deflate_level` (0–9) with byte `shuffle` when non-zero. Any
/// other `compression_kind` is rejected. The policy is ignored for in-memory
/// stores and persisted so later appends reuse it. Equivalent to
/// [`ts_store_create`] with `compression_kind = 1`, level 3, shuffle on.
///
/// # Safety
///
/// `out` must be valid for writing one pointer. When non-null, `path` must point to a valid,
/// null-terminated UTF-8 string. The returned handle must be released exactly once with
/// `ts_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_create_with_compression(
    path: *const c_char,
    in_memory: bool,
    compression_kind: u8,
    deflate_level: u8,
    shuffle: bool,
    out: *mut *mut TsStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return TS_ERR_NULL_POINTER;
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
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let store =
        match core_lib::create_store_with_compression(path.as_deref(), in_memory, compression) {
            Ok(s) => s,
            Err(e) => return map_core_error(e),
        };
    let handle = Box::new(TsStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    TS_OK
}

/// Open an existing time-series store and return an owning handle through `out`.
///
/// # Safety
///
/// `path` must point to a valid, null-terminated UTF-8 string, and `out` must be valid for writing
/// one pointer. The returned handle must be released exactly once with `ts_store_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_open(
    path: *const c_char,
    read_only: bool,
    out: *mut *mut TsStoreHandle,
) -> i32 {
    clear_error();
    if out.is_null() {
        set_error("out pointer is null");
        return TS_ERR_NULL_POINTER;
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
    let handle = Box::new(TsStoreHandle { inner: store });
    unsafe { *out = Box::into_raw(handle) };
    TS_OK
}

/// Release a store handle returned by `ts_store_create` or `ts_store_open`.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by this library that has not already been freed.
/// The handle must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_free(handle: *mut TsStoreHandle) {
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
            return Err(TS_ERR_INVALID_PARAMETER);
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
        TS_ERR_INVALID_PARAMETER
    })
}

/// Parse the `ts_store_add_single` / `ts_batch_add_single` argument list into
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(TS_ERR_NULL_POINTER);
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
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let logical_type = unsafe { cstr_to_optional_string(logical_type) }?;
    let features = unsafe { parse_features_json(features_json) }?;

    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(TS_ERR_INVALID_PARAMETER);
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

        logical_type,
    })
}

/// Add a SingleTimeSeries to the store.
///
/// `features_json`, when non-null, is parsed as a JSON object whose values must be int, float, or
/// bool. `logical_type` and `units` are optional.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required string
/// pointers must reference null-terminated UTF-8 strings; optional string pointers may be null.
/// `dims_ptr` must reference `ndims` elements when `ndims` is nonzero, and `data_ptr` must reference
/// `data_byte_len` bytes. `out_key` must be valid for writing one pointer. The returned key must be
/// released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_single(
    handle: *mut TsStoreHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- add_non_sequential --------------------------------------------------

/// Parse the `ts_store_add_non_sequential` / `ts_batch_add_non_sequential`
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if timestamps_unix_ms.is_null() || data_ptr.is_null() {
        set_error("an input pointer is null");
        return Err(TS_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(TS_ERR_INVALID_PARAMETER);
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
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let array = unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }?;
    let series = match core_lib::NonSequentialTimeSeries::new(timestamps, array, name) {
        Ok(series) => series,
        Err(error) => {
            set_error(error);
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let features = unsafe { parse_features_json(features_json) }?;
    let units = unsafe { cstr_to_optional_string(units) }?;
    let logical_type = unsafe { cstr_to_optional_string(logical_type) }?;
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data: core_lib::TimeSeriesData::NonSequentialTimeSeries(series),
        features,
        units,

        logical_type,
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
/// must be valid for writing one pointer. The returned key must be released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_non_sequential(
    handle: *mut TsStoreHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
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
            logical_type,
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
                *out_key = Box::into_raw(Box::new(TsKeyHandle {
                    inner: keys.remove(0).identity().clone(),
                }))
            };
            TS_OK
        }
        Err(error) => map_core_error(error),
    }
}

// ---- get_single -----------------------------------------------------------

/// Fetch a SingleTimeSeries by key in its native dtype and shape.
///
/// `out_dtype` receives the element dtype code (see [`ts_type_from_int`]'s dtype
/// siblings: f64=0, f32=1, i64=2, i32=3, u64=4, bool=5). `out_shape` /
/// `out_shape_len` return the full array shape `[length, *element_shape]` (the
/// first dim is time); `out_data` / `out_data_byte_len` return the raw
/// little-endian element bytes. The caller owns both buffers and must free
/// `*out_shape` with `ts_buffer_free_i64` and `*out_data` with
/// `ts_buffer_free_u8`, each using its returned length.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. The returned shape and data buffers must each be released
/// exactly once with the matching free function and returned length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_single(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
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
            return TS_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return TS_ERR_NULL_POINTER;
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
        return TS_ERR_NULL_POINTER;
    }
    let data = match store.inner.get_time_series(&key.inner, None) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    let single = match data {
        core_lib::TimeSeriesData::SingleTimeSeries(single) => single,
        core_lib::TimeSeriesData::NonSequentialTimeSeries(_) => {
            set_error("key does not identify a SingleTimeSeries");
            return TS_ERR_INVALID_PARAMETER;
        }
        // Forecast types are not yet exposed through this FFI entry point.
        core_lib::TimeSeriesData::Deterministic(_)
        | core_lib::TimeSeriesData::Probabilistic(_)
        | core_lib::TimeSeriesData::Scenarios(_) => {
            set_error("key identifies a forecast type; use the forecast FFI");
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let initial_ms = match datetime_to_unix_ms(single.initial_timestamp) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return TS_ERR_INTEGRITY;
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
    TS_OK
}

/// Fetch a NonSequentialTimeSeries by key.
///
/// `out_shape` returns the full array shape `[length, *element_shape]` (so callers can recover an
/// N-dimensional per-step element shape, e.g. a `(length, k)` FunctionData encoding); `out_dtype`
/// and `out_data` carry the row-major element bytes. `out_logical_type` is an optional opaque
/// element-typing tag (e.g. `"QuadraticFunctionData"`) copied into a caller-allocated buffer of
/// `logical_type_cap` bytes; the full length is reported in `out_logical_type_len` so the caller can
/// probe with a null/zero-capacity buffer first.
///
/// The caller owns the `out_timestamps`, `out_shape`, and `out_data` buffers and must release them
/// with `ts_buffer_free_i64`, `ts_buffer_free_i64`, and `ts_buffer_free_u8` respectively.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. Returned buffers must each be released exactly once with
/// the matching free function and returned length.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_non_sequential(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    out_timestamps: *mut *mut i64,
    out_timestamps_len: *mut u64,
    out_dtype: *mut i32,
    out_shape: *mut *mut i64,
    out_shape_len: *mut u64,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
    out_logical_type: *mut c_char,
    logical_type_cap: u64,
    out_logical_type_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(store) => store,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe { key.as_ref() } {
        Some(key) => key,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_timestamps.is_null()
        || out_timestamps_len.is_null()
        || out_dtype.is_null()
        || out_shape.is_null()
        || out_shape_len.is_null()
        || out_data.is_null()
        || out_data_byte_len.is_null()
        || out_logical_type_len.is_null()
    {
        return TS_ERR_NULL_POINTER;
    }
    let series = match store.inner.get_time_series(&key.inner, None) {
        Ok(core_lib::TimeSeriesData::NonSequentialTimeSeries(series)) => series,
        Ok(core_lib::TimeSeriesData::SingleTimeSeries(_)) => {
            set_error("key does not identify a NonSequentialTimeSeries");
            return TS_ERR_INVALID_PARAMETER;
        }
        // Forecast types are not yet exposed through this FFI entry point.
        Ok(
            core_lib::TimeSeriesData::Deterministic(_)
            | core_lib::TimeSeriesData::Probabilistic(_)
            | core_lib::TimeSeriesData::Scenarios(_),
        ) => {
            set_error("key identifies a forecast type; use the forecast FFI");
            return TS_ERR_INVALID_PARAMETER;
        }
        Err(error) => return map_core_error(error),
    };
    // The logical-type tag lives on the metadata row, not on the reconstructed series.
    let logical_type = match store.inner.get_metadata(&key.inner) {
        Ok(meta) => meta.logical_type.unwrap_or_default(),
        Err(error) => return map_core_error(error),
    };
    let mut timestamps = match series
        .timestamps
        .iter()
        .map(|timestamp| datetime_to_unix_ms(*timestamp).ok_or(TS_ERR_INTEGRITY))
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
        write_str_out(
            &logical_type,
            out_logical_type,
            logical_type_cap,
            out_logical_type_len,
        );
    }
    TS_OK
}

// ---- remove / has / counts / verify ---------------------------------------

/// Remove the time series identified by `key`.
///
/// # Safety
///
/// `handle` must be a live mutable store handle and `key` must be a live key handle created by this
/// library. Neither handle may be concurrently mutated for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_remove(
    handle: *mut TsStoreHandle,
    key: *const TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => return TS_ERR_NULL_POINTER,
    };
    match store.inner.remove_time_series(&key.inner) {
        Ok(()) => TS_OK,
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
pub unsafe extern "C" fn ts_store_has(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null() {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.has_time_series(&key.inner) {
        Ok(b) => {
            unsafe { *out_present = b };
            TS_OK
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
pub unsafe extern "C" fn ts_store_counts(
    handle: *const TsStoreHandle,
    out_components_with_time_series: *mut i64,
    out_static_time_series: *mut i64,
    out_forecasts: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_components_with_time_series.is_null()
        || out_static_time_series.is_null()
        || out_forecasts.is_null()
    {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.get_time_series_counts() {
        Ok(c) => {
            unsafe {
                *out_components_with_time_series = c.components_with_time_series;
                *out_static_time_series = c.static_time_series;
                *out_forecasts = c.forecasts;
            }
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Write the store's forecast parameters, optionally restricted to forecasts
/// with `filter_resolution` and/or `filter_interval` (empty/null = no filter).
///
/// `out_present` is set to `true` when a matching forecast exists, `false`
/// otherwise. Each of `out_horizon`, `out_interval`, `out_count`,
/// `out_resolution`, and `out_initial_ms` (the initial timestamp as unix ms)
/// receives the corresponding value, or `-1` when that field is absent
/// (durations, resolution, and counts are always non-negative when present, so
/// `-1` is an unambiguous "unset" sentinel).
///
/// # Safety
///
/// `handle` must be a live store handle; the filter args are plain scalars.
/// `out_present` must be valid for writing one `bool`; every other output pointer
/// must be valid for writing one `i64`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_forecast_parameters(
    handle: *const TsStoreHandle,
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
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null()
        || out_horizon.is_null()
        || out_interval.is_null()
        || out_count.is_null()
        || out_resolution.is_null()
        || out_initial_ms.is_null()
    {
        return TS_ERR_NULL_POINTER;
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
                // freed by the caller with `ts_string_free`.
                *out_horizon = opt_period_cstr(p.horizon);
                *out_interval = opt_period_cstr(p.interval);
                *out_count = p.count.map(|c| c as i64).unwrap_or(-1);
                *out_resolution = opt_period_cstr(p.resolution);
                *out_initial_ms = p
                    .initial_timestamp
                    .and_then(datetime_to_unix_ms)
                    .unwrap_or(-1);
            }
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Verify all `SingleTimeSeries` share one `(initial_timestamp, length)`.
///
/// `out_present` is `false` when the store has no `SingleTimeSeries`; otherwise
/// `true` and `out_initial_ms` / `out_length` receive the shared pair. Returns an
/// error when more than one distinct pair exists (the catalog is inconsistent).
///
/// # Safety
///
/// `handle` must be a live store handle. Each out pointer must be valid for one
/// write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_check_static_consistency(
    handle: *const TsStoreHandle,
    out_present: *mut bool,
    out_initial_ms: *mut i64,
    out_length: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null() || out_initial_ms.is_null() || out_length.is_null() {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.check_static_consistency() {
        Ok(None) => {
            unsafe {
                *out_present = false;
                *out_initial_ms = 0;
                *out_length = 0;
            }
            TS_OK
        }
        Ok(Some((ts, len))) => {
            unsafe {
                *out_present = true;
                *out_initial_ms = datetime_to_unix_ms(ts).unwrap_or(0);
                *out_length = len as i64;
            }
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// List the distinct resolutions present in the store as a JSON array of integer
/// milliseconds (ascending). When `has_time_series_type` is true the listing is
/// restricted to that `TS_TYPE_*` code; otherwise all types are considered.
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
pub unsafe extern "C" fn ts_store_get_resolutions(
    handle: *const TsStoreHandle,
    has_time_series_type: bool,
    time_series_type: i32,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
    }
    let ts_type = if has_time_series_type {
        match ts_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return TS_ERR_INVALID_PARAMETER;
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
    TS_OK
}

/// Association count grouped by time series type, as a JSON array of
/// `{"time_series_type": <name>, "count": <n>}` objects. Probe-then-fetch (see
/// `ts_store_list_keys`).
///
/// # Safety
///
/// `handle` must be a live store handle. `out_len` must be writable; `buf` must be
/// null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_counts_by_type(
    handle: *const TsStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
}

/// Write the number of distinct stored arrays (content hashes); shared series
/// count once.
///
/// # Safety
///
/// `handle` must be a live store handle. `out_count` must be valid for writing one
/// `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_num_distinct_arrays(
    handle: *const TsStoreHandle,
    out_count: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_count.is_null() {
        set_error("out_count is null");
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.num_distinct_arrays() {
        Ok(n) => {
            unsafe { *out_count = n };
            TS_OK
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
pub unsafe extern "C" fn ts_store_counts_detailed(
    handle: *const TsStoreHandle,
    out_components: *mut i64,
    out_supplemental_attributes: *mut i64,
    out_static_time_series: *mut i64,
    out_forecasts: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_components.is_null()
        || out_supplemental_attributes.is_null()
        || out_static_time_series.is_null()
        || out_forecasts.is_null()
    {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.time_series_counts_detailed() {
        Ok(c) => {
            unsafe {
                *out_components = c.components_with_time_series;
                *out_supplemental_attributes = c.supplemental_attributes_with_time_series;
                *out_static_time_series = c.static_time_series_count;
                *out_forecasts = c.forecast_count;
            }
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// List the distinct owner ids of `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) that have a time series, as a JSON array of integers.
/// Optionally restricted to one `time_series_type` (`TS_TYPE_*` code, gated by
/// `has_time_series_type`) and/or `resolution` (empty/null = no filter).
/// Probe-then-fetch (see `ts_store_list_keys`).
///
/// # Safety
///
/// `handle` must be a live store handle; the filter args are plain scalars.
/// `out_len` must be writable; `buf` must be null or valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_list_owner_ids(
    handle: *const TsStoreHandle,
    owner_category: i32,
    has_time_series_type: bool,
    time_series_type: i32,
    resolution: *const c_char,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let ts_type = if has_time_series_type {
        match ts_type_from_int(time_series_type) {
            Some(t) => Some(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return TS_ERR_INVALID_PARAMETER;
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
    TS_OK
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
pub unsafe extern "C" fn ts_store_static_summary(
    handle: *const TsStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
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
pub unsafe extern "C" fn ts_store_forecast_summary(
    handle: *const TsStoreHandle,
    buf: *mut c_char,
    cap: u64,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
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
pub unsafe extern "C" fn ts_store_get_compression(
    handle: *const TsStoreHandle,
    out_kind: *mut u8,
    out_level: *mut u8,
    out_shuffle: *mut bool,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_kind.is_null() || out_level.is_null() || out_shuffle.is_null() {
        return TS_ERR_NULL_POINTER;
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
    TS_OK
}

/// Verify store integrity and return the number of detected errors.
///
/// # Safety
///
/// `handle` must be a live store handle and `out_error_count` must be valid for writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_verify(
    handle: *const TsStoreHandle,
    out_error_count: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_error_count.is_null() {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.verify_integrity() {
        Ok(r) => {
            unsafe { *out_error_count = r.errors.len() as u64 };
            TS_OK
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
pub unsafe extern "C" fn ts_store_compact(handle: *mut TsStoreHandle) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    match store.inner.compact() {
        Ok(_) => TS_OK,
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
pub unsafe extern "C" fn ts_store_flush(handle: *mut TsStoreHandle) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    match store.inner.flush() {
        Ok(()) => TS_OK,
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
            return Err(TS_ERR_INVALID_PARAMETER);
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
        features,
    })
}

/// Look up a SingleTimeSeries metadata record by attributes. On success the
/// caller's out-params receive the initial timestamp, resolution, length, and
/// the 32-byte content hash (written into the `out_data_hash` buffer, which
/// must have room for 32 bytes). Returns `TS_ERR_NOT_FOUND` if absent.
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. Scalar output pointers must be valid for one
/// value and `out_data_hash` must be valid for 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_metadata(
    handle: *const TsStoreHandle,
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
    out_logical_type: *mut c_char,
    logical_type_cap: u64,
    out_logical_type_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_dtype.is_null()
        || out_logical_type_len.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
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
            return TS_ERR_INTEGRITY;
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
    // logical_type (optional): copy up to cap-1 bytes + NUL; report the full length.
    let lt = meta.logical_type.unwrap_or_default();
    let lt_bytes = lt.as_bytes();
    unsafe {
        *out_logical_type_len = lt_bytes.len() as u64;
        if !out_logical_type.is_null() && logical_type_cap > 0 {
            let n = lt_bytes.len().min((logical_type_cap - 1) as usize);
            ptr::copy_nonoverlapping(lt_bytes.as_ptr(), out_logical_type as *mut u8, n);
            *out_logical_type.add(n) = 0;
        }
    }
    TS_OK
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
pub unsafe extern "C" fn ts_store_has_by_attrs(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null() {
        return TS_ERR_NULL_POINTER;
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
            TS_OK
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
pub unsafe extern "C" fn ts_store_has_for_owner(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    ts_type: i32,
    use_type: bool,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null() {
        return TS_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let mut filter = core_lib::ListFilter::new()
        .owner_id(owner_id)
        .owner_category(category);
    if use_type {
        let t = match ts_type_from_int(ts_type) {
            Some(t) => t,
            None => {
                set_error(format!("invalid time_series_type {ts_type}"));
                return TS_ERR_INVALID_PARAMETER;
            }
        };
        filter = filter.time_series_type(t);
    }
    match store.inner.list_time_series(filter) {
        Ok(list) => {
            unsafe { *out_present = !list.is_empty() };
            TS_OK
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
pub unsafe extern "C" fn ts_store_remove_by_attrs(
    handle: *mut TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe {
        build_key_from_attrs(owner_id, owner_category, name, resolution, features_json)
    } {
        Ok(k) => k,
        Err(code) => return code,
    };
    match store.inner.remove_time_series(&key) {
        Ok(()) => TS_OK,
        Err(e) => map_core_error(e),
    }
}

/// Fetch a stored array by its 32-byte content hash. On success the caller owns
/// `*out_data` and must free it with `ts_buffer_free_u8`.
///
/// # Safety
///
/// `handle` must be a live store handle, `data_hash` must reference 32 readable bytes, and every
/// output pointer must be valid for writing its indicated value. The returned buffer must be
/// released exactly once with `ts_buffer_free_u8` using the returned byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_array_by_hash(
    handle: *const TsStoreHandle,
    data_hash: *const u8,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if data_hash.is_null() || out_dtype.is_null() || out_data.is_null() || out_byte_len.is_null() {
        set_error("a pointer is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
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
pub unsafe extern "C" fn ts_store_count_array_references(
    handle: *const TsStoreHandle,
    data_hash: *const u8,
    out_sts: *mut u64,
    out_dst: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if data_hash.is_null() || out_sts.is_null() || out_dst.is_null() {
        set_error("a pointer is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
}

// ---- Forecasts (Deterministic / DeterministicSingleTimeSeries / ...) -------

fn ts_type_from_int(i: i32) -> Option<core_lib::TimeSeriesType> {
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
pub const TS_TYPE_ABSTRACT_DETERMINISTIC: i32 = 100;

/// Map a forecast read request's `ts_type` code to a [`core_lib::RequestedType`]:
/// a concrete forecast type (2..=5) or the [`TS_TYPE_ABSTRACT_DETERMINISTIC`]
/// family. The non-forecast types `SingleTimeSeries` (0) and
/// `NonSequentialTimeSeries` (1) are rejected here so the forecast API reports a
/// clear "invalid time_series_type" error up front rather than failing later in
/// `emit_forecast_data` after a key is resolved and data is read.
fn requested_type_from_int(i: i32) -> Option<core_lib::RequestedType> {
    use core_lib::TimeSeriesType as T;
    if i == TS_TYPE_ABSTRACT_DETERMINISTIC {
        return Some(core_lib::RequestedType::AbstractDeterministic);
    }
    match ts_type_from_int(i) {
        Some(
            t @ (T::Deterministic
            | T::DeterministicSingleTimeSeries
            | T::Probabilistic
            | T::Scenarios),
        ) => Some(core_lib::RequestedType::Concrete(t)),
        _ => None,
    }
}

/// Inverse of [`ts_type_from_int`]: the integer discriminant for a
/// `TimeSeriesType` (must stay in sync with that mapping).
fn ts_type_to_int(t: core_lib::TimeSeriesType) -> i32 {
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

unsafe fn build_typed_key_from_attrs(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    features_json: *const c_char,
) -> Result<core_lib::KeyIdentity, i32> {
    let time_series_type = match ts_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let mut key =
        unsafe { build_key_from_attrs(owner_id, owner_category, name, resolution, features_json) }?;
    key.time_series_type = time_series_type;
    Ok(key)
}

/// Add a dense forecast. `data_ptr`/`data_byte_len` is the flattened storage
/// array (Deterministic: `[H, count, *E]`; Scenarios: `[scenario_count, H,
/// count, *E]`). `ts_type` must be 2=Deterministic or 5=Scenarios;
/// `DeterministicSingleTimeSeries` is not directly addable and is derived from a
/// stored `SingleTimeSeries` via `ts_store_transform_single_time_series`.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `owner_id` is a plain integer. Required strings
/// must be null-terminated UTF-8; optional strings may be null. `data_ptr` must reference `data_len`
/// elements and `out_key` must be valid for writing one pointer. The returned key must be released
/// with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_forecast(
    handle: *mut TsStoreHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `ts_store_add_forecast` / `ts_batch_add_forecast` argument list
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return Err(TS_ERR_NULL_POINTER);
    }
    let time_series_type = match ts_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let logical_type = unsafe { cstr_to_optional_string(logical_type) }?;
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
                return Err(TS_ERR_INVALID_PARAMETER);
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
                    return Err(TS_ERR_INVALID_PARAMETER);
                }
            }
        }
        other => {
            set_error(format!(
                "ts_store_add_forecast supports Deterministic and Scenarios; {other:?} \
                 is not directly addable (DeterministicSingleTimeSeries is derived via \
                 ts_store_transform_single_time_series)"
            ));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };

    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data,
        features,
        units,

        logical_type,
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
/// pointer. The returned key must be released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_probabilistic(
    handle: *mut TsStoreHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(r) => r,
        Err(c) => return c,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0).identity().clone(),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Parse the `ts_store_add_probabilistic` / `ts_batch_add_probabilistic`
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> Result<core_lib::AddRequest, i32> {
    if data_ptr.is_null() || percentiles_ptr.is_null() {
        set_error("a required pointer is null");
        return Err(TS_ERR_NULL_POINTER);
    }
    let owner_type = unsafe { cstr_to_str(owner_type) }?;
    let name = unsafe { cstr_to_str(name) }?;
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let units = unsafe { cstr_to_optional_string(units) }?;
    let features = unsafe { parse_features_json(features_json) }?;
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let percentiles =
        unsafe { slice::from_raw_parts(percentiles_ptr, percentiles_len as usize) }.to_vec();
    let logical_type = unsafe { cstr_to_optional_string(logical_type) }?;
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
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    Ok(core_lib::AddRequest {
        owner_id,
        owner_type: owner_type.to_string(),
        owner_category,
        data: core_lib::TimeSeriesData::Probabilistic(prob),
        features,
        units,

        logical_type,
    })
}

// ---- batched adds ----------------------------------------------------------
//
// A batch accumulates AddRequests client-side; `ts_store_add_batch` commits
// them through `Store::add_time_series_bulk` in ONE metadata transaction.
// This is the fast path for ingesting many series: per-item adds pay one
// SQLite commit each, while a batch pays a single commit for all items.

/// Create an empty add-batch. Building a batch performs no store I/O.
///
/// # Safety
///
/// The returned handle must be released exactly once with `ts_batch_free`
/// (regardless of whether it was submitted via `ts_store_add_batch`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_batch_new() -> *mut TsBatchHandle {
    Box::into_raw(Box::new(TsBatchHandle { items: Vec::new() }))
}

/// Free a batch handle created by `ts_batch_new`.
///
/// # Safety
///
/// `batch` must be null or a handle returned by `ts_batch_new` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_batch_free(batch: *mut TsBatchHandle) {
    if !batch.is_null() {
        drop(unsafe { Box::from_raw(batch) });
    }
}

/// Append a SingleTimeSeries to a batch. Arguments match
/// `ts_store_add_single` (minus the store handle and `out_key`); the data is
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
pub unsafe extern "C" fn ts_batch_add_single(
    batch: *mut TsBatchHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            TS_OK
        }
        Err(c) => c,
    }
}

/// Append a NonSequentialTimeSeries to a batch. Arguments match
/// `ts_store_add_non_sequential` (minus the store handle and `out_key`).
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
pub unsafe extern "C" fn ts_batch_add_non_sequential(
    batch: *mut TsBatchHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            TS_OK
        }
        Err(c) => c,
    }
}

/// Append a dense forecast (`ts_type` 2=Deterministic or 5=Scenarios) to a
/// batch. Arguments match `ts_store_add_forecast` (minus the store handle and
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
pub unsafe extern "C" fn ts_batch_add_forecast(
    batch: *mut TsBatchHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            TS_OK
        }
        Err(c) => c,
    }
}

/// Append a `Probabilistic` forecast to a batch. Arguments match
/// `ts_store_add_probabilistic` (minus the store handle and `out_key`).
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
pub unsafe extern "C" fn ts_batch_add_probabilistic(
    batch: *mut TsBatchHandle,
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
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
) -> i32 {
    clear_error();
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return TS_ERR_NULL_POINTER;
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
            logical_type,
            features_json,
            units,
        )
    } {
        Ok(req) => {
            batch.items.push(req);
            TS_OK
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
/// release each key with `ts_key_free`, then the array buffer itself with
/// `ts_keys_buffer_free(*out_keys, *out_len)` (the same contract as
/// `ts_store_get_time_series_keys`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_add_batch(
    handle: *mut TsStoreHandle,
    batch: *mut TsBatchHandle,
    out_keys: *mut *mut *mut TsKeyHandle,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let batch = match unsafe { batch.as_mut() } {
        Some(b) => b,
        None => {
            set_error("batch handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_keys.is_null() || out_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let items = std::mem::take(&mut batch.items);
    match store.inner.add_time_series_bulk(items) {
        Ok(keys) => {
            let mut handles: Vec<*mut TsKeyHandle> = keys
                .into_iter()
                .map(|k| {
                    Box::into_raw(Box::new(TsKeyHandle {
                        inner: k.identity().clone(),
                    }))
                })
                .collect();
            // Keep capacity == length so `ts_keys_buffer_free` can reconstruct the Vec.
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
            TS_OK
        }
        Err(e) => map_core_error(e),
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
pub unsafe extern "C" fn ts_store_transform_single_time_series(
    handle: *mut TsStoreHandle,
    horizon: *const c_char,
    interval: *const c_char,
    owner_category: i32,
    resolution: *const c_char,
    out_count: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_count.is_null() {
        set_error("a required pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    // `owner_category < 0` means "all categories"; an empty `resolution` means
    // "all resolutions".
    let category = match owner_category {
        c if c < 0 => None,
        0 => Some(core_lib::OwnerCategory::Component),
        1 => Some(core_lib::OwnerCategory::SupplementalAttribute),
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
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
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Read `Probabilistic` metadata. Like `ts_store_get_forecast_metadata` but also
/// returns the percentiles vector in `*out_percentiles` (caller frees with
/// `ts_buffer_free_f64`).
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. Scalar output pointers must each be valid for
/// one value, `out_data_hash` must be valid for 32 bytes, and `out_percentiles` must be valid for
/// writing one pointer. The returned percentile buffer must be released exactly once with
/// `ts_buffer_free_f64` using the returned length.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_probabilistic_metadata(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
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
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_percentiles.is_null() || out_percentiles_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            4, // Probabilistic
            resolution,
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
            return TS_ERR_INTEGRITY;
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
    }
    TS_OK
}

/// Read forecast metadata by attributes. Out-params receive initial timestamp,
/// resolution, horizon, interval, count, the stored array length, and the
/// 32-byte content hash (into `out_data_hash`).
///
/// # Safety
///
/// `handle` must be a live store handle. `owner_id` and `owner_category` (`0` =
/// Component, `1` = SupplementalAttribute) identify the owner. Required strings must be
/// null-terminated UTF-8; `features_json` may be null. Scalar output pointers must each be valid for
/// one value and `out_data_hash` must be valid for 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_forecast_metadata(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_horizon: *mut *mut c_char,
    out_interval: *mut *mut c_char,
    out_count: *mut u64,
    out_length: *mut u64,
    out_data_hash: *mut u8,
    logical_type_buf: *mut c_char,
    logical_type_cap: u64,
    out_logical_type_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_initial_ts_unix_ms.is_null()
        || out_resolution.is_null()
        || out_horizon.is_null()
        || out_interval.is_null()
        || out_count.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_logical_type_len.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            ts_type,
            resolution,
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
            return TS_ERR_INTEGRITY;
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
            meta.logical_type.as_deref().unwrap_or(""),
            logical_type_buf,
            logical_type_cap,
            out_logical_type_len,
        );
    }
    TS_OK
}

/// Fetch a forecast by attributes and return the full data array plus metadata.
///
/// `ts_type` is a read request: a concrete type (`2`=Deterministic,
/// `3`=DeterministicSingleTimeSeries, `4`=Probabilistic, `5`=Scenarios) or the
/// `TS_TYPE_ABSTRACT_DETERMINISTIC` (`100`) family, which matches a stored
/// `Deterministic` *or* `DeterministicSingleTimeSeries`. The catalog resolves the
/// family authoritatively — no client-side guess-and-retry — and writes the
/// concrete type that matched to `*out_matched_type`. An ambiguous family request
/// (both concrete types share the identity) returns `TS_ERR_INVALID_PARAMETER`;
/// a genuine miss returns the unmasked not-found error.
///
/// Reads a `Deterministic`, `Probabilistic`, or `Scenarios` forecast (DST is
/// synthesized into `Deterministic`). On success, the caller owns two heap
/// buffers and must free them with the matching deallocators:
///
/// - `*out_data` (byte buffer, `*out_data_byte_len` bytes) —
///   free with `ts_buffer_free_u8(*out_data, *out_data_byte_len)`.
/// - `*out_dims` (array of `u64`, `*out_ndims` elements) —
///   free with `ts_buffer_free_u64(*out_dims, *out_ndims)`.
/// - `*out_percentiles` (`f64` array, `*out_percentiles_len` elements) —
///   non-NULL only for `Probabilistic`; free with
///   `ts_buffer_free_f64(*out_percentiles, *out_percentiles_len)`.
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
/// - All `out_*` scalar pointers, including `out_matched_type`, must be valid
///   for writing one value each.
/// - `out_dims` must be valid for writing one pointer; the returned pointer
///   must be freed exactly once with `ts_buffer_free_u64` using `*out_ndims`.
/// - `out_data` must be valid for writing one pointer; the returned pointer
///   must be freed exactly once with `ts_buffer_free_u8` using
///   `*out_data_byte_len`.
/// - `out_percentiles` must be valid for writing one pointer; when the result
///   is not `Probabilistic` the pointer is set to null and `*out_percentiles_len`
///   to 0, so no free is needed. When non-null it must be freed exactly once
///   with `ts_buffer_free_f64` using `*out_percentiles_len`.
/// - All returned heap buffers are invalidated after their matching free call
///   and must not be used afterwards.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_forecast(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
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
            return TS_ERR_NULL_POINTER;
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
        return TS_ERR_NULL_POINTER;
    }
    let requested = match requested_type_from_int(ts_type) {
        Some(r) => r,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return TS_ERR_INVALID_PARAMETER;
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
    let key = match store.inner.resolve_forecast_key(
        attrs.owner_id,
        attrs.owner_category,
        &attrs.name,
        attrs.resolution,
        attrs.features,
        requested,
    ) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let matched_type = ts_type_to_int(key.time_series_type());
    let time_range = if time_range_present {
        let start = match unix_ms_to_datetime(time_range_start_ms) {
            Some(d) => d,
            None => {
                set_error(format!(
                    "invalid time_range_start_ms: {time_range_start_ms}"
                ));
                return TS_ERR_INVALID_PARAMETER;
            }
        };
        let end = match unix_ms_to_datetime(time_range_end_ms) {
            Some(d) => d,
            None => {
                set_error(format!("invalid time_range_end_ms: {time_range_end_ms}"));
                return TS_ERR_INVALID_PARAMETER;
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
/// used by [`ts_store_get_forecast`] and [`ts_store_get_forecast_by_key`].
///
/// # Safety
///
/// All out pointers must be non-null and valid for writing their indicated
/// values (the callers null-check them). The returned `out_dims`, `out_data`,
/// and (for `Probabilistic`) `out_percentiles` buffers are heap-allocated and
/// must be released by the caller with the matching `ts_buffer_free_*` function.
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
                    return TS_ERR_INTEGRITY;
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
            TS_OK
        }
        core_lib::TimeSeriesData::Probabilistic(prob) => {
            let initial_ms = match datetime_to_unix_ms(prob.initial_timestamp) {
                Some(n) => n,
                None => {
                    set_error("initial_timestamp out of i64 millisecond range");
                    return TS_ERR_INTEGRITY;
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
            TS_OK
        }
        core_lib::TimeSeriesData::Scenarios(scen) => {
            let initial_ms = match datetime_to_unix_ms(scen.initial_timestamp) {
                Some(n) => n,
                None => {
                    set_error("initial_timestamp out of i64 millisecond range");
                    return TS_ERR_INTEGRITY;
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
            TS_OK
        }
        other => {
            set_error(format!(
                "key identifies a {} time series; use the matching read function",
                other.time_series_type().as_str()
            ));
            TS_ERR_INVALID_PARAMETER
        }
    }
}

/// Fetch a forecast (`Deterministic` / `Probabilistic` / `Scenarios`, or a
/// `DeterministicSingleTimeSeries` synthesized into a `Deterministic`) by key.
///
/// This is the key-based counterpart to [`ts_store_get_forecast`]: the time
/// series type comes from `key` rather than an explicit `ts_type` argument. The
/// outputs and buffer-ownership rules are identical to [`ts_store_get_forecast`];
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
///   be freed exactly once with `ts_buffer_free_u64` using `*out_ndims`.
/// - `out_data` must be valid for writing one pointer; the returned pointer must
///   be freed exactly once with `ts_buffer_free_u8` using `*out_data_byte_len`.
/// - `out_percentiles` must be valid for writing one pointer; when the result is
///   not `Probabilistic` the pointer is set to null and `*out_percentiles_len`
///   to 0, so no free is needed. When non-null it must be freed exactly once
///   with `ts_buffer_free_f64` using `*out_percentiles_len`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_forecast_by_key(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
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
    // `ts_store_get_forecast`)
    out_matched_type: *mut i32,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return TS_ERR_NULL_POINTER;
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
        return TS_ERR_NULL_POINTER;
    }
    let matched_type = ts_type_to_int(key.inner.time_series_type);
    let time_range = if time_range_present {
        let start = match unix_ms_to_datetime(time_range_start_ms) {
            Some(d) => d,
            None => {
                set_error(format!(
                    "invalid time_range_start_ms: {time_range_start_ms}"
                ));
                return TS_ERR_INVALID_PARAMETER;
            }
        };
        let end = match unix_ms_to_datetime(time_range_end_ms) {
            Some(d) => d,
            None => {
                set_error(format!("invalid time_range_end_ms: {time_range_end_ms}"));
                return TS_ERR_INVALID_PARAMETER;
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
/// [`ts_store_get_single`], [`ts_store_get_non_sequential`],
/// [`ts_store_get_forecast_by_key`]); it lets an attribute-addressed caller
/// reuse the key-based read path without an `add`/lookup round trip.
/// an empty `resolution` means "unspecified".
///
/// # Safety
///
/// `owner_id` and `owner_category` (`0` = Component, `1` = SupplementalAttribute)
/// identify the owner. `name` must point to a valid, null-terminated
/// UTF-8 string. `features_json`, when non-null, must be a null-terminated UTF-8
/// JSON object. `out_key` must be valid for writing one pointer. The returned key
/// must be released exactly once with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_make_key_from_attrs(
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    features_json: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            ts_type,
            resolution,
            features_json,
        )
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    let handle = Box::new(TsKeyHandle { inner: key });
    unsafe { *out_key = Box::into_raw(handle) };
    TS_OK
}

/// List every time series key associated with `owner_id`. On success
/// `*out_keys` points to an array of `*out_len` owned key handles (one per
/// association, including derived `DeterministicSingleTimeSeries` rows), each
/// usable with the key-based read functions.
///
/// Ownership is two-tiered: free every individual `TsKey` with `ts_key_free`,
/// then free the array buffer itself with `ts_keys_buffer_free`. When the owner
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
pub unsafe extern "C" fn ts_store_get_time_series_keys(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    out_keys: *mut *mut *mut TsKeyHandle,
    out_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_keys.is_null() || out_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let keys = match store.inner.get_time_series_keys(owner_id, category) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let mut handles: Vec<*mut TsKeyHandle> = keys
        .into_iter()
        .map(|k| {
            Box::into_raw(Box::new(TsKeyHandle {
                inner: k.identity().clone(),
            }))
        })
        .collect();
    // Keep capacity == length so `ts_keys_buffer_free` can reconstruct the Vec.
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
    TS_OK
}

/// Encode metadata rows as a JSON array string. Each element carries the
/// association's owner + addressing fields and the temporal parameters the
/// binding needs to reconstruct a `TimeSeriesMetadata`. Durations are emitted
/// as integer milliseconds, `initial_timestamp_ms` as Unix epoch milliseconds,
/// and `data_hash` as a byte array; absent optionals are `null`.
// Serialize keys to a JSON array. Each object carries the identity tuple
// (`owner_id`, `owner_category`, `time_series_type`, `name`, `resolution`,
// `features`) plus the per-variant descriptive snapshot. Physical storage detail
// (`data_hash`, `dtype`, `logical_type`, `percentiles`) is deliberately absent —
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
            Some(f.interval),
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

/// List time series keys as a JSON array string (see `keys_to_json` for the
/// per-key shape). Every filter is optional and independent; with none set the
/// whole store is listed. A `has_*` flag of `false` (or a null string pointer)
/// disables that filter:
/// - `owner_id` / `owner_category` (`0` = Component, `1` = SupplementalAttribute)
/// - `time_series_type` (the `TS_TYPE_*` code)
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
pub unsafe extern "C" fn ts_store_list_keys(
    handle: *const TsStoreHandle,
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
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
}

/// Build a [`core_lib::ListFilter`] from the optional scalar/string filter args
/// shared by `ts_store_list_keys` and `ts_store_list_array_groups`. On a bad
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
                return Err(TS_ERR_INVALID_PARAMETER);
            }
        };
        filter = filter.owner_category(category);
    }
    if has_time_series_type {
        match ts_type_from_int(time_series_type) {
            Some(t) => filter = filter.time_series_type(t),
            None => {
                set_error(format!("invalid time_series_type {time_series_type}"));
                return Err(TS_ERR_INVALID_PARAMETER);
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
/// `ts_store_list_keys`.
///
/// # Safety
///
/// Identical to `ts_store_list_keys`: `handle` must be a live store handle;
/// `name` / `features_json` / `resolution` must each be null or a
/// null-terminated UTF-8 string; `out_len` must be writable; `buf` must be null
/// or valid for `cap` bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_list_array_groups(
    handle: *const TsStoreHandle,
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
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_len.is_null() {
        set_error("out_len is null");
        return TS_ERR_NULL_POINTER;
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
    TS_OK
}

/// Free the key-handle array returned by `ts_store_get_time_series_keys`.
///
/// This releases only the array buffer, not the keys it held: transfer each
/// `TsKey` out first (the Julia binding wraps each in a finalized object) and
/// release them individually with `ts_key_free`.
///
/// # Safety
///
/// `ptr` must be null or an array returned by `ts_store_get_time_series_keys`
/// with exactly `len` elements, not previously freed. It must not be used after
/// this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_keys_buffer_free(ptr: *mut *mut TsKeyHandle, len: u64) {
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
/// `ts_type_from_int`), resolution in milliseconds (`0` when unset), the owner
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
pub unsafe extern "C" fn ts_key_attributes(
    key: *const TsKeyHandle,
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
            return TS_ERR_NULL_POINTER;
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
        return TS_ERR_NULL_POINTER;
    }
    let k = &key.inner;
    let category_code = match k.owner_category {
        core_lib::OwnerCategory::Component => 0,
        core_lib::OwnerCategory::SupplementalAttribute => 1,
    };
    unsafe {
        *out_type = ts_type_to_int(k.time_series_type);
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
    TS_OK
}

/// Read an association's `name` by key, resolved through the stored metadata
/// (`Store::get_metadata`). This surfaces the per-association `name` that is not
/// carried on the key itself — the read path uses it to populate the returned
/// time series object.
///
/// `name` uses the probe-then-fetch convention (see [`ts_key_attributes`]).
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library.
/// `out_name_len` must be valid for writing one `u64`. `name_buf` may be null;
/// when non-null it must be valid for writing `name_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_association(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    name_buf: *mut c_char,
    name_cap: u64,
    out_name_len: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let key = match unsafe { key.as_ref() } {
        Some(k) => k,
        None => {
            set_error("key handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_name_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let meta = match store.inner.get_metadata(&key.inner) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        write_str_out(&meta.name, name_buf, name_cap, out_name_len);
    }
    TS_OK
}

/// Release a `u64` dims buffer returned by `ts_store_get_forecast`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_buffer_free_u64(ptr: *mut u64, len: u64) {
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
pub unsafe extern "C" fn ts_store_has_typed(
    handle: *const TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    features_json: *const c_char,
    out_present: *mut bool,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null() {
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            ts_type,
            resolution,
            features_json,
        )
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    match store.inner.has_time_series(&key) {
        Ok(b) => {
            unsafe { *out_present = b };
            TS_OK
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
pub unsafe extern "C" fn ts_store_remove_typed(
    handle: *mut TsStoreHandle,
    owner_id: i64,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    resolution: *const c_char,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe {
        build_typed_key_from_attrs(
            owner_id,
            owner_category,
            name,
            ts_type,
            resolution,
            features_json,
        )
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    match store.inner.remove_time_series(&key) {
        Ok(()) => TS_OK,
        Err(e) => map_core_error(e),
    }
}

/// Remove all time series, or all for a single owner when `has_owner` is true.
/// Returns `TS_OK` on success.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `has_owner`, `owner_id`, and
/// `owner_category` are plain scalars; when `has_owner` is true `owner_category`
/// (`0` = Component, `1` = SupplementalAttribute) scopes the clear to one owner.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_clear(
    handle: *mut TsStoreHandle,
    has_owner: bool,
    owner_id: i64,
    owner_category: i32,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let owner = if has_owner {
        let category = match owner_category {
            0 => core_lib::OwnerCategory::Component,
            1 => core_lib::OwnerCategory::SupplementalAttribute,
            other => {
                set_error(format!("invalid owner_category {other}"));
                return TS_ERR_INVALID_PARAMETER;
            }
        };
        Some((owner_id, category))
    } else {
        None
    };
    match store.inner.clear_time_series(owner) {
        Ok(_) => TS_OK,
        Err(e) => map_core_error(e),
    }
}

/// Reassign every time series owned by `old_owner_id` to `new_owner_id`.
/// When `out_updated` is non-null it receives the number of associations
/// changed. Returns `TS_OK` on success.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. `old_owner_id` and
/// `new_owner_id` are plain integers; `owner_category` (`0` = Component, `1` =
/// SupplementalAttribute) identifies the owner category. When non-null,
/// `out_updated` must point to writable `u64` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_replace_owner(
    handle: *mut TsStoreHandle,
    old_owner_id: i64,
    new_owner_id: i64,
    owner_category: i32,
    out_updated: *mut u64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
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
            TS_OK
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
pub unsafe extern "C" fn ts_key_free(key: *mut TsKeyHandle) {
    if !key.is_null() {
        unsafe { drop(Box::from_raw(key)) };
    }
}

/// Release an `f64` buffer returned by this library.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_buffer_free_f64(ptr: *mut f64, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

/// Free a `u8` buffer returned by `ts_store_get_array_by_hash`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` bytes. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_buffer_free_u8(ptr: *mut u8, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

/// Free an `i64` buffer returned by `ts_store_get_non_sequential`.
///
/// # Safety
///
/// `ptr` must be null or a buffer returned by this library with exactly `len` elements. It must not
/// have been freed previously and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_buffer_free_i64(ptr: *mut i64, len: u64) {
    if !ptr.is_null() {
        let len = len as usize;
        unsafe { drop(Vec::from_raw_parts(ptr, len, len)) };
    }
}

// ---- Error message --------------------------------------------------------

/// Copy the thread-local error message into `buf` (UTF-8, null-terminated).
/// Returns the number of bytes that would have been written (excluding the NUL)
/// in `*needed`. If `buf_len` is too small, `buf` is filled up to its length
/// and truncated; the function still returns `TS_OK` and the caller can decide
/// whether to retry with a larger buffer.
///
/// # Safety
///
/// `needed` may be null; otherwise it must be valid for writing one `u64`. `buf` may be null when
/// `buf_len` is zero; otherwise it must reference at least `buf_len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_last_error_message(
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
        return TS_OK;
    }
    let max_copy = std::cmp::min(buf_len.saturating_sub(1) as usize, bytes.len());
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, max_copy);
        *buf.add(max_copy) = 0;
    }
    TS_OK
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
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            set_error("features_json must be an object");
            return Err(TS_ERR_INVALID_PARAMETER);
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
                    return Err(TS_ERR_INVALID_PARAMETER);
                }
            }
            Value::String(s) => core_lib::FeatureValue::Str(s.clone()),
            other => {
                set_error(format!(
                    "feature {k}: must be int/float/bool/string, got {}",
                    type_name(other)
                ));
                return Err(TS_ERR_INVALID_PARAMETER);
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
pub struct TsStaticReaderHandle {
    inner: core_lib::StaticReader,
}

/// Opaque handle wrapping a core `ForecastReader` (one forecast type, per-key
/// windows).
pub struct TsForecastReaderHandle {
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
                return Err(TS_ERR_INVALID_PARAMETER);
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

/// Build a [`TsStaticReaderHandle`] over the SingleTimeSeries matching the
/// filter. `resolution` must be a non-empty ISO-8601 period (one resolution per reader); the
/// matched series must share one grid (`initial_timestamp` + `length`).
///
/// # Safety
///
/// `handle` must be a live store handle. `name` / `features_json` must be null
/// or valid null-terminated UTF-8. `out_reader` must be valid for writing one
/// pointer; the returned handle must be freed exactly once with
/// `ts_static_reader_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_build_static_reader(
    handle: *const TsStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_reader: *mut *mut TsStaticReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return TS_ERR_NULL_POINTER;
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
    unsafe { *out_reader = Box::into_raw(Box::new(TsStaticReaderHandle { inner: reader })) };
    TS_OK
}

/// Read the reader's master grid: `initial_timestamp` (unix ms), `resolution`
/// (an owned ISO-8601 duration string, e.g. `PT1H` / `P1M`), and the number of
/// timestamps on the grid.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. Each out pointer must be valid
/// for writing one value. On success `*out_resolution` is an owned C string the
/// caller must free exactly once with [`ts_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_static_reader_grid(
    reader: *const TsStaticReaderHandle,
    out_initial_ms: *mut i64,
    out_resolution: *mut *mut c_char,
    out_length: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null() || out_resolution.is_null() || out_length.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let initial = match datetime_to_unix_ms(reader.inner.initial_timestamp()) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return TS_ERR_INTEGRITY;
        }
    };
    unsafe {
        *out_initial_ms = initial;
        *out_resolution = period_cstr(reader.inner.resolution());
        *out_length = reader.inner.length() as u64;
    }
    TS_OK
}

/// Number of columnar groups in the reader.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_n` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_static_reader_num_groups(
    reader: *const TsStaticReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return TS_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.groups().len() as u64 };
    TS_OK
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
pub unsafe extern "C" fn ts_static_reader_group_info(
    reader: *const TsStaticReaderHandle,
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
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_num_columns.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let shape: Vec<i64> = group.element_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = group.dtype().code();
        *out_num_columns = group.num_columns() as u64;
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    TS_OK
}

/// Return an owned key handle for column `col_idx` of group `group_idx`. The
/// handle carries the column's identity and must be freed with `ts_key_free`.
///
/// # Safety
///
/// `reader` must be a live static-reader handle. `out_key` must be valid for
/// writing one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_static_reader_group_key(
    reader: *const TsStaticReaderHandle,
    group_idx: u64,
    col_idx: u64,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key is null");
        return TS_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let key = match group.keys().get(col_idx as usize) {
        Some(k) => k,
        None => {
            set_error(format!("column index {col_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let handle = Box::new(TsKeyHandle {
        inner: key.identity().clone(),
    });
    unsafe { *out_key = Box::into_raw(handle) };
    TS_OK
}

/// Read the value of every series at `at_unix_ms`, filling the reader's reusable
/// buffers. After this, `ts_static_reader_group_values` exposes each group's
/// bytes. Errors if `at_unix_ms` is off the reader's grid.
///
/// # Safety
///
/// `reader` must be a live static-reader handle and `store` a live store handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_static_reader_read(
    reader: *mut TsStaticReaderHandle,
    store: *const TsStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.static_read(&mut reader.inner, at) {
        Ok(()) => TS_OK,
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
pub unsafe extern "C" fn ts_static_reader_group_values(
    reader: *const TsStaticReaderHandle,
    group_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let group = match reader.inner.groups().get(group_idx as usize) {
        Some(g) => g,
        None => {
            set_error(format!("group index {group_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let bytes = group.values();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    TS_OK
}

/// Free a static-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `ts_store_build_static_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_static_reader_free(reader: *mut TsStaticReaderHandle) {
    if !reader.is_null() {
        unsafe { drop(Box::from_raw(reader)) };
    }
}

// ---- ForecastReader -------------------------------------------------------

/// Build a [`TsForecastReaderHandle`] over the forecasts matching the filter.
/// `time_series_type` must be a forecast type; a `Deterministic` reader is
/// abstract and also includes `DeterministicSingleTimeSeries`. `resolution`
/// must be positive; matched forecasts must share one window timeline.
///
/// # Safety
///
/// `handle` must be a live store handle. `name` / `features_json` must be null
/// or valid null-terminated UTF-8. `out_reader` must be valid for writing one
/// pointer; free the result with `ts_forecast_reader_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_build_forecast_reader(
    handle: *const TsStoreHandle,
    has_owner: bool,
    owner_id: i64,
    has_owner_category: bool,
    owner_category: i32,
    time_series_type: i32,
    name: *const c_char,
    resolution: *const c_char,
    features_json: *const c_char,
    out_reader: *mut *mut TsForecastReaderHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_reader.is_null() {
        set_error("out_reader is null");
        return TS_ERR_NULL_POINTER;
    }
    let ts_type = match ts_type_from_int(time_series_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {time_series_type}"));
            return TS_ERR_INVALID_PARAMETER;
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
    unsafe { *out_reader = Box::into_raw(Box::new(TsForecastReaderHandle { inner: reader })) };
    TS_OK
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
/// [`ts_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_forecast_reader_timeline(
    reader: *const TsForecastReaderHandle,
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
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_initial_ms.is_null()
        || out_resolution.is_null()
        || out_interval.is_null()
        || out_count.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let initial = match datetime_to_unix_ms(reader.inner.initial_timestamp()) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 millisecond range");
            return TS_ERR_INTEGRITY;
        }
    };
    unsafe {
        *out_initial_ms = initial;
        *out_resolution = period_cstr(reader.inner.resolution());
        *out_interval = period_cstr(reader.inner.interval());
        *out_count = reader.inner.count() as u64;
    }
    TS_OK
}

/// Number of per-key window entries in the reader.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_n` must be valid for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_forecast_reader_num_entries(
    reader: *const TsForecastReaderHandle,
    out_n: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_n.is_null() {
        set_error("out_n is null");
        return TS_ERR_NULL_POINTER;
    }
    unsafe { *out_n = reader.inner.entries().len() as u64 };
    TS_OK
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
pub unsafe extern "C" fn ts_forecast_reader_entry_info(
    reader: *const TsForecastReaderHandle,
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
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_dtype.is_null() || out_shape_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let shape: Vec<i64> = entry.window_shape().iter().map(|&d| d as i64).collect();
    unsafe {
        *out_dtype = entry.dtype().code();
        write_i64_slice_out(&shape, shape_buf, shape_cap, out_shape_len);
    }
    TS_OK
}

/// Return an owned key handle for entry `entry_idx`, freed with `ts_key_free`.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle. `out_key` must be valid for
/// writing one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_forecast_reader_entry_key(
    reader: *const TsForecastReaderHandle,
    entry_idx: u64,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_key.is_null() {
        set_error("out_key is null");
        return TS_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let handle = Box::new(TsKeyHandle {
        inner: entry.key().identity().clone(),
    });
    unsafe { *out_key = Box::into_raw(handle) };
    TS_OK
}

/// Read the forecast window at `at_unix_ms` for every entry, filling the
/// reader's reusable buffers. Errors if `at_unix_ms` is off the window timeline.
///
/// # Safety
///
/// `reader` must be a live forecast-reader handle and `store` a live store
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_forecast_reader_read(
    reader: *mut TsForecastReaderHandle,
    store: *const TsStoreHandle,
    at_unix_ms: i64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_mut() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let store = match unsafe { store.as_ref() } {
        Some(s) => s,
        None => {
            set_error("store handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    let at = match unix_ms_to_datetime(at_unix_ms) {
        Some(t) => t,
        None => {
            set_error("timestamp out of range");
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    match store.inner.forecast_read(&mut reader.inner, at) {
        Ok(()) => TS_OK,
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
pub unsafe extern "C" fn ts_forecast_reader_entry_values(
    reader: *const TsForecastReaderHandle,
    entry_idx: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut u64,
) -> i32 {
    clear_error();
    let reader = match unsafe { reader.as_ref() } {
        Some(r) => r,
        None => {
            set_error("reader handle is null");
            return TS_ERR_NULL_POINTER;
        }
    };
    if out_ptr.is_null() || out_byte_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let entry = match reader.inner.entries().get(entry_idx as usize) {
        Some(e) => e,
        None => {
            set_error(format!("entry index {entry_idx} out of bounds"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let bytes = entry.window();
    unsafe {
        *out_ptr = bytes.as_ptr();
        *out_byte_len = bytes.len() as u64;
    }
    TS_OK
}

/// Free a forecast-reader handle.
///
/// # Safety
///
/// `reader` must be null or a handle from `ts_store_build_forecast_reader`, not
/// previously freed, and unused after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_forecast_reader_free(reader: *mut TsForecastReaderHandle) {
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
        let handle = TsStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut TsStaticReaderHandle = ptr::null_mut();
        let rc = unsafe {
            ts_store_build_static_reader(
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
        assert_eq!(rc, TS_OK);
        assert!(!reader.is_null());

        // Grid. Resolution is an owned ISO-8601 C string.
        let (mut initial, mut len) = (0i64, 0u64);
        let mut res: *mut c_char = ptr::null_mut();
        assert_eq!(
            unsafe { ts_static_reader_grid(reader, &mut initial, &mut res, &mut len) },
            TS_OK
        );
        assert_eq!((initial, len), (T0_MS, 4));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        unsafe { ts_string_free(res) };

        // One f64 group, 2 columns, scalar shape.
        let mut n = 0u64;
        assert_eq!(
            unsafe { ts_static_reader_num_groups(reader, &mut n) },
            TS_OK
        );
        assert_eq!(n, 1);
        let (mut dtype, mut ncols, mut shape_len) = (-1i32, 0u64, 99u64);
        assert_eq!(
            unsafe {
                ts_static_reader_group_info(
                    reader,
                    0,
                    &mut dtype,
                    &mut ncols,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            TS_OK
        );
        assert_eq!((dtype, ncols, shape_len), (0, 2, 0)); // F64 code 0

        // Column keys (owners 1 then 2).
        for (col, owner) in [(0u64, 1i64), (1, 2)] {
            let mut key: *mut TsKeyHandle = ptr::null_mut();
            assert_eq!(
                unsafe { ts_static_reader_group_key(reader, 0, col, &mut key) },
                TS_OK
            );
            assert_eq!(unsafe { (*key).inner.owner_id }, owner);
            unsafe { ts_key_free(key) };
        }

        // Read at t0 + 2h -> [12, 22].
        let at = T0_MS + 2 * HOUR_MS;
        assert_eq!(unsafe { ts_static_reader_read(reader, &handle, at) }, TS_OK);
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { ts_static_reader_group_values(reader, 0, &mut p, &mut blen) },
            TS_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[12.0, 22.0]);

        // Off-grid read errors.
        assert_ne!(
            unsafe { ts_static_reader_read(reader, &handle, T0_MS + HOUR_MS / 2) },
            TS_OK
        );

        unsafe { ts_static_reader_free(reader) };
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
        let handle = TsStoreHandle { inner: store };

        let hour = std::ffi::CString::new("PT1H").unwrap();
        let mut reader: *mut TsForecastReaderHandle = ptr::null_mut();
        let rc = unsafe {
            ts_store_build_forecast_reader(
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
        assert_eq!(rc, TS_OK);

        let (mut initial, mut count) = (0i64, 0u64);
        let (mut res, mut interval): (*mut c_char, *mut c_char) =
            (ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            unsafe {
                ts_forecast_reader_timeline(
                    reader,
                    &mut initial,
                    &mut res,
                    &mut interval,
                    &mut count,
                )
            },
            TS_OK
        );
        assert_eq!((initial, count), (T0_MS, 3));
        assert_eq!(unsafe { CStr::from_ptr(res) }.to_str().unwrap(), "PT1H");
        assert_eq!(
            unsafe { CStr::from_ptr(interval) }.to_str().unwrap(),
            "PT1H"
        );
        unsafe {
            ts_string_free(res);
            ts_string_free(interval);
        }

        let mut n = 0u64;
        assert_eq!(
            unsafe { ts_forecast_reader_num_entries(reader, &mut n) },
            TS_OK
        );
        assert_eq!(n, 1);

        // Window shape [H] = [2].
        let (mut dtype, mut shape_len) = (-1i32, 0u64);
        assert_eq!(
            unsafe {
                ts_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    ptr::null_mut(),
                    0,
                    &mut shape_len,
                )
            },
            TS_OK
        );
        assert_eq!((dtype, shape_len), (0, 1));
        let mut shape = [0i64; 1];
        let mut got = 0u64;
        assert_eq!(
            unsafe {
                ts_forecast_reader_entry_info(
                    reader,
                    0,
                    &mut dtype,
                    shape.as_mut_ptr(),
                    1,
                    &mut got,
                )
            },
            TS_OK
        );
        assert_eq!(shape, [2]);

        // Window at index 1 (t0 + 1h) -> [10, 11].
        assert_eq!(
            unsafe { ts_forecast_reader_read(reader, &handle, T0_MS + HOUR_MS) },
            TS_OK
        );
        let (mut p, mut blen) = (ptr::null::<u8>(), 0u64);
        assert_eq!(
            unsafe { ts_forecast_reader_entry_values(reader, 0, &mut p, &mut blen) },
            TS_OK
        );
        assert_eq!(blen, 16);
        let vals = unsafe { slice::from_raw_parts(p as *const f64, 2) };
        assert_eq!(vals, &[10.0, 11.0]);

        unsafe { ts_forecast_reader_free(reader) };
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
        let handle = TsStoreHandle { inner: store };

        let name = CString::new("im").unwrap();
        let hour = CString::new("PT1H").unwrap();
        let mut key: *mut TsKeyHandle = ptr::null_mut();
        assert_eq!(
            unsafe {
                ts_make_key_from_attrs(5, 0, name.as_ptr(), 0, hour.as_ptr(), ptr::null(), &mut key)
            },
            TS_OK
        );

        let (mut initial, mut dtype) = (0i64, -1i32);
        let mut res: *mut c_char = ptr::null_mut();
        let mut shape_ptr: *mut i64 = ptr::null_mut();
        let mut shape_len = 0u64;
        let mut data_ptr: *mut u8 = ptr::null_mut();
        let mut data_len = 0u64;
        assert_eq!(
            unsafe {
                ts_store_get_single(
                    &handle,
                    key,
                    &mut initial,
                    &mut res,
                    &mut dtype,
                    &mut shape_ptr,
                    &mut shape_len,
                    &mut data_ptr,
                    &mut data_len,
                )
            },
            TS_OK
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
            ts_string_free(res);
            ts_buffer_free_i64(shape_ptr, shape_len);
            ts_buffer_free_u8(data_ptr, data_len);
            ts_key_free(key);
        }
    }
}
