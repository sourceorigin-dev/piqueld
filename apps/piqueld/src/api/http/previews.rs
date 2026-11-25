//! Previews: disposable deployments of a branch. Creating, deploying,
//! restoring, and deleting one need no application revision. A preview's status, logs,
//! deployments, and secrets use the environment endpoints with its ID.
use super::extract::{IdempotencyKey, Mutator, content_type};
use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams, ok, openapi::ApiErrors};
use crate::api::{Mutation, PreviewMutation, Restore};
use axum::{
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use piqueld_core::ApiErrorCode;
use piqueld_core::api::{
    AcceptedOperation, CreatePreviewRequest, CreatedPreview, DeletedPreview, Envelope, PreviewView,
    PrunePreviewsRequest, RestoreRequest,
};
use piqueld_core::{ApplicationId, EnvironmentId, GitBranch, PreviewSlot};

/// Creates a preview of a branch.
///
/// Creates and deploys a preview of `branch` of the application's manifest
/// repository, distinguished by `slot` when several previews share a branch.
/// Repeating the request returns the existing preview of that branch and
/// slot with its latest operation, without redeploying it. Applications
/// without a manifest repository fail with `preview_requires_repository`.
/// A new preview beyond a `[previews]` limit fails with 409
/// `preview_limit_reached`, whose `details` are a `PreviewLimitReached`
/// listing the previews it counts. Needs no application revision.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews",operation_id="createPreview",
    params(("id"=String,Path),IdempotencyKey),
    request_body=CreatePreviewRequest,
    responses((status=202,description="Preview created and its first deployment accepted",body=Envelope<CreatedPreview>),
    (status=200,description="The preview of this branch and slot already exists",body=Envelope<CreatedPreview>),
    ApiErrors<400, 404, 409, 415, 500, 503>))]
pub(super) async fn create(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<CreatePreviewRequest>,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Preview(PreviewMutation::Create {
        application: ApplicationId::parse(id)?,
        branch: GitBranch::parse(request.branch)?,
        slot: request.slot.map(PreviewSlot::parse).transpose()?,
    });
    mutator.accept(mutation, None, false).await
}

/// Which preview a local snapshot deploys to.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LocalPreviewQuery {
    /// The worktree's branch of the manifest repository.
    branch: String,
    /// Distinguishes several previews of one branch.
    slot: Option<String>,
}

impl QueryParams for LocalPreviewQuery {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::QueryInvalid;
    const INVALID_MESSAGE: &'static str = "branch is required, and branch and slot may appear once";
}

/// A Git bundle, sent as raw bytes; it only documents the request body.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
pub(super) struct GitBundle(std::marker::PhantomData<()>);

/// Deploys a snapshot of a local worktree to a preview.
///
/// The body is a Git bundle of one snapshot commit, at most `[snapshots]
/// max_upload_bytes`, thin or complete. It is verified and unbundled into a
/// repository of its own. The preview of `branch` and `slot` is created first if
/// needed, as `createPreview` does, and deploys the snapshot's manifest. Fails
/// with 409 `preview_requires_repository` or `preview_limit_reached`; 413
/// `snapshot_too_large`; 422 `snapshot_invalid`; 422 `snapshot_base_unavailable`,
/// whose `details.prerequisites` names the commits a complete bundle must
/// contain; or 408 `snapshot_timed_out`. Needs no application revision.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews/local",operation_id="deployLocalPreview",
    params(("id"=String,Path),LocalPreviewQuery,IdempotencyKey),
    request_body(content=inline(GitBundle),content_type="application/octet-stream",description="Git bundle of the snapshot commit"),
    responses((status=202,description="Snapshot received and its deployment accepted",body=Envelope<CreatedPreview>),
    ApiErrors<400, 404, 408, 409, 413, 415, 422, 500, 503>))]
pub(super) async fn deploy_local(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<LocalPreviewQuery>,
    body: Body,
) -> Result<Response, ApiError> {
    if content_type(mutator.headers()) != Some("application/octet-stream") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiErrorCode::ContentTypeUnsupported,
            "Content-Type must be application/octet-stream",
        ));
    }
    let preview = (
        ApplicationId::parse(id)?,
        GitBranch::parse(query.branch)?,
        query.slot.map(PreviewSlot::parse).transpose()?,
    );
    let length = mutator
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse().ok());
    mutator
        .answering(StatusCode::ACCEPTED)
        .deploy_snapshot(preview, (body, length))
        .await
}

/// Lists an application's previews.
///
/// Each preview comes with its status, latest operation, hostnames, origin,
/// `[previews]` bounds, sync state, and branch state, read with one
/// `git ls-remote` of the manifest repository.
/// Repository failures make every branch state `unknown`.
#[utoipa::path(get,path="/api/v1/applications/{id}/previews",operation_id="listPreviews",
    params(("id"=String,Path)),
    responses((status=200,description="Success",body=Envelope<Vec<PreviewView>>),
    ApiErrors<400, 404, 500, 503>))]
pub(super) async fn list(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.previews(&ApplicationId::parse(id)?).await?))
}

/// Gets a preview with its branch state.
#[utoipa::path(get,path="/api/v1/previews/{id}",operation_id="getPreview",
    params(("id"=String,Path,min_length=8,max_length=64)),
    responses((status=200,description="Success",body=Envelope<PreviewView>),
    ApiErrors<400, 404, 500, 503>))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.preview(&EnvironmentId::parse(id)?).await?))
}

/// Deploys the head of a preview's branch.
#[utoipa::path(post,path="/api/v1/previews/{id}/deploy",operation_id="deployPreview",
    params(("id"=String,Path),IdempotencyKey),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    ApiErrors<400, 404, 409, 500, 503>))]
pub(super) async fn deploy(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Preview(PreviewMutation::Deploy {
        id: EnvironmentId::parse(id)?,
    });
    mutator.accept(mutation, None, false).await
}

/// Restores an earlier deployment of a preview exactly.
///
/// Deploys `deployment` again as it ran, as `restoreDeployment` does for an
/// environment: its rendered manifest, its images and the secret versions it
/// pinned, without fetching, building, or running jobs. Refused unless it
/// succeeded (`deployment_not_restorable`), its images are present or can be
/// pulled again by digest (`image_unavailable`), and the secret versions it
/// pinned still exist (`restore_secrets_unavailable`).
#[utoipa::path(post,path="/api/v1/previews/{id}/restore",operation_id="restorePreviewDeployment",
    params(("id"=String,Path),IdempotencyKey),
    request_body=RestoreRequest,
    responses((status=202,description="Restore accepted",body=Envelope<AcceptedOperation>),
    ApiErrors<400, 404, 409, 415, 500, 502, 503>))]
pub(super) async fn restore(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<RestoreRequest>,
) -> Result<Response, ApiError> {
    mutator
        .restore(id, |id| Restore::Preview {
            id,
            deployment: request.deployment,
        })
        .await
}

/// Deletes a preview with its services, network, secrets, and every volume
/// it ever created; the deletion verifies the volumes are gone.
#[utoipa::path(delete,path="/api/v1/previews/{id}",operation_id="deletePreview",
    params(("id"=String,Path,min_length=8,max_length=64),IdempotencyKey),
    responses((status=202,description="Deletion operation",body=Envelope<AcceptedOperation>),
    ApiErrors<400, 404, 409, 500, 503>))]
pub(super) async fn delete(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Preview(PreviewMutation::Delete {
        id: EnvironmentId::parse(id)?,
    });
    mutator.accept(mutation, None, false).await
}

/// Deletes previews whose branch is gone.
///
/// Deletes each listed preview whose branch the manifest repository confirms
/// is gone when the request runs, and keeps the others. When the repository
/// cannot be read, fails with `repository_unavailable` and deletes nothing.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews/prune",operation_id="prunePreviews",
    params(("id"=String,Path)),
    request_body=PrunePreviewsRequest,
    responses((status=200,description="Previews whose deletion was accepted",body=Envelope<Vec<DeletedPreview>>),
    ApiErrors<400, 404, 409, 415, 500, 502, 503>))]
pub(super) async fn prune(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<PrunePreviewsRequest>,
) -> Result<Response, ApiError> {
    let application = ApplicationId::parse(id)?;
    mutator
        .answering(StatusCode::OK)
        .prune_previews(&application, &request.previews)
        .await
}
