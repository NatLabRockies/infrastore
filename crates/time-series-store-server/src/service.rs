//! `tonic` service implementation backed by a local `Store`.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use time_series_store_core::{ListFilter, Store, TimeSeriesError, TimeSeriesType};
use time_series_store_proto::convert::{
    features_from_pb, key_from_pb, metadata_to_pb, time_series_data_to_get_resp,
};
use time_series_store_proto::pb::{
    self, CountsReq, CountsResp, GetReq, GetResp, HasReq, HasResp, KeysReq, KeysResp, ListReq,
    ListResp, ResolutionsReq, ResolutionsResp, VerifyReq, VerifyResp,
    time_series_store_server::{TimeSeriesStore as TimeSeriesStoreSvc, TimeSeriesStoreServer},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

/// Trait service backed by a `Store`. Read-only RPCs only.
pub struct TimeSeriesStoreService {
    store: Arc<Mutex<Store>>,
}

impl TimeSeriesStoreService {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, TimeSeriesError> {
        let store = Store::open(path, true)?;
        Ok(Self::new(store))
    }

    pub fn into_server(self) -> TimeSeriesStoreServer<Self> {
        TimeSeriesStoreServer::new(self)
    }
}

/// Map a core `TimeSeriesError` onto a tonic `Status`.
fn map_err(e: TimeSeriesError) -> Status {
    match e {
        TimeSeriesError::NotFound => Status::not_found("time series not found"),
        TimeSeriesError::DuplicateTimeSeries => Status::already_exists("duplicate"),
        TimeSeriesError::InvalidParameter(m) => Status::invalid_argument(m),
        TimeSeriesError::IntegrityError(m) => Status::data_loss(m),
        TimeSeriesError::ReadOnlyStore => Status::failed_precondition("store is read-only"),
        TimeSeriesError::ConnectionError(m) => Status::unavailable(m),
        TimeSeriesError::IncompatibleForecast => {
            Status::failed_precondition("incompatible forecast")
        }
        TimeSeriesError::Io(e) => Status::internal(format!("io: {e}")),
        TimeSeriesError::Sqlite(e) => Status::internal(format!("sqlite: {e}")),
        TimeSeriesError::Serde(e) => Status::internal(format!("serde: {e}")),
    }
}

fn map_convert_err(e: time_series_store_proto::convert::ConvertError) -> Status {
    Status::invalid_argument(e.to_string())
}

#[tonic::async_trait]
impl TimeSeriesStoreSvc for TimeSeriesStoreService {
    async fn list_time_series(
        &self,
        request: Request<ListReq>,
    ) -> Result<Response<ListResp>, Status> {
        let req = request.into_inner();
        let mut filter = ListFilter::new();
        if let Some(uuid) = req.owner_uuid {
            filter = filter.owner_uuid(uuid);
        }
        if let Some(t) = req.owner_type {
            filter = filter.owner_type(t);
        }
        if let Some(t) = req.time_series_type {
            let pb_t = pb::TimeSeriesType::try_from(t)
                .map_err(|_| Status::invalid_argument(format!("unknown time_series_type {t}")))?;
            filter = filter.time_series_type(TimeSeriesType::from(pb_t));
        }
        if let Some(name) = req.name {
            filter = filter.name(name);
        }
        if let Some(ms) = req.resolution_ms {
            filter = filter.resolution(Duration::milliseconds(ms));
        }
        if let Some(f) = req.features {
            filter = filter.features(features_from_pb(f).map_err(map_convert_err)?);
        }

        let store = self.store.lock().await;
        let metas = store.list_time_series(filter).map_err(map_err)?;
        Ok(Response::new(ListResp {
            metadata: metas.iter().map(metadata_to_pb).collect(),
        }))
    }

    async fn get_time_series(&self, request: Request<GetReq>) -> Result<Response<GetResp>, Status> {
        let req = request.into_inner();
        let key = req
            .key
            .ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = key_from_pb(key).map_err(map_convert_err)?;

        let time_range = match (req.start_rfc3339, req.end_rfc3339) {
            (Some(s), Some(e)) => {
                let start: DateTime<Utc> = DateTime::parse_from_rfc3339(&s)
                    .map_err(|err| Status::invalid_argument(format!("start: {err}")))?
                    .with_timezone(&Utc);
                let end: DateTime<Utc> = DateTime::parse_from_rfc3339(&e)
                    .map_err(|err| Status::invalid_argument(format!("end: {err}")))?
                    .with_timezone(&Utc);
                Some((start, end))
            }
            (None, None) => None,
            _ => {
                return Err(Status::invalid_argument(
                    "start_rfc3339 and end_rfc3339 must be supplied together",
                ));
            }
        };

        let store = self.store.lock().await;
        let data = store.get_time_series(&key, time_range).map_err(map_err)?;
        Ok(Response::new(time_series_data_to_get_resp(&data)))
    }

    async fn get_time_series_keys(
        &self,
        request: Request<KeysReq>,
    ) -> Result<Response<KeysResp>, Status> {
        let req = request.into_inner();
        let store = self.store.lock().await;
        let keys = store
            .get_time_series_keys(&req.owner_uuid)
            .map_err(map_err)?;
        Ok(Response::new(KeysResp {
            keys: keys
                .iter()
                .map(time_series_store_proto::convert::key_to_pb)
                .collect(),
        }))
    }

    async fn get_resolutions(
        &self,
        request: Request<ResolutionsReq>,
    ) -> Result<Response<ResolutionsResp>, Status> {
        let req = request.into_inner();
        let ts_type = match req.time_series_type {
            Some(t) => Some(TimeSeriesType::from(
                pb::TimeSeriesType::try_from(t).map_err(|_| {
                    Status::invalid_argument(format!("unknown time_series_type {t}"))
                })?,
            )),
            None => None,
        };
        let store = self.store.lock().await;
        let durations = store.get_resolutions(ts_type).map_err(map_err)?;
        Ok(Response::new(ResolutionsResp {
            resolution_ms: durations.iter().map(|d| d.num_milliseconds()).collect(),
        }))
    }

    async fn get_counts(
        &self,
        _request: Request<CountsReq>,
    ) -> Result<Response<CountsResp>, Status> {
        let store = self.store.lock().await;
        let counts = store.get_time_series_counts().map_err(map_err)?;
        Ok(Response::new(CountsResp {
            components_with_time_series: counts.components_with_time_series,
            static_time_series: counts.static_time_series,
            forecasts: counts.forecasts,
        }))
    }

    async fn has_time_series(&self, request: Request<HasReq>) -> Result<Response<HasResp>, Status> {
        let req = request.into_inner();
        let key = req
            .key
            .ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = key_from_pb(key).map_err(map_convert_err)?;
        let store = self.store.lock().await;
        let present = store.has_time_series(&key).map_err(map_err)?;
        Ok(Response::new(HasResp { present }))
    }

    async fn verify_integrity(
        &self,
        _request: Request<VerifyReq>,
    ) -> Result<Response<VerifyResp>, Status> {
        let store = self.store.lock().await;
        let report = store.verify_integrity().map_err(map_err)?;
        Ok(Response::new(VerifyResp {
            errors: report.errors,
        }))
    }
}
