//! Resolving a stored time series from CLI selector flags.

use time_series_store_core::{Features, KeyIdentity, ListFilter, Store, TimeSeriesMetadata};

use crate::parse;

/// Flags that narrow a query down to (ideally) a single stored series.
#[derive(Debug, Clone, clap::Args)]
pub struct SelectorArgs {
    /// Owner ID of the time series.
    #[arg(long)]
    pub owner_id: Option<i64>,
    /// Owner category (component|supplemental_attribute).
    #[arg(long)]
    pub owner_category: Option<String>,
    /// Time series name.
    #[arg(long)]
    pub name: Option<String>,
    /// Time series type (single|non_sequential|deterministic|probabilistic|scenarios).
    #[arg(long = "type")]
    pub ts_type: Option<String>,
    /// Resolution, e.g. 1h or 15min.
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
        if let Some(t) = &self.ts_type {
            filter = filter.time_series_type(parse::parse_ts_type(t)?);
        }
        if let Some(r) = &self.resolution {
            filter = filter.resolution(parse::parse_duration(r)?);
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
                    "{n} time series matched; narrow with --name/--type/--resolution/--feature:\n"
                );
                for m in &matches {
                    msg.push_str(&format!(
                        "  - owner={} owner_category={} type={} name={} resolution={}\n",
                        m.owner_id,
                        m.owner_category.as_str(),
                        m.time_series_type.as_str(),
                        m.name,
                        m.resolution
                            .map(parse::format_duration)
                            .unwrap_or_else(|| "-".to_string()),
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
        features: meta.features.clone(),
    }
}
