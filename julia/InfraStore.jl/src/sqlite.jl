# ---- SQLite table export / import ------------------------------------------
#
# The normalized, partitioned table layout the CLI's `export -f sqlite` and
# `add --sqlite` exchange, driven through the same core code. The Parquet
# container of that layout stays CLI-only: it would link Arrow into the library.

"""
    export_sqlite(store, path; table_prefix="", time_range=nothing, filters...)
        -> Vector{NamedTuple}

Export the series the filter selects (the same filter keywords as
[`list_metadata`](@ref); none exports the whole store) as tables in the SQLite
database at `path` — one `<prefix><base>_values` / `<prefix><base>_series` pair
per `(time_series_type, value type, time_reference)` partition, the layout
`infrastore export -f sqlite` writes.

`time_range` clips each series as [`read_by_ids`](@ref)'s does. The database is
created if absent, and tables are only ever added: a name already taken, or an
empty series in the selection, throws before anything is written.
`table_prefix` scopes the names so several exports can share one database. The
values are streamed, a bounded batch at a time and each distinct array once, so
a store far larger than memory exports in a few hundred megabytes.

Returns one named tuple per partition written: `values_table`, `series_table`,
`time_series_type`, `value_type`, `time_reference`, `arrays`, `series`, `rows`.
"""
function export_sqlite(
    store::Store,
    path::AbstractString;
    table_prefix::AbstractString="",
    time_range::TimeRangeArg=nothing,
    kwargs...,
)
    has_range = time_range !== nothing
    tr_zoneless, tr_start, tr_end =
        has_range ? _time_range_args(time_range) : (false, Int64(0), Int64(0))
    path_arg = String(path)
    prefix_arg = String(table_prefix)
    json = _with_filter(; kwargs...) do filter
        return _owned_str(
            (out_json, out_len) -> @ccall libinfrastore.infrastore_store_export_sqlite(
                store::Ptr{Cvoid},
                filter::Ref{FilterRecord},
                has_range::Bool,
                tr_zoneless::Bool,
                tr_start::Int64,
                tr_end::Int64,
                path_arg::Cstring,
                prefix_arg::Cstring,
                out_json::Ref{Ptr{Cchar}},
                out_len::Ref{UInt64},
            )::Int32
        )
    end
    return [
        (
            values_table=String(r["values_table"]),
            series_table=String(r["series_table"]),
            time_series_type=String(r["time_series_type"]),
            value_type=String(r["value_type"]),
            time_reference=String(r["time_reference"]),
            arrays=Int(r["arrays"]),
            series=Int(r["series"]),
            rows=Int(r["rows"]),
        ) for r in JSON.parse(json)
    ]
end

"""
    import_sqlite!(store, path; table_prefix="", skip_checksum=false) -> Vector{Int64}

Add every series in the tables [`export_sqlite`](@ref) (or `infrastore export -f
sqlite`) wrote to the database at `path` under `table_prefix`, returning the new
catalog ids in the order read.

One all-or-nothing transaction across the whole database. Ids are always
assigned fresh; the ones the tables recorded are not reused. Each array is
checked against the `data_hash` its rows carry — pass `skip_checksum=true` after
editing values in place.
"""
function import_sqlite!(
    store::Store,
    path::AbstractString;
    table_prefix::AbstractString="",
    skip_checksum::Bool=false,
)
    out_len = Ref{UInt64}(0)
    out_ids = Ref{Ptr{Int64}}(C_NULL)
    path_arg = String(path)
    prefix_arg = String(table_prefix)
    _check(
        @ccall libinfrastore.infrastore_store_import_sqlite(
            store::Ptr{Cvoid},
            path_arg::Cstring,
            prefix_arg::Cstring,
            skip_checksum::Bool,
            out_len::Ref{UInt64},
            out_ids::Ref{Ptr{Int64}},
        )::Int32
    )
    n = Int(out_len[])
    added = Vector{Int64}(undef, n)
    if n > 0
        try
            copyto!(added, unsafe_wrap(Array, out_ids[], n; own=false))
        finally
            @ccall libinfrastore.infrastore_buffer_free_i64(
                out_ids[]::Ptr{Int64}, out_len[]::UInt64
            )::Cvoid
        end
    end
    return added
end
