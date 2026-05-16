using Test
using Dates
using TimeSeriesStore

@testset "TimeSeriesStore.jl smoke" begin
    store = Store(in_memory=true)

    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values, "load")

    key = add_time_series!(
        store, "42", "Generator", Component, ts;
        features=Dict("model_year" => 2030),
        units="MW",
    )

    @test has_time_series(store, key) == true

    got = get_time_series(store, key)
    @test got.initial_timestamp == initial
    @test got.data == values
    @test got.name == "load"
    @test got.scaling_factor_multiplier === nothing
    @test length(got.data) == 24

    # The same series is reachable attribute-addressed (both conventions unified).
    got_attr = get_time_series(SingleTimeSeries, store, "42", "load";
                               features=Dict("model_year" => 2030))
    @test got_attr.data == values
    @test got_attr.name == "load"
    # ...and via the type-parameterized key form.
    @test get_time_series(SingleTimeSeries, store, key).data == values

    counts = get_counts(store)
    @test counts.static_time_series == 1
    @test counts.components_with_time_series == 1
    @test counts.forecasts == 0

    @test verify_integrity(store) == 0

    remove_time_series!(store, key)
    @test has_time_series(store, key) == false

    @test_throws TimeSeriesStore.NotFoundError get_time_series(store, key)
end

@testset "name + scaling_factor_multiplier round-trip" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    values = collect(1.0:6.0)
    # name is required on the struct; scaling_factor_multiplier is optional.
    ts = SingleTimeSeries(t0, res, values, "load"; scaling_factor_multiplier="get_max_active_power")
    key = add_time_series!(store, "sfm-owner", "Generator", Component, ts)

    # Key-based read preserves both attributes.
    got = get_time_series(store, key)
    @test got.name == "load"
    @test got.scaling_factor_multiplier == "get_max_active_power"

    # Attribute-based read does too.
    got_attr = get_time_series(SingleTimeSeries, store, "sfm-owner", "load"; resolution=res)
    @test got_attr.name == "load"
    @test got_attr.scaling_factor_multiplier == "get_max_active_power"

    # The same array reused under a different name (features-like reuse).
    ts2 = SingleTimeSeries(t0, res, values, "wind")
    add_time_series!(store, "sfm-owner-2", "Generator", Component, ts2)
    other = get_time_series(SingleTimeSeries, store, "sfm-owner-2", "wind"; resolution=res)
    @test other.name == "wind"
    @test other.scaling_factor_multiplier === nothing
    @test other.data == values
end

@testset "non-sequential round-trip" begin
    store = Store(in_memory=true)
    timestamps = [
        DateTime(2024, 1, 1),
        DateTime(2024, 1, 1, 4),
        DateTime(2024, 1, 3),
    ]
    series = NonSequentialTimeSeries(timestamps, Int64[10, 20, 30], "events")
    key = add_time_series!(
        store, "irregular", "Generator", Component, series,
    )
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == Int64[10, 20, 30]
    @test got.name == "events"
    @test get_counts(store).static_time_series == 1

    # Attribute-addressed read returns the same series.
    got_attr = get_time_series(NonSequentialTimeSeries, store, "irregular", "events")
    @test got_attr.timestamps == timestamps
    @test got_attr.data == Int64[10, 20, 30]
    @test got_attr.name == "events"
end

@testset "attribute-based metadata + hash access" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values, "load")

    owner = "11111111-1111-1111-1111-111111111111"
    feats = Dict("model_year" => 2030, "scenario" => "high")  # string feature value
    add_time_series!(store, owner, "Generator", Component, ts;
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
            ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0), "load")
            add_time_series!(store, "1", "Generator", Component, ts)
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
    add_time_series!(store, "c1", "Generator", Component,
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "load"; logical_type="Int64"))
    m = get_metadata(store, "c1", "load"; resolution=res)
    @test m.dtype == Int64
    @test get_array_by_hash(store, m.data_hash, Int64) == Int64[10, 20, 30]

    # Multi-dim element tuple (4 steps × 3 coeffs) round-trips, row-major correct.
    A = Float64[i + j / 10 for i in 1:4, j in 1:3]
    add_time_series!(store, "c2", "Generator", Component,
        SingleTimeSeries(t0, res, A, "cost"; logical_type="QuadraticFunctionData"))
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

    key = add_time_series!(
        store, "det-owner", "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf"; scaling_factor_multiplier="sfm"),
    )

    fc = get_time_series(Deterministic, store, "det-owner", "pf")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test size(fc.data) == (H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
    @test fc.name == "pf"
    @test fc.scaling_factor_multiplier == "sfm"

    # The same forecast is reachable key-based (both conventions unified).
    fc_key = get_time_series(Deterministic, store, key)
    @test fc_key.count == count
    @test fc_key.data == data
    @test fc_key.name == "pf"
    @test fc_key.scaling_factor_multiplier == "sfm"
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

    add_time_series!(
        store, "det-win", "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf2"),
    )

    # Select windows 1 and 2 (0-indexed: window 1 = t0+6h, window 2 = t0+12h).
    # Julia 1-indexed: columns 2 and 3.
    win_start = t0 + Hour(6)
    win_end   = t0 + Hour(18)   # exclusive; covers windows at +6h and +12h

    fc = get_time_series(Deterministic, store, "det-win", "pf2"; time_range=(win_start, win_end))
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test size(fc.data) == (H, 2)
    # The selected slice should equal columns 2 and 3 of the original data.
    @test fc.data == data[:, 2:3]
    @test fc.name == "pf2"
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

    add_time_series!(
        store, "det-multidim", "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_md"),
    )

    fc = get_time_series(Deterministic, store, "det-multidim", "pf_md")
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

    key = add_time_series!(
        store, "prob-owner", "Generator", Component,
        Probabilistic(t0, res, hor, ivl, count, percentiles, data, "pf_prob"),
    )

    fc = get_time_series(Probabilistic, store, "prob-owner", "pf_prob")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test fc.percentiles ≈ percentiles
    @test size(fc.data) == (P, H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
    @test fc.name == "pf_prob"

    # Key-based read returns the same forecast, percentiles included.
    fc_key = get_time_series(Probabilistic, store, key)
    @test fc_key.percentiles ≈ percentiles
    @test fc_key.data == data
    @test fc_key.name == "pf_prob"
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

    add_time_series!(
        store, "prob-win", "Generator", Component,
        Probabilistic(t0, res, hor, ivl, count, percentiles, data, "pf_prob_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+4h, end at t0+12h.
    win_start = t0 + Hour(4)
    win_end   = t0 + Hour(12)

    fc = get_time_series(Probabilistic, store, "prob-win", "pf_prob_win"; time_range=(win_start, win_end))
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
    # Julia shape (scenario_count, H, count) = (3, 4, 2).
    data = Float64[s * 1000 + h * 10 + c for s in 1:scenario_count, h in 1:H, c in 1:count]

    key = add_time_series!(
        store, "scen-owner", "Generator", Component,
        Scenarios(t0, res, hor, ivl, count, data, "pf_scen"),
    )

    fc = get_time_series(Scenarios, store, "scen-owner", "pf_scen")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test fc.scenario_count == scenario_count
    @test size(fc.data) == (scenario_count, H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
    @test fc.name == "pf_scen"

    # Key-based read returns the same forecast.
    fc_key = get_time_series(Scenarios, store, key)
    @test fc_key.scenario_count == scenario_count
    @test fc_key.data == data
    @test fc_key.name == "pf_scen"
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

    add_time_series!(
        store, "scen-win", "Generator", Component,
        Scenarios(t0, res, hor, ivl, count, data, "pf_scen_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+8h, end at t0+24h.
    win_start = t0 + Hour(8)
    win_end   = t0 + Hour(24)

    fc = get_time_series(Scenarios, store, "scen-win", "pf_scen_win"; time_range=(win_start, win_end))
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

    add_time_series!(
        store, "det-i64", "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_i64"),
    )

    fc = get_time_series(Deterministic, store, "det-i64", "pf_i64")
    @test eltype(fc.data) == Int64
    @test fc.data == data
end

@testset "transform_single_time_series! derives DST read as Deterministic" begin
    # Underlying STS: total_len=8, H=4 (horizon=4h, res=1h), interval=2h
    # => interval_steps=2, count = (8 - 4) / 2 + 1 = 3.
    store = Store(in_memory=true)
    t0  = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(2)
    underlying = Float64[i for i in 0:7]

    add_time_series!(
        store, "dst-owner", "Generator", Component,
        SingleTimeSeries(t0, res, underlying, "dst"),
    )

    n = transform_single_time_series!(store, hor, ivl)
    @test n == 1

    fc = get_time_series(Deterministic, store, "dst-owner", "dst")
    @test fc.count == 3
    @test size(fc.data) == (4, 3)
    @test fc.name == "dst"
    # Row-major [H, C]: out[s, w] = underlying[w*2 + s] (0-indexed).
    expected = Float64[underlying[(w - 1) * 2 + s] for s in 1:4, w in 1:3]
    @test fc.data == expected

    # get_time_series_keys enumerates both the source STS and the derived DST;
    # key_info lets us pick the DST and read it back by key (no key from transform).
    keys = get_time_series_keys(store, "dst-owner")
    @test length(keys) == 2
    infos = [key_info(k) for k in keys]
    # time_series_type is the actual Julia type, as in InfrastructureSystems.jl.
    @test Set(i.time_series_type for i in infos) ==
          Set([SingleTimeSeries, DeterministicSingleTimeSeries])
    @test all(i -> i.owner_uuid == "dst-owner" && i.name == "dst", infos)

    dst_idx = findfirst(i -> i.time_series_type == DeterministicSingleTimeSeries, infos)
    # The actual type drives the read directly; a DST has no struct so it returns
    # a Deterministic.
    fc_key = get_time_series(infos[dst_idx].time_series_type, store, keys[dst_idx])
    @test fc_key isa Deterministic
    @test fc_key.count == 3
    @test fc_key.data == expected
    @test fc_key.name == "dst"

    # The source SingleTimeSeries key still reads back as the underlying series.
    sts_idx = findfirst(i -> i.time_series_type == SingleTimeSeries, infos)
    @test get_time_series(infos[sts_idx].time_series_type, store, keys[sts_idx]).data == underlying
end

@testset "get_time_series_keys empty owner" begin
    store = Store(in_memory=true)
    @test get_time_series_keys(store, "nobody") == TimeSeriesStore.TimeSeriesKey[]
end

@testset "key_info exposes attributes and features" begin
    store = Store(in_memory=true)
    res = Hour(1)
    feats = Dict("model_year" => 2030, "scenario" => "high", "active" => true)
    add_time_series!(store, "kf-owner", "Generator", Component,
        SingleTimeSeries(DateTime(2024, 1, 1), res, collect(1.0:6.0), "load");
        features=feats)

    keys = get_time_series_keys(store, "kf-owner")
    @test length(keys) == 1
    info = key_info(keys[1])
    @test info.owner_uuid == "kf-owner"
    @test info.name == "load"
    @test info.time_series_type == SingleTimeSeries
    @test info.resolution == Millisecond(res)
    # Features round-trip (JSON-scalar values preserve their types).
    @test info.features["model_year"] == 2030
    @test info.features["scenario"] == "high"
    @test info.features["active"] === true

    # The key's type resolves the series, and an attribute read using the
    # recovered features matches — confirming features round-trip faithfully.
    @test get_time_series(info.time_series_type, store, keys[1]).data == collect(1.0:6.0)
    @test get_time_series(info.time_series_type, store, info.owner_uuid, info.name;
                          resolution=info.resolution, features=info.features).data ==
          collect(1.0:6.0)
end

@testset "get_forecast_parameters" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 1, 1)
    res = Hour(1); hor = Hour(4); ivl = Hour(2); count = 3; H = 4
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]
    add_time_series!(store, "det-owner", "Generator", Component,
                     Deterministic(t0, res, hor, ivl, count, data, "pf"))

    params = get_forecast_parameters(store)
    @test params.horizon == Millisecond(hor)
    @test params.interval == Millisecond(ivl)
    @test params.count == count
    @test params.resolution == Millisecond(res)

    # No forecasts -> every field is nothing.
    empty = get_forecast_parameters(Store(in_memory=true))
    @test empty.horizon === nothing
    @test empty.interval === nothing
    @test empty.count === nothing
    @test empty.resolution === nothing
end

@testset "compression policies round-trip" begin
    for (compression, level, shuffle) in [(:none, 3, true), (:deflate, 9, false), (:deflate, 1, true)]
        mktempdir() do dir
            path = joinpath(dir, "store.nc")
            let store = Store(in_memory=false, path=path;
                              compression=compression, compression_level=level, shuffle=shuffle)
                ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0), "load")
                add_time_series!(store, "1", "Generator", Component, ts)
                flush!(store)
                TimeSeriesStore.close!(store)
            end
            # Reopen read-write and append, exercising the restored policy.
            let store = TimeSeriesStore.open_store(path; read_only=false)
                ts2 = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(13.0:24.0), "load")
                add_time_series!(store, "2", "Generator", Component, ts2)
                flush!(store)
                TimeSeriesStore.close!(store)
            end
            store = TimeSeriesStore.open_store(path; read_only=true)
            try
                @test verify_integrity(store) == 0
                # The persisted policy is restored on open.
                c = get_compression(store)
                @test c.compression == compression
                if compression == :deflate
                    @test c.level == level
                    @test c.shuffle == shuffle
                end
                m1 = get_metadata(store, "1", "load"; resolution=Hour(1))
                @test get_array_by_hash(store, m1.data_hash) == collect(1.0:12.0)
                m2 = get_metadata(store, "2", "load"; resolution=Hour(1))
                @test get_array_by_hash(store, m2.data_hash) == collect(13.0:24.0)
            finally
                TimeSeriesStore.close!(store)
            end
        end
    end

    @test get_compression(Store(in_memory=true)).compression == :none
    @test_throws ArgumentError Store(in_memory=true, compression=:lz4)
end
