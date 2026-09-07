"""
A value that holds until it changes: PersistentTimeSeries.

A `PersistentTimeSeries` is a **sparse step function**: breakpoints plus one
value each, where the value at an instant is the one belonging to the greatest
breakpoint at or before it. It is what a quantity that changes at irregular
moments and stays put in between actually is — a fuel contract price, a
commitment status, a tariff, a seasonal rating.

It is structurally identical to a `NonSequentialTimeSeries` — the same
timestamps-plus-values pair, sharing the same stored arrays — and differs
entirely in **read semantics**, which is why it is a separate type rather than a
flag. An irregular series says "a value at these instants and nowhere else"; a
step function says "this value from here until the next breakpoint". Storing a
fuel price as a `NonSequentialTimeSeries` would make 11:30 have no price, which
is wrong: the 09:00 nomination is still in force.

Two consequences follow, and both are deliberate:

  * the value is held forward **past the last breakpoint**, indefinitely — the
    last nomination stands until someone files another one;
  * it is **undefined before the first**, and asking is an error rather than a
    clamp, because nothing was in force yet.

This is an infrastore-local type: it sits outside the six Sienna defines, so it
does not travel in an OpenAPI document in either direction.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

Store(in_memory = true) do store
    # Intraday gas re-nominations: the delivered price the turbine is burning
    # against, changing only when a new nomination takes effect. Four changes in a
    # day — nothing like a value per hour, which is the point.
    nominations = [
        (Minute(6 * 60), 3.50),        # 06:00 morning cycle nomination
        (Minute(9 * 60), 4.10),        # 09:00 intraday 1: cooling load lifts demand
        (Minute(13 * 60 + 30), 5.25),  # 13:30 intraday 2: pipeline constraint binds
        (Minute(19 * 60), 4.40),       # 19:00 evening: constraint clears
    ]

    breakpoints = [DAY_START + offset for (offset, _) in nominations]
    prices = [price for (_, price) in nominations]

    series = PersistentTimeSeries(
        breakpoints,
        prices,
        "fuel_price";
        units = "\$/MMBtu",
        quantity_kind = "CostPerEnergy",
        unit_system = NaturalUnits,
        component_field = "fuel_cost",
        # A step function's scalar-collapse policy — what a consumer does when it
        # needs one number for a whole period — is the application's business and
        # rides here. The store has no column for it and never acts on it.
        application_data = """{"collapse": "time_weighted_mean"}""",
    )
    series_id = add_time_series!(store, SOLITUDE.id, SOLITUDE.type, Component, series)
    println("added $(SOLITUDE.type) '$(SOLITUDE.name)': id=$series_id")

    metadata = get_metadata_by_id(store, series_id)
    # Like the irregular type it shares storage with, a step function records no
    # resolution: its breakpoints are its own, and there is no grid to describe.
    println("resolution=$(metadata.resolution) length=$(metadata.length) " *
            "type=$(metadata.time_series_type)")

    read_back = read_by_id(store, series_id)
    println("\nthe four breakpoints as stored")
    println(static_frame(read_back))

    # `value_at` is the whole point: any instant, not just a breakpoint. 11:30 is not
    # in the array, and its price is the 09:00 nomination carried forward.
    probes = [DAY_START + Minute(m) for m in
              (6 * 60, 11 * 60 + 30, 13 * 60 + 30, 18 * 60, 23 * 60 + 59)]
    println("\nthe price in force at an arbitrary instant")
    println(DataFrame(
        instant = probes,
        price = [value_at(read_back, at) for at in probes],
        # The breakpoint the value came from — generally earlier than the instant,
        # which is the asymmetry that makes this a lookup rather than an index.
        in_force_since = [breakpoint_at(read_back, at) for at in probes],
    ))

    # Before the first breakpoint there is no value to carry forward, and the store
    # says so rather than inventing one.
    too_early = DAY_START + Minute(5 * 60 + 59)
    try
        value_at(read_back, too_early)
    catch err
        println("\nasking before the first breakpoint:")
        println("  $(typeof(err)): $(err.msg)")
    end
end
