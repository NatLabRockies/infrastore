//! `tonic` service implementation backed by a local `Store`.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use infrastore_core::{
    ListFilter, OwnerCategory, Period, Store, TimeRange, TimeSeriesError, TimeSeriesId,
    TimeSeriesType,
};

/// Parse an ISO-8601 period from a request, mapping failures to an
/// `invalid_argument` status.
fn parse_period(s: &str) -> Result<Period, Status> {
    Period::from_iso8601(s).map_err(|e| Status::invalid_argument(e.to_string()))
}

/// Build a [`ListFilter`] from a `ListMetadataReq`, mapping bad enums / periods to
/// `invalid_argument`. Shared by `ListTimeSeries` and `ListKeys`.
fn filter_from_list_req(req: ListMetadataReq) -> Result<ListFilter, Status> {
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
    if let Some(field) = req.component_field {
        filter = filter.component_field(field);
    }
    if let Some(zoneless) = req.zoneless {
        filter = filter.zoneless(zoneless);
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

/// Parse an optional `(start, end)` RFC3339 range; both must be supplied
/// together or neither. Shared by `GetTimeSeries` and `BulkRead`.
///
/// `zoneless` carries how the client *spelled* those bounds, which the core
/// checks against the series' own reference. The wire form is RFC3339 either
/// way — a zoneless client sends the wall clock read as if UTC, exactly as the
/// store holds it — so the flag is the only thing that distinguishes them, and
/// absent means zoned.
fn parse_time_range(
    start: Option<String>,
    end: Option<String>,
    zoneless: Option<bool>,
) -> Result<Option<TimeRange>, Status> {
    match (start, end) {
        (Some(s), Some(e)) => {
            let start = DateTime::parse_from_rfc3339(&s)
                .map_err(|err| Status::invalid_argument(format!("start: {err}")))?
                .with_timezone(&Utc);
            let end = DateTime::parse_from_rfc3339(&e)
                .map_err(|err| Status::invalid_argument(format!("end: {err}")))?
                .with_timezone(&Utc);
            Ok(Some(TimeRange::spelled(
                start,
                end,
                zoneless.unwrap_or(false),
            )))
        }
        (None, None) => Ok(None),
        _ => Err(Status::invalid_argument(
            "start_rfc3339 and end_rfc3339 must be supplied together",
        )),
    }
}
use infrastore_proto::convert::{
    features_from_pb, forecast_summary_row_to_pb, metadata_to_pb, requested_type_from_pb,
    static_summary_row_to_pb, time_series_data_to_read_resp,
};
use infrastore_proto::pb::{
    self, AssociationExistsReq, AssociationExistsResp, CheckStaticConsistencyReq,
    CheckStaticConsistencyResp, GetCountsByTypeReq, GetCountsByTypeResp, GetCountsReq,
    GetCountsResp, GetDetailedCountsReq, GetDetailedCountsResp, GetForecastParametersReq,
    GetForecastParametersResp, GetForecastSummaryReq, GetForecastSummaryResp, GetIntervalsReq,
    GetIntervalsResp, GetResolutionsReq, GetResolutionsResp, GetStaticSummaryReq,
    GetStaticSummaryResp, HasAnyTimeSeriesReq, HasAnyTimeSeriesResp, ListMetadataByIdsReq,
    ListMetadataByIdsResp, ListMetadataReq, ListMetadataResp, ListOwnerIdsReq, ListOwnerIdsResp,
    ReadByIdReq, ReadByIdResp, ReadByIdsReq, ReadByIdsResp, TimeSeriesMetadata, VerifyIntegrityReq,
    VerifyIntegrityResp,
    catalog_store_server::{CatalogStore as CatalogStoreSvc, CatalogStoreServer},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

/// Trait service backed by a `Store`. Read-only RPCs only.
pub struct CatalogStoreService {
    store: Arc<Mutex<Store>>,
    /// See [`DEFAULT_MAX_READ_IDS`].
    max_read_ids: usize,
}

/// Default ceiling on the number of ids one `ReadByIds` may name.
///
/// `ReadByIds` is the one RPC whose response size the *caller* chooses: it
/// returns a full copy of a series per id and does not collapse duplicates
/// (items correspond positionally to the ids asked for). Unbounded, a request
/// inside tonic's 4 MiB decode limit can name a couple of hundred thousand ids
/// and amplify a 900 KB request into an 822 MB response off a 16 KB store —
/// unauthenticated under the default `auth = "none"`.
///
/// Operators serving very large stores can raise it; see
/// `[server] max_read_ids`.
pub const DEFAULT_MAX_READ_IDS: usize = 4096;

impl CatalogStoreService {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            max_read_ids: DEFAULT_MAX_READ_IDS,
        }
    }

    pub fn from_path(path: &Path) -> Result<Self, TimeSeriesError> {
        let store = Store::open(path, true)?;
        Ok(Self::new(store))
    }

    /// Override the `ReadByIds` id ceiling. See [`DEFAULT_MAX_READ_IDS`].
    pub fn with_max_read_ids(mut self, max: usize) -> Self {
        self.max_read_ids = max;
        self
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

fn map_convert_err(e: infrastore_proto::convert::ConvertError) -> Status {
    Status::invalid_argument(e.to_string())
}

#[tonic::async_trait]
impl CatalogStoreSvc for CatalogStoreService {
    async fn list_metadata(
        &self,
        request: Request<ListMetadataReq>,
    ) -> Result<Response<ListMetadataResp>, Status> {
        let filter = filter_from_list_req(request.into_inner())?;
        let store = self.store.lock().await;
        let metas = store.list_metadata(filter).map_err(map_err)?;
        Ok(Response::new(ListMetadataResp {
            metadata: metas.iter().map(metadata_to_pb).collect(),
        }))
    }

    /// The catalog rows `ids` names, in the order the ids are given.
    ///
    /// `list_metadata` addressed by id — one query for a whole model's worth of
    /// recorded references rather than one call per reference. `NOT_FOUND` if
    /// any id names no row, because a caller naming ids is asserting they
    /// exist; `association_exists` is the call that treats a stale reference as
    /// an answer instead.
    async fn list_metadata_by_ids(
        &self,
        request: Request<ListMetadataByIdsReq>,
    ) -> Result<Response<ListMetadataByIdsResp>, Status> {
        let ids: Vec<TimeSeriesId> = request
            .into_inner()
            .ids
            .into_iter()
            .map(TimeSeriesId)
            .collect();
        let store = self.store.lock().await;
        let metas = store.list_metadata_by_ids(&ids).map_err(map_err)?;
        Ok(Response::new(ListMetadataByIdsResp {
            metadata: metas.iter().map(metadata_to_pb).collect(),
        }))
    }

    /// Whether an association is filed under `id`, without fetching its row.
    ///
    /// The remote form of the load-time reference check: a consumer holding ids
    /// in its own model sifts them here rather than calling `GetMetadataById`
    /// and catching `NOT_FOUND`, which hydrates a row to answer a yes/no.
    async fn association_exists(
        &self,
        request: Request<AssociationExistsReq>,
    ) -> Result<Response<AssociationExistsResp>, Status> {
        let id = request.into_inner().id;
        let store = self.store.lock().await;
        let present = store
            .association_exists(TimeSeriesId(id))
            .map_err(map_err)?;
        Ok(Response::new(AssociationExistsResp { present }))
    }

    async fn read_by_id(
        &self,
        request: Request<ReadByIdReq>,
    ) -> Result<Response<ReadByIdResp>, Status> {
        let req = request.into_inner();
        let time_range = parse_time_range(req.start_rfc3339, req.end_rfc3339, req.bounds_zoneless)?;
        let store = self.store.lock().await;
        let id = TimeSeriesId(req.id);
        let data = match time_range {
            Some(range) => store
                .read_by_ids_range(&[id], range)
                .map(|mut v| v.remove(0)),
            None => store.read_by_id(id, infrastore_core::ReadWindow::full()),
        }
        .map_err(map_err)?;
        // The read already stamped the row's element type onto `data`, so the
        // response describes what its bytes mean without a second catalog trip.
        Ok(Response::new(time_series_data_to_read_resp(&data)))
    }

    async fn get_resolutions(
        &self,
        request: Request<GetResolutionsReq>,
    ) -> Result<Response<GetResolutionsResp>, Status> {
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
        Ok(Response::new(GetResolutionsResp {
            resolution: durations.iter().map(|p| p.to_iso8601()).collect(),
        }))
    }

    async fn get_counts(
        &self,
        _request: Request<GetCountsReq>,
    ) -> Result<Response<GetCountsResp>, Status> {
        let store = self.store.lock().await;
        let counts = store.get_time_series_counts().map_err(map_err)?;
        Ok(Response::new(GetCountsResp {
            components_with_time_series: counts.components_with_time_series,
            static_time_series: counts.static_time_series,
            forecasts: counts.forecasts,
        }))
    }

    async fn get_forecast_parameters(
        &self,
        request: Request<GetForecastParametersReq>,
    ) -> Result<Response<GetForecastParametersResp>, Status> {
        let req = request.into_inner();
        let resolution = req.resolution.as_deref().map(parse_period).transpose()?;
        let interval = req.interval.as_deref().map(parse_period).transpose()?;
        let store = self.store.lock().await;
        let params = store
            .get_forecast_parameters(resolution, interval)
            .map_err(map_err)?;
        Ok(Response::new(GetForecastParametersResp {
            horizon: params.horizon.map(|p| p.to_iso8601()),
            interval: params.interval.map(|p| p.to_iso8601()),
            count: params.count.map(|c| c as u64),
            resolution: params.resolution.map(|p| p.to_iso8601()),
            initial_timestamp_rfc3339: params.initial_timestamp.map(|t| t.to_rfc3339()),
        }))
    }

    async fn has_any_time_series(
        &self,
        request: Request<HasAnyTimeSeriesReq>,
    ) -> Result<Response<HasAnyTimeSeriesResp>, Status> {
        let req = request.into_inner();
        let owner_category = pb::OwnerCategory::try_from(req.owner_category)
            .map_err(|_| {
                Status::invalid_argument(format!("unknown owner_category {}", req.owner_category))
            })
            .map(OwnerCategory::from)?;
        let filter = infrastore_core::ListFilter {
            owner_id: Some(req.owner_id),
            owner_category: Some(owner_category),
            name: Some(req.name),
            time_series_type: req
                .time_series_type
                .map(|t| requested_type_from_pb(t).map_err(map_convert_err))
                .transpose()?,
            resolution: req.resolution.as_deref().map(parse_period).transpose()?,
            interval: req.interval.as_deref().map(parse_period).transpose()?,
            features: Some(
                features_from_pb(pb::Features {
                    entries: req.features,
                })
                .map_err(map_convert_err)?,
            ),
            features_exact: true,
            ..Default::default()
        };
        let store = self.store.lock().await;
        let present = store.has_any_time_series(filter).map_err(map_err)?;
        Ok(Response::new(HasAnyTimeSeriesResp { present }))
    }

    async fn verify_integrity(
        &self,
        _request: Request<VerifyIntegrityReq>,
    ) -> Result<Response<VerifyIntegrityResp>, Status> {
        let store = self.store.lock().await;
        let report = store.verify_integrity().map_err(map_err)?;
        Ok(Response::new(VerifyIntegrityResp {
            errors: report.errors,
        }))
    }

    async fn get_metadata_by_id(
        &self,
        request: Request<pb::GetMetadataByIdReq>,
    ) -> Result<Response<TimeSeriesMetadata>, Status> {
        let id = request.into_inner().id;
        let store = self.store.lock().await;
        let meta = store
            .get_metadata_by_id(TimeSeriesId(id))
            .map_err(map_err)?
            .ok_or_else(|| Status::not_found(format!("no association has id {id}")))?;
        Ok(Response::new(metadata_to_pb(&meta)))
    }

    async fn read_by_ids(
        &self,
        request: Request<ReadByIdsReq>,
    ) -> Result<Response<ReadByIdsResp>, Status> {
        let req = request.into_inner();
        // The transport has already decoded the request by the time this runs —
        // tonic's own limit bounds that — but nothing past it has happened yet:
        // no id is looked up and the store is not touched. The cost of this call
        // is the caller's to choose, so the ceiling applies before that work
        // starts rather than after it.
        if req.ids.len() > self.max_read_ids {
            return Err(Status::resource_exhausted(format!(
                "ReadByIds requested {} ids, more than this server's limit of {}; \
                 split the request",
                req.ids.len(),
                self.max_read_ids
            )));
        }
        let time_range = parse_time_range(req.start_rfc3339, req.end_rfc3339, req.bounds_zoneless)?;
        let store = self.store.lock().await;
        let ids: Vec<TimeSeriesId> = req.ids.iter().copied().map(TimeSeriesId).collect();
        let datas = match time_range {
            Some(range) => store.read_by_ids_range(&ids, range),
            None => store.read_by_ids(&ids, infrastore_core::ReadWindow::full()),
        }
        .map_err(map_err)?;
        Ok(Response::new(ReadByIdsResp {
            items: datas.iter().map(time_series_data_to_read_resp).collect(),
        }))
    }

    async fn get_detailed_counts(
        &self,
        _request: Request<GetDetailedCountsReq>,
    ) -> Result<Response<GetDetailedCountsResp>, Status> {
        let store = self.store.lock().await;
        let c = store.time_series_counts_detailed().map_err(map_err)?;
        Ok(Response::new(GetDetailedCountsResp {
            components_with_time_series: c.components_with_time_series,
            supplemental_attributes_with_time_series: c.supplemental_attributes_with_time_series,
            static_time_series_count: c.static_time_series_count,
            forecast_count: c.forecast_count,
        }))
    }

    async fn get_counts_by_type(
        &self,
        _request: Request<GetCountsByTypeReq>,
    ) -> Result<Response<GetCountsByTypeResp>, Status> {
        let store = self.store.lock().await;
        let entries = store
            .counts_by_type()
            .map_err(map_err)?
            .into_iter()
            .map(|(t, n)| pb::get_counts_by_type_resp::Entry {
                time_series_type: pb::TimeSeriesType::from(t) as i32,
                count: n,
            })
            .collect();
        Ok(Response::new(GetCountsByTypeResp { entries }))
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
        request: Request<GetIntervalsReq>,
    ) -> Result<Response<GetIntervalsResp>, Status> {
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
        Ok(Response::new(GetIntervalsResp {
            interval: intervals.iter().map(|p| p.to_iso8601()).collect(),
        }))
    }

    async fn get_static_summary(
        &self,
        _request: Request<GetStaticSummaryReq>,
    ) -> Result<Response<GetStaticSummaryResp>, Status> {
        let store = self.store.lock().await;
        let rows = store.static_summary().map_err(map_err)?;
        Ok(Response::new(GetStaticSummaryResp {
            rows: rows.iter().map(static_summary_row_to_pb).collect(),
        }))
    }

    async fn get_forecast_summary(
        &self,
        _request: Request<GetForecastSummaryReq>,
    ) -> Result<Response<GetForecastSummaryResp>, Status> {
        let store = self.store.lock().await;
        let rows = store.forecast_summary().map_err(map_err)?;
        Ok(Response::new(GetForecastSummaryResp {
            rows: rows.iter().map(forecast_summary_row_to_pb).collect(),
        }))
    }

    async fn check_static_consistency(
        &self,
        request: Request<CheckStaticConsistencyReq>,
    ) -> Result<Response<CheckStaticConsistencyResp>, Status> {
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
            .map(|c| pb::check_static_consistency_resp::Row {
                resolution: c.resolution.to_iso8601(),
                initial_timestamp_rfc3339: c.initial_timestamp.to_rfc3339(),
                length: c.length as u64,
            })
            .collect();
        Ok(Response::new(CheckStaticConsistencyResp { rows }))
    }
}
