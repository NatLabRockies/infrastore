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

# ---- Forecast read tests (B3) ---------------------------------------------

@testset "Deterministic forecast round-trip" begin
    # H=4 (horizon=4h, resolution=1h), count=3, interval=1h, scalar values.
    store = Store(in_memory=true)
    t0  = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(1)
    count = 3
    H = 4
    # Shape [H, count] = [4, 3]; row-major layout.
    # data[h, c] = (h+1)*10 + (c+1)
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]  # Julia (col-maj) shape [4,3]

    add_forecast!(
        store, "det-owner", "Generator", Component, "pf",
        TimeSeriesStore.TS_TYPE_DETERMINISTIC,
        t0, res, hor, ivl, count, data,
    )

    fc = get_deterministic(store, "det-owner", "pf")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test size(fc.data) == (H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
end

@testset "Deterministic forecast window-selected read" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(6)   # windows start every 6 hours
    count = 4
    H = 4
    # data[h, c] = h*100 + c  (Julia column-major [H, count])
    data = Float64[h * 100 + c for h in 1:H, c in 1:count]

    add_forecast!(
        store, "det-win", "Generator", Component, "pf2",
        TimeSeriesStore.TS_TYPE_DETERMINISTIC,
        t0, res, hor, ivl, count, data,
    )

    # Select windows 1 and 2 (0-indexed: window 1 = t0+6h, window 2 = t0+12h).
    # Julia 1-indexed: columns 2 and 3.
    win_start = t0 + Hour(6)
    win_end   = t0 + Hour(18)   # exclusive; covers windows at +6h and +12h

    fc = get_deterministic(store, "det-win", "pf2"; time_range=(win_start, win_end))
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test size(fc.data) == (H, 2)
    # The selected slice should equal columns 2 and 3 of the original data.
    @test fc.data == data[:, 2:3]
end

@testset "Deterministic forecast multidim element shape" begin
    # H=3, count=2, element_shape=(2,) → stored shape [H=3, count=2, E=2].
    store = Store(in_memory=true)
    t0  = DateTime(2024, 2, 1)
    res = Hour(1)
    hor = Hour(3)
    ivl = Hour(3)
    count = 2
    H = 3
    E = 2
    # Julia array shape (H, count, E) = (3, 2, 2); values are distinguishable.
    data = Float64[h * 1000 + c * 10 + e for h in 1:H, c in 1:count, e in 1:E]

    add_forecast!(
        store, "det-multidim", "Generator", Component, "pf_md",
        TimeSeriesStore.TS_TYPE_DETERMINISTIC,
        t0, res, hor, ivl, count, data,
    )

    fc = get_deterministic(store, "det-multidim", "pf_md")
    @test size(fc.data) == (H, count, E)
    @test fc.data == data
end

@testset "Probabilistic forecast round-trip" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 3, 1)
    res = Hour(1)
    hor = Hour(6)
    ivl = Hour(6)
    count = 3
    H = 6
    percentiles = [0.1, 0.5, 0.9]
    P = length(percentiles)
    # Julia shape (P, H, count) = (3, 6, 3).
    data = Float64[p * 1000 + h * 10 + c for p in 1:P, h in 1:H, c in 1:count]

    add_probabilistic!(
        store, "prob-owner", "Generator", Component, "pf_prob",
        t0, res, hor, ivl, count, percentiles, data,
    )

    fc = get_probabilistic(store, "prob-owner", "pf_prob")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test fc.percentiles ≈ percentiles
    @test size(fc.data) == (P, H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
end

@testset "Probabilistic forecast window-selected read" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 3, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(4)
    count = 4
    H = 4
    percentiles = [0.25, 0.75]
    P = length(percentiles)
    data = Float64[p * 100 + h * 10 + c for p in 1:P, h in 1:H, c in 1:count]

    add_probabilistic!(
        store, "prob-win", "Generator", Component, "pf_prob_win",
        t0, res, hor, ivl, count, percentiles, data,
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+4h, end at t0+12h.
    win_start = t0 + Hour(4)
    win_end   = t0 + Hour(12)

    fc = get_probabilistic(store, "prob-win", "pf_prob_win"; time_range=(win_start, win_end))
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test fc.percentiles ≈ percentiles
    @test size(fc.data) == (P, H, 2)
    @test fc.data == data[:, :, 2:3]
end

@testset "Scenarios forecast round-trip" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 4, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(4)
    count = 2
    H = 4
    scenario_count = 3
    # Scenarios uses add_forecast! with TS_TYPE_SCENARIOS.
    # Julia shape (scenario_count, H, count) = (3, 4, 2).
    data = Float64[s * 1000 + h * 10 + c for s in 1:scenario_count, h in 1:H, c in 1:count]

    add_forecast!(
        store, "scen-owner", "Generator", Component, "pf_scen",
        TimeSeriesStore.TS_TYPE_SCENARIOS,
        t0, res, hor, ivl, count, data,
    )

    fc = get_scenarios(store, "scen-owner", "pf_scen")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test fc.scenario_count == scenario_count
    @test size(fc.data) == (scenario_count, H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
end

@testset "Scenarios forecast window-selected read" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 4, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(8)
    count = 4
    H = 4
    scenario_count = 2
    data = Float64[s * 100 + h * 10 + c for s in 1:scenario_count, h in 1:H, c in 1:count]

    add_forecast!(
        store, "scen-win", "Generator", Component, "pf_scen_win",
        TimeSeriesStore.TS_TYPE_SCENARIOS,
        t0, res, hor, ivl, count, data,
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+8h, end at t0+24h.
    win_start = t0 + Hour(8)
    win_end   = t0 + Hour(24)

    fc = get_scenarios(store, "scen-win", "pf_scen_win"; time_range=(win_start, win_end))
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test fc.scenario_count == scenario_count
    @test size(fc.data) == (scenario_count, H, 2)
    @test fc.data == data[:, :, 2:3]
end

@testset "Forecast non-Float64 dtype (Int64)" begin
    # Verify that non-f64 dtypes survive the FFI round-trip for Deterministic.
    store = Store(in_memory=true)
    t0  = DateTime(2024, 5, 1)
    res = Hour(1)
    hor = Hour(2)
    ivl = Hour(2)
    count = 3
    H = 2
    data = Int64[h * 100 + c for h in 1:H, c in 1:count]

    add_forecast!(
        store, "det-i64", "Generator", Component, "pf_i64",
        TimeSeriesStore.TS_TYPE_DETERMINISTIC,
        t0, res, hor, ivl, count, data,
    )

    fc = get_deterministic(store, "det-i64", "pf_i64")
    @test eltype(fc.data) == Int64
    @test fc.data == data
end
