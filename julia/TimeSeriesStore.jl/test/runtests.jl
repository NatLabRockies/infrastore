using Test
using Dates
using TimeSeriesStore

@testset "TimeSeriesStore.jl smoke" begin
    store = Store(in_memory=true)

    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values)

    key = add_time_series!(
        store, "42", "Generator", Component, "load", ts;
        features=Dict("model_year" => 2030),
        units="MW",
    )

    @test has_time_series(store, key) == true

    got = get_time_series(store, key)
    @test got.initial_timestamp == initial
    @test got.data == values
    @test length(got.data) == 24

    counts = get_counts(store)
    @test counts.static_time_series == 1
    @test counts.components_with_time_series == 1
    @test counts.forecasts == 0

    @test verify_integrity(store) == 0

    remove_time_series!(store, key)
    @test has_time_series(store, key) == false

    @test_throws TimeSeriesStore.NotFoundError get_time_series(store, key)
end

@testset "non-sequential round-trip" begin
    store = Store(in_memory=true)
    timestamps = [
        DateTime(2024, 1, 1),
        DateTime(2024, 1, 1, 4),
        DateTime(2024, 1, 3),
    ]
    series = NonSequentialTimeSeries(timestamps, Int64[10, 20, 30])
    key = add_time_series!(
        store, "irregular", "Generator", Component, "events", series,
    )
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == Int64[10, 20, 30]
    @test get_counts(store).static_time_series == 1
end

@testset "attribute-based metadata + hash access" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values)

    owner = "11111111-1111-1111-1111-111111111111"
    feats = Dict("model_year" => 2030, "scenario" => "high")  # string feature value
    add_time_series!(store, owner, "Generator", Component, "load", ts;
                     features=feats, units="MW")

    @test has_time_series(store, owner, "load"; resolution=resolution, features=feats)
    @test !has_time_series(store, owner, "load"; resolution=resolution,
                           features=Dict("model_year" => 2031))

    meta = get_metadata(store, owner, "load"; resolution=resolution, features=feats)
    @test meta.initial_timestamp == initial
    @test meta.resolution == Millisecond(resolution)
    @test meta.length == 24
    @test length(meta.data_hash) == 32

    fetched = get_array_by_hash(store, meta.data_hash)
    @test fetched == values

    remove_time_series!(store, owner, "load"; resolution=resolution, features=feats)
    @test !has_time_series(store, owner, "load"; resolution=resolution, features=feats)
    @test_throws TimeSeriesStore.NotFoundError get_metadata(store, owner, "load";
                                                       resolution=resolution, features=feats)
end

@testset "TimeSeriesStore.jl persistent round-trip" begin
    mktempdir() do dir
        path = joinpath(dir, "store.nc")
        let store = Store(in_memory=false, path=path)
            ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0))
            add_time_series!(store, "1", "Generator", Component, "load", ts)
            flush!(store)
            TimeSeriesStore.close!(store)
        end

        store = TimeSeriesStore.open_store(path; read_only=true)
        try
            counts = get_counts(store)
            @test counts.static_time_series == 1
            meta = get_metadata(store, "1", "load"; resolution=Hour(1))
            @test meta.length == 12
            @test get_array_by_hash(store, meta.data_hash) == collect(1.0:12.0)
        finally
            TimeSeriesStore.close!(store)
        end
    end
end

@testset "dtype-parameterized arrays" begin
    store = Store(in_memory=true)
    res = Hour(1)
    t0 = DateTime(2024, 1, 1)

    # Int64 scalar series round-trips with its dtype.
    add_time_series!(store, "c1", "Generator", Component, "load",
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "Int64"))
    m = get_metadata(store, "c1", "load"; resolution=res)
    @test m.dtype == Int64
    @test get_array_by_hash(store, m.data_hash, Int64) == Int64[10, 20, 30]

    # Multi-dim element tuple (4 steps × 3 coeffs) round-trips, row-major correct.
    A = Float64[i + j / 10 for i in 1:4, j in 1:3]
    add_time_series!(store, "c2", "Generator", Component, "cost",
        SingleTimeSeries(t0, res, A, "QuadraticFunctionData"))
    mq = get_metadata(store, "c2", "cost"; resolution=res)
    @test mq.dtype == Float64
    flat = get_array_by_hash(store, mq.data_hash, Float64)
    @test permutedims(reshape(flat, (3, 4)), (2, 1)) == A
end
