//! The `from_values` snippets from `docs/src/guides/rust.md` and
//! `docs/src/reference/rust-api.md`, compiled and run.
//!
//! Doc prose drifts silently; a test does not. These mirror the published
//! snippets line for line, so an API change that invalidates one fails here.

use chrono::{Duration, TimeZone, Utc};
use infrastore_core::{
    DecodedValues, Deterministic, ElementType, Features, OwnerCategory, Period, ReadWindow,
    SingleTimeSeries, TimeSeriesData, XyPoint, create_store,
};

#[test]
fn the_rust_guide_element_value_snippets_work() {
    let initial = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

    // One input-output cost curve per hour; point counts may differ per timestep.
    let curves = DecodedValues::PiecewiseLinear(vec![
        vec![
            XyPoint { x: 30.0, y: 1155.0 },
            XyPoint {
                x: 100.0,
                y: 4120.0,
            },
        ],
        vec![
            XyPoint { x: 30.0, y: 1353.0 },
            XyPoint { x: 65.0, y: 2730.0 },
            XyPoint {
                x: 100.0,
                y: 4223.0,
            },
        ],
    ]);

    let ts = SingleTimeSeries::from_values(initial, Duration::hours(1), &curves, "variable_cost")
        .unwrap();
    assert_eq!(ts.element_type, ElementType::PiecewiseLinear);

    let mut store = create_store(None, true).unwrap();
    let ts_id = store
        .add_time_series(
            42,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(ts),
            Features::new(),
        )
        .unwrap();

    let data = store.read_by_id(ts_id, ReadWindow::full()).unwrap();
    assert_eq!(data.decoded_values().unwrap(), curves);

    // A plain numeric series decodes to `Raw`.
    let plain = SingleTimeSeries::new(
        initial,
        Duration::hours(1),
        infrastore_core::TypedArray::from_f64(vec![2], &[1.0, 2.0]),
        "load",
    );
    assert_eq!(
        TimeSeriesData::SingleTimeSeries(plain)
            .decoded_values()
            .unwrap(),
        DecodedValues::Raw
    );

    // H = 2 (a two-hour horizon at hourly resolution) x count = 2 windows = 4 curves.
    let DecodedValues::PiecewiseLinear(rows) = &curves else {
        unreachable!()
    };
    let curves4 = DecodedValues::PiecewiseLinear([rows.clone(), rows.clone()].concat());
    let forecast = Deterministic::from_values(
        initial,
        Duration::hours(1),
        Duration::hours(2),
        Duration::hours(1),
        2,
        &curves4,
        "offer",
    )
    .unwrap();
    assert_eq!(forecast.data.shape, vec![2, 2, 7]);
    assert_eq!(forecast.element_type, ElementType::PiecewiseLinear);
}

/// The `element_type_of` / `encode_as` pair the reference documents as the way
/// to name the one series `from_values` cannot: a tuple with no rows.
#[test]
fn the_reference_empty_tuple_escape_hatch_works() {
    let empty = DecodedValues::Tuple(vec![]);
    // `tuple(0,f64)` is what the values imply, and it is not a legal element type.
    assert_eq!(
        infrastore_core::element_type_of(&empty),
        Some(ElementType::Tuple {
            arity: 0,
            dtype: infrastore_core::Dtype::F64
        })
    );

    let declared: ElementType = "tuple(3,f64)".parse().unwrap();
    let array = infrastore_core::encode_as(&empty, &[0], declared).unwrap();
    assert_eq!(array.shape, vec![0, 3]);

    let series = SingleTimeSeries::new(
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        Period::Fixed(Duration::hours(1)),
        array,
        "coeffs",
    )
    .with_element_type(declared);
    assert_eq!(series.element_type, declared);

    // And the declaration is checked against the packing it produces.
    let two_wide = DecodedValues::Tuple(vec![vec![1.0, 2.0]]);
    assert!(infrastore_core::encode_as(&two_wide, &[1], declared).is_err());
}
