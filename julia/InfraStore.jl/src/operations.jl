# ---- Operations -----------------------------------------------------------

# Convert a DateTime to Unix milliseconds.
function _to_unix_ms(dt::DateTime)
    return Int64(Dates.datetime2unix(dt) * 1000)
end

# Convert milliseconds since epoch back into a DateTime.
function _from_unix_ms(ms::Int64)
    return Dates.unix2datetime(ms / 1000)
end

# Lower an optional `(start, end)` DateTime range to the FFI's
# (present::Bool, start_ms::Int64, end_ms::Int64) triple. `nothing` -> no range.
function _time_range_args(time_range::Union{Nothing, Tuple{DateTime, DateTime}})
    time_range === nothing && return (false, Int64(0), Int64(0))
    return (true, _to_unix_ms(time_range[1]), _to_unix_ms(time_range[2]))
end

# Canonical fixed-span milliseconds -> ISO-8601 (the Rust core re-canonicalizes,
# so any correct fixed encoding round-trips).
function _fixed_ms_to_iso(ms::Integer)::String
    if ms % 1000 == 0
        return "PT$(ms ÷ 1000)S"
    else
        whole = ms ÷ 1000
        frac = rstrip(lpad(ms % 1000, 3, '0'), '0')
        return "PT$(whole).$(frac)S"
    end
end

# Encode a Julia `Dates.Period` as an ISO-8601 duration string. Calendar periods
# (`Year`/`Quarter`/`Month`) map to `P..Y`/`P..M`; fixed periods go through
# milliseconds.
function _period_to_iso(p::Period)::String
    if p isa Dates.Year
        return "P$(Dates.value(p))Y"
    elseif p isa Dates.Quarter
        return "P$(3 * Dates.value(p))M"
    elseif p isa Dates.Month
        return "P$(Dates.value(p))M"
    else
        return _fixed_ms_to_iso(Dates.toms(p))
    end
end

# Pass an optional resolution to the FFI: `nothing` -> C_NULL (unset).
_period_to_cstr(p) = p === nothing ? C_NULL : _period_to_iso(p)

# Decode an ISO-8601 duration string into a Julia `Dates.Period`. Calendar units
# (Y/M before the `T`) yield `Year`/`Month`; fixed units yield a `Millisecond`.
function _iso_to_period(s::AbstractString)::Period
    m = match(
        r"^P(?:(\d+)Y)?(?:(\d+)M)?(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:([\d.]+)S)?)?$",
        s,
    )
    m === nothing && error("invalid ISO-8601 period: $s")
    years, months, weeks, days, hours, mins, secs = m.captures
    has_cal = years !== nothing || months !== nothing
    has_fixed =
        weeks !== nothing ||
        days !== nothing ||
        hours !== nothing ||
        mins !== nothing ||
        secs !== nothing
    has_cal && has_fixed && error("ISO-8601 period mixes calendar and fixed units: $s")
    if has_cal
        total_months =
            (years === nothing ? 0 : 12 * parse(Int, years)) +
            (months === nothing ? 0 : parse(Int, months))
        return if total_months % 12 == 0
            Dates.Year(total_months ÷ 12)
        else
            Dates.Month(total_months)
        end
    end
    ms = 0
    weeks !== nothing && (ms += parse(Int, weeks) * 604_800_000)
    days !== nothing && (ms += parse(Int, days) * 86_400_000)
    hours !== nothing && (ms += parse(Int, hours) * 3_600_000)
    mins !== nothing && (ms += parse(Int, mins) * 60_000)
    secs !== nothing && (ms += round(Int, parse(Float64, secs) * 1000))
    return Dates.Millisecond(ms)
end

# Read + free an owned C string returned by the FFI; `nothing` for a null pointer.
function _take_cstr(ptr::Ptr{Cchar})
    ptr == C_NULL && return nothing
    s = unsafe_string(ptr)
    @ccall lib_path().infrastore_string_free(ptr::Ptr{Cchar})::Cvoid
    return s
end

# Read + free an owned ISO-8601 period C string; `nothing` if null.
function _take_period(ptr::Ptr{Cchar})
    return (s=_take_cstr(ptr); s === nothing ? nothing : _iso_to_period(s))
end

# Read an owned C string WITHOUT freeing it (`nothing` for null). For the
# multi-buffer decode paths, which release every FFI allocation in a single
# `finally` block so that an exception mid-decode cannot leak the rest.
_peek_cstr(ptr::Ptr{Cchar}) = ptr == C_NULL ? nothing : unsafe_string(ptr)

# `_peek_cstr` + ISO-8601 parse; `nothing` if null. The pointer is still owned
# by the caller's `finally` block.
function _peek_period(ptr::Ptr{Cchar})
    return (s=_peek_cstr(ptr); s === nothing ? nothing : _iso_to_period(s))
end

# Null-tolerant frees for FFI-owned allocations, used from `finally` blocks.
function _free_cstr(ptr::Ptr{Cchar})
    @ccall lib_path().infrastore_string_free(ptr::Ptr{Cchar})::Cvoid
end
function _free_i64(ptr::Ptr{Int64}, len::Integer)
    @ccall lib_path().infrastore_buffer_free_i64(
        ptr::Ptr{Int64}, UInt64(len)::UInt64
    )::Cvoid
end
function _free_u8(ptr::Ptr{UInt8}, len::Integer)
    @ccall lib_path().infrastore_buffer_free_u8(ptr::Ptr{UInt8}, UInt64(len)::UInt64)::Cvoid
end
function _free_u64(ptr::Ptr{UInt64}, len::Integer)
    @ccall lib_path().infrastore_buffer_free_u64(
        ptr::Ptr{UInt64}, UInt64(len)::UInt64
    )::Cvoid
end
function _free_f64(ptr::Ptr{Float64}, len::Integer)
    @ccall lib_path().infrastore_buffer_free_f64(
        ptr::Ptr{Float64}, UInt64(len)::UInt64
    )::Cvoid
end

# The concrete time series types `add_time_series!` accepts (a
# `DeterministicSingleTimeSeries` is derived in-store via
# `transform_single_time_series!`, never added directly).
const _AddableTimeSeries = Union{
    SingleTimeSeries, NonSequentialTimeSeries, Deterministic, Probabilistic, Scenarios
}

"""
    add_time_series!(store, owner_id, owner_type, owner_category, ts;
                     features=Dict(), element_type=ts.element_type,
                     units=ts.units, application_data=ts.application_data) -> TimeSeriesKey

Add a time series (`SingleTimeSeries`, `NonSequentialTimeSeries`,
`Deterministic`, `Probabilistic`, or `Scenarios`) and return its
[`TimeSeriesKey`](@ref). `owner_id` identifies the owning component /
supplemental attribute (a signed 64-bit integer). The association `name` comes
from the time series object (`ts.name`), as do its `element_type` and `units`
labels.

A `features` key that shadows a field of a time series or of the key that
addresses one (`name`, `resolution`, `owner_id`, …) is rejected: those names are
reserved so that a feature can never silently change the meaning of a
keyword-argument query.

The same methods accept an [`AddBatch`](@ref) in place of the `Store` to stage
many adds for one [`add_time_series_bulk!`](@ref) commit; a single-store add is
exactly a one-item batch.
"""
function add_time_series!(
    store::Store,
    owner_id::Integer,
    owner_type::AbstractString,
    owner_category::OwnerCategory,
    ts::_AddableTimeSeries;
    kwargs...,
)
    batch = AddBatch()
    add_time_series!(batch, owner_id, owner_type, owner_category, ts; kwargs...)
    return only(add_time_series_bulk!(store, batch))
end

"""
    get_metadata(store, key) -> TimeSeriesMetadata
    get_metadata(T, store, owner_id, owner_category, name; resolution, interval, features=Dict()) -> TimeSeriesMetadata
    get_metadata(store, owner_id, owner_category, name; resolution, features=Dict()) -> TimeSeriesMetadata

The complete [`TimeSeriesMetadata`](@ref) of one stored association, addressed
either by a `TimeSeriesKey` or by attributes.

The attribute form takes the time series type as its first argument, exactly like
[`get_time_series`](@ref): `SingleTimeSeries`, `NonSequentialTimeSeries`,
`Deterministic`, `Probabilistic`, or `Scenarios` — where `Deterministic`
resolves a stored `DeterministicSingleTimeSeries` too, and the returned
metadata's `time_series_type` reports which form was found. `owner_category` is
the owner's `OwnerCategory` (`Component` or `SupplementalAttribute`); `interval`
is only needed to disambiguate forecasts that differ solely by interval.

Omitting the type reads a `SingleTimeSeries`, matching the same shorthand on
[`has_time_series`](@ref) and [`remove_time_series!`](@ref).

Throws `NotFoundError` if absent.

```julia
get_metadata(store, 42, Component, "load"; resolution=Hour(1))
get_metadata(Scenarios, store, 42, Component, "wind"; resolution=Hour(1))
```
"""
function get_metadata(store::Store, key::TimeSeriesKey)
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_get_metadata_by_key(
            store::Ptr{Cvoid},
            key::Ptr{Cvoid},
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    return _decode_metadata(JSON.parse(json))
end

function get_metadata(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
) where {T}
    code = _type_code(T)
    # A `Deterministic` request may be satisfied by a stored
    # `DeterministicSingleTimeSeries`, so its key is not knowable up front: let
    # the core resolve it. Every other type addresses its key directly.
    key = if code == INFRASTORE_TYPE_DETERMINISTIC
        get_time_series_key(
            T,
            store,
            owner_id,
            owner_category,
            name;
            resolution=resolution,
            interval=interval,
            features=features,
        )
    else
        _make_key(
            owner_id,
            owner_category,
            name,
            code;
            resolution=resolution,
            interval=interval,
            features=features,
        )
    end
    return get_metadata(store, key)
end

function get_metadata(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
)
    return get_metadata(
        SingleTimeSeries,
        store,
        owner_id,
        owner_category,
        name;
        resolution=resolution,
        features=features,
    )
end

"""
    rename_time_series!(store, key, new_name) -> TimeSeriesKey

Rename the series identified by `key` to `new_name`, returning the renamed key
(same identity, new name). Only the catalog name changes.
"""
function rename_time_series!(store::Store, key::TimeSeriesKey, new_name::AbstractString)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_rename(
        store::Ptr{Cvoid},
        key::Ptr{Cvoid},
        String(new_name)::Cstring,
        out_key::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_time_series_key(T, store, owner_id, owner_category, name; resolution, interval, features=Dict()) -> TimeSeriesKey

The [`TimeSeriesKey`](@ref) of the stored time series of type `T` with the given
attributes — the attribute-addressed counterpart of
[`get_time_series_keys`](@ref), which enumerates one owner's keys.

`T` is any stored type. `Deterministic` matches a stored
`DeterministicSingleTimeSeries` too, and the returned key names the concrete
stored type either way. `resolution` and `interval` narrow the identity.

The key is resolved against the catalog, so it always names something stored:
a miss throws `NotFoundError`, and a request matching several series throws
`InvalidParameterError` listing the candidates — narrow it with a `resolution`
and/or an `interval`.

Use the returned handle with the key-based readers, [`bulk_read`](@ref),
[`get_metadata`](@ref), [`rename_time_series!`](@ref), or
[`remove_time_series!`](@ref).
"""
function get_time_series_key(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
) where {T}
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_resolve_forecast_key(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        Int32(_type_code(T))::Int32,
        out_key::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    return TimeSeriesKey(out_key[])
end

"""
    get_array_by_hash(store, data_hash, ::Type{T}=Float64) -> Vector{T}

Fetch the full stored array for a 32-byte content hash, decoding the raw element
bytes as `T`. For multi-dimensional element shapes the result is the flat
row-major vector; the caller reshapes using the known element shape.

Throws [`InvalidParameterError`](@ref) when the array is not stored as `T`. The
store knows its own dtype and reports it through the ABI, so a mismatch is a
question this call can answer rather than a reinterpretation it should perform;
the error names the dtype to ask for.
"""
function get_array_by_hash(
    store::Store, data_hash::Vector{UInt8}, ::Type{T}=Float64
) where {T}
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_get_array_by_hash(
        store::Ptr{Cvoid},
        data_hash::Ptr{UInt8},
        out_dtype::Ref{Int32},
        out_data::Ref{Ptr{UInt8}},
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    bytes = try
        copy(unsafe_wrap(Array, out_data[], Int(out_len[]); own=false))
    finally
        _free_u8(out_data[], out_len[])
    end
    stored = _julia_dtype(out_dtype[])
    if stored !== T
        throw(
            InvalidParameterError(
                "array is stored as $stored, not $T; " *
                "call get_array_by_hash(store, data_hash, $stored)",
            ),
        )
    end
    return collect(reinterpret(T, bytes))
end

"""
    count_array_references(store, data_hash) -> ArrayReferenceCounts

Count the `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
referencing the 32-byte content hash `data_hash`, across all owners. A
`DeterministicSingleTimeSeries` shares the underlying array of the
`SingleTimeSeries` it was derived from, so a caller uses these counts to decide
whether removing a `SingleTimeSeries` would orphan a DST. Resolved by a single
catalog query in the Rust core.
"""
function count_array_references(store::Store, data_hash::Vector{UInt8})
    length(data_hash) == 32 || throw(InvalidParameterError("data_hash must be 32 bytes"))
    out_sts = Ref{UInt64}(0)
    out_dst = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_count_array_references(
        store::Ptr{Cvoid},
        data_hash::Ptr{UInt8},
        out_sts::Ref{UInt64},
        out_dst::Ref{UInt64},
    )::Int32
    _check(code)
    return ArrayReferenceCounts(Int(out_sts[]), Int(out_dst[]))
end

"""
    get_array_nd(store, data_hash, T, dims) -> Array{T}

Fetch a stored array and reshape it to `dims` as a column-major Julia array. The
store hands back row-major bytes, so this is the inverse of the row-major encoding
used on write (handles the column-major ↔ row-major transpose for rank ≥ 2).
"""
function get_array_nd(store::Store, data_hash::Vector{UInt8}, ::Type{T}, dims) where {T}
    flat = get_array_by_hash(store, data_hash, T)
    n = length(dims)
    n <= 1 && return reshape(flat, dims...)
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, n)))
end

"""
    has_time_series(store, owner_id, owner_category, name; resolution, features=Dict()) -> Bool

`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_time_series(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    out = Ref{Bool}(false)
    code = @ccall lib_path().infrastore_store_has_by_attrs(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        resolution_iso::Cstring,
        features_json::Cstring,
        out::Ref{Bool},
    )::Int32
    _check(code)
    return out[]
end

"""
    has_for_owner(store, owner_id, owner_category; time_series_type=nothing) -> Bool

True if `(owner_id, owner_category)` has any time series, optionally restricted to
a single `time_series_type` (the Julia type) — the name-less existence query.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_for_owner(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory;
    time_series_type::Union{Nothing, Type}=nothing,
)
    out = Ref{Bool}(false)
    use_type = time_series_type !== nothing
    code = @ccall lib_path().infrastore_store_has_for_owner(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        (use_type ? _filter_type_code(time_series_type) : Int32(0))::Int32,
        use_type::Bool,
        out::Ref{Bool},
    )::Int32
    _check(code)
    return out[]
end

"""
    remove_time_series!(store, owner_id, owner_category, name; resolution, features=Dict())

`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function remove_time_series!(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    features_json = isempty(features) ? C_NULL : JSON.json(features)
    code = @ccall lib_path().infrastore_store_remove_by_attrs(
        store::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        resolution_iso::Cstring,
        features_json::Cstring,
    )::Int32
    _check(code)
    return nothing
end

function get_time_series(
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    out_initial = Ref{Int64}(0)
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_dtype = Ref{Int32}(0)
    out_shape = Ref{Ptr{Int64}}(C_NULL)
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_data_len = Ref{UInt64}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = @ccall lib_path().infrastore_store_get_single(
        store::Ptr{Cvoid},
        key::Ptr{Cvoid},
        tr_present::Bool,
        tr_start::Int64,
        tr_end::Int64,
        out_initial::Ref{Int64},
        out_resolution::Ref{Ptr{Cchar}},
        out_dtype::Ref{Int32},
        out_shape::Ref{Ptr{Int64}},
        out_shape_len::Ref{UInt64},
        out_data::Ref{Ptr{UInt8}},
        out_data_len::Ref{UInt64},
        out_application_data::Ref{Ptr{Cchar}},
        out_element_type::Ref{Ptr{Cchar}},
        out_units::Ref{Ptr{Cchar}},
        out_quantity_kind::Ref{Ptr{Cchar}},
        out_unit_system::Ref{Ptr{Cchar}},
        out_component_field::Ref{Ptr{Cchar}},
    )::Int32
    _check(code)

    # Decode inside try/finally: every FFI allocation is released exactly once
    # in the `finally`, so an exception mid-decode cannot leak the rest.
    try
        # Full array shape [length, *element_shape] (row-major dims), then bytes.
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        data = _decode_array(bytes, out_dtype[], dims)
        return SingleTimeSeries(
            _from_unix_ms(out_initial[]),
            _peek_period(out_resolution[]),
            data,
            _key_name(key);
            application_data=_peek_cstr(out_application_data[]),
            element_type=_peek_cstr(out_element_type[]),
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            component_field=_peek_cstr(out_component_field[]),
        )
    finally
        _free_i64(out_shape[], out_shape_len[])
        _free_u8(out_data[], out_data_len[])
        _free_cstr(out_resolution[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one SingleTimeSeries from a bulk-read result slot. Like the other
# bulk reconstructors it carries `application_data`, `element_type`, and `units`: those live on
# the series, and the bulk-result getters return them, so a series read in bulk
# and the same series read individually produce equal structs.
function _bulk_single(result::Ptr{Cvoid}, idx::Integer, name::AbstractString)
    out_initial = Ref{Int64}(0);
    out_resolution = Ref{Ptr{Cchar}}(C_NULL)
    out_dtype = Ref{Int32}(0);
    out_shape = Ref{Ptr{Int64}}(C_NULL);
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_data_len = Ref{UInt64}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_bulk_result_get_single(
            result::Ptr{Cvoid},
            UInt64(idx)::UInt64,
            out_initial::Ref{Int64},
            out_resolution::Ref{Ptr{Cchar}},
            out_dtype::Ref{Int32},
            out_shape::Ref{Ptr{Int64}},
            out_shape_len::Ref{UInt64},
            out_data::Ref{Ptr{UInt8}},
            out_data_len::Ref{UInt64},
            out_application_data::Ref{Ptr{Cchar}},
            out_element_type::Ref{Ptr{Cchar}},
            out_units::Ref{Ptr{Cchar}},
            out_quantity_kind::Ref{Ptr{Cchar}},
            out_unit_system::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    try
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        data = _decode_array(bytes, out_dtype[], dims)
        return SingleTimeSeries(
            _from_unix_ms(out_initial[]), _peek_period(out_resolution[]), data, name;
            application_data=_peek_cstr(out_application_data[]),
            element_type=_peek_cstr(out_element_type[]),
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            component_field=_peek_cstr(out_component_field[]),
        )
    finally
        _free_i64(out_shape[], out_shape_len[])
        _free_u8(out_data[], out_data_len[])
        _free_cstr(out_resolution[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one NonSequentialTimeSeries from a bulk-read result slot (carrying
# `application_data` / `element_type` / `units`, as `_bulk_single` does).
function _bulk_non_sequential(result::Ptr{Cvoid}, idx::Integer, name::AbstractString)
    out_ts = Ref{Ptr{Int64}}(C_NULL);
    out_ts_len = Ref{UInt64}(0)
    out_dtype = Ref{Int32}(0);
    out_shape = Ref{Ptr{Int64}}(C_NULL);
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_data_len = Ref{UInt64}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_bulk_result_get_non_sequential(
            result::Ptr{Cvoid},
            UInt64(idx)::UInt64,
            out_ts::Ref{Ptr{Int64}},
            out_ts_len::Ref{UInt64},
            out_dtype::Ref{Int32},
            out_shape::Ref{Ptr{Int64}},
            out_shape_len::Ref{UInt64},
            out_data::Ref{Ptr{UInt8}},
            out_data_len::Ref{UInt64},
            out_application_data::Ref{Ptr{Cchar}},
            out_element_type::Ref{Ptr{Cchar}},
            out_units::Ref{Ptr{Cchar}},
            out_quantity_kind::Ref{Ptr{Cchar}},
            out_unit_system::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    try
        ts_ms = copy(unsafe_wrap(Array, out_ts[], Int(out_ts_len[]); own=false))
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        data = _decode_array(bytes, out_dtype[], dims)
        return NonSequentialTimeSeries(
            _from_unix_ms.(ts_ms), data, name;
            application_data=_peek_cstr(out_application_data[]),
            element_type=_peek_cstr(out_element_type[]),
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            component_field=_peek_cstr(out_component_field[]),
        )
    finally
        _free_i64(out_ts[], out_ts_len[])
        _free_i64(out_shape[], out_shape_len[])
        _free_u8(out_data[], out_data_len[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one forecast (Deterministic / Probabilistic / Scenarios) from a
# bulk-read result slot; `type_code` is the ts_type discriminant. As above, the
# descriptive attributes come back with the data.
function _bulk_forecast(
    result::Ptr{Cvoid}, idx::Integer, type_code::Integer, name::AbstractString
)
    out_initial = Ref{Int64}(0);
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL);
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0);
    out_scen = Ref{UInt64}(0)
    out_ndims = Ref{UInt64}(0);
    out_dims = Ref{Ptr{UInt64}}(C_NULL);
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL);
    out_byte_len = Ref{UInt64}(0)
    out_pct = Ref{Ptr{Float64}}(C_NULL);
    out_pct_len = Ref{UInt64}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_bulk_result_get_forecast(
            result::Ptr{Cvoid},
            UInt64(idx)::UInt64,
            out_initial::Ref{Int64},
            out_res::Ref{Ptr{Cchar}},
            out_horizon::Ref{Ptr{Cchar}},
            out_interval::Ref{Ptr{Cchar}},
            out_count::Ref{UInt64},
            out_scen::Ref{UInt64},
            out_ndims::Ref{UInt64},
            out_dims::Ref{Ptr{UInt64}},
            out_dtype::Ref{Int32},
            out_data::Ref{Ptr{UInt8}},
            out_byte_len::Ref{UInt64},
            out_pct::Ref{Ptr{Float64}},
            out_pct_len::Ref{UInt64},
            out_application_data::Ref{Ptr{Cchar}},
            out_element_type::Ref{Ptr{Cchar}},
            out_units::Ref{Ptr{Cchar}},
            out_quantity_kind::Ref{Ptr{Cchar}},
            out_unit_system::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    local data, initial, resolution, horizon, interval, count, percentiles
    local application_data, element_type, units, quantity_kind, unit_system
    local component_field
    try
        dims = Int.(unsafe_wrap(Array, out_dims[], Int(out_ndims[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_byte_len[]); own=false))
        percentiles = if Int(out_pct_len[]) > 0 && out_pct[] != C_NULL
            copy(unsafe_wrap(Array, out_pct[], Int(out_pct_len[]); own=false))
        else
            Float64[]
        end
        data = _decode_array(bytes, out_dtype[], dims)
        initial = _from_unix_ms(out_initial[])
        resolution = _peek_period(out_res[])
        horizon = _peek_period(out_horizon[])
        interval = _peek_period(out_interval[])
        count = Int(out_count[])
        application_data = _peek_cstr(out_application_data[])
        element_type = _peek_cstr(out_element_type[])
        units = _peek_cstr(out_units[])
        quantity_kind = _peek_cstr(out_quantity_kind[])
        unit_system = _unit_system(_peek_cstr(out_unit_system[]))
        component_field = _peek_cstr(out_component_field[])
    finally
        _free_u64(out_dims[], out_ndims[])
        _free_u8(out_data[], out_byte_len[])
        _free_f64(out_pct[], out_pct_len[])
        _free_cstr(out_res[])
        _free_cstr(out_horizon[])
        _free_cstr(out_interval[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
    if type_code == INFRASTORE_TYPE_PROBABILISTIC
        return Probabilistic(
            initial, resolution, horizon, interval, count, percentiles, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            component_field=component_field,
        )
    elseif type_code == INFRASTORE_TYPE_SCENARIOS
        return Scenarios(
            initial, resolution, horizon, interval, count, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            component_field=component_field,
        )
    else
        return Deterministic(
            initial, resolution, horizon, interval, count, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            component_field=component_field,
        )
    end
end

"""
    bulk_read(store, keys; time_range=nothing) -> Vector

Read many full series at once, returning one per key in order — dispatching on
each key's stored type to the proper Julia struct (`SingleTimeSeries`,
`NonSequentialTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`).
The packed `SingleTimeSeries` are read in a single decompress-once pass per
dataset. Pass `time_range = (start, stop)` to slice every series to that window.
"""
function bulk_read(
    store::Store,
    keys::AbstractVector{TimeSeriesKey};
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    n = length(keys)
    out = Vector{Any}(undef, n)
    n == 0 && return out

    key_handles = Ptr{Cvoid}[k.handle for k in keys]
    out_result = Ref{Ptr{Cvoid}}(C_NULL)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = GC.@preserve keys key_handles @ccall lib_path().infrastore_store_bulk_read(
        store::Ptr{Cvoid},
        key_handles::Ptr{Ptr{Cvoid}},
        UInt64(n)::UInt64,
        tr_present::Bool,
        tr_start::Int64,
        tr_end::Int64,
        out_result::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    result = out_result[]
    try
        for i in 1:n
            out_type = Ref{Int32}(0)
            _check(
                @ccall lib_path().infrastore_bulk_result_item_type(
                    result::Ptr{Cvoid}, UInt64(i - 1)::UInt64, out_type::Ref{Int32}
                )::Int32
            )
            name = _key_name(keys[i])
            t = Int(out_type[])
            out[i] = if t == INFRASTORE_TYPE_SINGLE
                _bulk_single(result, i - 1, name)
            elseif t == INFRASTORE_TYPE_NON_SEQUENTIAL
                _bulk_non_sequential(result, i - 1, name)
            else
                _bulk_forecast(result, i - 1, t, name)
            end
        end
    finally
        @ccall lib_path().infrastore_bulk_result_free(result::Ptr{Cvoid})::Cvoid
    end
    return out
end

function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    out_timestamps = Ref{Ptr{Int64}}(C_NULL)
    out_timestamps_len = Ref{UInt64}(0)
    out_dtype = Ref{Int32}(0)
    out_shape = Ref{Ptr{Int64}}(C_NULL)
    out_shape_len = Ref{UInt64}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_data_len = Ref{UInt64}(0)
    # `application_data` comes back as an owned C string of its full length, like every other
    # getter. (An earlier revision copied it into a fixed 256-byte buffer, which
    # silently truncated any longer payload.)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    tr_present, tr_start, tr_end = _time_range_args(time_range)
    code = @ccall lib_path().infrastore_store_get_non_sequential(
        store::Ptr{Cvoid},
        key::Ptr{Cvoid},
        tr_present::Bool,
        tr_start::Int64,
        tr_end::Int64,
        out_timestamps::Ref{Ptr{Int64}},
        out_timestamps_len::Ref{UInt64},
        out_dtype::Ref{Int32},
        out_shape::Ref{Ptr{Int64}},
        out_shape_len::Ref{UInt64},
        out_data::Ref{Ptr{UInt8}},
        out_data_len::Ref{UInt64},
        out_application_data::Ref{Ptr{Cchar}},
        out_element_type::Ref{Ptr{Cchar}},
        out_units::Ref{Ptr{Cchar}},
        out_quantity_kind::Ref{Ptr{Cchar}},
        out_unit_system::Ref{Ptr{Cchar}},
        out_component_field::Ref{Ptr{Cchar}},
    )::Int32
    _check(code)

    try
        timestamp_ms = copy(
            unsafe_wrap(Array, out_timestamps[], Int(out_timestamps_len[]); own=false)
        )
        # Full array shape [length, *element_shape] (row-major dims), then bytes.
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        data = _decode_array(bytes, out_dtype[], dims)
        return NonSequentialTimeSeries(
            _from_unix_ms.(timestamp_ms), data, _key_name(key);
            application_data=_peek_cstr(out_application_data[]),
            element_type=_peek_cstr(out_element_type[]),
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            component_field=_peek_cstr(out_component_field[]),
        )
    finally
        _free_i64(out_timestamps[], out_timestamps_len[])
        _free_i64(out_shape[], out_shape_len[])
        _free_u8(out_data[], out_data_len[])
        _free_cstr(out_application_data[])
        _free_cstr(out_element_type[])
        _free_cstr(out_units[])
        _free_cstr(out_quantity_kind[])
        _free_cstr(out_unit_system[])
        _free_cstr(out_component_field[])
    end
end
