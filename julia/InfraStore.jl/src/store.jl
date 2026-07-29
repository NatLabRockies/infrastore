# ---- Keys -----------------------------------------------------------------

mutable struct TimeSeriesKey
    handle::Ptr{Cvoid}
    function TimeSeriesKey(handle::Ptr{Cvoid})
        k = new(handle)
        finalizer(_finalize_key, k)
        return k
    end
end

function _finalize_key(k::TimeSeriesKey)
    if k.handle != C_NULL
        @ccall lib_path().infrastore_key_free(k.handle::Ptr{Cvoid})::Cvoid
        k.handle = C_NULL
    end
end

# ---- Store ----------------------------------------------------------------

mutable struct Store
    handle::Ptr{Cvoid}
    function Store(handle::Ptr{Cvoid})
        s = new(handle)
        finalizer(close!, s)
        return s
    end
end

function close!(s::Store)
    if s.handle != C_NULL
        @ccall lib_path().infrastore_store_free(s.handle::Ptr{Cvoid})::Cvoid
        s.handle = C_NULL
    end
end

"""
    Store(; in_memory=true, path=nothing,
            compression=:deflate, compression_level=3, shuffle=true)

Construct a new store. Pass `path` (and `in_memory=false`) to persist to an
HDF5 file on disk.

`compression` selects the on-disk filter for HDF5 data variables:
`:deflate` (default) applies DEFLATE at `compression_level` (0–9) with optional
byte `shuffle`; `:none` disables compression. The setting is ignored for
in-memory stores and is persisted so later appends reuse it.
"""
function Store(;
    in_memory::Bool=true,
    path::Union{Nothing, AbstractString}=nothing,
    compression::Union{Symbol, AbstractString}=:deflate,
    compression_level::Integer=3,
    shuffle::Bool=true,
)
    kind = Symbol(compression)
    compression_kind = if kind === :none
        UInt8(0)
    elseif kind === :deflate
        UInt8(1)
    else
        throw(
            ArgumentError(
                "unknown compression $(repr(compression)), expected :deflate or :none"
            ),
        )
    end
    out = Ref{Ptr{Cvoid}}(C_NULL)
    cpath = path === nothing ? C_NULL : String(path)
    code = @ccall lib_path().infrastore_store_create_with_compression(
        cpath::Cstring,
        in_memory::Bool,
        compression_kind::UInt8,
        UInt8(compression_level)::UInt8,
        shuffle::Bool,
        out::Ref{Ptr{Cvoid}},
    )::Int32
    _check(code)
    return Store(out[])
end

"""
    open_store(path; read_only=false)

Open an existing on-disk store.
"""
function open_store(path::AbstractString; read_only::Bool=false)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_open(
        path::Cstring, read_only::Bool, out::Ref{Ptr{Cvoid}}
    )::Int32
    _check(code)
    return Store(out[])
end

"""
    Store(f::Function; kwargs...)
    open_store(f::Function, path; read_only=false)

Do-block forms: construct (or open) a store, run `f(store)`, and guarantee
`close!` on exit — including on throw.

```julia
Store(in_memory=true) do store
    add_time_series!(store, 1, "Generator", Component, ts)
end
```
"""
function Store(f::Function; kwargs...)
    s = Store(; kwargs...)
    try
        return f(s)
    finally
        close!(s)
    end
end

function open_store(f::Function, path::AbstractString; read_only::Bool=false)
    s = open_store(path; read_only=read_only)
    try
        return f(s)
    finally
        close!(s)
    end
end

# ---- Transactions ----------------------------------------------------------

"""
    begin_transaction!(store)

Begin a transaction spanning subsequent operations, so that adds, removals, and
transforms either all take effect or none do. Calls nest; only the outermost
commit makes anything durable.

Prefer the do-block [`transaction`](@ref), which cannot leak an open
transaction. Removals are reversible only inside one.

Holds the SQLite write lock until the outermost commit or rollback, so another
writer on the same artifact will block and then fail on its busy timeout. Scope
a transaction to the span that actually needs atomicity.
"""
function begin_transaction!(store::Store)
    _check(
        @ccall lib_path().infrastore_store_begin_transaction(
            store.handle::Ptr{Cvoid}
        )::Int32
    )
    return nothing
end

"""
    commit_transaction!(store)

Commit the innermost open transaction. Committing the outermost one makes the
whole span durable. Errors if no transaction is open.
"""
function commit_transaction!(store::Store)
    _check(
        @ccall lib_path().infrastore_store_commit_transaction(
            store.handle::Ptr{Cvoid}
        )::Int32
    )
    return nothing
end

"""
    rollback_transaction!(store)

Roll back the innermost open transaction, undoing every operation it covered.
Errors if no transaction is open.
"""
function rollback_transaction!(store::Store)
    _check(
        @ccall lib_path().infrastore_store_rollback_transaction(
            store.handle::Ptr{Cvoid}
        )::Int32
    )
    return nothing
end

"""
    in_transaction(store) -> Bool

Whether a transaction is currently open on `store`.
"""
function in_transaction(store::Store)
    out = Ref{Bool}(false)
    _check(
        @ccall lib_path().infrastore_store_in_transaction(
            store.handle::Ptr{Cvoid}, out::Ref{Bool}
        )::Int32
    )
    return out[]
end

"""
    transaction(f, store)

Run `f()` inside a transaction: commit if it returns, roll back if it throws.
Returns `f`'s value.

```julia
transaction(store) do
    add_time_series!(store, 1, "Generator", Component, ts)
    remove_time_series!(store, old_key)
end
```

Both operations take effect or neither does — including the removal, which
outside a transaction is irreversible.

A failure in the rollback itself is logged rather than thrown, so the error that
caused the unwind is the one the caller sees.
"""
function transaction(f::Function, store::Store)
    begin_transaction!(store)
    result = try
        f()
    catch
        try
            rollback_transaction!(store)
        catch rollback_err
            @error "InfraStore transaction rollback failed; the store may retain " *
                "partial work from the transaction" exception = rollback_err
        end
        rethrow()
    end
    commit_transaction!(store)
    return result
end
