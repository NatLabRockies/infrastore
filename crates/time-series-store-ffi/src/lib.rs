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
    initial_ts_unix_ns: i64,
    resolution_ns: i64,
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

    let initial_timestamp = match unix_ns_to_datetime(initial_ts_unix_ns) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ns: {initial_ts_unix_ns}"));
            return TS_ERR_INVALID_PARAMETER;
        }
    };
    let resolution = Duration::nanoseconds(resolution_ns);
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
/// null-terminated UTF-8 strings; optional string pointers may be null. `timestamps_unix_ns` must
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
    timestamps_unix_ns: *const i64,
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
    if out_key.is_null() || timestamps_unix_ns.is_null() || data_ptr.is_null() {
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
        slice::from_raw_parts(timestamps_unix_ns, timestamps_len as usize)
            .iter()
            .map(|&ns| unix_ns_to_datetime(ns).ok_or(ns))
            .collect::<std::result::Result<Vec<_>, _>>()
    } {
        Ok(timestamps) => timestamps,
        Err(ns) => {
            set_error(format!("invalid timestamp unix nanoseconds: {ns}"));
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
    out_initial_ts_unix_ns: *mut i64,
    out_resolution_ns: *mut i64,
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
    if out_initial_ts_unix_ns.is_null()
        || out_resolution_ns.is_null()
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
    let initial_ns = match datetime_to_unix_ns(single.initial_timestamp) {
        Some(n) => n,
        None => {
            set_error("initial_timestamp out of i64 nanosecond range");
            return TS_ERR_INTEGRITY;
        }
    };
    let resolution_ns = single
        .resolution
        .num_nanoseconds()
        .unwrap_or_else(|| single.resolution.num_seconds() * 1_000_000_000);
    let mut buf: Vec<f64> = single.data.to_f64_vec().unwrap_or_default();
    let len = buf.len() as u64;
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    unsafe {
        *out_initial_ts_unix_ns = initial_ns;
        *out_resolution_ns = resolution_ns;
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
        .map(|timestamp| datetime_to_unix_ns(*timestamp).ok_or(TS_ERR_INTEGRITY))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(timestamps) => timestamps,
        Err(code) => {
            set_error("timestamp out of i64 nanosecond range");
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
    resolution_ns: i64,
    features_json: *const c_char,
) -> Result<core_lib::TimeSeriesKey, i32> {
    let owner_uuid = unsafe { cstr_to_str(owner_uuid) }.inspect_err(|_| {
        set_error("owner_uuid is invalid");
    })?;
    let name = unsafe { cstr_to_str(name) }.inspect_err(|_| {
        set_error("name is invalid");
    })?;
    let features = unsafe { parse_features_json(features_json) }?;
    let resolution = if resolution_ns <= 0 {
        None
    } else {
        Some(Duration::nanoseconds(resolution_ns))
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
    resolution_ns: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ns: *mut i64,
    out_resolution_ns: *mut i64,
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
    if out_initial_ts_unix_ns.is_null()
        || out_resolution_ns.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
        || out_dtype.is_null()
        || out_logical_type_len.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ns, features_json) }
    {
        Ok(k) => k,
        Err(code) => return code,
    };
    let meta = match store.inner.get_metadata(&key) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let initial_ns = match meta.initial_timestamp.and_then(datetime_to_unix_ns) {
        Some(n) => n,
        None => {
            set_error("metadata missing or out-of-range initial_timestamp");
            return TS_ERR_INTEGRITY;
        }
    };
    let res_ns = match meta.resolution {
        Some(r) => r
            .num_nanoseconds()
            .unwrap_or_else(|| r.num_seconds() * 1_000_000_000),
        None => 0,
    };
    unsafe {
        *out_initial_ts_unix_ns = initial_ns;
        *out_resolution_ns = res_ns;
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
    resolution_ns: i64,
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
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ns, features_json) }
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
    resolution_ns: i64,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe { build_key_from_attrs(owner_uuid, name, resolution_ns, features_json) }
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

unsafe fn build_typed_key_from_attrs(
    owner_uuid: *const c_char,
    name: *const c_char,
    ts_type: i32,
    resolution_ns: i64,
    features_json: *const c_char,
) -> Result<core_lib::TimeSeriesKey, i32> {
    let time_series_type = match ts_type_from_int(ts_type) {
        Some(t) => t,
        None => {
            set_error(format!("invalid time_series_type {ts_type}"));
            return Err(TS_ERR_INVALID_PARAMETER);
        }
    };
    let mut key = unsafe { build_key_from_attrs(owner_uuid, name, resolution_ns, features_json) }?;
    key.time_series_type = time_series_type;
    Ok(key)
}

/// Add a forecast. `data_ptr`/`data_len` is the flattened storage array
/// (Deterministic: `(horizon_count, count)` column-major; DST: the underlying
/// SingleTimeSeries array). `ts_type`: 2=Deterministic, 3=DeterministicSingleTimeSeries.
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
    initial_ts_unix_ns: i64,
    resolution_ns: i64,
    horizon_ns: i64,
    interval_ns: i64,
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
    let initial_timestamp = match unix_ns_to_datetime(initial_ts_unix_ns) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ns: {initial_ts_unix_ns}"));
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

    match store.inner.add_forecast(
        owner_uuid,
        owner_type,
        owner_category,
        name,
        time_series_type,
        initial_timestamp,
        Duration::nanoseconds(resolution_ns),
        Duration::nanoseconds(horizon_ns),
        Duration::nanoseconds(interval_ns),
        count as usize,
        array,
        features,
        units,
        scaling_expr,
        None,
        logical_type,
    ) {
        Ok(key) => {
            let handle = Box::new(TsKeyHandle { inner: key });
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
    initial_ts_unix_ns: i64,
    resolution_ns: i64,
    horizon_ns: i64,
    interval_ns: i64,
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
    let initial_timestamp = match unix_ns_to_datetime(initial_ts_unix_ns) {
        Some(d) => d,
        None => {
            set_error(format!("invalid initial_ts_unix_ns: {initial_ts_unix_ns}"));
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

    match store.inner.add_forecast(
        owner_uuid,
        owner_type,
        owner_category,
        name,
        core_lib::TimeSeriesType::Probabilistic,
        initial_timestamp,
        Duration::nanoseconds(resolution_ns),
        Duration::nanoseconds(horizon_ns),
        Duration::nanoseconds(interval_ns),
        count as usize,
        array,
        features,
        units,
        scaling_expr,
        Some(percentiles),
        logical_type,
    ) {
        Ok(key) => {
            let handle = Box::new(TsKeyHandle { inner: key });
            unsafe { *out_key = Box::into_raw(handle) };
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
    resolution_ns: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ns: *mut i64,
    out_resolution_ns: *mut i64,
    out_horizon_ns: *mut i64,
    out_interval_ns: *mut i64,
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
            resolution_ns,
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
    let initial_ns = match meta.initial_timestamp.and_then(datetime_to_unix_ns) {
        Some(n) => n,
        None => {
            set_error("forecast metadata missing initial_timestamp");
            return TS_ERR_INTEGRITY;
        }
    };
    let dur_ns = |d: Option<Duration>| {
        d.map(|x| {
            x.num_nanoseconds()
                .unwrap_or_else(|| x.num_seconds() * 1_000_000_000)
        })
        .unwrap_or(0)
    };
    let mut pct: Vec<f64> = meta.percentiles.unwrap_or_default();
    let pct_len = pct.len() as u64;
    let pct_ptr = pct.as_mut_ptr();
    std::mem::forget(pct);
    unsafe {
        *out_initial_ts_unix_ns = initial_ns;
        *out_resolution_ns = dur_ns(meta.resolution);
        *out_horizon_ns = dur_ns(meta.horizon);
        *out_interval_ns = dur_ns(meta.interval);
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
    resolution_ns: i64,
    features_json: *const c_char,
    out_initial_ts_unix_ns: *mut i64,
    out_resolution_ns: *mut i64,
    out_horizon_ns: *mut i64,
    out_interval_ns: *mut i64,
    out_count: *mut u64,
    out_length: *mut u64,
    out_data_hash: *mut u8,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_ref() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    if out_initial_ts_unix_ns.is_null()
        || out_resolution_ns.is_null()
        || out_horizon_ns.is_null()
        || out_interval_ns.is_null()
        || out_count.is_null()
        || out_length.is_null()
        || out_data_hash.is_null()
    {
        set_error("an out pointer is null");
        return TS_ERR_NULL_POINTER;
    }
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ns, features_json)
    } {
        Ok(k) => k,
        Err(c) => return c,
    };
    let meta = match store.inner.get_metadata(&key) {
        Ok(m) => m,
        Err(e) => return map_core_error(e),
    };
    let initial_ns = match meta.initial_timestamp.and_then(datetime_to_unix_ns) {
        Some(n) => n,
        None => {
            set_error("forecast metadata missing initial_timestamp");
            return TS_ERR_INTEGRITY;
        }
    };
    let dur_ns = |d: Option<Duration>| {
        d.map(|x| {
            x.num_nanoseconds()
                .unwrap_or_else(|| x.num_seconds() * 1_000_000_000)
        })
        .unwrap_or(0)
    };
    unsafe {
        *out_initial_ts_unix_ns = initial_ns;
        *out_resolution_ns = dur_ns(meta.resolution);
        *out_horizon_ns = dur_ns(meta.horizon);
        *out_interval_ns = dur_ns(meta.interval);
        *out_count = meta.count.unwrap_or(0) as u64;
        *out_length = meta.length.unwrap_or(0) as u64;
        ptr::copy_nonoverlapping(meta.data_hash.as_ptr(), out_data_hash, 32);
    }
    TS_OK
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
    resolution_ns: i64,
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
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ns, features_json)
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
    resolution_ns: i64,
    features_json: *const c_char,
) -> i32 {
    clear_error();
    let store = match unsafe { handle.as_mut() } {
        Some(s) => s,
        None => return TS_ERR_NULL_POINTER,
    };
    let key = match unsafe {
        build_typed_key_from_attrs(owner_uuid, name, ts_type, resolution_ns, features_json)
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

fn unix_ns_to_datetime(ns: i64) -> Option<DateTime<Utc>> {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

fn datetime_to_unix_ns(dt: DateTime<Utc>) -> Option<i64> {
    dt.timestamp_nanos_opt()
}

use chrono::TimeZone;
