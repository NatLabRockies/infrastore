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
