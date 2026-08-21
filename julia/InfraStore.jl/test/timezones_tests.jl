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

    # And a reader reads at one.
    sts_key = add_time_series!(
        store, 3, "Generator", Component,
        SingleTimeSeries(DateTime(2024, 1, 1), Hour(1), collect(10.0:13.0), "load"),
    )
    reader = build_static_reader(store; resolution=Hour(1), owner_id=3)
    static_read!(reader, ZonedDateTime(DateTime(2023, 12, 31, 19), denver))  # = 2024-01-01T02:00Z
    @test static_values(reader, 1)[1] == 12.0
    @test has_time_series(store, sts_key)
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
