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

function Base.:(==)(
    a::SupplementalAttributeAssociation, b::SupplementalAttributeAssociation
)
    return a.component_id == b.component_id &&
           a.component_type == b.component_type &&
           a.attribute_id == b.attribute_id &&
           a.attribute_type == b.attribute_type
end

function Base.hash(a::SupplementalAttributeAssociation, h::UInt)
    h = hash(a.component_id, h)
    h = hash(a.component_type, h)
    h = hash(a.attribute_id, h)
    return hash(a.attribute_type, h)
end

function Base.show(io::IO, a::SupplementalAttributeAssociation)
    return print(
        io,
        "SupplementalAttributeAssociation(",
        a.component_type,
        " ",
        a.component_id,
        " <- ",
        a.attribute_type,
        " ",
        a.attribute_id,
        ")",
    )
end

function Base.:(==)(a::ParentChildAssociation, b::ParentChildAssociation)
    return a.parent_id == b.parent_id &&
           a.parent_type == b.parent_type &&
           a.child_id == b.child_id &&
           a.child_type == b.child_type
end

function Base.hash(a::ParentChildAssociation, h::UInt)
    h = hash(a.parent_id, h)
    h = hash(a.parent_type, h)
    h = hash(a.child_id, h)
    return hash(a.child_type, h)
end

function Base.show(io::IO, a::ParentChildAssociation)
    return print(
        io,
        "ParentChildAssociation(",
        a.parent_type,
        " ",
        a.parent_id,
        " -> ",
        a.child_type,
        " ",
        a.child_id,
        ")",
    )
end

# Build a filter payload for the FFI. Returns `C_NULL` when nothing is set, so
# the common "everything" query skips JSON entirely. An empty `Vector{String}`
# is a deliberate "none of these types" and is forwarded as such.
function _assoc_filter_json(pairs...)
    filter = Dict{String,Any}()
    for (key, value) in pairs
        value === nothing && continue
        filter[key] = value isa Integer ? Int64(value) : String[String(v) for v in value]
    end
    return isempty(filter) ? C_NULL : JSON.json(filter)
end

function _supplemental_filter_json(
    component_id, component_types, attribute_id, attribute_types
)
    return _assoc_filter_json(
        "component_id" => component_id,
        "component_types" => component_types,
        "attribute_id" => attribute_id,
        "attribute_types" => attribute_types,
    )
end

function _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_filter_json(
        "parent_id" => parent_id,
        "parent_types" => parent_types,
        "child_id" => child_id,
        "child_types" => child_types,
    )
end

function _supplemental_json(a::SupplementalAttributeAssociation)
    return Dict(
        "component_id" => a.component_id,
        "component_type" => a.component_type,
        "attribute_id" => a.attribute_id,
        "attribute_type" => a.attribute_type,
    )
end

function _parent_child_json(a::ParentChildAssociation)
    return Dict(
        "parent_id" => a.parent_id,
        "parent_type" => a.parent_type,
        "child_id" => a.child_id,
        "child_type" => a.child_type,
    )
end

function _decode_supplemental(r::AbstractDict)
    return SupplementalAttributeAssociation(
        Int64(r["component_id"]),
        String(r["component_type"]),
        Int64(r["attribute_id"]),
        String(r["attribute_type"]),
    )
end

function _decode_parent_child(r::AbstractDict)
    return ParentChildAssociation(
        Int64(r["parent_id"]),
        String(r["parent_type"]),
        Int64(r["child_id"]),
        String(r["child_type"]),
    )
end

# Shared result handling for the two families. Julia requires a `ccall` symbol
# to be a literal, so each call site names its own export and passes a closure
# that performs the call with the out pointer supplied here — the same shape as
# `_filter_probe` above.

# For exports returning a row/entity count through a `u64` out pointer.
function _assoc_count_out(ccall_once)
    out = Ref{UInt64}(0)
    _check(ccall_once(out))
    return Int(out[])
end

# For exports returning a `bool` out pointer.
function _assoc_bool_out(ccall_once)
    out = Ref{Bool}(false)
    _check(ccall_once(out))
    return out[]
end

# For exports returning an `i64` count out pointer.
function _assoc_i64_out(ccall_once)
    out = Ref{Int64}(0)
    _check(ccall_once(out))
    return Int(out[])
end

"""
    add_supplemental_attribute_association!(store, association)

Attach a supplemental attribute to a component. Throws
`DuplicateAssociationError` if that component already carries that attribute,
whatever type names are supplied.
"""
function add_supplemental_attribute_association!(
    store::Store, association::SupplementalAttributeAssociation
)
    _check(
        ccall(
            (:infrastore_store_add_supplemental_attribute_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Cstring, Int64, Cstring),
            store.handle,
            association.component_id,
            association.component_type,
            association.attribute_id,
            association.attribute_type,
        ),
    )
    return nothing
end

"""
    add_supplemental_attribute_associations!(store, associations) -> Int

Attach many in one all-or-nothing transaction, returning the number inserted. A
duplicate anywhere in the batch rolls the whole batch back. This is the import
half of the round trip whose export is
[`list_supplemental_attribute_associations`](@ref) with no filter.
"""
function add_supplemental_attribute_associations!(
    store::Store, associations::AbstractVector{SupplementalAttributeAssociation}
)
    payload = JSON.json([_supplemental_json(a) for a in associations])
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_add_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            payload,
            out,
        ),
    )
end

"""
    has_supplemental_attribute_association(store; filters...) -> Bool

Whether any attachment matches the filter. Filter keywords, all optional and
ANDed: `component_id`, `component_types` (a `Vector{String}` of concrete type
names), `attribute_id`, `attribute_types`. With no filter, this is "does the
store hold any attachment at all".
"""
function has_supplemental_attribute_association(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    return _assoc_bool_out(
        out -> ccall(
            (:infrastore_store_has_supplemental_attribute_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Bool}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    list_supplemental_attribute_associations(store; filters...) -> Vector{SupplementalAttributeAssociation}

Full attachment rows matching the filter (same keywords as
[`has_supplemental_attribute_association`](@ref)), in insertion order. With no
filter this exports the whole table, which is what a JSON serialization round
trip needs.
"""
function list_supplemental_attribute_associations(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_list_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return SupplementalAttributeAssociation[
        _decode_supplemental(r) for r in JSON.parse(json)
    ]
end

"""
    list_supplemental_attribute_ids(store; filters...) -> Vector{Int}

Distinct attribute ids matching the filter, ascending — the attributes attached
to a component when `component_id` is set.
"""
function list_supplemental_attribute_ids(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_list_supplemental_attribute_ids, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    list_components_with_attributes(store; filters...) -> Vector{Int}

Distinct component ids matching the filter, ascending — the components carrying
an attribute when `attribute_id` is set.
"""
function list_components_with_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_list_components_with_attributes, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    remove_supplemental_attribute_associations!(store; filters...) -> Int

Remove every attachment matching the filter, returning the number removed.
Removing nothing is not an error: callers that expect a specific count assert on
the return value.
"""
function remove_supplemental_attribute_associations!(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _supplemental_filter_json(
        component_id, component_types, attribute_id, attribute_types
    )
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_remove_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    replace_supplemental_attribute_component_id!(store, old_id, new_id) -> Int

Move every attachment from component `old_id` to `new_id`, returning the rows
updated. Throws `DuplicateAssociationError` if `new_id` already carries one of
the attributes being moved.
"""
function replace_supplemental_attribute_component_id!(
    store::Store, old_id::Integer, new_id::Integer
)
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_replace_supplemental_attribute_component_id, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Int64, Ref{UInt64}),
            store.handle,
            Int64(old_id),
            Int64(new_id),
            out,
        ),
    )
end

# `kind`: 0 = matching rows, 1 = distinct attributes, 2 = distinct components.
function _supplemental_count(store::Store, filter_json, kind::Integer)
    out = Ref{Int64}(0)
    _check(
        ccall(
            (:infrastore_store_count_supplemental_attribute_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Int32, Ref{Int64}),
            store.handle,
            filter_json,
            Int32(kind),
            out,
        ),
    )
    return Int(out[])
end

"""
    count_supplemental_attribute_associations(store; filters...) -> Int

Number of attachments matching the filter.
"""
function count_supplemental_attribute_associations(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        0,
    )
end

"""
    count_supplemental_attributes(store; filters...) -> Int

Number of *distinct* attributes among the attachments matching the filter.
"""
function count_supplemental_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        1,
    )
end

"""
    count_components_with_attributes(store; filters...) -> Int

Number of *distinct* components among the attachments matching the filter.
"""
function count_components_with_attributes(
    store::Store;
    component_id::Union{Nothing,Integer}=nothing,
    component_types::Union{Nothing,AbstractVector}=nothing,
    attribute_id::Union{Nothing,Integer}=nothing,
    attribute_types::Union{Nothing,AbstractVector}=nothing,
)
    return _supplemental_count(
        store,
        _supplemental_filter_json(
            component_id, component_types, attribute_id, attribute_types
        ),
        2,
    )
end

"""
    supplemental_attribute_counts_by_type(store) -> Vector{SupplementalAttributeTypeCount}

Attachment counts grouped by attribute type, ordered by type.
"""
function supplemental_attribute_counts_by_type(store::Store)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_supplemental_attribute_counts_by_type, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            buf,
            cap,
            out_len,
        ),
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
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_supplemental_attribute_summary, lib_path()),
            Int32,
            (Ptr{Cvoid}, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            buf,
            cap,
            out_len,
        ),
    )
    return SupplementalAttributeSummaryRow[
        SupplementalAttributeSummaryRow(
            String(r["component_type"]), String(r["attribute_type"]), Int(r["count"])
        ) for r in JSON.parse(json)
    ]
end

"""
    add_parent_child_association!(store, association)

Record a directed edge between two components. Throws
`DuplicateAssociationError` if that ordered pair is already related; the
reversed pair is a different edge.
"""
function add_parent_child_association!(store::Store, association::ParentChildAssociation)
    _check(
        ccall(
            (:infrastore_store_add_parent_child_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Cstring, Int64, Cstring),
            store.handle,
            association.parent_id,
            association.parent_type,
            association.child_id,
            association.child_type,
        ),
    )
    return nothing
end

"""
    add_parent_child_associations!(store, associations) -> Int

Record many edges in one all-or-nothing transaction, returning the number
inserted.
"""
function add_parent_child_associations!(
    store::Store, associations::AbstractVector{ParentChildAssociation}
)
    payload = JSON.json([_parent_child_json(a) for a in associations])
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_add_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            payload,
            out,
        ),
    )
end

"""
    has_parent_child_association(store; filters...) -> Bool

Whether any edge matches the filter. Filter keywords, all optional and ANDed:
`parent_id`, `parent_types`, `child_id`, `child_types`.
"""
function has_parent_child_association(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_bool_out(
        out -> ccall(
            (:infrastore_store_has_parent_child_association, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Bool}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    list_parent_child_associations(store; filters...) -> Vector{ParentChildAssociation}

Full edge rows matching the filter (same keywords as
[`has_parent_child_association`](@ref)), in insertion order.
"""
function list_parent_child_associations(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_list_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            buf,
            cap,
            out_len,
        ),
    )
    return ParentChildAssociation[_decode_parent_child(r) for r in JSON.parse(json)]
end

# `endpoint`: 0 = parents, 1 = children.
function _parent_child_ids(store::Store, filter_json, endpoint::Integer)
    json = _filter_probe(
        store,
        (buf, cap, out_len) -> ccall(
            (:infrastore_store_list_parent_child_ids, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
            store.handle,
            filter_json,
            Int32(endpoint),
            buf,
            cap,
            out_len,
        ),
    )
    return Int[Int(i) for i in JSON.parse(json)]
end

"""
    list_children(store; filters...) -> Vector{Int}

Distinct child ids matching the filter, ascending — the children of a component
when `parent_id` is set.
"""
function list_children(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    return _parent_child_ids(
        store, _parent_child_filter_json(parent_id, parent_types, child_id, child_types), 1
    )
end

"""
    list_parents(store; filters...) -> Vector{Int}

Distinct parent ids matching the filter, ascending — the parents of a component
when `child_id` is set.
"""
function list_parents(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    return _parent_child_ids(
        store, _parent_child_filter_json(parent_id, parent_types, child_id, child_types), 0
    )
end

"""
    remove_parent_child_associations!(store; filters...) -> Int

Remove every edge matching the filter, returning the number removed. Removing
nothing is not an error.
"""
function remove_parent_child_associations!(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    filter_json = _parent_child_filter_json(parent_id, parent_types, child_id, child_types)
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_remove_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{UInt64}),
            store.handle,
            filter_json,
            out,
        ),
    )
end

"""
    replace_parent_child_component_id!(store, old_id, new_id) -> Int

Rewrite component `old_id` to `new_id` on both ends of every edge, returning the
rows updated. Throws `DuplicateAssociationError` if the rewrite would duplicate
an edge `new_id` already has.
"""
function replace_parent_child_component_id!(store::Store, old_id::Integer, new_id::Integer)
    return _assoc_count_out(
        out -> ccall(
            (:infrastore_store_replace_parent_child_component_id, lib_path()),
            Int32,
            (Ptr{Cvoid}, Int64, Int64, Ref{UInt64}),
            store.handle,
            Int64(old_id),
            Int64(new_id),
            out,
        ),
    )
end

"""
    count_parent_child_associations(store; filters...) -> Int

Number of edges matching the filter.
"""
function count_parent_child_associations(
    store::Store;
    parent_id::Union{Nothing,Integer}=nothing,
    parent_types::Union{Nothing,AbstractVector}=nothing,
    child_id::Union{Nothing,Integer}=nothing,
    child_types::Union{Nothing,AbstractVector}=nothing,
)
    out = Ref{Int64}(0)
    _check(
        ccall(
            (:infrastore_store_count_parent_child_associations, lib_path()),
            Int32,
            (Ptr{Cvoid}, Cstring, Ref{Int64}),
            store.handle,
            _parent_child_filter_json(parent_id, parent_types, child_id, child_types),
            out,
        ),
    )
    return Int(out[])
end

"""
    get_forecast_parameters(store; resolution=nothing, interval=nothing) -> ForecastParameters

Return the store's [`ForecastParameters`](@ref), optionally restricted to
forecasts with the given `resolution` and/or `interval` (`Period`s). Every field
is `nothing` when no forecast matches.
"""
function get_forecast_parameters(
    store::Store;
    resolution::Union{Nothing,Period}=nothing,
    interval::Union{Nothing,Period}=nothing,
)
    fres = _period_to_cstr(resolution)
    fivl = _period_to_cstr(interval)
    present = Ref{Bool}(false)
    horizon_out = Ref{Ptr{Cchar}}(C_NULL);
    interval_out = Ref{Ptr{Cchar}}(C_NULL)
    count = Ref{Int64}(-1);
    resolution_out = Ref{Ptr{Cchar}}(C_NULL);
    initial_out = Ref{Int64}(-1)
    code = ccall(
        (:infrastore_store_get_forecast_parameters, lib_path()),
        Int32,
        (
            Ptr{Cvoid},
            Cstring,
            Cstring,
            Ref{Bool},
            Ref{Ptr{Cchar}},
            Ref{Ptr{Cchar}},
            Ref{Int64},
            Ref{Ptr{Cchar}},
            Ref{Int64},
        ),
        store.handle,
        fres,
        fivl,
        present,
        horizon_out,
        interval_out,
        count,
        resolution_out,
        initial_out,
    )
    _check(code)
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
function check_static_consistency(store::Store; resolution::Union{Nothing,Period}=nothing)
    fres = _period_to_cstr(resolution)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:infrastore_store_check_static_consistency, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        fres,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:infrastore_store_check_static_consistency, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        fres,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    rows = JSON.parse(String(buf[1:Int(out_len[])]))
    return StaticGrid[
        StaticGrid(
            _from_unix_ms(Int64(r["initial_timestamp_ms"])),
            _iso_to_period(String(r["resolution"])),
            Int(r["length"]),
        ) for r in rows
    ]
end

"""
    get_resolutions(store; time_series_type=nothing) -> Vector{Period}

Return the distinct resolutions stored, in the core's stored (lexical-by-ISO)
order. When `time_series_type` (the Julia type) is given the result is
restricted to that type. This is a single catalog query in the core rather than
a scan of every association.
"""
function get_resolutions(store::Store; time_series_type::Union{Nothing,Type}=nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? _filter_type_code(time_series_type) : Int32(0)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:infrastore_store_get_resolutions, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:infrastore_store_get_resolutions, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    isos = JSON.parse(String(buf[1:Int(out_len[])]))
    return Period[_iso_to_period(String(s)) for s in isos]
end

"""
    get_intervals(store; time_series_type=nothing) -> Vector{Period}

Return the distinct forecast intervals stored (lexical-by-ISO order), the
interval analog of [`get_resolutions`](@ref). When `time_series_type` (the Julia
type) is given the result is restricted to that type; non-forecast types return
an empty vector.
"""
function get_intervals(store::Store; time_series_type::Union{Nothing,Type}=nothing)
    has_type = time_series_type !== nothing
    type_arg = has_type ? _filter_type_code(time_series_type) : Int32(0)
    out_len = Ref{UInt64}(0)
    code = ccall(
        (:infrastore_store_get_intervals, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:infrastore_store_get_intervals, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int32, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_type,
        type_arg,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    isos = JSON.parse(String(buf[1:Int(out_len[])]))
    return Period[_iso_to_period(String(s)) for s in isos]
end

"""
    read_only(store) -> Bool

Whether the store was opened read-only.
"""
function read_only(store::Store)
    out = Ref{Bool}(false)
    code = ccall(
        (:infrastore_store_read_only, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}),
        store.handle,
        out,
    )
    _check(code)
    return out[]
end

"""
    get_compression(store) -> CompressionSettings

Return the store's [`CompressionSettings`](@ref). For a store opened from disk
this reflects the policy it was created with; in-memory stores report `:none`.
"""
function get_compression(store::Store)
    kind = Ref{UInt8}(0);
    level = Ref{UInt8}(0);
    shuffle = Ref{Bool}(false)
    code = ccall(
        (:infrastore_store_get_compression, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{UInt8}, Ref{UInt8}, Ref{Bool}),
        store.handle,
        kind,
        level,
        shuffle,
    )
    _check(code)
    return CompressionSettings(kind[] == 0 ? :none : :deflate, Int(level[]), shuffle[])
end

"""
    get_path(store) -> Union{Nothing,String}

Return the filesystem path backing the store's NetCDF file, or `nothing` for an
in-memory store.
"""
function get_path(store::Store)
    has_path = Ref{Bool}(false)
    out_len = Ref{UInt64}(0)
    # Probe: a null buffer reports the required length without copying.
    code = ccall(
        (:infrastore_store_get_path, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_path,
        C_NULL,
        UInt64(0),
        out_len,
    )
    _check(code)
    has_path[] || return nothing
    # +1 leaves room for the trailing NUL `write_str_out` appends.
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    code = ccall(
        (:infrastore_store_get_path, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{Bool}, Ptr{UInt8}, UInt64, Ref{UInt64}),
        store.handle,
        has_path,
        buf,
        UInt64(length(buf)),
        out_len,
    )
    _check(code)
    return _take_buffer_string(buf, out_len[])
end

"""
    verify_integrity(store) -> Int

Recompute each stored array's content hash and return how many disagree with the
hash recorded alongside them. `0` means every array checked out.

Checks the NetCDF half of the store only — the SQLite catalog is not inspected,
so `0` does not mean the store as a whole is sound. A catalog that is corrupted,
truncated, or paired with the wrong `.nc` file still returns `0`, while every read
of the affected series throws. For catalog-side checks use
[`check_static_consistency`] (per-resolution grid agreement) and [`compact!`]
(which reports the unreachable arrays and feature sets a delete left behind — an
expected state, not corruption).
"""
function verify_integrity(store::Store)
    out = Ref{UInt64}(0)
    code = ccall(
        (:infrastore_store_verify, lib_path()),
        Int32,
        (Ptr{Cvoid}, Ref{UInt64}),
        store.handle,
        out,
    )
    _check(code)
    return Int(out[])
end

function compact!(store::Store)
    code = ccall(
        (:infrastore_store_compact, lib_path()), Int32, (Ptr{Cvoid},), store.handle
    )
    _check(code)
    return nothing
end

"""
    flush!(store)

Flush pending writes (NetCDF arrays + SQLite metadata) to disk. After this the
on-disk `<path>.nc` and `<path>.sqlite` artifacts can be copied for persistence.
"""
function flush!(store::Store)
    code = ccall((:infrastore_store_flush, lib_path()), Int32, (Ptr{Cvoid},), store.handle)
    _check(code)
    return nothing
end

"""
    persist!(store, path)

Persist the store to `path` (NetCDF) and `\$path.sqlite` (metadata), materializing
an in-memory store to disk. Existing target files are overwritten.
"""
function persist!(store::Store, path::AbstractString)
    code = ccall(
        (:infrastore_store_persist, lib_path()),
        Int32,
        (Ptr{Cvoid}, Cstring),
        store.handle,
        path,
    )
    _check(code)
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
    owner_id::Union{Nothing,Integer}=nothing,
    owner_category::Union{Nothing,OwnerCategory}=nothing,
)
    has_owner = owner_id !== nothing
    if has_owner && owner_category === nothing
        throw(ArgumentError("clear! with owner_id also requires owner_category"))
    end
    owner_arg = has_owner ? Int64(owner_id) : Int64(0)
    category_arg = has_owner ? _category_int(owner_category) : Int32(0)
    code = ccall(
        (:infrastore_store_clear, lib_path()),
        Int32,
        (Ptr{Cvoid}, Bool, Int64, Int32),
        store.handle,
        has_owner,
        owner_arg,
        category_arg,
    )
    _check(code)
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
    code = ccall(
        (:infrastore_store_replace_owner, lib_path()),
        Int32,
        (Ptr{Cvoid}, Int64, Int64, Int32, Ref{UInt64}),
        store.handle,
        Int64(old_owner_id),
        Int64(new_owner_id),
        _category_int(owner_category),
        out,
    )
    _check(code)
    return Int(out[])
end
