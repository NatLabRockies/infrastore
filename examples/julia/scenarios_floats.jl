"""
Ensembles instead of quantiles: Scenarios.

A `Scenarios` forecast has the same shape as a `Probabilistic` one — an extra
axis in front of (horizon steps, windows) — but the axis means something
different, and the difference matters to the optimization that consumes it.

A percentile is a marginal statement about *one* hour: "there is a 10% chance
this hour comes in below p10". Stacking p10 across every hour does not describe
any day that could actually happen, because the low hours are not independent.

A scenario *is* a day that could happen: one coherent trajectory, usually drawn
from a historical weather year or a numerical weather ensemble member. Hour 14
and hour 15 of a member are correlated because they came from the same weather.
That is what a stochastic unit-commitment model needs — it solves over whole
trajectories, not over per-hour quantiles. The three members below are three
weather days, written out rather than sampled, so the correlation within a member
is plain to see.

So the axis is unlabelled: scenarios are interchangeable members
(`scenario_count` of them), and unlike `Probabilistic` there is nothing to name
them with. Which weather day a member came from is the consumer's bookkeeping —
a `features` tag or `application_data` on the series, not a field the store owns.

A member is a trajectory *within one window*, and only there. Nothing ties
member 2 of the run issued at 06:00 to member 2 of the run issued at 12:00: they
are separate draws that happen to share an index, and stitching them together
would invent a correlation the forecast never claimed. So this example stores a
single 12-hour window (`count = 1`, and `interval` is the canonical zero a
one-window forecast has, since there is no second window to step to), which is
what makes the whole-day statistic at the bottom legitimate. A multi-window
ensemble is fine — see `deterministic_floats.jl` for what windows mean — but
each window's members have to be summarized on their own.
"""

using Dates
using DataFrames
using InfraStore

include("shared.jl")

const RESOLUTION = Hour(1)
const HORIZON = Hour(12)
const INTERVAL = Hour(0)   # a single-window forecast steps nowhere
const COUNT = 1
const FIRST_ISSUE_HOUR = 6

horizon_steps = Int(HORIZON / RESOLUTION)
availability = solar_availability()

# One cloud trajectory per member, hour by hour over the window. These are
# weather *days*, not per-hour draws: within a member the hours move together,
# which is exactly the structure a per-hour quantile throws away.
const MEMBERS = [
    # A clear day: the plant tracks its clear-sky profile all the way through.
    ("clear", fill(1.00, horizon_steps)),
    # Morning stratus that burns off by noon and leaves a clean afternoon.
    ("morning_cloud", [0.35, 0.30, 0.40, 0.65, 0.90, 1.00,
                       1.00, 1.00, 1.00, 1.00, 1.00, 1.00]),
    # A clear morning, then afternoon convection building into a thunderstorm.
    ("afternoon_storm", [1.00, 1.00, 1.00, 0.95, 0.85, 0.60,
                         0.35, 0.20, 0.25, 0.45, 0.70, 0.80]),
]
const SCENARIO_COUNT = length(MEMBERS)

values = Array{Float64}(undef, SCENARIO_COUNT, horizon_steps, COUNT)
for (member, (_, cloud)) in enumerate(MEMBERS), step in 1:horizon_steps
    hour = FIRST_ISSUE_HOUR + (step - 1)
    values[member, step, 1] =
        round(availability[hour % 24 + 1] * cloud[step], digits = 4)
end

Store(in_memory = true) do store
    forecast = Scenarios(
        DAY_START + Hour(FIRST_ISSUE_HOUR),
        RESOLUTION,
        HORIZON,
        INTERVAL,
        COUNT,
        values,
        "max_active_power";
        units = "per_unit",
        quantity_kind = "ActivePower",
        unit_system = ComponentBase,
        # What bounds a plant's output is usually its rating rather than a
        # separate max-power field, so that is the field these values drive.
        component_field = "rating",
    )
    series_id = add_time_series!(
        store, SUNDANCE_PV.id, SUNDANCE_PV.type, Component, forecast;
        # Which weather days the members came from is the consumer's record, not the
        # store's; a feature tag is the natural place for it.
        features = Dict("ensemble" => "ecmwf_2012", "model_year" => 2030),
    )
    println("added $(SUNDANCE_PV.type) '$(SUNDANCE_PV.name)': id=$series_id")

    read_back = read_by_id(store, series_id)
    println("scenario_count=$(read_back.scenario_count) count=$(read_back.count)")

    # MW from a 60 MW plant, one column per member.
    mw(member, step) =
        round(read_back.data[member, step, 1] * SUNDANCE_PV.base_power_mw, digits = 2)

    frame = DataFrame(timestamp = window_timestamps(read_back, 1))
    for (member, (label, _)) in enumerate(MEMBERS)
        frame[!, label] = [mw(member, s) for s in 1:horizon_steps]
    end
    println("\nensemble members, MW from a 60 MW plant")
    println(frame)

    # Each member is one plausible trajectory over this window, so summing it is a
    # per-member statistic — the thing you cannot compute from per-hour quantiles,
    # where "the p10 day" is not a day at all. Summing across windows instead would
    # be meaningless, which is why there is only one here.
    println("\nwindow energy per member (hourly steps, so MW sums to MWh)")
    println(DataFrame(
        member = [label for (label, _) in MEMBERS],
        energy_mwh = [round(sum(mw(m, s) for s in 1:horizon_steps), digits = 1)
                      for m in 1:SCENARIO_COUNT],
    ))
end
