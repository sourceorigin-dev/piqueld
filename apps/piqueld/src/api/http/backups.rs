//! Backup repositories: where piqueld writes its backups.
use super::access::Audited;
use super::{ApiError, ApiJson, ApiPath, ApiState, ok, openapi::ApiErrors};
use crate::auth::Identity;
use axum::{Extension, extract::State, response::IntoResponse};
use piqueld_core::{
    access::{GlobalPermission, Permission},
    api::Envelope,
    backup::{BackupRepository, BackupRepositoryName, RecoveryKit, RepositoryRequest},
};

/// Lists backup repositories by name.
///
/// Needs `system:read` or `backups:manage`. Keys are never returned.
#[utoipa::path(get,path="/api/v1/backup/repositories",operation_id="backupRepositories",responses((status=200,body=Envelope<Vec<BackupRepository>>),ApiErrors<403, 503>))]
pub(super) async fn repositories(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> Result<impl IntoResponse, ApiError> {
    if !identity.grants.has_global(GlobalPermission::BackupsManage) {
        identity
            .grants
            .require(Permission::Global(GlobalPermission::SystemRead))?;
    }
    Ok(ok(state.backup_repositories().await?))
}

/// Creates a backup repository.
///
/// Creates it at its location, which must be missing or empty, with two keys:
/// one piqueld keeps, encrypted with the secret master key, and a recovery
/// key returned only in this response. Needs a recent passkey sign-in. A taken
/// name or location fails with 409 `backup_repository_exists`, a directory
/// holding files with 409 `backup_repository_not_empty`, and one piqueld
/// can't write with 503 `backup_repository_unavailable`.
#[utoipa::path(post,path="/api/v1/backup/repositories/{name}",operation_id="createBackupRepository",params(("name"=String,Path)),request_body=RepositoryRequest,
    responses((status=200,body=Envelope<RecoveryKit>),(status=403,response=inline(super::openapi::ReauthenticationResponse)),ApiErrors<400, 409, 415, 503>))]
pub(super) async fn create_repository(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiPath(name): ApiPath<String>,
    ApiJson(request): ApiJson<RepositoryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let name = BackupRepositoryName::parse(name)?;
    identity.require_fresh()?;
    Ok(ok(state
        .create_backup_repository(audited.author(&identity), &name, request)
        .await?))
}

/// Checks a backup repository's structure.
///
/// Opens it with piqueld's key and checks that its index, packs and
/// snapshots agree, without reading their data, then records the result. A
/// repository that can't be opened fails the check.
#[utoipa::path(post,path="/api/v1/backup/repositories/{name}/check",operation_id="checkBackupRepository",params(("name"=String,Path)),responses((status=200,body=Envelope<BackupRepository>),ApiErrors<400, 404, 503>))]
pub(super) async fn check_repository(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiPath(name): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .check_backup_repository(
            audited.author(&identity),
            &BackupRepositoryName::parse(name)?,
        )
        .await?))
}

/// Removes a backup repository from piqueld.
///
/// Its backups stay where they are, and its recovery key still opens them.
#[utoipa::path(delete,path="/api/v1/backup/repositories/{name}",operation_id="deleteBackupRepository",params(("name"=String,Path)),responses((status=200,body=Envelope<bool>),ApiErrors<400, 404, 503>))]
pub(super) async fn delete_repository(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiPath(name): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .delete_backup_repository(
            audited.author(&identity),
            &BackupRepositoryName::parse(name)?,
        )
        .await?;
    Ok(ok(true))
}
