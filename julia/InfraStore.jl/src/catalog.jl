# ---- Catalog listings ------------------------------------------------------
#
# The wire carries no key. `list_metadata` is the identify half — it answers
# which series exist and hands back the catalog `id` that addresses each — and
# every read and removal takes that id.

# The Julia time series type for a key's integer type code.
function _type_for_code(code::Integer)
    if code == INFRASTORE_TYPE_SINGLE
        SingleTimeSeries
    elseif code == INFRASTORE_TYPE_NON_SEQUENTIAL
        NonSequentialTimeSeries
    elseif code == INFRASTORE_TYPE_PERSISTENT
        PersistentTimeSeries
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
    PersistentTimeSeries,
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
_type_code(::Type{PersistentTimeSeries}) = INFRASTORE_TYPE_PERSISTENT
_type_code(::Type{Deterministic}) = INFRASTORE_TYPE_DETERMINISTIC
_type_code(::Type{DeterministicSingleTimeSeries}) = INFRASTORE_TYPE_DETERMINISTIC_SINGLE
_type_code(::Type{Probabilistic}) = INFRASTORE_TYPE_PROBABILISTIC
_type_code(::Type{Scenarios}) = INFRASTORE_TYPE_SCENARIOS
function _type_code(::Type{T}) where {T}
    # `Type{}` is invariant, so a parameterized spelling never matches the
    # methods above and lands here. Strip the parameters and answer for the base
    # type: the store addresses a series by its identity — (owner, category,
    # type, name, resolution, interval, features) — which carries no element
    # type, so `{T,N}` can only restate what the matched arrays are, never
    # select between them. It is accepted rather than rejected so that the
    # parameterized `time_series_type` of a metadata row round-trips into every
    # type-taking call; what it names beyond the base type is ignored.
    base = _base_time_series_type(T)
    base === nothing && throw(InvalidParameterError("$T is not a time series type"))
    return _type_code(base)
end

# The unparameterized time series type `T` is a spelling of, or `nothing` if it
# is no kind of time series. `T` itself when it is already bare.
function _base_time_series_type(::Type{T}) where {T}
    for base in _TIME_SERIES_TYPES
        T <: base && return base
    end
    return nothing
end

# The type code a catalog *filter* takes: any stored type, in any spelling.
# `Deterministic` is widened to both deterministic storage forms by the core's
# catalog predicate, so a filter never has to name
# `DeterministicSingleTimeSeries` to see it.
_filter_type_code(::Type{T}) where {T} = Int32(_type_code(T))

# The Julia time series type for a metadata row's type name (the `as_str` form).
function _type_for_name(name::AbstractString)
    if name == "SingleTimeSeries"
        SingleTimeSeries
    elseif name == "NonSequentialTimeSeries"
        NonSequentialTimeSeries
    elseif name == "PersistentTimeSeries"
        PersistentTimeSeries
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

# The *parameterized* Julia type of a metadata row: the row's stored type with
# the `{T, N}` of the values it holds, so `md.time_series_type` names what a
# read of that row hands back rather than only which of the six kinds it is. Both
# parameters come from the row itself, so this needs no extra query and no
# change to what is stored.
#
# For a plain numeric series `T` is the dtype and `N` is one more than the rank of
# `element_shape`, which the core records as the stored array's shape after its
# leading axis.
#
# For a *composite* element type the row names what a read hands back, which is
# the decoded values: `T` is the domain type — `PiecewiseLinear`,
# `NTuple{3, Float64}` — and `N` is one *lower*, because the axis the values were
# packed across is the axis decoding consumes. Keep this in step with
# `_read_values`: the two describe the same read, one as a type and one as data.
#
# A `DeterministicSingleTimeSeries` is the exception, because it is a view: its
# row carries the `element_shape` of the source `SingleTimeSeries`, while a read
# materializes the `(H, count, element_dims...)` array of the `Deterministic` it
# becomes — one axis more.
#
# An `element_type` this wrapper does not recognize leaves the base type bare
# rather than guessing: the core owns the vocabulary, and a row written by a
# newer one must still decode.
function _parameterized_type(
    name::AbstractString, element_type::AbstractString, element_shape
)
    base = _type_for_name(name)
    extra = base === DeterministicSingleTimeSeries ? 2 : 1
    # A composite element type is decoded on read, and the values it decodes to
    # occupy the trailing axis it was packed across — so the logical rank is one
    # lower than the stored array's, and `T` is the domain type rather than the
    # dtype the bytes are held in.
    if is_composite_element_type(element_type)
        value_type = _decoded_value_type(element_type)
        value_type === nothing || return base{value_type, length(element_shape) + extra - 1}
    end
    dtype = _physical_dtype_of(element_type)
    dtype === nothing && return base
    return base{dtype, length(element_shape) + extra}
end

# The Julia type a composite `element_type` decodes to, or `nothing` when this
# version does not map it — in which case the row keeps describing the stored
# numbers, which is what a read of it hands back.
function _decoded_value_type(element_type::AbstractString)
    kind = _element_kind(element_type)
    kind === :tuple || return get(DEFAULT_ELEMENT_TYPES, kind, nothing)
    m = match(_TUPLE_TAG, element_type)
    return m === nothing ? nothing : NTuple{parse(Int, m.captures[1]), Float64}
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

function _decode_metadata(r::AbstractDict)
    percentiles = r["percentiles"]
    element_type = String(r["element_type"])
    element_shape = Tuple(Int(d) for d in r["element_shape"])
    return TimeSeriesMetadata(
        Int64(r["owner_id"]),
        String(r["owner_type"]),
        _category_for_name(r["owner_category"]),
        _parameterized_type(r["time_series_type"], element_type, element_shape),
        String(r["name"]),
        hex2bytes(String(r["data_hash"])),
        _row_timestamp(r["initial_timestamp_ms"]),
        _row_period(r["resolution"]),
        _row_period(r["horizon"]),
        _row_period(r["interval"]),
        _row_int(r["count"]),
        _row_int(r["length"]),
        percentiles === nothing ? nothing : Vector{Float64}(percentiles),
        element_type,
        element_shape,
        Dict{String, Any}(r["features"]),
        r["units"] === nothing ? nothing : String(r["units"]),
        r["quantity_kind"] === nothing ? nothing : String(r["quantity_kind"]),
        _unit_system(r["unit_system"] === nothing ? nothing : String(r["unit_system"])),
        _row_time_reference(r),
        r["component_field"] === nothing ? nothing : String(r["component_field"]),
        r["application_data"] === nothing ? nothing : String(r["application_data"]),
        # Always present on a row that came from the catalog.
        r["id"] === nothing ? nothing : Int64(r["id"]),
    )
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
    list_metadata(store; owner_id=nothing, owner_category=nothing, time_series_type=nothing,
              name=nothing, resolution=nothing, interval=nothing, features=nothing) -> Vector{TimeSeriesMetadata}

The catalog row of every stored time series matching the (all-optional,
independent) filters, as [`TimeSeriesMetadata`](@ref)s. With no filter set the
whole store is listed.

The listing that answers identity questions: which series exist, what type and
grid each is, which array each resolves to (`data_hash`), and the `id` that
addresses it. A row carries no timestamp vector — an irregular series' time axis
is the one part of a row that costs a read per row, so a listing omits it and
[`read_by_id`](@ref) returns the series with its axis.

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
- `features` — match rows whose features include all the given entries (subset).
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
function list_metadata(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_metadata, store; kwargs...)
    return TimeSeriesMetadata[_decode_metadata(r) for r in JSON.parse(json)]
end

"""
    list_metadata_by_ids(store, ids) -> Vector{TimeSeriesMetadata}

The catalog rows named by `ids`, in the order the ids are given.

[`list_metadata`](@ref) addressed by id instead of by attributes — the bulk
companion to [`get_metadata_by_id`](@ref), and what a consumer hydrating a model
full of recorded ids wants: one catalog query for the whole set rather than one
call per reference.

Throws `NotFoundError` if any id names no row. A listing by attributes returns
what matches, but a caller naming ids is asserting they exist, and a silently
short result would let a stale reference pass as an absent match. Sift the set
with [`association_exists`](@ref) first when some are expected to have gone.
Repeats are returned once each, in place.
"""
function list_metadata_by_ids(store::Store, ids::AbstractVector{<:Integer})
    raw = Int64[Int64(i) for i in ids]
    json = _owned_str(
        (out_json, out_len) -> @ccall lib_path().infrastore_store_list_metadata_by_ids(
            store::Ptr{Cvoid},
            raw::Ptr{Int64},
            length(raw)::UInt64,
            out_json::Ref{Ptr{Cchar}},
            out_len::Ref{UInt64},
        )::Int32
    )
    return TimeSeriesMetadata[_decode_metadata(r) for r in JSON.parse(json)]
end

"""
    list_names(store; filters...) -> Vector{String}

Distinct series names matching the filter (same filters as [`list_metadata`](@ref)),
sorted.
"""
function list_names(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_names, store; kwargs...)
    return String[String(s) for s in JSON.parse(json)]
end

"""
    list_owner_types(store; filters...) -> Vector{String}

Distinct owner types matching the filter (same filters as [`list_metadata`](@ref)),
sorted.
"""
function list_owner_types(store::Store; kwargs...)
    json = _filter_list_json(:infrastore_store_list_owner_types, store; kwargs...)
    return String[String(s) for s in JSON.parse(json)]
end

"""
    remove_by_filter!(store; filters...) -> Int

Remove every series matching the filter (same filters as [`list_metadata`](@ref)) in
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
    remove_by_ids!(store, ids; owner=nothing) -> Int

Remove many associations named by their catalog `id`, in one all-or-nothing
transaction, returning the number removed.

The removal direction of the id every write hands back: a caller that recorded
ids in its own model retires one without rebuilding the identity it was filed
under, and an id names exactly one row where a key could match a whole forecast
family. Throws `NotFoundError` if any id
names no row, leaving the store untouched — sift the set with
[`association_exists`](@ref) first when some references are expected to have
gone. A repeated id is removed, and counted, once.

Pass `owner = (owner_id, category)` to hold every id to that owner: the row's
owner is read and the row deleted by the same transaction, and a row belonging
to anyone else throws [`OwnerMismatchError`](@ref) with the whole batch rolled
back.

A caller that means "retire *this* owner's series" must use the guard rather
than confirming the owner in a call of its own. An id survives a reassignment,
so a separate check has a window after it in which the row can move to another
owner, and the removal then retires that owner's series — the very thing the
check was for. The category is half the owner, since a component and a
supplemental attribute can share an integer id.

See also [`read_by_ids`](@ref), the read direction of the same reference.
"""
function remove_by_ids!(
    store::Store,
    ids::AbstractVector{<:Integer};
    owner::Union{Nothing, Tuple{Integer, OwnerCategory}}=nothing,
)
    isempty(ids) && return 0
    id_vec = Int64[Int64(id) for id in ids]
    (has_owner, owner_id, owner_category) = _owner_guard(owner)
    out_removed = Ref{UInt64}(0)
    code = GC.@preserve id_vec @ccall lib_path().infrastore_store_remove_by_ids(
        store::Ptr{Cvoid},
        id_vec::Ptr{Int64},
        UInt64(length(id_vec))::UInt64,
        has_owner::Bool,
        owner_id::Int64,
        owner_category::Int32,
        out_removed::Ref{UInt64},
    )::Int32
    _check(code)
    return Int(out_removed[])
end

"""
    has_any_time_series(store; owner_id=nothing, owner_category=nothing,
                        time_series_type=nothing, name=nothing, resolution=nothing,
                        interval=nothing, features=nothing) -> Bool

True iff at least one stored time series matches the filter — the existence
probe over the same (all-optional, independent) filters as [`list_metadata`](@ref),
answered off the catalog indexes without hydrating or marshaling any rows, so
it is safe for hot per-component loops.

An existence question is an *identify* operation, which is why it stayed
attribute-addressed when the reads moved to ids: routing it through a
resolution would trade an index seek for a row fetch in exactly the loops it
exists for. `features` is a subset match here — but it stays on indexes: the
store probes the requested
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

Grouped static-series (SingleTimeSeries + NonSequentialTimeSeries +
PersistentTimeSeries) summary: one
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
