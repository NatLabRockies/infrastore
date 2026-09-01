# The shared corpus every binding's codec is held to.
#
# `conformance/element_type_vectors.json` is generated from `infrastore-core`'s
# `codec::conformance` vectors and read by the Python, TypeScript and Julia codec
# tests, so all four implementations are checked against one definition of the
# encodings rather than against each other. Regenerate it with
# `UPDATE_CONFORMANCE_VECTORS=1 cargo test -p infrastore-core conformance`.

const _VECTORS_PATH = normpath(
    joinpath(@__DIR__, "..", "..", "..", "conformance", "element_type_vectors.json")
)

# The stored array as Julia holds it. The corpus records `shape` and `values` in
# row-major order — the layout the store writes — so the axes are reversed for
# the reshape and permuted back.
function _storage_array(vector)
    shape = Int.(vector["shape"])
    values = Float64.(vector["values"])
    n = length(shape)
    n == 1 && return values
    return permutedims(reshape(values, reverse(shape)...), reverse(ntuple(identity, n)))
end

# A decoded result flattened back to the corpus' row-major timestep order.
function _row_major(decoded)
    decoded isa AbstractVector && return decoded
    n = ndims(decoded)
    return vec(permutedims(decoded, reverse(ntuple(identity, n))))
end

# The values a vector's `decoded` payload describes, as this package builds them.
function _expected(vector)
    kind = vector["decoded"]["kind"]
    steps = vector["decoded"]["timesteps"]
    if kind == "linear_function"
        return [IS_LinearFunction(s["proportional"], s["constant"]) for s in steps]
    elseif kind == "quadratic_function"
        return [
            IS_QuadraticFunction(s["quadratic"], s["proportional"], s["constant"])
            for s in steps
        ]
    elseif kind == "piecewise_linear"
        return [
            InfraStore.PiecewiseLinear([
                (x=Float64(p["x"]), y=Float64(p["y"])) for p in s
            ]) for s in steps
        ]
    elseif kind == "piecewise_step"
        return [
            InfraStore.PiecewiseStep(Float64.(s["x"]), Float64.(s["y"])) for s in steps
        ]
    elseif kind == "tuple"
        return [Tuple(Float64.(s)) for s in steps]
    end
    return nothing
end

const IS_LinearFunction = InfraStore.LinearFunction
const IS_QuadraticFunction = InfraStore.QuadraticFunction

@testset "element_type conformance vectors decode to their pinned values" begin
    vectors = JSON.parsefile(_VECTORS_PATH)["vectors"]
    @test !isempty(vectors)
    checked = 0
    for vector in vectors
        expected = _expected(vector)
        expected === nothing && continue
        array = _storage_array(vector)
        decoded = decode_element_values(
            array, vector["element_type"], vector["leading_dims"]
        )
        @test _row_major(decoded) == expected
        # The decoded result keeps the array's leading shape, so a forecast comes
        # back windowed rather than flattened.
        @test size(decoded) == size(array)[1:(vector["leading_dims"])]
        checked += 1
    end
    # A corpus that stopped covering the composite kinds would pass silently.
    @test checked == length(vectors)
end

@testset "element_type conformance vectors round-trip through the encoder" begin
    vectors = JSON.parsefile(_VECTORS_PATH)["vectors"]
    for vector in vectors
        expected = _expected(vector)
        expected === nothing && continue
        array = _storage_array(vector)
        lead = size(array)[1:(vector["leading_dims"])]
        # Encode from the shape the values came back in, so the leading axes are
        # the ones the store recorded.
        values = if vector["leading_dims"] == 1
            expected
        else
            permutedims(
                reshape(expected, reverse(lead)...),
                reverse(ntuple(identity, length(lead))),
            )
        end
        encoded, tag = encode_element_values(values)
        @test tag == vector["element_type"]
        @test size(encoded) == size(array)
        @test encoded == array
    end
end

@testset "a scalar element type has nothing to decode" begin
    array = Float64[1.0, 2.0, 3.0]
    @test decode_element_values(array, "f64") === array
    @test !is_composite_element_type("f64")
    @test !is_composite_element_type(nothing)
    @test is_composite_element_type("piecewise_linear")
    @test is_composite_element_type("tuple(3,f64)")
    # A spelling written by a newer core reads back as raw numbers rather than
    # failing: the core owns the vocabulary.
    @test decode_element_values(array, "something_new") === array
    @test !is_composite_element_type("something_new")
end

@testset "the codec decodes into a consumer's own types" begin
    # The whole point of `types`: InfrastructureSystems.jl substitutes its
    # `FunctionData` and pays no conversion. Stand in for it with a local type
    # whose constructor has the same shape.
    struct MyCurve
        pts::Vector{Any}
    end
    Base.:(==)(a::MyCurve, b::MyCurve) = a.pts == b.pts

    curves = [
        InfraStore.PiecewiseLinear([(x=0.0, y=1.0), (x=1.0, y=2.0)]),
        InfraStore.PiecewiseLinear([(x=3.0, y=4.0), (x=5.0, y=6.0)]),
    ]
    array, tag = encode_element_values(curves)
    mine = decode_element_values(
        array, tag; types=merge(DEFAULT_ELEMENT_TYPES, (piecewise_linear=MyCurve,))
    )
    @test mine isa Vector{MyCurve}
    @test [p.pts for p in mine] == [c.points for c in curves]
end

@testset "the codec rejects rows it cannot describe" begin
    # x and y lengths that the leading count could not describe.
    @test_throws InfraStore.InvalidParameterError InfraStore.PiecewiseStep(
        [1.0, 2.0, 3.0], [10.0]
    )
    # A ragged count that overruns its row.
    bad = reshape(Float64[9.0, 1.0, 2.0], 1, 3)
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        bad, "piecewise_linear"
    )
    # A count that fits the row only under a *narrower* rule than its kind uses:
    # 4 points need `1 + 2*4 = 9` slots and 5 steps need `2*5 = 10`, so both of
    # these rows are short. The guard has to know which kind it is guarding, or
    # the decode walks off the end of the row with a `BoundsError` instead.
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        reshape(Float64[4.0, 1.0, 2.0, 3.0, 4.0], 1, 5), "piecewise_linear"
    )
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        reshape(Float64[5.0, 1.0, 2.0, 3.0, 4.0, 5.0], 1, 6), "piecewise_step"
    )
    # A leading slot that is not a count at all. `Int(NaN)` throws an
    # `InexactError`, which is not what a malformed row should report. A
    # *fractional* count is the same kind of row: `1.6` is not "2 points", and
    # rounding it would decode a malformed array into plausible-looking values
    # rather than reporting it — the core refuses it in `validate_ragged_rows`.
    for count in (NaN, Inf, -1.0, 1e30, 1.6, 0.4)
        @test_throws InfraStore.InvalidParameterError decode_element_values(
            reshape(Float64[count, 1.0, 2.0, 3.0, 4.0], 1, 5), "piecewise_linear"
        )
    end
    # The element axis itself has a width per kind, which the core states in
    # `validate_element_dims`. Unchecked, a short row threw a `BoundsError` out
    # of the decode and a long one was silently ignored.
    for (tag, width) in (
        ("linear_function", 1),
        ("linear_function", 3),
        ("quadratic_function", 2),
        ("quadratic_function", 4),
        # `n, x1, y1, ..., xn, yn` is odd; `n, x1..xn, y1..y(n-1)` is 2n or 1.
        ("piecewise_linear", 4),
        ("piecewise_step", 3),
    )
        @test_throws InfraStore.InvalidParameterError decode_element_values(
            zeros(Float64, 1, width), tag
        )
    end
    # The widths each kind does allow still decode, empty rows included.
    @test decode_element_values([1.0 2.0], "linear_function") ==
        [InfraStore.LinearFunction(1.0, 2.0)]
    @test decode_element_values(zeros(Float64, 1, 1), "piecewise_linear") ==
        [InfraStore.PiecewiseLinear([])]
    @test decode_element_values(zeros(Float64, 1, 1), "piecewise_step") ==
        [InfraStore.PiecewiseStep([], [])]
    # A tuple arity the array cannot hold.
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        bad, "tuple(7,f64)"
    )
    # A leading-dims count that does not leave exactly one element axis.
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        bad, "piecewise_linear", 2
    )
    # A value type with no encoding.
    @test_throws InfraStore.InvalidParameterError encode_element_values(["a", "b"])
end

@testset "a series of domain values round-trips as those values" begin
    # The point of the write and read paths knowing the codec: what a write is
    # given is what a read hands back, with no encode/decode step in the caller
    # and no `element_type=` to remember.
    store = Store(; in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    cases = (
        ("lin", [InfraStore.LinearFunction(i, 2i) for i in 1.0:4.0], "linear_function"),
        (
            "quad",
            [InfraStore.QuadraticFunction(i, 2i, 3i) for i in 1.0:4.0],
            "quadratic_function",
        ),
        (
            "pwl",
            [
                InfraStore.PiecewiseLinear([(x=0.0, y=i), (x=1.0, y=2i)])
                for i in 1.0:4.0
            ],
            "piecewise_linear",
        ),
        (
            "step",
            [InfraStore.PiecewiseStep([0.0, 1.0, 2.0], [i, 2i]) for i in 1.0:4.0],
            "piecewise_step",
        ),
        ("tup", [(i, 2i, 3i) for i in 1.0:4.0], "tuple(3,f64)"),
    )

    for (name, values, tag) in cases
        ts = SingleTimeSeries(t0, res, values, name)
        # The constructor names the element type from the values; there is
        # nothing left for the caller to declare.
        @test ts.element_type == tag
        id = add_time_series!(store, 1, "Generator", Component, ts)

        md = get_metadata_by_id(store, id)
        @test md.element_type == tag
        read = read_by_id(store, id)
        @test read.data == values
        @test typeof(read) === typeof(ts)
        # The row names what the read hands back, composite types included.
        @test md.time_series_type == typeof(read)
        # `raw` keeps the packing: one axis more, held as the physical dtype.
        packed = read_by_id(store, id; raw=true)
        @test ndims(packed.data) == ndims(read.data) + 1
        @test eltype(packed.data) === Float64
        @test decode_element_values(packed.data, tag) == values
    end

    # A forecast stacks windows in front of the element axis, so the values keep
    # the window shape they were written with.
    windows = [
        InfraStore.PiecewiseLinear([(x=0.0, y=Float64(h + w)), (x=1.0, y=1.0)])
        for h in 1:4, w in 1:2
    ]
    id = add_time_series!(
        store, 2, "Generator", Component,
        Deterministic(t0, res, Hour(4), Hour(1), 2, windows, "det"),
    )
    read = read_by_id(store, id)
    @test read.data == windows
    @test size(read.data) == (4, 2)
    @test get_metadata_by_id(store, id).time_series_type ==
        Deterministic{InfraStore.PiecewiseLinear, 2}

    # A declaration that contradicts the values is an error, not an override.
    @test_throws InfraStore.InvalidParameterError SingleTimeSeries(
        t0, res, [InfraStore.LinearFunction(1.0, 2.0)], "bad"; element_type="f64"
    )
    # And the same through the other door: a write is where the declaration
    # would otherwise be dropped without a word, since the values name the tag.
    lin = SingleTimeSeries(t0, res, [InfraStore.LinearFunction(1.0, 2.0)], "lin2")
    @test_throws InfraStore.InvalidParameterError add_time_series!(
        store, 3, "Generator", Component, lin; element_type="tuple(2,f64)"
    )
    close!(store)
end

@testset "the irregular types carry domain values too" begin
    # `NonSequentialTimeSeries` and `PersistentTimeSeries` are static series on
    # an explicit time axis, and nothing about that axis changes what the values
    # are — so both doors have to encode at the boundary the same way the
    # regular one does. The persistent write path is the one that did not: it
    # sent the values raw, so a `LinearFunction` series failed at the ABI with
    # "unsupported element dtype" while the read half already decoded.
    store = Store(; in_memory=true)
    stamps = [DateTime(2024, 1, 1), DateTime(2024, 4, 1), DateTime(2024, 9, 1)]
    values = [InfraStore.LinearFunction(i, 2i) for i in 1.0:3.0]

    for (owner, ctor) in enumerate((NonSequentialTimeSeries, PersistentTimeSeries))
        ts = ctor(stamps, values, "cost")
        # The constructor names the element type from the values, on both.
        @test ts.element_type == "linear_function"
        id = add_time_series!(store, owner, "ThermalStandard", Component, ts)

        md = get_metadata_by_id(store, id)
        @test md.element_type == "linear_function"
        read = read_by_id(store, id)
        @test read.data == values
        @test typeof(read) === typeof(ts)
        @test md.time_series_type == typeof(read)
        # `raw` still hands back the packing.
        packed = read_by_id(store, id; raw=true)
        @test ndims(packed.data) == ndims(read.data) + 1
        @test decode_element_values(packed.data, "linear_function") == values

        # And a contradicting declaration is an error at both doors, not an
        # override — the constructor's and the write's.
        @test_throws InfraStore.InvalidParameterError ctor(
            stamps, values, "bad"; element_type="f64"
        )
        @test_throws InfraStore.InvalidParameterError add_time_series!(
            store, owner, "ThermalStandard", Component, ts; element_type="tuple(2,f64)"
        )
    end
    close!(store)
end

@testset "a plain numeric series is untouched by the codec" begin
    # The codec must not change what a scalar series does: the values are the
    # numbers, and `element_type` stays the dtype spelling.
    store = Store(; in_memory=true)
    t0 = DateTime(2024, 1, 1)
    values = Float64[1, 2, 3, 4]
    id = add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), values, "load")
    )
    md = get_metadata_by_id(store, id)
    @test md.element_type == "f64"
    @test md.time_series_type == SingleTimeSeries{Float64, 1}
    read = read_by_id(store, id)
    @test read.data == values
    @test read_by_id(store, id; raw=true).data == values
    close!(store)
end

@testset "an empty series still knows its element type" begin
    # A zero-length series is storable, and its element type is a property of the
    # *type* of its values, not of any value in it — so an empty typed array must
    # not fall through to the numeric path and record no element type at all.
    t0 = DateTime(2024, 1, 1)
    for T in (
        InfraStore.LinearFunction,
        InfraStore.QuadraticFunction,
        InfraStore.PiecewiseLinear,
        InfraStore.PiecewiseStep,
    )
        ts = SingleTimeSeries(t0, Hour(1), T[], "empty")
        @test ts.element_type == InfraStore.element_type_tag(T[])
        array, tag = encode_element_values(T[])
        @test tag == ts.element_type
        @test size(array, 1) == 0
        decoded = decode_element_values(array, tag)
        @test decoded == T[]
        # `==` alone would pass on a `Vector{Any}`: the eltype is what a write
        # reads the element type back off, so an empty decode has to keep it.
        @test eltype(decoded) === T
    end
    for T in (NTuple{2, Float64}, NTuple{3, Float64})
        ts = SingleTimeSeries(t0, Hour(1), T[], "empty")
        array, tag = encode_element_values(T[])
        @test tag == ts.element_type
        @test size(array) == (0, length(T.parameters))
        decoded = decode_element_values(array, tag)
        @test decoded == T[]
        @test eltype(decoded) === T
    end
end

@testset "an empty series of domain values round-trips through the store" begin
    # The read hands back a typed empty vector, so what comes out of one store
    # goes straight into the next. An untyped `Vector{Any}` reads back fine and
    # then fails the write, which is the half a `== T[]` assertion cannot see.
    store = Store(; in_memory=true)
    t0 = DateTime(2024, 1, 1)
    for (owner, T) in enumerate((
        InfraStore.LinearFunction,
        InfraStore.QuadraticFunction,
        InfraStore.PiecewiseLinear,
        InfraStore.PiecewiseStep,
        NTuple{3, Float64},
    ))
        id = add_time_series!(
            store, owner, "Generator", Component,
            SingleTimeSeries(t0, Hour(1), T[], "empty"),
        )
        read = read_by_id(store, id)
        @test eltype(read.data) === T
        @test isempty(read.data)
        @test get_metadata_by_id(store, id).time_series_type == SingleTimeSeries{T, 1}
        # The whole point: the value that came out is a value that goes back in.
        again = add_time_series!(
            store, owner, "Generator", Component,
            SingleTimeSeries(t0, Hour(1), read.data, "empty_again"),
        )
        @test get_metadata_by_id(store, again).element_type ==
            get_metadata_by_id(store, id).element_type
    end
    close!(store)
end

@testset "the value types compare by their fields, not by identity" begin
    # Julia's default `==` on a struct is `===`, which on Float64 fields is the
    # inverse of IEEE in both directions: it separates `0.0` from `-0.0` and
    # equates `NaN` with itself.
    @test InfraStore.LinearFunction(0.0, 1.0) == InfraStore.LinearFunction(-0.0, 1.0)
    @test !(InfraStore.LinearFunction(NaN, 1.0) == InfraStore.LinearFunction(NaN, 1.0))
    @test InfraStore.QuadraticFunction(0.0, 1.0, 2.0) ==
        InfraStore.QuadraticFunction(-0.0, 1.0, 2.0)
    @test !(
        InfraStore.QuadraticFunction(NaN, 1.0, 2.0) ==
        InfraStore.QuadraticFunction(NaN, 1.0, 2.0)
    )
    # Distinct values must not collide across the two types, and equal values
    # must hash alike so the types are usable as dictionary keys.
    @test hash(InfraStore.LinearFunction(1.0, 2.0)) ==
        hash(InfraStore.LinearFunction(1.0, 2.0))
    @test hash(InfraStore.QuadraticFunction(1.0, 2.0, 3.0)) ==
        hash(InfraStore.QuadraticFunction(1.0, 2.0, 3.0))
    @test hash(InfraStore.LinearFunction(1.0, 2.0)) !=
        hash(InfraStore.QuadraticFunction(0.0, 1.0, 2.0))
    d = Dict(InfraStore.LinearFunction(1.0, 2.0) => "a")
    @test d[InfraStore.LinearFunction(1.0, 2.0)] == "a"
end

@testset "a zero-arity tuple has no element type" begin
    # `NTuple{0, Float64}` is a legal Julia type but `tuple(0,f64)` is not a legal
    # element type: the core's grammar rejects arity zero, so encoding one would
    # write a row that could not be read back.
    @test_throws InfraStore.InvalidParameterError encode_element_values(
        NTuple{0, Float64}[()]
    )
    @test_throws InfraStore.InvalidParameterError decode_element_values(
        zeros(Float64, 2, 0), "tuple(0,f64)"
    )
end

# Two stand-ins for a consumer extending the codec, at top level because a method
# defined inside a `@testset` is not visible to code the same block calls.
struct HalfExtended
    v::Float64
end
InfraStore.element_type_tag(::AbstractVector{HalfExtended}) = "linear_function"

struct FullyExtended
    v::Float64
end
InfraStore.element_type_tag(::AbstractVector{FullyExtended}) = "linear_function"
InfraStore.element_row_width(::AbstractVector{FullyExtended}) = 2
function InfraStore.write_element_row!(row, v::FullyExtended)
    row[1] = v.v
    row[2] = 0.0
    return nothing
end

@testset "the encodable predicate checks the whole extension contract" begin
    # `is_element_values` is what the write path asks before it commits to
    # encoding, so a type carrying only part of the extension must answer `false`
    # here rather than fail later inside `encode_element_values` with a
    # `MethodError` naming an internal function.
    @test !InfraStore.is_element_values([HalfExtended(1.0)])
    @test_throws InfraStore.InvalidParameterError encode_element_values(
        [HalfExtended(1.0)]
    )

    # The complete contract opts a consumer's type in, which is what makes the
    # check above a contract test rather than a rejection of unknown types.
    @test InfraStore.is_element_values([FullyExtended(1.0)])
    array, tag = encode_element_values([FullyExtended(1.0)])
    @test tag == "linear_function"
    @test array == reshape(Float64[1.0, 0.0], 1, 2)
end
