# ---- Timestamp readers ----------------------------------------------------
#
# Stateful readers for the simulation access pattern: a loop over every
# timestamp wants the value of every series at that instant. Build a reader
# once (it resolves the catalog layout and owns reusable buffers), then call
# the read function per timestamp and pull each group's / entry's values. The
# returned arrays are copies in canonical column-major layout, so they stay
# valid across subsequent reads.

# A forecast reader covers one forecast type, so it takes a narrower set than
# `_type_code`: static types are rejected here. A `Deterministic` reader spans
# both deterministic storage forms, matching the read request rule.
const _FORECAST_TYPES = (
    Deterministic, DeterministicSingleTimeSeries, Probabilistic, Scenarios
)

function _int_for_type(::Type{T}) where {T}
    T in _FORECAST_TYPES || throw(InvalidParameterError("$T is not a forecast type"))
    return _type_code(T)
end

# Copy `byte_len` bytes at `ptr` into a fresh `T` array and reshape from the
# stored row-major `dims` to canonical column-major Julia layout. The pointer is
# reader-owned (valid only until the next read), so we always copy.
function _reader_values(
    ptr::Ptr{UInt8}, byte_len::UInt64, ::Type{T}, dims::AbstractVector{<:Integer}
) where {T}
    n = byte_len == 0 ? 0 : Int(byte_len) ÷ sizeof(T)
    flat = Vector{T}(undef, n)
    if n > 0
        GC.@preserve flat unsafe_copyto!(pointer(flat), Ptr{T}(ptr), n)
    end
    nd = length(dims)
    nd <= 1 && return flat
    return permutedims(reshape(flat, reverse(dims)...), reverse(ntuple(identity, nd)))
end

# ---- StaticReader ---------------------------------------------------------

"""
One `(dtype, element_shape)` columnar group of a [`StaticReader`]. `keys[j]`
identifies column `j` of the values matrix returned by [`static_values`].
"""
struct StaticGroup
    dtype::DataType
    element_shape::Vector{Int}
    keys::Vector{TimeSeriesKey}
end

"""
A prepared reader over the static series matching a build filter — either the
`SingleTimeSeries` on one grid, or the `NonSequentialTimeSeries` on one timestamp
vector. Build with [`build_static_reader`], read a timestamp with
[`static_read!`], then pull each group's values with [`static_values`]. Inspect
the layout via [`static_groups`] / [`static_grid`] / [`static_timestamps`].
"""
mutable struct StaticReader
    handle::Ptr{Cvoid}
    store::Store
    groups::Vector{StaticGroup}
    function StaticReader(handle::Ptr{Cvoid}, store::Store, groups::Vector{StaticGroup})
        r = new(handle, store, groups)
        finalizer(_finalize_static_reader, r)
        return r
    end
end

function _finalize_static_reader(r::StaticReader)
    if r.handle != C_NULL
        @ccall lib_path().infrastore_static_reader_free(r::Ptr{Cvoid})::Cvoid
        r.handle = C_NULL
    end
end

# Root the reader for the duration of a ccall (see the note in store.jl).
Base.unsafe_convert(::Type{Ptr{Cvoid}}, r::StaticReader) = r.handle

function _static_group_layout(reader::StaticReader, gi::Integer)
    out_dtype = Ref{Int32}(0)
    out_ncols = Ref{UInt64}(0)
    out_shape_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_static_reader_group_info(
        reader::Ptr{Cvoid},
        UInt64(gi)::UInt64,
        out_dtype::Ref{Int32},
        out_ncols::Ref{UInt64},
        C_NULL::Ptr{Int64},
        UInt64(0)::UInt64,
        out_shape_len::Ref{UInt64},
    )::Int32
    _check(code)
    shape = Vector{Int64}(undef, Int(out_shape_len[]))
    if out_shape_len[] > 0
        code = @ccall lib_path().infrastore_static_reader_group_info(
            reader::Ptr{Cvoid},
            UInt64(gi)::UInt64,
            out_dtype::Ref{Int32},
            out_ncols::Ref{UInt64},
            shape::Ptr{Int64},
            UInt64(length(shape))::UInt64,
            out_shape_len::Ref{UInt64},
        )::Int32
        _check(code)
    end
    keys = Vector{TimeSeriesKey}(undef, Int(out_ncols[]))
    for col in 0:(Int(out_ncols[]) - 1)
        out_key = Ref{Ptr{Cvoid}}(C_NULL)
        code = @ccall lib_path().infrastore_static_reader_group_key(
            reader::Ptr{Cvoid},
            UInt64(gi)::UInt64,
            UInt64(col)::UInt64,
            out_key::Ref{Ptr{Cvoid}},
        )::Int32
        _check(code)
        keys[col + 1] = TimeSeriesKey(out_key[])
    end
    return StaticGroup(_julia_dtype(out_dtype[]), Int.(shape), keys)
end

"""
    build_static_reader(store; resolution=nothing, time_series_type=SingleTimeSeries,
                        owner_id=nothing, owner_category=nothing, name=nothing,
                        name_glob=nothing, features=Dict(), component_field=nothing)

Build a [`StaticReader`] over the static series matching the filter.

For `SingleTimeSeries` (the default) `resolution` (a `Period`) is required — one
resolution per reader — and the matched series must share one grid
(`initial_timestamp` + `length`). For `time_series_type=NonSequentialTimeSeries`
pass no `resolution`: an irregular series has none, and the matched series must
instead share one timestamp vector (read it with [`static_timestamps`]), which is
also what pools their arrays on disk.

The remaining keywords are [`list_keys`](@ref)'s filters, `name_glob` (a
case-sensitive SQLite `GLOB` pattern over the name) included.
"""
function build_static_reader(
    store::Store;
    resolution::Union{Nothing, Period}=nothing,
    time_series_type::Type=SingleTimeSeries,
    owner_id::Union{Nothing, Integer}=nothing,
    owner_category::Union{Nothing, OwnerCategory}=nothing,
    name::Union{Nothing, AbstractString}=nothing,
    name_glob::Union{Nothing, AbstractString}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    component_field::Union{Nothing, AbstractString}=nothing,
)
    time_series_type in (SingleTimeSeries, NonSequentialTimeSeries) || throw(
        InvalidParameterError(
            "build_static_reader handles the static types (SingleTimeSeries / " *
            "NonSequentialTimeSeries); got $time_series_type",
        ),
    )
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    name_glob_arg = name_glob === nothing ? C_NULL : String(name_glob)
    resolution_iso = resolution === nothing ? C_NULL : _period_to_iso(resolution)
    features_arg = isempty(features) ? C_NULL : JSON.json(features)
    component_field_arg = component_field === nothing ? C_NULL : String(component_field)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_build_static_reader(
        store::Ptr{Cvoid},
        _type_code(time_series_type)::Int32,
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        name_arg::Cstring,
        name_glob_arg::Cstring,
        resolution_iso::Cstring,
        features_arg::Cstring,
        component_field_arg::Cstring,
        out::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    # Wrap the raw handle in the finalized reader immediately, so a throw in
    # any of the layout queries below cannot leak it.
    reader = StaticReader(out[], store, StaticGroup[])
    out_n = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_static_reader_num_groups(
            reader::Ptr{Cvoid}, out_n::Ref{UInt64}
        )::Int32
    )
    append!(
        reader.groups, (_static_group_layout(reader, gi) for gi in 0:(Int(out_n[]) - 1))
    )
    return reader
end

"""
    static_grid(reader) -> StaticGrid

The reader's timeline. For a `SingleTimeSeries` reader the valid timestamps are
`initial_timestamp + k·resolution` for `k in 0:length-1`. For a
`NonSequentialTimeSeries` reader `resolution` is `nothing` — there is no constant
step — and [`static_timestamps`] gives the instants themselves.
"""
function static_grid(reader::StaticReader)
    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_static_reader_grid(
            reader::Ptr{Cvoid},
            out_initial::Ref{Int64},
            out_res::Ref{Ptr{Cchar}},
            out_len::Ref{UInt64},
        )::Int32
    )
    return StaticGrid(_from_unix_ms(out_initial[]), _take_period(out_res[]), Int(out_len[]))
end

"""
    static_timestamps(reader) -> Vector{DateTime}

Every timestamp on the reader's timeline, in order. The only way to enumerate an
irregular timeline, and equivalent to walking the grid for a regular one — so a
loop written against it works for either kind of reader.
"""
function static_timestamps(reader::StaticReader)
    out_len = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_static_reader_timestamps(
            reader::Ptr{Cvoid}, C_NULL::Ptr{Int64}, UInt64(0)::UInt64,
            out_len::Ref{UInt64},
        )::Int32
    )
    millis = Vector{Int64}(undef, Int(out_len[]))
    if out_len[] > 0
        _check(
            @ccall lib_path().infrastore_static_reader_timestamps(
                reader::Ptr{Cvoid}, millis::Ptr{Int64},
                UInt64(length(millis))::UInt64, out_len::Ref{UInt64},
            )::Int32
        )
    end
    return [_from_unix_ms(ms) for ms in millis]
end

"""
    static_groups(reader) -> Vector{StaticGroup}

The reader's columnar groups (resolved once at build time). Each [`StaticGroup`]
carries its `dtype`, `element_shape`, and the `keys` identifying each column.
"""
static_groups(reader::StaticReader) = reader.groups

"""
    static_read!(reader, t) -> reader

Read the value of every series at `t`, filling the reader's buffers. Throws if
`t` is off the reader's timeline. Follow with [`static_values`] per group.

`t` is a `DateTime` (read as UTC) or, with TimeZones loaded, a `ZonedDateTime`.
"""
function static_read!(reader::StaticReader, t)
    _check(
        @ccall lib_path().infrastore_static_reader_read(
            reader::Ptr{Cvoid},
            reader.store::Ptr{Cvoid},
            _to_unix_ms(t)::Int64,
        )::Int32
    )
    return reader
end

"""
    static_values(reader, group_index::Integer) -> Array

The values from the most recent [`static_read!`] for group `group_index`
(1-based), as a column-major array of size `(num_columns, element_shape...)`.
Column `j` corresponds to `static_groups(reader)[group_index].keys[j]`.
"""
function static_values(reader::StaticReader, group_index::Integer)
    group = reader.groups[group_index]
    out_ptr = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_static_reader_group_values(
            reader::Ptr{Cvoid},
            UInt64(group_index - 1)::UInt64,
            out_ptr::Ref{Ptr{UInt8}},
            out_len::Ref{UInt64},
        )::Int32
    )
    dims = vcat(length(group.keys), group.element_shape)
    return _reader_values(out_ptr[], out_len[], group.dtype, dims)
end

# ---- ForecastReader -------------------------------------------------------

"""
One forecast's entry in a [`ForecastReader`]. `key` identifies the forecast;
`window_shape` is the shape of a single window (`[H, *E]`, `[P, H, *E]`, or
`[scenarios, H, *E]`). `slot` is the 0-based index of the deduplicated window
read backing this entry — entries that share an array and read plan (e.g.
components referencing one shared forecast) report the same `slot`, so the
`.h5` data is read once per timestamp and a caller can group by `slot` to
materialize each unique window only once.
"""
struct ForecastEntry
    dtype::DataType
    window_shape::Vector{Int}
    key::TimeSeriesKey
    slot::Int
end

"""
A prepared reader over the forecasts of one type matching a build filter. Build
with [`build_forecast_reader`], read a timestamp with [`forecast_read!`], then
pull each entry's window with [`forecast_values`].
"""
mutable struct ForecastReader
    handle::Ptr{Cvoid}
    store::Store
    entries::Vector{ForecastEntry}
    function ForecastReader(
        handle::Ptr{Cvoid}, store::Store, entries::Vector{ForecastEntry}
    )
        r = new(handle, store, entries)
        finalizer(_finalize_forecast_reader, r)
        return r
    end
end

function _finalize_forecast_reader(r::ForecastReader)
    if r.handle != C_NULL
        @ccall lib_path().infrastore_forecast_reader_free(r::Ptr{Cvoid})::Cvoid
        r.handle = C_NULL
    end
end

# Root the reader for the duration of a ccall (see the note in store.jl).
Base.unsafe_convert(::Type{Ptr{Cvoid}}, r::ForecastReader) = r.handle

function _forecast_entry_layout(reader::ForecastReader, ei::Integer)
    out_dtype = Ref{Int32}(0)
    out_shape_len = Ref{UInt64}(0)
    code = @ccall lib_path().infrastore_forecast_reader_entry_info(
        reader::Ptr{Cvoid},
        UInt64(ei)::UInt64,
        out_dtype::Ref{Int32},
        C_NULL::Ptr{Int64},
        UInt64(0)::UInt64,
        out_shape_len::Ref{UInt64},
    )::Int32
    _check(code)
    shape = Vector{Int64}(undef, Int(out_shape_len[]))
    if out_shape_len[] > 0
        code = @ccall lib_path().infrastore_forecast_reader_entry_info(
            reader::Ptr{Cvoid},
            UInt64(ei)::UInt64,
            out_dtype::Ref{Int32},
            shape::Ptr{Int64},
            UInt64(length(shape))::UInt64,
            out_shape_len::Ref{UInt64},
        )::Int32
        _check(code)
    end
    out_key = Ref{Ptr{Cvoid}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_forecast_reader_entry_key(
            reader::Ptr{Cvoid}, UInt64(ei)::UInt64, out_key::Ref{Ptr{Cvoid}}
        )::Int32
    )
    out_slot = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_entry_slot(
            reader::Ptr{Cvoid}, UInt64(ei)::UInt64, out_slot::Ref{UInt64}
        )::Int32
    )
    return ForecastEntry(
        _julia_dtype(out_dtype[]), Int.(shape), TimeSeriesKey(out_key[]), Int(out_slot[])
    )
end

"""
    build_forecast_reader(store, time_series_type; resolution, owner_id=nothing,
                          owner_category=nothing, name=nothing, name_glob=nothing,
                          features=Dict(), component_field=nothing)

Build a [`ForecastReader`] over forecasts of `time_series_type` (a Julia type:
`Deterministic`, `Probabilistic`, `Scenarios`, or `DeterministicSingleTimeSeries`).
A `Deterministic` reader is abstract — it also includes
`DeterministicSingleTimeSeries`, read into identical `[H, *E]` windows.
`resolution` (a `Period`) is required; matched forecasts must share one window
timeline.

The remaining keywords are [`list_keys`](@ref)'s filters, `name_glob` (a
case-sensitive SQLite `GLOB` pattern over the name) included.
"""
function build_forecast_reader(
    store::Store,
    time_series_type::Type;
    resolution::Period,
    owner_id::Union{Nothing, Integer}=nothing,
    owner_category::Union{Nothing, OwnerCategory}=nothing,
    name::Union{Nothing, AbstractString}=nothing,
    name_glob::Union{Nothing, AbstractString}=nothing,
    features::AbstractDict=Dict{String, Any}(),
    component_field::Union{Nothing, AbstractString}=nothing,
)
    type_code = _int_for_type(time_series_type)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    name_glob_arg = name_glob === nothing ? C_NULL : String(name_glob)
    resolution_iso = _period_to_iso(resolution)
    features_arg = isempty(features) ? C_NULL : JSON.json(features)
    component_field_arg = component_field === nothing ? C_NULL : String(component_field)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_build_forecast_reader(
        store::Ptr{Cvoid},
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        Int32(type_code)::Int32,
        name_arg::Cstring,
        name_glob_arg::Cstring,
        resolution_iso::Cstring,
        features_arg::Cstring,
        component_field_arg::Cstring,
        out::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    # Wrap the raw handle in the finalized reader immediately, so a throw in
    # any of the layout queries below cannot leak it.
    reader = ForecastReader(out[], store, ForecastEntry[])
    out_n = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_num_entries(
            reader::Ptr{Cvoid}, out_n::Ref{UInt64}
        )::Int32
    )
    append!(
        reader.entries, (_forecast_entry_layout(reader, ei) for ei in 0:(Int(out_n[]) - 1))
    )
    return reader
end

"""
    forecast_timeline(reader) -> ForecastTimeline

The reader's window timeline. Valid timestamps are `initial_timestamp +
k·interval` for `k in 0:count-1`.
"""
function forecast_timeline(reader::ForecastReader)
    out_initial = Ref{Int64}(0)
    out_res = Ref{Ptr{Cchar}}(C_NULL)
    out_interval = Ref{Ptr{Cchar}}(C_NULL)
    out_count = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_timeline(
            reader::Ptr{Cvoid},
            out_initial::Ref{Int64},
            out_res::Ref{Ptr{Cchar}},
            out_interval::Ref{Ptr{Cchar}},
            out_count::Ref{UInt64},
        )::Int32
    )
    return ForecastTimeline(
        _from_unix_ms(out_initial[]),
        _take_period(out_res[]),
        _take_period(out_interval[]),
        Int(out_count[]),
    )
end

"""
    forecast_entries(reader) -> Vector{ForecastEntry}

The reader's per-key window entries (resolved once at build time). Each entry's
`slot` field identifies its deduplicated window read; entries sharing a `slot`
read the same `.h5` data once per timestamp.
"""
forecast_entries(reader::ForecastReader) = reader.entries

"""
    forecast_num_slots(reader) -> Int

The number of deduplicated window slots — i.e. the count of physical `.h5` reads
[`forecast_read!`] performs per timestamp. Entries that share an array and read
plan collapse to one slot, so this is `≤ length(forecast_entries(reader))`.
"""
function forecast_num_slots(reader::ForecastReader)
    out_n = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_num_slots(
            reader::Ptr{Cvoid}, out_n::Ref{UInt64}
        )::Int32
    )
    return Int(out_n[])
end

"""
    forecast_read!(reader, t) -> reader

Read the forecast window at `t` for every entry, filling the reader's buffers.
Throws if `t` is off the window timeline. Follow with [`forecast_values`].

`t` is a `DateTime` (read as UTC) or, with TimeZones loaded, a `ZonedDateTime`.
"""
function forecast_read!(reader::ForecastReader, t)
    _check(
        @ccall lib_path().infrastore_forecast_reader_read(
            reader::Ptr{Cvoid},
            reader.store::Ptr{Cvoid},
            _to_unix_ms(t)::Int64,
        )::Int32
    )
    return reader
end

"""
    forecast_values(reader, entry_index::Integer) -> Array

The window from the most recent [`forecast_read!`] for entry `entry_index`
(1-based), as a column-major array of size `window_shape`.
"""
function forecast_values(reader::ForecastReader, entry_index::Integer)
    entry = reader.entries[entry_index]
    out_ptr = Ref{Ptr{UInt8}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_entry_values(
            reader::Ptr{Cvoid},
            UInt64(entry_index - 1)::UInt64,
            out_ptr::Ref{Ptr{UInt8}},
            out_len::Ref{UInt64},
        )::Int32
    )
    return _reader_values(out_ptr[], out_len[], entry.dtype, entry.window_shape)
end
