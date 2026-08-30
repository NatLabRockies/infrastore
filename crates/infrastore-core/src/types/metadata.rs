use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::element_type::ElementType;
use super::id::TimeSeriesId;
use super::period::Period;
use super::time_reference::TimeReference;
use super::time_series::TimeSeriesType;
use crate::error::{Result, TimeSeriesError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnerCategory {
    Component,
    SupplementalAttribute,
}

impl OwnerCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            OwnerCategory::Component => "Component",
            OwnerCategory::SupplementalAttribute => "SupplementalAttribute",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Component" => OwnerCategory::Component,
            "SupplementalAttribute" => OwnerCategory::SupplementalAttribute,
            _ => return None,
        })
    }

    /// The storage code written to the SQLite catalog.
    ///
    /// As with [`crate::TimeSeriesType::code`], this is the *storage* encoding
    /// and [`Self::as_str`] is the *display and serde* one. Part of the on-disk
    /// contract: changing it requires a [`crate::DATA_FORMAT_VERSION`] bump.
    pub fn code(self) -> i64 {
        match self {
            OwnerCategory::Component => 0,
            OwnerCategory::SupplementalAttribute => 1,
        }
    }

    /// Inverse of [`Self::code`]. `None` for an unknown code, which in the
    /// catalog means a store written by an incompatible version.
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => OwnerCategory::Component,
            1 => OwnerCategory::SupplementalAttribute,
            _ => return None,
        })
    }
}

impl FromStr for OwnerCategory {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

/// Which unit basis a series' values are expressed in.
///
/// This is the per-unit declaration power-systems modelers know as the "unit
/// system" (PowerSystems.jl spells it `UnitSystem`, with `NATURAL_UNITS` and
/// `DEVICE_BASE`; `ComponentBase` is the same idea named for components rather
/// than devices). It is a *label*, not a conversion: the store neither holds
/// the base value nor rescales anything, so converting `ComponentBase` values
/// back to natural units is the consumer's job, using the base that lives on
/// the owning component in its own object graph.
///
/// `None` on a metadata row means *unspecified*, never `NaturalUnits` — every
/// row written before this field existed is `None`, and reading those as
/// natural units would assert a basis nobody declared.
///
/// Stored as its [`Self::as_str`] spelling, not an integer code: unlike
/// [`OwnerCategory`] this column sits in no index, so the readable form costs
/// nothing worth reclaiming. The column carries no `CHECK` constraint, which
/// is what lets a third basis (`system_base`, should a consumer need it) land
/// without a [`crate::DATA_FORMAT_VERSION`] bump.
/// The `rename_all` keeps serde on the [`Self::as_str`] spelling, which every
/// other surface — SQLite, the proto, the C ABI, Python, Julia, the CLI
/// descriptor — uses, and which [`Self::parse`] is the inverse of. Without it
/// the derive would emit `"NaturalUnits"`, so a value round-tripped through
/// serde and back into any of those surfaces would be rejected. The other enums
/// here need no attribute only because their variant names already match their
/// `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitSystem {
    /// Values are in the units named by `units` (e.g. `"MW"`).
    NaturalUnits,
    /// Values are per-unit against the owning component's own base.
    ComponentBase,
}

impl UnitSystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitSystem::NaturalUnits => "natural_units",
            UnitSystem::ComponentBase => "component_base",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "natural_units" => UnitSystem::NaturalUnits,
            "component_base" => UnitSystem::ComponentBase,
            _ => return None,
        })
    }
}

impl std::fmt::Display for UnitSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UnitSystem {
    type Err = TimeSeriesError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| {
            TimeSeriesError::InvalidParameter(format!(
                "unknown unit system {s:?}; expected one of natural_units, component_base"
            ))
        })
    }
}

/// A feature value. Part of a series' identity, so `Eq`, `Hash` and `Ord` all
/// have to agree — and for the `Float` variant they agree on *bits*, not on
/// IEEE comparison.
///
/// That is the catalog's own rule, not a convenience: [`crate::hash::features_hash`]
/// digests a float by its bit pattern, and `features_hash` is the column the
/// uniqueness index keys on. So the store already treats `0.0` and `-0.0` as two
/// different series. Deriving `PartialEq` gave IEEE semantics instead, where
/// `0.0 == -0.0` — equal values that hashed differently, breaking the `Hash`
/// contract for `FeatureValue`, `Features` and `KeyIdentity` alike. A
/// `HashSet<KeyIdentity>` would hold two members that compare equal
/// while `contains` missed one of them, and the type disagreed with the catalog
/// about what a distinct series is.
///
/// NaN is canonicalized to one bit pattern, so `Float(NaN) == Float(NaN)` and
/// `Eq`'s reflexivity is honest — the derived `PartialEq` made `impl Eq` a lie,
/// since a NaN feature was not equal to itself. (The store rejects a NaN feature
/// on write, because SQLite cannot store one; this keeps the in-memory type
/// coherent regardless.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FeatureValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// One bit pattern per logical float value: every NaN folds to the same one,
/// and every other value keeps its own bits (so `-0.0` stays distinct from
/// `0.0`). Shared by `Eq`, `Hash` and `Ord` so the three cannot drift apart.
fn canonical_bits(v: f64) -> u64 {
    if v.is_nan() {
        f64::NAN.to_bits()
    } else {
        v.to_bits()
    }
}

/// The discriminant `Ord` and `Hash` order variants by.
fn variant_tag(v: &FeatureValue) -> u8 {
    match v {
        FeatureValue::Int(_) => 0,
        FeatureValue::Float(_) => 1,
        FeatureValue::Bool(_) => 2,
        FeatureValue::Str(_) => 3,
    }
}

impl PartialEq for FeatureValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (FeatureValue::Int(a), FeatureValue::Int(b)) => a == b,
            (FeatureValue::Float(a), FeatureValue::Float(b)) => {
                canonical_bits(*a) == canonical_bits(*b)
            }
            (FeatureValue::Bool(a), FeatureValue::Bool(b)) => a == b,
            (FeatureValue::Str(a), FeatureValue::Str(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for FeatureValue {}

impl std::hash::Hash for FeatureValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        variant_tag(self).hash(state);
        match self {
            FeatureValue::Int(v) => v.hash(state),
            FeatureValue::Float(v) => canonical_bits(*v).hash(state),
            FeatureValue::Bool(v) => v.hash(state),
            FeatureValue::Str(v) => v.hash(state),
        }
    }
}

impl Ord for FeatureValue {
    /// A total order agreeing with [`Eq`]: variants in tag order, then values.
    ///
    /// Floats are compared with [`f64::total_cmp`] over the canonicalized value,
    /// so every NaN sorts as one value (matching `Eq`) rather than by payload as
    /// raw `total_cmp` would. It exists so `Features` — a `BTreeMap` of these —
    /// is `Ord`, which is what gives the columnar readers a deterministic column
    /// layout for series that differ only by feature.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        variant_tag(self).cmp(&variant_tag(other)).then_with(|| {
            match (self, other) {
                (FeatureValue::Int(a), FeatureValue::Int(b)) => a.cmp(b),
                (FeatureValue::Float(a), FeatureValue::Float(b)) => {
                    let (a, b) = (
                        if a.is_nan() { f64::NAN } else { *a },
                        if b.is_nan() { f64::NAN } else { *b },
                    );
                    a.total_cmp(&b)
                }
                (FeatureValue::Bool(a), FeatureValue::Bool(b)) => a.cmp(b),
                (FeatureValue::Str(a), FeatureValue::Str(b)) => a.cmp(b),
                // Unreachable: the tags already differed.
                _ => std::cmp::Ordering::Equal,
            }
        })
    }
}

impl PartialOrd for FeatureValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FeatureValue {
    pub fn kind(&self) -> &'static str {
        match self {
            FeatureValue::Int(_) => "int",
            FeatureValue::Float(_) => "float",
            FeatureValue::Bool(_) => "bool",
            FeatureValue::Str(_) => "str",
        }
    }
}

/// Sorted-by-key feature map. `BTreeMap` gives the sort order invariant for
/// free, which matters for hashing and the metadata uniqueness constraint.
pub type Features = BTreeMap<String, FeatureValue>;

/// Feature names the store refuses to accept, because each already names a
/// field of a time-series struct or of the key/metadata tuple that addresses
/// one. Consumers routinely spread a feature map into a keyword-argument query
/// (`get_time_series(...; name = "load", model_year = 2030)`), where a feature
/// called `name` or `resolution` would shadow the real field and silently
/// change what the query means. Rejecting them at write time keeps that
/// ambiguity out of the store entirely.
///
/// The comparison is exact and case-sensitive, matching how the catalog treats
/// every other identifier: `resolution` is reserved, `Resolution` is not.
///
/// Kept sorted so [`is_reserved_feature_name`] can binary-search it, and so a
/// reader can scan it.
pub const RESERVED_FEATURE_NAMES: &[&str] = &[
    "application_data",
    // Neither names a metadata field: `array_shape` is the wire spelling of the
    // stored array's native shape, and `association_id` the wire spelling of
    // `id`. Both are reserved for the same reason as `dtype` below -- they name
    // a field of the row as a consumer sees it, which is what the shadowing
    // rule is about, and the OpenAPI schema's own reserved list carries them.
    "array_shape",
    "association_id",
    "component_field",
    "count",
    "data",
    "data_hash",
    // Reserved though it names no metadata field: it is the spelling of a
    // `TypedArray`'s physical type in every binding, so allowing it as a
    // feature name would reintroduce the shadowing this list prevents.
    "dtype",
    "element_shape",
    "element_type",
    // Reserved for the same reason as `dtype`, plus one more: a consumer
    // passing this older spelling of `application_data` fails loudly instead of
    // having it silently accepted as an ordinary feature.
    "ext",
    "features",
    "horizon",
    // Reserved for the catalog row's own id, which every binding surfaces
    // alongside the fields above. Unlike `dtype` and `ext` below it, this one
    // is reserved *before* anything could have used it: the name is too
    // generic to be a plausible series discriminator, so the shadowing this
    // list prevents is the only thing it could cause.
    "id",
    "initial_timestamp",
    "interval",
    "length",
    "name",
    "owner_category",
    "owner_id",
    "owner_type",
    "percentiles",
    "quantity_kind",
    "resolution",
    "scenario_count",
    "time_reference",
    "time_series_type",
    "timestamps",
    "unit_system",
    "units",
    // Names no column and no struct field: it is the wire form's locator for
    // the dense data. Reserved because the shadowing runs the other way here --
    // a feature named `uri` is accepted on write, and the exported document
    // then fails the schema's own propertyNames rule on the features map.
    "uri",
];

/// Whether `name` is one of the [`RESERVED_FEATURE_NAMES`].
pub fn is_reserved_feature_name(name: &str) -> bool {
    RESERVED_FEATURE_NAMES.binary_search(&name).is_ok()
}

/// Reject a feature map that uses a reserved name as a key.
///
/// Applied on the write path only, so a store written before this rule existed
/// stays readable and its offending series can still be listed and removed.
pub fn validate_features(features: &Features) -> Result<()> {
    for (key, value) in features {
        if is_reserved_feature_name(key) {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "feature name {key:?} is reserved: it names a field of a time series \
                 or of the key that addresses one; reserved names are {}",
                RESERVED_FEATURE_NAMES.join(", ")
            )));
        }
        // Two float values cannot survive the catalog round trip, and both are
        // refused at the door rather than allowed to change underfoot. A feature
        // is part of a series' identity, so a value that comes back different
        // from the one written is not a cosmetic loss.
        //
        // NaN: SQLite has none, so `sqlite3_bind_double` stores it as NULL while
        // `value_kind` still says 'float', and the read path — which hydrates
        // *every* feature set a listing touches — then fails on the NULL. One
        // such row made `list_metadata`/`list_names` fail for the
        // whole store, including series sharing nothing with it, and survived
        // reopen because the bad row was on disk.
        //
        // Negative zero: SQLite's REAL storage keeps an exactly-integral value
        // as an integer, so `-0.0` is written and read back as `+0.0` — the sign
        // is simply gone. Everything else round-trips bit-for-bit, the
        // infinities included, so this is the whole of the exception.
        if let FeatureValue::Float(f) = value {
            let unstorable = if f.is_nan() {
                Some("NaN")
            } else if *f == 0.0 && f.is_sign_negative() {
                Some("negative zero")
            } else {
                None
            };
            if let Some(what) = unstorable {
                return Err(TimeSeriesError::InvalidParameter(format!(
                    "feature {key:?} is {what}, which the catalog cannot store faithfully; \
                     it would read back as a different value"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeSeriesMetadata {
    pub owner_id: i64,
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub data_hash: [u8; 32],

    // Temporal fields (all `Option` so future variants can leave some unset).
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub length: Option<usize>,
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub count: Option<usize>,
    pub timestamps: Option<Vec<DateTime<Utc>>>,

    pub features: Features,
    pub units: Option<String>,
    /// What kind of physical quantity the values measure (e.g. `"ActivePower"`,
    /// `"Energy"`, `"Length"`), or `None`.
    ///
    /// Free-form and never interpreted by the store; the recommended vocabulary
    /// is a [QUDT] `QuantityKind` local name. It sits above `units` rather than
    /// duplicating it: a quantity kind separates active from reactive power,
    /// which a units library's dimensional analysis cannot, since both are
    /// `[M L^2 T^-3]`. It also survives the case that motivates it — when
    /// [`Self::unit_system`] is [`UnitSystem::ComponentBase`] the values are
    /// per-unit and dimensionless, so this is the only record of what they
    /// measure and which base converts them back.
    ///
    /// [QUDT]: https://www.qudt.org/pages/QUDToverviewPage.html
    pub quantity_kind: Option<String>,
    /// Which basis the values are expressed in, or `None` for unspecified.
    pub unit_system: Option<UnitSystem>,
    /// How this series' timestamps were spelled, or `None` for unspecified.
    ///
    /// The store records instants; this records what they were written *as*, so
    /// a read can hand back the spelling a write arrived in. It changes nothing
    /// about the stored instants, the grid, or either content hash — see
    /// [`TimeReference`].
    ///
    /// `None` means *unspecified*, never [`TimeReference::Utc`]: it groups with
    /// the zoned variants for query bounds (an aware bound is accepted, a naive
    /// one refused) but it is not a claim that the timestamps were written as
    /// UTC.
    pub time_reference: Option<TimeReference>,
    /// The field on the owning component whose value this series varies over
    /// time (e.g. `"max_active_power"`, `"rating"`), or `None`.
    ///
    /// Free-form and never interpreted by the store: it names a field in the
    /// consumer's own object model, which this crate has no view of. It records
    /// what the values are *for*, where [`Self::name`] only says which series
    /// they are. The two coincide by convention in many models but are not the
    /// same thing: one component may carry several series for one field — a
    /// forecast and an actual, a set of weather years — distinguished by name
    /// or features, and a series' name is part of its identity where this is
    /// not. Descriptive, so it sits outside a series' identity and
    /// outside both content hashes.
    ///
    /// Named for the common case. The owner may also be a supplemental
    /// attribute ([`OwnerCategory::SupplementalAttribute`]), in which case this
    /// names a field on that attribute.
    pub component_field: Option<String>,
    /// Percentiles for a `Probabilistic` forecast; `None` for other types.
    pub percentiles: Option<Vec<f64>>,

    // Element typing of the stored array.
    /// What the stored elements mean and how one timestep is laid out. The
    /// physical dtype of the bytes is [`ElementType::physical_dtype`]; this is
    /// the single source of truth for both.
    pub element_type: ElementType,
    /// Per-step element shape (trailing dims after time); empty = scalar.
    pub element_shape: Vec<usize>,
    /// Opaque, package-owned payload stored verbatim for an application to
    /// reconstruct its own domain objects. The store never parses or interprets
    /// it; end users are not expected to set it. Element typing does *not*
    /// belong here — that is [`Self::element_type`].
    pub application_data: Option<String>,

    /// The catalog row's `id`, or `None`.
    ///
    /// Unlike every other field here, this describes the *row* rather than the
    /// data: it is what the catalog filed this association under, and a
    /// consumer stores it in its own object model as a durable reference back
    /// (a generator's cost function naming the series that varies it). It is
    /// never reissued once its row is deleted — see the `id` column in
    /// `metadata/schema.rs` — but it is meaningful only within the store that
    /// minted it and only alongside the table it came from.
    ///
    /// The direction decides what `None` means:
    ///
    /// * **Reading** — always `Some`. Every stored row has an id.
    /// * **Writing** — the catalog assigns, and every add ignores whatever this
    ///   field holds and returns the [`TimeSeriesId`] it chose. One writer is
    ///   the exception:
    ///   [`Store::import_association_rows`](crate::Store::import_association_rows)
    ///   files each row under the `Some` it carries, so a document that already
    ///   recorded its ids keeps the references it wrote down. That import wants
    ///   all-or-none across a batch, and explicit ids only ratchet the catalog's
    ///   counter *upward*, so one at or below the current high-water mark
    ///   collides and is refused with
    ///   [`TimeSeriesError::DuplicateAssociationId`].
    ///
    /// Descriptive, so it sits outside a series' identity and outside
    /// both content hashes: two series differing only in it are the same
    /// series, and it changes nothing about what is stored.
    ///
    /// [`TimeSeriesError::DuplicateAssociationId`]: crate::TimeSeriesError::DuplicateAssociationId
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<TimeSeriesId>,
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;
    use crate::types::array::TypedArray;
    use crate::types::key::KeyIdentity;
    use crate::types::time_series::{
        Deterministic, NonSequentialTimeSeries, Probabilistic, Scenarios, SingleTimeSeries,
    };

    fn features_of(pairs: &[(&str, FeatureValue)]) -> Features {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// Field names of a serde-serialized struct — the same spelling a consumer
    /// sees when it spreads a time series into keyword arguments.
    fn field_names<T: Serialize>(value: &T) -> Vec<String> {
        match serde_json::to_value(value).expect("struct serializes to JSON") {
            serde_json::Value::Object(map) => map.keys().cloned().collect(),
            other => panic!("expected a JSON object, got {other:?}"),
        }
    }

    #[test]
    fn reserved_names_are_sorted_and_unique() {
        // `is_reserved_feature_name` binary-searches, so order is load-bearing.
        let mut sorted = RESERVED_FEATURE_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, RESERVED_FEATURE_NAMES);
    }

    /// The canary for the whole rule: every field of every time-series struct,
    /// of the metadata row, and of the key identity must be reserved. Adding a
    /// field without adding its name here fails this test.
    #[test]
    fn every_time_series_field_name_is_reserved() {
        let t0 = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let hour = Duration::hours(1);
        let arr = |shape: Vec<usize>| {
            let n: usize = shape.iter().product();
            let values: Vec<f64> = (0..n).map(|i| i as f64).collect();
            TypedArray::from_f64(shape, &values)
        };

        let single = SingleTimeSeries::new(t0, hour, arr(vec![2]), "load");
        let non_sequential =
            NonSequentialTimeSeries::new(vec![t0, t0 + hour], arr(vec![2]), "load").unwrap();
        // H = 2, count = 3.
        let deterministic =
            Deterministic::new(t0, hour, hour * 2, hour, 3, arr(vec![2, 3]), "load").unwrap();
        let probabilistic = Probabilistic::new(
            t0,
            hour,
            hour * 2,
            hour,
            3,
            vec![0.1, 0.9],
            arr(vec![2, 2, 3]),
            "load",
        )
        .unwrap();
        let scenarios =
            Scenarios::new(t0, hour, hour * 2, hour, 3, 4, arr(vec![4, 2, 3]), "load").unwrap();

        let metadata = TimeSeriesMetadata {
            owner_id: 1,
            owner_type: "Generator".into(),
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            data_hash: [0u8; 32],
            initial_timestamp: Some(t0),
            resolution: Some(Period::Fixed(hour)),
            length: Some(2),
            horizon: None,
            interval: None,
            count: None,
            timestamps: None,
            features: Features::new(),
            units: None,
            quantity_kind: None,
            unit_system: None,
            time_reference: None,
            component_field: None,
            percentiles: None,
            element_type: ElementType::Scalar(single.data.dtype),
            element_shape: vec![],
            application_data: None,
            id: None,
        };
        let identity = KeyIdentity {
            owner_id: 1,
            owner_category: OwnerCategory::Component,
            time_series_type: TimeSeriesType::SingleTimeSeries,
            name: "load".into(),
            resolution: Some(Period::Fixed(hour)),
            interval: None,
            features: Features::new(),
        };

        let mut names = Vec::new();
        names.extend(field_names(&single));
        names.extend(field_names(&non_sequential));
        names.extend(field_names(&deterministic));
        names.extend(field_names(&probabilistic));
        names.extend(field_names(&scenarios));
        names.extend(field_names(&metadata));
        names.extend(field_names(&identity));

        for name in names {
            assert!(
                is_reserved_feature_name(&name),
                "{name:?} is a field of a time series or its key but is not in \
                 RESERVED_FEATURE_NAMES; add it there"
            );
        }
    }

    #[test]
    fn validate_features_accepts_ordinary_names() {
        let features = features_of(&[
            ("model_year", FeatureValue::Int(2030)),
            ("scenario", FeatureValue::Str("high".into())),
            ("calibrated", FeatureValue::Bool(true)),
        ]);
        assert!(validate_features(&features).is_ok());
        assert!(validate_features(&Features::new()).is_ok());
    }

    #[test]
    fn validate_features_rejects_every_reserved_name() {
        for reserved in RESERVED_FEATURE_NAMES {
            let features = features_of(&[
                ("model_year", FeatureValue::Int(2030)),
                (reserved, FeatureValue::Str("x".into())),
            ]);
            let err = validate_features(&features).unwrap_err();
            assert!(
                matches!(err, TimeSeriesError::InvalidParameter(ref m) if m.contains(reserved)),
                "{reserved} should be rejected by name, got {err}"
            );
        }
    }

    #[test]
    fn reserved_names_match_case_sensitively() {
        // Only the exact spelling is reserved; the catalog is case-sensitive
        // everywhere else, and a case-folded rule would reject legitimate
        // feature names that merely resemble a field.
        assert!(is_reserved_feature_name("resolution"));
        for spelling in ["Resolution", "RESOLUTION", "resolution_", "my_resolution"] {
            assert!(!is_reserved_feature_name(spelling), "{spelling}");
            let features = features_of(&[(spelling, FeatureValue::Int(1))]);
            assert!(validate_features(&features).is_ok(), "{spelling}");
        }
    }
}
