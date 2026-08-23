# ---- libinfrastore_ffi resolution ---------------------------------
#
# Resolution order:
#   1. `INFRASTORE_LIB` environment variable (development override). This is
#      what lets a local `cargo build` shadow the released binary; CI depends
#      on it.
#   2. The `libinfrastore_ffi` artifact (see Artifacts.toml): a per-platform
#      tarball hosted on this repository's GitHub Releases, downloaded by Pkg
#      at install time.
# The InfraStore_jll route was dropped while the Yggdrasil recipe waits for
# review; "Switching back to the JLL" in docs/src/releasing.md is the road
# back.

const _LIB_REF = Ref{String}("")

function _library_filename()
    return if Sys.iswindows()
        "infrastore_ffi.dll"
    elseif Sys.isapple()
        "libinfrastore_ffi.dylib"
    else
        "libinfrastore_ffi.so"
    end
end

"""
Path to the `libinfrastore_ffi` cdylib. Override with the
`INFRASTORE_LIB` environment variable (development builds); otherwise the
platform's `libinfrastore_ffi` artifact is used.
"""
function lib_path()
    if !isempty(_LIB_REF[])
        return _LIB_REF[]
    end
    p = get(ENV, "INFRASTORE_LIB", "")
    if isempty(p)
        p = joinpath(artifact"libinfrastore_ffi", "lib", _library_filename())
    end
    isfile(p) || error(
        "Could not locate libinfrastore_ffi at $(p). Set the INFRASTORE_LIB " *
        "environment variable to a built cdylib, or reinstall the package to " *
        "fetch the artifact.",
    )
    _LIB_REF[] = p
    return p
end

# ---- Runtime symbol resolution ----------------------------------------------
#
# `_filter_list_json` (catalog.jl) shares one call site across several exports,
# so its FFI symbol is a runtime `Symbol` argument rather than a literal
# `@ccall` target, and must be resolved with `dlsym`. Doing that on every call
# via `dlsym(dlopen(lib_path()), fname)` reopens the library each time — bumping
# its reference count with no matching `dlclose`, and walking the dynamic symbol
# table from scratch — about 190x the cost of a cached lookup, measured over a
# `list_time_series` loop. `_cached_dlsym` opens the library at most once and
# memoizes each symbol thereafter; the lock covers both the lazy `dlopen` and
# the dict, since Julia may call into this from multiple tasks.
const _SYMBOL_CACHE_LOCK = ReentrantLock()
const _SYMBOL_CACHE = Dict{Symbol, Ptr{Cvoid}}()
const _LIB_HANDLE = Ref{Ptr{Cvoid}}(C_NULL)

function _cached_dlsym(fname::Symbol)
    return lock(_SYMBOL_CACHE_LOCK) do
        get!(_SYMBOL_CACHE, fname) do
            if _LIB_HANDLE[] == C_NULL
                _LIB_HANDLE[] = dlopen(lib_path())
            end
            return dlsym(_LIB_HANDLE[], fname)
        end
    end
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
const INFRASTORE_ERR_STORE_EXISTS = Int32(11)
const INFRASTORE_ERR_MISMATCHED_ARTIFACT = Int32(12)
const INFRASTORE_ERR_INTERNAL = Int32(99)

# ---- Owner category --------------------------------------------------------

@enum OwnerCategory begin
    Component = 0
    SupplementalAttribute = 1
end

# ---- Unit system -----------------------------------------------------------

"""
    UnitSystem

Which basis a series' values are expressed in: `NaturalUnits` (the units named
by `units`) or `ComponentBase` (per-unit against the owning component's own
base). A series that declares neither leaves `unit_system === nothing`, which
means *unspecified* and is deliberately not the same as `NaturalUnits`.

The store records the declaration only — it holds no base value and rescales
nothing, so converting `ComponentBase` values back to natural units is the
caller's job.

Unlike [`OwnerCategory`], the integer values here are a Julia-side convenience
with no ABI meaning: the C boundary carries the `"natural_units"` /
`"component_base"` spellings, so a basis added later crosses as a name rather
than as an unrecognized code.
"""
@enum UnitSystem begin
    NaturalUnits = 0
    ComponentBase = 1
end

_unit_system_str(::Nothing) = nothing
_unit_system_str(u::UnitSystem) = u === NaturalUnits ? "natural_units" : "component_base"

_unit_system(::Nothing) = nothing
_unit_system(u::UnitSystem) = u
function _unit_system(s::AbstractString)
    s == "natural_units" && return NaturalUnits
    s == "component_base" && return ComponentBase
    return throw(
        ArgumentError(
            "invalid unit_system $(repr(String(s))); expected \"natural_units\" or \"component_base\""
        ),
    )
end

# ---- Time reference --------------------------------------------------------

"""
    TimeReference

How a time series' timestamps were *spelled*. The store records instants; this
records what they were written **as**, so a series comes back the way it went in
instead of being relabelled UTC at the boundary.

Four subtypes, and most rules split on a binary rather than on all four:
[`UTCReference`](@ref), [`FixedOffsetReference`](@ref) and [`ZoneReference`](@ref)
are *zoned* — they name an instant — while [`ZonelessReference`](@ref) does not.
A series that declares nothing leaves `time_reference === nothing`, which means
*unspecified*; for query bounds that groups with the zoned spellings, but it is
not a claim the timestamps were written as UTC.

An abstract type with subtypes rather than an `@enum` like [`UnitSystem`](@ref),
because two of the four carry a payload.

# A spelling, not a grid

A reference does not change how the grid is *stepped*: `resolution` and
`interval` are durations, so an hourly series has hourly **instants** whatever
its reference says. Rendering an hourly `America/Denver` series across the
November fall-back gives `01:00-06:00`, `01:00-07:00`, `02:00-07:00` — two
identical wall clocks, two distinct instants, correctly ordered.

A *local-clock* grid — hourly by the clock, so a 23-hour day in March and a
25-hour one in November — is a different thing and is inexpressible in
[`SingleTimeSeries`](@ref) and the dense forecasts, whose grid is a fixed count
of milliseconds. Use [`NonSequentialTimeSeries`](@ref), which carries an explicit
instant per value.

For the same reason a calendar `Month`/`Year` period steps on the **UTC**
calendar, not the reference's — TimeZones.jl steps the local clock, so the two
disagree by an hour at each DST transition. The store warns when a calendar
period meets a zoned reference.

# Inference

You do not normally construct these. A bare `Dates.DateTime` is a wall clock and
records [`ZonelessReference`](@ref); a `TimeZones.ZonedDateTime` records the
spelling its zone names — `UTC` as [`UTCReference`](@ref), any other
`FixedTimeZone` as [`FixedOffsetReference`](@ref), and a `VariableTimeZone` as
[`ZoneReference`](@ref) carrying its IANA name. Pass `time_reference=` to a
constructor to override.

# Reading

Reads keep returning a `DateTime` holding the **instant**, unchanged, with the
reference beside it on the series and on [`TimeSeriesMetadata`](@ref). Returning
a `ZonedDateTime` instead would make the return type depend on whether the caller
had loaded TimeZones, and `zdt == dt` throws in Julia — so the two pieces stay
separate, and `using TimeZones` adds a fusing helper
([`zoned_timestamp`](@ref)) that puts them back together losslessly.
"""
abstract type TimeReference end

"""
    UTCReference()

An instant, written as UTC. Distinct from `ZoneReference("UTC")`: the two render
identically forever, and the difference is only in what the catalog reports back
— which is the point of recording a spelling at all.
"""
struct UTCReference <: TimeReference end

"""
    FixedOffsetReference(minutes)

An instant, written at a fixed offset from UTC, in **minutes east**.

One offset applies to the whole series, transitions included — right for data
genuinely written that way, and wrong for a local series that crosses a DST
boundary, which wants [`ZoneReference`](@ref) instead.
"""
struct FixedOffsetReference <: TimeReference
    minutes::Int
    function FixedOffsetReference(minutes::Integer)
        # Compared as the integer that came in, before any conversion. `abs`
        # cannot represent `-typemin(Int)`, so `abs(Int(typemin(Int)))` is
        # `typemin(Int)` again -- negative, and therefore *below* the bound,
        # which let the least plausible offset there is through. Bounding the
        # original value also turns an oversized `BigInt` into this error rather
        # than an `InexactError` from the conversion.
        -24 * 60 < minutes < 24 * 60 || throw(
            InvalidParameterError(
                "time reference offset $minutes minutes is not a real UTC offset; " *
                "it must be strictly within a day of UTC",
            ),
        )
        return new(Int(minutes))
    end
end

"""
    ZoneReference(name)

An instant, written in a named IANA zone (`"America/Denver"`). The name is held
opaquely: the store records it and never resolves it.

Rendering a stored instant in a named zone depends on the tz database version, so
a retroactive rule change moves the displayed local time. The store records the
instant; the label is a rendering hint.
"""
struct ZoneReference <: TimeReference
    name::String
    function ZoneReference(name::AbstractString)
        s = String(name)
        # Shape only, mirroring `TimeReference::validate` in the Rust core.
        # Existence is deliberately *not* checked here: `add_time_series!` audits
        # the name against TimeZones' database and warns, because gating would
        # refuse legitimate data whenever IANA moves ahead of whichever copy
        # happened to be asked.
        isempty(s) && throw(InvalidParameterError("time reference zone name is empty"))
        ncodeunits(s) <= 64 || throw(
            InvalidParameterError(
                "time reference zone name is $(ncodeunits(s)) bytes, over the 64-byte " *
                "limit; no IANA name is anywhere near that long",
            ),
        )
        # The load-bearing check: one catalog column holds all four spellings, so
        # a zone name must not read as either literal or as an offset.
        (s == "utc" || s == "zoneless" || _offset_minutes(s) !== nothing) && throw(
            InvalidParameterError(
                "$(repr(s)) is the spelling of a non-zone time reference, so it cannot " *
                "also be a zone name (the IANA zone is spelled \"UTC\")",
            ),
        )
        occursin(r"^[A-Za-z][A-Za-z0-9_+-]*(/[A-Za-z][A-Za-z0-9_+-]*){0,2}$", s) || throw(
            InvalidParameterError(
                "time reference zone name $(repr(s)) is not shaped like an IANA name " *
                "(slash-separated components of letters, digits, '_', '+' or '-', each " *
                "starting with a letter), e.g. \"America/Denver\"",
            ),
        )
        return new(s)
    end
end

"""
    ZonelessReference()

A wall clock. Names no instant; the store holds it as if UTC and hands it back
unlabelled.
"""
struct ZonelessReference <: TimeReference end

# The catalog / ABI spelling of a reference -- `TimeReference::as_storage_string`
# in the Rust core. One string carries all four unambiguously because the core
# refuses a zone name that reads as an offset or as either literal.
_time_reference_str(::Nothing) = nothing
_time_reference_str(::UTCReference) = "utc"
_time_reference_str(::ZonelessReference) = "zoneless"
_time_reference_str(r::ZoneReference) = r.name
function _time_reference_str(r::FixedOffsetReference)
    sign = r.minutes < 0 ? "-" : "+"
    total = abs(r.minutes)
    return string(sign, lpad(total ÷ 60, 2, '0'), ":", lpad(total % 60, 2, '0'))
end

# Minutes east for an offset spelling (`-07:00`, `-0700`, `-07`), or `nothing`
# if the string is not an offset at all -- which is also how `ZoneReference`
# proves a zone name is not one in disguise.
function _offset_minutes(s::AbstractString)
    m = match(r"^([+-])(\d{2}):?(\d{2})?$", String(s))
    m === nothing && return nothing
    hours = parse(Int, m.captures[2])
    minutes = m.captures[3] === nothing ? 0 : parse(Int, m.captures[3])
    (hours <= 23 && minutes <= 59) || return nothing
    total = hours * 60 + minutes
    return m.captures[1] == "-" ? -total : total
end

# Inverse of `_time_reference_str`. The literals and the offset grammar are tried
# before the zone name, in the same order `ZoneReference` rules out, so parsing
# and validation cannot disagree about which spelling a string names.
function _parse_time_reference(s::AbstractString)
    str = String(s)
    str == "utc" && return UTCReference()
    str == "zoneless" && return ZonelessReference()
    offset = _offset_minutes(str)
    offset === nothing || return FixedOffsetReference(offset)
    return ZoneReference(str)
end

_time_reference(::Nothing) = nothing
_time_reference(r::TimeReference) = r
_time_reference(s::AbstractString) = _parse_time_reference(s)

# The default of every constructor's `time_reference=`: the caller said nothing,
# so the spelling is inferred from the timestamp handed in. Distinct from
# `nothing`, which is a *declaration* that the spelling is unspecified -- which
# is what a read passes back when the catalog column is NULL. Collapsing the two
# would make a series written by any other binding without a reference read back
# in Julia as a wall clock, and `add_time_series!` would then write that
# invention back to the store.
struct _Inferred end
const INFERRED = _Inferred()

# What a constructor's `time_reference=` accepts. `_Inferred` is in the union so
# the default is admissible; it is internal and nobody constructs one.
const TimeReferenceArg = Union{Nothing, TimeReference, AbstractString, _Inferred}

"""
    is_zoneless(reference) -> Bool

Whether `reference` is a wall clock rather than an instant. `false` for
`nothing`: an unspecified spelling groups with the zoned ones, which is what the
query-bound and mixed-selection rules split on.
"""
is_zoneless(::Nothing) = false
is_zoneless(r::TimeReference) = r isa ZonelessReference

"""
    zoned_timestamp(instant, reference) -> ZonedDateTime
    zoned_timestamp(series) -> ZonedDateTime
    zoned_timestamp(metadata) -> ZonedDateTime

Fuse a read instant back together with the spelling it was written in.

Reads return a `DateTime` holding the instant and a [`TimeReference`](@ref)
beside it, because widening the return type would make it depend on package load
order and would break every comparison against a `DateTime` literal. This is the
opt-in way to put the two halves back together, and it is **lossless**: the
instant plus the zone name reconstructs the exact value written, including which
side of a fall-back hour it was on.

Requires `using TimeZones` — the method lives in the package extension, so the tz
database is a cost only for callers who want this. Throws for a
[`ZonelessReference`](@ref) series, whose timestamps name no instant, and for a
series that recorded no reference at all.

```julia
using TimeZones
series = get_time_series(SingleTimeSeries, store, key)
zoned_timestamp(series)   # 2024-01-01T00:00:00-07:00
```
"""
function zoned_timestamp end

function zoned_timestamp(::DateTime, ::TimeReference)
    return throw(
        InvalidParameterError(
            "building a ZonedDateTime needs the TimeZones package and the tz database " *
            "it installs; run `using TimeZones` first, which loads the conversion.",
        ),
    )
end

function zoned_timestamp(instant::DateTime, ::Nothing)
    return throw(
        InvalidParameterError(
            "this series recorded no time_reference, so there is no spelling to render " *
            "$instant in. It is still the instant that was stored; label it yourself if " *
            "you know what it was written as.",
        ),
    )
end

# Whether this session's tz database knows `name`. `true` without one: the audit
# is a warning, and a layer with no database has nothing to warn about.
#
# Untyped on purpose. `InfraStoreTimeZonesExt` adds the real check on
# `::AbstractString`, and an extension may only add methods *more specific* than
# the ones it finds -- a same-signature definition is a method overwrite, which
# precompilation refuses outright.
_zone_is_known(_name) = true

# Warn about a zone name the loaded database does not have, on the write path.
#
# A warning, never a gate. Gating would refuse legitimate data whenever IANA's
# database moves ahead of whichever copy happened to be asked -- a caller whose
# TimeZones artifact already has `America/Coyhaique` would be blocked by a store
# whose own copy is a release behind. The instants are stored either way.
function _audit_zone(reference)
    reference isa ZoneReference || return reference
    _zone_is_known(reference.name) || @warn(
        "time_reference names an IANA zone this session's tz database does not have; " *
            "the instants are stored either way, but rendering them in that zone needs a " *
            "database that knows it",
        zone = reference.name,
    )
    return reference
end

# ---- Errors ---------------------------------------------------------------

abstract type TimeSeriesException <: Exception end

struct NotFoundError <: TimeSeriesException
    msg::String
end

struct DuplicateTimeSeriesError <: TimeSeriesException
    msg::String
end

struct DuplicateAssociationError <: TimeSeriesException
    msg::String
end

struct InvalidParameterError <: TimeSeriesException
    msg::String
end

struct IntegrityError <: TimeSeriesException
    msg::String
end

struct ReadOnlyStoreError <: TimeSeriesException
    msg::String
end

struct IncompatibleFormatError <: TimeSeriesException
    msg::String
end

struct IOError <: TimeSeriesException
    msg::String
end

"""
    StoreExistsError

A store already exists where one was about to be created. Creating there would
discard its arrays while keeping its catalog, leaving a store that reopens
cleanly with every array missing. Open it instead, or pass `overwrite=true`.
"""
struct StoreExistsError <: TimeSeriesException
    msg::String
end

"""
    MismatchedArtifactError

The HDF5 file and its `.sqlite` catalog do not carry the same generation stamp,
so they are halves of two different saves — one was copied, replaced, or created
without the other, or a save was interrupted between writing them.
"""
struct MismatchedArtifactError <: TimeSeriesException
    msg::String
end

struct GenericError <: TimeSeriesException
    msg::String
    code::Int32
end

function Base.showerror(io::IO, e::TimeSeriesException)
    return print(io, "InfraStore.", typeof(e).name.name, ": ", e.msg)
end

function _last_error_message()
    needed = Ref{UInt64}(0)
    @ccall lib_path().infrastore_last_error_message(
        C_NULL::Ptr{UInt8}, UInt64(0)::UInt64, needed::Ptr{UInt64}
    )::Int32
    n = Int(needed[])
    n == 0 && return ""
    buf = Vector{UInt8}(undef, n + 1)
    @ccall lib_path().infrastore_last_error_message(
        buf::Ptr{UInt8}, UInt64(n + 1)::UInt64, C_NULL::Ptr{UInt64}
    )::Int32
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
    elseif code == INFRASTORE_ERR_STORE_EXISTS
        throw(StoreExistsError(msg))
    elseif code == INFRASTORE_ERR_MISMATCHED_ARTIFACT
        throw(MismatchedArtifactError(msg))
    else
        throw(GenericError(msg, code))
    end
end

# ---- Fixed-size and catalog-sized string returns ----------------------------

# Exports whose output has a bounded size use a two-call protocol: a null buffer
# reports the required length, then a buffer of that size receives the body.
# `ccall_once(buf, capacity, out_len)` performs one call; Julia requires a
# `ccall` symbol to be a literal, so each call site passes a closure naming its
# own export. The `+ 1` leaves room for the trailing NUL the Rust side appends.
#
# Note what this costs: the Rust side retains nothing between the two calls, so
# it runs the whole operation — query included — once per call. That is fine for
# a path or a metadata row, and emphatically not fine for a listing over a large
# catalog, which is why those use `_owned_str` below.
function _probe(ccall_once)
    out_len = Ref{UInt64}(0)
    _check(ccall_once(C_NULL, UInt64(0), out_len))
    buf = Vector{UInt8}(undef, Int(out_len[]) + 1)
    _check(ccall_once(buf, UInt64(length(buf)), out_len))
    return String(buf[1:Int(out_len[])])
end

# Exports whose output scales with the catalog hand back an owned allocation
# instead, so the query runs and the rows serialize exactly once.
# `ccall_once(out_json, out_len)` performs the single call; the buffer is
# released with `infrastore_string_free` even if decoding throws.
function _owned_str(ccall_once)
    out_json = Ref{Ptr{Cchar}}(C_NULL)
    out_len = Ref{UInt64}(0)
    _check(ccall_once(out_json, out_len))
    ptr = out_json[]
    ptr == C_NULL && return ""
    try
        return unsafe_string(Ptr{UInt8}(ptr), Int(out_len[]))
    finally
        @ccall lib_path().infrastore_string_free(ptr::Ptr{Cchar})::Cvoid
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
    ret = @ccall lib_path().infrastore_store_init_logging(filter_ptr::Cstring)::Int32
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
