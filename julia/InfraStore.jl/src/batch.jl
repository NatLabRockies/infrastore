# ---- Batched adds ----------------------------------------------------------

"""
    AddBatch()

Accumulates pending add requests client-side; submit them with
[`add_time_series_bulk!`](@ref), which commits the whole batch in one metadata
transaction. This is the fast path for ingesting many time series: per-item
`add_time_series!` calls pay one SQLite commit each, while a batch pays a
single commit for all items.

Use the same `add_time_series!` methods with an `AddBatch` first argument in
place of the `Store`. The batch is drained by `add_time_series_bulk!` and may
be reused afterwards.
"""
mutable struct AddBatch
    handle::Ptr{Cvoid}
    count::Int
    function AddBatch()
        handle = ccall((:infrastore_batch_new, lib_path()), Ptr{Cvoid}, ())
        batch = new(handle, 0)
        finalizer(_finalize_batch, batch)
        return batch
    end
end

function _finalize_batch(b::AddBatch)
    if b.handle != C_NULL
        ccall((:infrastore_batch_free, lib_path()), Cvoid, (Ptr{Cvoid},), b.handle)
        b.handle = C_NULL
    end
    return nothing
end

Base.length(b::AddBatch) = b.count

_opt_string_arg(s) = s === nothing ? C_NULL : String(s)

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:infrastore_batch_add_single, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        _to_unix_ms(ts.initial_timestamp),
        _period_to_iso(ts.resolution),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::NonSequentialTimeSeries;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:infrastore_batch_add_non_sequential, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Ptr{Int64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        timestamps,
        UInt64(length(timestamps)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Deterministic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        INFRASTORE_TYPE_DETERMINISTIC,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
    )
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Scenarios;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        ts.name,
        INFRASTORE_TYPE_SCENARIOS,
        ts.initial_timestamp,
        ts.resolution,
        ts.horizon,
        ts.interval,
        ts.count,
        ts.data;
        features=features,
        units=units,
        ext=ext,
    )
end

function _batch_add_dense_forecast!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer,
    initial_timestamp::DateTime,
    resolution::Period,
    horizon::Period,
    interval::Period,
    count::Integer,
    data::AbstractArray;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=nothing,
)
    dtype = _dtype_code(eltype(data))
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    code = ccall(
        (:infrastore_batch_add_forecast, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int32,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        name,
        Int32(ts_type),
        _to_unix_ms(initial_timestamp),
        _period_to_iso(resolution),
        _period_to_iso(horizon),
        _period_to_iso(interval),
        UInt64(count),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Probabilistic;
    features::AbstractDict=Dict{String,Any}(),
    units::Union{Nothing,AbstractString}=nothing,
    ext::Union{Nothing,AbstractString}=ts.ext,
)
    dtype = _dtype_code(eltype(ts.data))
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
    code = ccall(
        (:infrastore_batch_add_probabilistic, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Int64,
            Cstring,
            Int32,
            Cstring,
            Int64,
            Cstring,
            Cstring,
            Cstring,
            UInt64,
            Ptr{Float64},
            UInt64,
            Int32,
            UInt64,
            Ptr{UInt64},
            Ptr{UInt8},
            UInt64,
            Cstring,
            Cstring,
            Cstring,
        ),
        batch.handle,
        Int64(owner_id),
        owner_type,
        _category_int(owner_category),
        ts.name,
        _to_unix_ms(ts.initial_timestamp),
        _period_to_iso(ts.resolution),
        _period_to_iso(ts.horizon),
        _period_to_iso(ts.interval),
        UInt64(ts.count),
        ts.percentiles,
        UInt64(length(ts.percentiles)),
        dtype,
        UInt64(length(dims)),
        dims,
        bytes,
        UInt64(length(bytes)),
        _opt_string_arg(ext),
        _features_arg(features),
        _opt_string_arg(units),
    )
    _check(code)
    batch.count += 1
    return batch
end

"""
    add_time_series_bulk!(store, batch::AddBatch) -> Vector{TimeSeriesKey}

Submit every request in `batch` through one all-or-nothing bulk add and return
the new keys in insertion order. The batch is drained in all cases — on error
nothing was committed and the batch is left empty.
"""
function add_time_series_bulk!(store::Store, batch::AddBatch)
    out_keys = Ref{Ptr{Ptr{Cvoid}}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:infrastore_store_add_batch, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ptr{Cvoid}, Ref{Ptr{Ptr{Cvoid}}}, Ref{UInt64}),
        store.handle,
        batch.handle,
        out_keys,
        out_len,
    )
    batch.count = 0
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via infrastore_key_free).
        raw = unsafe_wrap(Array, out_keys[], n; own=false)
        for i in 1:n
            keys[i] = TimeSeriesKey(raw[i])
        end
        ccall(
            (:infrastore_keys_buffer_free, lib_path()),
            Cvoid,
            (Ptr{Ptr{Cvoid}}, UInt64),
            out_keys[],
            out_len[],
        )
    end
    return keys
end
