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

Construct a new store. Pass `path` (and `in_memory=false`) to persist to a
NetCDF file on disk.

`compression` selects the on-disk filter for NetCDF data variables:
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
