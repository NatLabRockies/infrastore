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
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 1, 4), DateTime(2024, 1, 3)]
    series = NonSequentialTimeSeries(timestamps, Int64[10, 20, 30], "events")
    key = add_time_series!(store, 7, "Generator", Component, series)
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

@testset "non-sequential N-D + ext round-trip" begin
    store = Store(in_memory=true)
    timestamps = [DateTime(2024, 1, 1), DateTime(2024, 1, 1, 4), DateTime(2024, 1, 3)]
    # A (length, k) per-step element array tagged with an opaque extension payload, as a
    # FunctionData encoding would produce on the InfrastructureSystems.jl side.
    data = Float64[1 2; 3 4; 5 6]
    series = NonSequentialTimeSeries(timestamps, data, "curves"; ext="LinearFunctionData")
    key = add_time_series!(store, 9, "Generator", Component, series)
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps == timestamps
    @test got.data == data
    @test got.data isa Array{Float64,2}
    @test got.ext == "LinearFunctionData"
    @test got.name == "curves"

    got_attr = get_time_series(NonSequentialTimeSeries, store, 9, Component, "curves")
    @test got_attr.data == data
    @test got_attr.ext == "LinearFunctionData"
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
        path = joinpath(dir, "store.nc")
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

@testset "dtype-parameterized arrays" begin
    store = Store(in_memory=true)
    res = Hour(1)
    t0 = DateTime(2024, 1, 1)

    # Int64 scalar series round-trips with its dtype.
    add_time_series!(
        store,
        1001,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, Int64[10, 20, 30], "load"; ext="Int64"),
    )
    m = get_metadata(store, 1001, Component, "load"; resolution=res)
    @test m.dtype == Int64
    @test get_array_by_hash(store, m.data_hash, Int64) == Int64[10, 20, 30]

    # Multi-dim element tuple (4 steps × 3 coeffs) round-trips, row-major correct.
    A = Float64[i + j / 10 for i in 1:4, j in 1:3]
    add_time_series!(
        store,
        1002,
        "Generator",
        Component,
        SingleTimeSeries(t0, res, A, "cost"; ext="QuadraticFunctionData"),
    )
    mq = get_metadata(store, 1002, Component, "cost"; resolution=res)
    @test mq.dtype == Float64
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
    @test_throws InfraStore.NotFoundError get_time_series(
        Deterministic, store, 400, Component, "dst"
    )

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
    @test length(list_keys(store; time_series_type=InfraStore.INFRASTORE_TYPE_SINGLE)) == 4
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
    @test Set(get_resolutions(store; time_series_type=InfraStore.INFRASTORE_TYPE_SINGLE)) ==
        Set([Millisecond(Minute(5)), Millisecond(Hour(1))])
    @test isempty(
        get_resolutions(store; time_series_type=InfraStore.INFRASTORE_TYPE_DETERMINISTIC)
    )

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
    groups = Dict{Vector{UInt8},Vector{Int}}()
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
    @test list_owner_ids(
        store, Component; time_series_type=InfraStore.INFRASTORE_TYPE_DETERMINISTIC
    ) == [1]
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

@testset "AbstractDeterministic family resolution: miss and ambiguity" begin
    # The family is resolved in the core; a real miss is no longer masked by the
    # old guess-and-retry fallback.
    store = Store(in_memory=true)
    @test_throws InfraStore.NotFoundError get_time_series(
        AbstractDeterministic, store, 999, Component, "nope"
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
    res = Hour(1);
    hor = Hour(4);
    ivl = Hour(2);
    count = 3;
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
            path = joinpath(dir, "store.nc")
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
        path = joinpath(dir, "store.nc")
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
    @test typeof(si) == SingleTimeSeries{Int64,1}

    k_f = add_time_series!(
        store, 2, "Gen", Component, SingleTimeSeries(t0, res, Float32[1.5, 2.5, 3.5], "f")
    )
    sf = get_time_series(store, k_f)
    @test eltype(sf.data) == Float32
    @test sf.data == Float32[1.5, 2.5, 3.5]
    @test typeof(sf) == SingleTimeSeries{Float32,1}

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
    @test typeof(SingleTimeSeries(t0, res, Float64[1, 2, 3], "f")) ==
        SingleTimeSeries{Float64,1}
    @test typeof(SingleTimeSeries(t0, res, Int32[1 2; 3 4], "i")) ==
        SingleTimeSeries{Int32,2}
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
    det = Deterministic(
        t0, res, Hour(2), Hour(1), 5, Float64[i + s for s in 0:1, i in 1:5], "d"
    )
    @test typeof(det) == Deterministic{Float64,2}
    scen = Scenarios(
        t0,
        res,
        Hour(2),
        Hour(1),
        5,
        Float32[v for v in 1:(3 * 2 * 5)] |> a -> reshape(a, 3, 2, 5),
        "s",
    )
    @test typeof(scen) == Scenarios{Float32,3}
end

@testset "Phase 2 additions: units, time_range, discovery, rename, bulk dispatch" begin
    store = Store(in_memory=true)
    t0 = DateTime(2024, 1, 1)
    res = Hour(1)

    # units round-trips through get_metadata (previously write-only).
    sts = SingleTimeSeries(t0, res, collect(1.0:8.0), "load")
    k = add_time_series!(store, 1, "Generator", Component, sts; units="MW", ext="Profile")
    md = get_metadata(store, 1, Component, "load"; resolution=res)
    @test md.units == "MW"
    @test md.ext == "Profile"

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
    @test isempty(get_intervals(store; time_series_type=InfraStore.INFRASTORE_TYPE_SINGLE))
    @test sort(list_names(store)) == ["fc", "load"]
    @test list_names(store; owner_id=1) == ["load"]
    @test sort(list_owner_types(store)) == ["Bus", "Generator"]

    # Full metadata rows include units + ext.
    rows = list_time_series(store; owner_id=1)
    @test length(rows) == 1
    @test rows[1].units == "MW"
    @test rows[1].ext == "Profile"
    @test rows[1].dtype == Float64

    # get_probabilistic_metadata exposes percentiles + units without a data fetch.
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
    pmd = get_probabilistic_metadata(store, 3, Component, "pf")
    @test pmd.percentiles == [0.1, 0.5, 0.9]
    @test pmd.units == "MWp"

    # bulk_read dispatches on stored type.
    mixed = bulk_read(store, TimeSeriesKey[k, kf])
    @test mixed[1] isa SingleTimeSeries
    @test mixed[1].data == full.data
    @test mixed[2] isa Deterministic
    @test mixed[2].data == det.data

    # resolve_forecast_key resolves the abstract-deterministic family.
    rk = resolve_forecast_key(
        store, 2, Component, "fc", InfraStore.INFRASTORE_TYPE_ABSTRACT_DETERMINISTIC
    )
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
    fmd = get_forecast_metadata(
        store, 2, Component, "fc", InfraStore.INFRASTORE_TYPE_DETERMINISTIC; features=feats
    )
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
    pmd = get_probabilistic_metadata(store, 3, Component, "pf")
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
    k = add_time_series!(store, 1, "Generator", Component, sts)

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
    dest = joinpath(dir, "persisted.nc")
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
    dest = joinpath(dir, "assoc.nc")
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
    @test mu.dtype == UInt64
    @test get_array_by_hash(store, mu.data_hash, UInt64) == u
    @test get_time_series(SingleTimeSeries, store, 2001, Component, "u64").data == u

    i = Int32[typemin(Int32), -1, 0, typemax(Int32)]
    add_time_series!(
        store, 2002, "Generator", Component, SingleTimeSeries(t0, res, i, "i32")
    )
    mi = get_metadata(store, 2002, Component, "i32"; resolution=res)
    @test mi.dtype == Int32
    @test get_array_by_hash(store, mi.data_hash, Int32) == i
    got = get_time_series(SingleTimeSeries, store, 2002, Component, "i32")
    @test eltype(got.data) == Int32
    @test got.data == i

    b = Bool[true, false, true]
    add_time_series!(
        store, 2003, "Generator", Component, SingleTimeSeries(t0, res, b, "bools")
    )
    mb = get_metadata(store, 2003, Component, "bools"; resolution=res)
    @test mb.dtype == Bool
    @test get_array_by_hash(store, mb.data_hash, Bool) == b

    f = Float32[1.5, -2.25, 3.125]
    add_time_series!(
        store, 2004, "Generator", Component, SingleTimeSeries(t0, res, f, "f32")
    )
    mf = get_metadata(store, 2004, Component, "f32"; resolution=res)
    @test mf.dtype == Float32
    @test get_array_by_hash(store, mf.data_hash, Float32) == f
end

@testset "UInt64 and Int32 survive a disk round trip" begin
    mktempdir() do dir
        path = joinpath(dir, "dtypes.nc")
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
        missing_path = joinpath(dir, "does_not_exist.nc")
        # The catalog half is opened first, so a wholly absent store surfaces as
        # the generic mapped error rather than IOError. Pin the type, not just
        # "it throws".
        @test_throws InfraStore.TimeSeriesException open_store(missing_path; read_only=true)

        # A file that is not a NetCDF store at all.
        junk = joinpath(dir, "junk.nc")
        write(junk, "not a netcdf file")
        @test_throws InfraStore.TimeSeriesException open_store(junk; read_only=true)

        # A directory is not a store either.
        subdir = joinpath(dir, "adir.nc")
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
        path = joinpath(dir, "ro.nc")
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

@testset "a Microsecond resolution is silently flattened to zero" begin
    # FINDING F13, from the Julia side: a `Period` is a whole number of
    # milliseconds, so a `Microsecond(1)` resolution loses its magnitude. The add
    # succeeds and the stored resolution reads back as `Millisecond(0)` rather
    # than being rejected. PINNED, not fixed.
    store = Store(in_memory=true)
    t = DateTime(2024, 1, 1)
    add_time_series!(
        store,
        1,
        "Generator",
        Component,
        SingleTimeSeries(t, Microsecond(1), Float64[1, 2, 3, 4], "micro"),
    )
    keys = list_keys(store; owner_id=1)
    @test length(keys) == 1
    @test keys[1].resolution == Millisecond(0)

    # A full read still works; a time-sliced one cannot divide by a zero step.
    got = get_time_series(
        SingleTimeSeries, store, 1, Component, "micro"; resolution=Millisecond(0)
    )
    @test length(got.data) == 4
    @test_throws InfraStore.InvalidParameterError get_time_series(
        SingleTimeSeries,
        store,
        1,
        Component,
        "micro";
        resolution=Millisecond(0),
        time_range=(t, t + Second(1)),
    )
end

@testset "pre-1970 and far-future timestamps round trip" begin
    mktempdir() do dir
        path = joinpath(dir, "spans.nc")
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
        path = joinpath(dir, "century.nc")
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
        SingleTimeSeries(t0, Hour(1), Float64[1, 2, 3, 4], "load"; ext="Profile");
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
    feats = Dict{String,Any}("scenario" => "high")
    read_md() =
        get_metadata(store, 1, Component, "load"; resolution=Hour(1), features=feats)
    md = read_md()
    @test md isa TimeSeriesMetadata
    @test md.owner_type == "Generator"
    @test md.owner_category == Component
    @test md.time_series_type == SingleTimeSeries
    # Fields that do not apply to a static series are `nothing`, not zero.
    @test md.horizon === nothing && md.count === nothing && md.percentiles === nothing
    @test md.dtype == Float64
    @test md.element_shape == ()
    @test md.ext == "Profile"
    @test md.units == "MW"
    @test md.features == feats
    # Value equality (a NamedTuple had it; a plain struct with a Vector and a
    # Dict field would fall back to identity without the generated `==`).
    @test md == read_md()
    @test length(Set([md, read_md()])) == 1
    @test occursin("TimeSeriesMetadata(owner_id=", repr(md))
    @test occursin(bytes2hex(md.data_hash), repr(md))

    fmd = get_forecast_metadata(
        store, 1, Component, "fc", InfraStore.INFRASTORE_TYPE_DETERMINISTIC
    )
    @test fmd isa TimeSeriesMetadata
    @test fmd.horizon == Millisecond(Hour(2))
    @test fmd.count == 2
    # The forecast getters carry `dtype` and `owner_type` too — one export, one
    # struct, so no field is dropped by the addressing path taken.
    @test fmd.dtype == Float64
    @test fmd.owner_type == "Generator"

    # The same record, addressed by key rather than by attributes.
    fkey = resolve_forecast_key(
        store, 1, Component, "fc", InfraStore.INFRASTORE_TYPE_DETERMINISTIC
    )
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
    @test mrow.dtype == Float64
    @test mrow.element_shape == ()
    @test mrow.percentiles === nothing
    @test mrow.units == "MW"
    @test mrow.ext == "Profile"
    @test mrow.data_hash == md.data_hash
    # list_time_series and get_metadata are two paths to the same record.
    @test mrow == md

    # Counts and summaries.
    @test get_counts(store) isa TimeSeriesCounts
    @test time_series_counts(store) isa TimeSeriesCountsDetailed
    @test time_series_counts(store).supplemental_attributes_with_time_series == 1
    @test counts_by_type(store) ==
        [TimeSeriesTypeCount(Deterministic, 1), TimeSeriesTypeCount(SingleTimeSeries, 2)]
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
    grid = only(check_static_consistency(store))
    @test grid == StaticGrid(t0, Millisecond(Hour(1)), 4)
    @test static_grid(build_static_reader(store; resolution=Hour(1))) == grid

    @test forecast_timeline(
        build_forecast_reader(store, Deterministic; resolution=Hour(1))
    ) == ForecastTimeline(t0, Millisecond(Hour(1)), Millisecond(Hour(1)), 2)

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
            ext="percentile-ext",
        ),
    )
    pmd = get_probabilistic_metadata(store, 1, Component, "pf")
    @test pmd isa TimeSeriesMetadata
    @test pmd.percentiles == [0.1, 0.9]
    @test pmd == get_probabilistic_metadata(store, 1, Component, "pf")
    # `ext` reaches a Probabilistic: the metadata surface no longer drops fields
    # depending on which getter was called.
    @test pmd.ext == "percentile-ext"
    @test pmd.dtype == Float64
    @test pmd == only(list_time_series(store))

    row = only(list_time_series(store))
    @test row.percentiles == [0.1, 0.9]
    @test row.time_series_type == Probabilistic
end
