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
    @test get_time_series(store, key).initial_timestamp == DateTime(2024, 1, 1, 14)

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
    got = get_time_series(NonSequentialTimeSeries, store, key)
    @test got.timestamps ==
        [DateTime(2024, 1, 1, 7), DateTime(2024, 1, 1, 13), DateTime(2024, 1, 1, 19)]

    # A time_range built from ZonedDateTimes selects by instant.
    sliced = get_time_series(
        NonSequentialTimeSeries, store, key;
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1, 6), denver),
            ZonedDateTime(DateTime(2024, 1, 1, 12), denver),
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
    @test has_time_series(store, sts_key)

    # A wall clock against that same instant-bearing axis is refused, as it is
    # on a ranged read.
    @test_throws InfraStore.InvalidParameterError static_read!(
        reader, DateTime(2024, 1, 1, 9)
    )
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

    got = get_time_series(store, key)
    # The read is unchanged: a `DateTime` holding the instant.
    @test got.initial_timestamp == DateTime(2024, 1, 1, 14)
    @test got.time_reference == ZoneReference("America/Denver")
    # And the two halves put back together are the value that was written --
    # this is what makes recording the spelling lossless rather than decorative.
    @test zoned_timestamp(got) == zoned

    # The catalog surfaces report it too.
    @test get_metadata(store, key).time_reference == ZoneReference("America/Denver")
    @test list_keys(store)[1].time_reference == ZoneReference("America/Denver")
    @test list_time_series(store)[1].time_reference == ZoneReference("America/Denver")
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
        got = get_time_series(store, key)
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
    sliced = get_time_series(
        store, zoned_key;
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1, 8), tz"UTC"),
            ZonedDateTime(DateTime(2024, 1, 1, 10), tz"UTC"),
        ),
    )
    @test sliced.initial_timestamp == DateTime(2024, 1, 1, 8)

    # A wall-clock bound against a series that records instants is a category
    # error, not a rounding one, so it is refused rather than coerced.
    @test_throws InfraStore.InvalidParameterError get_time_series(
        store, zoned_key;
        time_range=(DateTime(2024, 1, 1, 1), DateTime(2024, 1, 1, 3)),
    )
    # And the mirror image.
    @test_throws InfraStore.InvalidParameterError get_time_series(
        store, naive_key;
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1, 1), tz"UTC"),
            ZonedDateTime(DateTime(2024, 1, 1, 3), tz"UTC"),
        ),
    )

    # A range is one request, so both of its bounds have to agree on a spelling.
    @test_throws InfraStore.InvalidParameterError get_time_series(
        store, zoned_key;
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
    @test_throws InfraStore.InvalidParameterError bulk_read(
        store, [zoned_key, naive_key];
        time_range=(
            ZonedDateTime(DateTime(2024, 1, 1), tz"UTC"),
            ZonedDateTime(DateTime(2024, 1, 1, 3), tz"UTC"),
        ),
    )
    # Unranged, there is nothing for them to disagree about.
    @test length(bulk_read(store, [zoned_key, naive_key])) == 2

    # A reader materializes one timestamp axis, so a mixed cohort has no
    # spelling for it -- and the refusal is at build time, where the message can
    # name the series that disagree.
    @test_throws InfraStore.InvalidParameterError build_static_reader(
        store; resolution=Hour(1)
    )
    # `zoneless` is the constructive half: it is how a caller builds a coherent
    # selection instead of merely being told theirs is not.
    @test build_static_reader(store; resolution=Hour(1), zoneless=false) isa Any
    @test length(list_keys(store; zoneless=true)) == 1
    @test length(list_keys(store; zoneless=false)) == 1
end

@testset "a zoneless series has no ZonedDateTime to hand back" begin
    naive = SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(1.0:2.0), "naive")
    @test_throws InfraStore.InvalidParameterError zoned_timestamp(naive)
    @test is_zoneless(naive.time_reference)
    @test !is_zoneless(nothing)
end
