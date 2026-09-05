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
    # Parameters first, so a metadata row's `time_series_type` names a reader as
    # readily as the bare type does.
    base = _base_time_series_type(T)
    base in _FORECAST_TYPES || throw(InvalidParameterError("$T is not a forecast type"))
    return _type_code(base)
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
One `(dtype, element_shape)` columnar group of a [`StaticReader`]. `ids[j]` is
the catalog association id of column `j` of the values matrix returned by
[`static_values`].

A group carries ids, not a snapshot of each column's attributes: resolve one
with [`get_metadata_by_id`](@ref) to recover an owner or a name. That resolves
against the row as it is *now*, so a rename between building the reader and
reading a column is visible — where a snapshot would have frozen the old name.
"""
struct StaticGroup
    dtype::DataType
    element_shape::Vector{Int}
    ids::Vector{Int64}
end

"""
A prepared reader over the static series matching a build filter — the
`SingleTimeSeries` on one grid, the `NonSequentialTimeSeries` on one timestamp
vector, or the `PersistentTimeSeries` matched by it, each on breakpoints of its
own. Build with [`build_static_reader`], read a timestamp with
[`static_read!`], then pull each group's values with [`static_values`]. Inspect
the layout via [`static_groups`] / [`static_grid`] / [`static_timestamps`].
"""
mutable struct StaticReader
    handle::Ptr{Cvoid}
    store::Store
    groups::Vector{StaticGroup}
    # The axis' spelling, read once at build time. A reader is the build-once,
    # sweep-many path, so asking the FFI for it on every `static_read!` would
    # put a string allocation and a round trip on every timestep of a
    # simulation.
    time_reference::Union{Nothing, TimeReference}
    function StaticReader(handle::Ptr{Cvoid}, store::Store, groups::Vector{StaticGroup})
        r = new(handle, store, groups, nothing)
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
    ids = Vector{Int64}(undef, Int(out_ncols[]))
    for col in 0:(Int(out_ncols[]) - 1)
        out_id = Ref{Int64}(0)
        code = @ccall lib_path().infrastore_static_reader_group_id(
            reader::Ptr{Cvoid},
            UInt64(gi)::UInt64,
            UInt64(col)::UInt64,
            out_id::Ref{Int64},
        )::Int32
        _check(code)
        ids[col + 1] = out_id[]
    end
    return StaticGroup(_julia_dtype(out_dtype[]), Int.(shape), ids)
end

"""
    build_static_reader(store; resolution=nothing, time_series_type=SingleTimeSeries,
                        owner_id=nothing, owner_category=nothing, name=nothing,
                        name_glob=nothing, features=nothing, component_field=nothing,
                        zoneless=nothing)

Build a [`StaticReader`] over the static series matching the filter.

For `SingleTimeSeries` (the default) `resolution` (a `Period`) is required — one
resolution per reader — and the matched series must share one grid
(`initial_timestamp` + `length`). For `time_series_type=NonSequentialTimeSeries`
pass no `resolution`: an irregular series has none, and the matched series must
instead share one timestamp vector (read it with [`static_timestamps`]), which is
also what pools their arrays on disk.

`time_series_type=PersistentTimeSeries` also takes no `resolution`, and is the
one case where the matched series need **not** share a timeline: a step function
has a value at every instant from its first breakpoint onward, so each column
resolves hold-last on its own breakpoints. The reader's timestamps are then the
union of every column's breakpoints — every instant at which some column changes
value. Reading at an instant before some column's first breakpoint is an error
naming that column.

The remaining keywords are [`list_metadata`](@ref)'s filters, `name_glob` (a
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
    features::Union{Nothing, AbstractDict}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
    zoneless::Union{Nothing, Bool}=nothing,
)
    # Parameters first, so a metadata row's `time_series_type` names a reader as
    # readily as the bare type does.
    static_type = _base_time_series_type(time_series_type)
    static_type in (SingleTimeSeries, NonSequentialTimeSeries, PersistentTimeSeries) ||
        throw(
            InvalidParameterError(
                "build_static_reader handles the static types (SingleTimeSeries / " *
                "NonSequentialTimeSeries / PersistentTimeSeries); got $time_series_type",
            ),
        )
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    name_glob_arg = name_glob === nothing ? C_NULL : String(name_glob)
    resolution_iso = resolution === nothing ? C_NULL : _period_to_iso(resolution)
    features_arg =
        (features === nothing || isempty(features)) ? C_NULL : JSON.json(features)
    component_field_arg = component_field === nothing ? C_NULL : String(component_field)
    # A reader materializes one timestamp axis, so it needs one spelling for it.
    # `-1` leaves the choice to the caller's other filters; the core refuses a
    # cohort that spans both coherence groups either way, naming the series that
    # disagree.
    zoneless_arg = zoneless === nothing ? Int32(-1) : Int32(zoneless ? 1 : 0)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_build_static_reader(
        store::Ptr{Cvoid},
        _type_code(static_type)::Int32,
        has_owner::Bool,
        owner_arg::Int64,
        has_category::Bool,
        category_arg::Int32,
        name_arg::Cstring,
        name_glob_arg::Cstring,
        resolution_iso::Cstring,
        features_arg::Cstring,
        component_field_arg::Cstring,
        zoneless_arg::Int32,
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
    reader.time_reference = _static_reader_reference(reader)
    return reader
end

"""
    static_grid(reader) -> StaticGrid

The reader's timeline. For a `SingleTimeSeries` reader the valid timestamps are
`initial_timestamp + k·resolution` for `k in 0:length-1`. For a
`NonSequentialTimeSeries` or `PersistentTimeSeries` reader `resolution` is
`nothing` — there is no constant step — and [`static_timestamps`] gives the
instants themselves (for a persistent reader, the union of its columns'
breakpoints).
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
    return StaticGrid(
        _from_unix_ms(out_initial[]),
        _take_period(out_res[]),
        Int(out_len[]),
        reader.time_reference,
    )
end

# The one spelling the reader's axis carries, or `nothing` when the cohort
# records none. A separate FFI call rather than another out parameter on
# `infrastore_static_reader_grid`: adding one there would shift every following
# argument for anything already compiled against that declaration.
function _static_reader_reference(reader::StaticReader)
    out_ref = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_static_reader_time_reference(
            reader::Ptr{Cvoid}, out_ref::Ref{Ptr{Cchar}}
        )::Int32
    )
    return _take_time_reference(out_ref[])
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

`t` must be spelled the way the reader's axis was written: a bare `DateTime` is a
wall clock (zoneless) and reads a zoneless axis; an axis that records instants
(`utc`, an offset, a zone name, or one whose `time_reference` is unspecified)
needs a `ZonedDateTime`, with TimeZones loaded. A mismatch throws
`InvalidParameterError`, as it does on a ranged read.
"""
# Refuse a point read whose spelling the reader's axis cannot answer.
#
# A point is a query bound like any other. The ranged reads carry the spelling
# through `_time_range_args`, and the core refuses a bound the series cannot
# answer; a point read sent only the instant, so the check was skipped -- a bare
# `DateTime` (a wall clock) could read an instant-bearing axis, and a
# `ZonedDateTime` a zoneless one, each reinterpreted as UTC and returning a
# *row* rather than the error the same mismatch earns on a range.
#
# An *unset* reference groups with the zoned variants, as it does in the core
# (`TimeReference::accepts_zoned_bound`): an unspecified spelling is not a
# floating third case that answers either, so a bare `DateTime` is refused
# against it exactly as `read_by_id(...; start_time=)` refuses one.
#
# `on_mismatch` runs just before the throw. A reader passes
# `_invalidate_reader!` through it: refusing here is refusing the read, so the
# reader owes the caller the same empty buffers a refusal inside the core would
# leave. The check itself stays free of side effects, which is how the tests
# exercise it directly.
function _check_point_spelling(
    axis::Union{Nothing, TimeReference}, t, what::AbstractString; on_mismatch=nothing
)
    bound_zoneless = is_zoneless(_time_reference_of(t))
    axis_zoneless = is_zoneless(axis)  # `is_zoneless(nothing)` is false
    bound_zoneless == axis_zoneless && return nothing
    on_mismatch === nothing || on_mismatch()
    if bound_zoneless
        spelled = if axis === nothing
            "time_reference unspecified"
        else
            "time_reference \"$(_time_reference_str(axis))\""
        end
        throw(
            InvalidParameterError(
                "the read timestamp carries no zone, but $what records instants " *
                "($spelled); a wall clock does not name one, and the store will not " *
                "guess a zone for it",
            ),
        )
    end
    return throw(
        InvalidParameterError(
            "the read timestamp names an instant, but $what is zoneless; its " *
            "timestamps are wall clocks, so there is no defined mapping from an " *
            "instant onto them",
        ),
    )
end

"""
    static_values(reader, group_index::Integer) -> Array

The values from the most recent [`static_read!`] for group `group_index`
(1-based), as a column-major array of size `(num_columns, element_shape...)`.
Column `j` corresponds to `static_groups(reader)[group_index].ids[j]`; resolve it
with [`get_metadata_by_id`](@ref) to recover the series it came from.
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
    dims = vcat(length(group.ids), group.element_shape)
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
    id::Int64
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
    # See `StaticReader.time_reference`.
    time_reference::Union{Nothing, TimeReference}
    function ForecastReader(
        handle::Ptr{Cvoid}, store::Store, entries::Vector{ForecastEntry}
    )
        r = new(handle, store, entries, nothing)
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
    out_id = Ref{Int64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_entry_id(
            reader::Ptr{Cvoid}, UInt64(ei)::UInt64, out_id::Ref{Int64}
        )::Int32
    )
    out_slot = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_forecast_reader_entry_slot(
            reader::Ptr{Cvoid}, UInt64(ei)::UInt64, out_slot::Ref{UInt64}
        )::Int32
    )
    return ForecastEntry(
        _julia_dtype(out_dtype[]), Int.(shape), out_id[], Int(out_slot[])
    )
end

"""
    build_forecast_reader(store, time_series_type; resolution, owner_id=nothing,
                          owner_category=nothing, name=nothing, name_glob=nothing,
                          features=nothing, component_field=nothing, zoneless=nothing)

Build a [`ForecastReader`] over forecasts of `time_series_type` (a Julia type:
`Deterministic`, `Probabilistic`, `Scenarios`, or `DeterministicSingleTimeSeries`).
A `Deterministic` reader is abstract — it also includes
`DeterministicSingleTimeSeries`, read into identical `[H, *E]` windows.
`resolution` (a `Period`) is required; matched forecasts must share one window
timeline.

The remaining keywords are [`list_metadata`](@ref)'s filters, `name_glob` (a
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
    features::Union{Nothing, AbstractDict}=nothing,
    component_field::Union{Nothing, AbstractString}=nothing,
    zoneless::Union{Nothing, Bool}=nothing,
)
    type_code = _int_for_type(time_series_type)
    has_owner = owner_id !== nothing
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    has_category = owner_category !== nothing
    category_arg = has_category ? _category_int(owner_category) : Int32(0)
    name_arg = name === nothing ? C_NULL : String(name)
    name_glob_arg = name_glob === nothing ? C_NULL : String(name_glob)
    resolution_iso = _period_to_iso(resolution)
    features_arg =
        (features === nothing || isempty(features)) ? C_NULL : JSON.json(features)
    component_field_arg = component_field === nothing ? C_NULL : String(component_field)
    # A reader materializes one timestamp axis, so it needs one spelling for it.
    # `-1` leaves the choice to the caller's other filters; the core refuses a
    # cohort that spans both coherence groups either way, naming the series that
    # disagree.
    zoneless_arg = zoneless === nothing ? Int32(-1) : Int32(zoneless ? 1 : 0)
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
        zoneless_arg::Int32,
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
    reader.time_reference = _forecast_reader_reference(reader)
    return reader
end

# The window timeline's spelling; the forecast counterpart of
# `_static_reader_reference`.
function _forecast_reader_reference(reader::ForecastReader)
    out_ref = Ref{Ptr{Cchar}}(C_NULL)
    _check(
        @ccall lib_path().infrastore_forecast_reader_time_reference(
            reader::Ptr{Cvoid}, out_ref::Ref{Ptr{Cchar}}
        )::Int32
    )
    return _take_time_reference(out_ref[])
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
        reader.time_reference,
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

function static_read!(reader::StaticReader, t)
    _check_point_spelling(
        reader.time_reference,
        t,
        "this reader's timeline";
        on_mismatch=() -> _invalidate_reader!(reader),
    )
    _check(
        @ccall lib_path().infrastore_static_reader_read(
            reader::Ptr{Cvoid},
            reader.store::Ptr{Cvoid},
            _to_unix_ms(t)::Int64,
        )::Int32
    )
    return reader
end

# Drop what the reader is holding, so a refusal in `_check_point_spelling`
# leaves it empty.
#
# The spelling check runs in Julia, because the ABI's `at_unix_ms` cannot
# carry the bound's spelling — so a mismatch throws without the core ever seeing
# the call, and the core's own "a failed read leaves the reader empty" rule
# never gets a chance to apply. Without this, a successful read followed by a
# mismatched one left `static_values`/`forecast_values` serving the *earlier*
# window as though it answered the timestamp that just failed.
#
# Always succeeds on a live handle, so the return code is deliberately dropped:
# this runs on the way to throwing, and a second error would replace the one the
# caller needs to see.
function _invalidate_reader!(reader::StaticReader)
    @ccall lib_path().infrastore_static_reader_invalidate(reader::Ptr{Cvoid})::Int32
    return nothing
end

function _invalidate_reader!(reader::ForecastReader)
    @ccall lib_path().infrastore_forecast_reader_invalidate(reader::Ptr{Cvoid})::Int32
    return nothing
end

"""
    forecast_read!(reader, t) -> reader

Read the forecast window at `t` for every entry, filling the reader's buffers.
Throws if `t` is off the window timeline. Follow with [`forecast_values`].

`t` must be spelled the way the reader's axis was written: a bare `DateTime` is a
wall clock (zoneless) and reads a zoneless axis; an axis that records instants
(`utc`, an offset, a zone name, or one whose `time_reference` is unspecified)
needs a `ZonedDateTime`, with TimeZones loaded. A mismatch throws
`InvalidParameterError`, as it does on a ranged read.
"""
function forecast_read!(reader::ForecastReader, t)
    _check_point_spelling(
        reader.time_reference,
        t,
        "this reader's timeline";
        on_mismatch=() -> _invalidate_reader!(reader),
    )
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
