//! Resolving a stored time series from CLI selector flags.

use infrastore_core::{Features, ListFilter, Store, TimeSeriesMetadata};

use crate::fields;
use crate::parse;

/// How many candidates the ambiguous-selector error spells out before
/// summarizing the rest. Unbounded, this printed one line per match — 715 lines
/// of stderr on a store with 5000 series, which buries the message that matters.
const AMBIGUITY_LIST_MAX: usize = 10;

/// Which coherence group a selection is narrowed to.
///
/// The two groups never mix in one grid or one bulk read — there is no
/// timestamp axis that is true of a wall clock and an instant at once — so the
/// core refuses a selection spanning both. This is the constructive half of
/// that rule, and the CLI surface for `ListFilter::zoneless`.
///
/// Unrelated to the global `--zoneless`, which says how *incoming* timestamps
/// are to be read; this filters what is already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Spelling {
    /// Series that record instants: `utc`, a fixed offset, a named zone, or no
    /// declared reference at all.
    Zoned,
    /// Series whose timestamps are wall clocks (`time_reference = zoneless`).
    Zoneless,
}

/// Flags that narrow a query down to (ideally) a single stored series.
#[derive(Debug, Clone, clap::Args)]
pub struct SelectorArgs {
    /// Catalog association ID, as reported by `add`, `list`, and `info`.
    ///
    /// A point lookup: it names exactly one row, so it cannot be combined with
    /// the narrowing flags below, and it is not a filter — the commands that
    /// operate over a *set* of series take those flags instead.
    #[arg(long)]
    pub id: Option<i64>,
    /// Owner ID of the time series.
    #[arg(long)]
    pub owner_id: Option<i64>,
    /// Owner category (Component|SupplementalAttribute).
    #[arg(long)]
    pub owner_category: Option<String>,
    /// Time series name.
    #[arg(long)]
    pub name: Option<String>,
    /// Name pattern (SQLite GLOB: case-sensitive, `*`/`?` wildcards). ANDed
    /// with --name when both are given.
    #[arg(long)]
    pub name_glob: Option<String>,
    /// Owning component's field the values vary over time, e.g.
    /// max_active_power. Exact and case-sensitive; a series that declares no
    /// component_field matches no value.
    #[arg(long)]
    pub component_field: Option<String>,
    /// Time series type. `deterministic` also matches the
    /// DeterministicSingleTimeSeries rows that `transform` produces.
    #[arg(
        long = "type",
        value_name = "TYPE",
        long_help = "Time series type. One of:\n  \
                     SingleTimeSeries, NonSequentialTimeSeries, Deterministic,\n  \
                     DeterministicSingleTimeSeries, Probabilistic, Scenarios\n\
                     The lowercase short forms (single, non_sequential,\n\
                     deterministic_single, ...) are accepted too.\n\
                     `deterministic` also matches the DeterministicSingleTimeSeries rows\n\
                     that `transform` produces (they list with their own type); use\n\
                     `deterministic_single` to select only those."
    )]
    pub ts_type: Option<String>,
    /// Resolution as an ISO-8601 duration, e.g. PT1H or PT15M.
    #[arg(long)]
    pub resolution: Option<String>,
    /// Feature filter, repeatable: key=value.
    #[arg(long = "feature", value_name = "KEY=VALUE")]
    pub feature: Vec<String>,
    /// How the stored timestamps are spelled: `zoned` (they name instants) or
    /// `zoneless` (wall clocks). Splits a store holding both into a selection
    /// `grid` and the bulk reads can act on, which they refuse for a mix.
    /// Nothing to do with the global --zoneless, which is about input.
    #[arg(long, value_name = "SPELLING")]
    pub spelling: Option<Spelling>,
}

impl SelectorArgs {
    /// Build a [`ListFilter`] from the provided flags.
    /// Whether any flag other than `--id` narrows the selection.
    fn narrows_further(&self) -> bool {
        self.owner_id.is_some()
            || self.owner_category.is_some()
            || self.name.is_some()
            || self.name_glob.is_some()
            || self.component_field.is_some()
            || self.ts_type.is_some()
            || self.resolution.is_some()
            || self.spelling.is_some()
            || !self.feature.is_empty()
    }

    pub fn to_filter(&self) -> Result<ListFilter, String> {
        // `--id` is a primary-key lookup, not a predicate. The core deliberately
        // has no id filter — one would invite scan-shaped use of a point lookup
        // — so a command that works over a matching *set* cannot honor it.
        if self.id.is_some() {
            return Err(
                "--id names exactly one association, so it cannot select a set here; \
                 use it with `info`/`get`, or narrow this command with \
                 --owner-id/--name/--name-glob/--type instead"
                    .to_string(),
            );
        }
        let mut filter = ListFilter::new();
        if let Some(u) = self.owner_id {
            filter = filter.owner_id(u);
        }
        if let Some(c) = &self.owner_category {
            filter = filter.owner_category(parse::parse_owner_category(c)?);
        }
        if let Some(n) = &self.name {
            filter = filter.name(n.clone());
        }
        if let Some(g) = &self.name_glob {
            filter = filter.name_glob(g.clone());
        }
        if let Some(f) = &self.component_field {
            filter = filter.component_field(f.clone());
        }
        if let Some(t) = &self.ts_type {
            filter = filter.time_series_type(parse::parse_ts_type(t)?);
        }
        if let Some(r) = &self.resolution {
            filter = filter.resolution(parse::parse_period(r)?);
        }
        if let Some(s) = self.spelling {
            filter = filter.zoneless(s == Spelling::Zoneless);
        }
        if !self.feature.is_empty() {
            let mut features = Features::new();
            for pair in &self.feature {
                let (k, v) = parse::parse_feature_kv(pair)?;
                features.insert(k, v);
            }
            filter = filter.features(features);
        }
        Ok(filter)
    }

    /// Resolve to exactly one stored series, returning its metadata and key.
    /// Errors with a helpful list when zero or multiple series match.
    pub fn resolve(&self, store: &Store) -> Result<(TimeSeriesMetadata, i64), String> {
        if let Some(id) = self.id {
            if self.narrows_further() {
                return Err(
                    "--id already names exactly one association; drop the other selector flags"
                        .to_string(),
                );
            }
            let meta = store
                .get_metadata_by_id(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!(
                        "no association has id {id}. Ids are never reissued, so one that stops \
                         resolving stays stale rather than coming to name a different series"
                    )
                })?;
            let key = id_of(&meta)?;
            return Ok((meta, key));
        }
        let mut matches = store
            .list_time_series(self.to_filter()?)
            .map_err(|e| e.to_string())?;
        match matches.len() {
            0 => Err("no time series matched the selector".to_string()),
            1 => {
                let meta = matches.remove(0);
                let key = id_of(&meta)?;
                Ok((meta, key))
            }
            n => {
                let mut msg = format!(
                    "{n} time series matched; narrow with \
                     --owner-id/--name/--name-glob/--type/--resolution/--feature:\n"
                );
                for m in matches.iter().take(AMBIGUITY_LIST_MAX) {
                    msg.push_str(&format!("  - {}\n", fields::identity_line(m)));
                }
                if n > AMBIGUITY_LIST_MAX {
                    msg.push_str(&format!(
                        "  ... and {} more; run `list` with the same flags to see them all\n",
                        n - AMBIGUITY_LIST_MAX
                    ));
                }
                Err(msg)
            }
        }
    }
}

/// The exact-identity filter for one row: every identity field pinned, and the
/// feature set matched whole rather than as a subset. What `--replace` poses its
/// "is this already here?" question with.
pub fn exact_filter(meta: &TimeSeriesMetadata) -> infrastore_core::ListFilter {
    infrastore_core::ListFilter {
        owner_id: Some(meta.owner_id),
        owner_category: Some(meta.owner_category),
        time_series_type: Some(meta.time_series_type),
        name: Some(meta.name.clone()),
        resolution: meta.resolution,
        interval: meta.interval,
        features: Some(meta.features.clone()),
        features_exact: true,
        ..Default::default()
    }
}

/// The catalog id of a selected row — how every read and removal addresses it.
pub fn id_of(meta: &TimeSeriesMetadata) -> Result<i64, String> {
    meta.id
        .ok_or_else(|| format!("row {:?} carries no catalog id", meta.name))
}
