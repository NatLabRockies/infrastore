# ---- Association catalogs --------------------------------------------------
#
# Two catalogs, kept apart on purpose. `SupplementalAttributeAssociation`
# records which attributes are attached to which components;
# `ParentChildAssociation` records directed edges between components (a
# generator connected to a bus, say). Neither has anything to do with time
# series: removing a series never removes an association, and vice versa.
#
# Filters cross the FFI as one JSON object rather than positional arguments,
# because two of the four fields are string lists. Expanding an abstract type
# into its concrete subtypes stays here on the Julia side; the Rust core only
# ever sees concrete type names.
#
# The two families expose the same shapes over different column names, so the
# wrappers are generated from the tables below rather than written twice. Every
# FFI export is still spelled out literally in a table, so each stays greppable.

"""
    SupplementalAttributeAssociation(component_id, component_type, attribute_id, attribute_type)

A supplemental attribute attached to a component.

Identity is the `(component_id, attribute_id)` pair: the type names are labels
carried for filtering, so re-attaching the same pair under different type names
is still a duplicate.
"""
struct SupplementalAttributeAssociation
    component_id::Int64
    component_type::String
    attribute_id::Int64
    attribute_type::String
end

"""
    ParentChildAssociation(parent_id, parent_type, child_id, child_type)

A directed edge between two components — e.g. a generator (parent) connected to
a bus (child).

Identity is the *ordered* `(parent_id, child_id)` pair, so the reversed pair is
a different edge. Both endpoints are always components.
"""
struct ParentChildAssociation
    parent_id::Int64
    parent_type::String
    child_id::Int64
    child_type::String
end

# Both rows are (id, type, id, type) quadruples, so identity, hashing, display,
# and JSON marshalling are one implementation over the field layout.
const _AssocRow = Union{SupplementalAttributeAssociation, ParentChildAssociation}

function Base.:(==)(a::T, b::T) where {T <: _AssocRow}
    return all(getfield(a, i) == getfield(b, i) for i in 1:fieldcount(T))
end

function Base.hash(a::_AssocRow, h::UInt)
    for i in 1:fieldcount(typeof(a))
        h = hash(getfield(a, i), h)
    end
    return h
end

# An attachment reads component <- attribute; an edge reads parent -> child.
_assoc_arrow(::Type{SupplementalAttributeAssociation}) = " <- "
_assoc_arrow(::Type{ParentChildAssociation}) = " -> "

function Base.show(io::IO, a::T) where {T <: _AssocRow}
    return print(
        io,
        "$(nameof(T))($(getfield(a, 2)) $(getfield(a, 1))" *
        "$(_assoc_arrow(T))$(getfield(a, 4)) $(getfield(a, 3)))",
    )
end

function _assoc_json(a::T) where {T <: _AssocRow}
    return Dict{String, Any}(String(f) => getfield(a, f) for f in fieldnames(T))
end

function _decode_assoc(::Type{T}, r::AbstractDict) where {T <: _AssocRow}
    return T(
        (
            ft <: Integer ? ft(r[String(f)]) : String(r[String(f)]) for
            (f, ft) in zip(fieldnames(T), fieldtypes(T))
        )...,
    )
end

# Build a filter payload for the FFI. Returns `C_NULL` when nothing is set, so
# the common "everything" query skips JSON entirely. An empty `Vector{String}`
# is a deliberate "none of these types" and is forwarded as such.
function _assoc_filter_json(pairs...)
    filter = Dict{String, Any}()
    for (key, value) in pairs
        value === nothing && continue
        filter[key] = value isa Integer ? Int64(value) : String[String(v) for v in value]
    end
    return isempty(filter) ? C_NULL : JSON.json(filter)
end

# ---- Wrapper generation ----------------------------------------------------
#
# A row type supplies its family's filter keywords: the id columns as-is, the
# type columns pluralized, because `component_type` filters on
# `component_types`, a list of concrete type names.

function _filter_fields(::Type{T}) where {T}
    return map(f -> endswith(String(f), "_id") ? f : Symbol(f, "s"), fieldnames(T))
end

_is_id_field(f::Symbol) = endswith(String(f), "_id")

function _assoc_filter_kwargs(::Type{T}) where {T}
    return [
        Expr(
            :kw,
            :($f::Union{Nothing, $(_is_id_field(f) ? :Integer : :AbstractVector)}),
            :nothing,
        ) for f in _filter_fields(T)
    ]
end

function _assoc_filter_call(::Type{T}) where {T}
    return Expr(
        :call, :_assoc_filter_json, (:($(String(f)) => $f) for f in _filter_fields(T))...
    )
end

# One table per family: public name, FFI export, body shape, and the shape's
# selector argument where it takes one. The row type is supplied by the loop and
# fixes the filter keywords.
const _SUPPLEMENTAL_FILTER_API = (
    (:has_supplemental_attribute_association,
        :infrastore_store_has_supplemental_attribute_association, :bool, nothing),
    (:list_supplemental_attribute_associations,
        :infrastore_store_list_supplemental_attribute_associations, :rows, nothing),
    (:list_supplemental_attribute_ids,
        :infrastore_store_list_supplemental_attribute_ids, :ids, nothing),
    (:list_components_with_attributes,
        :infrastore_store_list_components_with_attributes, :ids, nothing),
    (:remove_supplemental_attribute_associations!,
        :infrastore_store_remove_supplemental_attribute_associations, :count_u64, nothing),
    # One count export, selecting what to count: 0 = matching rows,
    # 1 = distinct attributes, 2 = distinct components.
    (:count_supplemental_attribute_associations,
        :infrastore_store_count_supplemental_attribute_associations, :count_kind, 0),
    (:count_supplemental_attributes,
        :infrastore_store_count_supplemental_attribute_associations, :count_kind, 1),
    (:count_components_with_attributes,
        :infrastore_store_count_supplemental_attribute_associations, :count_kind, 2),
)

const _PARENT_CHILD_FILTER_API = (
    (:has_parent_child_association,
        :infrastore_store_has_parent_child_association, :bool, nothing),
    (:list_parent_child_associations,
        :infrastore_store_list_parent_child_associations, :rows, nothing),
    # One id export, selecting the endpoint: 0 = parents, 1 = children.
    (:list_parents, :infrastore_store_list_parent_child_ids, :endpoint_ids, 0),
    (:list_children, :infrastore_store_list_parent_child_ids, :endpoint_ids, 1),
    (:remove_parent_child_associations!,
        :infrastore_store_remove_parent_child_associations, :count_u64, nothing),
    (:count_parent_child_associations,
        :infrastore_store_count_parent_child_associations, :count_i64, nothing),
)

for (T, api) in (
        (SupplementalAttributeAssociation, _SUPPLEMENTAL_FILTER_API),
        (ParentChildAssociation, _PARENT_CHILD_FILTER_API),
    ),
    (fname, sym, shape, selector) in api

    body = if shape === :bool
        quote
            out = Ref{Bool}(false)
            _check(
                @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid}, filter_json::Cstring, out::Ref{Bool}
                )::Int32
            )
            return out[]
        end
    elseif shape === :rows
        quote
            json = _probe(
                (buf, cap, len) -> @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid},
                    filter_json::Cstring,
                    buf::Ptr{UInt8},
                    cap::UInt64,
                    len::Ref{UInt64},
                )::Int32
            )
            return $T[_decode_assoc($T, r) for r in JSON.parse(json)]
        end
    elseif shape === :ids
        quote
            json = _probe(
                (buf, cap, len) -> @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid},
                    filter_json::Cstring,
                    buf::Ptr{UInt8},
                    cap::UInt64,
                    len::Ref{UInt64},
                )::Int32
            )
            return Int[Int(i) for i in JSON.parse(json)]
        end
    elseif shape === :endpoint_ids
        quote
            json = _probe(
                (buf, cap, len) -> @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid},
                    filter_json::Cstring,
                    Int32($selector)::Int32,
                    buf::Ptr{UInt8},
                    cap::UInt64,
                    len::Ref{UInt64},
                )::Int32
            )
            return Int[Int(i) for i in JSON.parse(json)]
        end
    elseif shape === :count_u64
        quote
            out = Ref{UInt64}(0)
            _check(
                @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid}, filter_json::Cstring, out::Ref{UInt64}
                )::Int32
            )
            return Int(out[])
        end
    elseif shape === :count_i64
        quote
            out = Ref{Int64}(0)
            _check(
                @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid}, filter_json::Cstring, out::Ref{Int64}
                )::Int32
            )
            return Int(out[])
        end
    elseif shape === :count_kind
        quote
            out = Ref{Int64}(0)
            _check(
                @ccall lib_path().$sym(
                    store.handle::Ptr{Cvoid},
                    filter_json::Cstring,
                    Int32($selector)::Int32,
                    out::Ref{Int64},
                )::Int32
            )
            return Int(out[])
        end
    else
        error("unknown association wrapper shape $shape")
    end
    @eval function $fname(store::Store; $(_assoc_filter_kwargs(T)...))
        filter_json = $(_assoc_filter_call(T))
        $body
    end
end

for (fname, T, sym) in (
    (:add_supplemental_attribute_association!, SupplementalAttributeAssociation,
        :infrastore_store_add_supplemental_attribute_association),
    (:add_parent_child_association!, ParentChildAssociation,
        :infrastore_store_add_parent_child_association),
)
    id1, type1, id2, type2 = fieldnames(T)
    @eval function $fname(store::Store, association::$T)
        _check(
            @ccall lib_path().$sym(
                store.handle::Ptr{Cvoid},
                association.$id1::Int64,
                association.$type1::Cstring,
                association.$id2::Int64,
                association.$type2::Cstring,
            )::Int32
        )
        return nothing
    end
end

for (fname, T, sym) in (
    (:add_supplemental_attribute_associations!, SupplementalAttributeAssociation,
        :infrastore_store_add_supplemental_attribute_associations),
    (:add_parent_child_associations!, ParentChildAssociation,
        :infrastore_store_add_parent_child_associations),
)
    @eval function $fname(store::Store, associations::AbstractVector{$T})
        payload = JSON.json([_assoc_json(a) for a in associations])
        out = Ref{UInt64}(0)
        _check(
            @ccall lib_path().$sym(
                store.handle::Ptr{Cvoid}, payload::Cstring, out::Ref{UInt64}
            )::Int32
        )
        return Int(out[])
    end
end

for (fname, sym) in (
    (:replace_supplemental_attribute_component_id!,
        :infrastore_store_replace_supplemental_attribute_component_id),
    (:replace_parent_child_component_id!,
        :infrastore_store_replace_parent_child_component_id),
)
    @eval function $fname(store::Store, old_id::Integer, new_id::Integer)
        out = Ref{UInt64}(0)
        _check(
            @ccall lib_path().$sym(
                store.handle::Ptr{Cvoid},
                Int64(old_id)::Int64,
                Int64(new_id)::Int64,
                out::Ref{UInt64},
            )::Int32
        )
        return Int(out[])
    end
end

# ---- Documentation for the generated wrappers ------------------------------
#
# The parent/child family mirrors the supplemental family shape for shape, so
# its entries point at the supplemental docs rather than restating them.

@doc """
    add_supplemental_attribute_association!(store, association)

Attach a supplemental attribute to a component. Throws
`DuplicateAssociationError` if that component already carries that attribute,
whatever type names are supplied.
""" add_supplemental_attribute_association!

@doc """
    add_supplemental_attribute_associations!(store, associations) -> Int

Attach many in one all-or-nothing transaction, returning the number inserted. A
duplicate anywhere in the batch rolls the whole batch back. This is the import
half of the round trip whose export is
[`list_supplemental_attribute_associations`](@ref) with no filter.
""" add_supplemental_attribute_associations!

@doc """
    has_supplemental_attribute_association(store; filters...) -> Bool

Whether any attachment matches the filter. Filter keywords, all optional and
ANDed: `component_id`, `component_types` (a `Vector{String}` of concrete type
names), `attribute_id`, `attribute_types`. With no filter, this is "does the
store hold any attachment at all".
""" has_supplemental_attribute_association

@doc """
    list_supplemental_attribute_associations(store; filters...) -> Vector{SupplementalAttributeAssociation}

Full attachment rows matching the filter (same keywords as
[`has_supplemental_attribute_association`](@ref)), in insertion order. With no
filter this exports the whole table, which is what a JSON serialization round
trip needs.
""" list_supplemental_attribute_associations

@doc """
    list_supplemental_attribute_ids(store; filters...) -> Vector{Int}

Distinct attribute ids matching the filter, ascending — the attributes attached
to a component when `component_id` is set.
""" list_supplemental_attribute_ids

@doc """
    list_components_with_attributes(store; filters...) -> Vector{Int}

Distinct component ids matching the filter, ascending — the components carrying
an attribute when `attribute_id` is set.
""" list_components_with_attributes

@doc """
    remove_supplemental_attribute_associations!(store; filters...) -> Int

Remove every attachment matching the filter, returning the number removed.
Removing nothing is not an error: callers that expect a specific count assert on
the return value.
""" remove_supplemental_attribute_associations!

@doc """
    replace_supplemental_attribute_component_id!(store, old_id, new_id) -> Int

Move every attachment from component `old_id` to `new_id`, returning the rows
updated. Throws `DuplicateAssociationError` if `new_id` already carries one of
the attributes being moved.
""" replace_supplemental_attribute_component_id!

@doc """
    count_supplemental_attribute_associations(store; filters...) -> Int

Number of attachments matching the filter.
""" count_supplemental_attribute_associations

@doc """
    count_supplemental_attributes(store; filters...) -> Int

Number of *distinct* attributes among the attachments matching the filter.
""" count_supplemental_attributes

@doc """
    count_components_with_attributes(store; filters...) -> Int

Number of *distinct* components among the attachments matching the filter.
""" count_components_with_attributes

@doc """
    add_parent_child_association!(store, association)

Record a directed edge between two components. Throws
`DuplicateAssociationError` if that ordered pair is already related; the
reversed pair is a different edge.
""" add_parent_child_association!

@doc """
    add_parent_child_associations!(store, associations) -> Int

Record many edges in one all-or-nothing transaction, returning the number
inserted.
""" add_parent_child_associations!

@doc """
    has_parent_child_association(store; filters...) -> Bool

Whether any edge matches the filter. Filter keywords, all optional and ANDed:
`parent_id`, `parent_types`, `child_id`, `child_types`.
""" has_parent_child_association

@doc """
    list_parent_child_associations(store; filters...) -> Vector{ParentChildAssociation}

Full edge rows matching the filter (same keywords as
[`has_parent_child_association`](@ref)), in insertion order.
""" list_parent_child_associations

@doc """
    list_children(store; filters...) -> Vector{Int}

Distinct child ids matching the filter, ascending — the children of a component
when `parent_id` is set.
""" list_children

@doc """
    list_parents(store; filters...) -> Vector{Int}

Distinct parent ids matching the filter, ascending — the parents of a component
when `child_id` is set.
""" list_parents

@doc """
    remove_parent_child_associations!(store; filters...) -> Int

Remove every edge matching the filter, returning the number removed. Removing
nothing is not an error.
""" remove_parent_child_associations!

@doc """
    replace_parent_child_component_id!(store, old_id, new_id) -> Int

Rewrite component `old_id` to `new_id` on both ends of every edge, returning the
rows updated. Throws `DuplicateAssociationError` if the rewrite would duplicate
an edge `new_id` already has.
""" replace_parent_child_component_id!

@doc """
    count_parent_child_associations(store; filters...) -> Int

Number of edges matching the filter.
""" count_parent_child_associations

"""
    supplemental_attribute_counts_by_type(store) -> Vector{SupplementalAttributeTypeCount}

Attachment counts grouped by attribute type, ordered by type.
"""
function supplemental_attribute_counts_by_type(store::Store)
    json = _probe(
        (buf, cap, len) ->
            @ccall lib_path().infrastore_store_supplemental_attribute_counts_by_type(
                store.handle::Ptr{Cvoid}, buf::Ptr{UInt8}, cap::UInt64, len::Ref{UInt64}
            )::Int32
    )
    return SupplementalAttributeTypeCount[
        SupplementalAttributeTypeCount(String(r["type"]), Int(r["count"])) for
        r in JSON.parse(json)
    ]
end

"""
    supplemental_attribute_summary(store) -> Vector{SupplementalAttributeSummaryRow}

Attachment counts grouped by both type names, ordered by attribute type then
component type. The core does the GROUP BY; callers build any presentation table.
"""
function supplemental_attribute_summary(store::Store)
    json = _probe(
        (buf, cap, len) ->
            @ccall lib_path().infrastore_store_supplemental_attribute_summary(
                store.handle::Ptr{Cvoid}, buf::Ptr{UInt8}, cap::UInt64, len::Ref{UInt64}
            )::Int32
    )
    return SupplementalAttributeSummaryRow[
        SupplementalAttributeSummaryRow(
            String(r["component_type"]), String(r["attribute_type"]), Int(r["count"])
        ) for r in JSON.parse(json)
    ]
end

# ---- Store-wide queries and maintenance ------------------------------------

"""
    get_forecast_parameters(store; resolution=nothing, interval=nothing) -> ForecastParameters

Return the store's [`ForecastParameters`](@ref), optionally restricted to
forecasts with the given `resolution` and/or `interval` (`Period`s). Every field
is `nothing` when no forecast matches.
"""
function get_forecast_parameters(
    store::Store;
    resolution::Union{Nothing, Period}=nothing,
    interval::Union{Nothing, Period}=nothing,
)
    present = Ref{Bool}(false)
    horizon_out = Ref{Ptr{Cchar}}(C_NULL)
    interval_out = Ref{Ptr{Cchar}}(C_NULL)
    count = Ref{Int64}(-1)
    resolution_out = Ref{Ptr{Cchar}}(C_NULL)
    initial_out = Ref{Int64}(-1)
    _check(
        @ccall lib_path().infrastore_store_get_forecast_parameters(
            store.handle::Ptr{Cvoid},
            _period_to_cstr(resolution)::Cstring,
            _period_to_cstr(interval)::Cstring,
            present::Ref{Bool},
            horizon_out::Ref{Ptr{Cchar}},
            interval_out::Ref{Ptr{Cchar}},
            count::Ref{Int64},
            resolution_out::Ref{Ptr{Cchar}},
            initial_out::Ref{Int64},
        )::Int32
    )
    return ForecastParameters(
        _take_period(horizon_out[]),
        _take_period(interval_out[]),
        count[] < 0 ? nothing : Int(count[]),
        _take_period(resolution_out[]),
        initial_out[] < 0 ? nothing : _from_unix_ms(initial_out[]),
    )
end

"""
    check_static_consistency(store; resolution=nothing) -> Vector{StaticGrid}

Verify that, per resolution, every `SingleTimeSeries` shares one
`(initial_timestamp, length)` grid, and return one [`StaticGrid`](@ref) per
resolution present (empty vector when there are none), ordered by
resolution. Series at different
resolutions legitimately have different grids, so consistency is only required
within a resolution; pass `resolution` (a `Period`) to scope the check to one
grid. Throws `IntegrityError` when the `SingleTimeSeries` at a single
resolution disagree on their `(initial_timestamp, length)`. One catalog query.
"""
function check_static_consistency(store::Store; resolution::Union{Nothing, Period}=nothing)
    fres = _period_to_cstr(resolution)
    json = _probe(
        (buf, cap, len) -> @ccall lib_path().infrastore_store_check_static_consistency(
            store.handle::Ptr{Cvoid},
            fres::Cstring,
            buf::Ptr{UInt8},
            cap::UInt64,
            len::Ref{UInt64},
        )::Int32
    )
    return StaticGrid[
        StaticGrid(
            _from_unix_ms(Int64(r["initial_timestamp_ms"])),
            _iso_to_period(String(r["resolution"])),
            Int(r["length"]),
        ) for r in JSON.parse(json)
    ]
end

# `get_resolutions` and `get_intervals` share a signature and a decode; only the
# export differs.
for (fname, sym) in (
    (:get_resolutions, :infrastore_store_get_resolutions),
    (:get_intervals, :infrastore_store_get_intervals),
)
    @eval function $fname(store::Store; time_series_type::Union{Nothing, Type}=nothing)
        has_type = time_series_type !== nothing
        type_arg = has_type ? _filter_type_code(time_series_type) : Int32(0)
        json = _probe(
            (buf, cap, len) -> @ccall lib_path().$sym(
                store.handle::Ptr{Cvoid},
                has_type::Bool,
                type_arg::Int32,
                buf::Ptr{UInt8},
                cap::UInt64,
                len::Ref{UInt64},
            )::Int32
        )
        return Period[_iso_to_period(String(s)) for s in JSON.parse(json)]
    end
end

@doc """
    get_resolutions(store; time_series_type=nothing) -> Vector{Period}

Return the distinct resolutions stored, in the core's stored (lexical-by-ISO)
order. When `time_series_type` (the Julia type) is given the result is
restricted to that type. This is a single catalog query in the core rather than
a scan of every association.
""" get_resolutions

@doc """
    get_intervals(store; time_series_type=nothing) -> Vector{Period}

Return the distinct forecast intervals stored (lexical-by-ISO order), the
interval analog of [`get_resolutions`](@ref). When `time_series_type` (the Julia
type) is given the result is restricted to that type; non-forecast types return
an empty vector.
""" get_intervals

"""
    read_only(store) -> Bool

Whether the store was opened read-only.
"""
function read_only(store::Store)
    out = Ref{Bool}(false)
    _check(
        @ccall lib_path().infrastore_store_read_only(
            store.handle::Ptr{Cvoid}, out::Ref{Bool}
        )::Int32
    )
    return out[]
end

"""
    get_compression(store) -> CompressionSettings

Return the store's [`CompressionSettings`](@ref). For a store opened from disk
this reflects the policy it was created with; in-memory stores report `:none`.
"""
function get_compression(store::Store)
    kind = Ref{UInt8}(0)
    level = Ref{UInt8}(0)
    shuffle = Ref{Bool}(false)
    _check(
        @ccall lib_path().infrastore_store_get_compression(
            store.handle::Ptr{Cvoid},
            kind::Ref{UInt8},
            level::Ref{UInt8},
            shuffle::Ref{Bool},
        )::Int32
    )
    return CompressionSettings(kind[] == 0 ? :none : :deflate, Int(level[]), shuffle[])
end

"""
    get_path(store) -> Union{Nothing,String}

Return the filesystem path backing the store's HDF5 array file, or `nothing` for an
in-memory store.
"""
function get_path(store::Store)
    has_path = Ref{Bool}(false)
    json = _probe(
        (buf, cap, len) -> @ccall lib_path().infrastore_store_get_path(
            store.handle::Ptr{Cvoid},
            has_path::Ref{Bool},
            buf::Ptr{UInt8},
            cap::UInt64,
            len::Ref{UInt64},
        )::Int32
    )
    # The probe sets `has_path` on its first (length-only) call; an in-memory
    # store reports no path and an empty body.
    return has_path[] ? json : nothing
end

"""
    verify_integrity(store) -> Int

Recompute each stored array's content hash and return how many disagree with the
hash recorded alongside them. `0` means every array checked out.

Checks the HDF5 half of the store only — the SQLite catalog is not inspected,
so `0` does not mean the store as a whole is sound. A catalog that is corrupted,
truncated, or paired with the wrong `.h5` file still returns `0`, while every read
of the affected series throws. For catalog-side checks use
[`check_static_consistency`] (per-resolution grid agreement) and [`compact!`]
(which reports the unreachable arrays and feature sets a delete left behind — an
expected state, not corruption).
"""
function verify_integrity(store::Store)
    out = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_store_verify(
            store.handle::Ptr{Cvoid}, out::Ref{UInt64}
        )::Int32
    )
    return Int(out[])
end

function compact!(store::Store)
    return _check(
        @ccall lib_path().infrastore_store_compact(store.handle::Ptr{Cvoid})::Int32
    )
end

"""
    flush!(store)

Flush pending writes (HDF5 arrays + SQLite metadata) to disk. After this the
on-disk `<path>.h5` and `<path>.sqlite` artifacts can be copied for persistence.
"""
function flush!(store::Store)
    return _check(@ccall lib_path().infrastore_store_flush(store.handle::Ptr{Cvoid})::Int32)
end

"""
    persist!(store, path)

Persist the store to `path` (HDF5 arrays) and `\$path.sqlite` (metadata), materializing
an in-memory store to disk. Existing target files are overwritten.
"""
function persist!(store::Store, path::AbstractString)
    _check(
        @ccall lib_path().infrastore_store_persist(
            store.handle::Ptr{Cvoid}, path::Cstring
        )::Int32
    )
    return nothing
end

"""
    clear!(store; owner_id=nothing, owner_category=nothing)

Remove all time series (data + metadata) from the store, or only those belonging
to a single owner. An owner is the pair `(owner_id, owner_category)`, so to scope
the clear to one owner pass both `owner_id` and `owner_category` (an
`OwnerCategory`). With neither given the whole store is cleared.
"""
function clear!(
    store::Store;
    owner_id::Union{Nothing, Integer}=nothing,
    owner_category::Union{Nothing, OwnerCategory}=nothing,
)
    has_owner = owner_id !== nothing
    if has_owner && owner_category === nothing
        throw(ArgumentError("clear! with owner_id also requires owner_category"))
    end
    _check(
        @ccall lib_path().infrastore_store_clear(
            store.handle::Ptr{Cvoid},
            has_owner::Bool,
            (has_owner ? Int64(owner_id) : Int64(0))::Int64,
            (has_owner ? _category_int(owner_category) : Int32(0))::Int32,
        )::Int32
    )
    return nothing
end

"""
    replace_owner!(store, old_owner_id, new_owner_id, owner_category) -> Int

Reassign every time series owned by `(old_owner_id, owner_category)` to
`(new_owner_id, owner_category)`. `owner_category` is the owner's `OwnerCategory`
(`Component` or `SupplementalAttribute`). The underlying arrays are
content-addressed and shared, so only the association records change. Returns the
number of associations updated.
"""
function replace_owner!(
    store::Store,
    old_owner_id::Integer,
    new_owner_id::Integer,
    owner_category::OwnerCategory,
)
    out = Ref{UInt64}(0)
    _check(
        @ccall lib_path().infrastore_store_replace_owner(
            store.handle::Ptr{Cvoid},
            Int64(old_owner_id)::Int64,
            Int64(new_owner_id)::Int64,
            _category_int(owner_category)::Int32,
            out::Ref{UInt64},
        )::Int32
    )
    return Int(out[])
end
