# ZonedDateTime input, provided by the `InfraStoreTimeZonesExt` extension.
#
# Included from `runtests.jl` only when TimeZones is loadable — see the note
# there. TimeZones is already loaded by the time this file is included; the
# `using` below is what makes `tz"..."` and `ZonedDateTime` visible here.

using Test
using Dates
using InfraStore
using TimeZones

@testset "a ZonedDateTime names an instant and is converted" begin
    store = Store(in_memory=true)

    # 07:00 in Denver's winter offset is 14:00 UTC. The store holds the instant,
    # and reads return the UTC DateTime for it -- reads are unchanged.
    zoned = ZonedDateTime(DateTime(2024, 1, 1, 7), tz"America/Denver")
    key = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(zoned, Hour(1), collect(1.0:3.0), "load"),
    )
    @test read_by_id(store, key).initial_timestamp == DateTime(2024, 1, 1, 14)

    # The same instant written any other way is the same instant.
    @test InfraStore._utc_datetime(ZonedDateTime(DateTime(2024, 1, 1, 14), tz"UTC")) ==
        DateTime(2024, 1, 1, 14)
end

@testset "ZonedDateTime works wherever a timestamp does" begin
    store = Store(in_memory=true)
    denver = tz"America/Denver"

    # An explicit timestamp vector, given in a non-UTC zone.
    stamps = [ZonedDateTime(DateTime(2024, 1, 1, h), denver) for h in (0, 6, 12)]
    key = add_time_series!(
        store, 2, "Generator", Component,
        NonSequentialTimeSeries(stamps, collect(1.0:3.0), "events"),
    )
    got = read_by_id(store, key)
    @test got.timestamps ==
        [DateTime(2024, 1, 1, 7), DateTime(2024, 1, 1, 13), DateTime(2024, 1, 1, 19)]

    # A time_range built from ZonedDateTimes selects by instant.
    sliced = only(
        read_by_ids(
            store, [key];
            time_range=(
                ZonedDateTime(DateTime(2024, 1, 1, 6), denver),
                ZonedDateTime(DateTime(2024, 1, 1, 12), denver),
            ),
        ),
    )
    @test sliced.timestamps == [DateTime(2024, 1, 1, 13)]

    # And a reader reads at one -- against an axis that records instants. The
    # series is anchored on a ZonedDateTime, so its axis is spelled
    # `ZoneReference("America/Denver")` and an instant-bearing point read is the
    # matching spelling. (It used to be built from a bare `DateTime`, giving a
    # *wall-clock* axis that an instant cannot be mapped onto -- the same
    # category error the ranged reads refuse, which the point read simply was
    # not checking.)
    sts_key = add_time_series!(
        store, 3, "Generator", Component,
        SingleTimeSeries(
            ZonedDateTime(DateTime(2024, 1, 1), denver),  # = 2024-01-01T07:00Z
            Hour(1), collect(10.0:13.0), "load",
        ),
    )
    reader = build_static_reader(store; resolution=Hour(1), owner_id=3)
    @test static_grid(reader).time_reference == ZoneReference("America/Denver")
    static_read!(reader, ZonedDateTime(DateTime(2024, 1, 1, 2), denver))  # = 09:00Z
    @test static_values(reader, 1)[1] == 12.0
    @test association_exists(store, sts_key)

    # A wall clock against that same instant-bearing axis is refused, as it is
    # on a ranged read.
    @test_throws InfraStore.InvalidParameterError static_read!(
        reader, DateTime(2024, 1, 1, 9)
    )

    # An axis that recorded no spelling (`time_reference=nothing`, the shape a
    # legacy row arrives in) groups with the zoned ones, as in the core: a
    # ZonedDateTime reads it, and a bare DateTime is refused -- the same verdict
    # `read_by_id(...; start_time=)` reaches through the core on that series.
    ukey = add_time_series!(
        store, 4, "Generator", Component,
        SingleTimeSeries(
            DateTime(2024, 1, 1, 7), Hour(1), collect(20.0:23.0), "load";
            time_reference=nothing,
        ),
    )
    ureader = build_static_reader(store; resolution=Hour(1), owner_id=4)
    @test static_grid(ureader).time_reference === nothing
    static_read!(ureader, ZonedDateTime(DateTime(2024, 1, 1, 2), denver))  # = 09:00Z
    @test static_values(ureader, 1)[1] == 22.0
    @test_throws InfraStore.InvalidParameterError static_read!(
        ureader, DateTime(2024, 1, 1, 9)
    )
    @test_throws InfraStore.InvalidParameterError read_by_id(
        store, ukey; start_time=DateTime(2024, 1, 1, 9), len=1
    )
    @test read_by_id(
        store, ukey; start_time=ZonedDateTime(DateTime(2024, 1, 1, 2), denver), len=1
    ).data == [22.0]
end

@testset "an irregular vector is ordered by instant, not by wall clock" begin
    # Two ZonedDateTimes whose local readings ascend but whose instants do not.
    # Normalizing before the monotonicity check is what makes this an error
    # rather than a store nothing can read back in order.
    stamps = [
        ZonedDateTime(DateTime(2024, 1, 1, 12), tz"UTC"),          # 12:00Z
        ZonedDateTime(DateTime(2024, 1, 1, 13), tz"America/Denver"), # 20:00Z
        ZonedDateTime(DateTime(2024, 1, 1, 14), tz"Asia/Tokyo"),   # 05:00Z -- earlier
    ]
    @test_throws InfraStore.InvalidParameterError NonSequentialTimeSeries(
        stamps, collect(1.0:3.0), "mixed"
    )
end

@testset "a ZonedDateTime records the spelling its zone names" begin
    # Three zoned spellings, discriminated by the zone's *type* and name rather
    # than by its offset: `tz"UTC"` and `tz"+00:00"` place every instant
    # identically, and the point of recording a spelling is telling them apart.
    utc = SingleTimeSeries(
        ZonedDateTime(DateTime(2024, 1, 1), tz"UTC"), Hour(1), collect(1.0:2.0), "utc"
    )
    @test utc.time_reference == UTCReference()

    offset = SingleTimeSeries(
        ZonedDateTime(DateTime(2024, 1, 1), tz"-07:00"), Hour(1), collect(1.0:2.0), "off"
    )
    @test offset.time_reference == FixedOffsetReference(-420)

    denver = SingleTimeSeries(
        ZonedDateTime(DateTime(2024, 1, 1), tz"America/Denver"),
        Hour(1), collect(1.0:2.0), "den",
    )
    @test denver.time_reference == ZoneReference("America/Denver")

    # A bare DateTime is a wall clock, which is a different claim from any of
    # the three above.
    @test SingleTimeSeries(
        DateTime(2024, 1, 1), Hour(1), collect(1.0:2.0), "naive"
    ).time_reference == ZonelessReference()
end

@testset "the spelling survives a store round trip and fuses back" begin
    store = Store(in_memory=true)
    zoned = ZonedDateTime(DateTime(2024, 1, 1, 7), tz"America/Denver")
    key = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(zoned, Hour(1), collect(1.0:3.0), "load"),
    )

    got = read_by_id(store, key)
    # The read is unchanged: a `DateTime` holding the instant.
    @test got.initial_timestamp == DateTime(2024, 1, 1, 14)
    @test got.time_reference == ZoneReference("America/Denver")
    # And the two halves put back together are the value that was written --
    # this is what makes recording the spelling lossless rather than decorative.
    @test zoned_timestamp(got) == zoned

    # The catalog surfaces report it too.
    @test get_metadata_by_id(store, key).time_reference == ZoneReference("America/Denver")
    @test list_metadata(store)[1].time_reference == ZoneReference("America/Denver")
    @test list_metadata(store)[1].time_reference == ZoneReference("America/Denver")
end

@testset "the fold of an ambiguous local hour survives the round trip" begin
    # The instant plus the zone name reconstructs which side of the fall-back
    # hour a value was on. Both wall clocks read 01:00 in Denver; they are two
    # distinct instants, and each comes back as itself.
    store = Store(in_memory=true)
    denver = tz"America/Denver"
    for (owner, offset_hours) in ((10, 6), (11, 7))
        instant = DateTime(2020, 11, 1, offset_hours + 1)
        zoned = astimezone(ZonedDateTime(instant, tz"UTC"), denver)
        key = add_time_series!(
            store, owner, "Generator", Component,
            SingleTimeSeries(zoned, Hour(1), collect(1.0:2.0), "load"),
        )
        got = read_by_id(store, key)
        @test got.initial_timestamp == instant
        @test zoned_timestamp(got) == zoned
    end
end

@testset "a query bound must be spelled the way the series is" begin
    store = Store(in_memory=true)
    denver = tz"America/Denver"
    zoned_key = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(
            ZonedDateTime(DateTime(2024, 1, 1), denver), Hour(1), collect(1.0:4.0), "zoned"
        ),
    )
    naive_key = add_time_series!(
        store, 2, "Generator", Component,
        SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:4.0), "naive"),
    )

    # An aware bound need not match the series' own offset: both name the same
    # instant, and slicing is instant arithmetic.
    sliced = only(
        read_by_ids(
            store, [zoned_key];
            time_range=(
                ZonedDateTime(DateTime(2024, 1, 1, 8), tz"UTC"),
                ZonedDateTime(DateTime(2024, 1, 1, 10), tz"UTC"),
            ),
        ),
    )
    @test sliced.initial_timestamp == DateTime(2024, 1, 1, 8)

    # A wall-clock bound against a series that records instants is a category
    # error, not a rounding one, so it is refused rather than coerced.
    @test_throws InfraStore.InvalidParameterError read_by_ids(
        store, [zoned_key];
        time_range=(DateTime(2024, 1, 1, 1), DateTime(2024, 1, 1, 3)),
    )
    # And the mirror image.
    @test_throws InfraStore.InvalidParameterError read_by_ids(
        store, [naive_key];
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1, 1), tz"UTC"),
            ZonedDateTime(DateTime(2024, 1, 1, 3), tz"UTC"),
        ),
    )

    # A range is one request, so both of its bounds have to agree on a spelling.
    @test_throws InfraStore.InvalidParameterError read_by_ids(
        store, [zoned_key];
        time_range=(ZonedDateTime(DateTime(2024, 1, 1), tz"UTC"), DateTime(2024, 1, 1, 3)),
    )
end

@testset "a selection cannot span both coherence groups" begin
    store = Store(in_memory=true)
    zoned_key = add_time_series!(
        store, 1, "Generator", Component,
        SingleTimeSeries(
            ZonedDateTime(DateTime(2024, 1, 1), tz"UTC"), Hour(1), collect(1.0:4.0), "load"
        ),
    )
    naive_key = add_time_series!(
        store, 2, "Generator", Component,
        SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:4.0), "load"),
    )

    # One bound cannot be valid for both groups, so a ranged bulk read over a
    # mixed selection is refused outright.
    @test_throws InfraStore.InvalidParameterError read_by_ids(
        store, [zoned_key, naive_key];
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1), tz"UTC"),
            ZonedDateTime(DateTime(2024, 1, 1, 3), tz"UTC"),
        ),
    )
    # Unranged, there is nothing for them to disagree about.
    @test length(read_by_ids(store, [zoned_key, naive_key])) == 2

    # A reader materializes one timestamp axis, so a mixed cohort has no
    # spelling for it -- and the refusal is at build time, where the message can
    # name the series that disagree.
    @test_throws InfraStore.InvalidParameterError build_static_reader(
        store; resolution=Hour(1)
    )
    # `zoneless` is the constructive half: it is how a caller builds a coherent
    # selection instead of merely being told theirs is not.
    @test build_static_reader(store; resolution=Hour(1), zoneless=false) isa Any
    @test length(list_metadata(store; zoneless=true)) == 1
    @test length(list_metadata(store; zoneless=false)) == 1
end

@testset "a zoneless series has no ZonedDateTime to hand back" begin
    naive = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:2.0), "naive")
    @test_throws InfraStore.InvalidParameterError zoned_timestamp(naive)
    @test is_zoneless(naive.time_reference)
    @test !is_zoneless(nothing)
    # `zoned_timestamps` fuses the same two halves over the whole grid, so it
    # refuses for the same reason.
    @test_throws InfraStore.InvalidParameterError zoned_timestamps(naive)
end

@testset "a calendar grid steps the stored calendar, not TimeZones.jl's local one" begin
    # Julia is the one binding whose date library steps a *local* clock for an
    # irregular period: `ZonedDateTime + Month(1)` lands on the same local
    # day-of-month, while the core adds a month to the stored UTC instant. The
    # two disagree by the DST offset across a transition, so `timestamps` has to
    # step the bare `DateTime` the series holds -- stepping the ZonedDateTime
    # would make the Julia binding disagree with the core and with every other
    # binding about which instants the series contains.
    d = tz"America/Denver"
    start = ZonedDateTime(DateTime(2024, 11, 1), d)
    s = SingleTimeSeries(start, Month(1), collect(1.0:3.0), "monthly")

    # The stored instants: a month added on the UTC calendar.
    @test timestamps(s) ==
        [DateTime(2024, 11, 1, 6), DateTime(2024, 12, 1, 6), DateTime(2025, 1, 1, 6)]

    # Rendered in Denver, the second and third land at 23:00 the day *before* --
    # the offset changed under them. This is the documented drift, not a bug.
    @test zoned_timestamps(s) == [
        ZonedDateTime(DateTime(2024, 11, 1, 0), d),
        ZonedDateTime(DateTime(2024, 11, 30, 23), d),
        ZonedDateTime(DateTime(2024, 12, 31, 23), d),
    ]

    # And that is *not* what stepping the local clock gives, which is the whole
    # point of pinning it.
    @test zoned_timestamps(s) != [start + Month(k) for k in 0:2]
end

@testset "zoned_timestamps fuses a SingleTimeSeries grid with its spelling" begin
    denver = tz"America/Denver"
    start = ZonedDateTime(DateTime(2024, 1, 1), denver)
    series = SingleTimeSeries(start, Hour(1), collect(1.0:3.0), "load")

    # The zoneless grid and the zoned one describe the same instants; the second
    # just carries the spelling the series recorded.
    @test zoned_timestamps(series) ==
        [zoned_timestamp(t, series.time_reference) for t in timestamps(series)]
    @test zoned_timestamps(series)[1] == start
    @test length(zoned_timestamps(series)) == 3
    @test all(t -> t isa ZonedDateTime, zoned_timestamps(series))

    # A PT1H grid steps instants, not wall clocks: across spring-forward the
    # local hour jumps 01:00 -> 03:00 while the instants stay an hour apart.
    dst = SingleTimeSeries(
        ZonedDateTime(DateTime(2024, 3, 10, 1), denver), Hour(1), collect(1.0:2.0), "dst"
    )
    stamps = zoned_timestamps(dst)
    @test Dates.hour(DateTime(stamps[1])) == 1
    @test Dates.hour(DateTime(stamps[2])) == 3
    @test stamps[2] - stamps[1] == Hour(1)

    # And it survives a store round trip, which is where the spelling is
    # actually reconstructed rather than merely carried in memory.
    store = Store(in_memory=true)
    key = add_time_series!(store, 1, "Generator", Component, series)
    @test zoned_timestamps(read_by_id(store, key)) == zoned_timestamps(series)
end

@testset "a refused point read leaves the reader empty" begin
    # The spelling check runs in Julia, before the ccall, because the ABI's
    # `at_unix_ms` cannot carry the bound's spelling. So the core never sees the
    # call and its own "a failed read empties the reader" rule never applies:
    # without the invalidation the mismatch threw while `static_values` went on
    # serving the window the *previous, successful* read had filled, as though it
    # answered the timestamp that had just been refused.
    values = collect(1.0:4.0)
    initial = DateTime(2024, 1, 1)

    # A wall-clock axis reads a wall clock, and refuses an instant.
    wall = Store(in_memory=true)
    add_time_series!(
        wall, 1, "Generator", Component,
        SingleTimeSeries(initial, Hour(1), values, "load"),
    )
    r = build_static_reader(wall; resolution=Hour(1))
    static_read!(r, DateTime(2024, 1, 1, 1))
    @test static_values(r, 1) == [2.0]
    @test_throws InfraStore.InvalidParameterError static_read!(
        r, ZonedDateTime(DateTime(2024, 1, 1, 2), tz"UTC")
    )
    @test isempty(static_values(r, 1))
    # The reader still works afterwards.
    static_read!(r, DateTime(2024, 1, 1, 2))
    @test static_values(r, 1) == [3.0]

    # The same for a forecast reader, in the other direction: an instant-bearing
    # timeline refuses a bare DateTime.
    fc = Store(in_memory=true)
    add_time_series!(
        fc, 1, "Generator", Component,
        Deterministic(
            ZonedDateTime(initial, tz"UTC"), Hour(1), Hour(2), Hour(1), 2,
            reshape(collect(1.0:4.0), 2, 2), "load",
        ),
    )
    fr = build_forecast_reader(fc, Deterministic; resolution=Hour(1))
    forecast_read!(fr, ZonedDateTime(initial, tz"UTC"))
    @test !isempty(forecast_values(fr, 1))
    @test_throws InfraStore.InvalidParameterError forecast_read!(
        fr, DateTime(2024, 1, 1, 1)
    )
    @test isempty(forecast_values(fr, 1))
end
