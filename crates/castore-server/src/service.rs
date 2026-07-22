//! `tonic` service implementation backed by a local `Store`.

use std::path::Path;
use std::sync::Arc;

use castore_core::{
    KeyIdentity, ListFilter, OwnerCategory, Period, Store, TimeSeriesError, TimeSeriesType,
};
use chrono::{DateTime, Utc};

/// Parse an ISO-8601 period from a request, mapping failures to an
/// `invalid_argument` status.
fn parse_period(s: &str) -> Result<Period, Status> {
    Period::from_iso8601(s).map_err(|e| Status::invalid_argument(e.to_string()))
}

/// Build a [`ListFilter`] from a `ListReq`, mapping bad enums / periods to
/// `invalid_argument`. Shared by `ListTimeSeries` and `ListKeys`.
fn filter_from_list_req(req: ListReq) -> Result<ListFilter, Status> {
    let mut filter = ListFilter::new();
    if let Some(id) = req.owner_id {
        filter = filter.owner_id(id);
    }
    if let Some(c) = req.owner_category {
        let pb_c = pb::OwnerCategory::try_from(c)
            .map_err(|_| Status::invalid_argument(format!("unknown owner_category {c}")))?;
        filter = filter.owner_category(OwnerCategory::from(pb_c));
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
    if let Some(iso) = req.resolution {
        filter = filter.resolution(parse_period(&iso)?);
    }
    if let Some(iso) = req.interval {
        filter = filter.interval(parse_period(&iso)?);
    }
    if let Some(f) = req.features {
        filter = filter.features(features_from_pb(f).map_err(map_convert_err)?);
    }
    Ok(filter)
}

/// An inclusive-start, exclusive-end UTC time range.
type TimeRange = (DateTime<Utc>, DateTime<Utc>);

/// Parse an optional `(start, end)` RFC3339 range; both must be supplied
/// together or neither. Shared by `GetTimeSeries` and `BulkRead`.
fn parse_time_range(
    start: Option<String>,
    end: Option<String>,
) -> Result<Option<TimeRange>, Status> {
    match (start, end) {
        (Some(s), Some(e)) => {
            let start = DateTime::parse_from_rfc3339(&s)
                .map_err(|err| Status::invalid_argument(format!("start: {err}")))?
                .with_timezone(&Utc);
            let end = DateTime::parse_from_rfc3339(&e)
                .map_err(|err| Status::invalid_argument(format!("end: {err}")))?
                .with_timezone(&Utc);
            Ok(Some((start, end)))
        }
        (None, None) => Ok(None),
        _ => Err(Status::invalid_argument(
            "start_rfc3339 and end_rfc3339 must be supplied together",
        )),
    }
}
use castore_proto::convert::{
    features_from_pb, forecast_summary_row_to_pb, full_key_to_pb, key_from_pb, metadata_to_pb,
    requested_type_from_pb, static_summary_row_to_pb, time_series_data_to_get_resp,
};
use castore_proto::pb::{
    self, BulkReadReq, BulkReadResp, ConsistencyReq, ConsistencyResp, CountsByTypeResp, CountsReq,
    CountsResp, DetailedCountsResp, EmptyReq, ForecastParamsReq, ForecastParamsResp,
    ForecastSummaryResp, GetReq, GetResp, HasReq, HasResp, IntervalsReq, IntervalsResp, KeyReq,
    KeysReq, KeysResp, ListKeysReq, ListKeysResp, ListOwnerIdsReq, ListOwnerIdsResp, ListReq,
    ListResp, ResolutionsReq, ResolutionsResp, ResolveForecastKeyReq, ResolveForecastKeyResp,
    StaticSummaryResp, TimeSeriesMetadata, VerifyReq, VerifyResp,
    catalog_store_server::{CatalogStore as CatalogStoreSvc, CatalogStoreServer},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

/// Trait service backed by a `Store`. Read-only RPCs only.
pub struct CatalogStoreService {
    store: Arc<Mutex<Store>>,
}

impl CatalogStoreService {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, TimeSeriesError> {
        let store = Store::open(path, true)?;
        Ok(Self::new(store))
    }

    pub fn into_server(self) -> CatalogStoreServer<Self> {
        CatalogStoreServer::new(self)
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
        e @ TimeSeriesError::IncompatibleFormat { .. } => {
            Status::failed_precondition(e.to_string())
        }
        TimeSeriesError::Io(e) => Status::internal(format!("io: {e}")),
        TimeSeriesError::Sqlite(e) => Status::internal(format!("sqlite: {e}")),
        TimeSeriesError::Serde(e) => Status::internal(format!("serde: {e}")),
        // `TimeSeriesError` is non_exhaustive; surface future variants as
        // internal rather than failing to compile against a newer core.
        e => Status::internal(e.to_string()),
    }
}

fn map_convert_err(e: castore_proto::convert::ConvertError) -> Status {
    Status::invalid_argument(e.to_string())
}

#[tonic::async_trait]
impl CatalogStoreSvc for CatalogStoreService {
    async fn list_time_series(
        &self,
        request: Request<ListReq>,
    ) -> Result<Response<ListResp>, Status> {
        let filter = filter_from_list_req(request.into_inner())?;
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
        let time_range = parse_time_range(req.start_rfc3339, req.end_rfc3339)?;
        let store = self.store.lock().await;
        let data = store.get_time_series(&key, time_range).map_err(map_err)?;
        Ok(Response::new(time_series_data_to_get_resp(&data)))
    }

    async fn get_time_series_keys(
        &self,
        request: Request<KeysReq>,
    ) -> Result<Response<KeysResp>, Status> {
        let req = request.into_inner();
        let owner_category = pb::OwnerCategory::try_from(req.owner_category)
            .map_err(|_| {
                Status::invalid_argument(format!("unknown owner_category {}", req.owner_category))
            })
            .map(OwnerCategory::from)?;
        let store = self.store.lock().await;
        let keys = store
            .get_time_series_keys(req.owner_id, owner_category)
            .map_err(map_err)?;
        Ok(Response::new(KeysResp {
            keys: keys.iter().map(full_key_to_pb).collect(),
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
            resolution: durations.iter().map(|p| p.to_iso8601()).collect(),
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

    async fn get_forecast_parameters(
        &self,
        request: Request<ForecastParamsReq>,
    ) -> Result<Response<ForecastParamsResp>, Status> {
        let req = request.into_inner();
        let resolution = req.resolution.as_deref().map(parse_period).transpose()?;
        let interval = req.interval.as_deref().map(parse_period).transpose()?;
        let store = self.store.lock().await;
        let params = store
            .get_forecast_parameters(resolution, interval)
            .map_err(map_err)?;
        Ok(Response::new(ForecastParamsResp {
            horizon: params.horizon.map(|p| p.to_iso8601()),
            interval: params.interval.map(|p| p.to_iso8601()),
            count: params.count.map(|c| c as u64),
            resolution: params.resolution.map(|p| p.to_iso8601()),
            initial_timestamp_rfc3339: params.initial_timestamp.map(|t| t.to_rfc3339()),
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

    // ---- Additive read RPCs (Phase 4.4) ----

    async fn list_keys(
        &self,
        request: Request<ListKeysReq>,
    ) -> Result<Response<ListKeysResp>, Status> {
        let req = request.into_inner();
        let filter = filter_from_list_req(req.filter.unwrap_or_default())?;
        let store = self.store.lock().await;
        let rows = if req.with_hash {
            store
                .list_keys_with_hash(filter)
                .map_err(map_err)?
                .into_iter()
                .map(|(k, h)| pb::list_keys_resp::Row {
                    key: Some(full_key_to_pb(&k)),
                    data_hash: Some(h.to_vec()),
                })
                .collect()
        } else {
            store
                .list_keys(filter)
                .map_err(map_err)?
                .into_iter()
                .map(|k| pb::list_keys_resp::Row {
                    key: Some(full_key_to_pb(&k)),
                    data_hash: None,
                })
                .collect()
        };
        Ok(Response::new(ListKeysResp { rows }))
    }

    async fn get_metadata(
        &self,
        request: Request<KeyReq>,
    ) -> Result<Response<TimeSeriesMetadata>, Status> {
        let key = request
            .into_inner()
            .key
            .ok_or_else(|| Status::invalid_argument("missing key"))?;
        let key = key_from_pb(key).map_err(map_convert_err)?;
        let store = self.store.lock().await;
        let meta = store.get_metadata(&key).map_err(map_err)?;
        Ok(Response::new(metadata_to_pb(&meta)))
    }

    async fn bulk_read(
        &self,
        request: Request<BulkReadReq>,
    ) -> Result<Response<BulkReadResp>, Status> {
        let req = request.into_inner();
        let time_range = parse_time_range(req.start_rfc3339, req.end_rfc3339)?;
        let keys = req
            .keys
            .into_iter()
            .map(key_from_pb)
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_convert_err)?;
        let refs: Vec<&KeyIdentity> = keys.iter().collect();
        let store = self.store.lock().await;
        let datas = store.bulk_read_range(&refs, time_range).map_err(map_err)?;
        Ok(Response::new(BulkReadResp {
            items: datas.iter().map(time_series_data_to_get_resp).collect(),
        }))
    }

    async fn get_detailed_counts(
        &self,
        _request: Request<EmptyReq>,
    ) -> Result<Response<DetailedCountsResp>, Status> {
        let store = self.store.lock().await;
        let c = store.time_series_counts_detailed().map_err(map_err)?;
        Ok(Response::new(DetailedCountsResp {
            components_with_time_series: c.components_with_time_series,
            supplemental_attributes_with_time_series: c.supplemental_attributes_with_time_series,
            static_time_series_count: c.static_time_series_count,
            forecast_count: c.forecast_count,
        }))
    }

    async fn get_counts_by_type(
        &self,
        _request: Request<EmptyReq>,
    ) -> Result<Response<CountsByTypeResp>, Status> {
        let store = self.store.lock().await;
        let entries = store
            .counts_by_type()
            .map_err(map_err)?
            .into_iter()
            .map(|(t, n)| pb::counts_by_type_resp::Entry {
                time_series_type: pb::TimeSeriesType::from(t) as i32,
                count: n,
            })
            .collect();
        Ok(Response::new(CountsByTypeResp { entries }))
    }

    async fn list_owner_ids(
        &self,
        request: Request<ListOwnerIdsReq>,
    ) -> Result<Response<ListOwnerIdsResp>, Status> {
        let req = request.into_inner();
        let category = pb::OwnerCategory::try_from(req.owner_category)
            .map_err(|_| {
                Status::invalid_argument(format!("unknown owner_category {}", req.owner_category))
            })
            .map(OwnerCategory::from)?;
        let ts_type = match req.time_series_type {
            Some(t) => Some(TimeSeriesType::from(
                pb::TimeSeriesType::try_from(t).map_err(|_| {
                    Status::invalid_argument(format!("unknown time_series_type {t}"))
                })?,
            )),
            None => None,
        };
        let resolution = req.resolution.as_deref().map(parse_period).transpose()?;
        let store = self.store.lock().await;
        let ids = store
            .list_owner_ids(category, ts_type, resolution)
            .map_err(map_err)?;
        Ok(Response::new(ListOwnerIdsResp { owner_id: ids }))
    }

    async fn get_intervals(
        &self,
        request: Request<IntervalsReq>,
    ) -> Result<Response<IntervalsResp>, Status> {
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
        let intervals = store.get_intervals(ts_type).map_err(map_err)?;
        Ok(Response::new(IntervalsResp {
            interval: intervals.iter().map(|p| p.to_iso8601()).collect(),
        }))
    }

    async fn get_static_summary(
        &self,
        _request: Request<EmptyReq>,
    ) -> Result<Response<StaticSummaryResp>, Status> {
        let store = self.store.lock().await;
        let rows = store.static_summary().map_err(map_err)?;
        Ok(Response::new(StaticSummaryResp {
            rows: rows.iter().map(static_summary_row_to_pb).collect(),
        }))
    }

    async fn get_forecast_summary(
        &self,
        _request: Request<EmptyReq>,
    ) -> Result<Response<ForecastSummaryResp>, Status> {
        let store = self.store.lock().await;
        let rows = store.forecast_summary().map_err(map_err)?;
        Ok(Response::new(ForecastSummaryResp {
            rows: rows.iter().map(forecast_summary_row_to_pb).collect(),
        }))
    }

    async fn check_static_consistency(
        &self,
        request: Request<ConsistencyReq>,
    ) -> Result<Response<ConsistencyResp>, Status> {
        let resolution = request
            .into_inner()
            .resolution
            .as_deref()
            .map(parse_period)
            .transpose()?;
        let store = self.store.lock().await;
        let rows = store
            .check_static_consistency(resolution)
            .map_err(map_err)?
            .into_iter()
            .map(|c| pb::consistency_resp::Row {
                resolution: c.resolution.to_iso8601(),
                initial_timestamp_rfc3339: c.initial_timestamp.to_rfc3339(),
                length: c.length as u64,
            })
            .collect();
        Ok(Response::new(ConsistencyResp { rows }))
    }

    async fn resolve_forecast_key(
        &self,
        request: Request<ResolveForecastKeyReq>,
    ) -> Result<Response<ResolveForecastKeyResp>, Status> {
        let req = request.into_inner();
        let category = pb::OwnerCategory::try_from(req.owner_category)
            .map_err(|_| {
                Status::invalid_argument(format!("unknown owner_category {}", req.owner_category))
            })
            .map(OwnerCategory::from)?;
        let resolution = req.resolution.as_deref().map(parse_period).transpose()?;
        let interval = req.interval.as_deref().map(parse_period).transpose()?;
        let features = match req.features {
            Some(f) => features_from_pb(f).map_err(map_convert_err)?,
            None => castore_core::Features::new(),
        };
        let requested = requested_type_from_pb(
            req.requested
                .ok_or_else(|| Status::invalid_argument("missing requested type"))?,
        )
        .map_err(map_convert_err)?;
        let store = self.store.lock().await;
        let key = store
            .resolve_forecast_key(
                req.owner_id,
                category,
                &req.name,
                resolution,
                interval,
                features,
                requested,
            )
            .map_err(map_err)?;
        Ok(Response::new(ResolveForecastKeyResp {
            key: Some(full_key_to_pb(&key)),
        }))
    }
}
