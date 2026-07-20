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
        store, 42, "Generator", Component, ts;
        features=Dict("model_year" => 2030),
        units="MW",
    )

    @test has_time_series(store, key) == true

    got = get_time_series(store, key)
    @test got.initial_timestamp == initial
    @test got.data == values
    @test got.name == "load"
    @test length(got.data) == 24

    # The same series is reachable attribute-addressed (both conventions unified).
    got_attr = get_time_series(SingleTimeSeries, store, 42, Component, "load";
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

@testset "bulk_read SingleTimeSeries" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)

    keys = TimeSeriesKey[]
    for i in 0:4
        vals = collect((i * 10.0):(i * 10.0 + 11.0))   # length 12, distinct per series
        ts = SingleTimeSeries(initial, resolution, vals, "load")
        push!(keys, add_time_series!(store, i + 1, "Generator", Component, ts))
    end

    series = bulk_read(store, keys)
    @test length(series) == 5
    for i in 0:4
        expected = collect((i * 10.0):(i * 10.0 + 11.0))
        @test series[i + 1].data == expected
        @test series[i + 1].name == "load"
        @test series[i + 1].initial_timestamp == initial
        # Matches the per-key read, in order.
        @test series[i + 1].data == get_time_series(store, keys[i + 1]).data
    end

    # Empty input returns an empty vector without touching the store.
    @test bulk_read(store, TimeSeriesKey[]) == SingleTimeSeries[]
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
        store, 7, "Generator", Component, series,
    )
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == Int64[10, 20, 30]
    @test got.name == "events"
    @test get_counts(store).static_time_series == 1

    # Attribute-addressed read returns the same series.
    got_attr = get_time_series(NonSequentialTimeSeries, store, 7, Component, "events")
    @test got_attr.timestamps == timestamps
    @test got_attr.data == Int64[10, 20, 30]
    @test got_attr.name == "events"
end

@testset "non-sequential N-D + logical_type round-trip" begin
    store = Store(in_memory=true)
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 1, 4), DateTime(2024, 1, 3)]
    # A (length, k) per-step element array tagged with an opaque logical type, as a
    # FunctionData encoding would produce on the InfrastructureSystems.jl side.
    data = Float64[1 2; 3 4; 5 6]
    series = NonSequentialTimeSeries(timestamps, data, "curves"; logical_type="LinearFunctionData")
    key = add_time_series!(store, 9, "Generator", Component, series)
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == data
    @test got.data isa Array{Float64,2}
    @test got.logical_type == "LinearFunctionData"
    @test got.name == "curves"

    got_attr = get_time_series(NonSequentialTimeSeries, store, 9, Component, "curves")
    @test got_attr.data == data
    @test got_attr.logical_type == "LinearFunctionData"
end

@testset "attribute-based metadata + hash access" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values, "load")

    owner = 11
    feats = Dict("model_year" => 2030, "scenario" => "high")  # string feature value
    add_time_series!(store, owner, "Generator", Component, ts;
                     features=feats, units="MW")

    @test has_time_series(store, owner, Component, "load"; resolution=resolution, features=feats)
    @test !has_time_series(store, owner, Component, "load"; resolution=resolution,
                           features=Dict("model_year" => 2031))

    meta = get_metadata(store, owner, Component, "load"; resolution=resolution, features=feats)
    @test meta.initial_timestamp == initial
    @test meta.resolution == Millisecond(resolution)
    @test meta.length == 24
    @test length(meta.data_hash) == 32

    fetched = get_array_by_hash(store, meta.data_hash)
    @test fetched == values

    remove_time_series!(store, owner, Component, "load"; resolution=resolution, features=feats)
    @test !has_time_series(store, owner, Component, "load"; resolution=resolution, features=feats)
    @test_throws TimeSeriesStore.NotFoundError get_metadata(store, owner, Component, "load";
                                                       resolution=resolution, features=feats)
end

@testset "TimeSeriesStore.jl persistent round-trip" begin
    mktempdir() do dir
        path = joinpath(dir, "store.nc")
        let store = Store(in_memory=false, path=path)
            ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0), "load")
            add_time_series!(store, 1, "Generator", Component, ts)
            flush!(store)
            TimeSeriesStore.close!(store)
        end

        store = TimeSeriesStore.open_store(path; read_only=true)
        try
            counts = get_counts(store)
            @test counts.static_time_series == 1
            meta = get_metadata(store, 1, Component, "load"; resolution=Hour(1))
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
    add_time_series!(store, 1001, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "load"; logical_type="Int64"))
    m = get_metadata(store, 1001, Component, "load"; resolution=res)
    @test m.dtype == Int64
    @test get_array_by_hash(store, m.data_hash, Int64) == Int64[10, 20, 30]

    # Multi-dim element tuple (4 steps × 3 coeffs) round-trips, row-major correct.
    A = Float64[i + j / 10 for i in 1:4, j in 1:3]
    add_time_series!(store, 1002, "Generator", Component,
        SingleTimeSeries(t0, res, A, "cost"; logical_type="QuadraticFunctionData"))
    mq = get_metadata(store, 1002, Component, "cost"; resolution=res)
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
        store, 100, "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf"),
    )

    fc = get_time_series(Deterministic, store, 100, Component, "pf")
    @test fc.initial_timestamp == t0
    @test fc.resolution == Millisecond(res)
    @test fc.horizon == Millisecond(hor)
    @test fc.interval == Millisecond(ivl)
    @test fc.count == count
    @test size(fc.data) == (H, count)
    @test eltype(fc.data) == Float64
    @test fc.data == data
    @test fc.name == "pf"

    # The same forecast is reachable key-based (both conventions unified).
    fc_key = get_time_series(Deterministic, store, key)
    @test fc_key.count == count
    @test fc_key.data == data
    @test fc_key.name == "pf"
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
        store, 110, "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf2"),
    )

    # Select windows 1 and 2 (0-indexed: window 1 = t0+6h, window 2 = t0+12h).
    # Julia 1-indexed: columns 2 and 3.
    win_start = t0 + Hour(6)
    win_end   = t0 + Hour(18)   # exclusive; covers windows at +6h and +12h

    fc = get_time_series(Deterministic, store, 110, Component, "pf2"; time_range=(win_start, win_end))
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
        store, 120, "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_md"),
    )

    fc = get_time_series(Deterministic, store, 120, Component, "pf_md")
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
        store, 200, "Generator", Component,
        Probabilistic(t0, res, hor, ivl, count, percentiles, data, "pf_prob"),
    )

    fc = get_time_series(Probabilistic, store, 200, Component, "pf_prob")
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
        store, 210, "Generator", Component,
        Probabilistic(t0, res, hor, ivl, count, percentiles, data, "pf_prob_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+4h, end at t0+12h.
    win_start = t0 + Hour(4)
    win_end   = t0 + Hour(12)

    fc = get_time_series(Probabilistic, store, 210, Component, "pf_prob_win"; time_range=(win_start, win_end))
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
        store, 300, "Generator", Component,
        Scenarios(t0, res, hor, ivl, count, data, "pf_scen"),
    )

    fc = get_time_series(Scenarios, store, 300, Component, "pf_scen")
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
        store, 310, "Generator", Component,
        Scenarios(t0, res, hor, ivl, count, data, "pf_scen_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+8h, end at t0+24h.
    win_start = t0 + Hour(8)
    win_end   = t0 + Hour(24)

    fc = get_time_series(Scenarios, store, 310, Component, "pf_scen_win"; time_range=(win_start, win_end))
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
        store, 130, "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_i64"),
    )

    fc = get_time_series(Deterministic, store, 130, Component, "pf_i64")
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
        store, 400, "Generator", Component,
        SingleTimeSeries(t0, res, underlying, "dst"),
    )

    n = transform_single_time_series!(store, hor, ivl)
    @test n == 1

    # The family read resolves to the stored DST (no concrete Deterministic here).
    fc = get_time_series(AbstractDeterministic, store, 400, Component, "dst")
    @test fc.count == 3
    @test size(fc.data) == (4, 3)
    @test fc.name == "dst"
    # Row-major [H, C]: out[s, w] = underlying[w*2 + s] (0-indexed).
    expected = Float64[underlying[(w - 1) * 2 + s] for s in 1:4, w in 1:3]
    @test fc.data == expected

    # A concrete Deterministic request must NOT match a DST.
    @test_throws TimeSeriesStore.NotFoundError get_time_series(
        Deterministic, store, 400, Component, "dst")

    # get_time_series_keys enumerates both the source STS and the derived DST;
    # key_info lets us pick the DST and read it back by key (no key from transform).
    keys = get_time_series_keys(store, 400, Component)
    @test length(keys) == 2
    infos = [key_info(k) for k in keys]
    # time_series_type is the actual Julia type, as in InfrastructureSystems.jl.
    @test Set(i.time_series_type for i in infos) ==
          Set([SingleTimeSeries, DeterministicSingleTimeSeries])
    @test all(i -> i.owner_id == 400 && i.owner_category == Component && i.name == "dst", infos)

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

    # Reference counting lives in the core: the STS and its derived DST share one
    # underlying array, so count_array_references reports one of each. The content
    # hash is physical storage detail, read via the metadata descriptor — it is not
    # carried on a key, so list_keys does not expose it.
    hash = get_metadata(store, 400, Component, "dst").data_hash
    @test count_array_references(store, hash) == (sts = 1, dst = 1)
end

@testset "list_keys filters (owner, type, name, resolution, features)" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    vals = Float64[1, 2, 3, 4]
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), vals, "load"); features=Dict("scenario" => "high"))
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), vals, "load"); features=Dict("scenario" => "low"))
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Minute(5), vals, "wind"))
    add_time_series!(store, 2, "Bus", SupplementalAttribute,
        SingleTimeSeries(t0, Hour(1), vals, "load"))

    @test length(list_keys(store)) == 4
    @test length(list_keys(store; owner_id=1)) == 3
    @test length(list_keys(store; owner_category=SupplementalAttribute)) == 1
    @test length(list_keys(store; name="load")) == 3
    @test length(list_keys(store; time_series_type=TimeSeriesStore.TS_TYPE_SINGLE)) == 4
    @test length(list_keys(store; resolution=Minute(5))) == 1
    # Feature filter is a subset match.
    fkeys = list_keys(store; owner_id=1, name="load", features=Dict("scenario" => "high"))
    @test length(fkeys) == 1
    @test fkeys[1].name == "load"
    @test fkeys[1].resolution == Hour(1)
    # Combined filters narrowing to a single key.
    @test length(list_keys(store; owner_id=1, name="wind", resolution=Minute(5))) == 1
    @test isempty(list_keys(store; owner_id=1, name="wind", resolution=Hour(1)))

    # get_resolutions: distinct resolutions (order is lexical-by-ISO, so compare
    # as a set — periods of different kinds have no numeric total order).
    @test Set(get_resolutions(store)) == Set([Millisecond(Minute(5)), Millisecond(Hour(1))])
    @test Set(get_resolutions(store; time_series_type=TimeSeriesStore.TS_TYPE_SINGLE)) ==
          Set([Millisecond(Minute(5)), Millisecond(Hour(1))])
    @test isempty(get_resolutions(store; time_series_type=TimeSeriesStore.TS_TYPE_DETERMINISTIC))

    # counts_by_type: all four are SingleTimeSeries here.
    cbt = counts_by_type(store)
    @test length(cbt) == 1
    @test cbt[1].time_series_type == SingleTimeSeries
    @test cbt[1].count == 4
    # num_distinct_arrays: the two "load" Hour(1) series share content (same vals,
    # initial, resolution) and dedup to one array; "wind" and the supp-attr "load"
    # add two more distinct owner/name combos but identical values still dedup by
    # content hash, so distinct arrays == 1.
    @test num_distinct_arrays(store) == 1
end

@testset "list_array_groups annotates rows with the content hash" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    shared = Float64[1, 2, 3, 4]
    # Two owners with identical data dedup to one array (one hash); a third owner's
    # distinct data gets a different hash.
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), copy(shared), "load"))
    add_time_series!(store, 2, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), copy(shared), "load"))
    add_time_series!(store, 3, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[9, 8, 7, 6], "load"))

    rows = list_array_groups(store)
    @test length(rows) == 3
    # Rows carry every list_keys field plus a 64-char hex data_hash.
    @test all(r -> r.data_hash isa String && length(r.data_hash) == 64, rows)
    @test all(r -> r.name == "load", rows)

    groups = Dict{String, Vector{Int}}()
    for r in rows
        push!(get!(groups, r.data_hash, Int[]), Int(r.owner_id))
    end
    @test length(groups) == 2
    shared_owners = only([v for v in values(groups) if length(v) > 1])
    @test sort(shared_owners) == [1, 2]

    # Filters behave exactly like list_keys.
    @test length(list_array_groups(store; owner_id=3)) == 1
    @test only(list_array_groups(store; owner_id=1)).data_hash ==
          only(list_array_groups(store; owner_id=2)).data_hash
end

@testset "time_series_counts and list_owner_ids" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    v = Float64[1, 2, 3, 4]
    add_time_series!(store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), v, "load"))
    add_time_series!(store, 2, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[5, 6, 7, 8], "load"))
    add_time_series!(store, 9, "Bus", SupplementalAttribute,
        SingleTimeSeries(t0, Hour(1), v, "voltage"))  # shares content with owner 1
    add_time_series!(store, 1, "Generator", Component,
        Deterministic(t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"))

    c = time_series_counts(store)
    @test c.components_with_time_series == 2            # owners 1, 2
    @test c.supplemental_attributes_with_time_series == 1  # owner 9
    @test c.static_time_series_count == 2               # [1,2,3,4] (x2 shared) + [5,6,7,8]
    @test c.forecast_count == 1                         # one Deterministic array

    @test sort!(list_owner_ids(store, Component)) == [1, 2]
    @test list_owner_ids(store, SupplementalAttribute) == [9]
    @test list_owner_ids(store, Component;
        time_series_type=TimeSeriesStore.TS_TYPE_DETERMINISTIC) == [1]
    @test sort!(list_owner_ids(store, Component; resolution=Hour(1))) == [1, 2]
    @test isempty(list_owner_ids(store, Component; resolution=Minute(5)))
end

@testset "check_static_consistency and filtered get_forecast_parameters" begin
    store = Store(in_memory=true)
    @test isempty(check_static_consistency(store))

    t0 = DateTime(2024, 1, 1)
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "a"))
    add_time_series!(store, 2, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[5, 6, 7, 8], "a"))
    cs = check_static_consistency(store)
    @test length(cs) == 1
    @test cs[1].resolution == Millisecond(Hour(1))
    @test cs[1].initial_timestamp == t0
    @test cs[1].length == 4

    # A second resolution is a distinct grid, not an inconsistency.
    add_time_series!(store, 4, "Generator", Component,
        SingleTimeSeries(t0, Minute(30), Float64[1, 2, 3, 4, 5, 6, 7, 8], "a"))
    multi = check_static_consistency(store)
    @test length(multi) == 2
    @test Set(g.resolution for g in multi) ==
          Set([Millisecond(Hour(1)), Millisecond(Minute(30))])
    # Scoping to one resolution returns only that grid.
    hourly = check_static_consistency(store; resolution=Hour(1))
    @test length(hourly) == 1
    @test hourly[1].length == 4

    # A differing length at an existing resolution is an inconsistency.
    add_time_series!(store, 3, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], "a"))
    @test_throws TimeSeriesStore.IntegrityError check_static_consistency(store)
    # The other resolution's grid still checks out on its own.
    ok = check_static_consistency(store; resolution=Minute(30))
    @test length(ok) == 1 && ok[1].length == 8

    # Filtered forecast parameters.
    fstore = Store(in_memory=true)
    add_time_series!(fstore, 1, "Generator", Component,
        Deterministic(t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"))
    p = get_forecast_parameters(fstore; resolution=Hour(1), interval=Hour(1))
    @test p.horizon == Millisecond(Hour(2))
    @test p.interval == Millisecond(Hour(1))
    @test p.count == 2
    @test p.initial_timestamp == t0
    # A non-matching interval yields no parameters.
    q = get_forecast_parameters(fstore; interval=Hour(3))
    @test q.horizon === nothing
    @test q.count === nothing
end

@testset "static_summary and forecast_summary" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    v = Float64[1, 2, 3, 4]
    # owners 1 & 2 share the static group (Generator/load/Hour(1)/length 4).
    add_time_series!(store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), v, "load"))
    add_time_series!(store, 2, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), Float64[9, 9, 9, 9], "load"))
    add_time_series!(store, 1, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), v, "wind"))  # separate group (name)
    add_time_series!(store, 1, "Generator", Component,
        Deterministic(t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"))

    ss = static_summary(store)
    @test length(ss) == 2
    load_row = only(filter(r -> r.name == "load", ss))
    @test load_row.count == 2                     # grouped across owners 1 and 2
    @test load_row.time_step_count == 4
    @test load_row.time_series_type == SingleTimeSeries
    @test load_row.owner_type == "Generator"
    @test load_row.resolution == Millisecond(Hour(1))
    @test only(filter(r -> r.name == "wind", ss)).count == 1

    fs = forecast_summary(store)
    @test length(fs) == 1
    @test fs[1].name == "fc"
    @test fs[1].count == 1
    @test fs[1].window_count == 2
    @test fs[1].horizon == Millisecond(Hour(2))
    @test fs[1].interval == Millisecond(Hour(1))
    @test fs[1].time_series_type == Deterministic
end

@testset "AbstractDeterministic family resolution: miss and ambiguity" begin
    # The family is resolved in the core; a real miss is no longer masked by the
    # old guess-and-retry fallback.
    store = Store(in_memory=true)
    @test_throws TimeSeriesStore.NotFoundError get_time_series(
        AbstractDeterministic, store, 999, Component, "nope")

    # Two Deterministic forecasts of one variable at the same resolution but
    # different intervals (e.g. day-ahead vs intra-day). Interval is part of the
    # identity, so both coexist; a read that does not pin the interval is
    # ambiguous, and pinning it disambiguates.
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(2)
    add_time_series!(
        store, 401, "Generator", Component,
        Deterministic(t0, res, hor, Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )
    add_time_series!(
        store, 401, "Generator", Component,
        Deterministic(t0, res, hor, Hour(6), 2, reshape(Float64[10, 11, 12, 13], 2, 2), "dup"),
    )
    @test_throws TimeSeriesStore.InvalidParameterError get_time_series(
        Deterministic, store, 401, Component, "dup")
    # Pinning the interval disambiguates.
    cd = get_time_series(Deterministic, store, 401, Component, "dup"; interval=Hour(6))
    @test cd isa Deterministic
    @test cd.interval == Hour(6)
end

@testset "Deterministic and DeterministicSingleTimeSeries are mutually exclusive" begin
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(2)
    ivl = Hour(1)

    # Adding a Deterministic when a DST view of the same family exists is rejected.
    store = Store(in_memory=true)
    add_time_series!(
        store, 401, "Generator", Component,
        SingleTimeSeries(t0, res, Float64[i for i in 0:7], "dup"),
    )
    transform_single_time_series!(store, hor, ivl)
    @test_throws TimeSeriesStore.InvalidParameterError add_time_series!(
        store, 401, "Generator", Component,
        Deterministic(t0, res, hor, ivl, 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )

    # The reverse: deriving a DST when a Deterministic of the same family exists.
    store2 = Store(in_memory=true)
    add_time_series!(
        store2, 401, "Generator", Component,
        Deterministic(t0, res, hor, ivl, 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )
    add_time_series!(
        store2, 401, "Generator", Component,
        SingleTimeSeries(t0, res, Float64[i for i in 0:7], "dup"),
    )
    @test_throws TimeSeriesStore.InvalidParameterError transform_single_time_series!(
        store2, hor, ivl)
end

@testset "get_time_series_keys empty owner" begin
    store = Store(in_memory=true)
    @test get_time_series_keys(store, 999, Component) == TimeSeriesStore.TimeSeriesKey[]
end

@testset "key_info exposes attributes and features" begin
    store = Store(in_memory=true)
    res = Hour(1)
    feats = Dict("model_year" => 2030, "scenario" => "high", "active" => true)
    add_time_series!(store, 500, "Generator", Component,
        SingleTimeSeries(DateTime(2024, 1, 1), res, collect(1.0:6.0), "load");
        features=feats)

    keys = get_time_series_keys(store, 500, Component)
    @test length(keys) == 1
    info = key_info(keys[1])
    @test info.owner_id == 500
    @test info.owner_category == Component
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
    @test get_time_series(info.time_series_type, store, info.owner_id, info.owner_category, info.name;
                          resolution=info.resolution, features=info.features).data ==
          collect(1.0:6.0)
end

@testset "get_forecast_parameters" begin
    store = Store(in_memory=true)
    t0  = DateTime(2024, 1, 1)
    res = Hour(1); hor = Hour(4); ivl = Hour(2); count = 3; H = 4
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]
    add_time_series!(store, 100, "Generator", Component,
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
                add_time_series!(store, 1, "Generator", Component, ts)
                flush!(store)
                TimeSeriesStore.close!(store)
            end
            # Reopen read-write and append, exercising the restored policy.
            let store = TimeSeriesStore.open_store(path; read_only=false)
                ts2 = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(13.0:24.0), "load")
                add_time_series!(store, 2, "Generator", Component, ts2)
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
                m1 = get_metadata(store, 1, Component, "load"; resolution=Hour(1))
                @test get_array_by_hash(store, m1.data_hash) == collect(1.0:12.0)
                m2 = get_metadata(store, 2, Component, "load"; resolution=Hour(1))
                @test get_array_by_hash(store, m2.data_hash) == collect(13.0:24.0)
            finally
                TimeSeriesStore.close!(store)
            end
        end
    end

    @test get_compression(Store(in_memory=true)).compression == :none
    @test_throws ArgumentError Store(in_memory=true, compression=:lz4)
end

@testset "AddBatch bulk add" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)

    batch = AddBatch()
    @test length(batch) == 0
    for i in 1:10
        ts = SingleTimeSeries(initial, resolution, collect(Float64.(i:(i + 23))), "load")
        add_time_series!(batch, i, "Generator", Component, ts;
                         features=Dict("scenario" => i), units="MW")
    end
    # A forecast and a non-sequential series in the same batch.
    det_data = reshape(collect(1.0:12.0), 3, 4)  # canonical shape (H=3, count=4)
    det = Deterministic(initial, Hour(1), Hour(3), Hour(1), 4, det_data, "fc")
    add_time_series!(batch, 600, "Generator", Component, det)
    ns = NonSequentialTimeSeries(
        [DateTime(2024, 1, 1), DateTime(2024, 1, 2)], Int64[1, 2], "events")
    add_time_series!(batch, 700, "Generator", Component, ns)
    @test length(batch) == 12

    keys = add_time_series_bulk!(store, batch)
    @test length(keys) == 12
    @test length(batch) == 0  # drained, reusable

    for i in 1:10
        got = get_time_series(store, keys[i])
        @test got.data == collect(Float64.(i:(i + 23)))
    end
    fc = get_time_series(Deterministic, store, keys[11])
    @test fc.data == det_data
    got_ns = get_time_series(NonSequentialTimeSeries, store, keys[12])
    @test got_ns.data == Int64[1, 2]

    counts = get_counts(store)
    @test counts.static_time_series == 11
    @test counts.forecasts == 1
end

@testset "AddBatch all-or-nothing rollback" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    ts = SingleTimeSeries(initial, Hour(1), collect(1.0:24.0), "load")

    batch = AddBatch()
    add_time_series!(batch, 800, "Generator", Component, ts)
    add_time_series!(batch, 800, "Generator", Component, ts)
    @test_throws TimeSeriesStore.DuplicateTimeSeriesError add_time_series_bulk!(store, batch)
    @test length(batch) == 0
    @test isempty(get_time_series_keys(store, 800, Component))

    # The batch is reusable after a failed submit.
    add_time_series!(batch, 800, "Generator", Component, ts)
    keys = add_time_series_bulk!(store, batch)
    @test length(keys) == 1
end

@testset "owner category disambiguates a shared owner_id" begin
    # A Component and a SupplementalAttribute may reuse the SAME numeric owner_id
    # while keeping independent time series. Add identical-attribute
    # SingleTimeSeries to each and assert they coexist and are independently
    # readable / removable.
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    owner_id = 1234

    comp_vals = collect(1.0:24.0)
    supp_vals = collect(100.0:123.0)

    comp_key = add_time_series!(
        store, owner_id, "Generator", Component,
        SingleTimeSeries(initial, resolution, comp_vals, "load"),
    )
    supp_key = add_time_series!(
        store, owner_id, "Outage", SupplementalAttribute,
        SingleTimeSeries(initial, resolution, supp_vals, "load"),
    )

    # Two distinct owners despite the shared numeric id.
    @test get_counts(store).components_with_time_series == 2

    # Each category reads back its own data, key-based...
    @test get_time_series(store, comp_key).data == comp_vals
    @test get_time_series(store, supp_key).data == supp_vals

    # ...and attribute-addressed, keyed on the category.
    @test get_time_series(SingleTimeSeries, store, owner_id, Component, "load"; resolution=resolution).data == comp_vals
    @test get_time_series(SingleTimeSeries, store, owner_id, SupplementalAttribute, "load"; resolution=resolution).data == supp_vals

    # has_time_series / get_metadata are independent per category.
    @test has_time_series(store, owner_id, Component, "load"; resolution=resolution)
    @test has_time_series(store, owner_id, SupplementalAttribute, "load"; resolution=resolution)

    # get_time_series_keys is scoped to (owner_id, owner_category).
    comp_keys = get_time_series_keys(store, owner_id, Component)
    supp_keys = get_time_series_keys(store, owner_id, SupplementalAttribute)
    @test length(comp_keys) == 1
    @test length(supp_keys) == 1
    @test key_info(comp_keys[1]).owner_category == Component
    @test key_info(supp_keys[1]).owner_category == SupplementalAttribute

    # Removing the component series leaves the supplemental one intact.
    remove_time_series!(store, owner_id, Component, "load"; resolution=resolution)
    @test !has_time_series(store, owner_id, Component, "load"; resolution=resolution)
    @test has_time_series(store, owner_id, SupplementalAttribute, "load"; resolution=resolution)
    @test get_time_series(store, supp_key).data == supp_vals
    @test isempty(get_time_series_keys(store, owner_id, Component))
    @test length(get_time_series_keys(store, owner_id, SupplementalAttribute)) == 1
end

@testset "StaticReader: columnar reads across dtypes and shapes" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 1, 1)
    res = Hour(1)
    add_time_series!(store, 2, "Gen", Component, SingleTimeSeries(t0, res, [20.0, 21.0, 22.0, 23.0], "load"))
    add_time_series!(store, 1, "Gen", Component, SingleTimeSeries(t0, res, [10.0, 11.0, 12.0, 13.0], "load"))
    add_time_series!(store, 3, "Gen", Component, SingleTimeSeries(t0, res, Int64[100, 101, 102, 103], "count"))
    # f64 with element shape (2,): data shape (time=4, E=2).
    pair = Float64[t * 10 + e for t in 1:4, e in 1:2]
    add_time_series!(store, 5, "Gen", Component, SingleTimeSeries(t0, res, pair, "pair"))

    r = build_static_reader(store; resolution=res)
    grid = static_grid(r)
    @test grid.length == 4
    @test grid.initial_timestamp == t0

    groups = static_groups(r)
    # Order: f64 scalar, f64 [2], i64 scalar.
    @test length(groups) == 3
    @test groups[1].dtype == Float64 && groups[1].element_shape == Int[]
    @test [key_info(k).owner_id for k in groups[1].keys] == [1, 2]
    @test groups[2].dtype == Float64 && groups[2].element_shape == [2]
    @test groups[3].dtype == Int64 && groups[3].element_shape == Int[]

    static_read!(r, t0 + Hour(2))            # index 2 -> Julia row 3
    @test static_values(r, 1) == [12.0, 22.0]
    @test static_values(r, 2)[1, :] == pair[3, :]
    @test static_values(r, 3) == Int64[102]

    # Buffer reuse: a later read overwrites in place.
    static_read!(r, t0 + Hour(3))
    @test static_values(r, 1) == [13.0, 23.0]

    # Off-grid read throws.
    @test_throws TimeSeriesStore.InvalidParameterError static_read!(r, t0 + Minute(30))
end

@testset "ForecastReader: windows incl. multidim element shape" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 2, 1)
    H, count, E = 3, 2, 2
    data = Float64[h * 1000 + c * 10 + e for h in 1:H, c in 1:count, e in 1:E]
    add_time_series!(store, 9, "Gen", Component,
                     Deterministic(t0, Hour(1), Hour(3), Hour(3), count, data, "pf"))

    r = build_forecast_reader(store, Deterministic; resolution=Hour(1))
    tl = forecast_timeline(r)
    @test tl.count == count
    @test tl.interval == Millisecond(Hour(3))
    ents = forecast_entries(r)
    @test length(ents) == 1
    @test ents[1].window_shape == [H, E]

    # Window k (interval 3h): window k == data[:, k+1, :].
    for k in 0:(count - 1)
        forecast_read!(r, t0 + Hour(3) * k)
        @test forecast_values(r, 1) == data[:, k + 1, :]
    end

    @test_throws TimeSeriesStore.InvalidParameterError forecast_read!(r, t0 + Hour(1))
end

@testset "ForecastReader: Deterministic reader includes DST identically" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 3, 1)
    res = Hour(1)
    sts = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0]
    add_time_series!(store, 1, "Gen", Component, SingleTimeSeries(t0, res, sts, "load"))
    # Deterministic whose windows match the DST: value[s, k] = sts[k + s]; shape (H=2, count=5).
    det = Float64[sts[k + s] for s in 0:1, k in 1:5]
    add_time_series!(store, 2, "Gen", Component, Deterministic(t0, res, Hour(2), res, 5, det, "gen"))
    transform_single_time_series!(store, Hour(2), res)

    r = build_forecast_reader(store, Deterministic; resolution=res)
    @test forecast_timeline(r).count == 5
    ents = forecast_entries(r)
    @test length(ents) == 2
    types = [key_info(e.key).time_series_type for e in ents]
    @test DeterministicSingleTimeSeries in types
    @test Deterministic in types

    for k in 0:4
        forecast_read!(r, t0 + Hour(k))
        w_dst = forecast_values(r, 1)
        w_det = forecast_values(r, 2)
        @test w_dst == w_det
        @test w_dst == [sts[k + 1], sts[k + 2]]
    end
end

@testset "ForecastReader: shared forecast reads once per timestamp" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 5, 1)
    res = Hour(1)
    H, count = 2, 3
    shared = Float64[s * 10 + k for s in 0:(H - 1), k in 1:count]
    other = shared .+ 100.0
    # Owners 1-3 add byte-identical data (content-addressed -> one array);
    # owner 4 is distinct.
    for owner in (1, 2, 3)
        add_time_series!(store, owner, "Gen", Component,
                         Deterministic(t0, res, Hour(H), res, count, shared, "pf"))
    end
    add_time_series!(store, 4, "Gen", Component,
                     Deterministic(t0, res, Hour(H), res, count, other, "pf"))

    r = build_forecast_reader(store, Deterministic; resolution=res)
    ents = forecast_entries(r)
    # Four components, two unique arrays: four entries, two physical reads.
    @test length(ents) == 4
    @test forecast_num_slots(r) == 2
    # Owners 1-3 share one slot; owner 4 is its own.
    @test ents[1].slot == ents[2].slot == ents[3].slot
    @test ents[4].slot != ents[1].slot

    for k in 0:(count - 1)
        forecast_read!(r, t0 + Hour(k))
        @test forecast_values(r, 1) == forecast_values(r, 2) == forecast_values(r, 3)
        @test forecast_values(r, 1) == shared[:, k + 1]
        @test forecast_values(r, 4) == other[:, k + 1]
    end
end

@testset "Reader reshape matches get_time_series (Julia oracle)" begin
    store = Store(in_memory=true)
    t0 = DateTime(2032, 1, 1)
    res = Hour(1)

    # Static with a 2-D element shape (2, 3): data shape (time=4, 2, 3). The
    # reader reshapes each timestep's element block back to (2, 3); cross-check
    # against get_time_series (which also reshapes dtype/shape-correctly).
    sdata = Float64[t * 100 + a * 10 + b for t in 1:4, a in 1:2, b in 1:3]
    skey = add_time_series!(store, 1, "Gen", Component, SingleTimeSeries(t0, res, sdata, "v"))
    full = get_time_series(store, skey)
    @test full.data == sdata
    r = build_static_reader(store; resolution=res)
    @test static_groups(r)[1].element_shape == [2, 3]
    for i in 0:3
        static_read!(r, t0 + Hour(i))
        vals = static_values(r, 1)                    # (ncols=1, 2, 3)
        @test size(vals) == (1, 2, 3)
        @test vals[1, :, :] == full.data[i + 1, :, :] # reader reshape == get_time_series
    end

    # Probabilistic window reshape (P, H) — count axis (2) removed.
    pstore = Store(in_memory=true)
    P, H, count = 3, 2, 4
    pdata = Float64[p * 1000 + h * 10 + c for p in 1:P, h in 1:H, c in 1:count]
    add_time_series!(pstore, 2, "Gen", Component,
                     Probabilistic(t0, res, Hour(H), Hour(1), count, [0.1, 0.5, 0.9], pdata, "pf"))
    full_p = get_time_series(Probabilistic, pstore, 2, Component, "pf")
    @test full_p.data == pdata
    fr = build_forecast_reader(pstore, Probabilistic; resolution=res)
    @test forecast_entries(fr)[1].window_shape == [P, H]
    for k in 0:(count - 1)
        forecast_read!(fr, t0 + Hour(k))
        w = forecast_values(fr, 1)                    # (P, H)
        @test size(w) == (P, H)
        @test w == full_p.data[:, :, k + 1]
    end
end

@testset "get_time_series(store, key) preserves dtype and shape" begin
    store = Store(in_memory=true)
    t0 = DateTime(2033, 1, 1)
    res = Hour(1)

    # Non-Float64 dtype is preserved (previously forced to Float64).
    k_i = add_time_series!(store, 1, "Gen", Component,
                           SingleTimeSeries(t0, res, Int64[10, 20, 30], "i"))
    si = get_time_series(store, k_i)
    @test eltype(si.data) == Int64
    @test si.data == Int64[10, 20, 30]
    @test typeof(si) == SingleTimeSeries{Int64,1}

    k_f = add_time_series!(store, 2, "Gen", Component,
                           SingleTimeSeries(t0, res, Float32[1.5, 2.5, 3.5], "f"))
    sf = get_time_series(store, k_f)
    @test eltype(sf.data) == Float32
    @test sf.data == Float32[1.5, 2.5, 3.5]
    @test typeof(sf) == SingleTimeSeries{Float32,1}

    k_b = add_time_series!(store, 3, "Gen", Component,
                           SingleTimeSeries(t0, res, Bool[true, false, true, false], "b"))
    sb = get_time_series(store, k_b)
    @test eltype(sb.data) == Bool
    @test sb.data == Bool[true, false, true, false]
    @test typeof(sb) == SingleTimeSeries{Bool,1}

    # Multi-dimensional element shape is reshaped (previously flattened).
    A = Float64[t * 100 + a * 10 + b for t in 1:4, a in 1:2, b in 1:3]  # (4, 2, 3)
    k_m = add_time_series!(store, 4, "Gen", Component, SingleTimeSeries(t0, res, A, "m"))
    sm = get_time_series(store, k_m)
    @test size(sm.data) == (4, 2, 3)
    @test sm.data == A
    @test typeof(sm) == SingleTimeSeries{Float64,3}

    # Int64 multi-dim: both dtype and shape preserved together.
    B = Int64[t * 10 + e for t in 1:3, e in 1:2]  # (3, 2)
    k_im = add_time_series!(store, 5, "Gen", Component, SingleTimeSeries(t0, res, B, "im"))
    sim = get_time_series(store, k_im)
    @test eltype(sim.data) == Int64
    @test size(sim.data) == (3, 2)
    @test sim.data == B
    @test typeof(sim) == SingleTimeSeries{Int64,2}
end

@testset "parametric constructors infer {T,N} and normalize views" begin
    t0 = DateTime(2033, 1, 1)
    res = Hour(1)

    # Inference from the value array's eltype/ndims.
    @test typeof(SingleTimeSeries(t0, res, Float64[1, 2, 3], "f")) == SingleTimeSeries{Float64,1}
    @test typeof(SingleTimeSeries(t0, res, Int32[1 2; 3 4], "i")) == SingleTimeSeries{Int32,2}
    @test typeof(NonSequentialTimeSeries([t0, t0 + res], Float32[1, 2], "n")) ==
          NonSequentialTimeSeries{Float32,1}
    @test typeof(NonSequentialTimeSeries([t0, t0 + res], Int32[1 2; 3 4], "n2")) ==
          NonSequentialTimeSeries{Int32,2}

    # Views/ranges/reshapes normalize to a concrete Array{T,N}.
    base = Float64[1, 2, 3, 4, 5, 6]
    sts_view = SingleTimeSeries(t0, res, view(base, 1:3), "v")
    @test sts_view.data isa Array{Float64,1}
    @test sts_view.data == Float64[1, 2, 3]
    sts_reshaped = SingleTimeSeries(t0, res, reshape(base, 2, 3), "r")
    @test sts_reshaped.data isa Array{Float64,2}

    # Forecast structs infer {T,N} too.
    det = Deterministic(t0, res, Hour(2), Hour(1), 5, Float64[i + s for s in 0:1, i in 1:5], "d")
    @test typeof(det) == Deterministic{Float64,2}
    scen = Scenarios(t0, res, Hour(2), Hour(1), 5, Float32[v for v in 1:(3 * 2 * 5)] |> a -> reshape(a, 3, 2, 5), "s")
    @test typeof(scen) == Scenarios{Float32,3}
end
