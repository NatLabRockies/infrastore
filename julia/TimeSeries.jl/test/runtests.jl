using Test
using Dates
using TimeSeries

@testset "TimeSeries.jl smoke" begin
    store = TimeSeriesStore(in_memory=true)

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

    @test_throws TimeSeries.NotFoundError get_time_series(store, key)
end

@testset "attribute-based metadata + hash access" begin
    store = TimeSeriesStore(in_memory=true)
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
    @test_throws TimeSeries.NotFoundError get_metadata(store, owner, "load";
                                                       resolution=resolution, features=feats)
end

@testset "TimeSeries.jl persistent round-trip" begin
    mktempdir() do dir
        path = joinpath(dir, "store.nc")
        let store = TimeSeriesStore(in_memory=false, path=path)
            ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0))
            add_time_series!(store, "1", "Generator", Component, "load", ts)
            flush!(store)
            TimeSeries.close!(store)
        end

        store = TimeSeries.open_store(path; read_only=true)
        try
            counts = get_counts(store)
            @test counts.static_time_series == 1
            meta = get_metadata(store, "1", "load"; resolution=Hour(1))
            @test meta.length == 12
            @test get_array_by_hash(store, meta.data_hash) == collect(1.0:12.0)
        finally
            TimeSeries.close!(store)
        end
    end
end
