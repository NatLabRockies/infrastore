//! Resolving a stored time series from CLI selector flags.

use infrastore_core::{Features, KeyIdentity, ListFilter, Store, TimeSeriesMetadata};

use crate::fields;
use crate::parse;

/// How many candidates the ambiguous-selector error spells out before
/// summarizing the rest. Unbounded, this printed one line per match — 715 lines
/// of stderr on a store with 5000 series, which buries the message that matters.
const AMBIGUITY_LIST_MAX: usize = 10;

/// Flags that narrow a query down to (ideally) a single stored series.
#[derive(Debug, Clone, clap::Args)]
pub struct SelectorArgs {
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
}

impl SelectorArgs {
    /// Build a [`ListFilter`] from the provided flags.
    pub fn to_filter(&self) -> Result<ListFilter, String> {
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
    pub fn resolve(&self, store: &Store) -> Result<(TimeSeriesMetadata, KeyIdentity), String> {
        let mut matches = store
            .list_time_series(self.to_filter()?)
            .map_err(|e| e.to_string())?;
        match matches.len() {
            0 => Err("no time series matched the selector".to_string()),
            1 => {
                let meta = matches.remove(0);
                let key = key_of(&meta);
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

/// Reconstruct the lookup key (identity) from a metadata record.
pub fn key_of(meta: &TimeSeriesMetadata) -> KeyIdentity {
    KeyIdentity {
        owner_id: meta.owner_id,
        owner_category: meta.owner_category,
        time_series_type: meta.time_series_type,
        name: meta.name.clone(),
        resolution: meta.resolution,
        interval: meta.interval,
        features: meta.features.clone(),
    }
}
