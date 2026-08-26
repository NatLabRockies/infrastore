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
    features::Union{Nothing, AbstractDict}=nothing,
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
        store::Ptr{Cvoid},
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

# Every type a request may name. The methods below give each its ABI code; this
# list exists so the fallback can tell a *parameterized* spelling of one of them
# (`SingleTimeSeries{Float64}`) from a type that is not a time series type at
# all. Keep it in step with those methods.
const _TIME_SERIES_TYPES = (
    SingleTimeSeries,
    NonSequentialTimeSeries,
    Deterministic,
    DeterministicSingleTimeSeries,
    Probabilistic,
    Scenarios,
)

# The integer type code for a Julia time series type — the inverse of
# `_type_for_code`. Every code names a stored type; the widening of a
# `Deterministic` request to both deterministic storage forms happens in the
# Rust core, not here.
_type_code(::Type{SingleTimeSeries}) = INFRASTORE_TYPE_SINGLE
_type_code(::Type{NonSequentialTimeSeries}) = INFRASTORE_TYPE_NON_SEQUENTIAL
_type_code(::Type{Deterministic}) = INFRASTORE_TYPE_DETERMINISTIC
_type_code(::Type{DeterministicSingleTimeSeries}) = INFRASTORE_TYPE_DETERMINISTIC_SINGLE
_type_code(::Type{Probabilistic}) = INFRASTORE_TYPE_PROBABILISTIC
_type_code(::Type{Scenarios}) = INFRASTORE_TYPE_SCENARIOS
function _type_code(::Type{T}) where {T}
    for base in _TIME_SERIES_TYPES
        if T !== base && T <: base
            # `Type{}` is invariant, so a parameterized spelling never matches the
            # methods above and lands here. The store addresses a series by its
            # identity — (owner, category, type, name, resolution, interval,
            # features) — which carries no element type, so one key already
            # resolves to exactly one stored array. `{T,N}` on a *request* could
            # only restate what that array is, never select between arrays. What a
            # read hands back carries the stored dtype and rank in its own `{T,N}`.
            throw(
                InvalidParameterError(
                    "$T names an element type, which is not part of a time series' " *
                    "identity; pass $base and take the element type from the result",
                ),
            )
        end
    end
    return throw(InvalidParameterError("$T is not a time series type"))
end

# Reject a request type before any work happens. The readers bound their type
# argument covariantly (`T <: SingleTimeSeries`) rather than pinning it
# (`::Type{SingleTimeSeries}`), so that `SingleTimeSeries{Float64}` reaches this
# explanation instead of a `MethodError` naming a signature nobody wrote.
_check_request_type(::Type{T}) where {T} = (_type_code(T); nothing)

# The type code a catalog *filter* takes: any stored type. `Deterministic` is
# widened to both deterministic storage forms by the core's catalog predicate,
# so a filter never has to name `DeterministicSingleTimeSeries` to see it.
_filter_type_code(::Type{T}) where {T} = Int32(_type_code(T))

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

# The row's `time_reference`, absent on a store written before the column
# existed and `nothing` on a row that declared none.
function _row_time_reference(r::AbstractDict)
    value = get(r, "time_reference", nothing)
    return value === nothing ? nothing : _time_reference(String(value))
end

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
        _row_time_reference(r),
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
        k.time_reference,
        hex2bytes(String(r["data_hash"])),
    )
end

function _decode_metadata(r::AbstractDict)
    percentiles = r["percentiles"]
    return TimeSeriesMetadata(
        Int64(r["owner_id"]),
        String(r["owner_type"]),
        _category_for_name(r["owner_category"]),
        Int64(r["association_id"]),
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
        String(r["element_type"]),
        Tuple(Int(d) for d in r["element_shape"]),
        Dict{String, Any}(r["features"]),
        r["units"] === nothing ? nothing : String(r["units"]),
        r["quantity_kind"] === nothing ? nothing : String(r["quantity_kind"]),
        _unit_system(r["unit_system"] === nothing ? nothing : String(r["unit_system"])),
        _row_time_reference(r),
        r["component_field"] === nothing ? nothing : String(r["component_field"]),
        r["application_data"] === nothing ? nothing : String(r["application_data"]),
    )
end

"""
    reserve_association_ids!(store, count::Integer=1) -> Int64

Reserve `count` consecutive `association_id` values and return the first. The
caller owns `first:first + count - 1` and passes them to `add_time_series!` as
the `association_id` keyword.

Use this when a row's id must be known before the row exists — building a key at
stage time, before the batch flushes. Reserved ids are consumed whether or not
they are written, so an abandoned batch leaves a gap; an id is never reused.
"""
function reserve_association_ids!(store::Store, count::Integer=1)
    out_first = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_reserve_association_ids(
        store::Ptr{Cvoid},
        UInt64(count)::UInt64,
        out_first::Ref{Int64},
    )::Int32
    _check(code)
    return out_first[]
end

# How many ids `next_association_id!` takes per round trip. Sized so a staging
# loop crosses the FFI boundary once per few hundred series rather than once per
# series, while an abandoned block wastes an inconsequential span of a 63-bit
# space.
const ID_POOL_BLOCK = Int64(256)

"""
    next_association_id!(store) -> Int64

One id for a row about to be staged, drawn from a locally held block.

Same guarantees as [`reserve_association_ids!`](@ref) — the id is spent whether
or not a row is ever written under it, and it is never handed out twice — but a
caller staging N series pays one round trip per block instead of N. A consumer
that stages a series at a time (InfrastructureSystems does) would otherwise
cross the FFI boundary and touch the catalog's sequence row once per series,
while the array data beside it is already batched.

Ids left unused in a block are gaps, which the sequence permits by design. Use
[`reserve_association_ids!`](@ref) instead when you need a known contiguous run.
"""
function next_association_id!(store::Store)
    if store.id_pool_next >= store.id_pool_stop
        first = reserve_association_ids!(store, ID_POOL_BLOCK)
        store.id_pool_next = first
        store.id_pool_stop = first + ID_POOL_BLOCK
    end
    id = store.id_pool_next
    store.id_pool_next += 1
    return id
end

"""
    get_time_series_metadata(store, association_id::Int64) -> TimeSeriesMetadata

The complete [`TimeSeriesMetadata`](@ref) of the association carrying the given
minted surrogate id — the indexed counterpart of [`get_metadata`](@ref),
addressed by the store-assigned id instead of the full identity tuple.

Throws `NotFoundError` if no association carries it.
"""
function get_time_series_metadata(store::Store, association_id::Int64)
    json = _probe(
        (buf, cap, out_len) ->
            @ccall lib_path().infrastore_store_get_time_series_metadata_by_association_id(
                store::Ptr{Cvoid},
                association_id::Int64,
                buf::Ptr{UInt8},
                cap::UInt64,
                out_len::Ref{UInt64},
            )::Int32
    )
    return _decode_metadata(JSON.parse(json))
end

# Marshal the shared catalog-filter arguments every `infrastore_store_list_*` /
# `infrastore_store_remove_by_filter` FFI takes, as a tuple in argument order.
function _filter_args(
    owner_id, owner_category, time_series_type, name, resolution, interval, features,
    component_field=nothing, name_glob=nothing, zoneless=nothing,
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
        name_glob === nothing ? C_NULL : String(name_glob),
        _period_to_cstr(resolution),
        _period_to_cstr(interval),
        (features === nothing || isempty(features)) ? C_NULL : JSON.json(features),
        component_field === nothing ? C_NULL : String(component_field),
        # Tri-state: negative is "no filter", which is what a caller that does
        # not care passes. The two coherence groups are 0 and 1.
        zoneless === nothing ? Int32(-1) : Int32(zoneless ? 1 : 0),
    )
end

# Run one JSON-returning catalog-filter FFI export (`fname`) with the shared
# filter arguments. The exports share one C signature, so the symbol is resolved
# at runtime (`_cached_dlsym`, `lib.jl`) and called through the pointer.
#
# These return an owned string rather than following the probe-then-fetch
# convention: a listing's size scales with the catalog, and probe-then-fetch
# would run the query and serialize every row twice, once per call.
function _filter_list_json(
    fname::Symbol,
    store::Store;
    owner_id=nothing,
    owner_category=nothing,
    time_series_type=nothing,
    name=nothing,
    resolution=nothing,
    interval=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
    component_field=nothing,
    name_glob=nothing,
    zoneless=nothing,
)
    fptr = _cached_dlsym(fname)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, name_glob_arg, resolution_iso, interval_iso, features_json, component_field_arg, zoneless_arg) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features,
        component_field, name_glob, zoneless,
    )
    return _owned_str(
        (out_json, out_len) -> @ccall $fptr(
            store::Ptr{Cvoid},
            has_owner::Bool,
            owner_arg::Int64,
            has_category::Bool,
            category_arg::Int32,
            has_type::Bool,
            type_arg::Int32,
            name_arg::Cstring,
            name_glob_arg::Cstring,
            resolution_iso::Cstring,
            interval_iso::Cstring,
            features_json::Cstring,
            component_field_arg::Cstring,
            zoneless_arg::Int32,
            out_json::Ref{Ptr{Cchar}},
            out_len::Ref{UInt64},
        )::Int32
    )
end

"""
    list_keys(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, resolution=nothing, interval=nothing, features=nothing) -> Vector{KeyRow}

List the key of every stored time series matching the (all-optional, independent)
filters, as [`KeyRow`](@ref)s. With no filter set the whole store is listed.

- `owner_id`, `owner_category` (an `OwnerCategory`) — scope to one owner.
- `time_series_type` — the Julia type (`SingleTimeSeries`, `Deterministic`, ...).
  `Deterministic` also matches the `DeterministicSingleTimeSeries` rows that
  `transform_single_time_series!` derives; each row still reports its own stored
  type, and passing `DeterministicSingleTimeSeries` selects only those.
- `name` — exact association name.
- `name_glob` — a SQLite `GLOB` pattern over the name (e.g. `"wind_*"`),
  case-sensitive. Composes with `name` rather than replacing it.
- `resolution` — a `Period`.
- `interval` — a `Period`; forecasts only (static rows carry no interval and
  never match an interval filter).
- `features` — match keys whose features include all the given entries (subset).
- `component_field` — exact, case-sensitive match on the owning component's
  field (e.g. `"max_active_power"`). A row that declares none matches no value,
  so this cannot select the rows that left it unset.
- `zoneless` — `true` keeps only the rows whose timestamps are wall clocks;
  `false` keeps everything that names an instant, including the rows that
  recorded no [`TimeReference`](@ref) at all. A binary predicate rather than a
  match on a specific spelling, because those two groups are what the store's
  mixed-selection rules split on — one time bound, or one shared timestamp axis,
  cannot serve both.
"""
function list_keys(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_keys, store; kwargs...)
    return [_decode_key_row(r) for r in JSON.parse(json)]
end

"""
    list_time_series(store; owner_id=nothing, owner_category=nothing,
                     time_series_type=nothing, name=nothing, resolution=nothing,
                     interval=nothing, features=nothing) -> Vector{TimeSeriesMetadata}

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
    features::Union{Nothing, AbstractDict}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
    name_glob::Union{Nothing, AbstractString}=nothing,
    zoneless::Union{Nothing, Bool}=nothing,
)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, name_glob_arg, resolution_iso, interval_iso, features_json, component_field_arg, zoneless_arg) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features,
        component_field, name_glob, zoneless,
    )
    out_removed = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_store_remove_by_filter(
        store::Ptr{Cvoid},
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        has_type::Bool,
        type_arg::Int32,
        name_arg::Cstring,
        name_glob_arg::Cstring,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        component_field_arg::Cstring,
        zoneless_arg::Int32,
        out_removed::Ref{UInt64},
    )::Int32
    _check(code)
    return Int(out_removed[])
end

"""
    list_array_groups(store; owner_id=nothing, owner_category=nothing,
                      time_series_type=nothing, name=nothing, resolution=nothing,
                      interval=nothing, features=nothing) -> Vector{ArrayGroupRow}

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
    # `out_resolution` is null on the probe: it is a fresh allocation on every
    # call that passes it, so the probe would otherwise hand back a string this
    # call has no use for and the fetch below would overwrite.
    code = @ccall lib_path().infrastore_key_attributes(
        key::Ptr{Cvoid},
        out_type::Ref{Int32},
        C_NULL::Ptr{Cvoid},
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
    name_buf = Vector{UInt8}(undef, Int(name_len[]) + 1)
    feat_buf = Vector{UInt8}(undef, Int(feat_len[]) + 1)
    code = @ccall lib_path().infrastore_key_attributes(
        key::Ptr{Cvoid},
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
    ::Type{T},
    store::Store,
    key::TimeSeriesKey;
    time_range::TimeRangeArg=nothing,
) where {T <: SingleTimeSeries}
    _check_request_type(T)
    return get_time_series(store, key; time_range=time_range)
end

"""
    get_time_series(SingleTimeSeries, store, owner_id, owner_category, name; resolution, features, time_range)

Attribute-addressed counterpart to `get_time_series(store, key)`. `owner_category`
is the owner's `OwnerCategory` (`Component` or `SupplementalAttribute`). The
optional `time_range` `(start, stop)` slices like the key-based form.
"""
function get_time_series(
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
    time_range::TimeRangeArg=nothing,
) where {T <: SingleTimeSeries}
    _check_request_type(T)
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
    ::Type{T},
    store::Store,
    owner_id::Integer,
    owner_category::OwnerCategory,
    name::AbstractString;
    resolution::Union{Nothing, Period}=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
    time_range::TimeRangeArg=nothing,
) where {T <: NonSequentialTimeSeries}
    _check_request_type(T)
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
        store::Ptr{Cvoid}, key::Ptr{Cvoid}
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
        store::Ptr{Cvoid},
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
        store::Ptr{Cvoid}, key::Ptr{Cvoid}, out::Ref{Bool}
    )::Int32
    _check(code)
    return out[]
end

"""
    has_any_time_series(store; owner_id=nothing, owner_category=nothing,
                        time_series_type=nothing, name=nothing, resolution=nothing,
                        interval=nothing, features=nothing) -> Bool

True iff at least one stored time series matches the filter — the existence
probe over the same (all-optional, independent) filters as [`list_keys`](@ref),
answered off the catalog indexes without hydrating or marshaling any rows, so
it is safe for hot per-component loops. `features` is a subset match, unlike
the exact-key [`has_time_series`](@ref) forms, which compare the whole feature
set by content hash — but it stays on indexes: the store probes the requested
set as an exact set (by hash) first, so callers passing the complete feature
set get a single covering-index seek; only genuinely partial feature lists take
the indexed per-feature fallback probe.
"""
function has_any_time_series(
    store::Store;
    owner_id=nothing,
    owner_category=nothing,
    time_series_type=nothing,
    name=nothing,
    resolution=nothing,
    interval=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
    component_field=nothing,
    name_glob=nothing,
    zoneless=nothing,
)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, name_glob_arg, resolution_iso, interval_iso, features_json, component_field_arg, zoneless_arg) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features,
        component_field, name_glob, zoneless,
    )
    out = Ref{Bool}(false)
    code = @ccall lib_path().infrastore_store_has_any_by_filter(
        store::Ptr{Cvoid},
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        has_type::Bool,
        type_arg::Int32,
        name_arg::Cstring,
        name_glob_arg::Cstring,
        resolution_iso::Cstring,
        interval_iso::Cstring,
        features_json::Cstring,
        component_field_arg::Cstring,
        zoneless_arg::Int32,
        out::Ref{Bool},
    )::Int32
    _check(code)
    return out[]
end

"""
    get_counts(store) -> TimeSeriesCounts

Association counts: components with time series, static series, and forecasts.
"""
function get_counts(store::Store)
    a = Ref{Int64}(0)
    b = Ref{Int64}(0)
    c = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_counts(
        store::Ptr{Cvoid}, a::Ref{Int64}, b::Ref{Int64}, c::Ref{Int64}
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
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_counts_by_type(
            store::Ptr{Cvoid},
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    rows = JSON.parse(json)
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
        store::Ptr{Cvoid}, out::Ref{Int64}
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
    a = Ref{Int64}(0)
    b = Ref{Int64}(0)
    c = Ref{Int64}(0)
    d = Ref{Int64}(0)
    code = @ccall lib_path().infrastore_store_counts_detailed(
        store::Ptr{Cvoid}, a::Ref{Int64}, b::Ref{Int64}, c::Ref{Int64}, d::Ref{Int64}
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
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_list_owner_ids(
            store::Ptr{Cvoid},
            cat::Int32,
            has_type::Bool,
            type_arg::Int32,
            resolution_iso::Cstring,
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    return Int[Int(i) for i in JSON.parse(json)]
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
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_static_summary(
            store::Ptr{Cvoid},
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    return StaticSummaryRow[_decode_static_summary_row(r) for r in JSON.parse(json)]
end

"""
    forecast_summary(store) -> Vector{ForecastSummaryRow}

Grouped forecast summary: one row per distinct `(owner_type, owner_category,
time_series_type, name, initial_timestamp, resolution, horizon, interval,
window_count)` with `count` = the number of associations in the group.
"""
function forecast_summary(store::Store)
    json = _probe(
        (buf, cap, out_len) -> @ccall lib_path().infrastore_store_forecast_summary(
            store::Ptr{Cvoid},
            buf::Ptr{UInt8},
            cap::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    return ForecastSummaryRow[_decode_forecast_summary_row(r) for r in JSON.parse(json)]
end
