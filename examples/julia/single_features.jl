"""
Features: many series for one component field, told apart by their tags.

A capacity-expansion or resource-adequacy study rarely has *one* load forecast.
It has the same feeder's demand under several demand-growth scenarios, drawn
from several historical weather years, for several model years. All of them are
`max_active_power` on the same load component, so the name alone cannot separate
them.

Features do. A feature map is part of a series' *identity*: two series that
differ only by a feature are two distinct series, not a duplicate write. They
are also the query vocabulary — `list_metadata(store; features = ...)` selects
the subset a run wants, which is how a study picks "the 2012 weather year under
high electrification" out of a store holding all of them.

Feature values may be `Int`, `Float64`, `Bool`, or `String` — the JSON scalars,
since the map is serialized. Names that would collide with a field of a series
(`name`, `resolution`, `units`, …) are rejected, because consumers routinely
splat a feature map into keyword arguments.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

# Growth multipliers applied to the same underlying feeder shape. A real study
# would read a distinct profile per combination; the point here is the tagging.
const SCENARIOS = ["reference" => 1.00, "high_electrification" => 1.18]
const WEATHER_YEARS = (2007, 2012)

Store(in_memory = true) do store
    for (scenario, growth) in SCENARIOS, weather_year in WEATHER_YEARS
        # A hotter weather year lifts the evening air-conditioning peak.
        peak = BUS_A_LOAD.base_power_mw * growth * (weather_year == 2012 ? 1.04 : 1.0)
        series = SingleTimeSeries(
            DAY_START,
            Hour(1),
            feeder_load_mw(peak),
            "max_active_power";
            units = "MW",
            quantity_kind = "ActivePower",
            unit_system = NaturalUnits,
            component_field = "max_active_power",
        )
        id = add_time_series!(
            store, BUS_A_LOAD.id, BUS_A_LOAD.type, Component, series;
            features = Dict(
                "scenario" => scenario,
                "weather_year" => weather_year,
                "model_year" => 2030,
            ),
        )
        println("added $scenario/$weather_year: id=$id")
    end

    # All four series share one owner, one name, and one grid; only the tags differ.
    println("\nseries stored for $(BUS_A_LOAD.name): $(length(list_metadata(store)))")

    # A study run selects the combination it models. Features not named in the
    # filter are unconstrained, so this matches both weather years.
    matches = list_metadata(
        store;
        owner_id = BUS_A_LOAD.id,
        owner_category = Component,
        features = Dict("scenario" => "high_electrification"),
    )

    println("\nhigh-electrification rows")
    println(DataFrame(
        id = [m.id for m in matches],
        owner_id = [m.owner_id for m in matches],
        name = [m.name for m in matches],
        features = [m.features for m in matches],
        length = [m.length for m in matches],
    ))

    for metadata in matches
        series = read_by_id(store, metadata.id)
        peak_mw = maximum(series.data)
        println("\nid=$(metadata.id) features=$(metadata.features) peak=$peak_mw MW")
        println(first(static_frame(series), 3))
    end
end
