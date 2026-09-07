"""
One timestep, several numbers: fixed-width tuple values.

A series' value at a timestep does not have to be a scalar. `element_type` says
what one timestep's value *means*, and `tuple(N,f64)` declares a fixed-width
group of numbers that belong together.

The domain case here is complex power at the feeder: active power in MW and
reactive power in MVar, measured together and only meaningful together, since
the power factor is the relationship between them. Most models hold active and
reactive power as two separate component fields, and would carry two
`SingleTimeSeries`. Storing them as one `tuple(2,f64)` series is what a consumer
whose own object model holds a complex injection would do — the pair is written
once, read back once, and can never drift out of step or be filtered apart by
accident.

**In Julia there is nothing to declare.** A `Vector{NTuple{2,Float64}}` already
says what it is, so the constructor reads `element_type` off the values; passing
one that contradicts them is an error rather than an override. The two spellings
below store identical bytes and differ only in what the *caller* holds: a vector
of pairs, or a matrix whose trailing axis happens to be the element.
"""

using Dates
using InfraStore

include("shared.jl")

Store(in_memory = true) do store
    active_mw = feeder_load_mw(BUS_A_LOAD.base_power_mw)
    # A lagging feeder at roughly 0.95 power factor: Q = P * tan(acos(0.95)).
    reactive_mvar = round.(active_mw .* tan(acos(0.95)), digits = 2)

    # Each element is one (active_power, reactive_power) value.
    values = collect(zip(active_mw, reactive_mvar))

    series = SingleTimeSeries(
        DAY_START,
        Hour(1),
        values,
        "complex_power";
        units = "MW,MVar",
        # `quantity_kind` names one physical quantity, and a tuple of two kinds has
        # no honest single answer, so it is left unset here. That is a fair warning:
        # a consumer reading this series has to know the packing convention from
        # somewhere else — here, active power first, then reactive.
        unit_system = NaturalUnits,
        component_field = "active_power",
    )
    series_id = add_time_series!(store, BUS_A_LOAD.id, BUS_A_LOAD.type, Component, series)
    println("added $(BUS_A_LOAD.type) '$(BUS_A_LOAD.name)': id=$series_id")

    metadata = get_metadata_by_id(store, series_id)
    println("element_type=$(metadata.element_type) " *
            "element_shape=$(metadata.element_shape) length=$(metadata.length)")
    # The stored type is parameterized off the row, so it names the values rather
    # than their packing — and equals the type a read hands back.
    println("time_series_type=$(metadata.time_series_type)")

    read_back = read_by_id(store, series_id)
    println("\nhourly complex power at the feeder")
    println(first(static_frame(read_back), 6))

    # The same series from a matrix instead. `element_type=` is required here,
    # because plain numbers cannot say that a row of two is one value rather than
    # two samples — the declaration is doing work the tuple type did above.
    matrix = SingleTimeSeries(
        DAY_START,
        Hour(1),
        hcat(active_mw, reactive_mvar),
        "complex_power_from_matrix";
        element_type = "tuple(2,f64)",
        units = "MW,MVar",
        unit_system = NaturalUnits,
        component_field = "active_power",
    )
    matrix_id = add_time_series!(store, BUS_A_LOAD.id, BUS_A_LOAD.type, Component, matrix)

    # Identical bytes: the two rows share one stored array, and `data_hash` says so.
    println("\nsame values, two spellings — one stored array: ",
            get_metadata_by_id(store, matrix_id).data_hash == metadata.data_hash)
    # `raw = true` is how to get the packing back rather than the values.
    println("as values: ", eltype(read_by_id(store, series_id).data))
    println("as packed: ", size(read_by_id(store, series_id; raw = true).data))
end
