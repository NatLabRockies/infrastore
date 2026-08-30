//! Typed Rust client for the read-only gRPC service.
//!
//! [`RemoteClient`] mirrors the read methods of [`infrastore_core::Store`]
//! over the wire. Construct it with [`RemoteClient::connect`]; a unifying
//! `Store`/client trait is deliberately out of scope (the store is sync, the
//! client async).

use chrono::{DateTime, Utc};
use infrastore_core::{
    ForecastSummaryRow, OwnerCategory, Period, Result as CoreResult, StaticConsistency,
    StaticSummaryRow, TimeRange, TimeSeriesCountsDetailed, TimeSeriesData, TimeSeriesError,
    TimeSeriesMetadata, TimeSeriesType,
};

/// Parse an ISO-8601 period received over the wire, mapping failures to a
/// connection error (the server is the source of truth for the encoding).
fn iso_to_period(s: &str) -> CoreResult<Period> {
    Period::from_iso8601(s).map_err(|e| TimeSeriesError::ConnectionError(e.to_string()))
}

fn opt_iso_to_period(s: Option<String>) -> CoreResult<Option<Period>> {
    s.filter(|s| !s.is_empty())
        .map(|s| iso_to_period(&s))
        .transpose()
}

/// Map a wire-conversion error to an integrity error (the server is the source
/// of truth for the encoding).
fn convert_err(e: impl std::fmt::Display) -> TimeSeriesError {
    TimeSeriesError::IntegrityError(format!("convert: {e}"))
}

/// Build a `ListReq` from the typed filter params shared by `list_time_series`
/// and the metadata listings.
#[allow(clippy::too_many_arguments)]
fn build_list_req(
    owner_id: Option<i64>,
    owner_category: Option<OwnerCategory>,
    owner_type: Option<String>,
    time_series_type: Option<TimeSeriesType>,
    name: Option<String>,
    component_field: Option<String>,
    zoneless: Option<bool>,
    resolution: Option<Period>,
    interval: Option<Period>,
    features: Option<&infrastore_core::Features>,
) -> ListReq {
    ListReq {
        owner_id,
        owner_category: owner_category.map(|c| pb::OwnerCategory::from(c) as i32),
        owner_type,
        time_series_type: time_series_type.map(|t| pb::TimeSeriesType::from(t) as i32),
        name,
        component_field,
        zoneless,
        resolution: resolution.map(|p| p.to_iso8601()),
        interval: interval.map(|p| p.to_iso8601()),
        features: features.map(features_to_pb),
    }
}
use infrastore_proto::convert::{
    features_to_pb, forecast_summary_row_from_pb, get_resp_to_time_series_data, metadata_from_pb,
    requested_type_to_pb, static_summary_row_from_pb,
};
use infrastore_proto::pb::{
    self, BulkReadReq, ConsistencyReq, CountsReq, EmptyReq, ForecastParamsReq, GetReq, HasReq,
    IntervalsReq, ListOwnerIdsReq, ListReq, ResolutionsReq, VerifyReq,
    catalog_store_client::CatalogStoreClient,
};
use tokio::sync::Mutex;
use tonic::transport::Channel;

/// Read-only client wrapping a tonic-generated client. All methods translate
/// gRPC `Status` errors back into [`TimeSeriesError::ConnectionError`] so
/// callers don't need to know whether the store is local or remote.
pub struct RemoteClient {
    inner: Mutex<CatalogStoreClient<Channel>>,
}

impl RemoteClient {
    pub async fn connect(addr: String) -> CoreResult<Self> {
        let channel = Channel::from_shared(addr.clone())
            .map_err(|e| TimeSeriesError::ConnectionError(format!("invalid uri {addr}: {e}")))?
            .connect()
            .await
            .map_err(|e| TimeSeriesError::ConnectionError(format!("{addr}: {e}")))?;
        Ok(Self::from_channel(channel))
    }

    pub fn from_channel(channel: Channel) -> Self {
        Self {
            inner: Mutex::new(CatalogStoreClient::new(channel)),
        }
    }

    fn map_status(s: tonic::Status) -> TimeSeriesError {
        match s.code() {
            tonic::Code::NotFound => TimeSeriesError::NotFound,
            tonic::Code::AlreadyExists => TimeSeriesError::DuplicateTimeSeries,
            tonic::Code::InvalidArgument => {
                TimeSeriesError::InvalidParameter(s.message().to_string())
            }
            tonic::Code::DataLoss => TimeSeriesError::IntegrityError(s.message().to_string()),
            tonic::Code::FailedPrecondition => {
                TimeSeriesError::InvalidParameter(s.message().to_string())
            }
            _ => TimeSeriesError::ConnectionError(format!("{}: {}", s.code(), s.message())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_time_series(
        &self,
        owner_id: Option<i64>,
        owner_category: Option<OwnerCategory>,
        owner_type: Option<String>,
        time_series_type: Option<TimeSeriesType>,
        name: Option<String>,
        component_field: Option<String>,
        // Coherence predicate on the timestamp spelling; see
        // `infrastore_core::ListFilter::zoneless`.
        zoneless: Option<bool>,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: Option<&infrastore_core::Features>,
    ) -> CoreResult<Vec<TimeSeriesMetadata>> {
        let req = build_list_req(
            owner_id,
            owner_category,
            owner_type,
            time_series_type,
            name,
            component_field,
            zoneless,
            resolution,
            interval,
            features,
        );
        let mut inner = self.inner.lock().await;
        let resp = inner
            .list_time_series(req)
            .await
            .map_err(Self::map_status)?
            .into_inner();
        let mut out = Vec::with_capacity(resp.metadata.len());
        for m in resp.metadata {
            out.push(
                metadata_from_pb(m).map_err(|e| {
                    TimeSeriesError::IntegrityError(format!("metadata convert: {e}"))
                })?,
            );
        }
        Ok(out)
    }

    pub async fn get_time_series(
        &self,
        id: i64,
        time_range: Option<TimeRange>,
    ) -> CoreResult<TimeSeriesData> {
        let (start, end) = match time_range {
            Some(r) => (Some(r.start.to_rfc3339()), Some(r.end.to_rfc3339())),
            None => (None, None),
        };
        let req = GetReq {
            id,
            start_rfc3339: start,
            end_rfc3339: end,
            // The wire form is RFC3339 either way; this is what carries the
            // spelling, so the server can apply the same bound rule a local
            // read would.
            bounds_zoneless: time_range.map(|r| r.zoneless),
        };
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_time_series(req)
            .await
            .map_err(Self::map_status)?
            .into_inner();
        get_resp_to_time_series_data(resp, String::new())
            .map_err(|e| TimeSeriesError::IntegrityError(format!("get convert: {e}")))
    }


    pub async fn get_resolutions(
        &self,
        time_series_type: Option<TimeSeriesType>,
    ) -> CoreResult<Vec<Period>> {
        let req = ResolutionsReq {
            time_series_type: time_series_type.map(|t| pb::TimeSeriesType::from(t) as i32),
        };
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_resolutions(req)
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.resolution.iter().map(|s| iso_to_period(s)).collect()
    }

    pub async fn get_counts(&self) -> CoreResult<infrastore_core::TimeSeriesCounts> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_counts(CountsReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(infrastore_core::TimeSeriesCounts {
            components_with_time_series: resp.components_with_time_series,
            static_time_series: resp.static_time_series,
            forecasts: resp.forecasts,
        })
    }

    pub async fn get_forecast_parameters(
        &self,
        resolution: Option<Period>,
        interval: Option<Period>,
    ) -> CoreResult<infrastore_core::ForecastParameters> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_forecast_parameters(ForecastParamsReq {
                resolution: resolution.map(|p| p.to_iso8601()),
                interval: interval.map(|p| p.to_iso8601()),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        let initial_timestamp = match resp.initial_timestamp_rfc3339 {
            Some(s) => Some(
                DateTime::parse_from_rfc3339(&s)
                    .map_err(|e| TimeSeriesError::ConnectionError(e.to_string()))?
                    .with_timezone(&Utc),
            ),
            None => None,
        };
        Ok(infrastore_core::ForecastParameters {
            horizon: opt_iso_to_period(resp.horizon)?,
            interval: opt_iso_to_period(resp.interval)?,
            count: resp.count.map(|c| c as usize),
            resolution: opt_iso_to_period(resp.resolution)?,
            initial_timestamp,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn has_time_series(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
        name: &str,
        time_series_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: infrastore_core::Features,
    ) -> CoreResult<bool> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .has_time_series(HasReq {
                owner_id,
                owner_category: pb::OwnerCategory::from(owner_category) as i32,
                name: name.to_string(),
                time_series_type: time_series_type.map(|t| requested_type_to_pb(t) as i32),
                resolution: resolution.map(|p| p.to_iso8601()),
                interval: interval.map(|p| p.to_iso8601()),
                features: features_to_pb(&features).entries,
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(resp.present)
    }

    pub async fn verify_integrity(&self) -> CoreResult<infrastore_core::storage::IntegrityReport> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .verify_integrity(VerifyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(infrastore_core::storage::IntegrityReport {
            errors: resp.errors,
        })
    }

    // ---- Additive read RPCs (Phase 4.4) ----

    /// List full keys matching the filter, each paired with the array content
    /// hash when `with_hash` is set (`None` otherwise).
    #[allow(clippy::too_many_arguments)]

    /// Full metadata record for one association id.
    pub async fn get_metadata(&self, id: i64) -> CoreResult<TimeSeriesMetadata> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_metadata(pb::IdReq { id })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        metadata_from_pb(resp).map_err(convert_err)
    }

    /// Read several series at once, optionally time-sliced.
    pub async fn bulk_read(
        &self,
        ids: &[i64],
        time_range: Option<TimeRange>,
    ) -> CoreResult<Vec<TimeSeriesData>> {
        let (start_rfc3339, end_rfc3339) = match time_range {
            Some(r) => (Some(r.start.to_rfc3339()), Some(r.end.to_rfc3339())),
            None => (None, None),
        };
        let mut inner = self.inner.lock().await;
        let resp = inner
            .bulk_read(BulkReadReq {
                ids: ids.to_vec(),
                start_rfc3339,
                end_rfc3339,
                bounds_zoneless: time_range.map(|r| r.zoneless),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.items
            .into_iter()
            .map(|item| get_resp_to_time_series_data(item, String::new()).map_err(convert_err))
            .collect()
    }

    /// Distinct owners per category and distinct arrays per kind.
    pub async fn time_series_counts_detailed(&self) -> CoreResult<TimeSeriesCountsDetailed> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_detailed_counts(EmptyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(TimeSeriesCountsDetailed {
            components_with_time_series: resp.components_with_time_series,
            supplemental_attributes_with_time_series: resp.supplemental_attributes_with_time_series,
            static_time_series_count: resp.static_time_series_count,
            forecast_count: resp.forecast_count,
        })
    }

    /// Association count grouped by time series type.
    pub async fn counts_by_type(&self) -> CoreResult<Vec<(TimeSeriesType, i64)>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_counts_by_type(EmptyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.entries
            .into_iter()
            .map(|e| {
                let t = pb::TimeSeriesType::try_from(e.time_series_type)
                    .map(TimeSeriesType::from)
                    .map_err(|_| convert_err("unknown time_series_type"))?;
                Ok((t, e.count))
            })
            .collect()
    }

    /// Distinct owner ids of `category` with a time series, optionally scoped.
    pub async fn list_owner_ids(
        &self,
        category: OwnerCategory,
        time_series_type: Option<TimeSeriesType>,
        resolution: Option<Period>,
    ) -> CoreResult<Vec<i64>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .list_owner_ids(ListOwnerIdsReq {
                owner_category: pb::OwnerCategory::from(category) as i32,
                time_series_type: time_series_type.map(|t| pb::TimeSeriesType::from(t) as i32),
                resolution: resolution.map(|p| p.to_iso8601()),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(resp.owner_id)
    }

    /// Distinct forecast intervals, optionally scoped to one type.
    pub async fn get_intervals(
        &self,
        time_series_type: Option<TimeSeriesType>,
    ) -> CoreResult<Vec<Period>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_intervals(IntervalsReq {
                time_series_type: time_series_type.map(|t| pb::TimeSeriesType::from(t) as i32),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.interval.iter().map(|s| iso_to_period(s)).collect()
    }

    /// Grouped static-series summary.
    pub async fn static_summary(&self) -> CoreResult<Vec<StaticSummaryRow>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_static_summary(EmptyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.rows
            .into_iter()
            .map(|r| static_summary_row_from_pb(r).map_err(convert_err))
            .collect()
    }

    /// Grouped forecast summary.
    pub async fn forecast_summary(&self) -> CoreResult<Vec<ForecastSummaryRow>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_forecast_summary(EmptyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.rows
            .into_iter()
            .map(|r| forecast_summary_row_from_pb(r).map_err(convert_err))
            .collect()
    }

    /// Per-resolution static-grid consistency rows (errors on divergence).
    pub async fn check_static_consistency(
        &self,
        resolution: Option<Period>,
    ) -> CoreResult<Vec<StaticConsistency>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .check_static_consistency(ConsistencyReq {
                resolution: resolution.map(|p| p.to_iso8601()),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        resp.rows
            .into_iter()
            .map(|r| {
                Ok(StaticConsistency {
                    resolution: iso_to_period(&r.resolution)?,
                    initial_timestamp: DateTime::parse_from_rfc3339(&r.initial_timestamp_rfc3339)
                        .map_err(convert_err)?
                        .with_timezone(&Utc),
                    length: r.length as usize,
                })
            })
            .collect()
    }

    /// Resolve a forecast addressed by attributes plus a requested type.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_metadata(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
        name: &str,
        resolution: Option<Period>,
        interval: Option<Period>,
        features: infrastore_core::Features,
        requested: TimeSeriesType,
    ) -> CoreResult<TimeSeriesMetadata> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .resolve_metadata(pb::ResolveMetadataReq {
                owner_id,
                owner_category: pb::OwnerCategory::from(owner_category) as i32,
                name: name.to_string(),
                resolution: resolution.map(|p| p.to_iso8601()),
                interval: interval.map(|p| p.to_iso8601()),
                features: Some(features_to_pb(&features)),
                requested: Some(requested_type_to_pb(requested)),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        metadata_from_pb(resp).map_err(convert_err)
    }
}
