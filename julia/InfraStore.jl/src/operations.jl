# ---- Operations -----------------------------------------------------------

# Normalize a timestamp argument to the UTC `DateTime` the FFI takes.
#
# Julia's `DateTime` carries no time zone, so the wrapper has to decide what one
# means, and it means UTC -- the store records instants, and this is the only
# reading under which a value written here comes back as itself. That is a
# convention, not a fact about the value, which is why it is stated in the docs
# and in the error below rather than left to be discovered.
#
# A `TimeZones.ZonedDateTime` needs no convention: it names an instant outright.
# The method converting one lives in the `InfraStoreTimeZonesExt` extension, so
# TimeZones is loaded only by callers who use it -- and until it is loaded, the
# fallback below says so instead of raising a bare `MethodError`.
_utc_datetime(dt::DateTime) = dt

# A `Date` is midnight UTC on that day. Not new behavior: the constructors used
# to hand their argument to a `::DateTime` field, and `convert(DateTime, ::Date)`
# accepted one silently. Keeping the method keeps this change purely widening --
# without it, code that passed a `Date` would start hitting the fallback below.
_utc_datetime(d::Date) = DateTime(d)

function _utc_datetime(x)
    return throw(
        InvalidParameterError(
            "expected a DateTime, which this package reads as UTC, but got a $(typeof(x)). " *
            "To pass a TimeZones.ZonedDateTime -- which names an instant on its own -- run " *
            "`using TimeZones` first; that loads the conversion.",
        ),
    )
end

# Which spelling a timestamp argument arrived in -- the read-side inverse of
# which this package deliberately does not have: reads keep returning a
# `DateTime` holding the instant, with the reference beside it.
#
# A bare `DateTime` is a wall clock naming no instant, so it records
# `ZonelessReference`. That is a change from the old UTC-by-convention reading,
# and it is the honest one: the convention was never a fact about the value, and
# the store now has somewhere to say so. The stored instant is unchanged either
# way, so a series written before and after this reads back identically -- only
# the recorded spelling differs.
#
# The `ZonedDateTime` method lives in `InfraStoreTimeZonesExt`, beside the
# `_utc_datetime` one it mirrors.
_time_reference_of(::DateTime) = ZonelessReference()

# A `Date` is a wall-clock day, on the same terms.
_time_reference_of(::Date) = ZonelessReference()

# Anything else has already been rejected by `_utc_datetime`, which says what to
# do about it; this exists so the two are never out of step.
_time_reference_of(x) = (_utc_datetime(x); ZonelessReference())

# The one spelling a vector of timestamps carries. A series records one
# reference, so its timestamps have to agree on one -- and a vector mixing wall
# clocks with instants is a mistake worth reporting at the door.
function _vector_time_reference(timestamps)
    isempty(timestamps) && return nothing
    first_ref = _time_reference_of(first(timestamps))
    for (index, t) in enumerate(timestamps)
        ref = _time_reference_of(t)
        ref == first_ref || throw(
            InvalidParameterError(
                "timestamps disagree about how they are spelled: element 1 is " *
                "$(_time_reference_str(first_ref)) but element $index is " *
                "$(_time_reference_str(ref)); one series records one spelling",
            ),
        )
    end
    return first_ref
end

# The `time_reference` a constructor records: the caller's declaration when it
# made one, else the spelling inferred from the timestamp it was handed. An
# explicit `nothing` is a declaration -- of *unspecified* -- and is recorded as
# given; only the `INFERRED` default reads the timestamp.
_resolved_time_reference(::_Inferred, timestamp) = _time_reference_of(timestamp)
_resolved_time_reference(declared, _timestamp) = _time_reference(declared)

# A `(start, end)` time range argument. The ends are `Any` rather than `DateTime`
# so a `ZonedDateTime` can be passed when TimeZones is loaded; `_time_range_args`
# normalizes both through `_utc_datetime`.
const TimeRangeArg = Union{Nothing, Tuple{Any, Any}}

# Convert a timestamp to Unix milliseconds.
#
# Integer arithmetic throughout. `datetime2unix` returns Float64 *seconds*, and
# multiplying that back up by 1000 does not land on an integer for most
# millisecond-precision instants outside roughly 2004-2038 -- `Int64` then threw
# `InexactError` on a perfectly ordinary timestamp. A `DateTime` is already an
# integer millisecond count internally, so no float need be involved.
function _to_unix_ms(dt::DateTime)
    return Dates.value(dt) - Dates.UNIXEPOCH
end

# Anything else -- a `ZonedDateTime`, or a mistake -- goes through the
# normalization above first.
_to_unix_ms(x) = _to_unix_ms(_utc_datetime(x))

# Convert milliseconds since epoch back into a DateTime. The exact inverse of
# `_to_unix_ms`, and likewise float-free.
function _from_unix_ms(ms::Int64)
    return DateTime(Dates.UTM(ms + Dates.UNIXEPOCH))
end

# Lower an optional `(start, end)` DateTime range to the FFI's
# (present::Bool, zoneless::Bool, start_ms::Int64, end_ms::Int64) tuple.
# `nothing` -> no range.
#
# The bounds cross as Unix milliseconds either way -- a wall clock is sent as the
# instant it would name read as UTC, exactly as the store holds one -- so
# `zoneless` is the only thing that tells the two apart. The core refuses a bound
# whose spelling the series cannot answer rather than coercing it.
function _time_range_args(time_range::TimeRangeArg)
    time_range === nothing && return (false, false, Int64(0), Int64(0))
    start_ref = _time_reference_of(time_range[1])
    end_ref = _time_reference_of(time_range[2])
    is_zoneless(start_ref) == is_zoneless(end_ref) || throw(
        InvalidParameterError(
            "the two time_range bounds are spelled differently: one names an instant " *
            "and the other is a bare wall clock. A range is one request; spell both " *
            "bounds the way the series is.",
        ),
    )
    return (
        true,
        is_zoneless(start_ref),
        _to_unix_ms(time_range[1]),
        _to_unix_ms(time_range[2]),
    )
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

# Read + free an owned time-reference C string; `nothing` if null. Null is a
# real answer here -- it is a cohort that records no spelling -- and is distinct
# from `ZonelessReference()`, which is the positive claim that the axis is wall
# clocks.
function _take_time_reference(ptr::Ptr{Cchar})
    return (s=_take_cstr(ptr); s === nothing ? nothing : _parse_time_reference(s))
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
    SingleTimeSeries, NonSequentialTimeSeries, PersistentTimeSeries, Deterministic,
    Probabilistic, Scenarios,
}

"""
    add_time_series!(store, owner_id, owner_type, owner_category, ts;
                     features=nothing, element_type=ts.element_type,
                     units=ts.units, application_data=ts.application_data) -> Int64

Add a time series (`SingleTimeSeries`, `NonSequentialTimeSeries`,
`PersistentTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`) and
return the catalog `id` its row was filed under — the handle every read and
removal takes, and the one a caller records in its own object model. `owner_id`
identifies the owning component / supplemental attribute (a signed 64-bit integer). The
association `name` comes from the time series object (`ts.name`), as do its
`element_type` and `units` labels.

A `features` key that shadows a field of a time series or of the identity a row
is filed under (`name`, `resolution`, `owner_id`, …) is rejected: those names
are reserved so that a feature can never silently change the meaning of a
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
    get_metadata_by_id(store, id) -> Union{TimeSeriesMetadata, Nothing}

The metadata of the association filed under catalog `id`, or `nothing` if the
catalog holds no such row.

The read direction of the id every write hands back: a caller that recorded ids
in its own model resolves them here rather than keeping an id-to-key map beside
the store. `nothing` rather than a `NotFoundError`, because a caller validating
references it persisted earlier is asking whether one still resolves, and a
stale reference is an answer.

See also [`association_exists`](@ref), which answers the same question without
building the row.
"""
function get_metadata_by_id(store::Store, id::Integer)
    out_present = Ref{Bool}(false)
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_get_metadata_by_id(
            store::Ptr{Cvoid},
            Int64(id)::Int64,
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
            out_present::Ref{Bool},
        )::Int32
    )
    out_present[] || return nothing
    return _decode_metadata(JSON.parse(json))
end

"""
    association_exists(store, id) -> Bool

Whether an association is filed under catalog `id`.

A primary-key probe that fetches no row, so a model can check every reference it
holds on load rather than discovering a dangling one mid-run. An id is never
reissued once its row is deleted, so a reference that stops resolving stays
stale — it can never come to mean a different series.
"""
function association_exists(store::Store, id::Integer)
    out_present = Ref{Bool}(false)
    _check(
        @ccall lib_path().infrastore_store_association_exists(
            store::Ptr{Cvoid}, Int64(id)::Int64, out_present::Ref{Bool}
        )::Int32
    )
    return out_present[]
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
    has_time_series(store, owner_id, owner_category, name; resolution, features=nothing) -> Bool

`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`).
"""
function has_time_series(
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
)
    resolution_iso = _period_to_cstr(resolution)
    features_json =
        (features === nothing || isempty(features)) ? C_NULL : JSON.json(features)
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

# Reconstruct one SingleTimeSeries from a bulk-read result slot. Like the other
# bulk reconstructors it carries `application_data`, `element_type`, and `units`: those live on
# the series, and the bulk-result getters return them, so a series read in bulk
# and the same series read individually produce equal structs.
# The values a read hands back: the stored numbers turned into whatever their
# `element_type` says they are, so a read returns what the write was given.
#
# `raw = true` keeps the packed array — for a caller that wants the bytes as
# stored, or one whose element type this version does not map (which decodes to
# the array either way, so the flag is about intent, not capability).
_read_values(data, ::Nothing, ::Bool, ::NamedTuple) = data
function _read_values(
    data, element_type::AbstractString, raw::Bool, types::NamedTuple
)
    return raw ? data : decode_element_values(data, element_type; types=types)
end

function _bulk_single(
    result::Ptr{Cvoid}, idx::Integer, name::AbstractString, raw::Bool,
    types::NamedTuple,
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
    out_time_reference = Ref{Ptr{Cchar}}(C_NULL)
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
            out_time_reference::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    try
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        raw_data = _decode_array(bytes, out_dtype[], dims)
        element_type = _peek_cstr(out_element_type[])
        return SingleTimeSeries(
            _from_unix_ms(out_initial[]), _peek_period(out_resolution[]),
            _read_values(raw_data, element_type, raw, types), name;
            application_data=_peek_cstr(out_application_data[]),
            element_type=element_type,
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            time_reference=_time_reference(_peek_cstr(out_time_reference[])),
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
        _free_cstr(out_time_reference[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one NonSequentialTimeSeries from a bulk-read result slot (carrying
# `application_data` / `element_type` / `units`, as `_bulk_single` does).
function _bulk_non_sequential(
    result::Ptr{Cvoid}, idx::Integer, name::AbstractString, raw::Bool,
    types::NamedTuple,
)
    out_ts = Ref{Ptr{Int64}}(C_NULL)
    out_ts_len = Ref{UInt64}(0)
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
    out_time_reference = Ref{Ptr{Cchar}}(C_NULL)
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
            out_time_reference::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    try
        ts_ms = copy(unsafe_wrap(Array, out_ts[], Int(out_ts_len[]); own=false))
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        raw_data = _decode_array(bytes, out_dtype[], dims)
        element_type = _peek_cstr(out_element_type[])
        return NonSequentialTimeSeries(
            _from_unix_ms.(ts_ms), _read_values(raw_data, element_type, raw, types), name;
            application_data=_peek_cstr(out_application_data[]),
            element_type=element_type,
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            time_reference=_time_reference(_peek_cstr(out_time_reference[])),
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
        _free_cstr(out_time_reference[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one PersistentTimeSeries from a bulk-read result slot. Identical
# to `_bulk_non_sequential` above -- the two types have the same payload (carrying
# `application_data` / `element_type` / `units`, as `_bulk_single` does), and the
# same element-type decoding applies.
function _bulk_persistent(
    result::Ptr{Cvoid}, idx::Integer, name::AbstractString, raw::Bool,
    types::NamedTuple,
)
    out_ts = Ref{Ptr{Int64}}(C_NULL)
    out_ts_len = Ref{UInt64}(0)
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
    out_time_reference = Ref{Ptr{Cchar}}(C_NULL)
    out_component_field = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_bulk_result_get_persistent(
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
            out_time_reference::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    try
        ts_ms = copy(unsafe_wrap(Array, out_ts[], Int(out_ts_len[]); own=false))
        dims = Int.(unsafe_wrap(Array, out_shape[], Int(out_shape_len[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_data_len[]); own=false))
        raw_data = _decode_array(bytes, out_dtype[], dims)
        element_type = _peek_cstr(out_element_type[])
        return PersistentTimeSeries(
            _from_unix_ms.(ts_ms), _read_values(raw_data, element_type, raw, types), name;
            application_data=_peek_cstr(out_application_data[]),
            element_type=element_type,
            units=_peek_cstr(out_units[]),
            quantity_kind=_peek_cstr(out_quantity_kind[]),
            unit_system=_unit_system(_peek_cstr(out_unit_system[])),
            time_reference=_time_reference(_peek_cstr(out_time_reference[])),
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
        _free_cstr(out_time_reference[])
        _free_cstr(out_component_field[])
    end
end

# Reconstruct one forecast (Deterministic / Probabilistic / Scenarios) from a
# bulk-read result slot; `type_code` is the ts_type discriminant. As above, the
# descriptive attributes come back with the data.
function _bulk_forecast(
    result::Ptr{Cvoid}, idx::Integer, type_code::Integer, name::AbstractString,
    raw::Bool, types::NamedTuple,
)
    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_horizon = Ref{Ptr{Cchar}}(C_NULL)
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0)
    out_scen = Ref{UInt64}(0)
    out_ndims = Ref{UInt64}(0)
    out_dims = Ref{Ptr{UInt64}}(C_NULL)
    out_dtype = Ref{Int32}(0)
    out_data = Ref{Ptr{UInt8}}(C_NULL)
    out_byte_len = Ref{UInt64}(0)
    out_pct = Ref{Ptr{Float64}}(C_NULL)
    out_pct_len = Ref{UInt64}(0)
    out_application_data = Ref{Ptr{Cchar}}(C_NULL)
    out_element_type = Ref{Ptr{Cchar}}(C_NULL)
    out_units = Ref{Ptr{Cchar}}(C_NULL)
    out_quantity_kind = Ref{Ptr{Cchar}}(C_NULL)
    out_unit_system = Ref{Ptr{Cchar}}(C_NULL)
    out_time_reference = Ref{Ptr{Cchar}}(C_NULL)
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
            out_time_reference::Ref{Ptr{Cchar}},
            out_component_field::Ref{Ptr{Cchar}},
        )::Int32
    )
    local raw_data, initial, resolution, horizon, interval, count, percentiles
    local application_data, element_type, units, quantity_kind, unit_system
    local time_reference, component_field
    try
        dims = Int.(unsafe_wrap(Array, out_dims[], Int(out_ndims[]); own=false))
        bytes = copy(unsafe_wrap(Array, out_data[], Int(out_byte_len[]); own=false))
        percentiles = if Int(out_pct_len[]) > 0 && out_pct[] != C_NULL
            copy(unsafe_wrap(Array, out_pct[], Int(out_pct_len[]); own=false))
        else
            Float64[]
        end
        raw_data = _decode_array(bytes, out_dtype[], dims)
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
        time_reference = _time_reference(_peek_cstr(out_time_reference[]))
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
        _free_cstr(out_time_reference[])
        _free_cstr(out_component_field[])
    end
    data = _read_values(raw_data, element_type, raw, types)
    if type_code == INFRASTORE_TYPE_PROBABILISTIC
        return Probabilistic(
            initial, resolution, horizon, interval, count, percentiles, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            time_reference=time_reference, component_field=component_field,
        )
    elseif type_code == INFRASTORE_TYPE_SCENARIOS
        return Scenarios(
            initial, resolution, horizon, interval, count, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            time_reference=time_reference, component_field=component_field,
        )
    else
        return Deterministic(
            initial, resolution, horizon, interval, count, data, name;
            application_data=application_data, element_type=element_type, units=units,
            quantity_kind=quantity_kind, unit_system=unit_system,
            time_reference=time_reference, component_field=component_field,
        )
    end
end

# The name of bulk-read item `idx` (0-based), as an owned C string the FFI hands
# over. The result handle carries each item's name whichever way the read was
# addressed, so both `read_by_id` and `read_by_ids` label their items from here.
function _bulk_item_name(result::Ptr{Cvoid}, idx::Integer)
    out_name = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_bulk_result_item_name(
            result::Ptr{Cvoid}, UInt64(idx)::UInt64, out_name::Ref{Ptr{Cchar}}
        )::Int32
    )
    return something(_take_cstr(out_name[]), "")
end

# Decode every item out of a bulk-read result handle into the proper Julia
# struct, freeing the handle even when a decode throws. Both the name and the
# type discriminant come off the handle itself, so the keyed and id-addressed
# reads decode by exactly the same route.
function _decode_bulk_result(
    result::Ptr{Cvoid},
    n::Integer,
    raw::Bool=false,
    types::NamedTuple=DEFAULT_ELEMENT_TYPES,
)
    out = Vector{Any}(undef, n)
    try
        for i in 1:n
            out_type = Ref{Int32}(0)
            _check(
                @ccall lib_path().infrastore_bulk_result_item_type(
                    result::Ptr{Cvoid}, UInt64(i - 1)::UInt64, out_type::Ref{Int32}
                )::Int32
            )
            name = _bulk_item_name(result, i - 1)
            t = Int(out_type[])
            out[i] = if t == INFRASTORE_TYPE_SINGLE
                _bulk_single(result, i - 1, name, raw, types)
            elseif t == INFRASTORE_TYPE_NON_SEQUENTIAL
                _bulk_non_sequential(result, i - 1, name, raw, types)
            elseif t == INFRASTORE_TYPE_PERSISTENT
                _bulk_persistent(result, i - 1, name, raw, types)
            else
                _bulk_forecast(result, i - 1, t, name, raw, types)
            end
        end
    finally
        @ccall lib_path().infrastore_bulk_result_free(result::Ptr{Cvoid})::Cvoid
    end
    return out
end

"""
    read_by_ids(store, ids; time_range=nothing) -> Vector

Read many full series named by their catalog association `id`, returning one per
id in the order the ids were given — repeats included, and dispatching on each
row's stored type to the proper Julia struct (`SingleTimeSeries`,
`NonSequentialTimeSeries`, `Deterministic`, `Probabilistic`, or `Scenarios`).
The packed `SingleTimeSeries` are read in a single decompress-once pass per
dataset.

The read direction of the id every write hands back: a caller that recorded ids
in its own model resolves them here rather than keeping an id-to-key map beside
the store. Throws `NotFoundError` if any id names no row — unlike
[`association_exists`](@ref), which asks the question, this call is already
committed to reading, so a stale reference is a failure rather than an answer.
The error does not say *which* id dangled; sift them with `association_exists`
when that matters.

Pass `time_range = (start, stop)` to clip every series to that window. A range
*clips* to what is there, where [`read_by_id`](@ref)'s window is *checked* — an
export names bounds and does not know how many steps each series has inside
them. Both bounds must be spelled the way the series are, and a selection
spanning both coherence groups (zoneless and instant-bearing) is refused rather
than resolved per series; narrow it with `list_metadata`'s `zoneless` filter.
"""
function read_by_ids(
    store::Store,
    ids::AbstractVector{<:Integer};
    time_range::TimeRangeArg=nothing,
    raw::Bool=false,
    types::NamedTuple=DEFAULT_ELEMENT_TYPES,
)
    n = length(ids)
    n == 0 && return Vector{Any}(undef, 0)
    id_vec = Int64[Int64(id) for id in ids]
    out_result = Ref{Ptr{Cvoid}}(C_NULL)
    if time_range === nothing
        _check(
            @ccall lib_path().infrastore_store_read_by_ids(
                store::Ptr{Cvoid},
                id_vec::Ptr{Int64},
                UInt64(n)::UInt64,
                out_result::Ref{Ptr{Cvoid}},
            )::Int32
        )
    else
        _, tr_zoneless, tr_start, tr_end = _time_range_args(time_range)
        _check(
            @ccall lib_path().infrastore_store_read_by_ids_range(
                store::Ptr{Cvoid},
                id_vec::Ptr{Int64},
                UInt64(n)::UInt64,
                tr_zoneless::Bool,
                tr_start::Int64,
                tr_end::Int64,
                out_result::Ref{Ptr{Cvoid}},
            )::Int32
        )
    end
    return _decode_bulk_result(out_result[], n, raw, types)
end

"""
    read_by_id(store, id; start_time=nothing, len=nothing, count=nothing, owner=nothing)

Read the series filed under catalog association `id`, or the window of it these
arguments name, in **one** call — dispatching on the row's stored type to the
proper Julia struct exactly as [`read_by_ids`](@ref) does.

The id is a primary-key lookup and the row it lands on carries the grid, so the
store resolves the window itself: a caller holding an id spends nothing to learn
a series' `resolution` or `count` before asking for the second day of it. With
no keywords this is [`read_by_ids`](@ref) for a single id.

`start_time` is the first timestamp to read — a window boundary
(`initial_timestamp + k·interval`) for a forecast — and may be a `DateTime` or,
with TimeZones loaded, a `ZonedDateTime`; the store refuses a bound spelled
differently from the series rather than coercing it. `len` counts timesteps and
applies to `SingleTimeSeries` / `NonSequentialTimeSeries`; `count` counts windows
and applies to the forecasts. Passing the one that does not apply throws
`InvalidParameterError`.

A window is *checked*, not clamped: a `start_time` off the series' own grid, or a
`len` / `count` running past its end, throws `InvalidParameterError` — where the
`time_range` on [`read_by_ids`](@ref) would quietly
hand back the smaller answer that fits. Throws `NotFoundError` if `id` names no
row, following [`read_by_ids`](@ref).

Pass `owner = (owner_id, category)` to hold the row to that owner, and get
[`OwnerMismatchError`](@ref) when it belongs to someone else. The owner comes off
the same row the values are materialized from, so the guarded read costs exactly
what the unguarded one does — where confirming the owner in a call of its own
would be a second round trip whose answer describes the row as it was rather than
the row being read.

See also [`read_by_ids`](@ref), [`remove_by_ids!`](@ref), and
[`get_metadata_by_id`](@ref).
"""
function read_by_id(
    store::Store,
    id::Integer;
    start_time=nothing,
    len::Union{Nothing, Integer}=nothing,
    count::Union{Nothing, Integer}=nothing,
    owner::Union{Nothing, Tuple{Integer, OwnerCategory}}=nothing,
    raw::Bool=false,
    types::NamedTuple=DEFAULT_ELEMENT_TYPES,
)
    start_present = start_time !== nothing
    start_zoneless = start_present && is_zoneless(_time_reference_of(start_time))
    start_ms = start_present ? _to_unix_ms(start_time) : Int64(0)
    (has_owner, owner_id, owner_category) = _owner_guard(owner)
    out_result = Ref{Ptr{Cvoid}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_store_read_by_id(
            store::Ptr{Cvoid},
            Int64(id)::Int64,
            start_present::Bool,
            start_zoneless::Bool,
            start_ms::Int64,
            (len !== nothing)::Bool,
            UInt64(len === nothing ? 0 : len)::UInt64,
            (count !== nothing)::Bool,
            UInt64(count === nothing ? 0 : count)::UInt64,
            has_owner::Bool,
            owner_id::Int64,
            owner_category::Int32,
            out_result::Ref{Ptr{Cvoid}},
        )::Int32
    )
    return only(_decode_bulk_result(out_result[], 1, raw, types))
end
