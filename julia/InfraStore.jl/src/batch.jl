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
    units::Union{Nothing, AbstractString}=ts.units,
    quantity_kind::Union{Nothing, AbstractString}=ts.quantity_kind,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=ts.unit_system,
    component_field::Union{Nothing, AbstractString}=ts.component_field,
    application_data::Union{Nothing, AbstractString}=ts.application_data,
    element_type::Union{Nothing, AbstractString}=ts.element_type,
)
    element_type_arg = _element_type_arg(element_type, ts.data)
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
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
        _opt_string_arg(application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(units)::Cstring,
        _opt_string_arg(quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(_unit_system(unit_system)))::Cstring,
        _opt_string_arg(component_field)::Cstring,
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
    units::Union{Nothing, AbstractString}=ts.units,
    quantity_kind::Union{Nothing, AbstractString}=ts.quantity_kind,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=ts.unit_system,
    component_field::Union{Nothing, AbstractString}=ts.component_field,
    application_data::Union{Nothing, AbstractString}=ts.application_data,
    element_type::Union{Nothing, AbstractString}=ts.element_type,
)
    timestamps = Int64[_to_unix_ms(timestamp) for timestamp in ts.timestamps]
    element_type_arg = _element_type_arg(element_type, ts.data)
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
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
        _opt_string_arg(application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(units)::Cstring,
        _opt_string_arg(quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(_unit_system(unit_system)))::Cstring,
        _opt_string_arg(component_field)::Cstring,
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
    units::Union{Nothing, AbstractString}=ts.units,
    quantity_kind::Union{Nothing, AbstractString}=ts.quantity_kind,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=ts.unit_system,
    component_field::Union{Nothing, AbstractString}=ts.component_field,
    application_data::Union{Nothing, AbstractString}=ts.application_data,
    element_type::Union{Nothing, AbstractString}=ts.element_type,
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
        quantity_kind=quantity_kind,
        unit_system=unit_system,
        component_field=component_field,
        application_data=application_data,
        element_type=element_type,
    )
end

function add_time_series!(
    batch::AddBatch,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::Scenarios;
    features::Union{Nothing, AbstractDict}=nothing,
    units::Union{Nothing, AbstractString}=ts.units,
    quantity_kind::Union{Nothing, AbstractString}=ts.quantity_kind,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=ts.unit_system,
    component_field::Union{Nothing, AbstractString}=ts.component_field,
    application_data::Union{Nothing, AbstractString}=ts.application_data,
    element_type::Union{Nothing, AbstractString}=ts.element_type,
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
        quantity_kind=quantity_kind,
        unit_system=unit_system,
        component_field=component_field,
        application_data=application_data,
        element_type=element_type,
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
    features::Union{Nothing, AbstractDict}=nothing,
    units::Union{Nothing, AbstractString}=nothing,
    quantity_kind::Union{Nothing, AbstractString}=nothing,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
    application_data::Union{Nothing, AbstractString}=nothing,
    element_type::Union{Nothing, AbstractString}=nothing,
)
    element_type_arg = _element_type_arg(element_type, data)
    dims = UInt64[size(data)...]
    bytes = _row_major_bytes(data)
    code = @ccall lib_path().infrastore_batch_add_forecast(
        batch::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        owner_type::Cstring,
        _category_int(owner_category)::Int32,
        name::Cstring,
        Int32(ts_type)::Int32,
        _to_unix_ms(initial_timestamp)::Int64,
        _period_to_iso(resolution)::Cstring,
        _period_to_iso(horizon)::Cstring,
        _period_to_iso(interval)::Cstring,
        UInt64(count)::UInt64,
        element_type_arg::Cstring,
        UInt64(length(dims))::UInt64,
        dims::Ptr{UInt64},
        bytes::Ptr{UInt8},
        UInt64(length(bytes))::UInt64,
        _opt_string_arg(application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(units)::Cstring,
        _opt_string_arg(quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(_unit_system(unit_system)))::Cstring,
        _opt_string_arg(component_field)::Cstring,
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
    units::Union{Nothing, AbstractString}=ts.units,
    quantity_kind::Union{Nothing, AbstractString}=ts.quantity_kind,
    unit_system::Union{Nothing, UnitSystem, AbstractString}=ts.unit_system,
    component_field::Union{Nothing, AbstractString}=ts.component_field,
    application_data::Union{Nothing, AbstractString}=ts.application_data,
    element_type::Union{Nothing, AbstractString}=ts.element_type,
)
    element_type_arg = _element_type_arg(element_type, ts.data)
    dims = UInt64[size(ts.data)...]
    bytes = _row_major_bytes(ts.data)
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
        _opt_string_arg(application_data)::Cstring,
        _features_arg(features)::Cstring,
        _opt_string_arg(units)::Cstring,
        _opt_string_arg(quantity_kind)::Cstring,
        _opt_string_arg(_unit_system_str(_unit_system(unit_system)))::Cstring,
        _opt_string_arg(component_field)::Cstring,
    )::Int32
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
    code = @ccall lib_path().infrastore_store_add_batch(
        store::Ptr{Cvoid},
        batch::Ptr{Cvoid},
        out_keys::Ref{Ptr{Ptr{Cvoid}}},
        out_len::Ref{UInt64},
    )::Int32
    batch.count = 0
    _check(code)
    n = Int(out_len[])
    keys = Vector{TimeSeriesKey}(undef, n)
    if n > 0
        # Copy each owned handle into a finalized wrapper, then free the array
        # buffer (the wrappers own the handles and free them via infrastore_key_free).
        try
            raw = unsafe_wrap(Array, out_keys[], n; own=false)
            for i in 1:n
                keys[i] = TimeSeriesKey(raw[i])
            end
        finally
            @ccall lib_path().infrastore_keys_buffer_free(
                out_keys[]::Ptr{Ptr{Cvoid}}, out_len[]::UInt64
            )::Cvoid
        end
    end
    return keys
end
