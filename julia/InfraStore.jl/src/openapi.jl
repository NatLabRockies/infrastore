# ---- OpenAPI-row association serde -----------------------------------------
#
# Direct JSON serde of the two association catalogs, in the wire spelling
# SiennaSchemas defines (`TimeSeries/*.json`,
# `Core/Associations/SupplementalAttributeAssociation.json`). The Rust core
# (`infrastore_core::openapi`) owns the mapping between catalog rows and schema
# rows; this file is a thin wrapper over the four FFI exports it backs.
#
# Two exports and the reconcile report use the owned-string convention
# (`_owned_str`) because their size scales with the catalog or because the
# report is itself structured JSON; import returns its row count through an
# out-param, matching `add_supplemental_attribute_associations!`.

# `:strict` -> 0, `:update_descriptive` -> 1 (must match
# `infrastore_core::ReconcilePolicy` / `crates/infrastore-ffi/src/lib.rs`).
function _reconcile_policy_code(policy::Symbol)
    policy === :strict && return Int32(0)
    policy === :update_descriptive && return Int32(1)
    return throw(
        InvalidParameterError(
            "unknown reconcile policy $(repr(policy)); expected :strict or :update_descriptive"
        ),
    )
end

function _decode_reconcile_report(r::AbstractDict)
    return ReconcileReport(
        Int(r["matched"]),
        Int(r["updated"]),
        Int(r["missing_in_store"]),
        Int(r["unmatched_in_store"]),
        String[String(c) for c in r["conflicts"]],
    )
end

"""
    export_time_series_associations_openapi(store; owner_id=nothing,
        owner_category=nothing, time_series_type=nothing, name=nothing,
        resolution=nothing, interval=nothing, features=Dict(),
        component_field=nothing) -> String

Export `time_series_associations` matching the filter (the same filter
keywords as [`list_time_series`](@ref)) as a sorted OpenAPI-row JSON array.
Each row's `uri` and `data_hash` are the hex-encoded content hash the store
already has for that row — never a caller-supplied locator. With no filter
this exports the whole catalog.
"""
function export_time_series_associations_openapi(
    store::Store;
    owner_id=nothing,
    owner_category=nothing,
    time_series_type=nothing,
    name=nothing,
    resolution=nothing,
    interval=nothing,
    features=Dict{String, Any}(),
    component_field=nothing,
)
    (has_owner, owner_arg, has_category, category_arg, has_type, type_arg, name_arg, resolution_iso, interval_iso, features_json, component_field_arg) = _filter_args(
        owner_id, owner_category, time_series_type, name, resolution, interval, features,
        component_field,
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
                out_json::Ref{Ptr{Cchar}},
                out_len::Ref{UInt64},
            )::Int32
    )
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

"""
    reconcile_time_series_associations_openapi!(store, json::AbstractString;
        policy::Symbol=:strict) -> ReconcileReport

Reconcile a JSON array of time-series association OpenAPI rows against the
store's catalog: match by identity, apply `policy` (`:strict` or
`:update_descriptive`) to any descriptive drift, and throw
`InfraStore.ReconcileConflictError` (naming every offending row) for anything
neither policy can resolve. Under `:strict` any drift — descriptive or
geometric — is an error; under `:update_descriptive` descriptive drift
(`units`, `quantity_kind`, `unit_system`, `component_field`,
`application_data`) is rewritten from the JSON, while geometry drift is still
an error. A row's `uri` and `data_hash` are informational and never checked —
a document from another store may carry foreign values for either. Runs in
one transaction when it writes.
"""
function reconcile_time_series_associations_openapi!(
    store::Store, json::AbstractString; policy::Symbol=:strict
)
    policy_code = _reconcile_policy_code(policy)
    json_arg = String(json)
    report_json = _owned_str(
        (out_json, out_len) ->
            @ccall lib_path().infrastore_store_reconcile_time_series_associations_openapi(
                store::Ptr{Cvoid},
                json_arg::Cstring,
                policy_code::Int32,
                out_json::Ref{Ptr{Cchar}},
                out_len::Ref{UInt64},
            )::Int32
    )
    return _decode_reconcile_report(JSON.parse(report_json))
end
