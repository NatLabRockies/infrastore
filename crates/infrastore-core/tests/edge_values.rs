//! Corner-case *values*: non-finite floats, empty / minimal arrays, extreme
//! integers, and hostile strings.
//!
//! Much of what is asserted here was previously **undefined** — no test said
//! whether a zero-length series or a 10 kB name is accepted or rejected. These
//! tests therefore *pin* the behavior the shipping code has today, with a
//! comment saying so; they are tripwires against silent drift, not a
//! specification anyone designed. Where the pinned behavior looks wrong it is
//! marked `// FINDING:` and recorded in `TEST_COVERAGE_PLAN.md` §9.
//!
//! Value round trips run through `common::for_each_backend` so the in-memory
//! and persisted-HDF5 paths are held to the same answer.

use chrono::{DateTime, Duration, TimeZone, Utc};
use infrastore_core::{
    AddRequest, Deterministic, Dtype, Features, ListFilter, NonSequentialTimeSeries, OwnerCategory,
    Probabilistic, Scenarios, SingleTimeSeries, Store, TimeSeriesData, TimeSeriesKey, TypedArray,
    create_store, open_store,
};

mod common;
use common::for_each_backend;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
}

fn add(store: &mut Store, owner: i64, data: TimeSeriesData) -> TimeSeriesKey {
    store
        .add(AddRequest::new(
            owner,
            "Generator",
            OwnerCategory::Component,
            data,
        ))
        .unwrap()
}

fn sts(name: &str, data: TypedArray) -> TimeSeriesData {
    TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(t0(), Duration::hours(1), data, name))
}

// ===========================================================================
// 1.1 Non-finite floats
// ===========================================================================

/// The f64 values that must survive a round trip bit-for-bit. `-0.0` is
/// included because it compares `==` to `0.0`, so only a byte comparison can
/// catch it being flattened.
fn nonfinite_f64() -> Vec<f64> {
    vec![
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
    ]
}

fn nonfinite_f32() -> Vec<f32> {
    vec![
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -0.0,
        0.0,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
    ]
}

#[test]
fn non_finite_f64_static_round_trips_bit_exact() {
    let values = nonfinite_f64();
    let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| add(store, 1, sts("nonfinite", data.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            // Byte comparison, not `==`: NaN != NaN and -0.0 == 0.0 would both
            // let a corrupted round trip pass.
            assert_eq!(
                single.data.bytes, data.bytes,
                "{backend}: f64 non-finite bytes changed"
            );
            let back = single.data.to_vec::<f64>().unwrap();
            assert!(back[0].is_nan(), "{backend}: NaN lost");
            assert_eq!(back[1], f64::INFINITY, "{backend}");
            assert_eq!(back[2], f64::NEG_INFINITY, "{backend}");
            assert!(
                back[3].is_sign_negative() && back[3] == 0.0,
                "{backend}: -0.0 lost its sign"
            );
        },
    );
}

#[test]
fn non_finite_f32_static_round_trips_bit_exact() {
    let values = nonfinite_f32();
    let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| add(store, 1, sts("nonfinite32", data.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            assert_eq!(single.data.dtype, Dtype::F32, "{backend}");
            assert_eq!(
                single.data.bytes, data.bytes,
                "{backend}: f32 non-finite bytes changed"
            );
        },
    );
}

#[test]
fn non_finite_deterministic_round_trips_bit_exact() {
    // H = 2, count = 3, so shape [2, 3] = 6 values.
    let values = vec![
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0,
        1.0,
        f64::MAX,
    ];
    let data = TypedArray::from_slice(vec![2, 3], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let det = Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(1),
                    3,
                    data.clone(),
                    "det_nonfinite",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Deterministic(det))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(
                det.data.bytes, data.bytes,
                "{backend}: forecast non-finite bytes changed"
            );
        },
    );
}

#[test]
fn differing_nan_bit_patterns_content_address_to_one_array() {
    // `hash.rs` canonicalizes every NaN to one quiet-NaN bit pattern before
    // hashing, so two arrays that differ *only* in which NaN payload they carry
    // are the same stored array. This pins that end-to-end through the store's
    // deduplication, not just at the hashing layer.
    let quiet = f64::NAN;
    let alt = f64::from_bits(0x7ff8_0000_0000_0001);
    let signaling = f64::from_bits(0xfff8_0000_dead_beef);
    assert!(alt.is_nan() && signaling.is_nan());

    let a = TypedArray::from_slice(vec![3], &[1.0, quiet, 3.0]).unwrap();
    let b = TypedArray::from_slice(vec![3], &[1.0, alt, 3.0]).unwrap();
    let c = TypedArray::from_slice(vec![3], &[1.0, signaling, 3.0]).unwrap();
    assert_ne!(a.bytes, b.bytes, "inputs must differ bitwise to be a test");
    assert_ne!(a.bytes, c.bytes);

    for_each_backend(
        {
            let (a, b, c) = (a.clone(), b.clone(), c.clone());
            move |store| {
                add(store, 1, sts("nan", a.clone()));
                add(store, 2, sts("nan", b.clone()));
                add(store, 3, sts("nan", c.clone()))
            }
        },
        |store, key, backend| {
            assert_eq!(
                store.num_distinct_arrays().unwrap(),
                1,
                "{backend}: NaN payloads must not defeat deduplication"
            );
            // All three owners read back the *first* stored array's bytes,
            // because they share one content-addressed array.
            let got = store.get_time_series(key.identity(), None).unwrap();
            assert_eq!(
                got.as_single().unwrap().data.bytes,
                a.bytes,
                "{backend}: shared array is the first-written payload"
            );
        },
    );
}

#[test]
fn hdf5_default_fill_value_is_not_special_cased() {
    // 9.969209968386869e+36 was netcdf-c's default f64 `_FillValue`, from the
    // era when this store was netcdf-backed. The HDF5 backend has no such
    // sentinel, but the pin is kept: a stored value that happens to equal a
    // fill sentinel must survive a reopen as data, not be read back as
    // "missing".
    const NC_FILL_DOUBLE: f64 = 9.969_209_968_386_869e36;
    // netcdf-c's `NC_FILL_FLOAT`, the f32 nearest the double above; it carries
    // only f32's precision, so writing more digits is a clippy error.
    const NC_FILL_FLOAT: f32 = 9.969_21e36;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let f64_data = TypedArray::from_slice(vec![3], &[1.0, NC_FILL_DOUBLE, 3.0]).unwrap();
    let f32_data = TypedArray::from_slice(vec![3], &[1.0f32, NC_FILL_FLOAT, 3.0]).unwrap();

    let (k64, k32) = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let k64 = add(&mut store, 1, sts("fill64", f64_data.clone()));
        let k32 = add(&mut store, 2, sts("fill32", f32_data.clone()));
        store.flush().unwrap();
        (k64, k32)
    };

    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(
        store
            .get_time_series(k64.identity(), None)
            .unwrap()
            .as_single()
            .unwrap()
            .data
            .bytes,
        f64_data.bytes,
        "f64 fill-value sentinel was not preserved"
    );
    assert_eq!(
        store
            .get_time_series(k32.identity(), None)
            .unwrap()
            .as_single()
            .unwrap()
            .data
            .bytes,
        f32_data.bytes,
        "f32 fill-value sentinel was not preserved"
    );
    assert!(store.verify_integrity().unwrap().ok());
}

// ===========================================================================
// 1.2 Empty and minimal arrays
// ===========================================================================

#[test]
fn zero_length_single_time_series_is_pinned() {
    // PIN: a zero-length series is currently *accepted* in memory. Nothing in
    // the add path rejects `length == 0`.
    let empty = TypedArray::from_slice(vec![0], &[] as &[f64]).unwrap();
    assert_eq!(empty.length(), 0);
    assert!(empty.bytes.is_empty());

    let mut store = create_store(None, true).unwrap();
    let key = add(&mut store, 1, sts("empty", empty.clone()));
    let got = store.get_time_series(key.identity(), None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 0);
    assert!(single.data.bytes.is_empty());
    assert_eq!(store.get_metadata(key.identity()).unwrap().length, Some(0));
}

#[test]
fn zero_length_single_time_series_on_disk_is_pinned() {
    // PIN: what the *persisted* backend does with a zero-length series. The
    // HDF5 path packs series into fixed-length datasets keyed partly by
    // length, so length 0 is a degenerate dataset shape.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let empty = TypedArray::from_slice(vec![0], &[] as &[f64]).unwrap();

    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = add(&mut store, 1, sts("empty", empty));
        store.flush().unwrap();
        key
    };

    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let single = got.as_single().unwrap();
    assert_eq!(single.length, 0);
    assert!(single.data.bytes.is_empty());
    assert_eq!(single.data.shape, vec![0]);
    // The degenerate dataset still passes the integrity check.
    assert!(store.verify_integrity().unwrap().ok());
}

#[test]
fn single_element_series_round_trips_on_both_backends() {
    let one = TypedArray::from_slice(vec![1], &[42.5f64]).unwrap();
    for_each_backend(
        {
            let one = one.clone();
            move |store| add(store, 1, sts("one", one.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            assert_eq!(single.length, 1, "{backend}");
            assert_eq!(single.data.to_f64_vec().unwrap(), vec![42.5], "{backend}");
            // A one-step window is the whole series.
            let sliced = store
                .get_time_series(key.identity(), Some((t0(), t0() + Duration::hours(1))))
                .unwrap();
            assert_eq!(
                sliced.as_single().unwrap().data.to_f64_vec().unwrap(),
                vec![42.5],
                "{backend}: single-step window"
            );
        },
    );
}

#[test]
fn deterministic_with_count_one_round_trips_and_window_selects() {
    // H = 2, count = 1 -> shape [2, 1].
    let data = TypedArray::from_slice(vec![2, 1], &[10.0f64, 20.0]).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let det = Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(6),
                    1,
                    data.clone(),
                    "count1",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Deterministic(det))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.count, 1, "{backend}");
            assert_eq!(det.data.shape, vec![2, 1], "{backend}");

            // Select the only window at its own start.
            let one = store
                .get_time_series(key.identity(), Some((t0(), t0() + Duration::hours(6))))
                .unwrap();
            let det = one.as_deterministic().unwrap();
            assert_eq!(det.count, 1, "{backend}: only window selected");
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                vec![10.0, 20.0],
                "{backend}"
            );
        },
    );
}

#[test]
fn deterministic_with_horizon_count_one_round_trips() {
    // H = 1 (horizon == resolution), count = 3 -> shape [1, 3].
    let data = TypedArray::from_slice(vec![1, 3], &[1.0f64, 2.0, 3.0]).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let det = Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(1),
                    Duration::hours(1),
                    3,
                    data.clone(),
                    "h1",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Deterministic(det))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let det = got.as_deterministic().unwrap();
            assert_eq!(det.data.shape, vec![1, 3], "{backend}");
            assert_eq!(
                det.data.to_f64_vec().unwrap(),
                vec![1.0, 2.0, 3.0],
                "{backend}"
            );
        },
    );
}

#[test]
fn probabilistic_with_one_percentile_round_trips() {
    // P = 1, H = 2, count = 2 -> shape [1, 2, 2].
    let data = TypedArray::from_slice(vec![1, 2, 2], &[1.0f64, 2.0, 3.0, 4.0]).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let p = Probabilistic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(1),
                    2,
                    vec![0.5],
                    data.clone(),
                    "p1",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Probabilistic(p))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let p = got.as_probabilistic().unwrap();
            assert_eq!(p.percentiles, vec![0.5], "{backend}");
            assert_eq!(p.data.shape, vec![1, 2, 2], "{backend}");
        },
    );
}

#[test]
fn scenarios_with_one_scenario_round_trips() {
    // S = 1, H = 2, count = 2 -> shape [1, 2, 2].
    let data = TypedArray::from_slice(vec![1, 2, 2], &[5.0f64, 6.0, 7.0, 8.0]).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let s = Scenarios::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(1),
                    2,
                    1,
                    data.clone(),
                    "s1",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Scenarios(s))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let s = got.as_scenarios().unwrap();
            assert_eq!(s.scenario_count, 1, "{backend}");
            assert_eq!(s.data.shape, vec![1, 2, 2], "{backend}");
        },
    );
}

// ===========================================================================
// 1.3 Extreme integers
// ===========================================================================

#[test]
fn extreme_i64_round_trips_through_disk() {
    let values = vec![i64::MIN, -1, 0, 1, i64::MAX];
    let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| add(store, 1, sts("i64", data.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            assert_eq!(single.data.dtype, Dtype::I64, "{backend}");
            assert_eq!(single.data.to_vec::<i64>().unwrap(), values, "{backend}");
            assert_eq!(single.data.bytes, data.bytes, "{backend}");
        },
    );
}

#[test]
fn extreme_u64_round_trips_through_disk() {
    // u64::MAX has the sign bit set; a signed round trip anywhere in the
    // HDF5 path would corrupt it.
    let values = vec![0u64, 1, i64::MAX as u64, u64::MAX];
    let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| add(store, 1, sts("u64", data.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            assert_eq!(single.data.dtype, Dtype::U64, "{backend}");
            assert_eq!(single.data.to_vec::<u64>().unwrap(), values, "{backend}");
            assert_eq!(single.data.bytes, data.bytes, "{backend}");
        },
    );
}

#[test]
fn extreme_i32_round_trips_through_disk() {
    let values = vec![i32::MIN, -1, 0, i32::MAX];
    let data = TypedArray::from_slice(vec![values.len()], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| add(store, 1, sts("i32", data.clone()))
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            let single = got.as_single().unwrap();
            assert_eq!(single.data.dtype, Dtype::I32, "{backend}");
            assert_eq!(single.data.to_vec::<i32>().unwrap(), values, "{backend}");
        },
    );
}

#[test]
fn extreme_integers_in_a_forecast_round_trip() {
    let values = vec![i64::MIN, i64::MAX, 0, -1, 1, i64::MIN + 1];
    let data = TypedArray::from_slice(vec![2, 3], &values).unwrap();
    for_each_backend(
        {
            let data = data.clone();
            move |store| {
                let det = Deterministic::new(
                    t0(),
                    Duration::hours(1),
                    Duration::hours(2),
                    Duration::hours(1),
                    3,
                    data.clone(),
                    "extreme",
                )
                .unwrap();
                add(store, 1, TimeSeriesData::Deterministic(det))
            }
        },
        |store, key, backend| {
            let got = store.get_time_series(key.identity(), None).unwrap();
            assert_eq!(
                got.as_deterministic()
                    .unwrap()
                    .data
                    .to_vec::<i64>()
                    .unwrap(),
                values,
                "{backend}"
            );
        },
    );
}

// ===========================================================================
// 1.4 Hostile strings
// ===========================================================================

/// Names that must be stored and retrieved verbatim. Several are SQLite `GLOB`
/// metacharacters, which the exact-name filter must treat as literals.
const HOSTILE_NAMES: &[&str] = &[
    "负荷_ø",            // non-ASCII, multi-byte
    "with spaces",       // spaces
    "quote'name",        // SQL single quote
    "double\"quote",     // SQL double quote
    "back\\slash",       // backslash
    "wind[1]",           // GLOB character class
    "a*b",               // GLOB zero-or-more
    "q?mark",            // GLOB single-char
    "100%_load",         // LIKE metacharacters (not GLOB metacharacters)
    "tab\tand\nnewline", // control characters
];

#[test]
fn hostile_names_round_trip_and_match_exactly() {
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    let mut keys = Vec::new();
    for (i, name) in HOSTILE_NAMES.iter().enumerate() {
        keys.push(add(&mut store, i as i64 + 1, sts(name, data.clone())));
    }

    // Every name is stored verbatim and readable by its own key.
    for (name, key) in HOSTILE_NAMES.iter().zip(&keys) {
        assert_eq!(key.name(), *name);
        let meta = store.get_metadata(key.identity()).unwrap();
        assert_eq!(meta.name, *name, "name not stored verbatim");
        assert!(store.has_time_series(key.identity()).unwrap());
    }

    // `list_names` returns all of them (distinct, sorted).
    let listed = store.list_names(ListFilter::new()).unwrap();
    assert_eq!(listed.len(), HOSTILE_NAMES.len());
    for name in HOSTILE_NAMES {
        assert!(listed.contains(&name.to_string()), "{name} missing");
    }

    // The exact-name filter treats every metacharacter as a literal: `a*b`
    // matches only `a*b`, never `wind[1]` or anything else.
    for name in HOSTILE_NAMES {
        let matched = store
            .list_names(ListFilter::new().name(*name))
            .expect("exact-name filter");
        assert_eq!(matched, vec![name.to_string()], "exact name {name:?}");
    }
}

#[test]
fn name_glob_follows_sqlite_glob_semantics() {
    // PIN what a caller must escape. `ListFilter::name_glob` renders straight
    // into SQLite `name GLOB ?`, so its metacharacters are `*`, `?`, `[...]`.
    // There is no escaping API: a literal `*` in a name is NOT addressable by
    // an exact `name_glob`, and callers who need literal matching must use
    // `ListFilter::name`.
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();
    for (i, name) in [
        "a*b",
        "axb",
        "ab",
        "wind[1]",
        "windx",
        "q?mark",
        "qzmark",
        "100%_load",
    ]
    .iter()
    .enumerate()
    {
        add(&mut store, i as i64 + 1, sts(name, data.clone()));
    }

    let glob = |p: &str| {
        let mut v = store.list_names(ListFilter::new().name_glob(p)).unwrap();
        v.sort();
        v
    };

    // `*` is a wildcard, so the pattern `a*b` matches the literal `a*b` AND
    // `axb` AND `ab`. This is the escaping hazard.
    assert_eq!(glob("a*b"), vec!["a*b", "ab", "axb"]);
    // `[...]` is a character class: `wind[1]` matches `wind1`, not `wind[1]`.
    // Nothing named `wind1` exists here, so the literal name does NOT match
    // its own text used as a pattern.
    assert!(
        glob("wind[1]").is_empty(),
        "GLOB treats [1] as a class, so it must not match the literal name"
    );
    // A character class does what it says.
    assert_eq!(glob("win[dx]*"), vec!["wind[1]", "windx"]);
    // `?` is single-character: matches `q?mark` and `qzmark` alike.
    assert_eq!(glob("q?mark"), vec!["q?mark", "qzmark"]);
    // `%` and `_` are LIKE metacharacters and are literal under GLOB.
    assert_eq!(glob("100%_load"), vec!["100%_load"]);
    assert!(glob("100%Xload").is_empty(), "_ is literal under GLOB");
}

#[test]
fn empty_string_name_is_pinned() {
    // PIN: the empty name is accepted. Nothing validates non-emptiness.
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();
    let key = add(&mut store, 1, sts("", data));
    assert_eq!(key.name(), "");
    assert_eq!(store.get_metadata(key.identity()).unwrap().name, "");
    assert_eq!(store.list_names(ListFilter::new()).unwrap(), vec![""]);
    // Addressable by exact filter.
    assert_eq!(
        store.list_keys(ListFilter::new().name("")).unwrap().len(),
        1
    );
}

#[test]
fn ten_kilobyte_name_is_pinned() {
    // PIN: names are not length-limited. A 10 kB name round trips through the
    // catalog (TEXT column, no CHECK constraint) and through the HDF5 half,
    // which stores no name at all.
    let name: String = "n".repeat(10_240);
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = add(&mut store, 1, sts(&name, data));
        store.flush().unwrap();
        key
    };
    let store = open_store(path.as_path(), true).unwrap();
    let meta = store.get_metadata(key.identity()).unwrap();
    assert_eq!(meta.name.len(), 10_240);
    assert_eq!(meta.name, name);
}

#[test]
fn hostile_owner_type_units_and_ext_round_trip() {
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();
    let owner_type = "Générateur[1]*'\"";
    let units = "MW·h⁻¹ (‰)";
    let application_data = "{\"kind\": \"负荷\", \"q\": \"it's\"}";

    let key = store
        .add(AddRequest::new(
            1,
            owner_type,
            OwnerCategory::Component,
            sts("load", data.clone())
                .with_units(units)
                .with_application_data(application_data),
        ))
        .unwrap();

    let meta = store.get_metadata(key.identity()).unwrap();
    assert_eq!(meta.owner_type, owner_type);
    assert_eq!(meta.units.as_deref(), Some(units));
    assert_eq!(meta.application_data.as_deref(), Some(application_data));
    // The exact owner-type filter matches it literally despite the
    // metacharacters.
    assert_eq!(
        store
            .list_keys(ListFilter::new().owner_type(owner_type))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.list_owner_types(ListFilter::new()).unwrap(),
        vec![owner_type.to_string()]
    );
}

#[test]
fn application_data_is_stored_verbatim_even_when_not_valid_json() {
    // PIN: `application_data` is an opaque TEXT blob. The core never parses it, so
    // syntactically invalid JSON is stored and returned unchanged.
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();
    let garbage = "{not json at all: ]]}\0trailing";

    let key = store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            sts("load", data).with_application_data(garbage),
        ))
        .unwrap();
    assert_eq!(
        store
            .get_metadata(key.identity())
            .unwrap()
            .application_data
            .as_deref(),
        Some(garbage),
        "application_data must be opaque, not validated or normalized"
    );
}

#[test]
fn one_megabyte_ext_round_trips_through_disk() {
    let payload = format!("{{\"blob\":\"{}\"}}", "x".repeat(1024 * 1024));
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let key = store
            .add(AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                sts("load", data).with_application_data(payload.clone()),
            ))
            .unwrap();
        store.flush().unwrap();
        key
    };
    let store = open_store(path.as_path(), true).unwrap();
    assert_eq!(
        store.get_metadata(key.identity()).unwrap().application_data,
        Some(payload)
    );
}

#[test]
fn hostile_feature_keys_and_values_round_trip_and_disambiguate() {
    use infrastore_core::FeatureValue;
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    let mut features = Features::new();
    features.insert(
        "键 with spaces".into(),
        FeatureValue::Str("值'\"[*]".into()),
    );
    // Not -0.0: SQLite's REAL storage drops the sign of a negative zero, so it
    // is refused on write (see the round-trip test below). The infinities and
    // subnormals do survive bit-for-bit.
    features.insert("f".into(), FeatureValue::Float(f64::NEG_INFINITY));
    features.insert("b".into(), FeatureValue::Bool(false));

    let key = store
        .add(
            AddRequest::new(
                1,
                "Generator",
                OwnerCategory::Component,
                sts("load", data.clone()),
            )
            .with_features(features.clone()),
        )
        .unwrap();
    assert_eq!(
        store.get_metadata(key.identity()).unwrap().features,
        features
    );

    // A different feature set is a different series, not a duplicate.
    let mut other = features.clone();
    other.insert("b".into(), FeatureValue::Bool(true));
    let key2 = store
        .add(
            AddRequest::new(1, "Generator", OwnerCategory::Component, sts("load", data))
                .with_features(other),
        )
        .unwrap();
    assert_ne!(key.identity().features, key2.identity().features);
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 2);
}

#[test]
fn hostile_names_survive_a_non_sequential_disk_round_trip() {
    let name = "负荷_ø[*]'\"";
    let timestamps = vec![t0(), t0() + Duration::minutes(7), t0() + Duration::days(3)];
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.h5");
    let key = {
        let mut store = create_store(Some(path.as_path()), false).unwrap();
        let series = NonSequentialTimeSeries::new(timestamps.clone(), data.clone(), name).unwrap();
        let key = add(
            &mut store,
            1,
            TimeSeriesData::NonSequentialTimeSeries(series),
        );
        store.flush().unwrap();
        key
    };
    let store = open_store(path.as_path(), true).unwrap();
    let got = store.get_time_series(key.identity(), None).unwrap();
    let ns = got.as_non_sequential().unwrap();
    assert_eq!(ns.name, name);
    assert_eq!(ns.timestamps, timestamps);
}

#[test]
fn a_nan_feature_value_is_rejected_and_leaves_the_catalog_readable() {
    // SQLite has no NaN: `sqlite3_bind_double` stores it as NULL while
    // `value_kind` still says 'float', and the read path — which hydrates every
    // feature set a listing touches — then fails on the NULL. Accepting such a
    // value would make `list_keys`/`list_names`/`get_metadata` fail for the
    // *whole* store, including series sharing nothing with it, and survive a
    // reopen because the bad row is on disk. So it fails on the way in.
    use infrastore_core::FeatureValue;
    let mut store = create_store(None, true).unwrap();
    let data = TypedArray::from_slice(vec![3], &[1.0f64, 2.0, 3.0]).unwrap();

    store
        .add(AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            sts("healthy", data.clone()),
        ))
        .unwrap();

    let mut features = Features::new();
    features.insert("x".into(), FeatureValue::Float(f64::NAN));
    let err = store
        .add(
            AddRequest::new(
                2,
                "Generator",
                OwnerCategory::Component,
                sts("load", data.clone()),
            )
            .with_features(features),
        )
        .unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(ref m) if m.contains("NaN")),
        "{err}"
    );

    // The rejection is total: nothing was written, so the catalog still reads.
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 1);

    // Negative zero is refused for the same reason, and separately: SQLite keeps
    // an exactly-integral REAL as an integer, so the sign is lost and it reads
    // back as +0.0.
    let mut features = Features::new();
    features.insert("x".into(), FeatureValue::Float(-0.0));
    let err = store
        .add(
            AddRequest::new(
                3,
                "Generator",
                OwnerCategory::Component,
                sts("load", data.clone()),
            )
            .with_features(features),
        )
        .unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(ref m)
            if m.contains("negative zero")),
        "{err}"
    );

    // Every other float value round-trips bit-for-bit, the infinities included.
    for (i, v) in [
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        1.5,
        f64::MIN_POSITIVE,
    ]
    .into_iter()
    .enumerate()
    {
        let mut features = Features::new();
        features.insert("x".into(), FeatureValue::Float(v));
        store
            .add(
                AddRequest::new(
                    10 + i as i64,
                    "Generator",
                    OwnerCategory::Component,
                    sts("load", data.clone()),
                )
                .with_features(features),
            )
            .unwrap_or_else(|e| panic!("float feature {v} should be storable: {e}"));
    }
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 6);

    // And what came back is what went in.
    for key in store.list_keys(ListFilter::new()).unwrap() {
        if let Some(FeatureValue::Float(v)) = key.identity().features.get("x") {
            let stored = store.get_metadata(key.identity()).unwrap();
            assert_eq!(
                stored.features.get("x"),
                Some(&FeatureValue::Float(*v)),
                "float feature {v} did not survive the catalog"
            );
        }
    }
}

#[test]
fn an_element_count_too_large_to_represent_is_an_error_not_a_panic() {
    // Both of these multiply caller-controlled numbers. Unchecked they panic in
    // a debug build -- against `TypedArray::new`'s documented `Result` contract
    // -- and, worse, *wrap* in a release build, where the workspace profile
    // leaves `overflow-checks` off. A wrapped product of 0 makes an empty buffer
    // "match" a shape describing 2^61 elements, and a wrapped slot count makes
    // an impossible ragged row pass the width check that `codec::decode` then
    // trusts.
    use infrastore_core::{Dtype, ElementType};

    // Shape product overflows `usize`.
    let err = TypedArray::new(Dtype::F64, vec![2_305_843_009_213_693_952, 8], vec![]).unwrap_err();
    assert!(err.contains("usize"), "{err}");
    let err =
        TypedArray::from_slice(vec![2_305_843_009_213_693_952, 8], &[] as &[f64]).unwrap_err();
    assert!(err.contains("usize"), "{err}");

    // Element count fits, but count * dtype size does not.
    let err = TypedArray::new(Dtype::F64, vec![usize::MAX / 4], vec![]).unwrap_err();
    assert!(err.contains("usize"), "{err}");

    // A ragged row whose declared point count overflows the slots it would need.
    let huge = TypedArray::from_f64(vec![1, 1], &[9_223_372_036_854_775_808.0]);
    let err = ElementType::PiecewiseLinear
        .validate_array(&huge, 1)
        .unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(ref m)
            if m.contains("row width")),
        "{err}"
    );

    // Ordinary shapes and well-formed ragged rows are unaffected.
    assert!(TypedArray::new(Dtype::F64, vec![3, 2], vec![0u8; 48]).is_ok());
    let ok = TypedArray::from_f64(vec![1, 5], &[2.0, 0.0, 1.0, 10.0, 20.0]);
    assert!(ElementType::PiecewiseLinear.validate_array(&ok, 1).is_ok());
}

#[test]
fn feature_value_equality_hashing_and_ordering_agree() {
    // `FeatureValue` is part of a series' identity, so the three have to be one
    // rule. They are the *catalog's* rule: `features_hash` digests a float by
    // its bit pattern, and that hash is what the uniqueness index keys on. A
    // derived `PartialEq` gave IEEE semantics instead, so `0.0 == -0.0` compared
    // equal while hashing differently — breaking the `Hash` contract all the way
    // up through `Features`, `KeyIdentity` and `TimeSeriesKey`.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use infrastore_core::FeatureValue;

    fn hash_of(v: &FeatureValue) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    // Equal implies equal hashes, for every variant.
    for (a, b) in [
        (FeatureValue::Int(7), FeatureValue::Int(7)),
        (FeatureValue::Float(1.5), FeatureValue::Float(1.5)),
        (FeatureValue::Float(f64::NAN), FeatureValue::Float(f64::NAN)),
        (
            FeatureValue::Float(f64::INFINITY),
            FeatureValue::Float(f64::INFINITY),
        ),
        (FeatureValue::Bool(true), FeatureValue::Bool(true)),
        (FeatureValue::Str("x".into()), FeatureValue::Str("x".into())),
    ] {
        assert_eq!(a, b, "{a:?} vs {b:?}");
        assert_eq!(hash_of(&a), hash_of(&b), "{a:?} vs {b:?}");
        assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal, "{a:?} vs {b:?}");
    }

    // `Eq` is reflexive even for NaN, which the derived impl was not: a NaN
    // feature was not equal to itself, so such a key could never be found in any
    // `HashMap` or `HashSet` while `impl Eq` claimed otherwise.
    let nan = FeatureValue::Float(f64::NAN);
    assert_eq!(nan, nan.clone());

    // Unequal implies a defined, consistent ordering — never `Equal`.
    for (a, b) in [
        (FeatureValue::Float(0.0), FeatureValue::Float(-0.0)),
        (FeatureValue::Float(1.0), FeatureValue::Float(2.0)),
        (FeatureValue::Int(1), FeatureValue::Float(1.0)),
        (FeatureValue::Bool(false), FeatureValue::Bool(true)),
    ] {
        assert_ne!(a, b, "{a:?} vs {b:?}");
        assert_ne!(a.cmp(&b), std::cmp::Ordering::Equal, "{a:?} vs {b:?}");
        assert_eq!(
            a.cmp(&b),
            b.cmp(&a).reverse(),
            "{a:?} vs {b:?} antisymmetry"
        );
    }

    // The contract that was actually broken: a set lookup by an equal value.
    let mut set = std::collections::HashSet::new();
    set.insert(FeatureValue::Float(0.0));
    assert!(set.contains(&FeatureValue::Float(0.0)));
    assert!(
        !set.contains(&FeatureValue::Float(-0.0)),
        "-0.0 is a distinct value here, as it is to the catalog"
    );
}

#[test]
fn malformed_inputs_report_errors_rather_than_panicking() {
    // Four places where caller- or data-supplied numbers reached arithmetic or
    // an index without a guard. All are reachable from safe, public API; a debug
    // build panicked and a release build, where the workspace profile leaves
    // `overflow-checks` off, would have wrapped instead.
    use infrastore_core::{DecodedValues, Dtype, Period, codec};

    // 1. An ISO-8601 duration may repeat a unit — `parse_components` applies no
    //    uniqueness rule — and one day component already reaches the edge of i64
    //    milliseconds, so a second overflowed the unguarded `+=`. Reachable from
    //    an arbitrary caller string, including one arriving over gRPC. The
    //    calendar accumulator is the same shape: a year component multiplies by
    //    12 under a guard, then lands in `months`, which needs one too.
    for s in [
        "P106751991167D106751991167D",
        "PT2562047788015H2562047788015H",
        "P106751991167W106751991167W",
        "P768614336404564650Y768614336404564650Y",
        "P9223372036854775807M9223372036854775807M",
        "P768614336404564650Y9223372036854775807M",
    ] {
        let err = Period::from_iso8601(s).unwrap_err();
        // An overflow reads like every other parse failure and names the input,
        // rather than reporting a different, vaguer message than a malformed
        // string would.
        assert!(
            matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(ref m)
                if m.contains("invalid ISO-8601 period") && m.contains(s)),
            "{s}: {err}"
        );
    }
    // A single in-range component is unaffected.
    assert!(Period::from_iso8601("P106751991167D").is_ok());
    // The largest year count a `Period::Months` can hold (the month total is an
    // i32); one component of it parses, two overflow the accumulator above.
    assert_eq!(
        Period::from_iso8601("P178956970Y").unwrap(),
        Period::months(178_956_970 * 12)
    );
    assert_eq!(Period::from_iso8601("P1Y6M").unwrap(), Period::months(18));
    assert_eq!(
        Period::from_iso8601("P1DT2H").unwrap(),
        Period::fixed(Duration::days(1) + Duration::hours(2))
    );

    // 2. `element_type_of` manufactures `tuple(0,f64)` for an empty Tuple, and
    //    `validate_array` accepts it, so the crate's own round trip reached a
    //    `chunks_exact(0)`.
    let empty = DecodedValues::Tuple(vec![]);
    let et = codec::element_type_of(&empty).unwrap();
    let arr = codec::encode(&empty, &[0]).unwrap();
    let err = codec::decode(&arr, et, 1).unwrap_err();
    assert!(
        matches!(err, infrastore_core::TimeSeriesError::InvalidParameter(ref m)
            if m.contains("nothing to decode")),
        "{err}"
    );

    // 3. A `TypedArray` whose bytes do not match its shape is buildable in safe
    //    code, because the fields are public. Both write paths must report it;
    //    the bulk one used to panic on an unchecked slice index, after the
    //    dataset had already been created.
    let dir = tempfile::tempdir().unwrap();
    let mut store = create_store(Some(dir.path().join("bad.h5").as_path()), false).unwrap();
    let mismatched = TypedArray {
        dtype: Dtype::F64,
        shape: vec![4],
        bytes: vec![0u8; 8],
    };
    let request = || {
        AddRequest::new(
            1,
            "Generator",
            OwnerCategory::Component,
            TimeSeriesData::SingleTimeSeries(SingleTimeSeries::new(
                Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
                Duration::hours(1),
                mismatched.clone(),
                "bad",
            )),
        )
    };
    assert!(store.add(request()).is_err(), "single add must report it");
    assert!(
        store.add_time_series_bulk(vec![request()]).is_err(),
        "bulk add must report it too, not panic"
    );
    assert_eq!(store.list_keys(ListFilter::new()).unwrap().len(), 0);
}
