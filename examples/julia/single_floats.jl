"""
A day of hourly power values per component: the everyday `SingleTimeSeries`.

A `SingleTimeSeries` is a value at every step of a regular grid — an initial
timestamp, a resolution, and a dense array. It is what a modeler reaches for
when a component's field varies over the simulation horizon: a thermal unit's
usable capacity, a solar plant's availability, a feeder's demand.

The three series below make the same point three ways, and deliberately differ
in `unit_system`:

  * the gas turbine's capacity is stored in MW (`NaturalUnits`);
  * the PV plant's output is stored per-unit on its own 60 MW base power
    (`ComponentBase`);
  * the feeder load is stored in MW again.

Nothing in the store rescales anything. `unit_system` is a declaration about
what the numbers already are, so a consumer reading a `ComponentBase` series
knows it needs the owning component's base power to get back to MW. In
particular a per-unit series here is per-unit because that is the basis its
values are on, not because it is a normalized shape awaiting a scale factor —
the store applies no scaling factors, so anything that needs scaling should be
scaled before it is written.
"""

using Dates
using InfraStore

include("shared.jl")

# (component, values, units, unit_system, component_field) — one hourly day each.
#
# All three series are *named* "max_active_power", the usual name for a series
# that bounds a component's output. The field each one actually drives differs
# by component type, and `component_field` is where that is recorded: a solar
# plant's ceiling is its `rating`, while the load and the thermal unit each have
# a `max_active_power` of their own.
inputs = [
    (SOLITUDE, thermal_capacity_mw(SOLITUDE.base_power_mw), "MW", NaturalUnits,
     "max_active_power"),
    (SUNDANCE_PV, solar_availability(), "per_unit", ComponentBase, "rating"),
    (BUS_A_LOAD, feeder_load_mw(BUS_A_LOAD.base_power_mw), "MW", NaturalUnits,
     "max_active_power"),
]

# The do-block form, which every example here uses: the store is released on the
# way out, including on a throw. A `Store` registers a finalizer too, so nothing
# leaks either way — but the GC decides *when*, which on disk means an open file
# handle and a held SQLite write lock for an unbounded stretch. `close!(store)`
# is the explicit form for a store that outlives a block.
Store(in_memory = true) do store
    series_ids = Int64[]
    for (component, values, units, unit_system, component_field) in inputs
        series = SingleTimeSeries(
            DAY_START,
            Hour(1),        # resolution: one value per hour
            values,
            # The series name is part of its identity; `component_field` says what
            # the values are *for*. They often coincide, and here they deliberately
            # do not.
            "max_active_power";
            units = units,
            quantity_kind = "ActivePower",
            unit_system = unit_system,
            component_field = component_field,
        )
        id = add_time_series!(store, component.id, component.type, Component, series)
        push!(series_ids, id)
        println("added $(component.type) '$(component.name)': id=$id")
    end

    # The id returned by the write is the only way to address a stored series. A
    # consumer records it on its own object (a generator holding the id of the
    # series that varies its capacity) and reads it back with it.
    println("\nnothing but the ids is needed to read the values back:")
    for (metadata, series) in
        zip(list_metadata_by_ids(store, series_ids), read_by_ids(store, series_ids))
        println("\nid=$(metadata.id) owner=$(metadata.owner_type)/$(metadata.owner_id)" *
                " field=$(metadata.component_field)" *
                " $(metadata.units) ($(metadata.unit_system))")
        println(static_frame(series))
    end
end
