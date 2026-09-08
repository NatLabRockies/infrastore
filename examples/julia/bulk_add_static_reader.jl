"""
A fleet ingested in one transaction, then swept hour by hour with a reader.

Two habits that matter once a store holds more than a handful of series:

  * **Write inside a transaction.** Outside one, every `add_time_series!`
    commits its own catalog row and flushes the HDF5 file first, so a row can
    never name bytes the file did not receive — and that flush costs about the
    same for one array as for a thousand. Each add also lands as a column of
    its own in the file, one dataset per call. Inside a `transaction` each add
    is a savepoint instead: the flush happens once at commit, the adds the span
    covers are buffered and written together as one block sized to them — the
    layout `add_time_series_bulk!` produces — and either every series of the
    fleet lands or none does. The loop stays a plain loop.

  * **Read per timestamp with a `StaticReader`, not per series.** A simulation
    walks the clock and wants, at each instant, the value of every series at
    once. `read_by_ids` hands back whole arrays; a reader is built once over a
    filter, pins one timeline, and fills a preallocated columnar buffer on every
    `static_read!`, so the loop itself allocates almost nothing.

The sweep below is a reserve-margin check: at every hour of the day, the fleet's
available capacity against the load it has to serve. Solar is stored per-unit on
each plant's own base power (`unit_system = ComponentBase`), which is why the
loop needs the owning component and not just the series — infrastore never
rescales anything, so the multiply is the consumer's.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

# A fleet: eight gas turbines, six solar plants, five feeders. Each is a
# component in the consumer's object model with its own id and base power; the
# profiles come from `shared.jl` and are scaled per unit so the columns differ.
const THERMAL = [SystemComponent(110 + i, "ThermalGenerator", "ct_$i", 60.0 + 15.0 * i)
                 for i in 0:7]
const SOLAR = [SystemComponent(130 + i, "SolarPlant", "pv_$i", 40.0 + 10.0 * i) for i in 0:5]
const LOADS = [SystemComponent(210 + i, "Load", "feeder_$i", 90.0 + 25.0 * i) for i in 0:4]
const FLEET = Dict(c.id => c for c in vcat(THERMAL, SOLAR, LOADS))

"""One hourly day of `max_active_power` for a component, on the shared grid."""
function fleet_series(component::SystemComponent)
    if component.type == "ThermalGenerator"
        values = thermal_capacity_mw(component.base_power_mw)
        units, unit_system, field = "MW", NaturalUnits, "max_active_power"
    elseif component.type == "SolarPlant"
        # Per-unit on the plant's own base power: a cloudier site scales the
        # whole curve down, and the store keeps it exactly as given.
        cloudiness = 1.0 - 0.05 * (component.id % 3)
        values = round.(solar_availability() .* cloudiness, digits = 4)
        units, unit_system, field = "per_unit", ComponentBase, "rating"
    else
        values = feeder_load_mw(component.base_power_mw)
        units, unit_system, field = "MW", NaturalUnits, "max_active_power"
    end
    return SingleTimeSeries(
        DAY_START,
        Hour(1),
        values,
        "max_active_power";
        units = units,
        quantity_kind = "ActivePower",
        unit_system = unit_system,
        component_field = field,
    )
end

Store(in_memory = true) do store
    # One transaction around the whole fleet. The adds inside are the ordinary
    # one-series call; the transaction is what makes the fleet atomic — a
    # failure on the last feeder rolls every generator back too — what defers
    # the HDF5 flush to the single commit when the block returns, and what lets
    # the span's adds be written as one block per shape group that fills the
    # HDF5 chunks whole. A throw anywhere inside unwinds with a rollback and
    # rethrows.
    #
    # An `AddBatch` + `add_time_series_bulk!` is the same write, spelled for a
    # cohort already in hand rather than produced by a loop.
    series_ids = transaction(store) do
        ids = Int64[]
        for group in (THERMAL, SOLAR, LOADS)
            for component in group
                push!(ids, add_time_series!(store, component.id, component.type, Component,
                                            fleet_series(component)))
            end
            println("added $(lpad(length(group), 2)) $(group[1].type) series")
        end
        ids
    end
    println("committed $(length(series_ids)) series in one transaction: " *
            "ids $(first(series_ids))..$(last(series_ids))")

    # Build the reader once, outside the loop. The filter is the same identity
    # vocabulary `list_metadata` takes; the resolution pins the grid, and every
    # matched series must lie on it — a stray series on another grid is refused
    # here, by name, rather than silently dropped.
    reader = build_static_reader(store; resolution = Hour(1), name = "max_active_power")
    grid = static_grid(reader)
    println("\nreader: $(grid.length) steps of $(pretty(grid.resolution)) " *
            "from $(grid.initial_timestamp)")

    # Columns are grouped by (dtype, element_shape); every series here is a
    # scalar Float64, so there is one group and its `ids` give the column order.
    # Resolve each id to its component *once*, here, so the loop does no catalog
    # lookups.
    group = only(static_groups(reader))
    columns = [FLEET[get_metadata_by_id(store, id).owner_id] for id in group.ids]
    is_thermal = [c.type == "ThermalGenerator" for c in columns]
    is_solar = [c.type == "SolarPlant" for c in columns]
    is_load = [c.type == "Load" for c in columns]
    base_power = [c.base_power_mw for c in columns]

    # The sweep. `static_timestamps` enumerates the reader's own axis, so this
    # loop is the same for a regular grid and for an irregular cohort;
    # `static_read!` fills the buffer in place and `static_values` hands it back
    # shaped (num_columns, element_dims...) — here just a Vector, one entry per
    # column. The axis is zoneless because `DAY_START` is a bare `DateTime`, so
    # a bare `DateTime` is also how it is queried.
    rows = DataFrame(hour = Int[], thermal_mw = Float64[], solar_mw = Float64[],
                     load_mw = Float64[], margin_pct = Float64[])
    for t in static_timestamps(reader)
        static_read!(reader, t)
        values = static_values(reader, 1)
        thermal_mw = sum(values[is_thermal])
        solar_mw = sum(values[is_solar] .* base_power[is_solar])   # per-unit -> MW
        load_mw = sum(values[is_load])
        push!(rows, (
            hour(t),
            round(thermal_mw, digits = 1),
            round(solar_mw, digits = 1),
            round(load_mw, digits = 1),
            round(100.0 * (thermal_mw + solar_mw - load_mw) / load_mw, digits = 1),
        ))
    end

    println("\nhourly reserve margin across the fleet:")
    show(stdout, rows; allrows = true)
    println()
end
