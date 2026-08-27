using Test
using Dates
using InfraStore

@testset "InfraStore.jl smoke" begin
    store = Store(in_memory=true)

    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values, "load")

    key = add_time_series!(
        store,
        42,
        "Generator",
        Component,
        ts;
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
    got_attr = get_time_series(
        SingleTimeSeries, store, 42, Component, "load"; features=Dict("model_year" => 2030)
    )
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

    @test_throws InfraStore.NotFoundError get_time_series(store, key)
end

@testset "bulk_read SingleTimeSeries" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)

    keys = TimeSeriesKey[]
    for i in 0:4
        vals = collect((i * 10.0):(i * 10.0 + 11.0))   # length 12, distinct per series
        ts = SingleTimeSeries(initial, resolution, vals, "load")
        push!(keys, add_time_series!(store, i + 1, "Generator", Component, ts).key)
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
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 1, 4), DateTime(2024, 1, 3)]
    series = NonSequentialTimeSeries(timestamps, Int64[10, 20, 30], "events")
    key = add_time_series!(store, 7, "Generator", Component, series; features=nothing)
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == Int64[10, 20, 30]
    @test got.name == "events"
    @test get_counts(store).static_time_series == 1

    # Attribute-addressed read returns the same series.
    got_attr = get_time_series(
        NonSequentialTimeSeries, store, 7, Component, "events"; features=nothing
    )
    @test got_attr.timestamps == timestamps
    @test got_attr.data == Int64[10, 20, 30]
    @test got_attr.name == "events"
    @test has_time_series(
        NonSequentialTimeSeries,
        store,
        7,
        Component,
        "events";
        resolution=nothing,
        features=nothing,
    )
    @test length(list_keys(store; owner_id=7, features=nothing)) == 1
end

@testset "non-sequential N-D + application_data round-trip" begin
    store = Store(in_memory=true)
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 1, 4), DateTime(2024, 1, 3)]
    # A (length, k) per-step element array tagged with an opaque extension payload, as a
    # FunctionData encoding would produce on the InfrastructureSystems.jl side.
    data = Float64[1 2; 3 4; 5 6]
    series = NonSequentialTimeSeries(
        timestamps, data, "curves"; application_data="LinearFunctionData"
    )
    key = add_time_series!(store, 9, "Generator", Component, series)
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == data
    @test got.data isa Array{Float64, 2}
    @test got.application_data == "LinearFunctionData"
    @test got.name == "curves"

    got_attr = get_time_series(NonSequentialTimeSeries, store, 9, Component, "curves")
    @test got_attr.data == data
    @test got_attr.application_data == "LinearFunctionData"
end

@testset "non-sequential application_data round-trips untruncated at any length" begin
    # Regression: the reader once copied `application_data` into a fixed 256-byte buffer,
    # silently truncating longer payloads (and appending a stray NUL). A JSON
    # application_data payload comfortably exceeds that.
    store = Store(in_memory=true)
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 2)]
    long_application_data = "{\"payload\":\"" * "x"^4096 * "\"}"
    series = NonSequentialTimeSeries(
        timestamps,
        Float64[1.0, 2.0],
        "big-application_data";
        application_data=long_application_data,
    )
    key = add_time_series!(store, 11, "Generator", Component, series)
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.application_data == long_application_data
    @test length(got.application_data) == length(long_application_data)
end

@testset "attribute-based metadata + hash access" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)
    values = collect(100.0:123.0)
    ts = SingleTimeSeries(initial, resolution, values, "load")

    owner = 11
    feats = Dict("model_year" => 2030, "scenario" => "high")  # string feature value
    add_time_series!(store, owner, "Generator", Component, ts; features=feats, units="MW")

    @test has_time_series(
        store, owner, Component, "load"; resolution=resolution, features=feats
    )
    @test !has_time_series(
        store,
        owner,
        Component,
        "load";
        resolution=resolution,
        features=Dict("model_year" => 2031),
    )

    meta = get_metadata(
        store, owner, Component, "load"; resolution=resolution, features=feats
    )
    @test meta.initial_timestamp == initial
    @test meta.resolution == Millisecond(resolution)
    @test meta.length == 24
    @test length(meta.data_hash) == 32

    fetched = get_array_by_hash(store, meta.data_hash)
    @test fetched == values

    remove_time_series!(
        store, owner, Component, "load"; resolution=resolution, features=feats
    )
    @test !has_time_series(
        store, owner, Component, "load"; resolution=resolution, features=feats
    )
    @test_throws InfraStore.NotFoundError get_metadata(
        store, owner, Component, "load"; resolution=resolution, features=feats
    )
end

@testset "InfraStore.jl persistent round-trip" begin
    mktempdir() do dir
        path = joinpath(dir, "store.h5")
        let store = Store(in_memory=false, path=path)
            ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0), "load")
            add_time_series!(store, 1, "Generator", Component, ts)
            flush!(store)
            InfraStore.close!(store)
        end

        store = InfraStore.open_store(path; read_only=true)
        try
            counts = get_counts(store)
            @test counts.static_time_series == 1
            meta = get_metadata(store, 1, Component, "load"; resolution=Hour(1))
            @test meta.length == 12
            @test get_array_by_hash(store, meta.data_hash) == collect(1.0:12.0)
        finally
            InfraStore.close!(store)
        end
    end
end

@testset "element-type-parameterized arrays" begin
    store = Store(in_memory=true)
    res = Hour(1)
    t0 = DateTime(2024, 1, 1)

    # Int64 scalar series round-trips with its element type, which for a plain
    # numeric series is just the dtype spelling.
    add_time_series!(
        store,
        1001,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "load"),
    )
    m = get_metadata(store, 1001, Component, "load"; resolution=res)
    @test m.element_type == "i64"
    @test get_array_by_hash(store, m.data_hash, Int64) == Int64[10, 20, 30]

    # Multi-dim element tuple (4 steps × 3 coeffs) round-trips, row-major correct.
    A = Float64[i + j / 10 for i in 1:4, j in 1:3]
    add_time_series!(
        store,
        1002,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, A, "cost"; element_type="quadratic_function"),
    )
    mq = get_metadata(store, 1002, Component, "cost"; resolution=res)
    @test mq.element_type == "quadratic_function"
    # A value read carries the element type back too, so a caller can decode the
    # rows without a second metadata lookup.
    @test get_time_series(
        SingleTimeSeries, store, 1002, Component, "cost"; resolution=res
    ).element_type == "quadratic_function"
    flat = get_array_by_hash(store, mq.data_hash, Float64)
    @test permutedims(reshape(flat, (3, 4)), (2, 1)) == A
end

# ---- Forecast read tests (B3) ---------------------------------------------

@testset "Deterministic forecast round-trip" begin
    # H=4 (horizon=4h, resolution=1h), count=3, interval=1h, scalar values.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(1)
    count = 3
    H = 4
    # Shape [H, count] = [4, 3]; row-major layout.
    # data[h, c] = (h+1)*10 + (c+1)
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]  # Julia (col-maj) shape [4,3]

    key = add_time_series!(
        store,
        100,
        "Generator",
        Component,
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
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(6)   # windows start every 6 hours
    count = 4
    H = 4
    # data[h, c] = h*100 + c  (Julia column-major [H, count])
    data = Float64[h * 100 + c for h in 1:H, c in 1:count]

    add_time_series!(
        store,
        110,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf2"),
    )

    # Select windows 1 and 2 (0-indexed: window 1 = t0+6h, window 2 = t0+12h).
    # Julia 1-indexed: columns 2 and 3.
    win_start = t0 + Hour(6)
    win_end = t0 + Hour(18)   # exclusive; covers windows at +6h and +12h

    fc = get_time_series(
        Deterministic, store, 110, Component, "pf2"; time_range=(win_start, win_end)
    )
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
    t0 = DateTime(2024, 2, 1)
    res = Hour(1)
    hor = Hour(3)
    ivl = Hour(3)
    count = 2
    H = 3
    E = 2
    # Julia array shape (H, count, E) = (3, 2, 2); values are distinguishable.
    data = Float64[h * 1000 + c * 10 + e for h in 1:H, c in 1:count, e in 1:E]

    add_time_series!(
        store,
        120,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_md"),
    )

    fc = get_time_series(Deterministic, store, 120, Component, "pf_md")
    @test size(fc.data) == (H, count, E)
    @test fc.data == data
end

@testset "Probabilistic forecast round-trip" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 3, 1)
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
        store,
        200,
        "Generator",
        Component,
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
    t0 = DateTime(2024, 3, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(4)
    count = 4
    H = 4
    percentiles = [0.25, 0.75]
    P = length(percentiles)
    data = Float64[p * 100 + h * 10 + c for p in 1:P, h in 1:H, c in 1:count]

    add_time_series!(
        store,
        210,
        "Generator",
        Component,
        Probabilistic(t0, res, hor, ivl, count, percentiles, data, "pf_prob_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+4h, end at t0+12h.
    win_start = t0 + Hour(4)
    win_end = t0 + Hour(12)

    fc = get_time_series(
        Probabilistic, store, 210, Component, "pf_prob_win"; time_range=(win_start, win_end)
    )
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test fc.percentiles ≈ percentiles
    @test size(fc.data) == (P, H, 2)
    @test fc.data == data[:, :, 2:3]
end

@testset "Scenarios forecast round-trip" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 4, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(4)
    count = 2
    H = 4
    scenario_count = 3
    # Julia shape (scenario_count, H, count) = (3, 4, 2).
    data = Float64[s * 1000 + h * 10 + c for s in 1:scenario_count, h in 1:H, c in 1:count]

    key = add_time_series!(
        store,
        300,
        "Generator",
        Component,
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
    t0 = DateTime(2024, 4, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(8)
    count = 4
    H = 4
    scenario_count = 2
    data = Float64[s * 100 + h * 10 + c for s in 1:scenario_count, h in 1:H, c in 1:count]

    add_time_series!(
        store,
        310,
        "Generator",
        Component,
        Scenarios(t0, res, hor, ivl, count, data, "pf_scen_win"),
    )

    # Select windows 2 and 3 (Julia columns 2 and 3): start at t0+8h, end at t0+24h.
    win_start = t0 + Hour(8)
    win_end = t0 + Hour(24)

    fc = get_time_series(
        Scenarios, store, 310, Component, "pf_scen_win"; time_range=(win_start, win_end)
    )
    @test fc.initial_timestamp == win_start
    @test fc.count == 2
    @test fc.scenario_count == scenario_count
    @test size(fc.data) == (scenario_count, H, 2)
    @test fc.data == data[:, :, 2:3]
end

@testset "Forecast non-Float64 dtype (Int64)" begin
    # Verify that non-f64 dtypes survive the FFI round-trip for Deterministic.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 5, 1)
    res = Hour(1)
    hor = Hour(2)
    ivl = Hour(2)
    count = 3
    H = 2
    data = Int64[h * 100 + c for h in 1:H, c in 1:count]

    add_time_series!(
        store,
        130,
        "Generator",
        Component,
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
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(2)
    underlying = Float64[i for i in 0:7]

    add_time_series!(
        store, 400, "Generator", Component, SingleTimeSeries(t0, res, underlying, "dst")
    )

    n = transform_single_time_series!(store, hor, ivl).transformed
    @test n == 1

    # Asking for `Deterministic` resolves the stored DST: the transform is a
    # storage detail, not something the caller has to name.
    fc = get_time_series(Deterministic, store, 400, Component, "dst")
    @test fc.count == 3
    @test size(fc.data) == (4, 3)
    @test fc.name == "dst"
    # Row-major [H, C]: out[s, w] = underlying[w*2 + s] (0-indexed).
    expected = Float64[underlying[(w - 1) * 2 + s] for s in 1:4, w in 1:3]
    @test fc.data == expected

    # Naming the derived type explicitly reads the same values — it narrows the
    # query, it does not change the result struct.
    @test get_time_series(
        DeterministicSingleTimeSeries, store, 400, Component, "dst"
    ).data == expected

    # The detail stays inspectable: the resolved key reports the stored type.
    @test key_info(
        get_time_series_key(Deterministic, store, 400, Component, "dst")
    ).time_series_type == DeterministicSingleTimeSeries

    # `AbstractDeterministic` is not part of the public surface.
    @test !isdefined(InfraStore, :AbstractDeterministic)

    # get_time_series_keys enumerates both the source STS and the derived DST;
    # key_info lets us pick the DST and read it back by key (no key from transform).
    keys = get_time_series_keys(store, 400, Component)
    @test length(keys) == 2
    infos = [key_info(k) for k in keys]
    # time_series_type is the actual Julia type, as in InfrastructureSystems.jl.
    @test Set(i.time_series_type for i in infos) ==
        Set([SingleTimeSeries, DeterministicSingleTimeSeries])
    @test all(
        i -> i.owner_id == 400 && i.owner_category == Component && i.name == "dst", infos
    )

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
    @test get_time_series(infos[sts_idx].time_series_type, store, keys[sts_idx]).data ==
        underlying

    # Reference counting lives in the core: the STS and its derived DST share one
    # underlying array, so count_array_references reports one of each. The content
    # hash is physical storage detail, read via the metadata descriptor — it is not
    # carried on a key, so list_keys does not expose it.
    hash = get_metadata(store, 400, Component, "dst").data_hash
    @test count_array_references(store, hash) == ArrayReferenceCounts(1, 1)
end

@testset "list_keys filters (owner, type, name, resolution, features)" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    vals = Float64[1, 2, 3, 4]
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), vals, "load");
        features=Dict("scenario" => "high"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), vals, "load");
        features=Dict("scenario" => "low"),
    )
    add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Minute(5), vals, "wind")
    )
    add_time_series!(
        store, 2, "Bus", SupplementalAttribute, SingleTimeSeries(t0, Hour(1), vals, "load")
    )

    @test length(list_keys(store)) == 4
    @test length(list_keys(store; owner_id=1)) == 3
    @test length(list_keys(store; owner_category=SupplementalAttribute)) == 1
    @test length(list_keys(store; name="load")) == 3
    @test length(list_keys(store; time_series_type=SingleTimeSeries)) == 4
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
    @test Set(get_resolutions(store; time_series_type=SingleTimeSeries)) ==
        Set([Millisecond(Minute(5)), Millisecond(Hour(1))])
    @test isempty(get_resolutions(store; time_series_type=Deterministic))

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

@testset "has_any_time_series covers the list_keys filter surface" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    vals = Float64[1, 2, 3, 4]
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), vals, "load");
        features=Dict("scenario" => "high", "model" => "m1"),
    )
    add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Minute(5), vals, "wind")
    )
    add_time_series!(
        store, 2, "Bus", SupplementalAttribute, SingleTimeSeries(t0, Hour(1), vals, "load")
    )

    @test has_any_time_series(store)
    @test has_any_time_series(store; owner_id=1, owner_category=Component)
    @test !has_any_time_series(store; owner_id=99, owner_category=Component)
    # The category is an independent predicate, not implied by the owner id.
    @test !has_any_time_series(store; owner_id=2, owner_category=Component)
    @test has_any_time_series(store; owner_id=2, owner_category=SupplementalAttribute)
    @test has_any_time_series(store; owner_id=1, owner_category=Component, name="load")
    @test !has_any_time_series(store; owner_id=2, owner_category=Component, name="wind")
    @test has_any_time_series(store; time_series_type=SingleTimeSeries, name="wind")
    @test !has_any_time_series(store; time_series_type=Deterministic)
    @test has_any_time_series(store; name="wind", resolution=Minute(5))
    @test !has_any_time_series(store; name="wind", resolution=Hour(1))
    # A period equal to the stored one matches regardless of spelling.
    @test has_any_time_series(store; name="wind", resolution=Second(300))
    # Features are a subset match: any stored series carrying at least the
    # requested pairs counts, unlike the exact-set has_time_series forms.
    @test has_any_time_series(store; owner_id=1, features=Dict("scenario" => "high"))
    @test has_any_time_series(
        store;
        owner_id=1,
        features=Dict("scenario" => "high", "model" => "m1"),
    )
    @test !has_any_time_series(store; owner_id=1, features=Dict("scenario" => "mid"))
    # No-features filter matches featured and featureless rows alike.
    @test has_any_time_series(store; owner_id=1, name="load")
end

@testset "list_array_groups annotates rows with the content hash" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    shared = Float64[1, 2, 3, 4]
    # Two owners with identical data dedup to one array (one hash); a third owner's
    # distinct data gets a different hash.
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), copy(shared), "load"),
    )
    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), copy(shared), "load"),
    )
    add_time_series!(
        store,
        3,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[9, 8, 7, 6], "load"),
    )

    rows = list_array_groups(store)
    @test length(rows) == 3
    # Rows carry every list_keys field plus the 32-byte content hash.
    @test all(r -> r.data_hash isa Vector{UInt8} && length(r.data_hash) == 32, rows)
    @test all(r -> r.name == "load", rows)

    # A Vector{UInt8} hashes by content, so it groups directly as a Dict key.
    groups = Dict{Vector{UInt8}, Vector{Int}}()
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
    add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), v, "load")
    )
    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[5, 6, 7, 8], "load"),
    )
    add_time_series!(
        store, 9, "Bus", SupplementalAttribute, SingleTimeSeries(t0, Hour(1), v, "voltage")
    )  # shares content with owner 1
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Deterministic(
            t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"
        ),
    )

    c = time_series_counts(store)
    @test c.components_with_time_series == 2            # owners 1, 2
    @test c.supplemental_attributes_with_time_series == 1  # owner 9
    @test c.static_time_series_count == 2               # [1,2,3,4] (x2 shared) + [5,6,7,8]
    @test c.forecast_count == 1                         # one Deterministic array

    @test sort!(list_owner_ids(store, Component)) == [1, 2]
    @test list_owner_ids(store, SupplementalAttribute) == [9]
    @test list_owner_ids(store, Component; time_series_type=Deterministic) == [1]
    @test sort!(list_owner_ids(store, Component; resolution=Hour(1))) == [1, 2]
    @test isempty(list_owner_ids(store, Component; resolution=Minute(5)))
end

@testset "check_static_consistency and filtered get_forecast_parameters" begin
    store = Store(in_memory=true)
    @test isempty(check_static_consistency(store))

    t0 = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "a"),
    )
    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[5, 6, 7, 8], "a"),
    )
    cs = check_static_consistency(store)
    @test length(cs) == 1
    @test cs[1].resolution == Millisecond(Hour(1))
    @test cs[1].initial_timestamp == t0
    @test cs[1].length == 4

    # A second resolution is a distinct grid, not an inconsistency.
    add_time_series!(
        store,
        4,
        "Generator",
        Component,
        SingleTimeSeries(t0, Minute(30), Float64[1, 2, 3, 4, 5, 6, 7, 8], "a"),
    )
    multi = check_static_consistency(store)
    @test length(multi) == 2
    @test Set(g.resolution for g in multi) ==
        Set([Millisecond(Hour(1)), Millisecond(Minute(30))])
    # Scoping to one resolution returns only that grid.
    hourly = check_static_consistency(store; resolution=Hour(1))
    @test length(hourly) == 1
    @test hourly[1].length == 4

    # A differing length at an existing resolution is an inconsistency.
    add_time_series!(
        store,
        3,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], "a"),
    )
    @test_throws InfraStore.IntegrityError check_static_consistency(store)
    # The other resolution's grid still checks out on its own.
    ok = check_static_consistency(store; resolution=Minute(30))
    @test length(ok) == 1 && ok[1].length == 8

    # Filtered forecast parameters.
    fstore = Store(in_memory=true)
    add_time_series!(
        fstore,
        1,
        "Generator",
        Component,
        Deterministic(
            t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"
        ),
    )
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
    add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), v, "load")
    )
    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[9, 9, 9, 9], "load"),
    )
    add_time_series!(
        store, 1, "Generator", Component, SingleTimeSeries(t0, Hour(1), v, "wind")
    )  # separate group (name)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Deterministic(
            t0, Hour(1), Hour(2), Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "fc"
        ),
    )

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

@testset "Deterministic resolution: miss and ambiguity" begin
    # Resolution happens in the core; a real miss is not masked by a
    # guess-and-retry fallback.
    store = Store(in_memory=true)
    @test_throws InfraStore.NotFoundError get_time_series(
        Deterministic, store, 999, Component, "nope"
    )

    # Two Deterministic forecasts of one variable at the same resolution but
    # different intervals (e.g. day-ahead vs intra-day). Interval is part of the
    # identity, so both coexist; a read that does not pin the interval is
    # ambiguous, and pinning it disambiguates.
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(2)
    add_time_series!(
        store,
        401,
        "Generator",
        Component,
        Deterministic(t0, res, hor, Hour(1), 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )
    add_time_series!(
        store,
        401,
        "Generator",
        Component,
        Deterministic(
            t0, res, hor, Hour(6), 2, reshape(Float64[10, 11, 12, 13], 2, 2), "dup"
        ),
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic, store, 401, Component, "dup"
    )
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
        store,
        401,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[i for i in 0:7], "dup"),
    )
    transform_single_time_series!(store, hor, ivl)
    @test_throws InfraStore.InvalidParameterError add_time_series!(
        store,
        401,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )

    # The reverse: deriving a DST when a Deterministic of the same family exists.
    store2 = Store(in_memory=true)
    add_time_series!(
        store2,
        401,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, 2, reshape(Float64[0, 1, 2, 3], 2, 2), "dup"),
    )
    add_time_series!(
        store2,
        401,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[i for i in 0:7], "dup"),
    )
    @test_throws InfraStore.InvalidParameterError transform_single_time_series!(
        store2, hor, ivl
    )
end

@testset "get_time_series_keys empty owner" begin
    store = Store(in_memory=true)
    @test get_time_series_keys(store, 999, Component) == InfraStore.TimeSeriesKey[]
end

@testset "key_info exposes attributes and features" begin
    store = Store(in_memory=true)
    res = Hour(1)
    feats = Dict("model_year" => 2030, "scenario" => "high", "active" => true)
    add_time_series!(
        store,
        500,
        "Generator",
        Component,
        SingleTimeSeries(DateTime(2024, 1, 1), res, collect(1.0:6.0), "load");
        features=feats,
    )

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
    @test get_time_series(
        info.time_series_type,
        store,
        info.owner_id,
        info.owner_category,
        info.name;
        resolution=info.resolution,
        features=info.features,
    ).data == collect(1.0:6.0)
end

@testset "get_forecast_parameters" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(4)
    ivl = Hour(2)
    count = 3
    H = 4
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]
    add_time_series!(
        store,
        100,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf"),
    )

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
    for (compression, level, shuffle) in
        [(:none, 3, true), (:deflate, 9, false), (:deflate, 1, true)]
        mktempdir() do dir
            path = joinpath(dir, "store.h5")
            let store = Store(
                    in_memory=false,
                    path=path;
                    compression=compression,
                    compression_level=level,
                    shuffle=shuffle,
                )
                ts = SingleTimeSeries(
                    DateTime(2024, 1, 1), Hour(1), collect(1.0:12.0), "load"
                )
                add_time_series!(store, 1, "Generator", Component, ts)
                flush!(store)
                InfraStore.close!(store)
            end
            # Reopen read-write and append, exercising the restored policy.
            let store = InfraStore.open_store(path; read_only=false)
                ts2 = SingleTimeSeries(
                    DateTime(2024, 1, 1), Hour(1), collect(13.0:24.0), "load"
                )
                add_time_series!(store, 2, "Generator", Component, ts2)
                flush!(store)
                InfraStore.close!(store)
            end
            store = InfraStore.open_store(path; read_only=true)
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
                InfraStore.close!(store)
            end
        end
    end

    @test get_compression(Store(in_memory=true)).compression == :none
    @test_throws ArgumentError Store(in_memory=true, compression=:lz4)
end

@testset "get_path" begin
    # In-memory stores have no backing path.
    @test get_path(Store(in_memory=true)) === nothing

    mktempdir() do dir
        path = joinpath(dir, "store.h5")
        store = Store(in_memory=false, path=path)
        try
            @test get_path(store) == path
        finally
            InfraStore.close!(store)
        end
        # A reopened store reports the path it was opened with.
        reopened = open_store(path; read_only=true)
        try
            @test get_path(reopened) == path
        finally
            InfraStore.close!(reopened)
        end
    end
end

@testset "AddBatch bulk add" begin
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1)
    resolution = Hour(1)

    batch = AddBatch()
    @test length(batch) == 0
    for i in 1:10
        ts = SingleTimeSeries(initial, resolution, collect(Float64.(i:(i + 23))), "load")
        add_time_series!(
            batch, i, "Generator", Component, ts; features=Dict("scenario" => i), units="MW"
        )
    end
    # A forecast and a non-sequential series in the same batch.
    det_data = reshape(collect(1.0:12.0), 3, 4)  # canonical shape (H=3, count=4)
    det = Deterministic(initial, Hour(1), Hour(3), Hour(1), 4, det_data, "fc")
    add_time_series!(batch, 600, "Generator", Component, det)
    ns = NonSequentialTimeSeries(
        [DateTime(2024, 1, 1), DateTime(2024, 1, 2)], Int64[1, 2], "events"
    )
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
    @test_throws InfraStore.DuplicateTimeSeriesError add_time_series_bulk!(store, batch)
    @test length(batch) == 0
    @test isempty(get_time_series_keys(store, 800, Component))

    # The batch is reusable after a failed submit.
    add_time_series!(batch, 800, "Generator", Component, ts)
    keys = add_time_series_bulk!(store, batch)
    @test length(keys) == 1
end

@testset "reserved feature names are rejected on add" begin
    store = Store(in_memory=true)
    ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load")

    for name in ("name", "resolution", "owner_id", "application_data")
        @test_throws InfraStore.InvalidParameterError add_time_series!(
            store, 900, "Generator", Component, ts;
            features=Dict("model_year" => 2030, name => "shadowed"),
        )
    end
    @test isempty(get_time_series_keys(store, 900, Component))

    # A rejected item rolls the whole batch back.
    batch = AddBatch()
    add_time_series!(batch, 901, "Generator", Component, ts)
    add_time_series!(
        batch, 902, "Generator", Component, ts; features=Dict("horizon" => "PT2H")
    )
    @test_throws InfraStore.InvalidParameterError add_time_series_bulk!(store, batch)
    @test isempty(get_time_series_keys(store, 901, Component))

    # Exact, case-sensitive: a near miss is an ordinary feature.
    features = Dict{String, Any}("Name" => "load", "resolution_hours" => 1)
    key = add_time_series!(store, 903, "Generator", Component, ts; features=features)
    @test key_info(key).features == features
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
        store,
        owner_id,
        "Generator",
        Component,
        SingleTimeSeries(initial, resolution, comp_vals, "load"),
    )
    supp_key = add_time_series!(
        store,
        owner_id,
        "Outage",
        SupplementalAttribute,
        SingleTimeSeries(initial, resolution, supp_vals, "load"),
    )

    # Two distinct owners despite the shared numeric id.
    @test get_counts(store).components_with_time_series == 2

    # Each category reads back its own data, key-based...
    @test get_time_series(store, comp_key).data == comp_vals
    @test get_time_series(store, supp_key).data == supp_vals

    # ...and attribute-addressed, keyed on the category.
    @test get_time_series(
        SingleTimeSeries, store, owner_id, Component, "load"; resolution=resolution
    ).data == comp_vals
    @test get_time_series(
        SingleTimeSeries,
        store,
        owner_id,
        SupplementalAttribute,
        "load";
        resolution=resolution,
    ).data == supp_vals

    # has_time_series / get_metadata are independent per category.
    @test has_time_series(store, owner_id, Component, "load"; resolution=resolution)
    @test has_time_series(
        store, owner_id, SupplementalAttribute, "load"; resolution=resolution
    )

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
    @test has_time_series(
        store, owner_id, SupplementalAttribute, "load"; resolution=resolution
    )
    @test get_time_series(store, supp_key).data == supp_vals
    @test isempty(get_time_series_keys(store, owner_id, Component))
    @test length(get_time_series_keys(store, owner_id, SupplementalAttribute)) == 1
end

@testset "StaticReader: columnar reads across dtypes and shapes" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 1, 1)
    res = Hour(1)
    add_time_series!(
        store,
        2,
        "Gen",
        Component,
        SingleTimeSeries(t0, res, [20.0, 21.0, 22.0, 23.0], "load"),
    )
    add_time_series!(
        store,
        1,
        "Gen",
        Component,
        SingleTimeSeries(t0, res, [10.0, 11.0, 12.0, 13.0], "load"),
    )
    add_time_series!(
        store,
        3,
        "Gen",
        Component,
        SingleTimeSeries(t0, res, Int64[100, 101, 102, 103], "count"),
    )
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
    @test_throws InfraStore.InvalidParameterError static_read!(r, t0 + Minute(30))

    # A regular grid enumerates the same way an irregular timeline does.
    @test static_timestamps(r) == [t0 + Hour(k) for k in 0:3]
    @test grid.resolution == res
end

@testset "static reader over an irregular cohort" begin
    store = Store(in_memory=true)
    t0 = DateTime(2030, 1, 1)
    # Three instants with no constant step between them; two components observe
    # the same ones, so they share a time axis (and one packed dataset).
    stamps = [t0, t0 + Minute(37), t0 + Hour(9)]
    add_time_series!(
        store,
        2,
        "Gen",
        Component,
        NonSequentialTimeSeries(stamps, [20.0, 21.0, 22.0], "outage"),
    )
    add_time_series!(
        store,
        1,
        "Gen",
        Component,
        NonSequentialTimeSeries(stamps, [10.0, 11.0, 12.0], "outage"),
    )

    r = build_static_reader(store; time_series_type=NonSequentialTimeSeries)
    grid = static_grid(r)
    @test grid.length == 3
    @test grid.initial_timestamp == t0
    # No constant step to report; the instants come from `static_timestamps`.
    @test grid.resolution === nothing
    @test static_timestamps(r) == stamps

    groups = static_groups(r)
    @test length(groups) == 1
    @test [key_info(k).owner_id for k in groups[1].keys] == [1, 2]

    for (i, t) in enumerate(stamps)
        static_read!(r, t)
        @test static_values(r, 1) == [9.0 + i, 19.0 + i]
    end

    # Between two instants there is no value, so the read throws rather than
    # picking a neighbour.
    @test_throws InfraStore.InvalidParameterError static_read!(r, t0 + Minute(1))
    # A resolution filter makes no sense for an irregular reader.
    @test_throws InfraStore.InvalidParameterError build_static_reader(
        store; time_series_type=NonSequentialTimeSeries, resolution=Hour(1)
    )
    # And a series on a different axis cannot join the cohort.
    add_time_series!(
        store,
        3,
        "Gen",
        Component,
        NonSequentialTimeSeries(
            [t0, t0 + Minute(38), t0 + Hour(9)], [1.0, 2.0, 3.0], "outage"
        ),
    )
    @test_throws InfraStore.InvalidParameterError build_static_reader(
        store; time_series_type=NonSequentialTimeSeries
    )
end

@testset "ForecastReader: windows incl. multidim element shape" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 2, 1)
    H, count, E = 3, 2, 2
    data = Float64[h * 1000 + c * 10 + e for h in 1:H, c in 1:count, e in 1:E]
    add_time_series!(
        store,
        9,
        "Gen",
        Component,
        Deterministic(t0, Hour(1), Hour(3), Hour(3), count, data, "pf"),
    )

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

    @test_throws InfraStore.InvalidParameterError forecast_read!(r, t0 + Hour(1))
end

@testset "ForecastReader: Deterministic reader includes DST identically" begin
    store = Store(in_memory=true)
    t0 = DateTime(2031, 3, 1)
    res = Hour(1)
    sts = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0]
    add_time_series!(store, 1, "Gen", Component, SingleTimeSeries(t0, res, sts, "load"))
    # Deterministic whose windows match the DST: value[s, k] = sts[k + s]; shape (H=2, count=5).
    det = Float64[sts[k + s] for s in 0:1, k in 1:5]
    add_time_series!(
        store, 2, "Gen", Component, Deterministic(t0, res, Hour(2), res, 5, det, "gen")
    )
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
        add_time_series!(
            store,
            owner,
            "Gen",
            Component,
            Deterministic(t0, res, Hour(H), res, count, shared, "pf"),
        )
    end
    add_time_series!(
        store, 4, "Gen", Component, Deterministic(t0, res, Hour(H), res, count, other, "pf")
    )

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
    skey = add_time_series!(
        store, 1, "Gen", Component, SingleTimeSeries(t0, res, sdata, "v")
    )
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
    add_time_series!(
        pstore,
        2,
        "Gen",
        Component,
        Probabilistic(t0, res, Hour(H), Hour(1), count, [0.1, 0.5, 0.9], pdata, "pf"),
    )
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
    k_i = add_time_series!(
        store, 1, "Gen", Component, SingleTimeSeries(t0, res, Int64[10, 20, 30], "i")
    )
    si = get_time_series(store, k_i)
    @test eltype(si.data) == Int64
    @test si.data == Int64[10, 20, 30]
    @test typeof(si) == SingleTimeSeries{Int64, 1}

    k_f = add_time_series!(
        store, 2, "Gen", Component, SingleTimeSeries(t0, res, Float32[1.5, 2.5, 3.5], "f")
    )
    sf = get_time_series(store, k_f)
    @test eltype(sf.data) == Float32
    @test sf.data == Float32[1.5, 2.5, 3.5]
    @test typeof(sf) == SingleTimeSeries{Float32, 1}

    k_b = add_time_series!(
        store,
        3,
        "Gen",
        Component,
        SingleTimeSeries(t0, res, Bool[true, false, true, false], "b"),
    )
    sb = get_time_series(store, k_b)
    @test eltype(sb.data) == Bool
    @test sb.data == Bool[true, false, true, false]
    @test typeof(sb) == SingleTimeSeries{Bool, 1}

    # Multi-dimensional element shape is reshaped (previously flattened).
    A = Float64[t * 100 + a * 10 + b for t in 1:4, a in 1:2, b in 1:3]  # (4, 2, 3)
    k_m = add_time_series!(store, 4, "Gen", Component, SingleTimeSeries(t0, res, A, "m"))
    sm = get_time_series(store, k_m)
    @test size(sm.data) == (4, 2, 3)
    @test sm.data == A
    @test typeof(sm) == SingleTimeSeries{Float64, 3}

    # Int64 multi-dim: both dtype and shape preserved together.
    B = Int64[t * 10 + e for t in 1:3, e in 1:2]  # (3, 2)
    k_im = add_time_series!(store, 5, "Gen", Component, SingleTimeSeries(t0, res, B, "im"))
    sim = get_time_series(store, k_im)
    @test eltype(sim.data) == Int64
    @test size(sim.data) == (3, 2)
    @test sim.data == B
    @test typeof(sim) == SingleTimeSeries{Int64, 2}
end

@testset "parametric constructors infer {T,N} and normalize views" begin
    t0 = DateTime(2033, 1, 1)
    res = Hour(1)

    # Inference from the value array's eltype/ndims.
    @test typeof(SingleTimeSeries(t0, res, Float64[1, 2, 3], "f")) ==
        SingleTimeSeries{Float64, 1}
    @test typeof(SingleTimeSeries(t0, res, Int32[1 2; 3 4], "i")) ==
        SingleTimeSeries{Int32, 2}
    @test typeof(NonSequentialTimeSeries([t0, t0 + res], Float32[1, 2], "n")) ==
        NonSequentialTimeSeries{Float32, 1}
    @test typeof(NonSequentialTimeSeries([t0, t0 + res], Int32[1 2; 3 4], "n2")) ==
        NonSequentialTimeSeries{Int32, 2}

    # Views/ranges/reshapes normalize to a concrete Array{T,N}.
    base = Float64[1, 2, 3, 4, 5, 6]
    sts_view = SingleTimeSeries(t0, res, view(base, 1:3), "v")
    @test sts_view.data isa Array{Float64, 1}
    @test sts_view.data == Float64[1, 2, 3]
    sts_reshaped = SingleTimeSeries(t0, res, reshape(base, 2, 3), "r")
    @test sts_reshaped.data isa Array{Float64, 2}

    # Forecast structs infer {T,N} too.
    det = Deterministic(
        t0, res, Hour(2), Hour(1), 5, Float64[i + s for s in 0:1, i in 1:5], "d"
    )
    @test typeof(det) == Deterministic{Float64, 2}
    scen = Scenarios(
        t0,
        res,
        Hour(2),
        Hour(1),
        5,
        Float32[v for v in 1:(3 * 2 * 5)] |> a -> reshape(a, 3, 2, 5),
        "s",
    )
    @test typeof(scen) == Scenarios{Float32, 3}
end

@testset "quantity_kind, unit_system, and component_field round-trip on every read path" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    sts = SingleTimeSeries(
        t0, res, collect(1.0:4.0), "load";
        units="MW", quantity_kind="ActivePower", unit_system=ComponentBase,
        component_field="max_active_power",
    )
    k = add_time_series!(store, 1, "Generator", Component, sts)

    # The catalog row records both...
    md = get_metadata(store, k)
    @test md.quantity_kind == "ActivePower"
    @test md.unit_system === ComponentBase
    @test md.component_field == "max_active_power"
    @test list_time_series(store; owner_id=1)[1].unit_system === ComponentBase

    # ...and the get path puts them back on the struct, as it already does for
    # `units` -- a descriptor that survived the write but not the read would be
    # worse than one that was never stored.
    got = get_time_series(store, k)
    @test got.quantity_kind == "ActivePower"
    @test got.unit_system === ComponentBase
    @test got.component_field == "max_active_power"

    # The bulk path builds its own struct, so it must agree rather than drop them.
    @test bulk_read(store, [k])[1].unit_system === ComponentBase
    @test bulk_read(store, [k])[1].component_field == "max_active_power"

    # Unset means unspecified, NOT NaturalUnits: nothing declared a basis here.
    bare = SingleTimeSeries(t0, res, collect(1.0:4.0), "bare")
    kb = add_time_series!(store, 2, "Generator", Component, bare)
    @test get_metadata(store, kb).unit_system === nothing
    @test get_metadata(store, kb).component_field === nothing
    @test get_time_series(store, kb).quantity_kind === nothing
    @test get_time_series(store, kb).component_field === nothing

    # A string spelling is accepted and normalized; an unknown one is rejected
    # rather than degrading to `nothing`.
    @test SingleTimeSeries(
        t0, res, collect(1.0:4.0), "s"; unit_system="natural_units"
    ).unit_system === NaturalUnits
    @test_throws ArgumentError SingleTimeSeries(
        t0, res, collect(1.0:4.0), "s"; unit_system="system_base"
    )
end

@testset "component_field filter selects across owners on every filter path" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    for (owner, name, field) in [
        (1, "max_active_power", "max_active_power"),
        (1, "rating", "rating"),
        (2, "max_active_power", "max_active_power"),
        (3, "legacy", nothing),
    ]
        ts = SingleTimeSeries(
            t0, res, collect(1.0:4.0), name; component_field=field
        )
        add_time_series!(store, owner, "Generator", Component, ts)
    end

    # One field, every component that varies it -- and it composes with the
    # owner scope.
    @test sort([
        r.owner_id for r in list_keys(store; component_field="max_active_power")
    ]) ==
        [1, 2]
    @test length(list_keys(store; owner_id=1, component_field="max_active_power")) == 1
    @test length(list_time_series(store; component_field="rating")) == 1

    # Exact and case-sensitive; no glob semantics.
    @test isempty(list_keys(store; component_field="max_active"))
    @test isempty(list_keys(store; component_field="Max_Active_Power"))

    # A row that declares none is unreachable through the filter.
    @test isempty(list_keys(store; component_field="legacy"))

    # The reader filter takes it too -- the columnar sweep case.
    reader = build_static_reader(
        store; resolution=res, component_field="max_active_power"
    )
    @test sum(length(g.keys) for g in reader.groups) == 2
end

@testset "name_glob filter across the catalog and reader surface" begin
    # The core has had `ListFilter::name_glob` since the discovery surface
    # landed, and Python and the CLI both expose it; the C ABI did not, so no
    # Julia caller could reach it and every name-pattern query had to list the
    # store and filter in Julia.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    for (owner, name) in
        [(1, "wind_speed"), (1, "wind_dir"), (2, "solar_ghi"), (3, "Wind_speed")]
        add_time_series!(
            store, owner, "Generator", Component,
            SingleTimeSeries(t0, res, collect(1.0:4.0), name),
        )
    end

    @test sort(list_names(store; name_glob="wind_*")) == ["wind_dir", "wind_speed"]
    @test length(list_keys(store; name_glob="wind_*")) == 2
    # The pattern matches the whole name, so a leading `*` reaches both
    # spellings of the speed series.
    @test length(list_time_series(store; name_glob="*_speed")) == 2
    @test length(list_array_groups(store; name_glob="wind_*")) == 2
    @test list_owner_types(store; name_glob="solar_*") == ["Generator"]
    @test has_any_time_series(store; name_glob="solar_*")
    @test !has_any_time_series(store; name_glob="hydro_*")

    # Case-sensitive, as SQLite GLOB is -- the capitalized row is a different
    # series, not a near miss.
    @test length(list_keys(store; name_glob="Wind*")) == 1

    # Composes with the other filters rather than replacing them.
    @test length(list_keys(store; owner_id=1, name_glob="wind_*")) == 2
    @test isempty(list_keys(store; owner_id=2, name_glob="wind_*"))
    @test length(list_keys(store; name="wind_dir", name_glob="wind_*")) == 1
    @test isempty(list_keys(store; name="solar_ghi", name_glob="wind_*"))

    # The reader builders take it too, so a columnar sweep can be scoped by
    # pattern without listing first.
    reader = build_static_reader(store; resolution=res, name_glob="wind_*")
    @test sum(length(g.keys) for g in reader.groups) == 2

    # And it drives a removal.
    @test remove_by_filter!(store; name_glob="wind_*") == 2
    @test sort(list_names(store)) == ["Wind_speed", "solar_ghi"]
end

@testset "Phase 2 additions: units, time_range, discovery, rename, bulk dispatch" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    # units round-trips through get_metadata (previously write-only).
    sts = SingleTimeSeries(t0, res, collect(1.0:8.0), "load")
    k = add_time_series!(
        store, 1, "Generator", Component, sts; units="MW", application_data="Profile"
    )
    md = get_metadata(store, 1, Component, "load"; resolution=res)
    @test md.units == "MW"
    @test md.application_data == "Profile"

    # time_range slicing on the SingleTimeSeries get path matches a full read.
    full = get_time_series(store, k)
    sliced = get_time_series(store, k; time_range=(t0 + Hour(2), t0 + Hour(5)))
    @test sliced.data == full.data[3:5]
    @test sliced.initial_timestamp == t0 + Hour(2)

    # read_only reflects the store mode.
    @test read_only(store) == false

    # A forecast so discovery has an interval + a mixed bulk read.
    det = Deterministic(
        t0, res, Hour(2), Hour(1), 3, Float64[h * 10 + c for h in 1:2, c in 1:3], "fc"
    )
    kf = add_time_series!(store, 2, "Bus", Component, det)

    @test get_intervals(store) == [Hour(1)]
    @test isempty(get_intervals(store; time_series_type=SingleTimeSeries))
    @test sort(list_names(store)) == ["fc", "load"]
    @test list_names(store; owner_id=1) == ["load"]
    @test sort(list_owner_types(store)) == ["Bus", "Generator"]

    # Full metadata rows include units + application_data.
    rows = list_time_series(store; owner_id=1)
    @test length(rows) == 1
    @test rows[1].units == "MW"
    @test rows[1].application_data == "Profile"
    @test rows[1].element_type == "f64"

    # Probabilistic metadata exposes percentiles + units without a data fetch.
    prob = Probabilistic(
        t0,
        res,
        Hour(2),
        Hour(1),
        3,
        [0.1, 0.5, 0.9],
        Float64[p + h + c for p in 1:3, h in 1:2, c in 1:3],
        "pf",
    )
    add_time_series!(store, 3, "Generator", Component, prob; units="MWp")
    pmd = get_metadata(Probabilistic, store, 3, Component, "pf")
    @test pmd.percentiles == [0.1, 0.5, 0.9]
    @test pmd.units == "MWp"

    # bulk_read dispatches on stored type.
    mixed = bulk_read(store, TimeSeriesKey[k.key, kf.key])
    @test mixed[1] isa SingleTimeSeries
    @test mixed[1].data == full.data
    @test mixed[2] isa Deterministic
    @test mixed[2].data == det.data

    # get_time_series_key resolves a Deterministic request.
    rk = get_time_series_key(Deterministic, store, 2, Component, "fc")
    @test get_time_series(Deterministic, store, rk).data == det.data

    # rename_time_series! moves the association.
    nk = rename_time_series!(store, k, "load2")
    @test get_metadata(store, 1, Component, "load2"; resolution=res).units == "MW"
    @test_throws InfraStore.NotFoundError get_metadata(
        store, 1, Component, "load"; resolution=res
    )

    # remove_by_filter! removes matching series.
    removed = remove_by_filter!(store; owner_id=3)
    @test removed == 1
    @test isempty(list_names(store; owner_id=3))
end

@testset "Round-2 ABI: metadata element_shape/features, bulk remove" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    feats = Dict("scenario" => "high", "model_year" => 2030)

    # Static series with per-step element shape (2,): data dims (time=4, 2).
    mts = SingleTimeSeries(t0, res, reshape(collect(1.0:8.0), 4, 2), "flow")
    add_time_series!(store, 1, "Line", Component, mts; features=feats)
    md = get_metadata(store, 1, Component, "flow"; resolution=res, features=feats)
    @test md.element_shape == (2,)
    @test md.features == Dict("scenario" => "high", "model_year" => 2030)

    # Scalar series reports an empty shape and empty features.
    sts = SingleTimeSeries(t0, res, collect(1.0:4.0), "load")
    add_time_series!(store, 1, "Line", Component, sts)
    md0 = get_metadata(store, 1, Component, "load"; resolution=res)
    @test md0.element_shape == ()
    @test isempty(md0.features)

    # Forecast metadata carries the element shape + features too. The catalog's
    # element_shape is the stored array's trailing dims after its first axis:
    # a Deterministic with dims (H=2, count=3, E=2) reports (3, 2).
    det = Deterministic(
        t0, res, Hour(2), Hour(1), 3, reshape(collect(1.0:12.0), 2, 3, 2), "fc"
    )
    add_time_series!(store, 2, "Bus", Component, det; features=feats)
    fmd = get_metadata(Deterministic, store, 2, Component, "fc"; features=feats)
    @test fmd.element_shape == (3, 2)
    @test fmd.features == Dict("scenario" => "high", "model_year" => 2030)

    # Probabilistic dims (P=2, H=2, count=3) report trailing dims (2, 3).
    prob = Probabilistic(
        t0,
        res,
        Hour(2),
        Hour(1),
        3,
        [0.1, 0.9],
        Float64[p + h + c for p in 1:2, h in 1:2, c in 1:3],
        "pf",
    )
    add_time_series!(store, 3, "Generator", Component, prob)
    pmd = get_metadata(Probabilistic, store, 3, Component, "pf")
    @test pmd.element_shape == (2, 3)
    @test isempty(pmd.features)
    @test pmd.percentiles == [0.1, 0.9]

    # Bulk remove: all-or-nothing.
    keys = get_time_series_keys(store, 1, Component)
    @test length(keys) == 2
    @test remove_time_series!(store, keys) == 2
    @test isempty(get_time_series_keys(store, 1, Component))

    # Rollback: one already-removed key aborts the whole batch.
    kf = get_time_series_keys(store, 2, Component)[1]
    kp = get_time_series_keys(store, 3, Component)[1]
    @test remove_time_series!(store, [kf]) == 1
    @test_throws InfraStore.NotFoundError remove_time_series!(store, [kp, kf])
    @test has_time_series(store, kp)

    close!(store)
end

@testset "Round-2 Julia idioms: Base interface, do-block, time_range, persist!" begin
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    # Do-block form closes the store, even on throw.
    captured = Ref{Any}(nothing)
    result = Store(in_memory=true) do store
        captured[] = store
        add_time_series!(
            store,
            1,
            "Generator",
            Component,
            SingleTimeSeries(t0, res, collect(1.0:8.0), "load"),
        )
        42
    end
    @test result == 42
    @test captured[].handle == C_NULL
    @test_throws ErrorException Store(in_memory=true) do store
        captured[] = store
        error("boom")
    end
    @test captured[].handle == C_NULL

    store = Store(in_memory=true)
    sts = SingleTimeSeries(t0, res, collect(1.0:8.0), "load")
    k = add_time_series!(store, 1, "Generator", Component, sts).key

    # time_range on the typed key alias and the attribute-addressed forms.
    sliced = get_time_series(
        SingleTimeSeries, store, k; time_range=(t0 + Hour(2), t0 + Hour(5))
    )
    @test sliced.data == collect(3.0:5.0)
    sliced = get_time_series(
        SingleTimeSeries,
        store,
        1,
        Component,
        "load";
        resolution=res,
        time_range=(t0 + Hour(2), t0 + Hour(5)),
    )
    @test sliced.data == collect(3.0:5.0)

    nsts = NonSequentialTimeSeries(
        [t0, t0 + Hour(3), t0 + Hour(7)], [10.0, 20.0, 30.0], "events"
    )
    add_time_series!(store, 2, "Bus", Component, nsts)
    ns_sliced = get_time_series(
        NonSequentialTimeSeries,
        store,
        2,
        Component,
        "events";
        time_range=(t0 + Hour(1), t0 + Hour(5)),
    )
    @test ns_sliced.data == [20.0]

    # Key equality/hash delegate to core identity: separately-fetched keys of
    # the same series are equal, hash equal, and work as Dict keys.
    k2 = get_time_series_keys(store, 1, Component)[1]
    @test k == k2
    @test hash(k) == hash(k2)
    kother = get_time_series_keys(store, 2, Component)[1]
    @test k != kother
    d = Dict(k => "a")
    d[k2] = "b"
    @test length(d) == 1 && d[k] == "b"

    # show forms are compact one-liners.
    @test occursin("name=\"load\"", sprint(show, k))
    @test occursin("read_only=false", sprint(show, store))
    @test occursin("length=8", sprint(show, sts))
    det = Deterministic(t0, res, Hour(2), Hour(1), 3, reshape(collect(1.0:6.0), 2, 3), "fc")
    @test occursin("count=3", sprint(show, det))

    # Container interface delegates to `data`; forecast length = window count.
    @test length(sts) == 8
    @test eltype(typeof(sts)) == Float64
    @test sts[3] == 3.0
    @test collect(sts) == sts.data
    @test length(nsts) == 3
    @test length(det) == 3

    # persist! is exported and materializes an on-disk artifact.
    dir = mktempdir()
    dest = joinpath(dir, "persisted.h5")
    persist!(store, dest)
    @test isfile(dest)
    reopened = open_store(dest; read_only=true)
    @test get_time_series(SingleTimeSeries, store, 1, Component, "load").data ==
        get_time_series(SingleTimeSeries, reopened, 1, Component, "load").data
    close!(reopened)

    # Typed IOError is part of the exception hierarchy.
    @test InfraStore.IOError <: InfraStore.TimeSeriesException

    close!(store)
    @test occursin("closed", sprint(show, store))
end

@testset "catalog placement selects when the catalog reaches disk" begin
    dir = mktempdir()
    scratch = joinpath(dir, "scratch.h5")
    dest = joinpath(dir, "system.h5")
    ts = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load")

    # Arrays stream to the HDF5 file while the catalog stays in RAM, so nothing
    # is addressable on disk until persist!.
    store = Store(; in_memory=false, path=scratch, catalog=:memory)
    @test catalog_mode(store) === :memory
    add_time_series!(store, 1, "Generator", Component, ts)
    flush!(store)
    @test isfile(scratch)
    @test !isfile(scratch * ".sqlite")

    persist!(store, dest)
    @test isfile(dest * ".sqlite")
    close!(store)

    # The saved pair opens as an ordinary attached store.
    open_store(dest; read_only=true) do saved
        @test catalog_mode(saved) === :attached
        @test length(list_keys(saved)) == 1
    end

    # Load back into RAM, mutate, and save over the same destination.
    open_store(dest; catalog=:memory) do loaded
        @test catalog_mode(loaded) === :memory
        add_time_series!(loaded, 2, "Generator", Component, ts)
        persist!(loaded, dest)
    end
    open_store(dest; read_only=true) do saved
        @test length(list_keys(saved)) == 2
    end

    # The default still matches the backend, so existing call sites are unmoved.
    Store(; in_memory=true) do s
        @test catalog_mode(s) === :memory
    end
    Store(; in_memory=false, path=joinpath(dir, "plain.h5")) do s
        @test catalog_mode(s) === :attached
    end

    # Rejected in Julia, before any ccall.
    @test_throws ArgumentError Store(; in_memory=true, catalog=:bogus)
    @test_throws ArgumentError open_store(dest; catalog=:bogus)
end

@testset "Supplemental-attribute associations" begin
    store = Store(in_memory=true)

    attach(component_id, attribute_id) = SupplementalAttributeAssociation(
        component_id, "Generator", attribute_id, "GeographicInfo"
    )

    add_supplemental_attribute_association!(store, attach(1, 100))
    add_supplemental_attribute_association!(store, attach(2, 100))
    @test count_supplemental_attribute_associations(store) == 2
    @test list_supplemental_attribute_associations(store) ==
        [attach(1, 100), attach(2, 100)]

    # Identity is the (component, attribute) pair: re-attaching it under
    # different type names is still a duplicate.
    @test_throws InfraStore.DuplicateAssociationError add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(1, "Load", 100, "Outage")
    )

    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(1, "Generator", 101, "Outage")
    )

    # Filters, including the multi-type IN list IS3 renders after expanding an
    # abstract type, and the empty list that deliberately matches nothing.
    @test length(list_supplemental_attribute_associations(store; component_id=1)) == 2
    @test length(
        list_supplemental_attribute_associations(
            store; attribute_types=["GeographicInfo", "Outage"]
        ),
    ) == 3
    @test isempty(list_supplemental_attribute_associations(store; attribute_types=String[]))
    @test has_supplemental_attribute_association(store; component_id=1, attribute_id=100)
    @test !has_supplemental_attribute_association(store; component_id=7)

    # Distinct ids on either side, and the counts built on them.
    @test list_supplemental_attribute_ids(store; component_id=1) == [100, 101]
    @test list_components_with_attributes(store; attribute_id=100) == [1, 2]
    @test count_supplemental_attributes(store) == 2
    @test count_components_with_attributes(store) == 2

    @test supplemental_attribute_counts_by_type(store) == [
        SupplementalAttributeTypeCount("GeographicInfo", 2),
        SupplementalAttributeTypeCount("Outage", 1),
    ]
    summary = supplemental_attribute_summary(store)
    @test SupplementalAttributeSummaryRow("Generator", "GeographicInfo", 2) in summary
    @test sum(r.count for r in summary) == 3

    # Component rewrite, and the collision it can hit.
    @test remove_supplemental_attribute_associations!(store; component_id=2) == 1
    @test replace_supplemental_attribute_component_id!(store, 1, 5) == 2
    @test list_components_with_attributes(store) == [5]
    add_supplemental_attribute_association!(store, attach(6, 100))
    @test_throws InfraStore.DuplicateAssociationError replace_supplemental_attribute_component_id!(
        store, 6, 5
    )

    # Removing nothing is a count of zero, not an error.
    @test remove_supplemental_attribute_associations!(store; component_id=999) == 0

    # Bulk import/export round trip (IS3's from_records/to_records).
    exported = list_supplemental_attribute_associations(store)
    target = Store(in_memory=true)
    @test add_supplemental_attribute_associations!(target, exported) == length(exported)
    @test list_supplemental_attribute_associations(target) == exported

    # Base overloads: structural equality, hash, compact show.
    @test attach(1, 100) == attach(1, 100)
    @test hash(attach(1, 100)) == hash(attach(1, 100))
    @test attach(1, 100) != attach(2, 100)
    @test occursin("Generator 1 <- GeographicInfo 100", sprint(show, attach(1, 100)))

    close!(target)
    close!(store)
end

@testset "Parent/child associations" begin
    store = Store(in_memory=true)

    edge(parent_id, child_id) =
        ParentChildAssociation(parent_id, "Generator", child_id, "Bus")

    add_parent_child_association!(store, edge(1, 7))
    add_parent_child_association!(store, edge(2, 7))
    @test count_parent_child_associations(store) == 2
    @test list_parent_child_associations(store) == [edge(1, 7), edge(2, 7)]

    # Identity is the ordered pair, so the reverse is a different edge but a
    # repeat under different type names is not.
    @test_throws InfraStore.DuplicateAssociationError add_parent_child_association!(
        store, ParentChildAssociation(1, "Load", 7, "Area")
    )
    add_parent_child_association!(store, ParentChildAssociation(7, "Bus", 1, "Generator"))
    @test count_parent_child_associations(store) == 3

    add_parent_child_association!(store, edge(1, 8))
    @test list_children(store; parent_id=1) == [7, 8]
    @test list_parents(store; child_id=7) == [1, 2]
    @test has_parent_child_association(store; parent_id=1, child_id=8)
    @test !has_parent_child_association(store; parent_id=99)
    @test length(list_parent_child_associations(store; parent_types=["Bus"])) == 1
    @test isempty(list_parent_child_associations(store; child_types=String[]))

    # Rewriting a component id touches both ends of every edge.
    @test remove_parent_child_associations!(store; parent_types=["Bus"]) == 1
    @test replace_parent_child_component_id!(store, 1, 5) == 2
    @test list_parents(store) == [2, 5]
    @test remove_parent_child_associations!(store; parent_id=999) == 0

    # Bulk round trip and the show/equality overloads.
    exported = list_parent_child_associations(store)
    target = Store(in_memory=true)
    @test add_parent_child_associations!(target, exported) == length(exported)
    @test list_parent_child_associations(target) == exported
    @test hash(edge(1, 7)) == hash(edge(1, 7))
    @test occursin("Generator 1 -> Bus 7", sprint(show, edge(1, 7)))

    # Both catalogs survive persist!/open_store, and a read-only store rejects
    # writes while still serving reads.
    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(5, "Generator", 100, "GeographicInfo")
    )
    dir = mktempdir()
    dest = joinpath(dir, "assoc.h5")
    persist!(store, dest)
    reopened = open_store(dest; read_only=true)
    @test list_parent_child_associations(reopened) == exported
    @test count_supplemental_attribute_associations(reopened) == 1
    @test_throws InfraStore.ReadOnlyStoreError add_parent_child_association!(
        reopened, edge(9, 900)
    )
    close!(reopened)

    close!(target)
    close!(store)
end

@testset "is_empty" begin
    # Emptiness must account for every persistent table, not just the ones the
    # caller happens to know about: IS skips writing the artifact entirely when
    # the store reports empty, so anything the predicate misses is dropped with
    # no error. Each catalog is therefore held in isolation.
    store = Store(in_memory=true)
    @test is_empty(store)

    key = add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(DateTime(2030, 1, 1), Hour(1), collect(1.0:4.0), "load"),
    )
    @test !is_empty(store)
    remove_time_series!(store, key)
    @test is_empty(store)

    add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo")
    )
    @test !is_empty(store)
    @test remove_supplemental_attribute_associations!(store) == 1
    @test is_empty(store)

    # The case a client-side conjunction over the other two tables gets wrong.
    add_parent_child_association!(
        store, ParentChildAssociation(1, "Generator", 7, "Bus")
    )
    @test !is_empty(store)
    @test remove_parent_child_associations!(store) == 1
    @test is_empty(store)

    # Emptiness survives persist!/open_store in both directions.
    dir = mktempdir()
    dest = joinpath(dir, "empty.h5")
    persist!(store, dest)
    reopened = open_store(dest; read_only=true)
    @test is_empty(reopened)
    close!(reopened)

    add_parent_child_association!(
        store, ParentChildAssociation(1, "Generator", 7, "Bus")
    )
    dest2 = joinpath(dir, "edges.h5")
    persist!(store, dest2)
    populated = open_store(dest2; read_only=true)
    @test !is_empty(populated)
    close!(populated)

    close!(store)
end

# ---- OpenAPI-row association serde ------------------------------------------
#
# `export_time_series_associations_openapi`/`export_supplemental_attribute_associations_openapi`/
# `import_supplemental_attribute_associations_openapi!` wrap the three Rust
# core `openapi` methods. The golden tests below reproduce two of the
# checked-in fixtures at `conformance/openapi_row_fixtures/` (the core's own
# golden tests pin the rest).

const _OPENAPI_FIXTURES_DIR = normpath(
    joinpath(@__DIR__, "..", "..", "..", "conformance", "openapi_row_fixtures")
)

function _openapi_fixture(name)
    return InfraStore.JSON.parse(
        read(joinpath(_OPENAPI_FIXTURES_DIR, "$name.json"), String)
    )
end

@testset "OpenAPI-row association serde" begin
    @testset "export_time_series_associations_openapi reproduces the single_time_series fixture" begin
        store = Store(in_memory=true)
        single = SingleTimeSeries(
            DateTime(2030, 1, 1), Hour(1), fill(0.0, 8760), "max_active_power";
            units="MW", quantity_kind="ActivePower", unit_system=NaturalUnits,
            component_field="max_active_power",
        )
        add_time_series!(
            store, 7, "ThermalStandard", Component, single;
            features=Dict("scenario" => "high_load", "year" => 2030),
        )

        json = export_time_series_associations_openapi(store)
        rows = InfraStore.JSON.parse(json)
        @test length(rows) == 1
        row = rows[1]
        # The fixture is a golden of row *content*, so it carries no `id`: an id
        # is the store's own bookkeeping, and its value depends on how many rows
        # were written before it. That the export emits one is asserted here.
        @test row["id"] == 1
        want = _openapi_fixture("single_time_series")
        @test Dict(k => v for (k, v) in row if k != "id") == want

        close!(store)
    end

    @testset "export_supplemental_attribute_associations_openapi reproduces the fixture" begin
        store = Store(in_memory=true)
        add_supplemental_attribute_association!(
            store,
            SupplementalAttributeAssociation(
                7, "ThermalStandard", 481, "GeometricDistributionForcedOutage"
            ),
        )
        json = export_supplemental_attribute_associations_openapi(store)
        @test InfraStore.JSON.parse(json) ==
            [_openapi_fixture("supplemental_attribute_association")]
        close!(store)
    end

    @testset "supplemental-attribute export/import round trips" begin
        source = Store(in_memory=true)
        add_supplemental_attribute_associations!(
            source,
            [
                SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo"),
                SupplementalAttributeAssociation(2, "Load", 100, "GeographicInfo"),
            ],
        )
        exported = export_supplemental_attribute_associations_openapi(source)

        target = Store(in_memory=true)
        @test import_supplemental_attribute_associations_openapi!(target, exported) == 2
        re_exported = export_supplemental_attribute_associations_openapi(target)
        @test InfraStore.JSON.parse(re_exported) == InfraStore.JSON.parse(exported)

        close!(source)
        close!(target)
    end

    @testset "supplemental-attribute import rejects a duplicate within the batch" begin
        store = Store(in_memory=true)
        json = InfraStore.JSON.json([
            Dict(
                "component_id" => 1, "component_type" => "Generator",
                "attribute_id" => 100, "attribute_type" => "GeographicInfo",
            ),
            Dict(
                "component_id" => 1, "component_type" => "Generator",
                "attribute_id" => 100, "attribute_type" => "GeographicInfo",
            ),
        ])
        @test_throws InfraStore.DuplicateAssociationError import_supplemental_attribute_associations_openapi!(
            store, json
        )
        @test export_supplemental_attribute_associations_openapi(store) == "[]"
        close!(store)
    end

    # Infrastore never reconciles a data array against its association row: a
    # geometry disagreement between the two is rejected at the add boundary
    # instead, loudly and without writing anything. `Deterministic`'s `count`
    # is a separate field from the array's own shape (unlike `SingleTimeSeries`,
    # whose `length` this binding always derives from `data`), so it is the
    # one static/forecast type this binding can hand the store a mismatch for.
    @testset "add_time_series! rejects a Deterministic count/shape mismatch and leaves the store untouched" begin
        store = Store(in_memory=true)
        mismatched = Deterministic(
            DateTime(2030, 1, 1), Hour(1), Day(1), Hour(1),
            364,  # disagrees with the array's own count axis (365)
            fill(0.0, 24, 365), "max_active_power_forecast",
        )
        @test_throws InfraStore.InvalidParameterError add_time_series!(
            store, 7, "ThermalStandard", Component, mismatched
        )
        @test isempty(list_time_series(store))
        close!(store)
    end

    @testset "the memoized _cached_dlsym path is exercised by any list_* call" begin
        store = Store(in_memory=true)
        add_time_series!(
            store, 1, "Generator", Component,
            SingleTimeSeries(DateTime(2030, 1, 1), Hour(1), Float64[1, 2, 3, 4], "load"),
        )
        # Two different symbols through the same cache, each called twice: the
        # second call of each must be a cache hit, not a second `dlopen`.
        for _ in 1:2
            @test length(list_time_series(store)) == 1
            @test length(list_keys(store)) == 1
        end
        @test InfraStore._cached_dlsym(:infrastore_store_list_time_series) isa Ptr
        @test haskey(InfraStore._SYMBOL_CACHE, :infrastore_store_list_time_series)
        @test haskey(InfraStore._SYMBOL_CACHE, :infrastore_store_list_keys)
        close!(store)
    end
end

# ---- Coverage parity: dtypes, error paths, and untested exports -------------
#
# The suite above stores Float64 and Int64 and infers Int32 from a constructor,
# but never stores UInt64, Int32 or a Float32 forecast; it never operates on a
# closed store, never checks a mapped exception *type* on a bad `open_store`,
# and never calls `replace_owner!` or `clear!` at all.

@testset "stored round trips for UInt64 and Int32" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    # UInt64 values above typemax(Int64) would corrupt under any signed hop.
    u = UInt64[0, 1, UInt64(typemax(Int64)), typemax(UInt64)]
    add_time_series!(
        store, 2001, "Generator", Component, SingleTimeSeries(t0, res, u, "u64")
    )
    mu = get_metadata(store, 2001, Component, "u64"; resolution=res)
    @test mu.element_type == "u64"
    @test get_array_by_hash(store, mu.data_hash, UInt64) == u
    @test get_time_series(SingleTimeSeries, store, 2001, Component, "u64").data == u

    i = Int32[typemin(Int32), -1, 0, typemax(Int32)]
    add_time_series!(
        store, 2002, "Generator", Component, SingleTimeSeries(t0, res, i, "i32")
    )
    mi = get_metadata(store, 2002, Component, "i32"; resolution=res)
    @test mi.element_type == "i32"
    @test get_array_by_hash(store, mi.data_hash, Int32) == i
    got = get_time_series(SingleTimeSeries, store, 2002, Component, "i32")
    @test eltype(got.data) == Int32
    @test got.data == i

    b = Bool[true, false, true]
    add_time_series!(
        store, 2003, "Generator", Component, SingleTimeSeries(t0, res, b, "bools")
    )
    mb = get_metadata(store, 2003, Component, "bools"; resolution=res)
    @test mb.element_type == "bool"
    @test get_array_by_hash(store, mb.data_hash, Bool) == b

    f = Float32[1.5, -2.25, 3.125]
    add_time_series!(
        store, 2004, "Generator", Component, SingleTimeSeries(t0, res, f, "f32")
    )
    mf = get_metadata(store, 2004, Component, "f32"; resolution=res)
    @test mf.element_type == "f32"
    @test get_array_by_hash(store, mf.data_hash, Float32) == f
end

@testset "UInt64 and Int32 survive a disk round trip" begin
    mktempdir() do dir
        path = joinpath(dir, "dtypes.h5")
        t0 = DateTime(2024, 1, 1)
        res = Hour(1)
        u = UInt64[0, 1, typemax(UInt64)]
        i = Int32[typemin(Int32), 0, typemax(Int32)]

        store = Store(in_memory=false, path=path)
        add_time_series!(
            store, 1, "Generator", Component, SingleTimeSeries(t0, res, u, "u64")
        )
        add_time_series!(
            store, 2, "Generator", Component, SingleTimeSeries(t0, res, i, "i32")
        )
        flush!(store)
        close!(store)

        reopened = open_store(path; read_only=true)
        @test get_time_series(SingleTimeSeries, reopened, 1, Component, "u64").data == u
        got = get_time_series(SingleTimeSeries, reopened, 2, Component, "i32")
        @test eltype(got.data) == Int32
        @test got.data == i
        @test verify_integrity(reopened) == 0
        close!(reopened)
    end
end

@testset "Float32 forecast round trip" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 5, 1)
    res = Hour(1)
    hor = Hour(2)
    ivl = Hour(2)
    count = 3
    H = 2
    data = Float32[h * 1.5 + c for h in 1:H, c in 1:count]

    add_time_series!(
        store,
        2100,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, count, data, "pf_f32"),
    )
    fc = get_time_series(Deterministic, store, 2100, Component, "pf_f32")
    @test eltype(fc.data) == Float32
    @test fc.data == data

    # And a window-selected read keeps the dtype.
    win = get_time_series(
        Deterministic,
        store,
        2100,
        Component,
        "pf_f32";
        time_range=(t0 + Hour(2), t0 + Hour(4)),
    )
    @test eltype(win.data) == Float32
    @test win.count == 1
    @test win.data == data[:, 2:2]
end

@testset "Float32 Probabilistic and Scenarios round trips" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    hor = Hour(2)
    ivl = Hour(2)
    count = 2
    H = 2

    prob = Float32[p * 1000 + h * 10 + c for p in 1:2, h in 1:H, c in 1:count]
    add_time_series!(
        store,
        2110,
        "Generator",
        Component,
        Probabilistic(t0, res, hor, ivl, count, [0.1, 0.9], prob, "prob_f32"),
    )
    got = get_time_series(Probabilistic, store, 2110, Component, "prob_f32")
    @test eltype(got.data) == Float32
    @test got.data == prob
    @test got.percentiles == [0.1, 0.9]

    scen = Float32[s * 1000 + h * 10 + c for s in 1:3, h in 1:H, c in 1:count]
    add_time_series!(
        store,
        2111,
        "Generator",
        Component,
        # `scenario_count` is inferred from the leading dimension.
        Scenarios(t0, res, hor, ivl, count, scen, "scen_f32"),
    )
    got = get_time_series(Scenarios, store, 2111, Component, "scen_f32")
    @test eltype(got.data) == Float32
    @test got.data == scen
    @test got.scenario_count == 3
end

@testset "NaN, Inf and -0.0 round trip bit-exactly" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    values = Float64[NaN, Inf, -Inf, -0.0, 0.0, floatmax(Float64)]

    add_time_series!(
        store, 2200, "Generator", Component, SingleTimeSeries(t0, res, values, "nonfinite")
    )
    got = get_time_series(SingleTimeSeries, store, 2200, Component, "nonfinite").data

    # `==` is useless here: NaN != NaN and -0.0 == 0.0. Compare the bits.
    @test reinterpret(UInt64, got) == reinterpret(UInt64, values)
    @test isnan(got[1])
    @test got[2] == Inf
    @test got[3] == -Inf
    @test signbit(got[4]) && got[4] == 0.0
    @test !signbit(got[5])

    # Two arrays differing only in NaN payload deduplicate to one stored array:
    # the core canonicalizes NaN before hashing.
    alt = copy(values)
    reinterpret(UInt64, alt)[1] = 0x7ff8000000000001
    @test reinterpret(UInt64, alt) != reinterpret(UInt64, values)
    @test isnan(alt[1])
    add_time_series!(
        store, 2201, "Generator", Component, SingleTimeSeries(t0, res, alt, "nonfinite")
    )
    @test num_distinct_arrays(store) == 1
end

@testset "operations on a closed store raise, and close! is idempotent" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    key = add_time_series!(
        store,
        2300,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], "load"),
    )

    close!(store)
    # PIN: the handle is nulled, so every call goes through the ABI's
    # null-handle path, which `_check` maps to InvalidParameterError. This is
    # the Julia counterpart of Python's "store is closed" TimeSeriesError; the
    # two bindings report a closed store differently.
    @test_throws InfraStore.InvalidParameterError list_names(store)
    @test_throws InfraStore.InvalidParameterError get_time_series(store, key)
    @test_throws InfraStore.InvalidParameterError has_time_series(store, key)
    @test_throws InfraStore.InvalidParameterError get_counts(store)
    @test_throws InfraStore.InvalidParameterError num_distinct_arrays(store)

    # close! is idempotent: a second call is a no-op, not a double free.
    close!(store)
    close!(store)
    @test store.handle == C_NULL
end

@testset "open_store failures raise mapped exception types" begin
    mktempdir() do dir
        missing_path = joinpath(dir, "does_not_exist.h5")
        # The catalog half is opened first, so a wholly absent store surfaces as
        # the generic mapped error rather than IOError. Pin the type, not just
        # "it throws".
        @test_throws InfraStore.TimeSeriesException open_store(missing_path; read_only=true)

        # A file that is not an HDF5 store at all.
        junk = joinpath(dir, "junk.h5")
        write(junk, "not an hdf5 file")
        @test_throws InfraStore.TimeSeriesException open_store(junk; read_only=true)

        # A directory is not a store either.
        subdir = joinpath(dir, "adir.h5")
        mkdir(subdir)
        @test_throws InfraStore.TimeSeriesException open_store(subdir; read_only=true)
    end
end

@testset "forecast time_range error cases mirror the Python suite" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor = Hour(6)
    ivl = Hour(12)
    count = 4
    H = 6
    data = Float64[h * 10 + c for h in 1:H, c in 1:count]
    add_time_series!(
        store,
        2400,
        "Generator",
        Component,
        Deterministic(t0, res, hor, ivl, count, data, "det_errs"),
    )

    # 1. A start that is not a window boundary (windows are every 12h).
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic,
        store,
        2400,
        Component,
        "det_errs";
        time_range=(t0 + Hour(1), t0 + Hour(24)),
    )

    # 2. end < start.
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic, store, 2400, Component, "det_errs"; time_range=(t0 + Hour(24), t0)
    )

    # 3. A grid-aligned start past the last window (windows are 0..3).
    past = t0 + count * ivl
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic, store, 2400, Component, "det_errs"; time_range=(past, past + ivl)
    )

    # A zero-width range over an in-range start legitimately selects nothing.
    win = t0 + ivl
    empty = get_time_series(
        Deterministic, store, 2400, Component, "det_errs"; time_range=(win, win)
    )
    @test empty.count == 0
end

@testset "replace_owner! moves every series of one owner" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    for (owner, name, base) in
        [(2500, "load", 1.0), (2500, "voltage", 2.0), (2501, "load", 3.0)]
        add_time_series!(
            store,
            owner,
            "Generator",
            Component,
            SingleTimeSeries(t0, res, Float64[base, base + 1, base + 2], name),
        )
    end

    moved = replace_owner!(store, 2500, 2600, Component)
    @test moved == 2

    @test isempty(list_keys(store; owner_id=2500))
    moved_names = sort([k.name for k in list_keys(store; owner_id=2600)])
    @test moved_names == ["load", "voltage"]

    # The untouched owner still reads its own values.
    other = get_time_series(SingleTimeSeries, store, 2501, Component, "load")
    @test other.data == Float64[3, 4, 5]

    # Arrays are shared by hash, so nothing was copied.
    @test num_distinct_arrays(store) == 3

    # An owner with no series moves nothing.
    @test replace_owner!(store, 999_999, 999_998, Component) == 0

    # Moving into an identity that already exists is rejected, and nothing moves.
    @test_throws InfraStore.DuplicateTimeSeriesError replace_owner!(
        store, 2501, 2600, Component
    )
    @test length(list_keys(store; owner_id=2501)) == 1
    @test length(list_keys(store; owner_id=2600)) == 2
end

@testset "clear! removes everything, or just one owner" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    for owner in (2700, 2701)
        add_time_series!(
            store,
            owner,
            "Generator",
            Component,
            SingleTimeSeries(t0, res, Float64[owner, owner + 1], "load"),
        )
    end
    @test length(list_keys(store)) == 2

    # Scoped to one owner: the other survives and its array is kept.
    clear!(store; owner_id=2700, owner_category=Component)
    remaining = list_keys(store)
    @test length(remaining) == 1
    @test remaining[1].owner_id == 2701
    @test num_distinct_arrays(store) == 1

    # Unscoped: everything goes, arrays included.
    clear!(store)
    @test isempty(list_keys(store))
    @test num_distinct_arrays(store) == 0
    # Clearing an already-empty store is not an error.
    clear!(store)

    # `owner_id` without `owner_category` is an argument error, caught in Julia
    # before any FFI call ("both or neither", mirroring the Python binding).
    @test_throws ArgumentError clear!(store; owner_id=2700)
    # `owner_category` alone is accepted and clears that whole category.
    add_time_series!(
        store,
        2702,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[1, 2], "load"),
    )
    clear!(store; owner_category=Component)
end

@testset "clear! and replace_owner! are rejected on a read-only store" begin
    mktempdir() do dir
        path = joinpath(dir, "ro.h5")
        t0 = DateTime(2024, 1, 1)
        store = Store(in_memory=false, path=path)
        add_time_series!(
            store,
            1,
            "Generator",
            Component,
            SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], "load"),
        )
        flush!(store)
        close!(store)

        ro = open_store(path; read_only=true)
        @test read_only(ro) == true
        @test_throws InfraStore.ReadOnlyStoreError clear!(ro)
        @test_throws InfraStore.ReadOnlyStoreError replace_owner!(ro, 1, 2, Component)
        @test_throws InfraStore.ReadOnlyStoreError compact!(ro)
        # Reads still work.
        @test get_time_series(SingleTimeSeries, ro, 1, Component, "load").data ==
            Float64[1, 2, 3]
        close!(ro)
    end
end

@testset "compact! returns a report and rewrites the file" begin
    mktempdir() do dir
        path = joinpath(dir, "compact.h5")
        t0 = DateTime(2024, 1, 1)
        store = Store(in_memory=false, path=path)
        add_time_series!(
            store,
            1,
            "Generator",
            Component,
            SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "keep"),
        )
        # Big enough that dropping it moves the file size past HDF5's own noise.
        horizon, count = 48, 400
        add_time_series!(
            store,
            2,
            "Generator",
            Component,
            Deterministic(
                t0,
                Hour(1),
                Hour(horizon),
                Hour(1),
                count,
                reshape(collect(Float64, 1:(horizon * count)), horizon, count),
                "drop",
            ),
        )
        flush!(store)
        drop_key = only(get_time_series_keys(store, 2, Component))
        remove_time_series!(store, drop_key)
        flush!(store)

        before = filesize(path)
        report = compact!(store)
        after = filesize(path)

        @test report isa CompactionReport
        @test report.bytes_reclaimed == before - after > 0
        @test report.slots_reclaimed >= 0
        # The result struct gets the shared value semantics and labelled show.
        @test report == CompactionReport(
            report.slots_reclaimed,
            report.datasets_dropped,
            report.feature_sets_reclaimed,
            report.timestamp_sets_reclaimed,
            report.bytes_reclaimed,
        )
        @test occursin("bytes_reclaimed=", sprint(show, report))

        # The survivor is intact and the store is still usable across the swap.
        @test get_time_series(SingleTimeSeries, store, 1, Component, "keep").data ==
            Float64[1, 2, 3, 4]
        @test verify_integrity(store) == 0
        close!(store)
    end

    # An in-memory store has no file to rewrite.
    store = Store(in_memory=true)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), Float64[1, 2], "load"),
    )
    @test compact!(store).bytes_reclaimed == 0
    close!(store)
end

@testset "an embedded NUL in a name is rejected at the wrapper level" begin
    # PIN: Julia's `Cstring` conversion refuses a String containing a NUL, so the
    # call throws `ArgumentError` before any bytes reach the FFI. The C ABI would
    # otherwise see a silently truncated name.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    bad = "wind\0gust"
    @test_throws ArgumentError add_time_series!(
        store,
        2800,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], bad),
    )
    # Nothing was added.
    @test isempty(list_keys(store))

    # The same guard applies on the read path and to the owner type.
    @test_throws ArgumentError get_time_series(
        SingleTimeSeries, store, 2800, Component, bad
    )
    @test_throws ArgumentError add_time_series!(
        store,
        2801,
        "Gen\0erator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], "load"),
    )
end

@testset "non-ASCII names and units round trip" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    name = "负荷_ø"
    add_time_series!(
        store,
        2900,
        "Générateur",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3], name);
        units="MW·h⁻¹",
    )
    # `get_metadata` returns the array-side fields; the name and owner type come
    # back through the catalog row.
    m = get_metadata(store, 2900, Component, name; resolution=Hour(1))
    @test m.units == "MW·h⁻¹"
    rows = list_time_series(store; owner_id=2900)
    @test length(rows) == 1
    @test rows[1].name == name
    @test rows[1].owner_type == "Générateur"
    @test list_names(store) == [name]
    @test list_owner_types(store) == ["Générateur"]
end

# ---- Timestamp and period precision (Phase 4.1) ----------------------------
#
# Julia's `DateTime` is millisecond-precision, which happens to match the
# `Period` unit exactly, so nothing truncates on this side — but a `Microsecond`
# resolution can still be *constructed* and is silently flattened. Pinned here
# because Julia is the binding whose native type is coarsest.

@testset "millisecond timestamps and sub-second resolutions are exact" begin
    store = Store(in_memory=true)
    t = DateTime(2024, 1, 1, 0, 0, 0, 123)  # 123 ms
    @test Dates.millisecond(t) == 123

    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t, Hour(1), Float64[1, 2, 3, 4], "load"),
    )
    got = get_time_series(SingleTimeSeries, store, 1, Component, "load"; resolution=Hour(1))
    @test got.initial_timestamp == t
    @test Dates.millisecond(got.initial_timestamp) == 123

    # Sub-second resolutions down to one millisecond are exact.
    for (i, res) in
        enumerate([Millisecond(500), Millisecond(1), Millisecond(100), Second(1)])
        name = "res_$i"
        add_time_series!(
            store,
            100 + i,
            "Generator",
            Component,
            SingleTimeSeries(t, res, Float64[1, 2, 3, 4], name),
        )
        g = get_time_series(
            SingleTimeSeries, store, 100 + i, Component, name; resolution=res
        )
        @test g.resolution == Millisecond(res)

        # And the grid slices correctly at that resolution.
        sliced = get_time_series(
            SingleTimeSeries,
            store,
            100 + i,
            Component,
            name;
            resolution=res,
            time_range=(t + res, t + 3 * res),
        )
        @test sliced.data == Float64[2, 3]
        @test sliced.initial_timestamp == t + Millisecond(res)
    end
end

@testset "a Microsecond resolution is refused, not flattened to zero" begin
    # Formerly FINDING F13, pinned as flatten-and-accept; now fixed. A `Period`
    # is a whole number of milliseconds, so `Microsecond(1)` lost its magnitude:
    # the add succeeded, the resolution read back as `Millisecond(0)`, and only a
    # time-sliced read then failed, on a zero-length step. The write path now
    # rejects any resolution that is not a positive whole millisecond, as every
    # forecast constructor already did.
    store = Store(in_memory=true)
    t = DateTime(2024, 1, 1)
    # Note `_period_to_iso` rounds to the nearest millisecond, so only values
    # that round to zero (or below) are refused here; `Nanosecond(999_999)`
    # rounds *up* to 1 ms and is stored as such.
    for bad in [Microsecond(1), Microsecond(499), Millisecond(0), Hour(-1)]
        @test_throws InfraStore.InvalidParameterError add_time_series!(
            store,
            1,
            "Generator",
            Component,
            SingleTimeSeries(t, bad, Float64[1, 2, 3, 4], "micro"),
        )
    end
    @test isempty(list_keys(store; owner_id=1))

    # One whole millisecond is the finest grid the store can express.
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t, Millisecond(1), Float64[1, 2, 3, 4], "milli"),
    )
    keys = list_keys(store; owner_id=1)
    @test length(keys) == 1
    @test keys[1].resolution == Millisecond(1)
    got = get_time_series(
        SingleTimeSeries, store, 1, Component, "milli"; resolution=Millisecond(1)
    )
    @test length(got.data) == 4
end

@testset "pre-1970 and far-future timestamps round trip" begin
    mktempdir() do dir
        path = joinpath(dir, "spans.h5")
        cases = [
            ("pre_epoch", DateTime(1900, 1, 1)),
            ("just_before", DateTime(1969, 12, 31, 23, 59, 59)),
            ("epoch", DateTime(1970, 1, 1)),
            ("far_future", DateTime(2200, 6, 15, 12, 30, 45)),
        ]

        store = Store(in_memory=false, path=path)
        for (i, (name, ts)) in enumerate(cases)
            add_time_series!(
                store,
                i,
                "Generator",
                Component,
                SingleTimeSeries(ts, Hour(1), Float64[1, 2, 3, 4], name),
            )
        end
        flush!(store)
        close!(store)

        reopened = open_store(path; read_only=true)
        for (i, (name, ts)) in enumerate(cases)
            got = get_time_series(
                SingleTimeSeries, reopened, i, Component, name; resolution=Hour(1)
            )
            @test got.initial_timestamp == ts
            # A slice resolves against a negative epoch too.
            sliced = get_time_series(
                SingleTimeSeries,
                reopened,
                i,
                Component,
                name;
                resolution=Hour(1),
                time_range=(ts + Hour(1), ts + Hour(3)),
            )
            @test sliced.data == Float64[2, 3]
        end
        close!(reopened)
    end
end

@testset "a century-spanning non-sequential series round trips" begin
    mktempdir() do dir
        path = joinpath(dir, "century.h5")
        timestamps = [
            DateTime(1900, 1, 1),
            DateTime(1969, 12, 31, 23, 59, 59),
            DateTime(1970, 1, 1),
            DateTime(2024, 2, 29, 12, 0),   # leap day
            DateTime(2100, 12, 31, 23, 59, 59),
        ]
        values = Float64[0, 10, 20, 30, 40]

        store = Store(in_memory=false, path=path)
        add_time_series!(
            store,
            1,
            "Generator",
            Component,
            NonSequentialTimeSeries(timestamps, values, "century"),
        )
        flush!(store)
        close!(store)

        reopened = open_store(path; read_only=true)
        got = get_time_series(NonSequentialTimeSeries, reopened, 1, Component, "century")
        @test got.timestamps == timestamps
        @test got.data == values
        close!(reopened)
    end
end

@testset "non-sequential timestamps keep millisecond spacing" begin
    store = Store(in_memory=true)
    base = DateTime(2024, 1, 1)
    timestamps = [base, base + Millisecond(1), base + Millisecond(2), base + Second(1)]
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        NonSequentialTimeSeries(timestamps, Float64[1, 2, 3, 4], "precise"),
    )
    got = get_time_series(NonSequentialTimeSeries, store, 1, Component, "precise")
    @test got.timestamps == timestamps
end

# ---- Reader / mutation interaction (Phase 4.2) ------------------------------
#
# A `StaticReader` here is an owned object holding a build-time snapshot of the
# column set and array hashes, so nothing stops the store being mutated behind
# it. The two cases differ by whether the removed series' array was shared.

@testset "a stale reader over a reclaimed array errors on the next read" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "a"),
    )
    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[100, 101, 102, 103], "b"),
    )
    @test num_distinct_arrays(store) == 2

    reader = build_static_reader(store; resolution=Hour(1))
    static_read!(reader, t0)
    @test sort(vec(static_values(reader, 1))) == Float64[1, 100]

    # `list_keys` returns metadata rows, not keys, so remove by attributes.
    remove_time_series!(store, 1, Component, "a"; resolution=Hour(1))
    @test num_distinct_arrays(store) == 1

    # PIN: the read fails rather than returning whatever now occupies the
    # reclaimed slot.
    @test_throws InfraStore.NotFoundError static_read!(reader, t0)

    # A rebuilt reader works and sees only the survivor.
    rebuilt = build_static_reader(store; resolution=Hour(1))
    static_read!(rebuilt, t0)
    @test vec(static_values(rebuilt, 1)) == Float64[100]
end

@testset "a stale reader over a shared array returns its snapshot" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    for (owner, name) in [(1, "a"), (2, "b")]
        add_time_series!(
            store,
            owner,
            "Generator",
            Component,
            SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], name),
        )
    end
    @test num_distinct_arrays(store) == 1

    reader = build_static_reader(store; resolution=Hour(1))
    static_read!(reader, t0)
    before = copy(vec(static_values(reader, 1)))
    @test length(before) == 2

    remove_time_series!(store, 1, Component, "a"; resolution=Hour(1))
    @test num_distinct_arrays(store) == 1

    # The array is still alive for the survivor, so the stale read succeeds and
    # returns the build-time snapshot.
    static_read!(reader, t0)
    @test vec(static_values(reader, 1)) == before

    rebuilt = build_static_reader(store; resolution=Hour(1))
    static_read!(rebuilt, t0)
    @test length(vec(static_values(rebuilt, 1))) == 1
end

@testset "a reader built before an add does not see the new series" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "a"),
    )

    reader = build_static_reader(store; resolution=Hour(1))
    static_read!(reader, t0)
    @test length(vec(static_values(reader, 1))) == 1

    add_time_series!(
        store,
        2,
        "Generator",
        Component,
        SingleTimeSeries(t0, Hour(1), Float64[100, 101, 102, 103], "b"),
    )

    static_read!(reader, t0)
    @test length(vec(static_values(reader, 1))) == 1

    rebuilt = build_static_reader(store; resolution=Hour(1))
    static_read!(rebuilt, t0)
    @test length(vec(static_values(rebuilt, 1))) == 2
end

# ---- Result structs --------------------------------------------------------
#
# The catalog / metadata queries return structs (not NamedTuples or Dicts):
# typed fields, value equality, hashability, and a field-labelled `show`.

@testset "query results are structs with typed fields" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(
            t0, Hour(1), Float64[1, 2, 3, 4], "load"; application_data="Profile"
        );
        units="MW",
        features=Dict("scenario" => "high"),
    )
    add_time_series!(
        store,
        9,
        "GeographicInfo",
        SupplementalAttribute,
        SingleTimeSeries(t0, Hour(1), Float64[5, 6, 7, 8], "load"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Deterministic(t0, Hour(1), Hour(2), Hour(1), 2, reshape(1.0:4.0, 2, 2), "fc"),
    )

    # Metadata getters return the per-type metadata structs.
    feats = Dict{String, Any}("scenario" => "high")
    read_md() =
        get_metadata(store, 1, Component, "load"; resolution=Hour(1), features=feats)
    md = read_md()
    @test md isa TimeSeriesMetadata
    @test md.owner_type == "Generator"
    @test md.owner_category == Component
    @test md.time_series_type == SingleTimeSeries
    # Fields that do not apply to a static series are `nothing`, not zero.
    @test md.horizon === nothing && md.count === nothing && md.percentiles === nothing
    @test md.element_type == "f64"
    @test md.element_shape == ()
    @test md.application_data == "Profile"
    @test md.units == "MW"
    @test md.features == feats
    # Value equality (a NamedTuple had it; a plain struct with a Vector and a
    # Dict field would fall back to identity without the generated `==`).
    @test md == read_md()
    @test length(Set([md, read_md()])) == 1
    @test occursin("TimeSeriesMetadata(owner_id=", repr(md))
    @test occursin(bytes2hex(md.data_hash), repr(md))

    fmd = get_metadata(Deterministic, store, 1, Component, "fc")
    @test fmd isa TimeSeriesMetadata
    @test fmd.horizon == Millisecond(Hour(2))
    @test fmd.count == 2
    # A forecast record carries `dtype` and `owner_type` too — one export, one
    # struct, so no field is dropped by the type or addressing path taken.
    @test fmd.element_type == "f64"
    @test fmd.owner_type == "Generator"
    # The family sentinel resolves to whichever concrete type is stored.
    @test get_metadata(Deterministic, store, 1, Component, "fc") == fmd

    # The same record, addressed by key rather than by attributes.
    fkey = get_time_series_key(Deterministic, store, 1, Component, "fc")
    @test get_metadata(store, fkey) == fmd

    # Key rows: owner_category is the enum, time_series_type the Julia type.
    row = only(list_keys(store; owner_id=9))
    @test row isa KeyRow
    @test row.owner_category == SupplementalAttribute
    @test row.time_series_type == SingleTimeSeries
    @test row.horizon === nothing
    @test only(list_keys(store; owner_id=1, name="fc")).owner_category == Component

    info = key_info(only(get_time_series_keys(store, 9, SupplementalAttribute)))
    @test info isa KeyInfo
    @test info.owner_category == SupplementalAttribute

    # Array-group rows are key rows plus the hex hash.
    group = only(list_array_groups(store; owner_id=9))
    @test group isa ArrayGroupRow
    @test group.owner_category == SupplementalAttribute
    @test group.data_hash isa Vector{UInt8}
    @test length(group.data_hash) == 32

    # Full metadata rows carry the storage detail a key row omits.
    mrow = only(list_time_series(store; owner_id=1, name="load"))
    @test mrow isa TimeSeriesMetadata
    @test mrow.owner_type == "Generator"
    @test mrow.owner_category == Component
    @test mrow.element_type == "f64"
    @test mrow.element_shape == ()
    @test mrow.percentiles === nothing
    @test mrow.units == "MW"
    @test mrow.application_data == "Profile"
    @test mrow.data_hash == md.data_hash
    # list_time_series and get_metadata are two paths to the same record.
    @test mrow == md

    # Counts and summaries.
    @test get_counts(store) isa TimeSeriesCounts
    @test time_series_counts(store) isa TimeSeriesCountsDetailed
    @test time_series_counts(store).supplemental_attributes_with_time_series == 1
    # Ordered by the stored type code, so SingleTimeSeries precedes Deterministic.
    @test counts_by_type(store) ==
        [TimeSeriesTypeCount(SingleTimeSeries, 2), TimeSeriesTypeCount(Deterministic, 1)]
    @test only(filter(r -> r.owner_type == "GeographicInfo", static_summary(store))) ==
        StaticSummaryRow(
        "GeographicInfo",
        SupplementalAttribute,
        SingleTimeSeries,
        "load",
        t0,
        Millisecond(Hour(1)),
        4,
        1,
    )
    @test only(forecast_summary(store)).owner_category == Component

    # One StaticGrid type for both the consistency check and a reader's grid.
    # They differ in exactly one field: a reader knows the spelling of the axis
    # it spans, where the consistency check reports grids and has no reader to
    # ask, so it leaves the reference unset.
    grid = only(check_static_consistency(store))
    @test grid == StaticGrid(t0, Millisecond(Hour(1)), 4, nothing)
    @test grid.time_reference === nothing
    reader_grid = static_grid(build_static_reader(store; resolution=Hour(1)))
    @test reader_grid == StaticGrid(t0, Millisecond(Hour(1)), 4, ZonelessReference())

    @test forecast_timeline(
        build_forecast_reader(store, Deterministic; resolution=Hour(1))
    ) == ForecastTimeline(
        t0, Millisecond(Hour(1)), Millisecond(Hour(1)), 2, ZonelessReference()
    )

    @test get_forecast_parameters(store) == ForecastParameters(
        Millisecond(Hour(2)), Millisecond(Hour(1)), 2, Millisecond(Hour(1)), t0
    )
    @test get_forecast_parameters(store; resolution=Minute(5)) ==
        ForecastParameters(nothing, nothing, nothing, nothing, nothing)

    @test get_compression(store) == CompressionSettings(:none, 0, false)
    @test count_array_references(store, md.data_hash) == ArrayReferenceCounts(1, 0)
end

@testset "Probabilistic metadata struct" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Probabilistic(
            t0,
            Hour(1),
            Hour(2),
            Hour(1),
            2,
            [0.1, 0.9],
            reshape(1.0:8.0, 2, 2, 2),
            "pf";
            application_data="percentile-application_data",
        ),
    )
    pmd = get_metadata(Probabilistic, store, 1, Component, "pf")
    @test pmd isa TimeSeriesMetadata
    @test pmd.percentiles == [0.1, 0.9]
    @test pmd == get_metadata(Probabilistic, store, 1, Component, "pf")
    # `application_data` reaches a Probabilistic: the metadata surface no longer drops fields
    # depending on which getter was called.
    @test pmd.application_data == "percentile-application_data"
    @test pmd.element_type == "f64"
    @test pmd == only(list_time_series(store))

    row = only(list_time_series(store))
    @test row.percentiles == [0.1, 0.9]
    @test row.time_series_type == Probabilistic
end

@testset "get_metadata covers every stored time series type" begin
    # One getter, dispatched on the Julia type exactly like `get_time_series`.
    # Nothing is special-cased per type: a NonSequentialTimeSeries and a
    # Scenarios are as reachable as a SingleTimeSeries.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[1, 2, 3, 4], "a"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        NonSequentialTimeSeries([t0, t0 + Hour(2), t0 + Hour(5)], Float64[1, 2, 3], "b"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Deterministic(t0, res, Hour(2), Hour(1), 2, reshape(1.0:4.0, 2, 2), "c"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Probabilistic(
            t0, res, Hour(2), Hour(1), 2, [0.1, 0.9], reshape(1.0:8.0, 2, 2, 2), "d"
        ),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Scenarios(t0, res, Hour(2), Hour(1), 2, reshape(1.0:12.0, 3, 2, 2), "e"),
    )

    for (T, name) in (
        (SingleTimeSeries, "a"),
        (NonSequentialTimeSeries, "b"),
        (Deterministic, "c"),
        (Probabilistic, "d"),
        (Scenarios, "e"),
    )
        md = get_metadata(T, store, 1, Component, name)
        @test md isa TimeSeriesMetadata
        @test md.time_series_type == T
        @test md.name == name
        @test md.owner_type == "Generator"
        @test md.element_type == "f64"
    end

    # A transform-derived DST is addressable by its own type, and through the
    # family sentinel alongside it.
    @test transform_single_time_series!(store, Hour(2), Hour(1)).transformed == 1
    dst = get_metadata(DeterministicSingleTimeSeries, store, 1, Component, "a")
    @test dst.time_series_type == DeterministicSingleTimeSeries
    @test get_metadata(Deterministic, store, 1, Component, "a") == dst
    @test get_metadata(Deterministic, store, 1, Component, "c").time_series_type ==
        Deterministic

    # The type-less shorthand is the SingleTimeSeries one, as on has_time_series.
    @test get_metadata(store, 1, Component, "a") ==
        get_metadata(SingleTimeSeries, store, 1, Component, "a")

    # A type that is not a stored time series type is rejected up front.
    @test_throws InfraStore.InvalidParameterError get_metadata(
        Store, store, 1, Component, "a"
    )
end

@testset "typed lookups and filters name the Julia type" begin
    # has/remove/copy and every `time_series_type` filter take the Julia type,
    # not a wire code. The type-less has/remove forms stay the SingleTimeSeries
    # shorthand.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[1, 2, 3, 4], "a"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Scenarios(t0, res, Hour(2), Hour(1), 2, reshape(1.0:12.0, 3, 2, 2), "b"),
    )

    # A type-addressed existence check does not match a different stored type.
    @test has_time_series(SingleTimeSeries, store, 1, Component, "a"; resolution=res)
    @test !has_time_series(Scenarios, store, 1, Component, "a"; resolution=res)
    @test has_time_series(Scenarios, store, 1, Component, "b"; resolution=res)
    # …and agrees with the type-less SingleTimeSeries shorthand.
    @test has_time_series(store, 1, Component, "a"; resolution=res) ==
        has_time_series(SingleTimeSeries, store, 1, Component, "a"; resolution=res)

    # copy_time_series! preserves the stored type onto the new owner.
    copy_time_series!(Scenarios, store, 1, Component, "b", 2, "Generator"; resolution=res)
    @test has_time_series(Scenarios, store, 2, Component, "b"; resolution=res)
    @test only(list_keys(store; owner_id=2)).time_series_type == Scenarios
    # Arrays are content-addressed, so the copy adds an association, not data.
    @test num_distinct_arrays(store) == 2

    copy_time_series!(
        SingleTimeSeries,
        store,
        1,
        Component,
        "a",
        3,
        "Bus";
        new_name="renamed",
        resolution=res,
    )
    @test only(list_keys(store; owner_id=3)).name == "renamed"

    # A transform-derived DST stays a DST through a copy (a read-then-write
    # round trip would flatten it into a dense Deterministic). Both stored
    # SingleTimeSeries — "a" and the copy renamed onto the Bus — transform.
    @test transform_single_time_series!(store, Hour(2), Hour(1)).transformed == 2
    copy_time_series!(
        DeterministicSingleTimeSeries,
        store,
        1,
        Component,
        "a",
        4,
        "Generator";
        resolution=res,
    )
    @test only(list_keys(store; owner_id=4)).time_series_type ==
        DeterministicSingleTimeSeries

    # Every filter keyword takes the type too.
    @test length(list_keys(store; time_series_type=Scenarios)) == 2
    @test list_names(store; time_series_type=SingleTimeSeries) == ["a", "renamed"]
    @test list_owner_types(store; time_series_type=SingleTimeSeries) == ["Bus", "Generator"]
    @test list_owner_ids(store, Component; time_series_type=Scenarios) == [1, 2]
    @test has_for_owner(store, 1, Component; time_series_type=Scenarios)
    @test !has_for_owner(store, 3, Component; time_series_type=Scenarios)
    @test get_resolutions(store; time_series_type=Scenarios) == [Millisecond(res)]
    @test get_intervals(store; time_series_type=Scenarios) == [Millisecond(Hour(1))]
    @test isempty(get_intervals(store; time_series_type=SingleTimeSeries))
    @test only(list_time_series(store; time_series_type=Scenarios, owner_id=2)).name == "b"
    @test only(list_array_groups(store; time_series_type=Scenarios, owner_id=1)).name == "b"

    # Typed removal, then the filter form.
    remove_time_series!(Scenarios, store, 2, Component, "b"; resolution=res)
    @test !has_time_series(Scenarios, store, 2, Component, "b"; resolution=res)
    @test remove_by_filter!(store; time_series_type=Scenarios) == 1
    @test isempty(list_keys(store; time_series_type=Scenarios))

    # A `Deterministic` filter matches both storage forms (here the three
    # transform-derived / copied DSTs).
    family = list_keys(store; time_series_type=Deterministic)
    @test length(family) == 3
    @test all(k.time_series_type == DeterministicSingleTimeSeries for k in family)
    @test get_resolutions(store; time_series_type=Deterministic) ==
        [Millisecond(res)]
    # A type that is not a time series type at all is rejected everywhere.
    @test_throws InfraStore.InvalidParameterError has_time_series(
        Store, store, 1, Component, "a"
    )
    @test_throws InfraStore.InvalidParameterError list_keys(store; time_series_type=Store)
end

@testset "a parameterized request type is rejected, not ignored" begin
    # A request names a stored type, never an element type: the store addresses a
    # series by identity, which carries no dtype, so `SingleTimeSeries{Float64}`
    # has nothing to select on. Every entry point that takes a type must say so
    # the same way — this used to be three different failures, one of them a
    # `MethodError` raised only after the data had already been read.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    sts = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(t0, res, Float64[1, 2, 3, 4], "a"),
    )
    nsts = add_time_series!(
        store, 1, "Generator", Component,
        NonSequentialTimeSeries([t0, t0 + Hour(2)], Float64[9, 8], "b"),
    )
    det = add_time_series!(
        store, 1, "Generator", Component,
        Deterministic(t0, Hour(1), Hour(4), Hour(1), 2, rand(4, 2), "d"),
    )

    # The readers, keyed and attribute-addressed, static and forecast.
    @test_throws InfraStore.InvalidParameterError get_time_series(
        SingleTimeSeries{Float64}, store, sts
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        SingleTimeSeries{Float64, 1}, store, 1, Component, "a"; resolution=res
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        NonSequentialTimeSeries{Float64, 1}, store, nsts
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        NonSequentialTimeSeries{Float64}, store, 1, Component, "b"
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic{Float64, 2}, store, det
    )
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic{Float64, 2}, store, 1, Component, "d"; resolution=res
    )
    # The rest of the type-taking surface.
    @test_throws InfraStore.InvalidParameterError has_time_series(
        SingleTimeSeries{Float64}, store, 1, Component, "a"; resolution=res
    )
    @test_throws InfraStore.InvalidParameterError get_metadata(
        Deterministic{Float64, 2}, store, 1, Component, "d"; resolution=res
    )
    @test_throws InfraStore.InvalidParameterError remove_time_series!(
        Scenarios{Float64, 3}, store, 1, Component, "d"; resolution=res
    )
    @test_throws InfraStore.InvalidParameterError get_time_series_key(
        SingleTimeSeries{Float64}, store, 1, Component, "a"; resolution=res
    )
    @test_throws InfraStore.InvalidParameterError list_keys(
        store; time_series_type=SingleTimeSeries{Float64}
    )

    # The message names the type to pass instead, and is distinct from the one a
    # type that is no kind of time series gets.
    err = try
        get_time_series(SingleTimeSeries{Float64}, store, sts)
    catch e
        e
    end
    @test occursin("pass SingleTimeSeries", err.msg)
    @test occursin("not part of a time series' identity", err.msg)
    @test_throws InfraStore.InvalidParameterError list_keys(
        store; time_series_type=Vector{Float64}
    )

    # The forecast key reader validates *before* reading: removing the series
    # first would make a read raise NotFoundError, so the parameter error proves
    # nothing was fetched.
    remove_time_series!(store, det)
    @test_throws InfraStore.InvalidParameterError get_time_series(
        Deterministic{Float64, 2}, store, det
    )
    @test_throws InfraStore.NotFoundError get_time_series(Deterministic, store, det)

    # Unparameterized requests are untouched, and still carry the stored dtype
    # and rank out in the result's own parameters.
    @test typeof(get_time_series(SingleTimeSeries, store, sts)) ==
        SingleTimeSeries{Float64, 1}
    @test typeof(get_time_series(NonSequentialTimeSeries, store, nsts)) ==
        NonSequentialTimeSeries{Float64, 1}
    @test has_time_series(SingleTimeSeries, store, 1, Component, "a"; resolution=res)
    close!(store)
end

@testset "get_time_series_key addresses any type and validates" begin
    # The attribute-addressed counterpart of get_time_series_keys: it works for
    # static types too, not just forecasts, and the handle it returns always
    # names something stored.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Float64[1, 2, 3, 4], "a"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        NonSequentialTimeSeries([t0, t0 + Hour(2)], Float64[9, 8], "b"),
    )
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        Scenarios(t0, res, Hour(2), Hour(1), 2, reshape(1.0:12.0, 3, 2, 2), "c"),
    )

    # A static key round-trips through the key-based readers.
    sk = get_time_series_key(SingleTimeSeries, store, 1, Component, "a"; resolution=res)
    @test key_info(sk).time_series_type == SingleTimeSeries
    @test get_time_series(store, sk).data == Float64[1, 2, 3, 4]
    @test get_metadata(store, sk).name == "a"
    @test has_time_series(store, sk)

    nk = get_time_series_key(NonSequentialTimeSeries, store, 1, Component, "b")
    @test key_info(nk).time_series_type == NonSequentialTimeSeries
    @test get_time_series(NonSequentialTimeSeries, store, nk).data == Float64[9, 8]

    ck = get_time_series_key(Scenarios, store, 1, Component, "c"; resolution=res)
    @test key_info(ck).time_series_type == Scenarios

    # Keys from attributes feed a bulk read directly.
    @test length(bulk_read(store, [sk, nk, ck])) == 3

    # Resolution is against the catalog, so a miss and an ambiguous request are
    # both reported rather than handing back a key that names nothing.
    @test_throws InfraStore.NotFoundError get_time_series_key(
        SingleTimeSeries, store, 1, Component, "missing"
    )
    @test_throws InfraStore.NotFoundError get_time_series_key(
        Probabilistic, store, 1, Component, "a"
    )

    # The family sentinel picks whichever concrete type is stored.
    @test transform_single_time_series!(store, Hour(2), Hour(1)).transformed == 1
    @test key_info(
        get_time_series_key(Deterministic, store, 1, Component, "a"; resolution=res)
    ).time_series_type == DeterministicSingleTimeSeries

    @test_throws InfraStore.InvalidParameterError get_time_series_key(
        Store, store, 1, Component, "a"
    )
end

@testset "transactions span operations and reverse removals" begin
    store = Store(in_memory=true)
    mkts(base) = SingleTimeSeries(
        DateTime(2024, 1, 1), Hour(1), Float64[base + i for i in 0:7], "load"
    )
    add(owner, base) =
        add_time_series!(store, owner, "Generator", Component, mkts(base))

    k1 = add(1, 0.0)
    @test !in_transaction(store)

    # A throwing block rolls back everything it did, adds and removals alike.
    # Outside a transaction the removal would be irreversible.
    @test_throws ErrorException transaction(store) do
        add(2, 100.0)
        remove_time_series!(store, k1)
        @test in_transaction(store)
        @test length(list_keys(store)) == 1   # uncommitted work is visible inside
        error("boom")
    end
    @test !in_transaction(store)
    @test length(list_keys(store)) == 1
    # The array behind the removed association survived, not just its catalog row.
    @test get_time_series(store, k1).data[1] == 0.0

    # A clean block commits.
    transaction(store) do
        add(3, 200.0)
    end
    @test length(list_keys(store)) == 2
    @test !in_transaction(store)

    # Nesting: an inner rollback leaves the outer transaction open and intact.
    transaction(store) do
        add(4, 300.0)
        @test_throws ErrorException transaction(store) do
            add(5, 400.0)
            error("inner")
        end
        @test in_transaction(store)
        @test length(list_keys(store)) == 3
    end
    @test length(list_keys(store)) == 3

    # `commit_transaction!` runs inside the block's protected region: an error at
    # commit time propagates to the caller, the rollback attempt is logged rather
    # than masking it, and the store is left usable. (Sabotage the block's own
    # transaction so its commit has nothing to release.)
    @test_logs (:error, r"rollback failed") match_mode = :any begin
        @test_throws InfraStore.InvalidParameterError transaction(store) do
            add(6, 500.0)
            rollback_transaction!(store)
        end
    end
    @test !in_transaction(store)
    @test length(list_keys(store)) == 3
    transaction(store) do
        add(6, 500.0)
    end
    @test length(list_keys(store)) == 4
    @test !in_transaction(store)

    # Committing what was never begun is an error, not a silent no-op.
    @test_throws InfraStore.InvalidParameterError commit_transaction!(store)
    @test_throws InfraStore.InvalidParameterError rollback_transaction!(store)
end

@testset "units is declared on the struct and returned on read" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor, ivl, count = Hour(2), Hour(1), 3

    # A units label declared at construction reaches the store without being
    # passed to add_time_series!, and comes back on the reconstructed struct.
    sts = SingleTimeSeries(t0, res, collect(1.0:8.0), "load"; units="MW")
    k = add_time_series!(store, 1, "Generator", Component, sts)
    @test get_metadata(store, key_info(k).owner_id, Component, "load"; resolution=res).units ==
        "MW"
    @test get_time_series(store, k).units == "MW"

    # It survives a sliced read too (the label describes the values, not the window).
    @test get_time_series(store, k; time_range=(t0 + Hour(2), t0 + Hour(5))).units == "MW"

    # Omitting it leaves `nothing` end to end -- the user decides whether that
    # means unknown or dimensionless; the store neither fills it in nor guesses.
    plain = SingleTimeSeries(t0, res, collect(1.0:8.0), "unitless")
    @test plain.units === nothing
    kp = add_time_series!(store, 1, "Generator", Component, plain)
    @test get_time_series(store, kp).units === nothing

    # Non-sequential.
    stamps = [t0, t0 + Hour(1), t0 + Hour(4)]
    ns = NonSequentialTimeSeries(stamps, Float64[1, 2, 3], "events"; units="MWh")
    kn = add_time_series!(store, 2, "Generator", Component, ns)
    @test get_time_series(NonSequentialTimeSeries, store, kn).units == "MWh"

    # All three forecast types.
    det_data = Float64[h * 10 + c for h in 1:2, c in 1:3]
    kd = add_time_series!(
        store, 3, "Generator", Component,
        Deterministic(t0, res, hor, ivl, count, det_data, "fc"; units="MW"),
    )
    @test get_time_series(Deterministic, store, kd).units == "MW"

    pcts = [0.1, 0.9]
    prob_data = Float64[p + h + c for p in 1:2, h in 1:2, c in 1:3]
    kpr = add_time_series!(
        store, 4, "Generator", Component,
        Probabilistic(t0, res, hor, ivl, count, pcts, prob_data, "pf"; units="MW"),
    )
    @test get_time_series(Probabilistic, store, kpr).units == "MW"

    scen_data = Float64[s + h + c for s in 1:2, h in 1:2, c in 1:3]
    ks = add_time_series!(
        store, 5, "Generator", Component,
        Scenarios(t0, res, hor, ivl, count, scen_data, "sc"; units="MW"),
    )
    @test get_time_series(Scenarios, store, ks).units == "MW"

    # An explicit kwarg still wins over the struct's field: the kwarg is the
    # lower-level write API and predates the field.
    over = SingleTimeSeries(t0, res, collect(1.0:8.0), "override"; units="MW")
    ko = add_time_series!(store, 6, "Generator", Component, over; units="kW")
    @test get_time_series(store, ko).units == "kW"

    # units is not identity: two series differing only in their label collide.
    a = SingleTimeSeries(t0, res, collect(1.0:8.0), "dup"; units="MW")
    b = SingleTimeSeries(t0, res, collect(1.0:8.0), "dup"; units="kW")
    add_time_series!(store, 7, "Generator", Component, a)
    @test_throws InfraStore.DuplicateTimeSeriesError add_time_series!(
        store, 7, "Generator", Component, b
    )

    # ...and it cannot be smuggled in as a feature.
    @test_throws InfraStore.InvalidParameterError add_time_series!(
        store, 8, "Generator", Component,
        SingleTimeSeries(t0, res, collect(1.0:8.0), "feat");
        features=Dict("units" => "MW"),
    )
end

@testset "bulk reads carry the same descriptors as per-key reads" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    hor, ivl, count = Hour(2), Hour(1), 3

    ks = AddedTimeSeries[]
    push!(
        ks,
        add_time_series!(store, 1, "Generator", Component,
            SingleTimeSeries(
                t0, res, collect(1.0:8.0), "load"; units="MW", application_data="Profile"
            ),
        ),
    )
    push!(
        ks,
        add_time_series!(store, 2, "Generator", Component,
            NonSequentialTimeSeries([t0, t0 + Hour(1), t0 + Hour(4)], Float64[1, 2, 3],
                "events"; units="MWh", application_data="Events")),
    )
    push!(
        ks,
        add_time_series!(store, 3, "Generator", Component,
            Deterministic(t0, res, hor, ivl, count,
                Float64[h * 10 + c for h in 1:2, c in 1:3], "fc"; units="MW",
                application_data="Fc")),
    )

    # A bulk read and a per-key read of the same series must agree on every
    # descriptive attribute -- they describe the values, not the access path.
    bulk = bulk_read(store, ks)
    for (k, b) in zip(ks, bulk)
        single = get_time_series(key_info(k).time_series_type, store, k)
        @test b.units == single.units
        @test b.application_data == single.application_data
        @test b.element_type == single.element_type
    end
    @test bulk[1].units == "MW" && bulk[1].application_data == "Profile"
    @test bulk[2].units == "MWh" && bulk[2].application_data == "Events"
    @test bulk[3].units == "MW" && bulk[3].application_data == "Fc"

    # Unset stays unset through the bulk path too.
    kp = add_time_series!(store, 4, "Generator", Component,
        SingleTimeSeries(t0, res, collect(1.0:8.0), "bare"))
    b = only(bulk_read(store, [kp]))
    @test b.units === nothing
    @test b.application_data === nothing
end

@testset "transform_single_time_series! reports its full outcome" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    add_time_series!(
        store, 500, "Generator", Component,
        SingleTimeSeries(t0, res, Float64[i for i in 0:7], "load"),
    )

    out = transform_single_time_series!(store, Hour(4), Hour(2))
    @test out isa InfraStore.TransformOutcome
    @test out.transformed == 1
    @test out.sources == 1
    @test out.interval == Hour(2)
    @test !out.interval_normalized

    # An empty store distinguishes "nothing to do" from "everything skipped".
    empty_store = Store(in_memory=true)
    empty_out = transform_single_time_series!(empty_store, Hour(4), Hour(2))
    @test empty_out.sources == 0
    @test empty_out.transformed == 0
end

@testset "transform policy flags select the client contract" begin
    t0 = DateTime(2024, 6, 1)
    res = Hour(1)
    vals = Float64[i for i in 0:7]

    # normalize_single_window: a horizon spanning the series is stored as the
    # zero interval rather than verbatim.
    verbatim = Store(in_memory=true)
    add_time_series!(
        verbatim, 600, "Generator", Component, SingleTimeSeries(t0, res, vals, "load")
    )
    out = transform_single_time_series!(verbatim, Hour(8), Hour(8))
    @test out.interval_normalized
    @test out.interval == Hour(8)

    normalized = Store(in_memory=true)
    add_time_series!(
        normalized, 600, "Generator", Component, SingleTimeSeries(t0, res, vals, "load")
    )
    out = transform_single_time_series!(
        normalized, Hour(8), Hour(8); normalize_single_window=true
    )
    @test out.interval_normalized
    @test out.interval == Second(0)

    # require_uniform_forecast_grid: two resolutions deriving different counts
    # are one grid too many for InfrastructureSystems.jl, but fine by default.
    mixed() = begin
        s = Store(in_memory=true)
        add_time_series!(
            s, 1, "Generator", Component,
            SingleTimeSeries(t0, Hour(1), Float64[i for i in 0:23], "hourly"),
        )
        add_time_series!(
            s, 2, "Generator", Component,
            SingleTimeSeries(t0, Hour(2), Float64[i for i in 0:23], "two_hourly"),
        )
        s
    end
    @test transform_single_time_series!(mixed(), Hour(4), Hour(2)).transformed == 2
    @test_throws InfraStore.InvalidParameterError transform_single_time_series!(
        mixed(), Hour(4), Hour(2); require_uniform_forecast_grid=true
    )
end

# ---------------------------------------------------------------------------
# Guards protecting an artifact that is already on disk
# ---------------------------------------------------------------------------

@testset "creating over a saved store is refused" begin
    mktempdir() do dir
        path = joinpath(dir, "system.h5")
        Store(in_memory=false, path=path) do store
            add_time_series!(
                store, 1, "Generator", Component,
                SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
            )
            persist_catalog!(store)
        end
        before = filesize(path)

        # Truncating the arrays while the catalog survives would leave a store
        # that opens cleanly with every array missing.
        @test_throws InfraStore.StoreExistsError Store(in_memory=false, path=path)
        @test filesize(path) == before

        # overwrite=true discards both halves on purpose.
        Store(in_memory=false, path=path, overwrite=true) do store
            @test isempty(list_keys(store))
        end
    end
end

@testset "open_copy leaves the source alone" begin
    mktempdir() do dir
        src = joinpath(dir, "system.h5")
        dest = joinpath(dir, "scratch.h5")
        Store(in_memory=false, path=src) do store
            add_time_series!(
                store, 1, "Generator", Component,
                SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
            )
            persist_catalog!(store)
        end
        original = read(src)

        open_copy(src, dest) do copy
            @test length(list_keys(copy)) == 1
            add_time_series!(
                copy, 2, "Generator", Component,
                SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
            )
            flush!(copy)
        end
        @test read(src) == original

        # A destination that already holds a store is refused, like a create.
        @test_throws InfraStore.StoreExistsError open_copy(src, dest)
    end
end

@testset "persist_catalog! pairs an in-memory catalog with the arrays beside it" begin
    mktempdir() do dir
        path = joinpath(dir, "scratch.h5")
        store = Store(in_memory=false, path=path, catalog=:memory)
        add_time_series!(
            store, 1, "Generator", Component,
            SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
        )
        @test !isfile(path * ".sqlite")
        persist_catalog!(store)
        close!(store)

        open_store(path; read_only=true) do reopened
            @test length(list_keys(reopened)) == 1
        end
    end
end

@testset "an abandoned in-memory catalog leaves a half artifact" begin
    mktempdir() do dir
        path = joinpath(dir, "scratch.h5")
        # A scratch run that dies before landing its catalog: the arrays are on
        # disk, stamped, and nothing names them.
        store = Store(in_memory=false, path=path, catalog=:memory)
        add_time_series!(
            store, 1, "Generator", Component,
            SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
        )
        flush!(store)
        close!(store)
        @test !isfile(path * ".sqlite")

        # Reopening attached would otherwise pair those arrays with a fresh empty
        # catalog and read as a store with nothing in it. The paired stamp is what
        # turns that into a loud failure.
        @test_throws InfraStore.MismatchedArtifactError open_store(path)

        # And the leftover half blocks a fresh create until it is replaced on
        # purpose.
        @test_throws InfraStore.StoreExistsError Store(
            in_memory=false, path=path, catalog=:memory
        )
        Store(in_memory=false, path=path, catalog=:memory, overwrite=true) do fresh
            add_time_series!(
                fresh, 7, "Generator", Component,
                SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
            )
            persist_catalog!(fresh)
        end
        open_store(path; read_only=true) do reopened
            @test [k.owner_id for k in list_keys(reopened)] == [7]
        end
    end
end

@testset "compaction and rollback under an in-memory catalog" begin
    mktempdir() do dir
        path = joinpath(dir, "scratch.h5")
        store = Store(in_memory=false, path=path, catalog=:memory)
        for owner in (1, 2)
            add_time_series!(
                store, owner, "Generator", Component,
                SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:24.0), "load"),
            )
        end

        # A rollback undoes the array half against the backend, which the catalog
        # living in RAM does not change.
        begin_transaction!(store)
        add_time_series!(
            store, 3, "Generator", Component,
            SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(101.0:124.0), "load"),
        )
        rollback_transaction!(store)
        @test isempty(list_keys(store; owner_id=3))

        # Compaction rewrites only the arrays; the catalog is still RAM-only, so
        # the rewritten file has to keep the stamp it is paired with.
        remove_time_series!(
            store, get_time_series_key(SingleTimeSeries, store, 2, Component, "load")
        )
        report = compact!(store)
        @test report.slots_reclaimed + report.datasets_dropped > 0
        @test !isfile(path * ".sqlite")

        persist_catalog!(store)
        close!(store)

        open_store(path; read_only=true) do reopened
            @test [k.owner_id for k in list_keys(reopened)] == [1]
            @test verify_integrity(reopened) == 0
        end
    end
end

@testset "a dtype disagreement is reported, never reinterpreted" begin
    # Both halves of this are the same mistake: bytes decoded as a type the store
    # did not store them as. The dtype is known on both paths — the FFI reports
    # it on a read, and `eltype(data)` fixes it on a write — so a disagreement is
    # a question the wrapper can answer rather than a reinterpretation it should
    # perform. Silently reinterpreting produced numbers like `5.0e-323` in place
    # of `Int64[10, 20, 30]`, with no error anywhere.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    k = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "counts"),
    )
    h = get_metadata(store, k).data_hash

    # Reading as the wrong type is an error naming the right one...
    err = try
        get_array_by_hash(store, h)   # T defaults to Float64
        nothing
    catch e
        e
    end
    @test err isa InfraStore.InvalidParameterError
    @test occursin("Int64", sprint(showerror, err))
    # ...and reading as the stored type still works.
    @test get_array_by_hash(store, h, Int64) == Int64[10, 20, 30]

    # A declared element_type must agree with the array it describes. The
    # core validates only the total byte length, so a same-width mismatch
    # (Int64 declared "f64") used to be stored and read back as garbage;
    # only the width-mismatched case failed.
    @test_throws InfraStore.InvalidParameterError add_time_series!(
        store, 2, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[1, 2, 3, 4], "m"; element_type="f64"),
    )
    # The inner dtype of a tuple/function spelling is checked the same way.
    @test_throws InfraStore.InvalidParameterError add_time_series!(
        store, 3, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[1 2; 3 4], "t"; element_type="tuple(2,f64)"),
    )

    # Agreeing declarations, and no declaration at all, are unaffected.
    ki = add_time_series!(
        store, 4, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[1, 2, 3, 4], "ok_i64"; element_type="i64"),
    )
    @test get_time_series(store, ki).data == Int64[1, 2, 3, 4]
    add_time_series!(
        store, 5, "Generator", Component,
        SingleTimeSeries(
            t0, res, Float64[1 2; 3 4], "ok_tuple"; element_type="tuple(2,f64)"
        ),
    )
    kn = add_time_series!(
        store, 6, "Generator", Component,
        SingleTimeSeries(t0, res, Int64[7, 8], "inferred"),
    )
    @test get_time_series(store, kn).data == Int64[7, 8]
end

@testset "timestamps convert exactly, not through a float" begin
    # `_to_unix_ms` used to be `Int64(datetime2unix(dt) * 1000)`, routing an
    # integer millisecond count through Float64 seconds. Outside one accidentally
    # exact window (roughly 2004-2038) the product is not integral for a
    # millisecond-precision instant, and `Int64` threw `InexactError` on an
    # ordinary timestamp. A `DateTime` is already integer milliseconds, so the
    # conversion needs no float at all.
    for dt in [
        DateTime(1900, 1, 1),
        DateTime(1969, 12, 31, 23, 59, 59, 999),
        DateTime(1970, 1, 1),
        DateTime(2024, 1, 1, 0, 0, 0, 123),
        DateTime(2038, 3, 19, 21, 10, 26, 23),   # threw before the fix
        DateTime(2200, 6, 15, 12, 34, 56, 789),
        DateTime(9999, 12, 31, 23, 59, 59, 999),
    ]
        @test InfraStore._from_unix_ms(InfraStore._to_unix_ms(dt)) == dt
    end
    @test InfraStore._to_unix_ms(DateTime(1970, 1, 1)) == 0
    @test InfraStore._to_unix_ms(DateTime(1969, 12, 31, 23, 59, 59, 999)) == -1

    # And it reaches the store: a far-future millisecond timestamp round-trips.
    store = Store(in_memory=true)
    t = DateTime(2038, 3, 19, 21, 10, 26, 23)
    k = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(t, Hour(1), Float64[1, 2, 3], "load"),
    )
    @test get_time_series(store, k).initial_timestamp == t
end

@testset "Store(path=...) creates a file-backed store" begin
    # `in_memory` used to default to `true`, and the non-`overwrite` branch
    # passed both it and the path to `infrastore_store_create_with_catalog`,
    # which ignores the path when in-memory wins. The contradictory pair was
    # accepted silently: the user got an in-memory store, and everything written
    # was discarded at `close!` with no file ever created. It now defaults to
    # "whatever the path implies", and the contradiction is an error -- the same
    # rule the `overwrite=true` branch has always enforced.
    mktempdir() do dir
        path = joinpath(dir, "inferred.h5")
        s = Store(path=path)
        @test get_path(s) == path
        @test catalog_mode(s) === :attached
        add_time_series!(
            s, 1, "Generator", Component,
            SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), Float64[1, 2, 3], "load"),
        )
        close!(s)
        @test isfile(path)

        # It really is a store, and it holds what was written.
        open_store(path; read_only=true) do reopened
            @test length(list_keys(reopened)) == 1
        end

        # No path still means in-memory.
        mem = Store()
        @test get_path(mem) === nothing
        @test catalog_mode(mem) === :memory
        close!(mem)

        # Asking for both is refused rather than silently resolved.
        @test_throws ArgumentError Store(path=joinpath(dir, "x.h5"), in_memory=true)

        # And an explicit `in_memory=false` with a path still works.
        p2 = joinpath(dir, "explicit.h5")
        s2 = Store(path=p2, in_memory=false)
        close!(s2)
        @test isfile(p2)
    end
end

@testset "a key-addressed forecast read checks the type it was asked for" begin
    # The FFI reports the type it matched, and this path used to decode as the
    # requested `T` regardless. Asking for a `Deterministic` with a
    # `Probabilistic` key returned a `Deterministic{Float64,3}` whose `count`
    # disagreed with its own second axis — the percentile axis silently absorbed
    # as a leading dimension, the percentiles themselves dropped, no error. The
    # attribute-addressed form and `bulk_read` both dispatched correctly, which
    # is what made the asymmetry visible.
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    p = Probabilistic(
        t0, Hour(1), Hour(2), Hour(1), 4, [0.1, 0.5, 0.9],
        reshape(collect(1.0:24.0), (3, 2, 4)), "load",
    )
    kp = add_time_series!(store, 1, "Generator", Component, p)

    # The right type reads, and keeps what makes it that type.
    got = get_time_series(Probabilistic, store, kp)
    @test got isa Probabilistic
    @test size(got.data) == (3, 2, 4)
    @test got.percentiles == [0.1, 0.5, 0.9]

    # The wrong ones are refused, naming what the key actually holds.
    for T in (Deterministic, Scenarios)
        err = try
            get_time_series(T, store, kp)
            nothing
        catch e
            e
        end
        @test err isa InfraStore.InvalidParameterError
        @test occursin("Probabilistic", sprint(showerror, err))
    end

    # A Deterministic key still reads as one...
    d = Deterministic(
        t0, Hour(1), Hour(2), Hour(1), 3, reshape(collect(1.0:6.0), (2, 3)), "det"
    )
    kd = add_time_series!(store, 2, "Generator", Component, d)
    @test get_time_series(Deterministic, store, kd) isa Deterministic
    @test_throws InfraStore.InvalidParameterError get_time_series(Probabilistic, store, kd)

    # ...and the two deterministic forms stay interchangeable, because a
    # DeterministicSingleTimeSeries is a view that always reads back dense.
    add_time_series!(
        store, 3, "Generator", Component,
        SingleTimeSeries(t0, Hour(1), collect(1.0:8.0), "load"),
    )
    transform_single_time_series!(store, Hour(2), Hour(1))
    kdst = only(
        filter(
            k -> key_info(k).time_series_type == DeterministicSingleTimeSeries,
            get_time_series_keys(store, 3, Component),
        ),
    )
    @test get_time_series(Deterministic, store, kdst) isa Deterministic
    @test get_time_series(DeterministicSingleTimeSeries, store, kdst) isa Deterministic
    @test_throws InfraStore.InvalidParameterError get_time_series(Scenarios, store, kdst)
end

# ---------------------------------------------------------------------------
# ZonedDateTime input (the InfraStoreTimeZonesExt weak-dependency extension)
# ---------------------------------------------------------------------------

@testset "a bare DateTime is a wall clock, and says so when it cannot be" begin
    # A bare `DateTime` names no instant, so it is recorded as a wall clock and
    # comes back as one. The stored instant is its fields read as if UTC --
    # unchanged from the old UTC-by-convention reading, which was never a fact
    # about the value; what is new is that the store now records that it was a
    # convention.
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1, 12)
    series = SingleTimeSeries(initial, Hour(1), collect(1.0:3.0), "load")
    @test series.time_reference == ZonelessReference()
    key = add_time_series!(store, 1, "Generator", Component, series)
    @test get_time_series(store, key).initial_timestamp == initial
    @test get_time_series(store, key).time_reference == ZonelessReference()

    # Anything that is neither a DateTime nor a ZonedDateTime is an
    # InvalidParameterError naming the fix, not a bare MethodError.
    err = try
        SingleTimeSeries("2024-01-01", Hour(1), collect(1.0:3.0), "load")
        nothing
    catch e
        e
    end
    @test err isa InfraStore.InvalidParameterError
    @test occursin("TimeZones", sprint(showerror, err))

    # A `Date` still means midnight UTC, as it did when the constructors relied
    # on `convert(DateTime, ::Date)`.
    date_key = add_time_series!(
        store, 2, "Generator", Component,
        SingleTimeSeries(Date(2024, 1, 1), Hour(1), collect(1.0:3.0), "load"),
    )
    @test get_time_series(store, date_key).initial_timestamp == DateTime(2024, 1, 1)
end

@testset "an unspecified reference is not a wall clock" begin
    # `nothing` and `ZonelessReference()` are different claims: one says the
    # spelling was never recorded, the other says the timestamps are wall
    # clocks. Only the *absence* of the keyword infers. This is the shape a
    # store written by another binding (or by a native Rust caller) that
    # declared no reference arrives in, so a read must not invent one -- an
    # invented `ZonelessReference()` would be written straight back by
    # `add_time_series!`, whose default is the series' own reference.
    store = Store(in_memory=true)
    initial = DateTime(2024, 1, 1, 12)
    series = SingleTimeSeries(
        initial, Hour(1), collect(1.0:3.0), "load"; time_reference=nothing
    )
    @test series.time_reference === nothing
    key = add_time_series!(store, 1, "Generator", Component, series)
    read_back = get_time_series(store, key)
    @test read_back.time_reference === nothing
    @test read_back.initial_timestamp == initial
    # And the catalog agrees with the series.
    @test only(list_time_series(store)).time_reference === nothing

    # The same for the vector-timestamped type, whose constructor reads its
    # spelling off the vector rather than off one timestamp.
    nsts = NonSequentialTimeSeries(
        [initial, initial + Hour(1)], [1.0, 2.0], "irregular"; time_reference=nothing
    )
    @test nsts.time_reference === nothing
    nkey = add_time_series!(store, 2, "Generator", Component, nsts)
    @test get_time_series(NonSequentialTimeSeries, store, nkey).time_reference === nothing

    # Re-adding what was read back records the same absence, rather than
    # promoting it to a wall clock on the way through.
    again = Store(in_memory=true)
    add_time_series!(again, 1, "Generator", Component, read_back)
    @test only(list_time_series(again)).time_reference === nothing
end

@testset "every integer offset is judged without overflowing" begin
    # `abs` cannot represent `-typemin(Int)`, so `abs(Int(typemin(Int)))` is
    # `typemin(Int)` again -- negative, and therefore *below* the bound, which
    # let the least plausible offset there is through as if it were valid. The
    # same shape of bug the Rust core had.
    for v in (typemin(Int), typemin(Int) + 1, typemax(Int), -24 * 60, 24 * 60)
        @test_throws InfraStore.InvalidParameterError FixedOffsetReference(v)
    end
    # An oversized BigInt reports the documented error too, rather than an
    # InexactError from the conversion.
    @test_throws InfraStore.InvalidParameterError FixedOffsetReference(big(10)^30)
    # The bound stays exclusive on both sides.
    for v in (-1439, -420, 0, 330, 1439)
        @test FixedOffsetReference(v).minutes == v
    end
end

@testset "a point read is a query bound too" begin
    # The ranged reads carry the spelling and the core refuses a bound the
    # series cannot answer; the point reads sent only the instant, so a bare
    # DateTime (a wall clock) could read an instant-bearing axis and return a
    # *row* where the same mismatch on a range raises.
    values = collect(1.0:4.0)
    initial = DateTime(2024, 1, 1)

    instants = Store(in_memory=true)
    add_time_series!(
        instants, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=UTCReference()),
    )
    r = build_static_reader(instants; resolution=Hour(1))
    @test_throws InfraStore.InvalidParameterError static_read!(r, DateTime(2024, 1, 1, 1))

    # A wall-clock axis still reads a wall clock.
    wall = Store(in_memory=true)
    add_time_series!(
        wall, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"),
    )
    rw = build_static_reader(wall; resolution=Hour(1))
    static_read!(rw, DateTime(2024, 1, 1, 1))
    @test static_values(rw, 1) == [2.0]

    # An unspecified axis has nothing to disagree with, so it accepts either.
    unspecified = Store(in_memory=true)
    add_time_series!(
        unspecified, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=nothing),
    )
    ru = build_static_reader(unspecified; resolution=Hour(1))
    static_read!(ru, DateTime(2024, 1, 1, 1))
    @test static_values(ru, 1) == [2.0]

    # The forecast point read obeys the same rule.
    fc = Store(in_memory=true)
    add_time_series!(
        fc, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=UTCReference()),
    )
    transform_single_time_series!(fc, Hour(2), Hour(1))
    fr = build_forecast_reader(fc, Deterministic; resolution=Hour(1))
    @test_throws InfraStore.InvalidParameterError forecast_read!(
        fr, DateTime(2024, 1, 1, 1)
    )

    # The axis spelling is cached on the reader, not re-fetched per timestep.
    @test r.time_reference == UTCReference()
    @test fr.time_reference == UTCReference()
end

@testset "a reader reports the spelling of the axis it spans" begin
    # A reader spans one timeline, so it carries one spelling -- and without it
    # a Julia caller could read the axis but not say how it was written, unable
    # to tell a wall-clock axis from an unspecified or a UTC one. That is the
    # distinction the axis exists to preserve, and every other binding reports
    # it, so the Julia readers must too.
    initial = DateTime(2024, 1, 1)
    values = collect(1.0:4.0)

    # A wall-clock cohort reports the positive claim, not an absence.
    wall = Store(in_memory=true)
    add_time_series!(
        wall, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"),
    )
    grid = static_grid(build_static_reader(wall; resolution=Hour(1)))
    @test grid.time_reference == ZonelessReference()

    # A cohort that declared no spelling reports `nothing`, which is a
    # different answer -- collapsing the two would let a read invent a claim
    # the writer never made.
    unspecified = Store(in_memory=true)
    add_time_series!(
        unspecified, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=nothing),
    )
    ugrid = static_grid(build_static_reader(unspecified; resolution=Hour(1)))
    @test ugrid.time_reference === nothing

    # And an instant-bearing cohort reports the spelling it was written in.
    utc = Store(in_memory=true)
    add_time_series!(
        utc, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=UTCReference()),
    )
    @test static_grid(build_static_reader(utc; resolution=Hour(1))).time_reference ==
        UTCReference()

    zoned = Store(in_memory=true)
    add_time_series!(
        zoned, 1, "Generator", Component,
        SingleTimeSeries(
            initial, Hour(1), values, "load";
            time_reference=ZoneReference("America/Denver"),
        ),
    )
    @test static_grid(build_static_reader(zoned; resolution=Hour(1))).time_reference ==
        ZoneReference("America/Denver")

    # A forecast reader's window timeline carries it on the same terms.
    fc = Store(in_memory=true)
    add_time_series!(
        fc, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"; time_reference=UTCReference()),
    )
    transform_single_time_series!(fc, Hour(2), Hour(1))
    timeline = forecast_timeline(
        build_forecast_reader(fc, Deterministic; resolution=Hour(1))
    )
    @test timeline.time_reference == UTCReference()
end

# The rest of the timestamp tests need the TimeZones weak dependency, and live in
# their own file because `tz"..."` cannot even be *lowered* without it — an
# `if available ... end` block around them would fail to macro-expand rather than
# skip. `include` is an ordinary runtime call, so it is only reached when the
# package loaded.
#
# `Pkg.test()` always provides TimeZones (see `[targets]` in Project.toml), which
# is how CI runs this suite. A bare `julia --project=julia/InfraStore.jl
# test/runtests.jl` does not, since a weak dependency is not loadable from the
# package's own environment; there the extension tests are skipped out loud
# rather than failing a run that is otherwise valid.
if (
    try
        @eval using TimeZones
        true
    catch
        false
    end
)
    include("timezones_tests.jl")
else
    @warn "TimeZones is not loadable here, so the ZonedDateTime tests were SKIPPED. " *
        "Run them with: julia --project=julia/InfraStore.jl -e 'using Pkg; Pkg.test()'"
end

@testset "association ids: the handle a caller stores in its own model" begin
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    sts(name, base=0.0) = SingleTimeSeries(t0, res, collect(base .+ (1.0:4.0)), name)

    store = Store(in_memory=true)

    # A write reports the id its row was filed under, and a read agrees.
    added = add_time_series!(store, 1, "Generator", Component, sts("load"))
    @test added isa AddedTimeSeries
    @test added.id == 1
    @test get_metadata(store, added).id == added.id

    # An explicit id is honored, and the catalog's counter ratchets past it, so
    # the next assigned id cannot land on top of one the caller placed.
    imported = add_time_series!(store, 2, "Generator", Component, sts("load"); id=500)
    @test imported.id == 500
    @test add_time_series!(store, 3, "Generator", Component, sts("load")).id == 501

    # An id resolves to its row; one nothing was filed under resolves to
    # `nothing`, because a caller checking a reference it persisted is asking a
    # question rather than making a demand.
    @test get_metadata_by_id(store, added.id).name == "load"
    @test association_exists(store, added.id)
    @test get_metadata_by_id(store, 9999) === nothing
    @test !association_exists(store, 9999)

    # A removed row's id stops resolving and is never handed out again, so a
    # stale reference can never come to mean a different series.
    remove_time_series!(store, added)
    @test !association_exists(store, added.id)
    replacement = add_time_series!(store, 1, "Generator", Component, sts("load"))
    @test replacement.id != added.id
    @test !association_exists(store, added.id)

    # A taken id is its own error, distinct from a duplicate series.
    @test_throws InfraStore.DuplicateAssociationIdError add_time_series!(
        store, 4, "Generator", Component, sts("load"); id=500
    )

    close!(store)
end

@testset "association ids: writes report them, and a view can be given one" begin
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)
    store = Store(in_memory=true)

    # The bulk form reports one id per item, in input order.
    batch = AddBatch()
    for i in 1:3
        add_time_series!(
            batch, i, "Generator", Component,
            SingleTimeSeries(t0, res, collect(1.0:4.0), "load"),
        )
    end
    added = add_time_series_bulk!(store, batch)
    @test [a.id for a in added] == [1, 2, 3]
    @test all(get_metadata(store, a).id == a.id for a in added)

    # A derived view is a row of its own: its own id, its source's array.
    long = add_time_series!(
        store, 10, "Generator", Component,
        SingleTimeSeries(t0, res, collect(1.0:24.0), "load"),
    )
    before = num_distinct_arrays(store)
    view = add_derived_view!(store, long, Hour(6), Hour(6))
    @test num_distinct_arrays(store) == before
    @test view.id != long.id
    @test get_metadata(store, view).time_series_type === DeterministicSingleTimeSeries
    @test get_metadata(store, view).data_hash == get_metadata(store, long).data_hash

    # …and an importer can name that id.
    other = add_time_series!(
        store, 11, "Generator", Component,
        SingleTimeSeries(t0, res, collect(1.0:24.0), "load"),
    )
    @test add_derived_view!(store, other, Hour(6), Hour(6); id=4242).id == 4242

    close!(store)
end

@testset "association ids: attachments report theirs, outside identity" begin
    store = Store(in_memory=true)

    @test add_supplemental_attribute_association!(
        store, SupplementalAttributeAssociation(1, "Generator", 100, "GeographicInfo")
    ) == 1

    fresh = SupplementalAttributeAssociation(2, "Generator", 100, "GeographicInfo")
    @test add_supplemental_attribute_association!(store, fresh) == 2

    rows = list_supplemental_attribute_associations(store)
    @test [r.id for r in rows] == [1, 2]

    # A row read back must equal the value that wrote it: identity is the
    # endpoint pair, so the id sits outside `==` and `hash` — and `hash` must
    # agree with `==` for every Set and Dict these land in.
    stored = rows[2]
    @test stored.id == 2
    @test fresh.id === nothing
    @test stored == fresh
    @test hash(stored) == hash(fresh)
    @test stored in Set([fresh])

    close!(store)
end
