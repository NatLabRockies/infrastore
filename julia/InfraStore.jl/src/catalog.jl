# ---- Attribute-addressed static reads --------------------------------------
#
# Every type supports both calling conventions. `get_time_series(T, store, key)`
# is keyed by a `TimeSeriesKey` handle (returned by `add_time_series!`);
# `get_time_series(T, store, owner_id, name; ...)` builds a key from attributes
# (the same `(owner_id, name, resolution, features)` addressing used by
# `has_time_series` / `remove_time_series!` / `get_metadata`) and routes through
# the key-based reader.

# Build a `TimeSeriesKey` from attributes via the FFI key constructor.
function _make_key(
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString,
    ts_type::Integer;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
)
    resolution_iso = _period_to_cstr(resolution)
    interval_iso = _period_to_cstr(interval)
    features_json = _features_arg(features)
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_make_key_from_attrs(
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        name::Cstring,
        Int32(ts_type)::Int32,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        out_key::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    return TimeSeriesKey(out_key[])
end

# The association name carried on a key handle (`KeyIdentity.name`); read off
# the key itself, so no store access is involved.
_key_name(key::TimeSeriesKey) = key_info(key).name

"""
    get_time_series_keys(store, owner_id, owner_category) -> Vector{TimeSeriesKey}

Every key associated with `(owner_id, owner_category)`, one per stored association
(including `DeterministicSingleTimeSeries` rows derived by
`transform_single_time_series!`). `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). Each key can be passed to the key-based
`get_time_series(Type, store, key)` readers — the way to read a transform-derived
forecast by key.
"""
function get_time_series_keys(
    store::Store, owner_id::Integer, owner_category::OwnerCategory
)
    out_keys = Ref{Ptr{Ptr{Cvoid}}}(C_NULL)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_get_time_series_keys(
        store.handle::Ptr{Cvoid},
        Int64(owner_id)::Int64,
        _category_int(owner_category)::Int32,
        out_keys::Ref{Ptr{Ptr{Cvoid}}},
        out_len::Ref{UInt64},
    )::Int32
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
        @ccall lib_path().infrastore_keys_buffer_free(
            out_keys[]::Ptr{Ptr{Cvoid}}, out_len[]::UInt64
        )::Cvoid
    end
    return keys
end

# The Julia time series type for a key's integer type code.
function _type_for_code(code::Integer)
    if code == INFRASTORE_TYPE_SINGLE
        SingleTimeSeries
    elseif code == INFRASTORE_TYPE_NON_SEQUENTIAL
        NonSequentialTimeSeries
    elseif code == INFRASTORE_TYPE_DETERMINISTIC
        Deterministic
    elseif code == INFRASTORE_TYPE_DETERMINISTIC_SINGLE
        DeterministicSingleTimeSeries
    elseif code == INFRASTORE_TYPE_PROBABILISTIC
        Probabilistic
    elseif code == INFRASTORE_TYPE_SCENARIOS
        Scenarios
    else
        throw(InvalidParameterError("unknown time series type code $code"))
    end
end

# The integer type code for a Julia time series type — the inverse of
# `_type_for_code`, plus the request-only `AbstractDeterministic` family
# sentinel, which is a valid thing to ask for but never a stored type.
_type_code(::Type{SingleTimeSeries}) = INFRASTORE_TYPE_SINGLE
_type_code(::Type{NonSequentialTimeSeries}) = INFRASTORE_TYPE_NON_SEQUENTIAL
_type_code(::Type{Deterministic}) = INFRASTORE_TYPE_DETERMINISTIC
_type_code(::Type{DeterministicSingleTimeSeries}) = INFRASTORE_TYPE_DETERMINISTIC_SINGLE
_type_code(::Type{Probabilistic}) = INFRASTORE_TYPE_PROBABILISTIC
_type_code(::Type{Scenarios}) = INFRASTORE_TYPE_SCENARIOS
_type_code(::Type{AbstractDeterministic}) = INFRASTORE_TYPE_ABSTRACT_DETERMINISTIC
function _type_code(::Type{T}) where {T}
    return throw(InvalidParameterError("$T is not a time series type"))
end

# The type code a catalog *filter* takes. A filter selects stored rows, so the
# request-only `AbstractDeterministic` family sentinel is not a valid value: it
# names no stored type. Ask for one of its two concrete members instead.
function _filter_type_code(::Type{T}) where {T}
    T === AbstractDeterministic && throw(
        InvalidParameterError(
            "AbstractDeterministic is a request-only family and matches no stored rows; " *
            "filter on Deterministic or DeterministicSingleTimeSeries",
        ),
    )
    return Int32(_type_code(T))
end

# The Julia time series type for a metadata row's type name (the `as_str` form).
function _type_for_name(name::AbstractString)
    if name == "SingleTimeSeries"
        SingleTimeSeries
    elseif name == "NonSequentialTimeSeries"
        NonSequentialTimeSeries
    elseif name == "Deterministic"
        Deterministic
    elseif name == "DeterministicSingleTimeSeries"
        DeterministicSingleTimeSeries
    elseif name == "Probabilistic"
        Probabilistic
    elseif name == "Scenarios"
        Scenarios
    else
        throw(InvalidParameterError("unknown time series type name $name"))
    end
end

_row_period(x) = x === nothing ? nothing : _iso_to_period(String(x))
_row_int(x) = x === nothing ? nothing : Int(x)
_row_timestamp(x) = x === nothing ? nothing : _from_unix_ms(Int64(x))

# The owner category of a catalog row, which the core writes as its name.
function _category_for_name(s::AbstractString)
    if s == "Component"
        Component
    elseif s == "SupplementalAttribute"
        SupplementalAttribute
    else
        throw(InvalidParameterError("unknown owner category $s"))
    end
end

function _decode_key_row(r::AbstractDict)
    return KeyRow(
        Int64(r["owner_id"]),
        _category_for_name(r["owner_category"]),
        _type_for_name(r["time_series_type"]),
        String(r["name"]),
        _row_timestamp(r["initial_timestamp_ms"]),
        _row_period(r["resolution"]),
        _row_int(r["length"]),
        _row_period(r["horizon"]),
        _row_period(r["interval"]),
        _row_int(r["count"]),
        Dict{String, Any}(r["features"]),
    )
end

# An array-group row is a key row plus the content hash of the array it resolves
# to. The core writes the hash as hex; the binding hands back the 32 bytes every
# other hash-taking function expects.
function _decode_array_group_row(r::AbstractDict)
    k = _decode_key_row(r)
    return ArrayGroupRow(
        k.owner_id,
        k.owner_category,
        k.time_series_type,
        k.name,
        k.initial_timestamp,
        k.resolution,
        k.length,
        k.horizon,
        k.interval,
        k.count,
        k.features,
        hex2bytes(String(r["data_hash"])),
    )
end

function _decode_metadata(r::AbstractDict)
    percentiles = r["percentiles"]
    return TimeSeriesMetadata(
        Int64(r["owner_id"]),
        String(r["owner_type"]),
        _category_for_name(r["owner_category"]),
        _type_for_name(r["time_series_type"]),
        String(r["name"]),
        hex2bytes(String(r["data_hash"])),
        _row_timestamp(r["initial_timestamp_ms"]),
        _row_period(r["resolution"]),
        _row_period(r["horizon"]),
        _row_period(r["interval"]),
        _row_int(r["count"]),
        _row_int(r["length"]),
        percentiles === nothing ? nothing : Vector{Float64}(percentiles),
        _dtype_for_name(r["dtype"]),
        Tuple(Int(d) for d in r["element_shape"]),
        Dict{String, Any}(r["features"]),
        r["units"] === nothing ? nothing : String(r["units"]),
        r["ext"] === nothing ? nothing : String(r["ext"]),
    )
end

# Marshal the shared catalog-filter arguments every `infrastore_store_list_*` /
# `infrastore_store_remove_by_filter` FFI takes, as a tuple in argument order.
function _filter_args(
    owner_id, owner_category, time_series_type, name, resolution, interval, features
)
    has_owner = owner_id !== nothing
    has_category = owner_category !== nothing
    has_type = time_series_type !== nothing
    return (
        has_owner,
        has_owner ? Int64(owner_id) : Int64(0),
        has_category,
        has_category ? _category_int(owner_category) : Int32(0),
        has_type,
        has_type ? _filter_type_code(time_series_type) : Int32(0),
        name === nothing ? C_NULL : String(name),
        _period_to_cstr(resolution),
        _period_to_cstr(interval),
        isempty(features) ? C_NULL : JSON.json(features),
    )
end

# Run one JSON-returning catalog-filter FFI export (`fname`) with the shared
# filter arguments, via probe-then-fetch. The exports share one C signature, so
# the symbol is resolved at runtime (`dlsym`) and called through the pointer.
function _filter_list_json(
    fname::Symbol,
    store::Store;
    owner_id=nothing,
    owner_category=nothing,
    time_series_type=nothing,
    name=nothing,
    resolution=nothing,
    interval=nothing,
    features=Dict{String, Any}(),
)
    fptr = dlsym(dlopen(lib_path()), fname)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, resolution_iso, interval_iso, features_json) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features
    )
    return _probe(
        (buf, cap, out_len) -> @ccall $fptr(
            store.handle::Ptr{Cvoid},
            has_owner::Bool,
            owner_arg::Int64,
            has_category::Bool,
            category_arg::Int32,
            has_type::Bool,
            type_arg::Int32,
            name_arg::Cstring,
            resolution_iso::Cstring,
            interval_iso::Cstring,
            features_json::Cstring,
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
end

"""
    list_keys(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, resolution=nothing, interval=nothing, features=Dict()) -> Vector{KeyRow}

List the key of every stored time series matching the (all-optional, independent)
filters, as [`KeyRow`](@ref)s. With no filter set the whole store is listed.

- `owner_id`, `owner_category` (an `OwnerCategory`) — scope to one owner.
- `time_series_type` — the Julia type (`SingleTimeSeries`, `Deterministic`, ...).
- `name` — exact association name.
- `resolution` — a `Period`.
- `interval` — a `Period`; forecasts only (static rows carry no interval and
  never match an interval filter).
- `features` — match keys whose features include all the given entries (subset).
"""
function list_keys(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_keys, store; kwargs...)
    return [_decode_key_row(r) for r in JSON.parse(json)]
end

"""
    list_time_series(store; owner_id=nothing, owner_category=nothing,
                     time_series_type=nothing, name=nothing, resolution=nothing,
                     interval=nothing, features=Dict()) -> Vector{TimeSeriesMetadata}

The full [`TimeSeriesMetadata`](@ref) of every association matching the filter
(the same filters as [`list_keys`](@ref)) — the listing counterpart of
[`get_metadata`](@ref), which returns one.
"""
function list_time_series(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_time_series, store; kwargs...)
    return TimeSeriesMetadata[_decode_metadata(r) for r in JSON.parse(json)]
end

"""
    list_names(store; filters...) -> Vector{String}

Distinct series names matching the filter (same filters as [`list_keys`](@ref)),
sorted.
"""
function list_names(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_names, store; kwargs...)
    return String[String(s) for s in JSON.parse(json)]
end

"""
    list_owner_types(store; filters...) -> Vector{String}

Distinct owner types matching the filter (same filters as [`list_keys`](@ref)),
sorted.
"""
function list_owner_types(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_owner_types, store; kwargs...)
    return String[String(s) for s in JSON.parse(json)]
end

"""
    remove_by_filter!(store; filters...) -> Int

Remove every series matching the filter (same filters as [`list_keys`](@ref)) in
one all-or-nothing transaction; returns the number removed (0 if none match).
"""
function remove_by_filter!(
    store::Store;
    owner_id::Union{Nothing, Integer}=nothing,
    owner_category::Union{Nothing, OwnerCategory}=nothing,
    time_series_type::Union{Nothing, Type}=nothing,
    name::Union{Nothing, AbstractString}=nothing,
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, resolution_iso, interval_iso, features_json) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features
    )
    out_removed = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_remove_by_filter(
        store.handle::Ptr{Cvoid},
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        has_type::Bool,
        type_arg::Int32,
        name_arg::Cstring,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        out_removed::Ref{UInt64},
    )::Int32
    _check(code)
    return Int(out_removed[])
end

"""
    list_array_groups(store; owner_id=nothing, owner_category=nothing,
                      time_series_type=nothing, name=nothing, resolution=nothing,
                      interval=nothing, features=Dict()) -> Vector{ArrayGroupRow}

Like [`list_keys`](@ref) (same filters, same row fields), but each
[`ArrayGroupRow`](@ref) additionally carries `data_hash`: the 32-byte content
hash of the array the row resolves to. Rows that share a stored array share
their `data_hash` — both deduplicated identical arrays and a `SingleTimeSeries`
together with any `DeterministicSingleTimeSeries` derived from it. Group the
returned rows by `data_hash` to find which time series share their underlying
data.

Resolved by a single catalog query in the core (the hash is read off each metadata
row); there are no per-row `get_metadata` round-trips.
"""
function list_array_groups(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_array_groups, store; kwargs...)
    return ArrayGroupRow[_decode_array_group_row(r) for r in JSON.parse(json)]
end

"""
    key_info(key) -> KeyInfo

Inspect an opaque `TimeSeriesKey` (e.g. one returned by `get_time_series_keys`),
returning its [`KeyInfo`](@ref). `time_series_type` is the Julia type (one of
`SingleTimeSeries`, `NonSequentialTimeSeries`, `Deterministic`,
`DeterministicSingleTimeSeries`, `Probabilistic`, `Scenarios`) — pass it straight
to `get_time_series(time_series_type, store, key)`.
"""
function key_info(key::TimeSeriesKey)
    out_type = Ref{Int32}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_owner = Ref{Int64}(0)
    out_category = Ref{Int32}(0)
    name_len = Ref{UInt64}(0)
    feat_len = Ref{UInt64}(0)
    # Probe the string lengths (type, resolution, owner id, and owner category are
    # filled on this call too).
    code = @ccall lib_path().infrastore_key_attributes(
        key.handle::Ptr{Cvoid},
        out_type::Ref{Int32},
        out_res::Ref{Ptr{Cchar}},
        out_owner::Ref{Int64},
        out_category::Ref{Int32},
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        name_len::Ref{UInt64},
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        feat_len::Ref{UInt64},
    )::Int32
    _check(code)
    # The probe call also allocates the resolution string; free it and re-read on
    # the fetch call below.
    _take_cstr(out_res[])
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    feat_buf = Vector{UInt8}(undef, Int(feat_len[]) + 1)
    code = @ccall lib_path().infrastore_key_attributes(
        key.handle::Ptr{Cvoid},
        out_type::Ref{Int32},
        out_res::Ref{Ptr{Cchar}},
        out_owner::Ref{Int64},
        out_category::Ref{Int32},
        name_buf::Ptr{UInt8},
        UInt64(length(name_buf))::UInt64,
        name_len::Ref{UInt64},
        feat_buf::Ptr{UInt8},
        UInt64(length(feat_buf))::UInt64,
        feat_len::Ref{UInt64},
    )::Int32
    _check(code)
    name = String(name_buf[1:Int(name_len[])])
    features = JSON.parse(String(feat_buf[1:Int(feat_len[])]))
    resolution = _take_period(out_res[])
    return KeyInfo(
        out_owner[],
        OwnerCategory(Int(out_category[])),
        name,
        _type_for_code(out_type[]),
        resolution,
        features,
    )
end

# Key-based alias so `SingleTimeSeries` matches the `get_time_series(T, store, key)`
# shape the other types use (the bare `get_time_series(store, key)` form is kept).
function get_time_series(
    ::Type{SingleTimeSeries},
    store::Store,
    key::TimeSeriesKey;
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    return get_time_series(store, key; time_range=time_range)
end

"""
    get_time_series(SingleTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Attribute-addressed counterpart to `get_time_series(store, key)`. `owner_category`
is the owner's `OwnerCategory` (`Component` or `SupplementalAttribute`). The
optional `time_range` `(start, stop)` slices like the key-based form.
"""
function get_time_series(
    ::Type{SingleTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    key = _make_key(
        owner_id,
        owner_category,
        name,
        INFRASTORE_TYPE_SINGLE;
        resolution=resolution,
        features=features,
    )
    return get_time_series(store, key; time_range=time_range)
end

"""
    get_time_series(NonSequentialTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Attribute-addressed counterpart to `get_time_series(NonSequentialTimeSeries, store, key)`.
`owner_category` is the owner's `OwnerCategory` (`Component` or
`SupplementalAttribute`). The optional `time_range` `(start, stop)` slices like
the key-based form.
"""
function get_time_series(
    ::Type{NonSequentialTimeSeries},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    time_range::Union{Nothing, Tuple{DateTime, DateTime}}=nothing,
)
    key = _make_key(
        owner_id,
        owner_category,
        name,
        INFRASTORE_TYPE_NON_SEQUENTIAL;
        resolution=resolution,
        features=features,
    )
    return get_time_series(NonSequentialTimeSeries, store, key; time_range=time_range)
end

function remove_time_series!(store::Store, key::TimeSeriesKey)
    code = @ccall lib_path().infrastore_store_remove(
        store.handle::Ptr{Cvoid}, key.handle::Ptr{Cvoid}
    )::Int32
    _check(code)
    return nothing
end

"""
    remove_time_series!(store, keys::Vector{TimeSeriesKey}) -> Int

Remove several time series in one all-or-nothing transaction, returning the
number removed. On any error (including a single missing key) nothing is
removed.
"""
function remove_time_series!(store::Store, keys::Vector{TimeSeriesKey})
    handles = Ptr{Cvoid}[k.handle for k in keys]
    out_removed = Ref{UInt64}(0)
    code = GC.@preserve keys @ccall lib_path().infrastore_store_remove_bulk(
        store.handle::Ptr{Cvoid},
        handles::Ptr{Ptr{Cvoid}},
        UInt64(length(handles))::UInt64,
        out_removed::Ref{UInt64},
    )::Int32
    _check(code)
    return Int(out_removed[])
end

function has_time_series(store::Store, key::TimeSeriesKey)
    out = Ref{Bool}(false)
    code = @ccall lib_path().infrastore_store_has(
        store.handle::Ptr{Cvoid}, key.handle::Ptr{Cvoid}, out::Ref{Bool}
    )::Int32
    _check(code)
    return out[]
end

"""
    get_counts(store) -> TimeSeriesCounts

Association counts: components with time series, static series, and forecasts.
"""
function get_counts(store::Store)
    a = Ref{Int64}(0);
    b = Ref{Int64}(0);
    c = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_counts(
        store.handle::Ptr{Cvoid}, a::Ref{Int64}, b::Ref{Int64}, c::Ref{Int64}
    )::Int32
    _check(code)
    return TimeSeriesCounts(Int(a[]), Int(b[]), Int(c[]))
end

"""
    counts_by_type(store) -> Vector{TimeSeriesTypeCount}

Association count grouped by time series type, as
[`TimeSeriesTypeCount`](@ref)s (`time_series_type` is the Julia type). One
catalog query in the core.
"""
function counts_by_type(store::Store)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_counts_by_type(
        store.handle::Ptr{Cvoid},
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = @ccall lib_path().infrastore_store_counts_by_type(
        store.handle::Ptr{Cvoid},
        buf::Ptr{UInt8},
        UInt64(length(buf))::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return TimeSeriesTypeCount[
        TimeSeriesTypeCount(_type_for_name(r["time_series_type"]), Int(r["count"])) for
        r in rows
    ]
end

"""
    num_distinct_arrays(store) -> Int

Number of distinct stored arrays (content hashes); series that share an array
(de-duplicated by content) count once.
"""
function num_distinct_arrays(store::Store)
    out = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_num_distinct_arrays(
        store.handle::Ptr{Cvoid}, out::Ref{Int64}
    )::Int32
    _check(code)
    return Int(out[])
end

"""
    time_series_counts(store) -> TimeSeriesCountsDetailed

Distinct owners per category and distinct stored arrays per kind. Arrays shared
by content count once.
"""
function time_series_counts(store::Store)
    a = Ref{Int64}(0);
    b = Ref{Int64}(0);
    c = Ref{Int64}(0);
    d = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_counts_detailed(
        store.handle::Ptr{Cvoid}, a::Ref{Int64}, b::Ref{Int64}, c::Ref{Int64}, d::Ref{Int64}
    )::Int32
    _check(code)
    return TimeSeriesCountsDetailed(Int(a[]), Int(b[]), Int(c[]), Int(d[]))
end

"""
    list_owner_ids(store, owner_category; time_series_type=nothing, resolution=nothing) -> Vector{Int}

Distinct owner ids of `owner_category` (an `OwnerCategory`) that have a time
series, optionally restricted by `time_series_type` (the Julia type)
and/or `resolution` (a `Period`).
"""
function list_owner_ids(
    store::Store,
    owner_category::OwnerCategory;
    time_series_type::Union{Nothing, Type}=nothing,
    resolution::Union{Nothing, Period}=nothing,
)
    has_type = time_series_type !== nothing
    type_arg = has_type ? _filter_type_code(time_series_type) : Int32(0)
    resolution_iso = _period_to_cstr(resolution)
    cat = _category_int(owner_category)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_list_owner_ids(
        store.handle::Ptr{Cvoid},
        cat::Int32,
        has_type::Bool,
        type_arg::Int32,
        resolution_iso::Cstring,
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = @ccall lib_path().infrastore_store_list_owner_ids(
        store.handle::Ptr{Cvoid},
        cat::Int32,
        has_type::Bool,
        type_arg::Int32,
        resolution_iso::Cstring,
        buf::Ptr{UInt8},
        UInt64(length(buf))::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    ids = JSON.parse(String(buf[1:Int(out_len[])]))
    return Int[Int(i) for i in ids]
end

function _decode_static_summary_row(r::AbstractDict)
    return StaticSummaryRow(
        String(r["owner_type"]),
        _category_for_name(r["owner_category"]),
        _type_for_name(r["time_series_type"]),
        String(r["name"]),
        _row_timestamp(r["initial_timestamp_ms"]),
        _row_period(r["resolution"]),
        _row_int(r["time_step_count"]),
        Int(r["count"]),
    )
end

function _decode_forecast_summary_row(r::AbstractDict)
    return ForecastSummaryRow(
        String(r["owner_type"]),
        _category_for_name(r["owner_category"]),
        _type_for_name(r["time_series_type"]),
        String(r["name"]),
        _row_timestamp(r["initial_timestamp_ms"]),
        _row_period(r["resolution"]),
        _row_period(r["horizon"]),
        _row_period(r["interval"]),
        _row_int(r["window_count"]),
        Int(r["count"]),
    )
end

"""
    static_summary(store) -> Vector{StaticSummaryRow}

Grouped static-series (SingleTimeSeries + NonSequentialTimeSeries) summary: one
row per distinct `(owner_type, owner_category, time_series_type, name,
initial_timestamp, resolution, time_step_count)` with `count` = the number of
associations in the group. The core does the GROUP BY; callers build any
presentation table (e.g. a DataFrame).
"""
function static_summary(store::Store)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_static_summary(
        store.handle::Ptr{Cvoid},
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = @ccall lib_path().infrastore_store_static_summary(
        store.handle::Ptr{Cvoid},
        buf::Ptr{UInt8},
        UInt64(length(buf))::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return StaticSummaryRow[_decode_static_summary_row(r) for r in rows]
end

"""
    forecast_summary(store) -> Vector{ForecastSummaryRow}

Grouped forecast summary: one row per distinct `(owner_type, owner_category,
time_series_type, name, initial_timestamp, resolution, horizon, interval,
window_count)` with `count` = the number of associations in the group.
"""
function forecast_summary(store::Store)
    out_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_forecast_summary(
        store.handle::Ptr{Cvoid},
        C_NULL::Ptr{UInt8},
        UInt64(0)::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = @ccall lib_path().infrastore_store_forecast_summary(
        store.handle::Ptr{Cvoid},
        buf::Ptr{UInt8},
        UInt64(length(buf))::UInt64,
        out_len::Ref{UInt64},
    )::Int32
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return ForecastSummaryRow[_decode_forecast_summary_row(r) for r in rows]
end
