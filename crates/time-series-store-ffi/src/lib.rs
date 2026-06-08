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

use chrono::{DateTime, Duration, Utc};
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

// ---- Handles --------------------------------------------------------------

pub struct TsStoreHandle {
    inner: core_lib::Store,
}

pub struct TsKeyHandle {
    inner: core_lib::TimeSeriesKey,
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

/// Add a SingleTimeSeries to the store.
///
/// `features_json`, when non-null, is parsed as a JSON object whose values must be int, float, or
/// bool. `logical_type`, `units`, and `scaling_expr` are optional.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. Required string pointers must reference
/// null-terminated UTF-8 strings; optional string pointers may be null. `dims_ptr` must reference
/// `ndims` elements when `ndims` is nonzero, and `data_ptr` must reference `data_byte_len` bytes.
/// `out_key` must be valid for writing one pointer. The returned key must be released with
/// `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_single(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution_ms: i64,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    scaling_expr: *const c_char,
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
    if data_ptr.is_null() {
        set_error("data_ptr is null");
        return TS_ERR_NULL_POINTER;
    }
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(c) => {
            set_error("owner_uuid is invalid");
            return c;
        }
    };
    let owner_type = match unsafe { cstr_to_str(owner_type) } {
        Ok(s) => s,
        Err(c) => {
            set_error("owner_type is invalid");
            return c;
        }
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(c) => {
            set_error("name is invalid");
            return c;
        }
    };
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let units = match unsafe { cstr_to_optional_string(units) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let scaling_expr = match unsafe { cstr_to_optional_string(scaling_expr) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let logical_type = match unsafe { cstr_to_optional_string(logical_type) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let features = match unsafe { parse_features_json(features_json) } {
        Ok(f) => f,
        Err(code) => return code,
    };

    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let resolution = Duration::milliseconds(resolution_ms);
    let array = match unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }
    {
        Ok(a) => a,
        Err(c) => return c,
    };
    let single = core_lib::SingleTimeSeries::new(initial_timestamp, resolution, array);

    let req = core_lib::AddRequest {
        owner_uuid: owner_uuid.to_string(),
        owner_type: owner_type.to_string(),
        owner_category,
        name: name.to_string(),
        data: core_lib::TimeSeriesData::SingleTimeSeries(single),
        features,
        units,
        scaling_factor_multiplier: scaling_expr,
        logical_type,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

// ---- add_non_sequential --------------------------------------------------

/// Add a NonSequentialTimeSeries to the store.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. Required string pointers must reference
/// null-terminated UTF-8 strings; optional string pointers may be null. `timestamps_unix_ms` must
/// reference `timestamps_len` elements, `dims_ptr` must reference `ndims` elements when `ndims` is
/// nonzero, and `data_ptr` must reference `data_byte_len` bytes. `out_key` must be valid for
/// writing one pointer. The returned key must be released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_non_sequential(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
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
    scaling_expr: *const c_char,
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
    if out_key.is_null() || timestamps_unix_ms.is_null() || data_ptr.is_null() {
        set_error("an input or output pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let owner_type = match unsafe { cstr_to_str(owner_type) } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(code) => return code,
    };
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
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
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let array = match unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }
    {
        Ok(array) => array,
        Err(code) => return code,
    };
    let series = match core_lib::NonSequentialTimeSeries::new(timestamps, array) {
        Ok(series) => series,
        Err(error) => {
            set_error(error);
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let features = match unsafe { parse_features_json(features_json) } {
        Ok(features) => features,
        Err(code) => return code,
    };
    let units = match unsafe { cstr_to_optional_string(units) } {
        Ok(value) => value,
        Err(code) => return code,
    };
    let scaling_factor_multiplier = match unsafe { cstr_to_optional_string(scaling_expr) } {
        Ok(value) => value,
        Err(code) => return code,
    };
    let logical_type = match unsafe { cstr_to_optional_string(logical_type) } {
        Ok(value) => value,
        Err(code) => return code,
    };
    let request = core_lib::AddRequest {
        owner_uuid: owner_uuid.to_string(),
        owner_type: owner_type.to_string(),
        owner_category,
        name: name.to_string(),
        data: core_lib::TimeSeriesData::NonSequentialTimeSeries(series),
        features,
        units,
        scaling_factor_multiplier,
        logical_type,
    };
    match store.inner.add_time_series_bulk(vec![request]) {
        Ok(mut keys) => {
            unsafe {
                *out_key = Box::into_raw(Box::new(TsKeyHandle {
                    inner: keys.remove(0),
                }))
            };
            TS_OK
        }
        Err(error) => map_core_error(error),
    }
}

// ---- get_single -----------------------------------------------------------

/// Fetch a SingleTimeSeries by key.
///
/// On success, the caller owns the buffer pointed to by `*out_data` and must
/// free it with `ts_buffer_free_f64(*out_data, *out_data_len)`.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. The returned data buffer must be released exactly once
/// with `ts_buffer_free_f64` using the returned length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_single(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution_ms: *mut i64,
    out_data: *mut *mut f64,
    out_data_len: *mut u64,
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
        || out_resolution_ms.is_null()
        || out_data.is_null()
        || out_data_len.is_null()
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
    let resolution_ms = single.resolution.num_milliseconds();
    let mut buf: Vec<f64> = single.data.to_f64_vec().unwrap_or_default();
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution_ms = resolution_ms;
        *out_data = ptr;
        *out_data_len = len;
    }
    TS_OK
}

/// Fetch a NonSequentialTimeSeries by key.
///
/// The caller owns both output buffers and must release them with
/// `ts_buffer_free_i64` and `ts_buffer_free_u8`.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library. Every output pointer must be
/// valid for writing its indicated value. Returned buffers must each be released exactly once with
/// the matching free function and returned length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_non_sequential(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    out_timestamps: *mut *mut i64,
    out_timestamps_len: *mut u64,
    out_dtype: *mut i32,
    out_data: *mut *mut u8,
    out_data_byte_len: *mut u64,
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
        || out_data.is_null()
        || out_data_byte_len.is_null()
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

    let dtype = series.data.dtype.code();
    let mut bytes = series.data.bytes;
    let data_byte_len = bytes.len() as u64;
    let data_ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    unsafe {
        *out_timestamps = timestamps_ptr;
        *out_timestamps_len = timestamps_len;
        *out_dtype = dtype;
        *out_data = data_ptr;
        *out_data_byte_len = data_byte_len;
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

/// Write the store's forecast parameters.
///
/// `out_present` is set to `true` when the store holds at least one forecast,
/// `false` otherwise. Each of `out_horizon_ms`, `out_interval_ms`,
/// `out_count`, and `out_resolution_ms` receives the corresponding value, or
/// `-1` when that field is absent (durations, resolution, and counts are always
/// non-negative when present, so `-1` is an unambiguous "unset" sentinel).
///
/// # Safety
///
/// `handle` must be a live store handle. `out_present` must be valid for writing
/// one `bool`; every other output pointer must be valid for writing one `i64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_forecast_parameters(
    handle: *const TsStoreHandle,
    out_present: *mut bool,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
    out_count: *mut i64,
    out_resolution_ms: *mut i64,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_present.is_null()
        || out_horizon_ms.is_null()
        || out_interval_ms.is_null()
        || out_count.is_null()
        || out_resolution_ms.is_null()
    {
        return TS_ERR_NULL_POINTER;
    }
    match store.inner.get_forecast_parameters() {
        Ok(p) => {
            let present = p.horizon.is_some()
                || p.interval.is_some()
                || p.count.is_some()
                || p.resolution.is_some();
            unsafe {
                *out_present = present;
                *out_horizon_ms = p.horizon.map(|d| d.num_milliseconds()).unwrap_or(-1);
                *out_interval_ms = p.interval.map(|d| d.num_milliseconds()).unwrap_or(-1);
                *out_count = p.count.map(|c| c as i64).unwrap_or(-1);
                *out_resolution_ms = p.resolution.map(|d| d.num_milliseconds()).unwrap_or(-1);
            }
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
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
// The Julia `RustTimeSeriesStore` works in terms of (owner_uuid, name,
// resolution, features) rather than opaque key handles, so these entry points
// build a `TimeSeriesKey` internally and route to the core store. v0 only
// resolves SingleTimeSeries.

unsafe fn build_key_from_attrs(
    owner_uuid: *const c_char,
    name: *const c_char,
    resolution_ms: i64,
    features_json: *const c_char,
) -> Result<core_lib::TimeSeriesKey, i32> {
    let owner_uuid = unsafe { cstr_to_str(owner_uuid) }.inspect_err(|_| {
        set_error("owner_uuid is invalid");
    })?;
    let name = unsafe { cstr_to_str(name) }.inspect_err(|_| {
        set_error("name is invalid");
    })?;
    let features = unsafe { parse_features_json(features_json) }?;
    let resolution = if resolution_ms <= 0 {
        None
    } else {
        Some(Duration::milliseconds(resolution_ms))
    };
    Ok(core_lib::TimeSeriesKey {
        owner_uuid: owner_uuid.to_string(),
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
/// `handle` must be a live store handle. Required strings must be null-terminated UTF-8;
/// `features_json` may be null. Scalar output pointers must be valid for one value and
/// `out_data_hash` must be valid for 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_metadata(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    resolution_ms: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution_ms: *mut i64,
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
        || out_resolution_ms.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_dtype.is_null()
        || out_logical_type_len.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ms, features_json) }
    {
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
    let res_ms = match meta.resolution {
        Some(r) => r.num_milliseconds(),
        None => 0,
    };
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution_ms = res_ms;
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
/// `handle` must be a live store handle. Required strings must be null-terminated UTF-8;
/// `features_json` may be null. `out_present` must be valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_has_by_attrs(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    resolution_ms: i64,
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
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ms, features_json) }
    {
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

/// True iff `owner_uuid` has any time series, optionally filtered to a single
/// time series type (`use_type` selects whether `ts_type` is applied). Answers
/// the name-less `has_time_series(owner)` / `has_time_series(owner, T)` queries.
///
/// # Safety
///
/// `handle` must be a live store handle; `owner_uuid` a null-terminated UTF-8
/// string; `out_present` valid for writing one bool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_has_for_owner(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
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
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut filter = core_lib::ListFilter::new().owner_uuid(owner_uuid);
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
/// `handle` must be a live mutable store handle. Required strings must be null-terminated UTF-8,
/// and `features_json` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_remove_by_attrs(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    resolution_ms: i64,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ms, features_json) }
    {
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
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
    features_json: *const c_char,
) -> Result<core_lib::TimeSeriesKey, i32> {
    let time_series_type = match ts_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let mut key = unsafe { build_key_from_attrs(owner_uuid, name, resolution_ms, features_json) }?;
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
/// `handle` must be a live mutable store handle. Required strings must be null-terminated UTF-8;
/// optional strings may be null. `data_ptr` must reference `data_len` elements and `out_key` must
/// be valid for writing one pointer. The returned key must be released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_forecast(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    ts_type: i32,
    initial_ts_unix_ms: i64,
    resolution_ms: i64,
    horizon_ms: i64,
    interval_ms: i64,
    count: u64,
    dtype: i32,
    ndims: u64,
    dims_ptr: *const u64,
    data_ptr: *const u8,
    data_byte_len: u64,
    logical_type: *const c_char,
    features_json: *const c_char,
    units: *const c_char,
    scaling_expr: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_key.is_null() || data_ptr.is_null() {
        set_error("a required pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let time_series_type = match ts_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let owner_type = match unsafe { cstr_to_str(owner_type) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let units = match unsafe { cstr_to_optional_string(units) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let scaling_expr = match unsafe { cstr_to_optional_string(scaling_expr) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let features = match unsafe { parse_features_json(features_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let logical_type = match unsafe { cstr_to_optional_string(logical_type) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let array = match unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }
    {
        Ok(a) => a,
        Err(c) => return c,
    };

    let resolution = Duration::milliseconds(resolution_ms);
    let horizon = Duration::milliseconds(horizon_ms);
    let interval = Duration::milliseconds(interval_ms);
    let data = match time_series_type {
        core_lib::TimeSeriesType::Deterministic => match core_lib::Deterministic::new(
            initial_timestamp,
            resolution,
            horizon,
            interval,
            count as usize,
            array,
        ) {
            Ok(d) => core_lib::TimeSeriesData::Deterministic(d),
            Err(e) => {
                set_error(e);
                return TS_ERR_INVALID_PARAMETER;
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
            ) {
                Ok(s) => core_lib::TimeSeriesData::Scenarios(s),
                Err(e) => {
                    set_error(e);
                    return TS_ERR_INVALID_PARAMETER;
                }
            }
        }
        other => {
            set_error(format!(
                "ts_store_add_forecast supports Deterministic and Scenarios; {other:?} \
                 is not directly addable (DeterministicSingleTimeSeries is derived via \
                 ts_store_transform_single_time_series)"
            ));
            return TS_ERR_INVALID_PARAMETER;
        }
    };

    let req = core_lib::AddRequest {
        owner_uuid: owner_uuid.to_string(),
        owner_type: owner_type.to_string(),
        owner_category,
        name: name.to_string(),
        data,
        features,
        units,
        scaling_factor_multiplier: scaling_expr,
        logical_type,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0),
            });
            unsafe { *out_key = Box::into_raw(handle) };
            TS_OK
        }
        Err(e) => map_core_error(e),
    }
}

/// Add a `Probabilistic` forecast. `data` is the flattened 3-D storage array
/// `(percentile_count, horizon_count, count)` column-major; `percentiles` is the
/// percentile vector.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. Required strings must be null-terminated UTF-8;
/// optional strings may be null. `percentiles_ptr` and `data_ptr` must reference their respective
/// element counts, and `out_key` must be valid for writing one pointer. The returned key must be
/// released with `ts_key_free`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_add_probabilistic(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
    owner_type: *const c_char,
    owner_category: i32,
    name: *const c_char,
    initial_ts_unix_ms: i64,
    resolution_ms: i64,
    horizon_ms: i64,
    interval_ms: i64,
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
    scaling_expr: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_key.is_null() || data_ptr.is_null() || percentiles_ptr.is_null() {
        set_error("a required pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let owner_type = match unsafe { cstr_to_str(owner_type) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let name = match unsafe { cstr_to_str(name) } {
        Ok(s) => s,
        Err(c) => return c,
    };
    let owner_category = match owner_category {
        0 => core_lib::OwnerCategory::Component,
        1 => core_lib::OwnerCategory::SupplementalAttribute,
        other => {
            set_error(format!("invalid owner_category {other}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let units = match unsafe { cstr_to_optional_string(units) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let scaling_expr = match unsafe { cstr_to_optional_string(scaling_expr) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let features = match unsafe { parse_features_json(features_json) } {
        Ok(f) => f,
        Err(c) => return c,
    };
    let initial_timestamp = match unix_ms_to_datetime(initial_ts_unix_ms) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ms: {initial_ts_unix_ms}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let percentiles =
        unsafe { slice::from_raw_parts(percentiles_ptr, percentiles_len as usize) }.to_vec();
    let logical_type = match unsafe { cstr_to_optional_string(logical_type) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    let array = match unsafe { build_typed_array(dtype, ndims, dims_ptr, data_ptr, data_byte_len) }
    {
        Ok(a) => a,
        Err(c) => return c,
    };

    let prob = match core_lib::Probabilistic::new(
        initial_timestamp,
        Duration::milliseconds(resolution_ms),
        Duration::milliseconds(horizon_ms),
        Duration::milliseconds(interval_ms),
        count as usize,
        percentiles,
        array,
    ) {
        Ok(p) => p,
        Err(e) => {
            set_error(e);
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let req = core_lib::AddRequest {
        owner_uuid: owner_uuid.to_string(),
        owner_type: owner_type.to_string(),
        owner_category,
        name: name.to_string(),
        data: core_lib::TimeSeriesData::Probabilistic(prob),
        features,
        units,
        scaling_factor_multiplier: scaling_expr,
        logical_type,
    };
    match store.inner.add_time_series_bulk(vec![req]) {
        Ok(mut keys) => {
            let handle = Box::new(TsKeyHandle {
                inner: keys.remove(0),
            });
            unsafe { *out_key = Box::into_raw(handle) };
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
    horizon_ms: i64,
    interval_ms: i64,
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
    match store.inner.transform_single_time_series(
        Duration::milliseconds(horizon_ms),
        Duration::milliseconds(interval_ms),
    ) {
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
/// `handle` must be a live store handle. Required strings must be null-terminated UTF-8;
/// `features_json` may be null. Scalar output pointers must each be valid for one value,
/// `out_data_hash` must be valid for 32 bytes, and `out_percentiles` must be valid for writing one
/// pointer. The returned percentile buffer must be released exactly once with
/// `ts_buffer_free_f64` using the returned length.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_probabilistic_metadata(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    resolution_ms: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution_ms: *mut i64,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
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
            owner_uuid,
            name,
            4, // Probabilistic
            resolution_ms,
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
    let dur_ms = |d: Option<Duration>| d.map(|x| x.num_milliseconds()).unwrap_or(0);
    let mut pct: Vec<f64> = meta.percentiles.unwrap_or_default();
    let pct_len = pct.len() as u64;
    let pct_ptr = pct.as_mut_ptr();
    std::mem::forget(pct);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution_ms = dur_ms(meta.resolution);
        *out_horizon_ms = dur_ms(meta.horizon);
        *out_interval_ms = dur_ms(meta.interval);
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
/// `handle` must be a live store handle. Required strings must be null-terminated UTF-8;
/// `features_json` may be null. Scalar output pointers must each be valid for one value and
/// `out_data_hash` must be valid for 32 bytes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_forecast_metadata(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ms: *mut i64,
    out_resolution_ms: *mut i64,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
    out_count: *mut u64,
    out_length: *mut u64,
    out_data_hash: *mut u8,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_initial_ts_unix_ms.is_null()
        || out_resolution_ms.is_null()
        || out_horizon_ms.is_null()
        || out_interval_ms.is_null()
        || out_count.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ms, features_json)
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
    let dur_ms = |d: Option<Duration>| d.map(|x| x.num_milliseconds()).unwrap_or(0);
    unsafe {
        *out_initial_ts_unix_ms = initial_ms;
        *out_resolution_ms = dur_ms(meta.resolution);
        *out_horizon_ms = dur_ms(meta.horizon);
        *out_interval_ms = dur_ms(meta.interval);
        *out_count = meta.count.unwrap_or(0) as u64;
        *out_length = meta.length.unwrap_or(0) as u64;
        ptr::copy_nonoverlapping(meta.data_hash.as_ptr(), out_data_hash, 32);
    }
    TS_OK
}

/// Fetch a forecast by attributes and return the full data array plus metadata.
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
/// - `owner_uuid`, `name` must point to valid, null-terminated UTF-8 strings
///   for the duration of the call; `features_json` may be null.
/// - All `out_*` scalar pointers must be valid for writing one value each.
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
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
    features_json: *const c_char,
    time_range_present: bool,
    time_range_start_ms: i64,
    time_range_end_ms: i64,
    // scalar metadata outputs
    out_initial_ts_unix_ms: *mut i64,
    out_resolution_ms: *mut i64,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
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
        || out_resolution_ms.is_null()
        || out_horizon_ms.is_null()
        || out_interval_ms.is_null()
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
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ms, features_json)
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
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
    let data = match store.inner.get_time_series(&key, time_range) {
        Ok(d) => d,
        Err(e) => return map_core_error(e),
    };
    unsafe {
        emit_forecast_data(
            data,
            out_initial_ts_unix_ms,
            out_resolution_ms,
            out_horizon_ms,
            out_interval_ms,
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
    out_resolution_ms: *mut i64,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
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
    let dur_to_ms = |d: Duration| d.num_milliseconds();

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
                *out_resolution_ms = dur_to_ms(det.resolution);
                *out_horizon_ms = dur_to_ms(det.horizon);
                *out_interval_ms = dur_to_ms(det.interval);
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
                *out_resolution_ms = dur_to_ms(prob.resolution);
                *out_horizon_ms = dur_to_ms(prob.horizon);
                *out_interval_ms = dur_to_ms(prob.interval);
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
                *out_resolution_ms = dur_to_ms(scen.resolution);
                *out_horizon_ms = dur_to_ms(scen.horizon);
                *out_interval_ms = dur_to_ms(scen.interval);
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
/// outputs and buffer-ownership rules are identical to [`ts_store_get_forecast`].
///
/// # Safety
///
/// - `handle` and `key` must be live handles created by this library; no
///   concurrent mutation is permitted for the duration of the call.
/// - All `out_*` scalar pointers must be valid for writing one value each.
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
    out_resolution_ms: *mut i64,
    out_horizon_ms: *mut i64,
    out_interval_ms: *mut i64,
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
        || out_resolution_ms.is_null()
        || out_horizon_ms.is_null()
        || out_interval_ms.is_null()
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
        return TS_ERR_NULL_POINTER;
    }
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
        emit_forecast_data(
            data,
            out_initial_ts_unix_ms,
            out_resolution_ms,
            out_horizon_ms,
            out_interval_ms,
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

/// Construct a `TimeSeriesKey` handle from attributes `(owner_uuid, name,
/// ts_type, resolution, features)`.
///
/// The returned key can be passed to the key-based read functions (e.g.
/// [`ts_store_get_single`], [`ts_store_get_non_sequential`],
/// [`ts_store_get_forecast_by_key`]); it lets an attribute-addressed caller
/// reuse the key-based read path without an `add`/lookup round trip.
/// `resolution_ms <= 0` means "unspecified".
///
/// # Safety
///
/// `owner_uuid` and `name` must point to valid, null-terminated UTF-8 strings.
/// `features_json`, when non-null, must be a null-terminated UTF-8 JSON object.
/// `out_key` must be valid for writing one pointer. The returned key must be
/// released exactly once with `ts_key_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_make_key_from_attrs(
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
    features_json: *const c_char,
    out_key: *mut *mut TsKeyHandle,
) -> i32 {
    clear_error();
    if out_key.is_null() {
        set_error("out_key pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ms, features_json)
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    let handle = Box::new(TsKeyHandle { inner: key });
    unsafe { *out_key = Box::into_raw(handle) };
    TS_OK
}

/// List every time series key associated with `owner_uuid`. On success
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
/// `handle` must be a live store handle and `owner_uuid` a null-terminated UTF-8
/// string. `out_keys` must be valid for writing one pointer and `out_len` for
/// writing one `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_get_time_series_keys(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
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
    let owner_uuid = match unsafe { cstr_to_str(owner_uuid) } {
        Ok(s) => s,
        Err(c) => {
            set_error("owner_uuid is invalid");
            return c;
        }
    };
    let keys = match store.inner.get_time_series_keys(owner_uuid) {
        Ok(k) => k,
        Err(e) => return map_core_error(e),
    };
    let mut handles: Vec<*mut TsKeyHandle> = keys
        .into_iter()
        .map(|k| Box::into_raw(Box::new(TsKeyHandle { inner: k })))
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
/// UUID and name strings, and the features as a JSON object string (`"{}"` when
/// empty — the same shape the attribute-addressed entry points accept).
///
/// Strings follow the probe-then-fetch convention: call with `owner_buf` /
/// `name_buf` / `features_buf` null (and capacities `0`) to learn the required
/// lengths via the matching `out_*_len`, then call again with buffers of at
/// least `len + 1` bytes. Each returned string is NUL-terminated and truncated
/// to its capacity; the reported length is always the untruncated byte length.
///
/// # Safety
///
/// `key` must be a live key handle created by this library. `out_type`,
/// `out_resolution_ms`, `out_owner_len`, `out_name_len`, and `out_features_len`
/// must each be valid for writing one value. `owner_buf` / `name_buf` /
/// `features_buf` may be null; when non-null they must be valid for writing
/// `owner_cap` / `name_cap` / `features_cap` bytes respectively.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_key_attributes(
    key: *const TsKeyHandle,
    out_type: *mut i32,
    out_resolution_ms: *mut i64,
    owner_buf: *mut c_char,
    owner_cap: u64,
    out_owner_len: *mut u64,
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
        || out_resolution_ms.is_null()
        || out_owner_len.is_null()
        || out_name_len.is_null()
        || out_features_len.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let k = &key.inner;
    unsafe {
        *out_type = ts_type_to_int(k.time_series_type);
        *out_resolution_ms = k.resolution.map(|r| r.num_milliseconds()).unwrap_or(0);
        write_str_out(&k.owner_uuid, owner_buf, owner_cap, out_owner_len);
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

/// Read an association's `name` and `scaling_factor_multiplier` by key, resolved
/// through the stored metadata (`Store::get_metadata`). This surfaces the
/// per-association attributes that are not carried on the key itself — the read
/// path uses it to populate the returned time series object.
///
/// Both strings use the probe-then-fetch convention (see [`ts_key_attributes`]).
/// An absent `scaling_factor_multiplier` reports length 0.
///
/// # Safety
///
/// `handle` and `key` must be live handles created by this library.
/// `out_name_len` and `out_scaling_len` must each be valid for writing one
/// `u64`. `name_buf` / `scaling_buf` may be null; when non-null they must be
/// valid for writing `name_cap` / `scaling_cap` bytes respectively.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ts_store_get_association(
    handle: *const TsStoreHandle,
    key: *const TsKeyHandle,
    name_buf: *mut c_char,
    name_cap: u64,
    out_name_len: *mut u64,
    scaling_buf: *mut c_char,
    scaling_cap: u64,
    out_scaling_len: *mut u64,
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
    if out_name_len.is_null() || out_scaling_len.is_null() {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let meta = match store.inner.get_metadata(&key.inner) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let scaling = meta.scaling_factor_multiplier.unwrap_or_default();
    unsafe {
        write_str_out(&meta.name, name_buf, name_cap, out_name_len);
        write_str_out(&scaling, scaling_buf, scaling_cap, out_scaling_len);
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
/// `handle` must be a live store handle. Required strings must be null-terminated UTF-8;
/// `features_json` may be null. `out_present` must be valid for writing one `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_has_typed(
    handle: *const TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
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
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ms, features_json)
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
/// `handle` must be a live mutable store handle. Required strings must be null-terminated UTF-8,
/// and `features_json` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_remove_typed(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ms: i64,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ms, features_json)
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    match store.inner.remove_time_series(&key) {
        Ok(()) => TS_OK,
        Err(e) => map_core_error(e),
    }
}

/// Remove all time series, or all for a single owner when `owner_uuid` is
/// non-null. Returns `TS_OK` on success.
///
/// # Safety
///
/// `handle` must be a live mutable store handle. When non-null, `owner_uuid` must point to a
/// null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_store_clear(
    handle: *mut TsStoreHandle,
    owner_uuid: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let owner = match unsafe { cstr_to_optional_string(owner_uuid) } {
        Ok(v) => v,
        Err(c) => return c,
    };
    match store.inner.clear_time_series(owner.as_deref()) {
        Ok(_) => TS_OK,
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
