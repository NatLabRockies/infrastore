use std::fmt;

use serde::{Deserialize, Serialize};

/// The catalog id of one time-series association — the only way to address a
/// stored series.
///
/// The catalog files every association under an `INTEGER PRIMARY KEY
/// AUTOINCREMENT`, so an id is never reissued once its row is deleted. A
/// consumer records the id a write hands back in its own object model (a
/// generator's `operation_cost` naming the series that varies it) and addresses
/// the series by it from then on. [`crate::Store::list_metadata`] recovers ids
/// from attributes for a caller that does not hold one.
///
/// A newtype rather than a bare `i64` because the store now hands out several
/// unrelated integer id streams — this one, `owner_id`, and the two association
/// catalogs' own ids — and every read, removal and rename takes one of them.
/// Passing an `owner_id` where a series id belongs is a type error here rather
/// than a lookup that silently finds the wrong row or none at all.
///
/// It is per-store and descriptive: `merge` assigns fresh ids, `diff` ignores
/// it, and `rename` / `reassign` / `compact` / `persist_to` all preserve it.
///
/// Serialized transparently as its integer, so the SQLite catalog, the gRPC
/// wire, and the OpenAPI document (where the schema spells it
/// `association_id`) are unchanged by the wrapper. The bindings exchange it as
/// a plain integer for the same reason.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct TimeSeriesId(pub i64);

impl TimeSeriesId {
    /// The underlying integer, for a boundary that has to speak in scalars —
    /// the C ABI, a SQLite bind, a protobuf field.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for TimeSeriesId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for TimeSeriesId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<TimeSeriesId> for i64 {
    fn from(id: TimeSeriesId) -> Self {
        id.0
    }
}

impl PartialEq<i64> for TimeSeriesId {
    fn eq(&self, other: &i64) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_its_integer() {
        let id = TimeSeriesId::from(7);
        assert_eq!(id.get(), 7);
        assert_eq!(i64::from(id), 7);
        assert_eq!(id.to_string(), "7");
    }

    #[test]
    fn serializes_as_a_bare_integer() {
        // The wrapper must be invisible on every wire the id already crossed:
        // the catalog, gRPC, and the OpenAPI document.
        let json = serde_json::to_string(&TimeSeriesId(42)).unwrap();
        assert_eq!(json, "42");
        assert_eq!(
            serde_json::from_str::<TimeSeriesId>("42").unwrap(),
            TimeSeriesId(42)
        );
    }

    #[test]
    fn optional_ids_serialize_as_null_or_integer() {
        assert_eq!(serde_json::to_string(&Some(TimeSeriesId(3))).unwrap(), "3");
        assert_eq!(
            serde_json::to_string(&Option::<TimeSeriesId>::None).unwrap(),
            "null"
        );
    }
}
