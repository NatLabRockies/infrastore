//! SQLite-backed metadata store.
//!
//! Stores [`TimeSeriesMetadata`] records and the (owner_id, type, name,
//! resolution, features) uniqueness invariant.

pub mod migrate;
pub mod schema;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Savepoint, params};
use serde::{Deserialize, Serialize};

use crate::types::id::TimeSeriesId;
use crate::types::period::Period;

use crate::error::{Result, TimeSeriesError};
use crate::hash::{features_hash, timestamps_hash};
use crate::storage::StorageBackend;
use crate::types::element_type::ElementType;
use crate::types::metadata::{
    FeatureValue, Features, OwnerCategory, TimeSeriesMetadata, UnitSystem,
};
use crate::types::time_reference::TimeReference;
use crate::types::time_series::TimeSeriesType;

/// The arrays the catalog references, paired with one diagnostic per row too
/// malformed to name one. Returned by [`MetadataStore::referenced_arrays`].
pub type ReferencedArrays = (Vec<([u8; 32], ElementType)>, Vec<String>);

/// The timestamp vectors the catalog references, paired with one diagnostic per
/// row too malformed to name one. The same shape as [`ReferencedArrays`], for
/// the same reason: a sweep must not act on a catalog it cannot read, and a
/// verification must report that rather than fail.
pub type ReferencedTimestamps = (HashSet<[u8; 32]>, Vec<String>);

/// A catalog row paired with its `INTEGER PRIMARY KEY` (SQLite rowid). Used
/// internally by [`Self::list_inner`]; public APIs surface metadata without
/// the raw storage id.
pub(crate) type IdentifiedRow = (i64, TimeSeriesMetadata);

/// A SQLite value's storage class, for a diagnostic that has to describe a
/// column holding the wrong kind of thing.
fn value_kind(value: &rusqlite::types::Value) -> &'static str {
    match value {
        rusqlite::types::Value::Null => "NULL",
        rusqlite::types::Value::Integer(_) => "an integer",
        rusqlite::types::Value::Real(_) => "a real",
        rusqlite::types::Value::Text(_) => "text",
        rusqlite::types::Value::Blob(_) => "a blob",
    }
}
/// Ids per `IN (...)` list in [`MetadataStore::list_by_ids`]. Well under
/// SQLite's default 32766 bound variables even with the predicate bound three
/// times, and large enough that a chunk is a bulk read in its own right.
const IDS_PER_QUERY: usize = 500;

/// Pages copied per step by [`MetadataStore::open_path_into_memory`]. The
/// online-backup API requires a positive step size; this one is large enough
/// that a realistic catalog finishes in a few iterations.
const BACKUP_PAGES_PER_STEP: std::ffi::c_int = 1024;

pub struct MetadataStore {
    conn: Connection,
    read_only: bool,
    /// Whether the two association tables exist on this connection.
    ///
    /// The DDL creates them on every writable open, so they are always present
    /// for a writable store. A read-only open of a store written before they
    /// existed cannot run DDL, and must degrade to empty reads rather than
    /// erroring — a table can never appear or vanish under a live connection, so
    /// this is resolved once at open.
    has_supplemental_attribute_table: bool,
    has_parent_child_table: bool,
    /// Recently decoded timestamp vectors — see [`TimestampCache`].
    ///
    /// `RefCell` rather than a lock because this type is already `!Sync` (it
    /// owns a `rusqlite::Connection`), so nothing can reach it from two threads
    /// at once; a `Store` crosses threads by being moved, and the gRPC server
    /// holds one behind its own `Mutex`.
    timestamps_cache: RefCell<TimestampCache>,
}

/// Timestamp vectors read back from the array file, memoized by content hash.
///
/// The read paths resolve one association at a time — `get_by_key` per key —
/// so a bulk read of N `NonSequentialTimeSeries` on one time axis fetched that
/// axis N times. Measured on 200 series over a 7,508-instant year, that was
/// ~70% of the whole call, for a vector it had already read 199 times.
///
/// This can be a plain memo with no invalidation because timestamp vectors are
/// **content-addressed and immutable**: a hash always maps to the same values,
/// so an entry can never go stale. The only thing it needs is a size bound, and
/// a small one suffices — the cache exists to collapse "one axis, read once per
/// row", not to be a general row cache. A store holds a handful of distinct
/// axes; series that do not share one miss anyway, and pay exactly what they
/// paid before.
///
/// One consequence worth naming: a hit is served without touching the store, so
/// if a vector were deleted *after* this process read it, the integrity error
/// [`MetadataStore::list_inner`] raises for a missing one would not fire for the
/// rest of the session. Nothing deletes a referenced vector (`compact` sweeps
/// only unreferenced ones), so that state is already corruption; the cache
/// narrows when it is reported, not whether.
#[derive(Debug, Default)]
struct TimestampCache {
    /// Least-recently used first. A linear scan is right at this size.
    entries: Vec<([u8; 32], Vec<DateTime<Utc>>)>,
}

impl TimestampCache {
    /// How many distinct time axes to keep decoded. Each entry costs one
    /// vector's worth of memory (12 bytes per timestamp), so this is deliberately
    /// small.
    const CAPACITY: usize = 4;

    fn get(&mut self, hash: &[u8; 32]) -> Option<Vec<DateTime<Utc>>> {
        let found = self.entries.iter().position(|(h, _)| h == hash)?;
        // Move to the back: the hot axis must not be evicted by a sweep over
        // several colder ones.
        let entry = self.entries.remove(found);
        let timestamps = entry.1.clone();
        self.entries.push(entry);
        Some(timestamps)
    }

    fn insert(&mut self, hash: [u8; 32], timestamps: &[DateTime<Utc>]) {
        if self.entries.iter().any(|(h, _)| *h == hash) {
            return;
        }
        if self.entries.len() == Self::CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((hash, timestamps.to_vec()));
    }
}

/// One grouped row of the static-series summary: a distinct
/// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
/// length)` combination and how many associations share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub time_step_count: Option<i64>,
    pub count: i64,
}

/// One grouped row of the forecast summary: a distinct
/// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
/// horizon, interval, window_count)` combination and how many associations
/// share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForecastSummaryRow {
    pub owner_type: String,
    pub owner_category: OwnerCategory,
    pub time_series_type: TimeSeriesType,
    pub name: String,
    pub initial_timestamp: Option<DateTime<Utc>>,
    pub resolution: Option<Period>,
    pub horizon: Option<Period>,
    pub interval: Option<Period>,
    pub window_count: Option<i64>,
    pub count: i64,
}

// ---- Association catalogs --------------------------------------------------
//
// Two tables, one shape. Both record a relationship between two catalog
// entities as a pair of `(id, type)` endpoints, so the SQL is written once
// against an [`AssocTable`] descriptor and each table supplies its own column
// names. The public types below are per-table and fully named, because the
// relationships are not interchangeable: a call site should never have to
// remember whether "from" means a component or an attribute.

/// Column layout of one association table. Every field is a `&'static str`
/// chosen in this module, never caller text, so interpolating them into SQL is
/// safe by construction.
#[derive(Debug, Clone, Copy)]
struct AssocTable {
    name: &'static str,
    left_id: &'static str,
    left_type: &'static str,
    right_id: &'static str,
    right_type: &'static str,
}

const SUPPLEMENTAL_ATTRIBUTE_TABLE: AssocTable = AssocTable {
    name: "supplemental_attribute_associations",
    left_id: "component_id",
    left_type: "component_type",
    right_id: "attribute_id",
    right_type: "attribute_type",
};

const PARENT_CHILD_TABLE: AssocTable = AssocTable {
    name: "parent_child_associations",
    left_id: "parent_id",
    left_type: "parent_type",
    right_id: "child_id",
    right_type: "child_type",
};

/// Which endpoint of an association a shared query addresses. Internal: the
/// public API names both endpoints outright (`list_children`,
/// `list_supplemental_attribute_ids`) rather than making callers pass a side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Left,
    Right,
}

impl AssocTable {
    fn id_column(self, endpoint: Endpoint) -> &'static str {
        match endpoint {
            Endpoint::Left => self.left_id,
            Endpoint::Right => self.right_id,
        }
    }

    fn type_column(self, endpoint: Endpoint) -> &'static str {
        match endpoint {
            Endpoint::Left => self.left_type,
            Endpoint::Right => self.right_type,
        }
    }
}

/// Table-agnostic predicate over an association table's four columns. Public
/// filters convert into this; it is never exposed.
#[derive(Debug, Default, Clone)]
struct EndpointFilter {
    left_id: Option<i64>,
    left_types: Option<Vec<String>>,
    right_id: Option<i64>,
    right_types: Option<Vec<String>>,
}

impl EndpointFilter {
    /// Render as a `WHERE` clause plus its bound parameters, so every query over
    /// either table (select, count, delete) shares one predicate builder.
    fn to_sql(&self, table: AssocTable) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut sql = String::from("WHERE 1=1");
        let mut params = Vec::new();
        push_eq(&mut sql, &mut params, table.left_id, self.left_id);
        push_eq(&mut sql, &mut params, table.right_id, self.right_id);
        push_type_list(&mut sql, &mut params, table.left_type, &self.left_types);
        push_type_list(&mut sql, &mut params, table.right_type, &self.right_types);
        (sql, params)
    }
}

/// One row of either association table, as the shared SQL returns it: the
/// catalog `id`, then the left endpoint's `(id, type)` and the right's. Named
/// because both tables share the same shape, so both `assoc_list` callers
/// destructure the same tuple.
type AssocRow = (i64, i64, String, i64, String);

/// One row of `supplemental_attribute_associations`: a supplemental attribute
/// attached to a component.
///
/// Identity is the `(component_id, attribute_id)` pair. The type names are
/// denormalized labels carried for filtering and reporting, so re-attaching the
/// same pair under different type names is still a duplicate.
///
/// [`Self::id`] is excluded from equality and hashing — see the `PartialEq` impl
/// below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplementalAttributeAssociation {
    pub component_id: i64,
    pub component_type: String,
    pub attribute_id: i64,
    pub attribute_type: String,
    /// The catalog row's `id`, or `None`.
    ///
    /// `Some` on anything read back; ignored on the way in, because this
    /// catalog's wire form carries no id and so has nothing to preserve — an
    /// attachment is always filed under an assigned id. See
    /// [`crate::TimeSeriesMetadata::id`], whose one import exception has no
    /// counterpart here, and note that the tables' id streams are independent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
}

/// Equality and hashing are over the endpoint pair and its labels, never the
/// `id`. Written out rather than derived, because deriving would quietly change
/// what these types mean: identity here is the `(component_id, attribute_id)`
/// pair — the doc above says so and the unique index enforces it — so two
/// values describing the same attachment must stay equal whether or not either
/// has been through the catalog. Folding the id in would also break `Hash`'s
/// contract with `Eq` for every set and map these land in.
impl PartialEq for SupplementalAttributeAssociation {
    fn eq(&self, other: &Self) -> bool {
        self.component_id == other.component_id
            && self.component_type == other.component_type
            && self.attribute_id == other.attribute_id
            && self.attribute_type == other.attribute_type
    }
}

impl Eq for SupplementalAttributeAssociation {}

impl std::hash::Hash for SupplementalAttributeAssociation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.component_id.hash(state);
        self.component_type.hash(state);
        self.attribute_id.hash(state);
        self.attribute_type.hash(state);
    }
}

/// One row of `parent_child_associations`: a directed edge between two
/// components, e.g. a generator (parent) connected to a bus (child).
///
/// Identity is the `(parent_id, child_id)` pair. Both endpoints are always
/// components — an attribute cannot appear here.
///
/// [`Self::id`] is excluded from equality and hashing, for the reasons given on
/// [`SupplementalAttributeAssociation`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParentChildAssociation {
    pub parent_id: i64,
    pub parent_type: String,
    pub child_id: i64,
    pub child_type: String,
    /// The catalog row's `id`, or `None`. See
    /// [`SupplementalAttributeAssociation::id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
}

impl PartialEq for ParentChildAssociation {
    fn eq(&self, other: &Self) -> bool {
        self.parent_id == other.parent_id
            && self.parent_type == other.parent_type
            && self.child_id == other.child_id
            && self.child_type == other.child_type
    }
}

impl Eq for ParentChildAssociation {}

impl std::hash::Hash for ParentChildAssociation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.parent_id.hash(state);
        self.parent_type.hash(state);
        self.child_id.hash(state);
        self.child_type.hash(state);
    }
}

/// One grouped row of the supplemental-attribute summary: a distinct
/// `(attribute_type, component_type)` pair and how many attachments share it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplementalAttributeSummaryRow {
    pub component_type: String,
    pub attribute_type: String,
    pub count: i64,
}

/// Predicate over `supplemental_attribute_associations`. All set fields are
/// ANDed; the default filter matches every row, which is what makes bulk export
/// a plain `list_supplemental_attribute_associations(&Default::default())`.
///
/// Serde-serializable so a binding can hand the whole predicate across a
/// boundary as one JSON value instead of four positional arguments; every field
/// is optional, so a partial object deserializes fine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementalAttributeFilter {
    pub component_id: Option<i64>,
    /// Concrete type names, rendered as SQL `IN (…)`. Expanding an abstract type
    /// into its concrete subtypes stays with the caller, where the type
    /// hierarchy lives. `Some(vec![])` is an empty allow-list and matches
    /// nothing.
    pub component_types: Option<Vec<String>>,
    pub attribute_id: Option<i64>,
    pub attribute_types: Option<Vec<String>>,
}

impl SupplementalAttributeFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn component_id(mut self, id: i64) -> Self {
        self.component_id = Some(id);
        self
    }
    pub fn component_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.component_types = Some(types.into_iter().map(Into::into).collect());
        self
    }
    pub fn attribute_id(mut self, id: i64) -> Self {
        self.attribute_id = Some(id);
        self
    }
    pub fn attribute_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.attribute_types = Some(types.into_iter().map(Into::into).collect());
        self
    }

    fn endpoints(&self) -> EndpointFilter {
        EndpointFilter {
            left_id: self.component_id,
            left_types: self.component_types.clone(),
            right_id: self.attribute_id,
            right_types: self.attribute_types.clone(),
        }
    }
}

/// Predicate over `parent_child_associations`, with the same semantics as
/// [`SupplementalAttributeFilter`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentChildFilter {
    pub parent_id: Option<i64>,
    pub parent_types: Option<Vec<String>>,
    pub child_id: Option<i64>,
    pub child_types: Option<Vec<String>>,
}

impl ParentChildFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn parent_id(mut self, id: i64) -> Self {
        self.parent_id = Some(id);
        self
    }
    pub fn parent_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.parent_types = Some(types.into_iter().map(Into::into).collect());
        self
    }
    pub fn child_id(mut self, id: i64) -> Self {
        self.child_id = Some(id);
        self
    }
    pub fn child_types(mut self, types: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.child_types = Some(types.into_iter().map(Into::into).collect());
        self
    }

    fn endpoints(&self) -> EndpointFilter {
        EndpointFilter {
            left_id: self.parent_id,
            left_types: self.parent_types.clone(),
            right_id: self.child_id,
            right_types: self.child_types.clone(),
        }
    }
}

/// Append an equality predicate for a set optional value. Every `column` in this
/// module is a literal chosen here, never caller text.
fn push_eq<T: rusqlite::ToSql + 'static>(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::ToSql>>,
    column: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        sql.push_str(&format!(" AND {column} = ?"));
        params.push(Box::new(value));
    }
}

/// Append an `IN (…)` predicate for a type allow-list.
fn push_type_list(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::ToSql>>,
    column: &str,
    types: &Option<Vec<String>>,
) {
    let Some(types) = types else { return };
    if types.is_empty() {
        // An empty allow-list selects nothing. SQLite rejects `IN ()`, so say it
        // with a false constant instead.
        sql.push_str(" AND 0");
        return;
    }
    let placeholders = vec!["?"; types.len()].join(", ");
    sql.push_str(&format!(" AND {column} IN ({placeholders})"));
    for t in types {
        params.push(Box::new(t.clone()));
    }
}

fn parse_opt_rfc3339(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| TimeSeriesError::IntegrityError(format!("bad timestamp: {e}")))?
                .with_timezone(&Utc),
        )),
    }
}

/// Decode a stored `owner_category` code. An unknown code means the catalog
/// was written by an incompatible version, hence `IntegrityError`.
fn decode_category(code: i64) -> Result<OwnerCategory> {
    OwnerCategory::from_code(code)
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad owner_category code {code}")))
}

/// Decode a stored `time_series_type` code. As with [`decode_category`], an
/// unknown code is an integrity failure rather than a filterable value.
fn decode_type(code: i64) -> Result<TimeSeriesType> {
    TimeSeriesType::from_code(code)
        .ok_or_else(|| TimeSeriesError::IntegrityError(format!("bad time_series_type code {code}")))
}

#[derive(Debug, Default, Clone)]
pub struct MetadataFilter {
    pub owner_id: Option<i64>,
    pub owner_category: Option<OwnerCategory>,
    pub owner_type: Option<String>,
    /// Type predicate: a widening read *request*, or one exact stored type.
    /// See [`TypeMatch`].
    pub time_series_type: Option<TypeMatch>,
    pub name: Option<String>,
    /// SQLite `GLOB` pattern on the name (case-sensitive; `*`/`?` wildcards).
    /// Combined with `name` as AND when both are set.
    pub name_glob: Option<String>,
    /// Exact match on the owning component's field (e.g. `"max_active_power"`).
    /// Case-sensitive, like every other identifier predicate here. A row that
    /// declares no `component_field` matches no value, since SQL equality is
    /// never true against NULL.
    pub component_field: Option<String>,
    /// Coherence predicate on the timestamp spelling: `Some(true)` selects the
    /// rows whose `time_reference` is `'zoneless'`, `Some(false)` selects
    /// everything else — the three zoned spellings *and* the rows that left the
    /// column NULL.
    ///
    /// A binary predicate rather than an exact match on purpose. The rows the
    /// store rejects a mixed selection over are exactly these two groups, and
    /// an exact-match filter cannot express the second one: an unset column
    /// matches no value at all under SQL equality (the trap
    /// [`Self::component_field`] documents). Here those rows are a coherence
    /// group, not an oversight.
    pub zoneless: Option<bool>,
    pub resolution: Option<Period>,
    /// Forecast window interval. When set, restricts to rows with exactly this
    /// interval (part of the identity); `None` does not filter on interval.
    pub interval: Option<Period>,
    /// Subset match: rows must contain at least these key/value pairs.
    pub features: Option<Features>,
    /// Exact features-set match by precomputed hash. When set, this is pushed
    /// into the SQL WHERE so the `uq_ts_assoc` unique index can pinpoint the row,
    /// avoiding a feature fetch+compare for siblings that share the other key
    /// columns. Distinct from `features` (an in-memory subset filter).
    pub features_hash: Option<[u8; 32]>,
    /// Restrict to these catalog ids — a primary-key lookup, not a scan.
    ///
    /// Internal to this layer, deliberately: the public [`crate::ListFilter`]
    /// has no counterpart, because a filter field invites scan-shaped use of
    /// what is a point lookup. It is here so the by-id reads share
    /// [`Self::list_inner`]'s feature and timestamp hydration instead of
    /// growing a second copy of it, and so a bulk by-id read is one query
    /// rather than one per id.
    ///
    /// `Some(vec![])` matches nothing, which is what a caller asking for no ids
    /// means; it is spelled as a false predicate rather than an empty `IN ()`,
    /// which is not valid SQLite.
    pub ids: Option<Vec<i64>>,
}

/// Remembers which content-addressed sets a batch of inserts has already
/// written, so each distinct one is written once per batch rather than once per
/// row. Scoped to a single transaction: it records what *this* batch wrote, and
/// carries no meaning once that transaction ends.
///
/// Two content-addressed sets hang off an association row — its feature set,
/// written into the catalog beside the row, and, for a
/// `NonSequentialTimeSeries`, its timestamp vector, written into the array file
/// before it. Both are shared, so one cache covers both. The timestamp half is
/// what keeps a bulk add of ten thousand irregular series on one time axis from
/// asking the backend ten thousand times for the same (potentially long) vector.
#[derive(Debug, Default)]
pub struct SharedSetCache {
    features: HashSet<[u8; 32]>,
    timestamps: HashSet<[u8; 32]>,
}

impl SharedSetCache {
    /// Record that this batch is writing the timestamp vector `hash`, returning
    /// true the first time it sees it. The write itself is the caller's — the
    /// vector lives in the array file, which the catalog has no handle on.
    pub fn note_timestamps(&mut self, hash: [u8; 32]) -> bool {
        self.timestamps.insert(hash)
    }
}

/// The full identity of one stored association: everything the uniqueness
/// invariant keys on. Periods stay in their catalog ISO-8601 encoding — this is
/// an equality/hash token, never a value to compute with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssociationIdentity {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub resolution: Option<String>,
    pub interval: Option<String>,
    pub features_hash: [u8; 32],
}

/// An association identity with the interval projected away: the "family" of
/// series that describe one variable of one owner at one resolution, across all
/// forecast intervals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesFamily {
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub resolution: Option<String>,
    pub features_hash: [u8; 32],
}

impl From<AssociationIdentity> for SeriesFamily {
    fn from(id: AssociationIdentity) -> Self {
        Self {
            owner_id: id.owner_id,
            owner_category: id.owner_category,
            name: id.name,
            resolution: id.resolution,
            features_hash: id.features_hash,
        }
    }
}

/// How a `time_series_type` predicate matches stored rows.
///
/// The distinction is load-bearing for both correctness and cost. A caller that
/// holds a [`crate::KeyIdentity`] already knows the row's concrete type, so
/// widening it would probe the uniqueness index twice and could match a row the
/// key does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeMatch {
    /// A read *request*, per [`TimeSeriesType::accepts`]: `Deterministic` spans
    /// both of its storage forms, every other type matches only itself.
    Requested(TimeSeriesType),
    /// Exactly one stored type — never widened.
    Exact(TimeSeriesType),
}

impl TypeMatch {
    /// The inclusive `(low, high)` storage-code range this predicate matches.
    fn code_span(self) -> (i64, i64) {
        match self {
            TypeMatch::Requested(t) => t.code_span(),
            TypeMatch::Exact(t) => (t.code(), t.code()),
        }
    }
}

/// Which SQL form a *widening* type predicate takes.
///
/// `Deterministic` and `DeterministicSingleTimeSeries` have adjacent codes, so
/// a widening request is expressible either as a `BETWEEN` range or a two-value
/// `IN`. Neither is universally better, and the difference is a query-plan
/// cliff rather than a constant factor:
///
/// * Alone on `idx_ts_type` (counts, type-scoped scans), `BETWEEN` is one index
///   seek where `IN` performs two — measured ~1.1x faster.
/// * Inside `uq_ts_assoc`, `time_series_type` is a *middle* column, and an
///   inequality there stops SQLite using every column after it. The plan drops
///   from a five-column covering seek to a fallback on `idx_name`. `IN` is
///   treated as equality-with-loop and keeps the full seek.
///
/// So the form is chosen by whether the filter also constrains a column that
/// follows `time_series_type` in that index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanForm {
    /// `BETWEEN ? AND ?` — for predicates standing alone on `idx_ts_type`.
    Range,
    /// `IN (?, ?)` — preserves the composite seek on `uq_ts_assoc`.
    In,
}

/// Append the `time_series_type` predicate for `requested` to `sql`/`params`.
///
/// The SQL form of [`TimeSeriesType::accepts`]: an equality when the predicate
/// names one code, otherwise `form`'s widening shape. Driven by
/// [`TypeMatch::code_span`] so the SQL and the in-memory rule cannot drift.
/// Shared by every catalog query that filters on type.
fn push_type_predicate(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::ToSql>>,
    requested: TypeMatch,
    form: SpanForm,
) {
    let (lo, hi) = requested.code_span();
    if lo == hi {
        sql.push_str(" AND time_series_type = ?");
        params.push(Box::new(lo));
        return;
    }
    match form {
        SpanForm::Range => {
            sql.push_str(" AND time_series_type BETWEEN ? AND ?");
            params.push(Box::new(lo));
            params.push(Box::new(hi));
        }
        SpanForm::In => {
            sql.push_str(" AND time_series_type IN (");
            for (i, code) in (lo..=hi).enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push('?');
                params.push(Box::new(code));
            }
            sql.push(')');
        }
    }
}

impl MetadataFilter {
    /// Which widening form the type predicate should take — see [`SpanForm`].
    ///
    /// `uq_ts_assoc` is
    /// `(owner_id, owner_category, time_series_type, name, resolution, interval, features_hash)`.
    /// If this filter constrains any column *after* `time_series_type`, the
    /// query wants that composite seek and a range would truncate it, so the
    /// `IN` form is used. Otherwise the predicate stands alone and the range
    /// form is the cheaper one.
    fn span_form(&self) -> SpanForm {
        let constrains_later_column = self.name.is_some()
            || self.name_glob.is_some()
            || self.resolution.is_some()
            || self.interval.is_some()
            || self.features_hash.is_some();
        if constrains_later_column {
            SpanForm::In
        } else {
            SpanForm::Range
        }
    }

    /// Render the filter as a `WHERE` clause plus its bound parameters, so the
    /// same predicate can be reused across the row query and the batched
    /// features query without building the SQL twice.
    ///
    /// `features` is not represented here: it is a subset match applied
    /// in memory after hydration, not a SQL predicate.
    fn to_sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut sql = String::from("WHERE 1=1");
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(ref ids) = self.ids {
            if ids.is_empty() {
                sql.push_str(" AND 0");
            } else {
                sql.push_str(" AND id IN (");
                for (i, id) in ids.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push('?');
                    params_vec.push(Box::new(*id));
                }
                sql.push(')');
            }
        }
        if let Some(owner_id) = self.owner_id {
            sql.push_str(" AND owner_id = ?");
            params_vec.push(Box::new(owner_id));
        }
        if let Some(owner_category) = self.owner_category {
            sql.push_str(" AND owner_category = ?");
            params_vec.push(Box::new(owner_category.code()));
        }
        if let Some(ref owner_type) = self.owner_type {
            sql.push_str(" AND owner_type = ?");
            params_vec.push(Box::new(owner_type.clone()));
        }
        if let Some(requested) = self.time_series_type {
            push_type_predicate(&mut sql, &mut params_vec, requested, self.span_form());
        }
        if let Some(ref name) = self.name {
            sql.push_str(" AND name = ?");
            params_vec.push(Box::new(name.clone()));
        }
        if let Some(ref pattern) = self.name_glob {
            sql.push_str(" AND name GLOB ?");
            params_vec.push(Box::new(pattern.clone()));
        }
        if let Some(ref component_field) = self.component_field {
            sql.push_str(" AND component_field = ?");
            params_vec.push(Box::new(component_field.clone()));
        }
        if let Some(zoneless) = self.zoneless {
            // `<>` is never true against NULL, so the negative arm has to say
            // "IS NULL OR" explicitly -- those rows belong to the zoned group.
            sql.push_str(if zoneless {
                " AND time_reference = 'zoneless'"
            } else {
                " AND (time_reference IS NULL OR time_reference <> 'zoneless')"
            });
        }
        if let Some(resolution) = self.resolution {
            sql.push_str(" AND resolution = ?");
            params_vec.push(Box::new(period_to_iso(resolution)));
        }
        if let Some(interval) = self.interval {
            sql.push_str(" AND interval = ?");
            params_vec.push(Box::new(period_to_iso(interval)));
        }
        if let Some(ref f_hash) = self.features_hash {
            sql.push_str(" AND features_hash = ?");
            params_vec.push(Box::new(f_hash.to_vec()));
        }
        (sql, params_vec)
    }
}

impl MetadataStore {
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        let has_supplemental_attribute_table =
            table_exists(&conn, "supplemental_attribute_associations")?;
        let has_parent_child_table = table_exists(&conn, "parent_child_associations")?;
        Ok(Self {
            conn,
            read_only: false,
            has_supplemental_attribute_table,
            has_parent_child_table,
            timestamps_cache: RefCell::default(),
        })
    }

    /// Copy the entire metadata database to a new SQLite file at `path`
    /// (used to materialize an in-memory store to disk). SQLite's `VACUUM INTO`
    /// creates the target, which must not already exist.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![path.to_string_lossy()])?;
        Ok(())
    }

    /// Copy the catalog file at `src` to `dest` without interpreting its
    /// schema — [`crate::Store::open_copy`]'s catalog half.
    ///
    /// Deliberately not `open_path(src, true)?.backup_to(dest)`. A read-only
    /// open runs [`migrate::check_read_only`], which refuses any catalog this
    /// build would migrate; refusing to *copy* such a catalog is backwards,
    /// because taking a writable copy is exactly how a caller migrates one
    /// without touching the original. `VACUUM INTO` reads pages and never the
    /// schema, so there is nothing here a revision could invalidate.
    ///
    /// Like [`Self::backup_to`], this reads through committed WAL content, so
    /// the copy needs no sidecar of its own, and `dest` must not already exist.
    pub fn copy_file_to(src: &Path, dest: &Path) -> Result<()> {
        let conn = open_read_only(src)?;
        // The one pragma from `init` that still applies: another handle to the
        // same artifact may hold a lock, and waiting beats a bare SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
        Ok(())
    }

    /// The generation stamp in the catalog file at `path`, read **without**
    /// opening it as a catalog — no `init`, no DDL, no migration ladder.
    ///
    /// [`crate::Store::open_with_catalog`]'s preflight. The stamp says whether
    /// these two files are halves of the same save, and that question has to be
    /// answered *before* anything writes to either of them: migrating the
    /// catalog of an artifact that is about to be rejected mutates a user's
    /// file to report a failure.
    ///
    /// `None` for a catalog with no `catalog_identity` table — one predating the
    /// stamp, which compares equal to an unstamped HDF5 half and is the
    /// legitimate pre-stamp artifact.
    ///
    /// A path with no catalog file also reads as `None`, but callers should not
    /// lean on that to detect one: a missing half is a different failure from a
    /// stamp disagreement and deserves its own diagnostic.
    /// [`crate::Store::open_with_catalog`] checks for the file before calling
    /// this, so the better error still gets there first.
    ///
    /// Errors only if the file is there and cannot be read. A damaged catalog
    /// is not silently treated as unstamped.
    pub fn read_generation_at(path: &Path) -> Result<Option<String>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = open_read_only(path)?;
        if !table_exists(&conn, "catalog_identity")? {
            return Ok(None);
        }
        Ok(conn
            .query_row("SELECT generation FROM catalog_identity LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn open_path(path: &Path, read_only: bool) -> Result<Self> {
        let conn = if read_only {
            open_read_only(path)?
        } else {
            Connection::open(path)?
        };
        Self::init(&conn)?;
        let has_supplemental_attribute_table =
            table_exists(&conn, "supplemental_attribute_associations")?;
        let has_parent_child_table = table_exists(&conn, "parent_child_associations")?;
        Ok(Self {
            conn,
            read_only,
            has_supplemental_attribute_table,
            has_parent_child_table,
            timestamps_cache: RefCell::default(),
        })
    }

    /// Copy an existing catalog file into a fresh in-memory database.
    ///
    /// The inverse of [`Self::backup_to`], but not its mirror image: `VACUUM
    /// INTO` cannot target `:memory:`, so this goes through SQLite's
    /// online-backup API instead. `path` is opened read-only and never written
    /// — once loaded, the only route back to disk is [`Store::persist_to`].
    ///
    /// The DDL runs *after* the copy, which makes this strictly better than a
    /// file open for stores predating an additive table: a read-only file open
    /// cannot run DDL and has to degrade to empty reads (see
    /// [`Self::has_supplemental_attribute_table`]), whereas copying into a
    /// writable in-memory database picks the table up. `read_only` is therefore
    /// only the software guard on mutation, not a property of the connection.
    pub fn open_path_into_memory(path: &Path, read_only: bool) -> Result<Self> {
        let src = open_read_only(path)?;
        let mut conn = Connection::open_in_memory()?;
        {
            // Both handles are exclusively ours, so there is no reader to yield
            // to between steps: no pause, and a step size large enough that a
            // realistic catalog copies in a handful of iterations.
            let backup = rusqlite::backup::Backup::new(&src, &mut conn)?;
            backup.run_to_completion(BACKUP_PAGES_PER_STEP, std::time::Duration::ZERO, None)?;
        }
        drop(src);
        Self::init(&conn)?;
        let has_supplemental_attribute_table =
            table_exists(&conn, "supplemental_attribute_associations")?;
        let has_parent_child_table = table_exists(&conn, "parent_child_associations")?;
        Ok(Self {
            conn,
            read_only,
            has_supplemental_attribute_table,
            has_parent_child_table,
            timestamps_cache: RefCell::default(),
        })
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        // Wait, rather than failing immediately with SQLITE_BUSY, when another
        // handle to the same on-disk artifact holds a lock (e.g. a CLI writer
        // and the read-only gRPC server overlapping). Harmless for in-memory and
        // read-only connections, which still acquire SHARED locks.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // WAL journal mode, for the *read* path: in rollback-journal mode every
        // read transaction pays a hot-journal `stat()` plus a shared-lock
        // fcntl dance per statement — ~20% of a key lookup in a per-series
        // read loop. WAL drops both (readers coordinate through the WAL
        // index) and also lets readers overlap a writer. The `-wal`/`-shm`
        // sidecars do not outlive the store: SQLite checkpoints and removes
        // them when the last connection closes, and `Store::persist_to`
        // checkpoints explicitly before copying the artifact pair, so the
        // "move the file and its .sqlite together" contract holds. A sidecar
        // can survive a crash; a read-write open recovers it (a read-only
        // open of a crashed store fails until one happens).
        //
        // Skipped for read-only connections (the pragma writes the header on
        // a rollback-mode file; a cleanly closed WAL store reads fine without
        // it) and no-ops for in-memory databases.
        if !conn.is_readonly(rusqlite::DatabaseName::Main)? {
            // `journal_mode` returns a row ("wal"); use `query_row` to consume
            // it — `execute` would report a misuse error.
            conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
            // NORMAL sync under WAL: commits stop fsyncing the WAL (the sync
            // moves to checkpoints), which is the dominant cost of small
            // single-op transactions — a per-series removal spends ~25% of its
            // time in that fsync. WAL guarantees the database stays consistent
            // either way; what NORMAL gives up is durability of the last few
            // commits on an OS crash or power loss, which this store accepts:
            // the artifact is rebuilt from serialized systems, and the prior
            // metadata store held everything in process memory.
            conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        }
        // `prepare_cached` keys on SQL text, and `MetadataFilter` renders a
        // distinct statement per combination of set predicates (times two, since
        // `list` issues a row query and a features query). rusqlite's default
        // cache holds 16, which a mixed workload can thrash past, silently
        // re-parsing on every call. Room for the realistic shapes is cheap.
        conn.set_prepared_statement_cache_capacity(64);
        // Bring the catalog to the current shape. A writable connection climbs
        // the migration ladder and then re-applies the (idempotent) DDL; see
        // `migrate::apply` for why that order matters. A read-only connection
        // can do neither — `CREATE TABLE IF NOT EXISTS` against a read-only
        // database errors even when it would be a no-op — so it only reports
        // whether what it is about to read is a shape this build understands.
        if conn.is_readonly(rusqlite::DatabaseName::Main)? {
            migrate::check_read_only(conn)?;
        } else {
            migrate::apply(conn)?;
        }
        Ok(())
    }

    /// The catalog's schema revision — see
    /// [`migrate::CATALOG_SCHEMA_REVISION`]. A catalog predating the stamp
    /// reads as revision 1, the shape any pre-ladder build stamped.
    pub fn schema_revision(&self) -> Result<i64> {
        migrate::read_revision(&self.conn)
    }

    /// This catalog's generation stamp — see the `catalog_identity` DDL.
    ///
    /// `None` when the catalog predates the stamp, and also for a read-only open
    /// of such a catalog, which cannot run the DDL that would create the table.
    /// Both read as "unstamped", which `Store::open` treats as a skipped check
    /// rather than a mismatch.
    pub fn generation(&self) -> Result<Option<String>> {
        if !table_exists(&self.conn, "catalog_identity")? {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row("SELECT generation FROM catalog_identity LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    /// Stamp this catalog, replacing any existing value. The table holds at most
    /// one row, so this clears it first.
    pub fn set_generation(&self, generation: &str) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        self.conn.execute("DELETE FROM catalog_identity", [])?;
        self.conn.execute(
            "INSERT INTO catalog_identity (generation) VALUES (?1)",
            params![generation],
        )?;
        Ok(())
    }

    /// Flush the WAL into the main database file and truncate it, so the
    /// `.sqlite` artifact is complete on its own (required before any
    /// file-level copy of it). No-op for read-only connections — which cannot
    /// checkpoint, and see only cleanly closed stores whose WAL is already
    /// empty — for in-memory databases (not in WAL mode), and while a
    /// transaction is open on this connection: checkpointing there would error
    /// (`SQLITE_LOCKED`), and mid-transaction the artifact is incomplete by
    /// definition — the post-commit flush checkpoints instead.
    pub fn checkpoint(&self) -> Result<()> {
        if self.read_only || !self.conn.is_autocommit() {
            return Ok(());
        }
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Begin a scoped unit of work, rolled back if the guard is dropped without
    /// [`Savepoint::commit`].
    ///
    /// This is a SQLite *savepoint* rather than a transaction so that it nests:
    /// outside any enclosing unit of work it behaves exactly like `BEGIN` /
    /// `COMMIT` (SQLite starts a transaction implicitly and releasing the
    /// outermost savepoint commits it), while inside one — see
    /// [`Store::begin_transaction`](crate::Store::begin_transaction) — it scopes
    /// just its own statements and leaves the enclosing transaction open.
    /// Every mutating entry point takes one, so each is atomic on its own and
    /// composes into a caller's larger transaction without changing behavior.
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        Ok(self.conn.savepoint()?)
    }

    /// Run `sql` directly on the connection, for the raw
    /// `SAVEPOINT`/`RELEASE`/`ROLLBACK TO` statements that drive a cross-operation
    /// transaction. Those cannot use [`Self::savepoint`]: the guard would have to
    /// outlive the call that opened it, which is precisely the borrow that does
    /// not survive a C ABI boundary.
    pub(crate) fn execute_txn_stmt(&self, sql: &str) -> Result<()> {
        if self.read_only {
            return Err(TimeSeriesError::ReadOnlyStore);
        }
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    /// Insert a metadata record + its features inside the supplied transaction.
    /// Returns the association id. Caller is responsible for committing.
    pub fn insert(tx: &Connection, meta: &TimeSeriesMetadata) -> Result<i64> {
        Self::insert_batched(tx, meta, &mut SharedSetCache::default())
    }

    /// [`Self::insert`], but reusing a caller-held [`SharedSetCache`] across the
    /// rows of one batch.
    ///
    /// Feature sets and timestamp vectors are content-addressed and shared, so
    /// in a batch that inserts N rows over a handful of distinct ones, all but
    /// the first row per set would issue `INSERT OR IGNORE` statements that
    /// write nothing. The cache remembers what this batch has already written
    /// and skips the rest — which is what stops a bulk add, and a transform,
    /// from scaling with the number of features (or timestamps) per series.
    pub fn insert_batched(
        tx: &Connection,
        meta: &TimeSeriesMetadata,
        cache: &mut SharedSetCache,
    ) -> Result<i64> {
        let f_hash = features_hash(&meta.features);
        let initial_ts = meta.initial_timestamp.map(|t| t.to_rfc3339());
        let resolution_iso = meta.resolution.map(period_to_iso);
        let horizon_iso = meta.horizon.map(period_to_iso);
        let interval_iso = meta.interval.map(period_to_iso);
        let timestamps_hash = meta.timestamps.as_deref().map(timestamps_hash);
        let percentiles_json = match &meta.percentiles {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let element_shape_json = serde_json::to_string(&meta.element_shape)?;

        // `prepare_cached` so bulk adds parse each INSERT's SQL once per
        // connection instead of once per row.
        //
        // `id` is bound rather than omitted: binding `NULL` into an `INTEGER
        // PRIMARY KEY` is how SQLite is asked to assign one, so a
        // caller-supplied id and an assigned one are the same statement -- no
        // second prepared statement, and no branch that could drift between
        // the two.
        //
        // The id is read back with `last_insert_rowid()`, *not* `RETURNING`.
        // `RETURNING` makes the statement need a statement journal, and with
        // an enclosing savepoint open (a caller's transaction) every such
        // statement truncates the sub-journal on close -- a front-to-back walk
        // of a chunk list when the catalog is in memory, over a journal that
        // grows with every page the transaction has touched. A batched load
        // under one transaction went quadratic on it: the tenth batch of 10k
        // rows was fifty times slower than the first. `last_insert_rowid()` is
        // per-connection and reports the most recent rowid insert, so it must
        // be read before the feature-set insert below, which would clobber it;
        // `tests/bulk_add_in_transaction.rs` pins the cost.
        let mut insert_stmt = tx.prepare_cached(
            "INSERT INTO time_series_associations
             (id, owner_id, owner_type, owner_category, time_series_type, name, data_hash,
              initial_timestamp, resolution, length, horizon, interval, count,
              timestamps_hash, units, quantity_kind, unit_system, time_reference,
              component_field, percentiles_json, element_type, element_shape,
              application_data, features_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
        )?;
        let result = insert_stmt
            .execute(params![
                meta.id.map(|i| i.get()),
                meta.owner_id,
                meta.owner_type,
                meta.owner_category.code(),
                meta.time_series_type.code(),
                meta.name,
                meta.data_hash.as_slice(),
                initial_ts,
                resolution_iso,
                meta.length.map(|l| l as i64),
                horizon_iso,
                interval_iso,
                meta.count.map(|c| c as i64),
                timestamps_hash.map(|h| h.to_vec()),
                meta.units,
                meta.quantity_kind,
                meta.unit_system.map(|u| u.as_str()),
                meta.time_reference
                    .as_ref()
                    .map(TimeReference::as_storage_string),
                meta.component_field,
                percentiles_json,
                meta.element_type.to_string(),
                element_shape_json,
                meta.application_data,
                f_hash.as_slice(),
            ])
            .map(|_| tx.last_insert_rowid());

        let id = match result {
            Ok(id) => id,
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                // Two different collisions arrive here as the same primary
                // result code, and they mean opposite things to the caller, so
                // the extended code is what tells them apart.
                //
                //   * PRIMARYKEY -- the caller supplied an explicit `id` that
                //     is already taken. Only reachable when `meta.id` is
                //     `Some`; an assigned id cannot collide by construction.
                //   * UNIQUE -- the identity tuple collided on the index over
                //     (owner_id, owner_category, time_series_type, name,
                //     resolution, interval, features_hash).
                //
                // Anything else constraint-shaped is left as the raw error
                // rather than guessed at: reporting a wrong one of these two
                // sends a caller looking in the wrong place entirely.
                return Err(match (err.extended_code, meta.id) {
                    (rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY, Some(id)) => {
                        TimeSeriesError::DuplicateAssociationId(id.get())
                    }
                    (rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE, _) => {
                        TimeSeriesError::DuplicateTimeSeries
                    }
                    _ => rusqlite::Error::SqliteFailure(err, None).into(),
                });
            }
            Err(e) => return Err(e.into()),
        };

        // `insert` on the cache returns true the first time this batch sees the
        // set; every later row carrying it is a no-op we can skip outright.
        if cache.features.insert(f_hash) {
            Self::insert_feature_set(tx, &f_hash, &meta.features)?;
        }

        Ok(id)
    }

    /// Record a feature set under its content hash, if it is not already stored.
    ///
    /// `OR IGNORE` makes this a no-op whenever some other association already
    /// wrote this exact set — which is the common case, and the whole point of
    /// content-addressing them: a derived `DeterministicSingleTimeSeries` shares
    /// its source's features, so it writes nothing here.
    ///
    /// Equal hash implies equal set (SHA-256 of the canonical encoding), so an
    /// ignored conflict cannot silently keep a *different* set under the same
    /// hash.
    fn insert_feature_set(tx: &Connection, f_hash: &[u8; 32], features: &Features) -> Result<()> {
        if features.is_empty() {
            return Ok(());
        }
        let mut feature_stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO feature_sets
             (features_hash, key, value_kind, value_int, value_float, value_bool, value_str)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (k, v) in features {
            let (kind, vi, vf, vb, vs): (
                &str,
                Option<i64>,
                Option<f64>,
                Option<i64>,
                Option<&str>,
            ) = match v {
                FeatureValue::Int(i) => ("int", Some(*i), None, None, None),
                FeatureValue::Float(f) => ("float", None, Some(*f), None, None),
                FeatureValue::Bool(b) => ("bool", None, None, Some(*b as i64), None),
                FeatureValue::Str(s) => ("str", None, None, None, Some(s.as_str())),
            };
            feature_stmt.execute(params![f_hash.as_slice(), k, kind, vi, vf, vb, vs])?;
        }
        Ok(())
    }

    /// Delete feature sets no association references any more, and return how
    /// many rows went. Deleting an association leaves its set behind (sets are
    /// shared, so deletion cannot cascade); this reclaims the ones that are now
    /// unreachable. Called from [`crate::Store::compact`].
    pub fn sweep_orphan_feature_sets(tx: &Connection) -> Result<usize> {
        let n = tx.execute(
            "DELETE FROM feature_sets
             WHERE features_hash NOT IN
                   (SELECT DISTINCT features_hash FROM time_series_associations)",
            [],
        )?;
        Ok(n)
    }

    /// The timestamp-vector counterpart of [`Self::sweep_orphan_feature_sets`],
    /// stopping one step short of it: the vectors live in the array file, not
    /// the catalog, so this reports which ones are still referenced and
    /// [`crate::Store::compact`] deletes the rest.
    ///
    /// Removing the last `NonSequentialTimeSeries` on a time axis leaves its
    /// vector behind — vectors are shared, so a deletion cannot cascade — and
    /// that is what the sweep reclaims.
    pub fn referenced_timestamp_hashes(&self) -> Result<ReferencedTimestamps> {
        // Read as a dynamic value for the same reason `referenced_arrays` does:
        // SQLite is dynamically typed, so a corrupted row can hold anything in a
        // BLOB column, and a typed getter would fail the whole query instead of
        // letting the caller report that one row.
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT timestamps_hash FROM time_series_associations
             WHERE timestamps_hash IS NOT NULL",
        )?;
        let mut out = HashSet::new();
        let mut problems = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let value: rusqlite::types::Value = row.get(0)?;
            let rusqlite::types::Value::Blob(bytes) = value else {
                problems.push(format!(
                    "malformed catalog row: timestamps_hash is {}, expected a 32-byte blob",
                    value_kind(&value)
                ));
                continue;
            };
            match bytes_to_hash32(&bytes) {
                Some(hash) => {
                    out.insert(hash);
                }
                None => problems.push(format!(
                    "malformed catalog row: timestamps_hash is {} bytes, expected 32",
                    bytes.len()
                )),
            }
        }
        Ok((out, problems))
    }

    /// Delete the one association filed under `id`. Returns its data_hash and
    /// stored type, or `None` if the catalog holds no such row — the caller
    /// decides whether a stale reference is an error.
    ///
    /// The id is the primary key, so this names exactly one row: unlike
    /// [`Self::delete_by_key`], whose NULL-interval wildcard can sweep a whole
    /// forecast family, a removal by reference removes only what the reference
    /// points at.
    ///
    /// `expected_owner`, when given, is a guard: the row is deleted only if it
    /// belongs to that owner, and otherwise nothing is deleted and
    /// [`TimeSeriesError::OwnerMismatch`] is returned. The owner is read by the
    /// same statement pair, under the caller's transaction, that does the
    /// delete — which is the whole point of taking it here rather than letting
    /// the caller check first: an id survives reassignment, so an owner
    /// confirmed by an earlier call can be the wrong one by the time the
    /// `DELETE` runs.
    pub fn delete_by_id(
        tx: &Connection,
        id: i64,
        expected_owner: Option<(i64, OwnerCategory)>,
    ) -> Result<Option<DeletedRow>> {
        let row: Option<RawDeletedRow> = tx
            .prepare_cached(
                "SELECT data_hash, time_series_type, owner_id, owner_category, name, \
                        resolution, features_hash \
                 FROM time_series_associations WHERE id = ?1",
            )?
            .query_row(params![id], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                ))
            })
            .optional()?;
        let Some((
            hash_bytes,
            type_code,
            owner_id,
            owner_category_code,
            name,
            resolution_iso,
            features_hash_bytes,
        )) = row
        else {
            return Ok(None);
        };
        let owner_category = decode_category(owner_category_code)?;
        if let Some((expected_id, expected_category)) = expected_owner {
            let actual_category = owner_category;
            if owner_id != expected_id || actual_category != expected_category {
                return Err(TimeSeriesError::OwnerMismatch {
                    id,
                    expected_id,
                    expected_category: expected_category.as_str(),
                    actual_id: owner_id,
                    actual_category: actual_category.as_str(),
                });
            }
        }
        tx.prepare_cached("DELETE FROM time_series_associations WHERE id = ?1")?
            .execute(params![id])?;
        let hash = bytes_to_hash32(&hash_bytes).ok_or_else(|| {
            TimeSeriesError::IntegrityError(format!(
                "malformed catalog row: data_hash is {} bytes, expected 32",
                hash_bytes.len()
            ))
        })?;
        let features_hash = bytes_to_hash32(&features_hash_bytes).ok_or_else(|| {
            TimeSeriesError::IntegrityError(format!(
                "malformed catalog row: features_hash is {} bytes, expected 32",
                features_hash_bytes.len()
            ))
        })?;
        Ok(Some(DeletedRow {
            data_hash: hash,
            time_series_type: decode_type(type_code)?,
            owner_id,
            owner_category,
            name,
            resolution: resolution_iso.as_deref().map(iso_to_period).transpose()?,
            features_hash,
        }))
    }

    /// Delete all associations for the owner `(owner_id, owner_category)`.
    /// Returns the data_hashes of removed rows.
    pub fn delete_by_owner(
        tx: &Connection,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> Result<Vec<[u8; 32]>> {
        let bytes_list: Vec<Vec<u8>> = collect_data_hashes(
            tx,
            "SELECT data_hash FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2",
            params![owner_id, owner_category.code()],
        )?;
        let hashes = bytes_list
            .into_iter()
            .filter_map(|bytes| bytes_to_hash32(&bytes))
            .collect::<Vec<_>>();
        tx.execute(
            "DELETE FROM time_series_associations WHERE owner_id = ?1 AND owner_category = ?2",
            params![owner_id, owner_category.code()],
        )?;
        Ok(hashes)
    }

    /// Reassign every association from `old_owner` to `new_owner` within the
    /// given `owner_category`. Only the owning id changes; type/category and the
    /// underlying arrays are untouched (arrays are content-addressed). Returns
    /// the rows updated.
    pub fn replace_owner(
        tx: &Connection,
        old_owner: i64,
        new_owner: i64,
        owner_category: OwnerCategory,
    ) -> Result<usize> {
        // A collision (the new owner already holds an identical association)
        // fires the unique index on the UPDATE; surface the spec error rather
        // than a raw rusqlite error (REVIEW_FOLLOWUPS.md item 5).
        tx.execute(
            "UPDATE time_series_associations SET owner_id = ?1
             WHERE owner_id = ?2 AND owner_category = ?3",
            params![new_owner, old_owner, owner_category.code()],
        )
        .map_err(map_unique_violation)
    }

    /// Rename one association identified by `key` to `new_name`, leaving its data
    /// and hash untouched. Returns the number of rows updated (0 if `key` matches
    /// nothing). A collision with an existing series of the new identity maps to
    /// [`TimeSeriesError::DuplicateTimeSeries`].
    /// Rename the association filed under `id`. One row by primary key, so no
    /// predicate can be wider than the caller asked for.
    /// Move one row to `new_name`, by primary key.
    ///
    /// The destination name may already be taken by a sibling sharing the rest
    /// of the identity, and the uniqueness index catches that. It has to be
    /// reported as [`TimeSeriesError::DuplicateTimeSeries`], the same as the
    /// insert path does: a raw `SqliteFailure` naming an index is a caller's
    /// problem stated in the catalog's vocabulary, and nothing above this can
    /// classify it.
    pub fn rename_by_id(tx: &Connection, id: i64, new_name: &str) -> Result<usize> {
        match tx
            .prepare_cached("UPDATE time_series_associations SET name = ?2 WHERE id = ?1")?
            .execute(rusqlite::params![id, new_name])
        {
            Ok(n) => Ok(n),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Err(TimeSeriesError::DuplicateTimeSeries)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Delete every association in the store. Returns the removed data_hashes.
    pub fn delete_all(tx: &Connection) -> Result<Vec<[u8; 32]>> {
        let bytes_list: Vec<Vec<u8>> = collect_data_hashes(
            tx,
            "SELECT data_hash FROM time_series_associations",
            params![],
        )?;
        let hashes = bytes_list
            .into_iter()
            .filter_map(|bytes| bytes_to_hash32(&bytes))
            .collect::<Vec<_>>();
        tx.execute("DELETE FROM time_series_associations", [])?;
        // Clearing the store empties it, so every feature set is unreachable by
        // construction. Drop them here rather than leaving the whole catalog's
        // worth of orphans for a compaction that a cleared store may never get.
        // The timestamp vectors are unreachable on the same terms, but they live
        // in the array file, which this connection has no handle on:
        // `Store::clear_time_series` sweeps them once this commits.
        tx.execute("DELETE FROM feature_sets", [])?;
        Ok(hashes)
    }

    pub fn list(
        &self,
        filter: &MetadataFilter,
        vectors: &dyn StorageBackend,
    ) -> Result<Vec<TimeSeriesMetadata>> {
        Ok(self
            .list_inner(filter, Some(vectors))?
            .0
            .into_iter()
            .map(|(_, m)| m)
            .collect())
    }

    /// Like [`Self::list`], but without hydrating the timestamp vectors.
    ///
    /// For callers that only need each row's *identity* — building a
    /// an identity, which never carries the vector. An irregular series
    /// comes back with `timestamps: None` and its axis unread, which is what
    /// keeps a key listing from fetching every axis the match spans out of the
    /// array file only to discard it.
    ///
    /// Not for a caller that will write the row back: [`Self::insert`] derives
    /// `timestamps_hash` from `timestamps`, so re-inserting an unhydrated row
    /// would drop its time axis.
    pub fn list_without_timestamps(
        &self,
        filter: &MetadataFilter,
    ) -> Result<Vec<TimeSeriesMetadata>> {
        Ok(self
            .list_inner(filter, None)?
            .0
            .into_iter()
            .map(|(_, m)| m)
            .collect())
    }

    /// [`Self::list`] without hydrating timestamp vectors, paired with the
    /// distinct vectors the matched rows reference.
    ///
    /// For the reader build path, which needs every row's identity and array but
    /// not a per-row copy of the shared time axis. Hydrating would clone one
    /// vector per row — a cohort of 50k irregular series on a year of hourly
    /// timestamps is gigabytes of identical data — while the reader wants the
    /// axis exactly once. The returned hashes name the cohorts *after* the
    /// in-memory features filter has run, so a caller can insist on one.
    pub fn list_timeline_cohorts(
        &self,
        filter: &MetadataFilter,
    ) -> Result<(Vec<TimeSeriesMetadata>, Vec<[u8; 32]>)> {
        let (rows, cohorts) = self.list_inner(filter, None)?;
        Ok((rows.into_iter().map(|(_, m)| m).collect(), cohorts))
    }

    /// Shared body of [`Self::list`] and [`Self::list_timeline_cohorts`]. Each
    /// row carries its catalog `id` alongside the metadata; the wrappers that
    /// don't need it drop it.
    fn list_inner(
        &self,
        filter: &MetadataFilter,
        vectors: Option<&dyn StorageBackend>,
    ) -> Result<(Vec<IdentifiedRow>, Vec<[u8; 32]>)> {
        let (where_clause, params_vec) = filter.to_sql();
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let sql = format!(
            "SELECT features_hash, owner_id, owner_type, owner_category, time_series_type, name,
                    data_hash, initial_timestamp, resolution, length, horizon,
                    interval, count, timestamps_hash, units, quantity_kind, unit_system,
                    time_reference, component_field, percentiles_json, element_type,
                    element_shape, application_data, id
             FROM time_series_associations {where_clause}"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows: Vec<([u8; 32], MetaRow)> = stmt
            .query_map(param_refs.as_slice(), parse_meta_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Hydrate timestamp vectors the same way features are hydrated below,
        // and for the same reason: they are content-addressed, so each DISTINCT
        // vector is fetched once no matter how many matched rows share it.
        // Skipped outright unless some row actually carries one — only
        // `NonSequentialTimeSeries` does, so a store without them never asks the
        // backend at all — and skipped entirely for a caller that only wants to
        // know *which* vectors are referenced (`vectors` is then `None`).
        //
        // Unlike the feature sets below, these do not live in the catalog: they
        // are data, stored beside the arrays (see `crate::timestamps`), so the
        // fetch is a read of the array file rather than a second SQL statement.
        let mut timestamps_by_hash: HashMap<[u8; 32], Vec<DateTime<Utc>>> = HashMap::new();
        if let Some(vectors) = vectors {
            // Serve what the memo already holds and go to the file only for the
            // rest. A keyed read resolves one row at a time, so this is what
            // stops a sweep over a cohort from re-reading its shared axis once
            // per key.
            for (_, row) in &rows {
                let Some(hash) = row.timestamps_hash else {
                    continue;
                };
                if timestamps_by_hash.contains_key(&hash) {
                    continue;
                }
                let timestamps = match self.cached_timestamps(&hash) {
                    Some(timestamps) => timestamps,
                    // A row naming a vector the store does not hold is a
                    // damaged artifact, not an empty timeline. Left out of the
                    // map so it is reported below against the row that named
                    // it, rather than as the bare `NotFound` the backend
                    // raises; any *other* failure already says what went wrong
                    // and is passed through.
                    None => match vectors.get_timestamps(&hash) {
                        Ok(timestamps) => {
                            self.cache_timestamps(hash, &timestamps);
                            timestamps
                        }
                        Err(TimeSeriesError::NotFound) => continue,
                        Err(e) => return Err(e),
                    },
                };
                timestamps_by_hash.insert(hash, timestamps);
            }
        }

        // Hydrate features in one query rather than one per row. Because feature
        // sets are content-addressed, this fetches each DISTINCT set once, no
        // matter how many matched rows share it — listing 50k series that all
        // carry the same two features reads two rows here, not 100k.
        //
        // Re-running the row predicate as a subquery (rather than binding an
        // `IN (...)` list of hashes) keeps this to two statements regardless of
        // match count, and sidesteps SQLite's bound-parameter ceiling on a large
        // store. Rows whose feature set is empty simply get no group.
        let feat_sql = format!(
            "SELECT fs.features_hash, fs.key, fs.value_kind, fs.value_int, fs.value_float,
                    fs.value_bool, fs.value_str
             FROM feature_sets fs
             WHERE fs.features_hash IN
                   (SELECT features_hash FROM time_series_associations {where_clause})"
        );
        let mut feat_stmt = self.conn.prepare_cached(&feat_sql)?;
        let mut by_hash: HashMap<[u8; 32], Features> = HashMap::new();
        let mut feat_rows = feat_stmt.query(param_refs.as_slice())?;
        while let Some(row) = feat_rows.next()? {
            let hash = bytes_to_hash32(&row.get::<_, Vec<u8>>(0)?).ok_or_else(|| {
                TimeSeriesError::IntegrityError("features_hash is not 32 bytes".into())
            })?;
            let (key, value) = parse_feature_row(row)?;
            // `Features` is a BTreeMap, so it orders keys itself; the query does
            // not need an ORDER BY.
            by_hash.entry(hash).or_default().insert(key, value);
        }

        let mut out = Vec::with_capacity(rows.len());
        // Distinct timestamp vectors among the *surviving* rows, in first-seen
        // order. Collected here rather than by a second query so the in-memory
        // features filter above is accounted for. The set is what tests
        // membership — a store whose irregular series each have their own axis
        // would make a linear scan of `cohorts` quadratic in the listing size.
        let mut cohorts: Vec<[u8; 32]> = Vec::new();
        let mut seen_cohorts: HashSet<[u8; 32]> = HashSet::new();
        for (f_hash, partial) in rows {
            // Cloned, not removed: many rows legitimately share one set.
            let features = by_hash.get(&f_hash).cloned().unwrap_or_default();
            // Optional features-subset filter, in-memory.
            if let Some(ref required) = filter.features
                && !is_subset(required, &features)
            {
                continue;
            }
            let timestamps = match partial.timestamps_hash {
                None => None,
                Some(hash) => {
                    if seen_cohorts.insert(hash) {
                        cohorts.push(hash);
                    }
                    if vectors.is_none() {
                        None
                    } else {
                        // The series cannot be read at all without its
                        // timestamps, so say so here rather than handing back a
                        // row that fails obscurely later.
                        Some(timestamps_by_hash.get(&hash).cloned().ok_or_else(|| {
                            TimeSeriesError::IntegrityError(format!(
                                "association '{}' (owner {}) references timestamp vector {}, \
                                 which the store does not hold",
                                partial.name,
                                partial.owner_id,
                                crate::hash::hash_hex(&hash)
                            ))
                        })?)
                    }
                }
            };
            let id = partial.id;
            out.push((id, partial.into_metadata(features, timestamps)));
        }
        Ok((out, cohorts))
    }

    /// The timestamp vector stored under `hash`, memoized.
    ///
    /// [`TimeSeriesError::NotFound`] if the store holds no such vector, which
    /// for a hash read off an association row means the two halves of the
    /// artifact are out of step.
    pub fn timestamps_for_hash(
        &self,
        hash: &[u8; 32],
        vectors: &dyn StorageBackend,
    ) -> Result<Vec<DateTime<Utc>>> {
        if let Some(timestamps) = self.cached_timestamps(hash) {
            return Ok(timestamps);
        }
        let timestamps = vectors.get_timestamps(hash)?;
        self.cache_timestamps(*hash, &timestamps);
        Ok(timestamps)
    }

    /// The memoized vector for `hash`, if it was decoded recently enough to
    /// still be held. See [`TimestampCache`].
    fn cached_timestamps(&self, hash: &[u8; 32]) -> Option<Vec<DateTime<Utc>>> {
        self.timestamps_cache.borrow_mut().get(hash)
    }

    fn cache_timestamps(&self, hash: [u8; 32], timestamps: &[DateTime<Utc>]) {
        self.timestamps_cache.borrow_mut().insert(hash, timestamps);
    }

    /// The identity of every association of `ts_type` — paired with its stored
    /// `horizon` (`None` for non-forecast rows) — read straight from the
    /// associations table with no feature hydration.
    ///
    /// `features_hash` is a stored column, so a caller that only needs to test
    /// identity ("does this series already exist?") can skip both the features
    /// join and the SHA-256 recomputation that [`Self::list`] would do. The
    /// horizon rides along because it is *not* part of the identity: a caller
    /// deciding whether an existing row satisfies a request (e.g. the DST
    /// transform's idempotency check) must compare it separately.
    pub fn list_identities(
        &self,
        ts_type: TimeSeriesType,
    ) -> Result<Vec<(AssociationIdentity, Option<Period>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT owner_id, owner_category, name, resolution, interval, features_hash, horizon
             FROM time_series_associations WHERE time_series_type = ?1",
        )?;
        let rows = stmt
            .query_map(params![ts_type.code()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(owner_id, cat, name, resolution, interval, hash_blob, horizon)| {
                    let identity = AssociationIdentity {
                        owner_id,
                        owner_category: decode_category(cat)?,
                        name,
                        resolution,
                        interval,
                        features_hash: hash_blob.as_slice().try_into().map_err(|_| {
                            TimeSeriesError::IntegrityError("features_hash is not 32 bytes".into())
                        })?,
                    };
                    let horizon = horizon.map(|s| iso_to_period(&s)).transpose()?;
                    Ok((identity, horizon))
                },
            )
            .collect()
    }

    /// True iff at least one association matches `filter` — the existence
    /// probe behind [`crate::Store::has_time_series`] and
    /// [`crate::Store::has_any_time_series`], both of which consumers call in
    /// hot per-component loops.
    ///
    /// `SELECT 1 ... LIMIT 1` over the same predicate [`Self::list`] uses, so
    /// the answer comes straight off an index: a full key identity is a
    /// covering seek of `uq_ts_assoc`, and an owner-only probe covers via
    /// `idx_category_owner`. Unlike `list`, no row leaves the index — nothing
    /// is hydrated, no JSON is parsed, and no second features query runs.
    ///
    /// A `features` filter keeps that guarantee through a two-step strategy
    /// (ported from InfrastructureSystems.jl's optimized `has_metadata`): the
    /// requested set is hashed and probed as an *exact* set first — callers
    /// overwhelmingly pass the complete feature set, and that equality rides
    /// `uq_ts_assoc` like any other keyed probe. Only when the exact probe
    /// misses (a genuinely partial feature list, or a true miss) does the
    /// indexed subset fallback [`Self::exists_feature_subset`] run.
    pub fn exists(&self, filter: &MetadataFilter) -> Result<bool> {
        if let Some(required) = filter.features.as_ref().filter(|f| !f.is_empty()) {
            // The exact-set shortcut substitutes its own hash, so it must not
            // override a caller that already pinned one.
            if filter.features_hash.is_none() {
                let mut exact = filter.clone();
                exact.features = None;
                exact.features_hash = Some(features_hash(required));
                if self.exists(&exact)? {
                    return Ok(true);
                }
            }
            return self.exists_feature_subset(filter, required);
        }
        let (where_clause, params_vec) = filter.to_sql();
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let sql = format!("SELECT 1 FROM time_series_associations {where_clause} LIMIT 1");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let found: Option<i64> = stmt
            .query_row(param_refs.as_slice(), |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Subset-match existence probe answered entirely in SQL: one correlated
    /// `EXISTS` seek of `feature_sets`' `(features_hash, key)` primary key per
    /// requested feature — nothing hydrated, no JSON parsed. The value
    /// comparison is kind-strict, matching [`is_subset`]'s `FeatureValue`
    /// equality (an `Int(2030)` never matches a `Str("2030")`).
    ///
    /// The SQL text depends only on the shape (feature count and value kinds
    /// in key order), so `prepare_cached` reuses statements across calls the
    /// same way the plain probes do.
    fn exists_feature_subset(&self, filter: &MetadataFilter, required: &Features) -> Result<bool> {
        let (where_clause, mut params_vec) = filter.to_sql();
        let mut sql = format!("SELECT 1 FROM time_series_associations {where_clause}");
        for (key, value) in required {
            let (kind, column, param): (&str, &str, Box<dyn rusqlite::ToSql>) = match value {
                FeatureValue::Int(i) => ("int", "value_int", Box::new(*i)),
                FeatureValue::Float(f) => ("float", "value_float", Box::new(*f)),
                FeatureValue::Bool(b) => ("bool", "value_bool", Box::new(*b as i64)),
                FeatureValue::Str(s) => ("str", "value_str", Box::new(s.clone())),
            };
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM feature_sets fs \
                 WHERE fs.features_hash = time_series_associations.features_hash \
                 AND fs.key = ? AND fs.value_kind = '{kind}' AND fs.{column} = ?)"
            ));
            params_vec.push(Box::new(key.clone()));
            params_vec.push(param);
        }
        sql.push_str(" LIMIT 1");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let found: Option<i64> = stmt
            .query_row(param_refs.as_slice(), |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// The row filed under `id`, or `None` if the catalog holds no such row.
    ///
    /// `None` rather than [`TimeSeriesError::NotFound`] because a caller
    /// validating references it stored earlier is *asking* whether one still
    /// resolves; a dangling reference is the answer, not an error.
    pub fn get_by_id(
        &self,
        id: i64,
        vectors: &dyn StorageBackend,
    ) -> Result<Option<TimeSeriesMetadata>> {
        let mut matches = self.list(
            &MetadataFilter {
                ids: Some(vec![id]),
                ..Default::default()
            },
            vectors,
        )?;
        Ok(matches.pop())
    }

    /// Every row named by `ids`, in catalog order and without duplicates —
    /// callers that need them in *their* order reorder by [`TimeSeriesMetadata::id`].
    ///
    /// One query rather than one per id, which is what makes a bulk read by
    /// reference cost the same as a bulk read by key.
    pub fn list_by_ids(
        &self,
        ids: &[i64],
        vectors: &dyn StorageBackend,
    ) -> Result<Vec<TimeSeriesMetadata>> {
        self.list_by_ids_with(ids, |f| self.list(f, vectors))
    }

    /// [`Self::list_by_ids`] without loading each irregular row's timestamp
    /// vector — the identity-question form, for a caller that wants the rows
    /// rather than the series.
    pub fn list_by_ids_without_timestamps(&self, ids: &[i64]) -> Result<Vec<TimeSeriesMetadata>> {
        self.list_by_ids_with(ids, |f| self.list_without_timestamps(f))
    }

    /// The chunking shared by both by-id listings.
    fn list_by_ids_with(
        &self,
        ids: &[i64],
        mut list: impl FnMut(&MetadataFilter) -> Result<Vec<TimeSeriesMetadata>>,
    ) -> Result<Vec<TimeSeriesMetadata>> {
        // Each id is one bound `?`, and `list_inner` binds the predicate more
        // than once per statement, so a model-sized set (tens of thousands of
        // references) would trip SQLite's variable limit — and every distinct
        // set size would be a distinct statement in the prepare cache. Sorted
        // and deduplicated first, so the chunks concatenate in id order (which
        // is catalog order) and no row appears twice.
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut rows = Vec::with_capacity(sorted.len());
        for chunk in sorted.chunks(IDS_PER_QUERY) {
            rows.extend(list(&MetadataFilter {
                ids: Some(chunk.to_vec()),
                ..Default::default()
            })?);
        }
        Ok(rows)
    }

    /// Whether a row is filed under `id`.
    ///
    /// A primary-key probe: one statement, no row fetched, no metadata
    /// hydrated. Cheap enough for a consumer to validate every reference in its
    /// model on load.
    pub fn exists_by_id(&self, id: i64) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .prepare_cached("SELECT 1 FROM time_series_associations WHERE id = ?1 LIMIT 1")?
            .query_row([id], |row| row.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    pub fn distinct_resolutions(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        let mut sql = String::from(
            "SELECT DISTINCT resolution FROM time_series_associations
             WHERE resolution IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            // Standalone predicate on `idx_ts_type` (no composite seek to
            // truncate), so the cheaper range form applies — see `SpanForm`.
            push_type_predicate(
                &mut sql,
                &mut params_vec,
                TypeMatch::Requested(t),
                SpanForm::Range,
            );
        }
        sql.push_str(" ORDER BY resolution ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|s| iso_to_period(&s)).collect()
    }

    /// Distinct forecast `interval`s, optionally scoped to one time series type.
    /// Ordered by the ISO-8601 text (lexical, like [`Self::distinct_resolutions`]):
    /// mixed period kinds have no numeric order, so text order is the stable
    /// choice. Only forecast rows carry an interval, so non-forecast types yield
    /// an empty list.
    pub fn distinct_intervals(&self, ts_type: Option<TimeSeriesType>) -> Result<Vec<Period>> {
        let mut sql = String::from(
            "SELECT DISTINCT interval FROM time_series_associations
             WHERE interval IS NOT NULL",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(t) = ts_type {
            // Standalone predicate on `idx_ts_type` (no composite seek to
            // truncate), so the cheaper range form applies — see `SpanForm`.
            push_type_predicate(
                &mut sql,
                &mut params_vec,
                TypeMatch::Requested(t),
                SpanForm::Range,
            );
        }
        sql.push_str(" ORDER BY interval ASC");
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(|s| iso_to_period(&s)).collect()
    }

    /// Every distinct timestamp spelling the catalog holds, sorted, plus whether
    /// any row left the column NULL.
    ///
    /// One `SELECT DISTINCT` rather than a projection over a full listing: this
    /// serves `store-info`, which is otherwise a constant-time report, and the
    /// column is low-cardinality by nature — a handful of values across a whole
    /// store. It is also the only surface that needs the *unspecified* rows
    /// counted separately, which is why the flag rides along instead of being a
    /// second query.
    pub fn distinct_time_references(&self) -> Result<(Vec<TimeReference>, bool)> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT time_reference FROM time_series_associations ORDER BY 1")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Option<String>>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut unspecified = false;
        let mut out = Vec::new();
        for row in rows {
            match row {
                None => unspecified = true,
                // An unparseable spelling is an integrity failure, on the same
                // terms as the read path: "unspecified" and "a value this build
                // cannot read" must not look alike.
                Some(s) => out.push(TimeReference::parse(&s)?),
            }
        }
        Ok((out, unspecified))
    }

    pub fn count_by_type(&self, ts_type: TimeSeriesType) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM time_series_associations WHERE time_series_type = ?1",
            params![ts_type.code()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Count `SingleTimeSeries` and `DeterministicSingleTimeSeries` associations
    /// referencing `data_hash`, returned as `(sts, dst)`. Other types referencing
    /// the same array (if any) are ignored. One grouped query, no feature fetch.
    pub fn count_array_references(&self, data_hash: &[u8; 32]) -> Result<(i64, i64)> {
        let mut stmt = self.conn.prepare(
            "SELECT time_series_type, COUNT(*) FROM time_series_associations
             WHERE data_hash = ?1 GROUP BY time_series_type",
        )?;
        let rows = stmt.query_map(params![data_hash.as_slice()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut sts = 0i64;
        let mut dst = 0i64;
        for row in rows {
            let (ts_type, n) = row?;
            match TimeSeriesType::from_code(ts_type) {
                Some(TimeSeriesType::SingleTimeSeries) => sts = n,
                Some(TimeSeriesType::DeterministicSingleTimeSeries) => dst = n,
                _ => {}
            }
        }
        Ok((sts, dst))
    }

    /// The `element_type` of the array content-addressed by `data_hash`.
    ///
    /// The catalog is the authority on element typing: the HDF5 file records
    /// only how wide an element is, not what it means, and `bool` and `u8` are
    /// not even distinguishable there. Every read therefore resolves the type
    /// here first. `NotFound` if no association references the hash — an array
    /// nothing points at cannot be typed, and cannot be read.
    pub fn element_type_for_hash(&self, data_hash: &[u8; 32]) -> Result<ElementType> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT element_type FROM time_series_associations WHERE data_hash = ?1 LIMIT 1",
        )?;
        let spelling: String = stmt
            .query_row(params![data_hash.as_slice()], |r| r.get(0))
            .optional()?
            .ok_or(TimeSeriesError::NotFound)?;
        ElementType::parse(&spelling).ok_or_else(|| {
            TimeSeriesError::IntegrityError(format!(
                "catalog holds an invalid element_type {spelling:?} for array {}",
                crate::hash::hash_hex(data_hash)
            ))
        })
    }

    /// Every distinct `(data_hash, element_type)` the catalog references, for
    /// the integrity sweep, plus one diagnostic per row too malformed to use.
    ///
    /// Arrays in the file that no association references are absent: they are
    /// unreachable, so nothing can read them and nothing records what their
    /// bytes mean.
    ///
    /// A malformed row is reported rather than returned as an error, because
    /// aborting here would stop the sweep at the first bad row and hide every
    /// other problem in the store — the opposite of what an integrity check is
    /// for.
    pub fn referenced_arrays(&self) -> Result<ReferencedArrays> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT data_hash, element_type FROM time_series_associations")?;
        // `data_hash` is read as a dynamic value, not a `Vec<u8>`: SQLite is
        // dynamically typed, so a corrupted row can hold TEXT (or anything else)
        // in a BLOB column, and a typed getter would fail the whole query
        // instead of letting the sweep report that one row and carry on.
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, rusqlite::types::Value>(0)?,
                r.get::<_, String>(1)?,
            ))
        })?;
        let mut out = Vec::new();
        let mut problems = Vec::new();
        for row in rows {
            let (hash, spelling) = row?;
            let hash = match hash {
                rusqlite::types::Value::Blob(bytes) => bytes,
                other => {
                    problems.push(format!(
                        "malformed catalog row: data_hash is {}, expected a 32-byte blob",
                        value_kind(&other)
                    ));
                    continue;
                }
            };
            let Ok(hash) = <[u8; 32]>::try_from(hash.as_slice()) else {
                problems.push(format!(
                    "malformed catalog row: data_hash is {} bytes, expected 32",
                    hash.len()
                ));
                continue;
            };
            match ElementType::parse(&spelling) {
                Some(element_type) => out.push((hash, element_type)),
                None => problems.push(format!(
                    "malformed catalog row: array {} has an invalid element_type {spelling:?}",
                    crate::hash::hash_hex(&hash)
                )),
            }
        }
        Ok((out, problems))
    }

    pub fn count_distinct_owners(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM
             (SELECT DISTINCT owner_id, owner_category FROM time_series_associations)",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Association count grouped by time series type, as `(type, count)` pairs,
    /// ordered by storage code (so `SingleTimeSeries` first, `Scenarios` last).
    /// One grouped query; types the core does not recognize are skipped.
    ///
    /// The `ORDER BY` is explicit rather than incidental: `GROUP BY` alone
    /// returns rows in whatever order the grouping used, which changed when the
    /// column became an integer code. Pinning it keeps the output stable for
    /// callers that compare whole result lists.
    pub fn counts_by_type(&self) -> Result<Vec<(TimeSeriesType, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT time_series_type, COUNT(*) FROM time_series_associations
             GROUP BY time_series_type ORDER BY time_series_type",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (ts_type, n) = row?;
            if let Some(ty) = TimeSeriesType::from_code(ts_type) {
                out.push((ty, n));
            }
        }
        Ok(out)
    }

    /// Number of distinct stored arrays (content hashes) referenced by any
    /// association. Series that share an array (de-duplicated by content) count
    /// once. One `COUNT(DISTINCT)` query.
    pub fn count_distinct_arrays(&self) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT data_hash) FROM time_series_associations",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Number of distinct owner ids in `category` that have any association.
    pub fn count_distinct_owners_in_category(&self, category: OwnerCategory) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT owner_id) FROM time_series_associations
             WHERE owner_category = ?1",
            params![category.code()],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Number of distinct stored arrays referenced by associations of any of
    /// `types`. Empty `types` yields 0.
    pub fn count_distinct_arrays_for_types(&self, types: &[TimeSeriesType]) -> Result<i64> {
        if types.is_empty() {
            return Ok(0);
        }
        let placeholders = vec!["?"; types.len()].join(",");
        let sql = format!(
            "SELECT COUNT(DISTINCT data_hash) FROM time_series_associations
             WHERE time_series_type IN ({placeholders})"
        );
        let codes: Vec<i64> = types.iter().map(|t| t.code()).collect();
        let n: i64 = self
            .conn
            .query_row(&sql, rusqlite::params_from_iter(codes), |r| r.get(0))?;
        Ok(n)
    }

    /// Grouped summary of the static series (SingleTimeSeries +
    /// NonSequentialTimeSeries): one row per distinct
    /// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
    /// length)` with the association count. One `GROUP BY` query.
    pub fn static_summary(&self) -> Result<Vec<StaticSummaryRow>> {
        // The static types are a contiguous code block, so this is one range
        // scan rather than a name list — see `TimeSeriesType::code_groups`.
        let (static_lo, static_hi, ..) = TimeSeriesType::code_groups();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT owner_type, owner_category, time_series_type, name,
                    initial_timestamp, resolution, length, COUNT(*)
             FROM time_series_associations
             WHERE time_series_type BETWEEN {static_lo} AND {static_hi}
             GROUP BY owner_type, owner_category, time_series_type, name,
                      initial_timestamp, resolution, length",
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (owner_type, oc, tt, name, its, res, len, count) = row?;
            out.push(StaticSummaryRow {
                owner_type,
                owner_category: decode_category(oc)?,
                time_series_type: decode_type(tt)?,
                name,
                initial_timestamp: parse_opt_rfc3339(its)?,
                resolution: res.map(|s| iso_to_period(&s)).transpose()?,
                time_step_count: len,
                count,
            });
        }
        Ok(out)
    }

    /// Grouped summary of forecasts: one row per distinct
    /// `(owner_type, owner_category, type, name, initial_timestamp, resolution,
    /// horizon, interval, window_count)` with the association count. One
    /// `GROUP BY` query.
    pub fn forecast_summary(&self) -> Result<Vec<ForecastSummaryRow>> {
        // The forecast types are the other contiguous code block.
        let (.., fc_lo, fc_hi) = TimeSeriesType::code_groups();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT owner_type, owner_category, time_series_type, name,
                    initial_timestamp, resolution, horizon, interval, count, COUNT(*)
             FROM time_series_associations
             WHERE time_series_type BETWEEN {fc_lo} AND {fc_hi}
             GROUP BY owner_type, owner_category, time_series_type, name,
                      initial_timestamp, resolution, horizon, interval, count",
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, i64>(9)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (owner_type, oc, tt, name, its, res, hor, iv, wcount, count) = row?;
            out.push(ForecastSummaryRow {
                owner_type,
                owner_category: decode_category(oc)?,
                time_series_type: decode_type(tt)?,
                name,
                initial_timestamp: parse_opt_rfc3339(its)?,
                resolution: res.map(|s| iso_to_period(&s)).transpose()?,
                horizon: hor.map(|s| iso_to_period(&s)).transpose()?,
                interval: iv.map(|s| iso_to_period(&s)).transpose()?,
                window_count: wcount,
                count,
            });
        }
        Ok(out)
    }

    /// Distinct `(resolution, initial_timestamp, length)` triples across the
    /// `SingleTimeSeries` associations, ordered by resolution (ISO-8601 text
    /// order, so equal resolutions are adjacent). Used to verify that each
    /// resolution's series share a single static grid; `resolution` optionally
    /// restricts the scan to one resolution. One `DISTINCT` query.
    pub fn distinct_single_grids(
        &self,
        resolution: Option<Period>,
        owner_category: Option<OwnerCategory>,
    ) -> Result<Vec<(Period, DateTime<Utc>, i64)>> {
        let res_iso = resolution.map(period_to_iso);
        let category = owner_category.map(|c| c.code());
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT resolution, initial_timestamp, length
             FROM time_series_associations
             WHERE time_series_type = ?1 AND (?2 IS NULL OR resolution = ?2)
               AND (?3 IS NULL OR owner_category = ?3)
             ORDER BY resolution",
        )?;
        let rows = stmt.query_map(
            params![TimeSeriesType::SingleTimeSeries.code(), res_iso, category],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (res_str, ts_str, len) = row?;
            let res = iso_to_period(&res_str)?;
            let ts = DateTime::parse_from_rfc3339(&ts_str)
                .map_err(|e| {
                    TimeSeriesError::IntegrityError(format!("bad initial_timestamp: {e}"))
                })?
                .with_timezone(&Utc);
            out.push((res, ts, len));
        }
        Ok(out)
    }

    /// Distinct owner ids in `category` that have an association, optionally
    /// restricted to one time series type and/or resolution.
    pub fn list_owner_ids(
        &self,
        category: OwnerCategory,
        ts_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> Result<Vec<i64>> {
        let mut sql = String::from(
            "SELECT DISTINCT owner_id FROM time_series_associations
             WHERE owner_category = ?",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(category.code())];
        if let Some(t) = ts_type {
            // Standalone predicate on `idx_ts_type` (no composite seek to
            // truncate), so the cheaper range form applies — see `SpanForm`.
            push_type_predicate(
                &mut sql,
                &mut params_vec,
                TypeMatch::Requested(t),
                SpanForm::Range,
            );
        }
        if let Some(res) = resolution {
            sql.push_str(" AND resolution = ?");
            params_vec.push(Box::new(period_to_iso(res)));
        }
        let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |r| r.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ---- Association catalogs ---------------------------------------------
    //
    // The `assoc_*` helpers are the shared engine: one implementation per query
    // shape, parameterized by an [`AssocTable`]. The typed wrappers below them
    // are what `Store` calls.
    //
    // Every read short-circuits when its table is absent, which happens only for
    // a read-only open of a store last written before these tables existed.
    // Returning the empty answer keeps that store readable rather than turning a
    // purely additive schema change into a hard failure; see the DDL comment in
    // `schema.rs`. Writes need no such guard: a writable open always ran the DDL.

    fn assoc_present(&self, table: AssocTable) -> bool {
        match table.name {
            "supplemental_attribute_associations" => self.has_supplemental_attribute_table,
            _ => self.has_parent_child_table,
        }
    }

    /// Every matching row's id and both endpoint pairs, in insertion order.
    fn assoc_list(&self, table: AssocTable, filter: &EndpointFilter) -> Result<Vec<AssocRow>> {
        if !self.assoc_present(table) {
            return Ok(Vec::new());
        }
        let (where_clause, params) = filter.to_sql(table);
        let param_refs = to_param_refs(&params);
        // Ordered by rowid so a bulk export/import round trip preserves the
        // order the caller inserted in.
        let sql = format!(
            "SELECT id, {}, {}, {}, {} FROM {} {where_clause} ORDER BY id",
            table.left_id, table.left_type, table.right_id, table.right_type, table.name
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn assoc_has(&self, table: AssocTable, filter: &EndpointFilter) -> Result<bool> {
        if !self.assoc_present(table) {
            return Ok(false);
        }
        let (where_clause, params) = filter.to_sql(table);
        let param_refs = to_param_refs(&params);
        let sql = format!("SELECT 1 FROM {} {where_clause} LIMIT 1", table.name);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let found: Option<i64> = stmt
            .query_row(param_refs.as_slice(), |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Distinct ids at `endpoint` among the matching rows, ascending.
    fn assoc_ids(
        &self,
        table: AssocTable,
        filter: &EndpointFilter,
        endpoint: Endpoint,
    ) -> Result<Vec<i64>> {
        if !self.assoc_present(table) {
            return Ok(Vec::new());
        }
        let (where_clause, params) = filter.to_sql(table);
        let param_refs = to_param_refs(&params);
        let id_col = table.id_column(endpoint);
        let sql = format!(
            "SELECT DISTINCT {id_col} FROM {} {where_clause} ORDER BY {id_col}",
            table.name
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |r| r.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Shared body of the counting queries: `projection` is a literal aggregate
    /// built from a fixed column name, never caller text.
    fn assoc_count(
        &self,
        table: AssocTable,
        filter: &EndpointFilter,
        projection: &str,
    ) -> Result<i64> {
        if !self.assoc_present(table) {
            return Ok(0);
        }
        let (where_clause, params) = filter.to_sql(table);
        let param_refs = to_param_refs(&params);
        let sql = format!("SELECT {projection} FROM {} {where_clause}", table.name);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        Ok(stmt.query_row(param_refs.as_slice(), |r| r.get(0))?)
    }

    /// Row counts grouped by the type label at `endpoint`, ordered by type.
    fn assoc_counts_by_type(
        &self,
        table: AssocTable,
        endpoint: Endpoint,
    ) -> Result<Vec<(String, i64)>> {
        if !self.assoc_present(table) {
            return Ok(Vec::new());
        }
        let type_col = table.type_column(endpoint);
        let sql = format!(
            "SELECT {type_col}, COUNT(*) FROM {} GROUP BY {type_col} ORDER BY {type_col}",
            table.name
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Refuse an explicit id that `time_series_associations` could ever have
    /// issued.
    ///
    /// Only one caller supplies ids —
    /// [`Store::import_association_rows`](crate::Store::import_association_rows),
    /// replaying a document that recorded them. Every ordinary add lets the
    /// catalog assign, so this is the whole of the explicit-id surface.
    ///
    /// "Never reissued" is a promise about *assigned* ids: `AUTOINCREMENT` only
    /// ratchets `sqlite_sequence` upward, so a deleted row's id is never handed
    /// out again. An explicit id is not covered by that mechanism — the primary
    /// key only refuses an id a *live* row holds, so an import could re-file a
    /// deleted id and a stale reference in some consumer's model would quietly
    /// resolve to the new series. This closes that hole: every explicit id must
    /// sit above the table's high-water mark, which is also what
    /// [`TimeSeriesError::DuplicateAssociationId`] already documents.
    ///
    /// Checked once per batch against the mark as it stands *before* the batch
    /// writes, so a document's rows may arrive in any order; a duplicate within
    /// the batch still lands on the primary key. Zero and negative ids are
    /// refused up front: no catalog ever issues one.
    pub fn check_explicit_time_series_ids(tx: &Connection, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if let Some(bad) = ids.iter().find(|&&id| id <= 0) {
            return Err(TimeSeriesError::InvalidParameter(format!(
                "association id {bad} is not a valid explicit id; ids are positive integers"
            )));
        }
        let high_water: i64 = tx
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = ?1",
                ["time_series_associations"],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        match ids.iter().find(|&&id| id <= high_water) {
            Some(&id) => Err(TimeSeriesError::DuplicateAssociationId(id)),
            None => Ok(()),
        }
    }

    /// Insert one endpoint pair and return the id the catalog filed it under.
    ///
    /// Neither association catalog takes a caller-supplied id — no wire form
    /// carries one, so there is nothing to preserve — so the id column is left
    /// out of the insert entirely and `AUTOINCREMENT` assigns. The id comes
    /// back from `last_insert_rowid()`, which is per-connection and reports
    /// the most recent rowid insert, so the read must stay immediately after
    /// the `execute`: any statement slipped between the two — a denormalized
    /// write, a lookup that itself inserts — makes this hand back that row's
    /// id instead. `RETURNING` would lift the ordering constraint, at the
    /// price the comment in the body describes.
    fn assoc_insert(
        tx: &Connection,
        table: AssocTable,
        left_id: i64,
        left_type: &str,
        right_id: i64,
        right_type: &str,
        detail: &str,
    ) -> Result<i64> {
        // A plain INSERT read back with `last_insert_rowid()`, not `RETURNING`,
        // for the reason given at the time series insert: `RETURNING` needs a
        // statement journal, and under a caller's transaction on an in-memory
        // catalog closing one costs a walk of everything the transaction has
        // touched, so the bulk paths that call this per row would go quadratic.
        let sql = format!(
            "INSERT INTO {} ({}, {}, {}, {}) VALUES (?1, ?2, ?3, ?4)",
            table.name, table.left_id, table.left_type, table.right_id, table.right_type
        );
        let mut stmt = tx.prepare_cached(&sql)?;
        stmt.execute(params![left_id, left_type, right_id, right_type])
            .map_err(|e| map_association_violation(e, detail))?;
        Ok(tx.last_insert_rowid())
    }

    fn assoc_delete(tx: &Connection, table: AssocTable, filter: &EndpointFilter) -> Result<usize> {
        let (where_clause, params) = filter.to_sql(table);
        let param_refs = to_param_refs(&params);
        let sql = format!("DELETE FROM {} {where_clause}", table.name);
        Ok(tx.execute(&sql, param_refs.as_slice())?)
    }

    // ---- Supplemental-attribute associations ------------------------------

    pub fn insert_supplemental_attribute_association(
        tx: &Connection,
        assoc: &SupplementalAttributeAssociation,
    ) -> Result<i64> {
        Self::assoc_insert(
            tx,
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            assoc.component_id,
            &assoc.component_type,
            assoc.attribute_id,
            &assoc.attribute_type,
            &format!(
                "attribute {} is already attached to component {}",
                assoc.attribute_id, assoc.component_id
            ),
        )
    }

    pub fn delete_supplemental_attribute_associations(
        tx: &Connection,
        filter: &SupplementalAttributeFilter,
    ) -> Result<usize> {
        Self::assoc_delete(tx, SUPPLEMENTAL_ATTRIBUTE_TABLE, &filter.endpoints())
    }

    /// Rewrite `old_id` to `new_id` wherever it names a component, e.g. after a
    /// component is replaced by one that inherits its attachments.
    pub fn replace_supplemental_attribute_component_id(
        tx: &Connection,
        old_id: i64,
        new_id: i64,
    ) -> Result<usize> {
        // `component_type` is a denormalized label carried for filtering, so a
        // move has to bring it up to date or the moved rows keep describing the
        // component they came from: filtering by the destination's real type
        // missed them, filtering by the source's type returned them under the
        // destination's id, and `supplemental_attribute_summary` split one
        // component across two contradictory type buckets.
        //
        // The destination's type is taken from the rows it already has. When it
        // has none the catalog has no other record of it — these rows become its
        // only ones — so the label carries over unchanged, and the caller is the
        // only one who could know better. (`assoc_counts_by_type` is unaffected
        // either way: it groups by *attribute* type, not component type.)
        // Only for a real move. `old_id == new_id` is a supported self-move that
        // rewrites nothing, so it must not relabel either — a component whose
        // rows disagree about its type keeps that disagreement rather than
        // having one of the two picked for it.
        let destination_type: Option<String> = if old_id == new_id {
            None
        } else {
            tx.prepare_cached(
                "SELECT component_type FROM supplemental_attribute_associations
                 WHERE component_id = ?1 LIMIT 1",
            )?
            .query_row(params![new_id], |r| r.get(0))
            .optional()?
        };
        tx.execute(
            "UPDATE supplemental_attribute_associations
             SET component_id = ?1,
                 component_type = COALESCE(?3, component_type)
             WHERE component_id = ?2",
            params![new_id, old_id, destination_type],
        )
        .map_err(|e| {
            map_association_violation(
                e,
                &format!(
                    "component {new_id} already carries an attribute that component \
                     {old_id} carries"
                ),
            )
        })
    }

    pub fn list_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<SupplementalAttributeAssociation>> {
        Ok(self
            .assoc_list(SUPPLEMENTAL_ATTRIBUTE_TABLE, &filter.endpoints())?
            .into_iter()
            .map(
                |(id, component_id, component_type, attribute_id, attribute_type)| {
                    SupplementalAttributeAssociation {
                        component_id,
                        component_type,
                        attribute_id,
                        attribute_type,
                        id: Some(id),
                    }
                },
            )
            .collect())
    }

    pub fn has_supplemental_attribute_association(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<bool> {
        self.assoc_has(SUPPLEMENTAL_ATTRIBUTE_TABLE, &filter.endpoints())
    }

    /// Distinct attribute ids matching `filter` — the attributes attached to a
    /// component when `filter.component_id` is set.
    pub fn list_supplemental_attribute_ids(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>> {
        self.assoc_ids(
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            &filter.endpoints(),
            Endpoint::Right,
        )
    }

    /// Distinct component ids matching `filter` — the components carrying an
    /// attribute when `filter.attribute_id` is set.
    pub fn list_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<Vec<i64>> {
        self.assoc_ids(
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            &filter.endpoints(),
            Endpoint::Left,
        )
    }

    pub fn count_supplemental_attribute_associations(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.assoc_count(
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            &filter.endpoints(),
            "COUNT(*)",
        )
    }

    pub fn count_supplemental_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.assoc_count(
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            &filter.endpoints(),
            "COUNT(DISTINCT attribute_id)",
        )
    }

    pub fn count_components_with_attributes(
        &self,
        filter: &SupplementalAttributeFilter,
    ) -> Result<i64> {
        self.assoc_count(
            SUPPLEMENTAL_ATTRIBUTE_TABLE,
            &filter.endpoints(),
            "COUNT(DISTINCT component_id)",
        )
    }

    /// Attachment counts grouped by attribute type.
    pub fn supplemental_attribute_counts_by_type(&self) -> Result<Vec<(String, i64)>> {
        self.assoc_counts_by_type(SUPPLEMENTAL_ATTRIBUTE_TABLE, Endpoint::Right)
    }

    /// Attachment counts grouped by both type labels.
    pub fn supplemental_attribute_summary(&self) -> Result<Vec<SupplementalAttributeSummaryRow>> {
        if !self.has_supplemental_attribute_table {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT attribute_type, component_type, COUNT(*)
             FROM supplemental_attribute_associations
             GROUP BY attribute_type, component_type
             ORDER BY attribute_type, component_type",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SupplementalAttributeSummaryRow {
                attribute_type: r.get(0)?,
                component_type: r.get(1)?,
                count: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    // ---- Parent/child associations ----------------------------------------

    pub fn insert_parent_child_association(
        tx: &Connection,
        assoc: &ParentChildAssociation,
    ) -> Result<i64> {
        Self::assoc_insert(
            tx,
            PARENT_CHILD_TABLE,
            assoc.parent_id,
            &assoc.parent_type,
            assoc.child_id,
            &assoc.child_type,
            &format!(
                "component {} is already the parent of component {}",
                assoc.parent_id, assoc.child_id
            ),
        )
    }

    pub fn delete_parent_child_associations(
        tx: &Connection,
        filter: &ParentChildFilter,
    ) -> Result<usize> {
        Self::assoc_delete(tx, PARENT_CHILD_TABLE, &filter.endpoints())
    }

    /// Rewrite `old_id` to `new_id` wherever it names a component, on either end
    /// of an edge. Done in one statement rather than one per column so a
    /// self-edge (`parent_id = child_id = old_id`) is counted once, not twice.
    pub fn replace_parent_child_component_id(
        tx: &Connection,
        old_id: i64,
        new_id: i64,
    ) -> Result<usize> {
        // Same rule as `replace_supplemental_attribute_component_id`: the type
        // labels are denormalized for filtering, so they move with the id. A
        // component can appear on either end of an edge, so its type is looked
        // up from both, and only the end actually being rewritten is relabelled.
        // Only for a real move, as above.
        let destination_type: Option<String> = if old_id == new_id {
            None
        } else {
            tx.prepare_cached(
                "SELECT parent_type FROM parent_child_associations WHERE parent_id = ?1
                 UNION ALL
                 SELECT child_type FROM parent_child_associations WHERE child_id = ?1
                 LIMIT 1",
            )?
            .query_row(params![new_id], |r| r.get(0))
            .optional()?
        };
        tx.execute(
            "UPDATE parent_child_associations
             SET parent_id   = CASE WHEN parent_id = ?2 THEN ?1 ELSE parent_id END,
                 parent_type = CASE WHEN parent_id = ?2 THEN COALESCE(?3, parent_type)
                                    ELSE parent_type END,
                 child_id    = CASE WHEN child_id  = ?2 THEN ?1 ELSE child_id  END,
                 child_type  = CASE WHEN child_id  = ?2 THEN COALESCE(?3, child_type)
                                    ELSE child_type END
             WHERE parent_id = ?2 OR child_id = ?2",
            params![new_id, old_id, destination_type],
        )
        .map_err(|e| {
            map_association_violation(
                e,
                &format!("rewriting component {old_id} to {new_id} would duplicate an edge"),
            )
        })
    }

    pub fn list_parent_child_associations(
        &self,
        filter: &ParentChildFilter,
    ) -> Result<Vec<ParentChildAssociation>> {
        Ok(self
            .assoc_list(PARENT_CHILD_TABLE, &filter.endpoints())?
            .into_iter()
            .map(
                |(id, parent_id, parent_type, child_id, child_type)| ParentChildAssociation {
                    parent_id,
                    parent_type,
                    child_id,
                    child_type,
                    id: Some(id),
                },
            )
            .collect())
    }

    pub fn has_parent_child_association(&self, filter: &ParentChildFilter) -> Result<bool> {
        self.assoc_has(PARENT_CHILD_TABLE, &filter.endpoints())
    }

    /// Distinct child ids matching `filter` — the children of a component when
    /// `filter.parent_id` is set.
    pub fn list_children(&self, filter: &ParentChildFilter) -> Result<Vec<i64>> {
        self.assoc_ids(PARENT_CHILD_TABLE, &filter.endpoints(), Endpoint::Right)
    }

    /// Distinct parent ids matching `filter` — the parents of a component when
    /// `filter.child_id` is set.
    pub fn list_parents(&self, filter: &ParentChildFilter) -> Result<Vec<i64>> {
        self.assoc_ids(PARENT_CHILD_TABLE, &filter.endpoints(), Endpoint::Left)
    }

    pub fn count_parent_child_associations(&self, filter: &ParentChildFilter) -> Result<i64> {
        self.assoc_count(PARENT_CHILD_TABLE, &filter.endpoints(), "COUNT(*)")
    }

    /// Whether the catalog holds no content of any kind — the emptiness
    /// predicate behind [`crate::Store::is_empty`]. One short-circuited
    /// `SELECT 1 ... LIMIT 1` per content table, so it costs index probes
    /// rather than the aggregate scans a conjunction over the count APIs would.
    ///
    /// **Every persistent content table must be probed here.** This is the one
    /// place that knows the full set. A table added to `schema.rs` and not
    /// added here makes a non-empty store report empty, and a consumer that
    /// skips writing the artifact when the store is empty (InfrastructureSystems.jl
    /// does exactly this) then drops those rows with no error.
    ///
    /// Excluded deliberately: `schema_version` and `catalog_identity` are
    /// bookkeeping and never empty; `feature_sets` is a content-addressed side
    /// table that only ever holds rows referenced from
    /// `time_series_associations`, so it is covered by probing it.
    pub fn is_empty(&self) -> Result<bool> {
        if self.exists(&MetadataFilter::default())? {
            return Ok(false);
        }
        for table in [SUPPLEMENTAL_ATTRIBUTE_TABLE, PARENT_CHILD_TABLE] {
            if self.assoc_has(table, &EndpointFilter::default())? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Borrow a boxed parameter list as rusqlite's slice-of-trait-objects form.
fn to_param_refs(params: &[Box<dyn rusqlite::ToSql>]) -> Vec<&dyn rusqlite::ToSql> {
    params
        .iter()
        .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
        .collect()
}

/// Whether `name` is a table in the main schema. Consulted once per connection
/// at open for each association table.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Map a constraint violation on an association table to
/// [`TimeSeriesError::DuplicateAssociation`], passing every other error through.
/// The unique index on the endpoint-id pair is the only constraint either table
/// carries, so a violation can mean nothing else. `detail` names the offending
/// pair in the relationship's own vocabulary.
fn map_association_violation(e: rusqlite::Error, detail: &str) -> TimeSeriesError {
    match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            TimeSeriesError::DuplicateAssociation(detail.to_string())
        }
        other => other.into(),
    }
}

/// Parse one `features` row into its key/value pair. The row must select
/// `key, value_kind, value_int, value_float, value_bool, value_str` starting at
/// the column after `association_id`.
fn parse_feature_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, FeatureValue)> {
    let key: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let value = match kind.as_str() {
        "int" => FeatureValue::Int(row.get::<_, i64>(3)?),
        "float" => FeatureValue::Float(row.get::<_, f64>(4)?),
        "bool" => FeatureValue::Bool(row.get::<_, i64>(5)? != 0),
        "str" => FeatureValue::Str(row.get::<_, String>(6)?),
        _ => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown feature kind: {kind}"),
                )),
            ));
        }
    };
    Ok((key, value))
}

/// Map a SQLite UNIQUE-index constraint violation to the spec's
/// [`TimeSeriesError::DuplicateTimeSeries`], passing every other error through.
/// Shared by the `INSERT` and `UPDATE` paths where the association uniqueness
/// index can fire (`rename`, `replace_owner`).
fn map_unique_violation(e: rusqlite::Error) -> TimeSeriesError {
    match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            TimeSeriesError::DuplicateTimeSeries
        }
        other => other.into(),
    }
}

/// Open a catalog file read-only, including on read-only media.
///
/// A WAL-mode database needs its `-shm` index to be read, and SQLite creates
/// that sidecar even for a read-only connection. Where the file cannot be
/// created — a read-only mount, a directory the process may not write, an
/// archive served to a reader — the open fails outright, which would make a
/// perfectly readable store unopenable for the two callers that read one
/// without writing: a `read_only` [`Store::open`] and the load-into-memory of
/// [`MetadataStore::open_path_into_memory`].
///
/// SQLite's answer is `immutable=1`, which reads the database file directly and
/// builds no index. It is only correct where nothing can be writing, and it
/// *ignores* a `-wal`: a database with one would silently read as though the
/// rows committed there did not exist. So it is the fallback, not the default,
/// and it is only tried when there is no `-wal` beside the database. A crashed
/// writer's sidecar on read-only media therefore still fails loudly, with
/// SQLite's own message, rather than quietly dropping its rows.
///
/// The probe read is what forces the index to be set up: opening a connection
/// is lazy, so without it the failure would surface later, at an arbitrary
/// query, past any chance to fall back.
fn open_read_only(path: &Path) -> Result<Connection> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let probe = |conn: &Connection| -> rusqlite::Result<()> {
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
    };
    let direct = Connection::open_with_flags(path, flags)
        .and_then(|conn| probe(&conn).map(|()| conn))
        .map_err(TimeSeriesError::from);
    let Err(e) = direct else {
        return direct;
    };
    if sqlite_sidecar(path, "-wal").exists() {
        return Err(e);
    }
    let immutable = Connection::open_with_flags(sqlite_uri(path, "immutable=1"), flags)
        .and_then(|conn| probe(&conn).map(|()| conn));
    // The fallback's own failure is reported as the original one: it is the
    // error for the way the database is actually meant to be opened, and
    // `immutable=1` is an implementation detail of this function.
    immutable.map_err(|_| e)
}

/// `path` as a SQLite `file:` URI carrying `query`.
///
/// Only the three characters SQLite reads as URI syntax are escaped; everything
/// else, including spaces, is passed through. Windows separators become forward
/// slashes, which SQLite accepts for a drive-letter path (`file:C:/dir/db`).
fn sqlite_uri(path: &Path, query: &str) -> String {
    let mut out = String::from("file:");
    for c in path.to_string_lossy().chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            '\\' => out.push('/'),
            c => out.push(c),
        }
    }
    out.push('?');
    out.push_str(query);
    out
}

/// The `-wal` / `-shm` companion of a SQLite database path.
fn sqlite_sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Canonical ISO-8601 encoding of a period for storage in the catalog.
fn period_to_iso(p: Period) -> String {
    p.to_iso8601()
}

/// Parse a period from its catalog ISO-8601 encoding. A parse failure is an
/// integrity error: the value was written by [`period_to_iso`].
fn iso_to_period(s: &str) -> Result<Period> {
    Period::from_iso8601(s)
        .map_err(|e| TimeSeriesError::IntegrityError(format!("bad period '{s}' in catalog: {e}")))
}

fn is_subset(required: &Features, actual: &Features) -> bool {
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|a| a == v))
}

fn bytes_to_hash32(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() == 32 {
        let mut h = [0u8; 32];
        h.copy_from_slice(bytes);
        Some(h)
    } else {
        None
    }
}

/// Helper to run a `SELECT data_hash` query and collect raw bytes, isolating
/// the prepared statement's lifetime so the caller's tx isn't borrowed.
fn collect_data_hashes(
    tx: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Vec<u8>>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt
        .query_map(params, |row| row.get::<_, Vec<u8>>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

struct MetaRow {
    /// The catalog's `INTEGER PRIMARY KEY`. Carried onto
    /// [`TimeSeriesMetadata::id`] as `Some`, since a stored row always has one,
    /// and used directly by [`Self::list_inner`] for the callers that want the
    /// id beside the metadata rather than inside it.
    id: i64,
    owner_id: i64,
    owner_type: String,
    owner_category: OwnerCategory,
    time_series_type: TimeSeriesType,
    name: String,
    data_hash: [u8; 32],
    initial_timestamp: Option<DateTime<Utc>>,
    resolution: Option<Period>,
    length: Option<usize>,
    horizon: Option<Period>,
    interval: Option<Period>,
    count: Option<usize>,
    /// The row's `timestamps_hash` column; the vector itself lives in the array
    /// file and is hydrated by the caller, which batches that lookup across rows.
    timestamps_hash: Option<[u8; 32]>,
    units: Option<String>,
    quantity_kind: Option<String>,
    unit_system: Option<UnitSystem>,
    time_reference: Option<TimeReference>,
    component_field: Option<String>,
    percentiles: Option<Vec<f64>>,
    element_type: crate::types::element_type::ElementType,
    element_shape: Vec<usize>,
    application_data: Option<String>,
}

impl MetaRow {
    fn into_metadata(
        self,
        features: Features,
        timestamps: Option<Vec<DateTime<Utc>>>,
    ) -> TimeSeriesMetadata {
        TimeSeriesMetadata {
            owner_id: self.owner_id,
            owner_type: self.owner_type,
            owner_category: self.owner_category,
            time_series_type: self.time_series_type,
            name: self.name,
            data_hash: self.data_hash,
            initial_timestamp: self.initial_timestamp,
            resolution: self.resolution,
            length: self.length,
            horizon: self.horizon,
            interval: self.interval,
            count: self.count,
            timestamps,
            features,
            units: self.units,
            quantity_kind: self.quantity_kind,
            unit_system: self.unit_system,
            time_reference: self.time_reference,
            component_field: self.component_field,
            percentiles: self.percentiles,
            element_type: self.element_type,
            element_shape: self.element_shape,
            application_data: self.application_data,
            // A row that came out of the catalog always has an id; `None` is
            // reserved for the write direction, where it means "assign one".
            id: Some(TimeSeriesId(self.id)),
        }
    }
}

/// Parse one association row. Column 0 is the `features_hash`, which is how the
/// caller looks the row's feature set up in the content-addressed `feature_sets`
/// table.
fn parse_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<([u8; 32], MetaRow)> {
    let features_hash: Vec<u8> = row.get(0)?;
    let owner_id: i64 = row.get(1)?;
    let owner_type: String = row.get(2)?;
    let owner_category: i64 = row.get(3)?;
    let time_series_type: i64 = row.get(4)?;
    let name: String = row.get(5)?;
    let data_hash_bytes: Vec<u8> = row.get(6)?;
    let initial_timestamp: Option<String> = row.get(7)?;
    let resolution_iso: Option<String> = row.get(8)?;
    let length: Option<i64> = row.get(9)?;
    let horizon_iso: Option<String> = row.get(10)?;
    let interval_iso: Option<String> = row.get(11)?;
    let count: Option<i64> = row.get(12)?;
    let timestamps_hash: Option<Vec<u8>> = row.get(13)?;
    let units: Option<String> = row.get(14)?;
    let quantity_kind: Option<String> = row.get(15)?;
    let unit_system_str: Option<String> = row.get(16)?;
    let time_reference_str: Option<String> = row.get(17)?;
    let component_field: Option<String> = row.get(18)?;
    let percentiles_json: Option<String> = row.get(19)?;
    let element_type_str: String = row.get(20)?;
    let element_shape_json: Option<String> = row.get(21)?;
    let application_data: Option<String> = row.get(22)?;
    let id: i64 = row.get(23)?;

    // An unrecognized basis is an error, not a silent `None`. The column has no
    // CHECK precisely so a future basis can be added without a format bump,
    // which means an older reader *will* meet one; reading it as "unspecified"
    // would quietly turn per-unit values into values of unknown basis, and the
    // consumer would have no way to know it had happened.
    let unit_system = unit_system_str
        .map(|s| {
            UnitSystem::parse(&s).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    16,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid unit_system: {s:?}"),
                    )),
                )
            })
        })
        .transpose()?;

    // Same reasoning as `unit_system` above, and one degree sharper: reading an
    // unparseable spelling as "unspecified" would hand a caller an aware
    // timestamp for a series that never claimed one.
    let time_reference = time_reference_str
        .map(|s| {
            TimeReference::parse(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    17,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid time_reference: {s:?} ({e})"),
                    )),
                )
            })
        })
        .transpose()?;

    let owner_category = OwnerCategory::from_code(owner_category).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid owner_category code: {owner_category}"),
            )),
        )
    })?;
    let ts_type = TimeSeriesType::from_code(time_series_type).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid time_series_type code: {time_series_type}"),
            )),
        )
    })?;
    let mut data_hash = [0u8; 32];
    if data_hash_bytes.len() != 32 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "data_hash must be 32 bytes",
            )),
        ));
    }
    data_hash.copy_from_slice(&data_hash_bytes);

    let initial_timestamp = initial_timestamp
        .map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let timestamps_hash = timestamps_hash
        .map(|bytes| {
            bytes_to_hash32(&bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    13,
                    rusqlite::types::Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "timestamps_hash must be 32 bytes",
                    )),
                )
            })
        })
        .transpose()?;

    let percentiles = percentiles_json
        .map(|s| serde_json::from_str::<Vec<f64>>(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, Box::new(e))
        })?;

    let element_type = crate::types::element_type::ElementType::parse(&element_type_str)
        .ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                19,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid element_type: {element_type_str}"),
                )),
            )
        })?;
    let element_shape: Vec<usize> = element_shape_json
        .map(|s| serde_json::from_str::<Vec<usize>>(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(20, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();

    let parse_period = |col: usize, s: Option<String>| -> rusqlite::Result<Option<Period>> {
        s.map(|s| {
            Period::from_iso8601(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    col,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    )),
                )
            })
        })
        .transpose()
    };
    let resolution = parse_period(8, resolution_iso)?;
    let horizon = parse_period(10, horizon_iso)?;
    let interval = parse_period(11, interval_iso)?;

    let features_hash = bytes_to_hash32(&features_hash).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "features_hash must be 32 bytes",
            )),
        )
    })?;

    Ok((
        features_hash,
        MetaRow {
            id,
            owner_id,
            owner_type,
            owner_category,
            time_series_type: ts_type,
            name,
            data_hash,
            initial_timestamp,
            resolution,
            length: length.map(|l| l as usize),
            horizon,
            interval,
            count: count.map(|c| c as usize),
            timestamps_hash,
            units,
            quantity_kind,
            unit_system,
            time_reference,
            component_field,
            percentiles,
            element_type,
            element_shape,
            application_data,
        },
    ))
}

// Allow Connection-level lookups through a transaction for reads (used by the
// `Store` layer where a tx is already in-flight for atomicity). Implemented as
// helper free fns so we don't have two parallel Send/Sync wrappers.
pub fn references_to_in_tx(tx: &Connection, data_hash: &[u8; 32]) -> Result<i64> {
    let count: i64 = tx
        .prepare_cached("SELECT COUNT(*) FROM time_series_associations WHERE data_hash = ?1")?
        .query_row(params![data_hash.as_slice()], |row| row.get(0))?;
    Ok(count)
}

/// Count the associations sitting on the explicit time axis `timestamps_hash`,
/// inside an in-flight transaction.
///
/// The timestamp-vector counterpart of [`references_to_in_tx`], and the reason
/// it is a separate function rather than a parameter: an axis is referenced
/// through its own column, so an array's reference count says nothing about it.
pub fn timestamp_references_in_tx(tx: &Connection, timestamps_hash: &[u8; 32]) -> Result<i64> {
    let count: i64 = tx
        .prepare_cached("SELECT COUNT(*) FROM time_series_associations WHERE timestamps_hash = ?1")?
        .query_row(params![timestamps_hash.as_slice()], |row| row.get(0))?;
    Ok(count)
}

/// Does an association of `conflicting_type` already exist sharing the
/// abstract-deterministic family identity `(owner_id, owner_category, name,
/// resolution, features)`, *ignoring* interval and the requesting type?
///
/// `Deterministic` and `DeterministicSingleTimeSeries` are mutually exclusive
/// for one family: the latter is a synthetic view of a `SingleTimeSeries`, so a
/// caller should never hold both. The catalog's unique index keys on
/// `time_series_type` and so cannot enforce this; the add and transform paths
/// call this inside their transaction to reject the overlap. The match is by
/// `features_hash` (a SHA-256 collision is the only false positive), which is
/// sufficient for a guard.
pub fn forecast_family_conflict(
    tx: &Connection,
    owner_id: i64,
    owner_category: OwnerCategory,
    name: &str,
    resolution: Option<Period>,
    features_hash: &[u8; 32],
    conflicting_type: TimeSeriesType,
) -> Result<bool> {
    let resolution_iso = resolution.map(period_to_iso);
    // `prepare_cached`: this runs once per Deterministic row in a bulk add, and
    // an uncached prepare (SQL parse + query plan) costs more than executing
    // the point query itself.
    let exists: Option<i64> = tx
        .prepare_cached(
            "SELECT 1 FROM time_series_associations
             WHERE owner_id = ?1 AND owner_category = ?2 AND time_series_type = ?3 AND name = ?4
               AND ((?5 IS NULL AND resolution IS NULL) OR resolution = ?5)
               AND features_hash = ?6
             LIMIT 1",
        )?
        .query_row(
            params![
                owner_id,
                owner_category.code(),
                conflicting_type.code(),
                name,
                resolution_iso,
                features_hash.as_slice(),
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

/// The columns [`MetadataStore::delete_by_id`] reads before deleting, as SQLite
/// hands them over: `(data_hash, time_series_type, owner_id, owner_category,
/// name, resolution, features_hash)`.
type RawDeletedRow = (Vec<u8>, i64, i64, i64, String, Option<String>, Vec<u8>);

/// What [`MetadataStore::delete_by_id`] knew about the row it removed: the
/// array it named, its type, and the **forecast family** it belonged to -- the
/// `(owner, name, resolution, features)` tuple [`forecast_family_conflict`]
/// probes. A removal needs the family, not just the hash, to decide whether a
/// `DeterministicSingleTimeSeries` lost its source: two owners' byte-identical
/// `SingleTimeSeries` share a hash but are different sources.
#[derive(Debug, Clone)]
pub struct DeletedRow {
    pub data_hash: [u8; 32],
    pub time_series_type: TimeSeriesType,
    pub owner_id: i64,
    pub owner_category: OwnerCategory,
    pub name: String,
    pub resolution: Option<Period>,
    pub features_hash: [u8; 32],
}

/// The first series whose move from `old_owner` to `new_owner` would put both
/// `Deterministic` and `DeterministicSingleTimeSeries` in one family, as
/// `(name, moving_type, existing_type)`.
///
/// [`forecast_family_conflict`] answers the question for one prospective row;
/// this answers it for the whole set that [`MetadataStore::replace_owner`] moves
/// in a single `UPDATE`, which cannot be decomposed into per-row checks without
/// listing every row first. The `CASE` maps each moving row to the type it
/// excludes, so one query covers both directions.
pub fn forecast_family_conflict_on_owner_move(
    tx: &Connection,
    old_owner: i64,
    new_owner: i64,
    owner_category: OwnerCategory,
) -> Result<Option<(String, TimeSeriesType, TimeSeriesType)>> {
    let det = TimeSeriesType::Deterministic.code();
    let dst = TimeSeriesType::DeterministicSingleTimeSeries.code();
    let row: Option<(String, i64, i64)> = tx
        .prepare_cached(
            "SELECT moving.name, moving.time_series_type, existing.time_series_type
             FROM time_series_associations AS moving
             JOIN time_series_associations AS existing
               ON existing.owner_id = ?2
              AND existing.owner_category = moving.owner_category
              AND existing.name = moving.name
              AND ((moving.resolution IS NULL AND existing.resolution IS NULL)
                   OR existing.resolution = moving.resolution)
              AND existing.features_hash = moving.features_hash
              AND existing.time_series_type = CASE moving.time_series_type
                                                WHEN ?4 THEN ?5
                                                WHEN ?5 THEN ?4
                                              END
             WHERE moving.owner_id = ?1 AND moving.owner_category = ?3
             LIMIT 1",
        )?
        .query_row(
            params![old_owner, new_owner, owner_category.code(), det, dst],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    row.map(|(name, moving, existing)| {
        Ok((
            name,
            TimeSeriesType::from_code(moving).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("unknown time_series_type {moving}"))
            })?,
            TimeSeriesType::from_code(existing).ok_or_else(|| {
                TimeSeriesError::IntegrityError(format!("unknown time_series_type {existing}"))
            })?,
        ))
    })
    .transpose()
}

#[cfg(test)]
mod timestamp_cache_tests {
    //! Guards on the memo's bookkeeping. A cache that returned the *wrong*
    //! vector for a hash would corrupt every irregular read that hit it, so the
    //! mapping is asserted directly rather than only through the store.

    use chrono::{Duration, TimeZone, Utc};

    use super::TimestampCache;

    fn vector(seed: i64) -> Vec<chrono::DateTime<Utc>> {
        let t0 = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        (0..3)
            .map(|k| t0 + Duration::minutes(seed * 100 + k))
            .collect()
    }

    #[test]
    fn each_hash_maps_to_its_own_vector() {
        let mut cache = TimestampCache::default();
        for seed in 0..TimestampCache::CAPACITY as i64 {
            cache.insert([seed as u8; 32], &vector(seed));
        }
        for seed in 0..TimestampCache::CAPACITY as i64 {
            assert_eq!(cache.get(&[seed as u8; 32]), Some(vector(seed)));
        }
        assert_eq!(cache.get(&[0xff; 32]), None, "an unseen hash is a miss");
    }

    #[test]
    fn re_inserting_a_held_hash_does_not_duplicate_it() {
        let mut cache = TimestampCache::default();
        cache.insert([1u8; 32], &vector(1));
        cache.insert([1u8; 32], &vector(1));
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_coldest_entry() {
        let mut cache = TimestampCache::default();
        let overflow = TimestampCache::CAPACITY as i64 + 2;
        for seed in 0..overflow {
            cache.insert([seed as u8; 32], &vector(seed));
        }
        assert_eq!(cache.entries.len(), TimestampCache::CAPACITY);
        // The oldest went; the most recent survived, still mapped correctly.
        assert_eq!(cache.get(&[0u8; 32]), None);
        assert_eq!(
            cache.get(&[(overflow - 1) as u8; 32]),
            Some(vector(overflow - 1))
        );
    }

    #[test]
    fn a_hit_is_refreshed_so_the_hot_axis_survives_a_sweep() {
        // The case this exists for: one shared axis read constantly, with other
        // axes passing through. Without move-to-back the hot one would be
        // evicted by its own age.
        let mut cache = TimestampCache::default();
        cache.insert([9u8; 32], &vector(9));
        for seed in 0..(TimestampCache::CAPACITY as i64 * 3) {
            assert_eq!(cache.get(&[9u8; 32]), Some(vector(9)), "hot axis at {seed}");
            cache.insert([seed as u8; 32], &vector(seed));
        }
        assert_eq!(cache.get(&[9u8; 32]), Some(vector(9)));
    }
}

#[cfg(test)]
mod index_plan_tests {
    //! Guards on the query planner's index choices.
    //!
    //! The five secondary indexes in `schema::DDL` (`idx_ts_type`, `idx_name`,
    //! `idx_owner_type`, `idx_category_owner`, `idx_interval`) were added after
    //! measuring 3–34x speedups on a 405k-row catalog — see the long comment
    //! beside them. Those measurements live in a comment; nothing stopped a
    //! later schema edit from quietly returning a hot predicate to a full table
    //! scan.
    //!
    //! Each test below applies the real DDL to a fresh in-memory database, runs
    //! `EXPLAIN QUERY PLAN` for one hot query *shape*, and asserts the expected
    //! index appears in the plan. These are planner-choice assertions, not
    //! timings: they need no data and are stable because SQLite's planner is
    //! deterministic for a fixed schema and query.
    //!
    //! A failure here means "this predicate no longer uses its index", which is
    //! a performance regression to investigate, not necessarily a bug.

    use super::schema;

    fn schema_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::DDL).unwrap();
        conn
    }

    /// The `EXPLAIN QUERY PLAN` output for `sql`, joined into one string.
    ///
    /// Each `?` in `sql` is bound to NULL. The plan is fixed at prepare time, so
    /// the bound values cannot influence which index the planner picks — they
    /// only need to be present for the statement to step.
    fn plan(conn: &rusqlite::Connection, sql: &str) -> String {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|e| panic!("preparing plan for {sql}: {e}"));
        let nulls = vec![rusqlite::types::Null; sql.matches('?').count()];
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params_from_iter(nulls), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.join("\n")
    }

    /// Assert the plan for `sql` names `index`, and that it is not a full scan
    /// of the wide association table.
    fn assert_uses_index(sql: &str, index: &str) {
        let conn = schema_conn();
        let p = plan(&conn, sql);
        assert!(
            p.contains(index),
            "expected {index} in the plan for:\n  {sql}\ngot:\n{p}"
        );
        assert!(
            !p.contains("SCAN time_series_associations\n")
                && !p.ends_with("SCAN time_series_associations"),
            "plan for {sql} fell back to a full table scan:\n{p}"
        );
    }

    #[test]
    fn exact_name_filter_uses_idx_name() {
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE name = ?",
            "idx_name",
        );
    }

    #[test]
    fn prefix_name_glob_uses_idx_name() {
        // The column has SQLite's default BINARY collation, which lets GLOB
        // range-seek on a literal prefix instead of scanning.
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE name GLOB 'wind_*'",
            "idx_name",
        );
    }

    #[test]
    fn component_field_filter_uses_its_partial_index() {
        // `idx_component_field` is partial (`WHERE component_field IS NOT
        // NULL`), which the planner may only use when it can prove the
        // condition holds. It can here, and for a *bound parameter* rather than
        // a literal: `component_field = ?` is false against NULL no matter what
        // the parameter binds to. If SQLite ever stopped drawing that
        // inference, the filter would silently degrade to a full scan of the
        // widest table in the catalog -- which is exactly what this pins.
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE component_field = ?",
            "idx_component_field",
        );
    }

    #[test]
    fn time_series_type_count_uses_idx_ts_type() {
        assert_uses_index(
            "SELECT COUNT(*) FROM time_series_associations WHERE time_series_type = ?",
            "idx_ts_type",
        );
    }

    #[test]
    fn owner_type_filter_uses_idx_owner_type() {
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE owner_type = ?",
            "idx_owner_type",
        );
    }

    #[test]
    fn distinct_owner_type_uses_idx_owner_type() {
        // A covering scan of the narrow index, not the wide table.
        assert_uses_index(
            "SELECT DISTINCT owner_type FROM time_series_associations",
            "idx_owner_type",
        );
    }

    #[test]
    fn category_scoped_owner_enumeration_uses_idx_category_owner() {
        // `idx_owner` leads with owner_id so it cannot serve a category-only
        // predicate; `idx_category_owner` leads with the category and keeps
        // `DISTINCT owner_id` covered.
        assert_uses_index(
            "SELECT DISTINCT owner_id FROM time_series_associations WHERE owner_category = ?",
            "idx_category_owner",
        );
    }

    #[test]
    fn distinct_interval_uses_idx_interval() {
        assert_uses_index(
            "SELECT DISTINCT interval FROM time_series_associations WHERE interval IS NOT NULL",
            "idx_interval",
        );
    }

    #[test]
    fn distinct_resolution_uses_idx_resolution() {
        assert_uses_index(
            "SELECT DISTINCT resolution FROM time_series_associations WHERE resolution IS NOT NULL",
            "idx_resolution",
        );
    }

    #[test]
    fn data_hash_lookup_uses_idx_hash() {
        // Regression side: `idx_hash` predates the five above and drives array
        // reference counting on every delete.
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE data_hash = ?",
            "idx_hash",
        );
    }

    #[test]
    fn owner_lookup_uses_idx_category_owner() {
        // `(owner_id, owner_category)` is served by `idx_category_owner`, not by
        // `idx_owner`: both cover the predicate, and the planner prefers the
        // narrower two-small-column index. Pinned as the observed choice — this
        // is why `idx_owner` alone was not enough for the category-scoped
        // enumeration above.
        assert_uses_index(
            "SELECT * FROM time_series_associations WHERE owner_id = ? AND owner_category = ?",
            "idx_category_owner",
        );
    }

    #[test]
    fn named_existence_probe_uses_an_index() {
        // The hot per-component probe behind `exists` when a caller asks
        // "does this owner have a series of this type with this name?" —
        // the exact SQL `MetadataFilter::to_sql` renders for that filter.
        // The four bound predicates form a left prefix of the uniqueness
        // index, so the planner answers with a covering four-column seek.
        assert_uses_index(
            "SELECT 1 FROM time_series_associations
             WHERE 1=1 AND owner_id = ? AND owner_category = ?
               AND time_series_type = ? AND name = ? LIMIT 1",
            "uq_ts_assoc_coalesced",
        );
    }

    #[test]
    fn the_full_identity_lookup_uses_the_unique_index() {
        // The hot single-key read: `get_by_key` resolves one row by its whole
        // identity and must land on the unique index, not scan.
        assert_uses_index(
            "SELECT * FROM time_series_associations
             WHERE owner_id = ? AND owner_category = ? AND time_series_type = ?
               AND name = ? AND COALESCE(resolution, '') = ?
               AND COALESCE(interval, '') = ? AND features_hash = ?",
            "uq_ts_assoc_coalesced",
        );
    }

    #[test]
    fn every_expected_index_exists_after_applying_the_ddl() {
        // Cheap tripwire against an index being deleted outright: the plan
        // assertions above would then fail with a confusing message.
        let conn = schema_conn();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'index' AND tbl_name = 'time_series_associations'
                 ORDER BY name",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for expected in [
            "idx_category_owner",
            "idx_hash",
            "idx_interval",
            "idx_name",
            "idx_owner",
            "idx_owner_type",
            "idx_resolution",
            "idx_ts_type",
            "uq_ts_assoc",
            "uq_ts_assoc_coalesced",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "index {expected} is missing; present: {names:?}"
            );
        }
    }
}

#[cfg(test)]
mod type_predicate_tests {
    //! Guards on the *shape* of the `time_series_type` predicate.
    //!
    //! `EXPLAIN QUERY PLAN` cannot fully separate these — SQLite renders an
    //! `IN` list as `time_series_type=?`, the same text an equality produces —
    //! so these assert on the generated SQL instead. Three rules are pinned:
    //!
    //! 1. A predicate naming one code is a plain equality.
    //! 2. A widening request standing alone becomes `BETWEEN` (one index seek).
    //! 3. A widening request inside an identity-shaped filter stays `IN`,
    //!    because an inequality on a middle column of `uq_ts_assoc` truncates
    //!    the composite seek — measured as a plan collapse to `idx_name`.

    use super::{MetadataFilter, SpanForm, TypeMatch, push_type_predicate};
    use crate::types::metadata::OwnerCategory;
    use crate::types::time_series::TimeSeriesType;

    fn predicate(m: TypeMatch, form: SpanForm) -> (String, usize) {
        let mut sql = String::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_type_predicate(&mut sql, &mut params, m, form);
        (sql, params.len())
    }

    #[test]
    fn an_exact_predicate_is_one_equality_in_either_form() {
        // The key-lookup path. Widening here would double the index probes on
        // the hottest read path, and the form must not change that.
        for form in [SpanForm::Range, SpanForm::In] {
            let (sql, n) = predicate(TypeMatch::Exact(TimeSeriesType::Deterministic), form);
            assert_eq!(sql, " AND time_series_type = ?", "{form:?}");
            assert_eq!(n, 1, "{form:?}");
        }
    }

    #[test]
    fn a_standalone_deterministic_request_is_a_range() {
        let (sql, n) = predicate(
            TypeMatch::Requested(TimeSeriesType::Deterministic),
            SpanForm::Range,
        );
        assert_eq!(sql, " AND time_series_type BETWEEN ? AND ?");
        assert_eq!(n, 2, "the span's two endpoints");
    }

    #[test]
    fn a_composite_deterministic_request_stays_in() {
        let (sql, n) = predicate(
            TypeMatch::Requested(TimeSeriesType::Deterministic),
            SpanForm::In,
        );
        assert_eq!(sql, " AND time_series_type IN (?, ?)");
        assert_eq!(n, 2);
    }

    #[test]
    fn every_non_deterministic_type_is_one_equality() {
        for t in [
            TimeSeriesType::SingleTimeSeries,
            TimeSeriesType::NonSequentialTimeSeries,
            TimeSeriesType::DeterministicSingleTimeSeries,
            TimeSeriesType::Probabilistic,
            TimeSeriesType::Scenarios,
        ] {
            for m in [TypeMatch::Requested(t), TypeMatch::Exact(t)] {
                let (sql, n) = predicate(m, SpanForm::Range);
                assert_eq!(sql, " AND time_series_type = ?", "{m:?}");
                assert_eq!(n, 1, "{m:?}");
            }
        }
    }

    #[test]
    fn identity_shaped_filters_pick_the_in_form() {
        // Anything constraining a column that follows `time_series_type` in
        // `uq_ts_assoc` wants the composite seek preserved.
        let base = MetadataFilter {
            owner_id: Some(1),
            owner_category: Some(OwnerCategory::Component),
            ..Default::default()
        };
        assert_eq!(base.span_form(), SpanForm::Range, "type-scoped only");

        for f in [
            MetadataFilter {
                name: Some("load".into()),
                ..base.clone()
            },
            MetadataFilter {
                name_glob: Some("lo*".into()),
                ..base.clone()
            },
            MetadataFilter {
                resolution: Some(crate::types::period::Period::from(chrono::Duration::hours(
                    1,
                ))),
                ..base.clone()
            },
            MetadataFilter {
                interval: Some(crate::types::period::Period::from(chrono::Duration::hours(
                    1,
                ))),
                ..base.clone()
            },
            MetadataFilter {
                features_hash: Some([0u8; 32]),
                ..base.clone()
            },
        ] {
            assert_eq!(f.span_form(), SpanForm::In, "{f:?}");
        }
    }

    #[test]
    fn the_widened_sql_binds_exactly_the_spanned_codes() {
        // The bound values, not just the shape: a range whose endpoints did not
        // match `code_span` would silently select the wrong types.
        let (lo, hi) = TimeSeriesType::Deterministic.code_span();
        assert_eq!(lo, TimeSeriesType::Deterministic.code());
        assert_eq!(hi, TimeSeriesType::DeterministicSingleTimeSeries.code());
        assert_eq!(hi - lo, 1, "adjacency is what makes the range correct");
    }
}
