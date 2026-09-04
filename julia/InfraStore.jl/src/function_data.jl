# ---- Element values -------------------------------------------------------
#
# The domain values behind the store's composite `element_type` spellings, and
# the codec between them and the flat arrays the store holds.
#
# The store owns the encodings — see `docs/src/reference/element-types.md` — and
# `conformance/element_type_vectors.json` pins them across every binding. This is
# the Julia implementation of that spec, and its tests read that corpus.
#
# These types are deliberately *permissive*: they accept exactly what the store
# accepts, including the zero- and one-point piecewise curves a domain type like
# InfrastructureSystems.jl's `PiecewiseLinearData` rejects. A codec that could not
# represent a stored row would be unable to read a store back.
#
# They are also deliberately named for the wire vocabulary (`PiecewiseLinear`,
# not `PiecewiseLinearData`), so that `using InfraStore, InfrastructureSystems`
# is not an ambiguity error. A consumer with its own domain types decodes
# straight into them — see `decode_element_values`' `types` keyword — and never
# materializes these at all.

"""
One `(x, y)` point of a piecewise-linear curve.

A plain `NamedTuple`, not a struct, so a consumer's own point type — an
InfrastructureSystems.jl `XY_COORDS`, say — is the same value and needs no
conversion.
"""
const XYCoords = @NamedTuple{x::Float64, y::Float64}

"""
    LinearFunction(proportional, constant)

`f(x) = proportional * x + constant`. The `linear_function` element type.
"""
struct LinearFunction
    proportional::Float64
    constant::Float64
end

"""
    QuadraticFunction(quadratic, proportional, constant)

`f(x) = quadratic * x^2 + proportional * x + constant`. The `quadratic_function`
element type.
"""
struct QuadraticFunction
    quadratic::Float64
    proportional::Float64
    constant::Float64
end

"""
    PiecewiseLinear(points)

A curve through `(x, y)` points, linearly interpolated between them. The
`piecewise_linear` element type.

`points` may be empty or hold a single point: the store stores what it is given,
and a curve too short to interpolate is still a row a read has to hand back.
"""
struct PiecewiseLinear
    points::Vector{XYCoords}

    PiecewiseLinear(points::AbstractVector{XYCoords}) = new(collect(points))
end

function PiecewiseLinear(points::AbstractVector)
    return PiecewiseLinear(XYCoords[(x=Float64(p[1]), y=Float64(p[2])) for p in points])
end

"""
    PiecewiseStep(x, y)

`n` x-coordinates and the `n - 1` values between them. The `piecewise_step`
element type.

Throws if the lengths disagree — that is the one rule the *encoding* itself
imposes, since the row's leading count has to describe both halves.
"""
struct PiecewiseStep
    x::Vector{Float64}
    y::Vector{Float64}

    function PiecewiseStep(x::AbstractVector, y::AbstractVector)
        expected = max(length(x) - 1, 0)
        length(y) == expected || throw(
            InvalidParameterError(
                "piecewise_step has $(length(x)) x-coordinates, so it needs " *
                "$expected y-values, but has $(length(y))",
            ),
        )
        return new(collect(Float64, x), collect(Float64, y))
    end
end

# Structural equality on all four value types. Julia's default `==` on a struct
# is `===`, which on these is the inverse of IEEE on both counts — it separates
# `0.0` from `-0.0` and equates `NaN` with itself — so a decoded value compared
# against one a consumer built would differ on a signed zero. Field-wise `==`
# gives the four types one rule.
function Base.:(==)(a::LinearFunction, b::LinearFunction)
    return a.proportional == b.proportional && a.constant == b.constant
end
function Base.hash(v::LinearFunction, h::UInt)
    return hash(v.constant, hash(v.proportional, hash(LinearFunction, h)))
end
function Base.:(==)(a::QuadraticFunction, b::QuadraticFunction)
    return a.quadratic == b.quadratic && a.proportional == b.proportional &&
           a.constant == b.constant
end
function Base.hash(v::QuadraticFunction, h::UInt)
    return hash(
        v.constant, hash(v.proportional, hash(v.quadratic, hash(QuadraticFunction, h)))
    )
end
Base.:(==)(a::PiecewiseLinear, b::PiecewiseLinear) = a.points == b.points
Base.hash(v::PiecewiseLinear, h::UInt) = hash(v.points, hash(PiecewiseLinear, h))
Base.:(==)(a::PiecewiseStep, b::PiecewiseStep) = a.x == b.x && a.y == b.y
Base.hash(v::PiecewiseStep, h::UInt) = hash(v.y, hash(v.x, hash(PiecewiseStep, h)))

function Base.show(io::IO, v::LinearFunction)
    return print(io, "LinearFunction(f(x) = $(v.proportional) x + $(v.constant))")
end
function Base.show(io::IO, v::QuadraticFunction)
    return print(
        io,
        "QuadraticFunction(f(x) = $(v.quadratic) x^2 + $(v.proportional) x + $(v.constant))",
    )
end
function Base.show(io::IO, v::PiecewiseLinear)
    return print(io, "PiecewiseLinear(", length(v.points), " points)")
end
Base.show(io::IO, v::PiecewiseStep) =
    print(io, "PiecewiseStep(", length(v.x), " x-coords)")

"Every element value type this package defines."
const FunctionData = Union{
    LinearFunction, QuadraticFunction, PiecewiseLinear, PiecewiseStep
}

# ---- The encode direction -------------------------------------------------
#
# Three small generic functions, extended per value type. A consumer with its own
# domain types adds methods for them and encodes without conversion; that is why
# they take the value rather than a tag.

"""
    element_type_tag(values) -> String

The canonical `element_type` spelling a vector of `values` encodes to.

Extend this, [`element_row_width`](@ref) and [`write_element_row!`](@ref) for a
domain type of your own to encode it directly.
"""
function element_type_tag end

"""
    element_row_width(values) -> Int

How many slots one timestep of `values` occupies. Takes the whole vector because
the ragged kinds are padded to their widest entry.
"""
function element_row_width end

"""
    write_element_row!(row, value)

Write one `value` into `row`, a view of `element_row_width` slots that is already
zeroed. Only the slots the value uses need writing; the rest stay padding.
"""
function write_element_row! end

element_type_tag(::AbstractVector{<:LinearFunction}) = "linear_function"
element_row_width(::AbstractVector{<:LinearFunction}) = 2
function write_element_row!(row, v::LinearFunction)
    row[1] = v.proportional
    row[2] = v.constant
    return nothing
end

element_type_tag(::AbstractVector{<:QuadraticFunction}) = "quadratic_function"
element_row_width(::AbstractVector{<:QuadraticFunction}) = 3
function write_element_row!(row, v::QuadraticFunction)
    row[1] = v.quadratic
    row[2] = v.proportional
    row[3] = v.constant
    return nothing
end

element_type_tag(::AbstractVector{<:PiecewiseLinear}) = "piecewise_linear"
# `n, x1, y1, ..., xn, yn`, padded to the widest curve in the series.
function element_row_width(values::AbstractVector{<:PiecewiseLinear})
    return 1 + 2 * maximum(v -> length(v.points), values; init=0)
end
function write_element_row!(row, v::PiecewiseLinear)
    row[1] = length(v.points)
    for (k, p) in enumerate(v.points)
        row[2k] = p.x
        row[2k + 1] = p.y
    end
    return nothing
end

element_type_tag(::AbstractVector{<:PiecewiseStep}) = "piecewise_step"
# `n, x1..xn, y1..y(n-1)` — a used span of `2n`, except an empty entry, whose one
# slot is the count itself. Hence the `max(_, 1)`: a series of only empty step
# functions still needs a column to hold their zeros.
function element_row_width(values::AbstractVector{<:PiecewiseStep})
    widest = maximum(v -> length(v.x), values; init=0)
    return max(2 * widest, 1)
end
function write_element_row!(row, v::PiecewiseStep)
    n = length(v.x)
    row[1] = n
    row[2:(1 + n)] .= v.x
    row[(2 + n):(1 + n + length(v.y))] .= v.y
    return nothing
end

# A tuple's arity is part of its element type, and the core's grammar has no
# `tuple(0,…)` spelling — encoding one would write a row that cannot be read
# back. `NTuple{0, Float64}` is a legal Julia type, so this has to be refused
# rather than assumed away.
function element_type_tag(::AbstractVector{NTuple{N, Float64}}) where {N}
    N == 0 && throw(
        InvalidParameterError(
            "a 0-tuple has no element type: tuple(0,f64) is not a valid spelling"
        ),
    )
    return "tuple($N,f64)"
end
element_row_width(::AbstractVector{NTuple{N, Float64}}) where {N} = N
function write_element_row!(row, v::NTuple{N, Float64}) where {N}
    for j in 1:N
        row[j] = v[j]
    end
    return nothing
end

"""
    is_element_values(values) -> Bool

Whether `values` carry an element encoding of their own, as opposed to being
numbers the store holds as they are.

True exactly when a value type has the three methods above — which is how a
consumer opts its own domain types in without this package knowing them. It is
deliberately *not* a check against [`FunctionData`](@ref): that union names what
this package defines, and a consumer's types are not in it.

All three are checked, not just the tag: this predicate is what the write path
asks before it commits to encoding, so a half-extended type answering `true`
here would be accepted by a constructor and then fail inside
[`encode_element_values`](@ref) with a `MethodError` naming an internal function.
The `write_element_row!` check goes through the element type because the row it
takes does not exist yet; declaring that argument more narrowly than
`AbstractVector{Float64}` opts a type back out.
"""
function is_element_values(values::AbstractVector)
    return applicable(element_type_tag, values) &&
           applicable(element_row_width, values) &&
           hasmethod(write_element_row!, Tuple{AbstractVector{Float64}, eltype(values)})
end
is_element_values(values::AbstractArray) = is_element_values(vec(values))

"""
    encode_element_values(values) -> (array, element_type)

Encode an array of per-timestep domain values into the flat `Float64` array the
store holds, and the canonical `element_type` naming its layout.

`values` has the shape the stored array has *without* its trailing element axis:
a vector for a static series, an `(H, count)` matrix for a `Deterministic`, an
`(P, H, count)` array for a `Probabilistic` or `Scenarios`. The returned array
adds the element axis last, which is where the store keeps it.

The ragged kinds are padded to their widest entry across the whole input, so the
same curve encodes to different bytes in a different series — that is the
storage layout, not a property of the value, and it is why two series holding
equal curves may not share an array.

```julia
curves = [PiecewiseLinear([(x = 0.0, y = 1.0), (x = 1.0, y = 3.0)]),
          PiecewiseLinear([(x = 0.0, y = 2.0)])]
array, element_type = encode_element_values(curves)   # (2, 5), "piecewise_linear"
```
"""
function encode_element_values(values::AbstractArray)
    flat = vec(values)
    # Checked here rather than as a fallback method on `element_type_tag`: a
    # fallback would make `applicable` true for every type, and `is_element_values`
    # is how the write path decides whether there is anything to pack at all.
    is_element_values(flat) || throw(
        InvalidParameterError("no element_type encodes $(eltype(flat)) values")
    )
    tag = element_type_tag(flat)
    width = element_row_width(flat)
    array = zeros(Float64, size(values)..., width)
    rows = reshape(array, length(flat), width)
    for (i, v) in enumerate(flat)
        write_element_row!(@view(rows[i, :]), v)
    end
    return (array, tag)
end

# ---- The decode direction -------------------------------------------------
#
# Decode is a lookup, not a dispatch: it starts from an `element_type` string off
# a catalog row, so the type to build has to be chosen by name. `types` is that
# choice, defaulting to this package's own values.

"""
The value types [`decode_element_values`](@ref) builds by default, one per
composite `element_type` spelling.

Pass your own to decode straight into domain types of your own:

```julia
decode_element_values(array, "piecewise_linear";
    types = merge(DEFAULT_ELEMENT_TYPES, (piecewise_linear = MyCurve,)))
```

Each entry is called with the values of one timestep, in the order the encoding
records them: `linear_function(proportional, constant)`,
`quadratic_function(quadratic, proportional, constant)`,
`piecewise_linear(points)` with a `Vector` of `(x, y)` NamedTuples, and
`piecewise_step(x_coords, y_values)`. Those are the signatures
InfrastructureSystems.jl's `FunctionData` constructors already have, which is the
point: a consumer substitutes its types and pays no conversion.

A `tuple(N,f64)` spelling always decodes to an `NTuple{N, Float64}`; it has no
entry because there is nothing to choose.
"""
const DEFAULT_ELEMENT_TYPES = (
    linear_function=LinearFunction,
    quadratic_function=QuadraticFunction,
    piecewise_linear=PiecewiseLinear,
    piecewise_step=PiecewiseStep,
)

const _TUPLE_TAG = r"^tuple\(\s*(\d+)\s*,\s*f64\s*\)$"

# One timestep's worth of slots -> one value. `row` is a view of the element axis.
_decode_element_row(::Type{T}, ::Val{:linear_function}, row) where {T} = T(row[1], row[2])
function _decode_element_row(::Type{T}, ::Val{:quadratic_function}, row) where {T}
    return T(row[1], row[2], row[3])
end

function _decode_element_row(::Type{T}, ::Val{:piecewise_linear}, row) where {T}
    n = _row_count(row, n -> 1 + 2n)
    return T(XYCoords[(x=row[2k], y=row[2k + 1]) for k in 1:n])
end

function _decode_element_row(::Type{T}, ::Val{:piecewise_step}, row) where {T}
    n = _row_count(row, n -> n == 0 ? 1 : 2n)
    return T(Float64[row[1 + j] for j in 1:n], Float64[row[1 + n + j] for j in 1:(n - 1)])
end

# A ragged row's leading slot is its used count, stored as a `Float64` because
# the whole array is one. Required to be an *exact* non-negative whole number,
# which is the rule the core states in `ElementType::validate_ragged_rows`: the
# value came back through the same float that carried it out, so a count no
# encoder could have written is a malformed row, not a count to round. `1.6` is
# not "2 points".
#
# `slots_for` is how many of the row's slots a count of `n` actually uses, which
# differs by kind: `1 + 2n` for a list of points, `2n` for x-coords plus steps
# (except an empty one, whose single slot is the count itself). Passed in rather
# than assumed, because the weaker `1 + n` this once checked lets an under-wide
# row through the guard and into an unchecked index — the same rule the core
# enforces in `ElementType::validate_ragged_rows`.
function _row_count(row, slots_for)
    raw = row[1]
    # The bound is part of the same test, not decoration: `NaN`, `Inf` and
    # anything past `typemax(Int)` make the conversion below throw an
    # `InexactError`, which is not the error a malformed row should report.
    (isinteger(raw) && abs(raw) < typemax(Int)) || throw(
        InvalidParameterError(
            "ragged element row leading count is $raw, which is not a " *
            "non-negative whole number",
        ),
    )
    n = Int(raw)
    # `n` comes out of the row's own data, so it is caller-controlled: bound it
    # by the row length *before* the arithmetic, which would otherwise wrap
    # silently on a count near `typemax(Int)` and make an over-long row look
    # short enough.
    (0 <= n <= length(row) && slots_for(n) <= length(row)) || throw(
        InvalidParameterError(
            "ragged element row declares $n entries, which does not fit its " *
            "$(length(row)) slots",
        ),
    )
    return n
end

"""
    decode_element_values(array, element_type, [leading_dims]; types) -> Array

Decode a stored array into the per-timestep values its `element_type` describes —
the inverse of [`encode_element_values`](@ref).

The result has `array`'s shape without its trailing element axis: a
`(length, k)` static array decodes to a `length`-vector, a `(H, count, k)`
`Deterministic` to an `(H, count)` matrix, a `(P, H, count, k)` `Probabilistic`
or `Scenarios` to a 3-D array. `leading_dims` is how many axes precede the
element one; it defaults to everything but the last, which is what a stored array
always is.

`array` is returned unchanged for a scalar `element_type` — a dtype spelling —
because there the stored numbers already *are* the values. That is not an error
and not an empty answer: check with `is_composite_element_type` if you need to
distinguish the cases.

Pass `types` to build domain types of your own; see [`DEFAULT_ELEMENT_TYPES`](@ref).
"""
function decode_element_values(
    array::AbstractArray,
    element_type::AbstractString,
    leading_dims::Integer=max(ndims(array) - 1, 1);
    types::NamedTuple=DEFAULT_ELEMENT_TYPES,
)
    tag = String(element_type)
    kind = _element_kind(tag)
    kind === nothing && return array
    ndims(array) == leading_dims + 1 || throw(
        InvalidParameterError(
            "element_type $tag occupies one trailing axis, so a $(ndims(array))-d " *
            "array has $(ndims(array) - 1) leading dims, not $leading_dims",
        ),
    )
    lead = size(array)[1:leading_dims]
    rows = reshape(array, prod(lead), size(array, ndims(array)))
    out = _decode_rows(kind, rows, types, tag)
    return leading_dims == 1 ? out : reshape(out, lead)
end

function _decode_rows(kind::Symbol, rows, types::NamedTuple, tag::AbstractString)
    m = match(_TUPLE_TAG, tag)
    if m !== nothing
        n = parse(Int, m.captures[1])
        n == 0 && throw(
            InvalidParameterError("$tag is not a valid element type: arity 0")
        )
        size(rows, 2) == n || throw(
            InvalidParameterError(
                "$tag is $n slots wide, but the array's element axis is " *
                "$(size(rows, 2))",
            ),
        )
        # The eltype is declared rather than inferred: a comprehension over an
        # empty axis produces no values to infer from, so a zero-length series
        # would decode to a `Vector{Tuple{Vararg{Float64}}}` (or a `Vector{Any}`
        # below) and no longer round-trip back into a write, which reads the
        # element type off the values' type.
        return NTuple{n, Float64}[ntuple(j -> rows[i, j], n) for i in axes(rows, 1)]
    end
    _validate_row_width(kind, size(rows, 2), tag)
    haskey(types, kind) || throw(
        InvalidParameterError("no type given for element_type $tag")
    )
    T = types[kind]
    return _decode_rows_typed(T, Val(kind), rows)
end

# The row loop, behind a function barrier.
#
# Decode starts from a tag string, so both the kind and the type to build are
# runtime values: `kind` is a `Symbol` and `T` comes out of a `NamedTuple`
# indexed by it, which infers as `Any`. Written inline, that made
# `_decode_element_row` a dynamic dispatch on *every row* — the cost paid once
# per value in a reader sweep, where a group is decoded a column at a time.
# Taking both as arguments moves the dispatch to this call, once per decode, and
# lets the loop inside specialize on the concrete type and kind.
function _decode_rows_typed(::Type{T}, kind::Val, rows) where {T}
    return T[_decode_element_row(T, kind, @view(rows[i, :])) for i in axes(rows, 1)]
end

# The trailing axis width a kind requires, mirroring the core's
# `ElementType::validate_element_dims`. The tuple branch above states its own,
# since its width is in its tag; these four are the kinds whose width is implied
# by the layout, and without the check a short row throws a `BoundsError` out of
# the decode and a long one is silently ignored.
function _validate_row_width(kind::Symbol, width::Integer, tag::AbstractString)
    fixed = if kind === :linear_function
        2
    elseif kind === :quadratic_function
        3
    else
        nothing
    end
    if fixed !== nothing
        width == fixed || throw(
            InvalidParameterError(
                "element_type $tag requires per-step element dims [$fixed], got [$width]"
            ),
        )
    elseif kind === :piecewise_linear
        # `n, x1, y1, ..., xn, yn`, so the width is odd and at least the count.
        (width >= 1 && isodd(width)) || throw(
            InvalidParameterError(
                "element_type $tag cannot have row width $width: a row holds " *
                "1 + 2*points values, so the width is odd",
            ),
        )
    elseif kind === :piecewise_step
        # `n, x1..xn, y1..y(n-1)`: 2n, or 1 when every timestep is empty.
        (width == 1 || (width >= 2 && iseven(width))) || throw(
            InvalidParameterError(
                "element_type $tag cannot have row width $width: a row holds " *
                "2*points values, or 1 when every timestep is empty",
            ),
        )
    end
    return nothing
end

# The kind an `element_type` names, or `nothing` for a plain dtype — where the
# stored numbers are the values and there is nothing to decode. An unrecognized
# spelling is `nothing` too: the core owns the vocabulary, so a row written by a
# newer one still reads back, as raw numbers, rather than throwing.
function _element_kind(tag::AbstractString)
    tag == "linear_function" && return :linear_function
    tag == "quadratic_function" && return :quadratic_function
    tag == "piecewise_linear" && return :piecewise_linear
    tag == "piecewise_step" && return :piecewise_step
    occursin(_TUPLE_TAG, tag) && return :tuple
    return nothing
end

"""
    is_composite_element_type(element_type) -> Bool

Whether `element_type` names a layout [`decode_element_values`](@ref) turns into
values, as opposed to a plain dtype whose stored numbers are already the answer.

False for a spelling this version does not recognize, which reads back as raw
numbers rather than failing.
"""
function is_composite_element_type(element_type::AbstractString)
    return _element_kind(element_type) !== nothing
end
is_composite_element_type(::Nothing) = false
