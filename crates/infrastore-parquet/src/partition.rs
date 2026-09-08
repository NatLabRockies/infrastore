//! What decides which file a series' rows go in, and what that file is called.
//!
//! A partition holds many series, and three things cannot vary within one
//! Parquet file without nullable or ill-typed columns: the **set of key
//! columns** (a forecast has an `issue_time`, a static series does not), the
//! **Arrow type of `value`**, and the **zone of `timestamp`**. So a selection is
//! partitioned by the triple `(time_series_type, value type, time_reference)`
//! and one file *pair* is written per distinct triple. Every column in either
//! file is then required — there are no nullable columns anywhere in this
//! format.
//!
//! The three keys are also written as ordinary columns of the series file. They
//! are constant within a partition, so they dictionary-encode to one entry and
//! cost essentially nothing, and a reader that unions a directory into one
//! relation still sees them.

use std::collections::{BTreeMap, BTreeSet};

use infrastore_core::{Dtype, ElementType, TimeReference, TimeSeriesType};

/// The `value` column's identity: what decides its Arrow type.
///
/// Composite kinds are keyed by **kind alone**, not by width. Their stored width
/// `w` varies per series — a `piecewise_linear` row is `[n, x1, y1, …]`
/// zero-padded to the widest timestep in *that* series — so keying by width
/// would scatter one kind across a file per width. Instead every composite row
/// in a file is re-padded to the widest series in it, which the layout allows:
/// the leading count `n` keeps each row self-describing whatever the padding.
/// The cost is a `data_hash` caveat, in [`crate::canonical`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueKind {
    /// A dtype plus the per-step element shape, empty for a plain scalar.
    Dense { dtype: Dtype, shape: Vec<usize> },
    /// A fixed-arity homogeneous tuple.
    Tuple { arity: usize, dtype: Dtype },
    /// One of the four function-data kinds, keyed without its width.
    Composite(ElementType),
}

impl ValueKind {
    /// The kind a stored row belongs to.
    pub fn of(element_type: ElementType, element_shape: &[usize]) -> Self {
        match element_type {
            ElementType::Scalar(dtype) => ValueKind::Dense {
                dtype,
                shape: element_shape.to_vec(),
            },
            ElementType::Tuple { arity, dtype } => ValueKind::Tuple { arity, dtype },
            composite => ValueKind::Composite(composite),
        }
    }

    /// Whether rows of this kind may need re-padding to share a file.
    pub fn is_composite(&self) -> bool {
        matches!(self, ValueKind::Composite(_))
    }

    /// The element type a file of this kind writes in its footer, given the
    /// width the file settled on.
    ///
    /// For a composite kind the width is the file's, not any one series', which
    /// is why it is a parameter rather than a field.
    pub fn element_type(&self) -> ElementType {
        match self {
            ValueKind::Dense { dtype, .. } => ElementType::Scalar(*dtype),
            ValueKind::Tuple { arity, dtype } => ElementType::Tuple {
                arity: *arity,
                dtype: *dtype,
            },
            ValueKind::Composite(kind) => *kind,
        }
    }

    /// The filename fragment for this kind.
    ///
    /// Shapes join with `x` (`f64_2x3`) and a tuple keeps its arity next to its
    /// dtype (`tuple3_f64`), because the canonical spellings — `tuple(3,f64)`,
    /// `[2, 3]` — carry `(`, `,` and spaces, which are either forbidden or
    /// merely awful in a filename.
    pub fn slug(&self) -> String {
        match self {
            ValueKind::Dense { dtype, shape } if shape.is_empty() => dtype.as_str().to_string(),
            ValueKind::Dense { dtype, shape } => format!(
                "{}_{}",
                dtype.as_str(),
                shape
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join("x")
            ),
            ValueKind::Tuple { arity, dtype } => format!("tuple{arity}_{}", dtype.as_str()),
            ValueKind::Composite(kind) => kind.to_string(),
        }
    }
}

/// One partition: everything that must be constant within a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionKey {
    pub time_series_type: TimeSeriesType,
    pub value_kind: ValueKind,
    /// `None` is *unspecified*, which is a partition of its own — a series that
    /// declared no spelling must not be pooled with one that declared UTC, even
    /// though both write a UTC-zoned column.
    pub time_reference: Option<TimeReference>,
}

/// Ordering is over a rendered form rather than derived, because neither
/// `TimeSeriesType` nor `TimeReference` is `Ord` — and neither should become one
/// for this crate's convenience, since neither has a meaningful order of its
/// own. What is needed here is only *a* total order, so that a re-run of the
/// same export names its files the same way.
///
/// The rendering must be **exact**, and agree with `Eq`: the zones `a/b` and
/// `a_b` are different partitions that want different timestamp columns, and an
/// order that flattened them the way the filename slug does would let a
/// `BTreeMap` pool them into one file. So the reference is keyed by its variant
/// and its storage string, and only the filename is sanitized.
impl PartitionKey {
    fn sort_key(&self) -> (i64, &ValueKind, Option<(u8, String)>) {
        let reference = self.time_reference.as_ref().map(|r| {
            let tag = match r {
                TimeReference::Utc => 0,
                TimeReference::FixedOffset(_) => 1,
                TimeReference::Zone(_) => 2,
                TimeReference::Zoneless => 3,
            };
            (tag, r.as_storage_string())
        });
        (self.time_series_type.code(), &self.value_kind, reference)
    }
}

impl Ord for PartitionKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for PartitionKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartitionKey {
    /// `<type>.<value-slug>.<reference-slug>` — the **stem** the partition's two
    /// files share.
    ///
    /// A partition is two files, `<stem>.values.parquet` and
    /// `<stem>.series.parquet`, and sharing a stem is how the import pairs them.
    ///
    /// A **convenience, not the truth**: the slug function is one-way (two zone
    /// names differing only in a character the slug flattens produce the same
    /// text), so the footer carries the exact key and [`disambiguate`] is what
    /// guarantees two partitions never land on one stem.
    pub fn stem(&self) -> String {
        format!(
            "{}.{}.{}",
            sanitize(self.time_series_type.as_str()),
            sanitize(&self.value_kind.slug()),
            sanitize(&reference_slug(self.time_reference.as_ref())),
        )
    }
}

/// The suffix of the file holding every distinct array in a partition, once.
pub const VALUES_SUFFIX: &str = ".values.parquet";
/// The suffix of the file holding one catalog row per series.
pub const SERIES_SUFFIX: &str = ".series.parquet";

/// `<stem>.values.parquet`.
pub fn values_name(stem: &str) -> String {
    format!("{stem}{VALUES_SUFFIX}")
}

/// `<stem>.series.parquet`.
pub fn series_name(stem: &str) -> String {
    format!("{stem}{SERIES_SUFFIX}")
}

/// The stem a partition file belongs to, whichever half it is.
///
/// `None` for a `.parquet` file that is neither half — a foreign file, which the
/// import reads as a values file with no series file beside it.
pub fn stem_of(file_name: &str) -> Option<&str> {
    file_name
        .strip_suffix(VALUES_SUFFIX)
        .or_else(|| file_name.strip_suffix(SERIES_SUFFIX))
}

/// The filename fragment for a time reference.
///
/// `Utc` and `Zoneless` use their storage literals, an unset reference uses the
/// same `unspecified` literal the `time_reference` column does, a zone uses its
/// IANA name, and an offset is spelled out in words — `-07:00` would carry both
/// a `:`, which Windows forbids outright, and a leading `-`, which reads as a
/// flag anywhere the name is passed to a command.
pub fn reference_slug(reference: Option<&TimeReference>) -> String {
    match reference {
        None => crate::schema::UNSPECIFIED_REFERENCE.to_string(),
        Some(TimeReference::Utc) => "utc".to_string(),
        Some(TimeReference::Zoneless) => "zoneless".to_string(),
        Some(TimeReference::Zone(name)) => name.clone(),
        Some(TimeReference::FixedOffset(minutes)) => {
            let sign = if *minutes < 0 { "minus" } else { "plus" };
            let abs = minutes.unsigned_abs();
            format!("offset_{sign}{:02}_{:02}", abs / 60, abs % 60)
        }
    }
}

/// Characters Windows forbids in a filename, plus the ones a shell or this
/// project's own spellings would make awkward.
///
/// `/` is the obvious one — an IANA zone is `America/Denver` — and `:` is next,
/// from a fixed offset. The rest of Windows' set (`<>"\|?*`) cannot appear in
/// anything this project produces, and is refused anyway rather than trusted not
/// to arrive: a zone name is free-form text the core validates only the *shape*
/// of, so a store can genuinely hold `Zone("a|b")`.
fn is_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Windows treats these as device names whatever the extension, so `CON.parquet`
/// cannot be created there. Compared case-insensitively against the stem.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Map one fragment onto the filename-safe alphabet.
///
/// Deliberately **not** reversible: an escape scheme that survived a round trip
/// would need characters that are themselves awkward, and nothing reads the
/// partition back out of the name — the footer carries it. What this must
/// guarantee is only that the result is a legal, non-empty path component on
/// Linux, macOS, and Windows.
pub fn sanitize(fragment: &str) -> String {
    let mapped: String = fragment
        .chars()
        .map(|c| if is_safe(c) { c } else { '_' })
        .collect();
    // Leading and trailing dots go: Windows silently strips a trailing one (so
    // `a.` and `a` are the same file there, which would make two partitions
    // collide invisibly), and a leading one makes a hidden file on Unix. Dots
    // inside a fragment are harmless and kept.
    let bare = mapped.trim_matches('.');
    // A fragment that is empty or a Windows device name is not a usable path
    // segment whatever the extension -- `CON.parquet` cannot be created there --
    // so give it a stem that is.
    if bare.is_empty()
        || RESERVED_DEVICE_NAMES
            .iter()
            .any(|d| bare.eq_ignore_ascii_case(d))
    {
        return format!("x_{bare}");
    }
    bare.to_string()
}

/// Assign every partition a distinct stem.
///
/// [`sanitize`] is many-to-one — the zones `a/b` and `a_b` both flatten to `a_b`
/// — so two partitions really can want one stem, and the second would silently
/// overwrite the first's two files. Collisions get a numeric suffix, in the
/// keys' own sort order so a re-run of the same export produces the same names.
pub fn disambiguate(keys: &[PartitionKey]) -> BTreeMap<PartitionKey, String> {
    let mut sorted: Vec<&PartitionKey> = keys.iter().collect();
    sorted.sort();
    sorted.dedup();

    // Every stem handed out, case-folded because macOS and Windows filesystems
    // treat two names differing only in case as one path. The suffixed stems are
    // tracked too: a key that naturally slugs to `foo_2` must not land on the
    // stem a collision was just moved to.
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut out = BTreeMap::new();
    for key in sorted {
        let base = key.stem();
        let mut candidate = base.clone();
        let mut n = 1usize;
        while taken.contains(&candidate.to_lowercase()) {
            n += 1;
            candidate = format!("{base}_{n}");
        }
        taken.insert(candidate.to_lowercase());
        out.insert(key.clone(), candidate);
    }
    out
}
