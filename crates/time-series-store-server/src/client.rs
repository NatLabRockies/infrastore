//! Typed Rust client for the read-only gRPC service.
//!
//! Mirrors the read methods of [`time_series_store_core::Store`] over the wire.
//! Returned by `time_series_store_core::connect()` once that thin wrapper is
//! exposed (via `RemoteStore`).

use chrono::{DateTime, Utc};
use time_series_store_core::{
    KeyIdentity, OwnerCategory, Period, Result as CoreResult, TimeSeriesData, TimeSeriesError,
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
use time_series_store_proto::convert::{
    features_to_pb, get_resp_to_time_series_data, key_from_pb, key_to_pb, metadata_from_pb,
};
use time_series_store_proto::pb::{
    self, CountsReq, ForecastParamsReq, GetReq, HasReq, KeysReq, ListReq, ResolutionsReq,
    VerifyReq, time_series_store_client::TimeSeriesStoreClient,
};
use tokio::sync::Mutex;
use tonic::transport::Channel;

/// Read-only client wrapping a tonic-generated client. All methods translate
/// gRPC `Status` errors back into [`TimeSeriesError::ConnectionError`] so
/// callers don't need to know whether the store is local or remote.
pub struct RemoteClient {
    inner: Mutex<TimeSeriesStoreClient<Channel>>,
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
            inner: Mutex::new(TimeSeriesStoreClient::new(channel)),
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
        resolution: Option<Period>,
        features: Option<&time_series_store_core::Features>,
    ) -> CoreResult<Vec<TimeSeriesMetadata>> {
        let req = ListReq {
            owner_id,
            owner_category: owner_category.map(|c| pb::OwnerCategory::from(c) as i32),
            owner_type,
            time_series_type: time_series_type.map(|t| pb::TimeSeriesType::from(t) as i32),
            name,
            resolution: resolution.map(|p| p.to_iso8601()),
            features: features.map(features_to_pb),
        };
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
        key: &KeyIdentity,
        time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> CoreResult<TimeSeriesData> {
        let (start, end) = match time_range {
            Some((s, e)) => (Some(s.to_rfc3339()), Some(e.to_rfc3339())),
            None => (None, None),
        };
        let req = GetReq {
            key: Some(key_to_pb(key)),
            start_rfc3339: start,
            end_rfc3339: end,
        };
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_time_series(req)
            .await
            .map_err(Self::map_status)?
            .into_inner();
        get_resp_to_time_series_data(resp, key.name.clone())
            .map_err(|e| TimeSeriesError::IntegrityError(format!("get convert: {e}")))
    }

    pub async fn get_time_series_keys(
        &self,
        owner_id: i64,
        owner_category: OwnerCategory,
    ) -> CoreResult<Vec<KeyIdentity>> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_time_series_keys(KeysReq {
                owner_id,
                owner_category: pb::OwnerCategory::from(owner_category) as i32,
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        let mut out = Vec::with_capacity(resp.keys.len());
        for k in resp.keys {
            out.push(
                key_from_pb(k)
                    .map_err(|e| TimeSeriesError::IntegrityError(format!("key convert: {e}")))?,
            );
        }
        Ok(out)
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

    pub async fn get_counts(&self) -> CoreResult<time_series_store_core::TimeSeriesCounts> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_counts(CountsReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(time_series_store_core::TimeSeriesCounts {
            components_with_time_series: resp.components_with_time_series,
            static_time_series: resp.static_time_series,
            forecasts: resp.forecasts,
        })
    }

    pub async fn get_forecast_parameters(
        &self,
    ) -> CoreResult<time_series_store_core::ForecastParameters> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .get_forecast_parameters(ForecastParamsReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(time_series_store_core::ForecastParameters {
            horizon: opt_iso_to_period(resp.horizon)?,
            interval: opt_iso_to_period(resp.interval)?,
            count: resp.count.map(|c| c as usize),
            resolution: opt_iso_to_period(resp.resolution)?,
            // The forecast-parameters gRPC message does not carry the initial
            // timestamp; it is not part of the read-only wire contract.
            initial_timestamp: None,
        })
    }

    pub async fn has_time_series(&self, key: &KeyIdentity) -> CoreResult<bool> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .has_time_series(HasReq {
                key: Some(key_to_pb(key)),
            })
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(resp.present)
    }

    pub async fn verify_integrity(
        &self,
    ) -> CoreResult<time_series_store_core::storage::IntegrityReport> {
        let mut inner = self.inner.lock().await;
        let resp = inner
            .verify_integrity(VerifyReq {})
            .await
            .map_err(Self::map_status)?
            .into_inner();
        Ok(time_series_store_core::storage::IntegrityReport {
            errors: resp.errors,
        })
    }
}
