"""
Measurements at the instants they were taken: NonSequentialTimeSeries.

A `SingleTimeSeries` says "a value every hour". A `NonSequentialTimeSeries` says
"a value at *these* instants and nowhere else" — it carries its own timestamp
vector instead of a resolution, and it makes no claim at all about the instants
in between. That is the right shape for measured data rather than modelled data:
telemetry, meter reads, market clearings on a non-uniform schedule.

Here it is the PV plant's real-time telemetry across a cloud passage. The plant
reports every 15 minutes when its output is steady, and every minute while a
cloud is crossing it. Nothing is interpolated: at 09:07 the store has no value
for this series, and that is the honest answer rather than a resampled one. A
modeler who needs a value at every instant wants a regular grid, or a step
function whose value is held forward — see `persistent_floats.jl`.

The timestamp vector is content-addressed and shared: several plants reporting on
the same schedule store their timestamps once. Note that reads and queries carry
the series' own irregular axis — a `NonSequentialTimeSeries` has no `resolution`,
so anything that assumes a uniform grid does not apply to it.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

Store(in_memory = true) do store
    # Minutes past midnight of each report, and the per-unit output reported.
    # 15-minute cadence in clear sky; 1-minute cadence through the 09:35–09:40 cloud.
    reports = [
        (8 * 60 + 45, 0.61),
        (9 * 60 + 0, 0.65),
        (9 * 60 + 15, 0.68),
        (9 * 60 + 30, 0.71),
        (9 * 60 + 35, 0.44),  # cloud edge
        (9 * 60 + 36, 0.19),
        (9 * 60 + 37, 0.12),
        (9 * 60 + 38, 0.15),
        (9 * 60 + 39, 0.38),
        (9 * 60 + 40, 0.69),  # back in full sun
        (9 * 60 + 45, 0.72),
        (10 * 60 + 0, 0.75),
    ]

    report_times = [DAY_START + Minute(minute) for (minute, _) in reports]
    values = [per_unit for (_, per_unit) in reports]

    series = NonSequentialTimeSeries(
        report_times,
        values,
        "measured_active_power";
        units = "per_unit",
        quantity_kind = "ActivePower",
        # Per-unit of the plant's 60 MW rating, like the modelled availability series.
        unit_system = ComponentBase,
        component_field = "active_power",
    )
    series_id = add_time_series!(store, SUNDANCE_PV.id, SUNDANCE_PV.type, Component, series)
    println("added $(SUNDANCE_PV.type) '$(SUNDANCE_PV.name)': id=$series_id")

    metadata = get_metadata_by_id(store, series_id)
    # An irregular series records no resolution and no initial_timestamp: there is
    # no grid to describe. Its identity includes the hash of its timestamp vector.
    println("resolution=$(metadata.resolution) " *
            "initial_timestamp=$(metadata.initial_timestamp) length=$(metadata.length)")

    read_back = read_by_id(store, series_id)
    frame = static_frame(read_back)
    frame.mw = round.(SUNDANCE_PV.base_power_mw .* frame.value, digits = 2)
    println("\ntelemetry across the cloud passage")
    println(frame)

    # The gap between consecutive reports is data, not an artifact: it is how a
    # consumer sees that the plant switched to fast reporting.
    gaps = Minute.(diff(timestamps(read_back)))
    println("\nreporting gaps: $gaps")
end
