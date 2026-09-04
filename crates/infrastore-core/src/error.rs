use thiserror::Error;

pub type Result<T> = std::result::Result<T, TimeSeriesError>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TimeSeriesError {
    #[error("time series not found")]
    NotFound,

    /// An id-addressed operation named a row belonging to an owner other than
    /// the one the caller expected.
    ///
    /// Raised only by the owner-guarded forms — [`crate::Store::read_by_id_for_owner`]
    /// and [`crate::Store::remove_by_ids_for_owner`] — which exist because the
    /// two halves cannot be checked separately: an id survives
    /// [`crate::Store::replace_owner`], so a row confirmed by one
    /// call and acted on by the next can move between them, and the removal
    /// that meant to retire *this* owner's series retires the new owner's
    /// instead.
    ///
    /// Distinct from [`Self::NotFound`], which says no row carries the id at
    /// all: here the row exists and the caller's belief about it is what is
    /// stale.
    #[error(
        "association {id} belongs to owner {actual_id} ({actual_category}), not \
         to the expected owner {expected_id} ({expected_category})"
    )]
    OwnerMismatch {
        id: i64,
        expected_id: i64,
        expected_category: &'static str,
        actual_id: i64,
        actual_category: &'static str,
    },

    #[error("a time series with the same key already exists")]
    DuplicateTimeSeries,

    /// An association with the same identity already exists: the
    /// `(component_id, attribute_id)` pair for a supplemental-attribute
    /// attachment, or the ordered `(parent_id, child_id)` pair for a
    /// parent/child edge. Type names are not part of either identity, so the
    /// same pair under different type names still collides.
    ///
    /// The payload names the offending pair in that relationship's own
    /// vocabulary; it is a human-readable message, not a parseable encoding.
    #[error("duplicate association: {0}")]
    DuplicateAssociation(String),

    /// A caller supplied an explicit association `id` that the catalog has
    /// already handed out.
    ///
    /// Distinct from [`Self::DuplicateTimeSeries`], which is the *identity*
    /// tuple colliding. Both surface as a SQLite constraint violation and are
    /// told apart by the extended result code, because they mean opposite
    /// things to the caller: a duplicate series is usually a re-add to fix, an
    /// id collision means the import's ids do not fit this store.
    ///
    /// Ids only ratchet upward, so this is what an import into a *non-empty*
    /// store looks like when its ids sit at or below the current high-water
    /// mark. Importing into a fresh store is the case that always works.
    #[error(
        "association id {0} is already in use; explicit ids can only be supplied \
         above the catalog's high-water mark, so a document's own ids fit a fresh \
         store but not one that has already assigned ids of its own"
    )]
    DuplicateAssociationId(i64),

    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("integrity check failed: {0}")]
    IntegrityError(String),

    #[error(
        "store was written in on-disk format {found}, but this build reads {expected}; \
         the formats are incompatible and no in-place upgrade is available"
    )]
    IncompatibleFormat {
        found: String,
        expected: &'static str,
    },

    /// The catalog beside this store is at an older revision than this build
    /// writes, and the connection is read-only so it cannot be upgraded.
    ///
    /// Unlike [`Self::IncompatibleFormat`] this is not a dead end: opening the
    /// store once for *writing* runs the migration ladder in
    /// [`crate::metadata::migrate`] and brings it up to date in place.
    #[error(
        "the store's catalog is at revision {found}, but this build writes revision \
         {expected}; open the store once for writing (for example with the \
         `infrastore` CLI) to upgrade it in place, then retry this read-only open"
    )]
    CatalogMigrationRequired { found: i64, expected: i64 },

    /// The catalog was written by a newer build than this one. There is no
    /// downgrade path, and this build must not touch it: its DDL and its
    /// migration ladder both describe an older shape.
    #[error(
        "the store's catalog is at revision {found}, which is newer than the \
         revision {expected} this build understands; it was written by a newer \
         infrastore and must not be opened by this one"
    )]
    CatalogTooNew { found: i64, expected: i64 },

    #[error(
        "the HDF5 file and its catalog do not carry the same generation stamp \
         (HDF5: {h5}, catalog: {sqlite}); they are halves of two different saves, \
         most likely because a save was interrupted between writing the two files \
         or because one of them was copied, replaced, or created without the other"
    )]
    MismatchedArtifact { h5: String, sqlite: String },

    /// A store already exists where one was about to be created.
    ///
    /// Creating truncates the HDF5 file but only *opens* the catalog beside it,
    /// so creating over an existing artifact would leave an empty array file
    /// paired with the old catalog's rows — a store that reopens cleanly and
    /// reports every series still present while every array is a dangling
    /// reference. Refusing is the only point that can tell "fresh store" apart
    /// from "this path already holds a save".
    #[error(
        "a store already exists at {path}; creating one there would discard its \
         arrays while leaving its catalog in place, which reopens as a store whose \
         every array is missing. Open it instead, or create it with the explicit \
         replacing form if you meant to discard it"
    )]
    StoreExists { path: String },

    /// The artifact at `path` is already open in this process.
    ///
    /// Every `Store` builds its own map from content hash to HDF5 column at
    /// open and trusts it for its lifetime, while libhdf5 shares one file
    /// object between two opens of the same file in a process. Two handles on
    /// one artifact therefore disagree: two writers each think the other's
    /// slot is free and overwrite it, and a reader beside a writer resolves a
    /// hash to a column that now holds another series' values. HDF5's file
    /// lock only refuses a second *process*, so this is the in-process half of
    /// the single-writer rule. Drop the handle you hold before opening another.
    #[error(
        "the store at {path} is already open in this process; a second handle would \
         read and write the wrong packed columns. Close the existing handle first"
    )]
    StoreInUse { path: String },

    #[error("store is read-only")]
    ReadOnlyStore,

    #[error("connection error: {0}")]
    ConnectionError(String),

    #[error("forecast parameters are incompatible with existing forecasts")]
    IncompatibleForecast,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
