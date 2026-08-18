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
        @ccall lib_path().infrastore_key_free(k::Ptr{Cvoid})::Cvoid
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
        @ccall lib_path().infrastore_store_free(s::Ptr{Cvoid})::Cvoid
        s.handle = C_NULL
    end
end

# Root the wrapper for the duration of a ccall. Passing the object itself (with
# a declared C type of `Ptr{Cvoid}`) routes through `unsafe_convert`, and the
# ccall machinery keeps the object — and therefore its Rust handle — alive until
# the foreign call returns. Passing `x.handle` would extract the bare pointer
# without rooting `x`, so a GC after the object's last syntactic use could run
# its finalizer (freeing the handle) while the call is still using it.
Base.unsafe_convert(::Type{Ptr{Cvoid}}, k::TimeSeriesKey) = k.handle
Base.unsafe_convert(::Type{Ptr{Cvoid}}, s::Store) = s.handle

"""
    _catalog_code(catalog, in_memory)

Translate a `catalog` keyword into the C ABI's `catalog_mode` byte. `nothing`
means "match the backend", which is what the constructors did before the keyword
existed.
"""
function _catalog_code(catalog::Union{Nothing, Symbol, AbstractString}, in_memory::Bool)
    catalog === nothing && return in_memory ? UInt8(1) : UInt8(0)
    mode = Symbol(catalog)
    mode === :attached && return UInt8(0)
    mode === :memory && return UInt8(1)
    return throw(
        ArgumentError("unknown catalog $(repr(catalog)), expected :attached or :memory")
    )
end

"""
    Store(; in_memory=nothing, path=nothing,
            compression=:deflate, compression_level=3, shuffle=true,
            catalog=nothing, overwrite=false)

Construct a new store. Pass `path` to persist to an HDF5 file on disk; the
store is in-memory when no path is given.

`in_memory` defaults to "whichever the path implies" and rarely needs setting.
Passing `path` together with `in_memory=true` is a contradiction and throws:
the FFI ignores the path for an in-memory store, so it used to be accepted
silently and everything written was discarded at `close!`, leaving no file
behind.

Throws [`StoreExistsError`](@ref) if `path` (or `\$path.sqlite`) already holds a
store: creating there would discard its arrays while keeping its catalog,
leaving a store that reopens cleanly with every array missing. Pass
`overwrite=true` to discard the existing artifact on purpose, or use
[`open_store`](@ref) to keep it.

`compression` selects the on-disk filter for HDF5 data variables:
`:deflate` (default) applies DEFLATE at `compression_level` (0–9) with optional
byte `shuffle`; `:none` disables compression. The setting is ignored for
in-memory stores and is persisted so later appends reuse it.

`catalog` places the SQLite catalog. `:attached` writes it to `\$path.sqlite`,
where every commit is durable; `:memory` holds it in RAM so it reaches disk only
through [`persist!`](@ref) — nothing survives a crash, which suits building a
store in a scratch directory beside volatile state. Arrays stream to the HDF5
file either way. The default matches the backend: `:memory` when `in_memory` is
true, else `:attached`.

!!! warning "Not thread-safe"
    A `Store` (and any reader built from it) must not be used from two tasks or
    threads concurrently: the Rust core mutates the handle without
    synchronization, so concurrent calls are undefined behavior, not just a
    race on results. Confine a store to one task, or guard every call with your
    own lock. Per-call locking inside this package would not make interleaved
    logical operations (e.g. two tasks sharing one transaction) correct, so it
    deliberately provides none.
"""
function Store(;
    in_memory::Union{Nothing, Bool}=nothing,
    path::Union{Nothing, AbstractString}=nothing,
    compression::Union{Symbol, AbstractString}=:deflate,
    compression_level::Integer=3,
    shuffle::Bool=true,
    catalog::Union{Nothing, Symbol, AbstractString}=nothing,
    overwrite::Bool=false,
)
    # A path means a file-backed store unless the caller says otherwise, and a
    # path with `in_memory=true` is a contradiction rather than a preference:
    # `infrastore_store_create_with_catalog` ignores the path in that case, so
    # accepting it silently produced an in-memory store whose contents vanished
    # at `close!` with no file ever created. The `overwrite` branch below has
    # always rejected its own version of this; this is the same rule for the
    # ordinary one.
    if in_memory === true && path !== nothing
        throw(
            ArgumentError(
                "in_memory=true ignores `path`; drop one of the two (omit `in_memory` to " *
                "let the path decide)",
            ),
        )
    end
    in_memory = in_memory === nothing ? path === nothing : in_memory

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
    catalog_mode = _catalog_code(catalog, in_memory)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    if overwrite
        in_memory && throw(
            ArgumentError(
                "overwrite=true is meaningless for an in-memory store: there is no artifact to replace"
            ),
        )
        path === nothing &&
            throw(ArgumentError("path is required when in_memory=false"))
        code = @ccall lib_path().infrastore_store_create_replacing(
            String(path)::Cstring,
            compression_kind::UInt8,
            UInt8(compression_level)::UInt8,
            shuffle::Bool,
            catalog_mode::UInt8,
            out::Ref{Ptr{Cvoid}},
        )::Int32
    else
        cpath = path === nothing ? C_NULL : String(path)
        code = @ccall lib_path().infrastore_store_create_with_catalog(
            cpath::Cstring,
            in_memory::Bool,
            compression_kind::UInt8,
            UInt8(compression_level)::UInt8,
            shuffle::Bool,
            catalog_mode::UInt8,
            out::Ref{Ptr{Cvoid}},
        )::Int32
    end
    _check(code)
    return Store(out[])
end

"""
    open_store(path; read_only=false, catalog=:attached)

Open an existing on-disk store.

`catalog=:memory` reads `\$path.sqlite` into RAM and leaves the file alone; later
mutations reach disk only through [`persist!`](@ref) or
[`persist_catalog!`](@ref). The HDF5 half is still opened in place, so with
`read_only=false` mutations land in `path` itself — use [`open_copy`](@ref) to
leave the original untouched until an explicit save.
"""
function open_store(
    path::AbstractString;
    read_only::Bool=false,
    catalog::Union{Symbol, AbstractString}=:attached,
)
    catalog_mode = _catalog_code(catalog, false)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_open_with_catalog(
        path::Cstring, read_only::Bool, catalog_mode::UInt8, out::Ref{Ptr{Cvoid}}
    )::Int32
    _check(code)
    return Store(out[])
end

"""
    open_copy(src, dest; catalog=:attached)

Copy the store at `src` to `dest` and open the copy read-write.

Both halves are copied, so `dest` is a complete, independent store, and `src` is
never opened for writing.

This is the safe way to load a store you care about and then change it.
[`open_store`](@ref) defaults to read-write, and every mutation then lands in
that file directly — HDF5 has no journal and no repair tool, so an interrupted
write there is unrecoverable. Working on a copy and calling `persist!(store,
src)` leaves the original intact until one atomic rename replaces it.

Throws [`StoreExistsError`](@ref) if `dest` already holds a store.
"""
function open_copy(
    src::AbstractString,
    dest::AbstractString;
    catalog::Union{Symbol, AbstractString}=:attached,
)
    catalog_mode = _catalog_code(catalog, false)
    out = Ref{Ptr{Cvoid}}(C_NULL)
    code = @ccall lib_path().infrastore_store_open_copy(
        src::Cstring, dest::Cstring, catalog_mode::UInt8, out::Ref{Ptr{Cvoid}}
    )::Int32
    _check(code)
    return Store(out[])
end

"""
    open_copy(f::Function, src, dest; catalog=:attached)

Do-block form: copy and open, run `f(store)`, and guarantee `close!` on exit.
"""
function open_copy(
    f::Function,
    src::AbstractString,
    dest::AbstractString;
    catalog::Union{Symbol, AbstractString}=:attached,
)
    s = open_copy(src, dest; catalog=catalog)
    try
        return f(s)
    finally
        close!(s)
    end
end

"""
    catalog_mode(store)

Where `store`'s catalog lives: `:attached` or `:memory`. See [`Store`](@ref).
"""
function catalog_mode(s::Store)
    out = Ref{UInt8}(0)
    code = @ccall lib_path().infrastore_store_catalog_mode(
        s::Ptr{Cvoid}, out::Ref{UInt8}
    )::Int32
    _check(code)
    return out[] == 0 ? :attached : :memory
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

function open_store(
    f::Function,
    path::AbstractString;
    read_only::Bool=false,
    catalog::Union{Symbol, AbstractString}=:attached,
)
    s = open_store(path; read_only=read_only, catalog=catalog)
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
            store::Ptr{Cvoid}
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
            store::Ptr{Cvoid}
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
            store::Ptr{Cvoid}
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
            store::Ptr{Cvoid}, out::Ref{Bool}
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
