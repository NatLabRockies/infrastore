# ---- libinfrastore_ffi resolution ---------------------------------
#
# Resolution order:
#   1. `INFRASTORE_LIB` environment variable (development override).
#   2. `InfraStore_jll` (the BinaryBuilder/Yggdrasil binary) when installed.
# The JLL is looked up without a hard dependency so this package still loads and
# works via the env var before the JLL is published to the registry.

const _LIB_REF = Ref{String}("")

function _jll_library_path()
    pkgid = Base.identify_package("InfraStore_jll")
    pkgid === nothing && return ""
    mod = try
        Base.require(pkgid)
    catch
        return ""
    end
    return if isdefined(mod, :libinfrastore_ffi)
        String(getproperty(mod, :libinfrastore_ffi))
    else
        ""
    end
end

"""
Path to the `libinfrastore_ffi` cdylib. Override with the
`INFRASTORE_LIB` environment variable (development builds); otherwise the
`InfraStore_jll` binary is used.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "INFRASTORE_LIB", "")
    if isempty(p)
        p = _jll_library_path()
    end
    isempty(p) && error(
        "Could not locate libinfrastore_ffi. Set the INFRASTORE_LIB " *
        "environment variable to a built cdylib, or install InfraStore_jll.",
    )
    _LIB_REF[] = p
    return p
end

# ---- Status codes (must match crates/infrastore-ffi/src/lib.rs) ----

const INFRASTORE_OK = Int32(0)
const INFRASTORE_ERR_NULL_POINTER = Int32(1)
const INFRASTORE_ERR_INVALID_UTF8 = Int32(2)
const INFRASTORE_ERR_INVALID_PARAMETER = Int32(3)
const INFRASTORE_ERR_NOT_FOUND = Int32(4)
const INFRASTORE_ERR_DUPLICATE = Int32(5)
const INFRASTORE_ERR_INTEGRITY = Int32(6)
const INFRASTORE_ERR_READ_ONLY = Int32(7)
const INFRASTORE_ERR_IO = Int32(8)
const INFRASTORE_ERR_INCOMPATIBLE_FORMAT = Int32(9)
const INFRASTORE_ERR_DUPLICATE_ASSOCIATION = Int32(10)
const INFRASTORE_ERR_INTERNAL = Int32(99)

# ---- Owner category --------------------------------------------------------

@enum OwnerCategory begin
    Component = 0
    SupplementalAttribute = 1
end

# ---- Errors ---------------------------------------------------------------

abstract type TimeSeriesException <: Exception end

struct NotFoundError <: TimeSeriesException
    msg::String;
end

struct DuplicateTimeSeriesError <: TimeSeriesException
    msg::String;
end

struct DuplicateAssociationError <: TimeSeriesException
    msg::String;
end

struct InvalidParameterError <: TimeSeriesException
    msg::String;
end

struct IntegrityError <: TimeSeriesException
    msg::String;
end

struct ReadOnlyStoreError <: TimeSeriesException
    msg::String;
end

struct IncompatibleFormatError <: TimeSeriesException
    msg::String;
end

struct IOError <: TimeSeriesException
    msg::String;
end

struct GenericError <: TimeSeriesException
    msg::String;
    code::Int32;
end

function Base.showerror(io::IO, e::TimeSeriesException)
    return print(io, "InfraStore.", typeof(e).name.name, ": ", e.msg)
end

function _last_error_message()
    needed = Ref{UInt64}(0)
    ccall(
        (:infrastore_last_error_message, lib_path()),
        Int32,
        (Ptr{UInt8}, UInt64, Ptr{UInt64}),
        C_NULL,
        UInt64(0),
        needed,
    )
    n = Int(needed[])
    n == 0 && return ""
    buf = Vector{UInt8}(undef, n + 1)
    ccall(
        (:infrastore_last_error_message, lib_path()),
        Int32,
        (Ptr{UInt8}, UInt64, Ptr{UInt64}),
        buf,
        UInt64(n + 1),
        C_NULL,
    )
    return String(buf[1:n])
end

function _check(code::Int32)
    code == INFRASTORE_OK && return nothing
    msg = _last_error_message()
    if code == INFRASTORE_ERR_NOT_FOUND
        throw(NotFoundError(msg))
    elseif code == INFRASTORE_ERR_DUPLICATE
        throw(DuplicateTimeSeriesError(msg))
    elseif code == INFRASTORE_ERR_DUPLICATE_ASSOCIATION
        throw(DuplicateAssociationError(msg))
    elseif code == INFRASTORE_ERR_INVALID_PARAMETER ||
        code == INFRASTORE_ERR_INVALID_UTF8 ||
        code == INFRASTORE_ERR_NULL_POINTER
        throw(InvalidParameterError(msg))
    elseif code == INFRASTORE_ERR_INTEGRITY
        throw(IntegrityError(msg))
    elseif code == INFRASTORE_ERR_READ_ONLY
        throw(ReadOnlyStoreError(msg))
    elseif code == INFRASTORE_ERR_INCOMPATIBLE_FORMAT
        throw(IncompatibleFormatError(msg))
    elseif code == INFRASTORE_ERR_IO
        throw(IOError(msg))
    else
        throw(GenericError(msg, code))
    end
end

# ---- Tracing ---------------------------------------------------------------

"""
    init_logging(level::AbstractString = "")

Initialize the Rust tracing subscriber.

`level` is a [`tracing_subscriber::EnvFilter`] directive string such as
`"debug"`, `"infrastore_core=debug"`, or
`"warn,infrastore_core=trace"`. Pass an empty string (the default)
to read the `RUST_LOG` environment variable; if that variable is also unset,
no output is produced.

The subscriber is initialized at most once per process — subsequent calls are
no-ops. `InfraStore.__init__` reads `RUST_LOG` on module load, so setting
`ENV["RUST_LOG"]` before `using InfraStore` is sufficient for the common
case.

Returns the FFI status code (`INFRASTORE_OK = 0`, `INFRASTORE_ERR_INVALID_PARAMETER = 3` for
an invalid directive string).
"""
function init_logging(level::AbstractString="")
    filter_ptr = isempty(level) ? C_NULL : level
    ret = ccall((:infrastore_store_init_logging, lib_path()), Int32, (Cstring,), filter_ptr)
    if ret != 0
        @warn "InfraStore.init_logging: infrastore_store_init_logging returned error code $ret"
    end
    return ret
end

# Read RUST_LOG at module-load time so that `using InfraStore` with RUST_LOG
# set in the environment automatically enables tracing without extra user code.
function __init__()
    rust_log = get(ENV, "RUST_LOG", "")
    if !isempty(rust_log)
        try
            init_logging(rust_log)
        catch e
            @warn "InfraStore.__init__: failed to initialize tracing" exception=e
        end
    end
end
