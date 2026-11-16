//! The traffic the gateway counted on an environment's routes.
use super::{ApiError, ApiPath, ApiQuery, ApiState, QueryParams, ok, openapi::ApiErrorResponse};
use axum::{extract::State, response::IntoResponse};
use piqueld_core::{EnvironmentId, api::Envelope, observability::TrafficReport};
use piqueld_store::traffic::{DAY_MS, TrafficQuery};

/// Query parameters selecting an environment's traffic.
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct TrafficParams {
    /// Start of the window in Unix ms; 30 days before its end by default.
    since_ms: Option<i64>,
    /// End of the window in Unix ms, exclusive; now by default.
    until_ms: Option<i64>,
    /// Only the routes to this service.
    service: Option<String>,
    /// Only the route with this hostname, such as `*.example.com`.
    hostname: Option<String>,
}

impl QueryParams for TrafficParams {
    const INVALID_CODE: &'static str = "traffic_query_invalid";
    const INVALID_MESSAGE: &'static str = "Invalid traffic query";
}

/// Gets the traffic of an environment's routes.
///
/// The gateway counts the requests each route proxying to a service serves,
/// while `ingress.traffic` is set, and keeps hourly totals for
/// `retention.traffic_days`. The series has hourly points for windows of up
/// to two days, and daily ones beyond. A window that ends before it starts,
/// or spans more than 400 days, fails with 400 `traffic_query_invalid`.
#[utoipa::path(get,path="/api/v1/environments/{id}/traffic",operation_id="environmentTraffic",params(("id"=String,Path),TrafficParams),
 responses((status=200,description="The environment's traffic",body=Envelope<TrafficReport>),
 (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(params): ApiQuery<TrafficParams>,
) -> Result<impl IntoResponse, ApiError> {
    let id = EnvironmentId::parse(id)?;
    let until_ms = params.until_ms.unwrap_or_else(piqueld_store::now_ms);
    let query = TrafficQuery {
        since_ms: params.since_ms.unwrap_or(until_ms - 30 * DAY_MS),
        until_ms,
        service: params.service.as_deref(),
        hostname: params.hostname.as_deref(),
    };
    let _admitted = state.admit(crate::api::Expensive::Analytics)?;
    Ok(ok(state.traffic(&id, query).await?))
}
