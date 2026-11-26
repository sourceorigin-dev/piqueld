//! Volumes: the Docker volumes the daemon tracks. An environment's volumes
//! are read with the environment; detached volumes, which belong to no
//! application, need installation-wide permissions.
use super::access::Audited;
use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams, ok, openapi::ApiErrors};
use crate::auth::Identity;
use axum::{Extension, extract::State, response::IntoResponse};
use piqueld_core::api::{AttachVolumeRequest, Envelope, VolumeView};
use piqueld_core::{ApiErrorCode, EnvironmentId, VolumeId, VolumeName};
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;

/// Registers the volume endpoints.
pub(super) fn router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(granted!(Global(SystemRead) => list))
        .routes(granted!(Global(SystemRead) => get))
        .routes(granted!(Global(SystemOperate) => delete))
        .routes(granted!(App(Read) => environment))
        .routes(granted!(App(Write) => attach))
}

/// Which volumes a listing returns.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct VolumesQuery {
    /// Only detached volumes, which no environment binds.
    #[serde(default)]
    detached: bool,
}

impl QueryParams for VolumesQuery {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::QueryInvalid;
    const INVALID_MESSAGE: &'static str = "detached must be true or false";
}

/// Lists every volume, attached ones first, or only detached ones.
#[utoipa::path(get,path="/api/v1/volumes",operation_id="listVolumes",
    params(VolumesQuery),
    responses((status=200,description="Success",body=Envelope<Vec<VolumeView>>),ApiErrors<400, 403, 500, 503>))]
pub(super) async fn list(
    State(state): State<ApiState>,
    ApiQuery(query): ApiQuery<VolumesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.volumes(query.detached).await?))
}

/// Gets one volume.
#[utoipa::path(get,path="/api/v1/volumes/{volume}",operation_id="getVolume",
    params(("volume"=String,Path)),
    responses((status=200,description="Success",body=Envelope<VolumeView>),ApiErrors<400, 403, 404, 500, 503>))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(volume): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.volume(&VolumeId::parse(volume)?).await?))
}

/// Deletes a detached volume and its data.
///
/// Attached volumes fail with 409 `volume_attached`. If Docker can't remove
/// it, the volume reappears as detached at the next volume survey, and
/// deleting it again retries.
#[utoipa::path(delete,path="/api/v1/volumes/{volume}",operation_id="deleteVolume",
    params(("volume"=String,Path)),
    responses((status=200,description="Volume deleted",body=Envelope<bool>),ApiErrors<400, 403, 404, 409, 500, 502, 503>))]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiPath(volume): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .delete_volume(audited.author(&identity), &VolumeId::parse(volume)?)
        .await?;
    Ok(ok(true))
}

/// Lists the volumes an environment binds its manifest volume names to.
#[utoipa::path(get,path="/api/v1/environments/{id}/volumes",operation_id="listEnvironmentVolumes",
    params(("id"=String,Path)),
    responses((status=200,description="Success",body=Envelope<Vec<VolumeView>>),ApiErrors<400, 404, 500, 503>))]
pub(super) async fn environment(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_volumes(&EnvironmentId::parse(id)?)
        .await?))
}

/// Attaches a detached volume to an environment under a volume name.
///
/// The environment mounts it from its next deployment. Needs `apps:write` on
/// the environment and `system:operate`, since the volume may hold another
/// application's data. A name the environment's manifest doesn't declare
/// fails with 409 `volume_name_undeclared`, a name it already binds with
/// `volume_name_bound`, an attached volume with `volume_attached`, and a
/// preview with `volume_preview`.
#[utoipa::path(put,path="/api/v1/environments/{id}/volumes/{name}",operation_id="attachVolume",
    params(("id"=String,Path),("name"=String,Path)),
    request_body=AttachVolumeRequest,
    responses((status=200,description="Volume attached",body=Envelope<VolumeView>),ApiErrors<400, 403, 404, 409, 415, 500, 503>))]
pub(super) async fn attach(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiPath((id, name)): ApiPath<(String, String)>,
    ApiJson(request): ApiJson<AttachVolumeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .attach_volume(
            audited.author(&identity),
            &EnvironmentId::parse(id)?,
            &VolumeName::parse(name)?,
            &request.volume,
        )
        .await?))
}
