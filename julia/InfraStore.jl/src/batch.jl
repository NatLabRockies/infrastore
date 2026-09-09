# ---- Batched adds ----------------------------------------------------------

"""
    AddBatch()

Accumulates pending add requests client-side; submit them with
[`add_time_series_bulk!`](@ref), which commits the whole batch in one metadata
transaction. This is the fast path for ingesting many time series *outside a
transaction*, where per-item `add_time_series!` calls pay one SQLite commit
and one HDF5 flush each while a batch pays one of each for all of them.

Inside a [`transaction`](@ref) it is not the fast path, because there is no
slow one to beat: a per-item call's savepoint is released into the enclosing
transaction rather than committed, and the adds buffer into the same blocks
this batch would write. Use a batch when the whole cohort is already in hand,
and a run of single adds when you would rather add each series as you build
it; the one difference left is that the buffer spills at 128 MiB and a batch
has no ceiling of its own.

Use the same `add_time_series!` methods with an `AddBatch` first argument in
place of the `Store`. The batch is drained by `add_time_series_bulk!` and may
be reused afterwards.
"""
mutable struct AddBatch
    handle::Ptr{Cvoid}
    count::Int
    function AddBatch()
        handle = @ccall lib_path().infrastore_batch_new()::Ptr{Cvoid}
        batch = new(handle, 0)
        finalizer(_finalize_batch, batch)
        return batch
    end
end

function _finalize_batch(b::AddBatch)
    if b.handle != C_NULL
        @ccall lib_path().infrastore_batch_free(b::Ptr{Cvoid})::Cvoid
        b.handle = C_NULL
    end
    return nothing
end

# Root the batch for the duration of a ccall (see the note in store.jl).
Base.unsafe_convert(::Type{Ptr{Cvoid}}, b::AddBatch) = b.handle

Base.length(b::AddBatch) = b.count

_opt_string_arg(s) = s === nothing ? C_NULL : String(s)

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::SingleTimeSeries;
    features::Union{Nothing, AbstractDict}=nothing,
)
    element_type_arg, dims, bytes = _wire_array(ts.element_type, ts.data)
    code = @ccall lib_path().infrastore_batch_add_single(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        ts.name::Cstring,
        _to_unix_ms(ts.initial_timestamp)::Int64,
        _period_to_iso(ts.resolution)::Cstring,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(ts.application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(ts.units)::Cstring,
        _opt_string_arg(ts.quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(ts.unit_system))::Cstring,
        _opt_string_arg(_time_reference_str(_audit_zone(ts.time_reference)))::Cstring,
        _opt_string_arg(ts.component_field)::Cstring,
    )::Int32
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
    features::Union{Nothing, AbstractDict}=nothing,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    element_type_arg, dims, bytes = _wire_array(ts.element_type, ts.data)
    code = @ccall lib_path().infrastore_batch_add_non_sequential(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        ts.name::Cstring,
        timestamps::Ptr{Int64},
        UInt64(length(timestamps))::UInt64,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(ts.application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(ts.units)::Cstring,
        _opt_string_arg(ts.quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(ts.unit_system))::Cstring,
        _opt_string_arg(_time_reference_str(_audit_zone(ts.time_reference)))::Cstring,
        _opt_string_arg(ts.component_field)::Cstring,
    )::Int32
    _check(code)
    batch.count += 1
    return batch
end

# Byte-for-byte the `NonSequentialTimeSeries` method above: the two types send
# the same payload -- a strictly increasing unix-millisecond vector plus one
# value each -- and differ only in what a read between those instants means.
function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::PersistentTimeSeries;
    features::Union{Nothing, AbstractDict}=nothing,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    element_type_arg, dims, bytes = _wire_array(ts.element_type, ts.data)
    code = @ccall lib_path().infrastore_batch_add_persistent(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        ts.name::Cstring,
        timestamps::Ptr{Int64},
        UInt64(length(timestamps))::UInt64,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(ts.application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(ts.units)::Cstring,
        _opt_string_arg(ts.quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(ts.unit_system))::Cstring,
        _opt_string_arg(_time_reference_str(_audit_zone(ts.time_reference)))::Cstring,
        _opt_string_arg(ts.component_field)::Cstring,
    )::Int32
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
    features::Union{Nothing, AbstractDict}=nothing,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        INFRASTORE_TYPE_DETERMINISTIC,
        ts;
        features=features,
    )
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Scenarios;
    features::Union{Nothing, AbstractDict}=nothing,
)
    return _batch_add_dense_forecast!(
        batch,
        owner_id,
        owner_type,
        owner_category,
        INFRASTORE_TYPE_SCENARIOS,
        ts;
        features=features,
    )
end

# `Deterministic` and `Scenarios` go down the same ABI call, distinguished only
# by the type tag; `Probabilistic` has its own because it carries percentiles.
function _batch_add_dense_forecast!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts_type::Integer,
    ts::Union{Deterministic, Scenarios};
    features::Union{Nothing, AbstractDict}=nothing,
)
    element_type_arg, dims, bytes = _wire_array(ts.element_type, ts.data)
    code = @ccall lib_path().infrastore_batch_add_forecast(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        ts.name::Cstring,
        Int32(ts_type)::Int32,
        _to_unix_ms(ts.initial_timestamp)::Int64,
        _period_to_iso(ts.resolution)::Cstring,
        _period_to_iso(ts.horizon)::Cstring,
        _period_to_iso(ts.interval)::Cstring,
        UInt64(ts.count)::UInt64,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(ts.application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(ts.units)::Cstring,
        _opt_string_arg(ts.quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(ts.unit_system))::Cstring,
        _opt_string_arg(_time_reference_str(_audit_zone(ts.time_reference)))::Cstring,
        _opt_string_arg(ts.component_field)::Cstring,
    )::Int32
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
    features::Union{Nothing, AbstractDict}=nothing,
)
    element_type_arg, dims, bytes = _wire_array(ts.element_type, ts.data)
    code = @ccall lib_path().infrastore_batch_add_probabilistic(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        ts.name::Cstring,
        _to_unix_ms(ts.initial_timestamp)::Int64,
        _period_to_iso(ts.resolution)::Cstring,
        _period_to_iso(ts.horizon)::Cstring,
        _period_to_iso(ts.interval)::Cstring,
        UInt64(ts.count)::UInt64,
        ts.percentiles::Ptr{Float64},
        UInt64(length(ts.percentiles))::UInt64,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(ts.application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(ts.units)::Cstring,
        _opt_string_arg(ts.quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(ts.unit_system))::Cstring,
        _opt_string_arg(_time_reference_str(_audit_zone(ts.time_reference)))::Cstring,
        _opt_string_arg(ts.component_field)::Cstring,
    )::Int32
    _check(code)
    batch.count += 1
    return batch
end

"""
    add_time_series_bulk!(store, batch::AddBatch) -> Vector{Int64}

Submit every request in `batch` through one all-or-nothing bulk add and return
the catalog `id` of each new row, in insertion order. The batch is drained in
all cases — on error nothing was committed and the batch is left empty.
"""
function add_time_series_bulk!(store::Store, batch::AddBatch)
    out_len = Ref{UInt64}(0)
    out_ids = Ref{Ptr{Int64}}(C_NULL)
    code = @ccall lib_path().infrastore_store_add_batch(
        store::Ptr{Cvoid},
        batch::Ptr{Cvoid},
        out_len::Ref{UInt64},
        out_ids::Ref{Ptr{Int64}},
    )::Int32
    batch.count = 0
    _check(code)
    n = Int(out_len[])
    added = Vector{Int64}(undef, n)
    if n > 0
        # Copy the ids out, then free the buffer the ABI handed over.
        try
            ids = unsafe_wrap(Array, out_ids[], n; own=false)
            copyto!(added, ids)
        finally
            @ccall lib_path().infrastore_buffer_free_i64(
                out_ids[]::Ptr{Int64}, out_len[]::UInt64
            )::Cvoid
        end
    end
    return added
end
