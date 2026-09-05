# ---- OpenAPI-row association serde -----------------------------------------
#
# Direct JSON serde of the two association catalogs, in the wire spelling
# SiennaSchemas defines (`TimeSeries/*.json`,
# `Core/Associations/SupplementalAttributeAssociation.json`). The Rust core
# (`infrastore_core::openapi`) owns the mapping between catalog rows and schema
# rows; this file is a thin wrapper over the four FFI exports it backs.
#
# The two exports use the owned-string convention (`_owned_str`) because their
# size scales with the catalog; the two imports return their row count through an
# out-param, matching `add_supplemental_attribute_associations!`.

"""
    export_time_series_associations_openapi(store; owner_id=nothing,
        owner_category=nothing, time_series_type=nothing, name=nothing,
        resolution=nothing, interval=nothing, features=nothing,
        component_field=nothing) -> String

Export `time_series_associations` matching the filter (the same filter
keywords as [`list_metadata`](@ref)) as a sorted OpenAPI-row JSON array.
Each row's `uri` and `data_hash` are the hex-encoded content hash the store
already has for that row — never a caller-supplied locator. With no filter
this exports the whole catalog, minus `PersistentTimeSeries` rows: the type is
an infrastore-local extension the wire contract has no schema for, so it is
omitted, and a filter naming it throws.
"""
function export_time_series_associations_openapi(
    store::Store;
    owner_id=nothing,
    owner_category=nothing,
    time_series_type=nothing,
    name=nothing,
    resolution=nothing,
    interval=nothing,
    features::Union{Nothing, AbstractDict}=nothing,
    component_field=nothing,
)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, _name_glob_arg, resolution_iso, interval_iso, features_json, component_field_arg, zoneless_arg) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features,
        component_field, nothing, nothing,
    )
    return _owned_str(
        (out_json, out_len) ->
            @ccall lib_path().infrastore_store_export_time_series_associations_openapi(
                store::Ptr{Cvoid},
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
                component_field_arg::Cstring,
                zoneless_arg::Int32,
                out_json::Ref{Ptr{Cchar}},
                out_len::Ref{UInt64},
            )::Int32
    )
end

"""
    import_time_series_associations_openapi!(store, json::AbstractString) -> Int

Bulk-ingest a JSON array of time-series association OpenAPI rows in one
all-or-nothing transaction, returning the number inserted. This is the import
half of the round trip whose export is
[`export_time_series_associations_openapi`](@ref).

Rows only: the document carries locators, never values, so every row must name
an array this store already holds, and each row keeps the `association_id` it
carries — an import that assigned fresh ids would leave every reference the
document records pointing at the wrong series. A `NonSequentialTimeSeries` row
also locates its time axis with `timestamps_uri`, since the values cannot imply
it: two irregular series with identical values on different axes share one
content-addressed array. A row whose array or axis is absent throws
`InvalidParameterError`.
"""
function import_time_series_associations_openapi!(store::Store, json::AbstractString)
    out = Ref{UInt64}(0)
    json_arg = String(json)
    _check(
        @ccall lib_path().infrastore_store_import_time_series_associations_openapi(
            store::Ptr{Cvoid}, json_arg::Cstring, out::Ref{UInt64}
        )::Int32
    )
    return Int(out[])
end

"""
    export_supplemental_attribute_associations_openapi(store) -> String

Export the whole `supplemental_attribute_associations` table as an OpenAPI-row
JSON array, sorted by `(component_id, attribute_id)`.
"""
function export_supplemental_attribute_associations_openapi(store::Store)
    return _owned_str(
        (out_json, out_len) ->
            @ccall lib_path().infrastore_store_export_supplemental_attribute_associations_openapi(
                store::Ptr{Cvoid}, out_json::Ref{Ptr{Cchar}}, out_len::Ref{UInt64}
            )::Int32
    )
end

"""
    import_supplemental_attribute_associations_openapi!(store, json::AbstractString) -> Int

Bulk-ingest a JSON array of supplemental-attribute association OpenAPI rows in
one all-or-nothing transaction, returning the number inserted. This is the
import half of the round trip whose export is
[`export_supplemental_attribute_associations_openapi`](@ref).
"""
function import_supplemental_attribute_associations_openapi!(
    store::Store, json::AbstractString
)
    out = Ref{UInt64}(0)
    json_arg = String(json)
    _check(
        @ccall lib_path().infrastore_store_import_supplemental_attribute_associations_openapi(
            store::Ptr{Cvoid}, json_arg::Cstring, out::Ref{UInt64}
        )::Int32
    )
    return Int(out[])
end
