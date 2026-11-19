//! Applications: listing, reading, applying and planning whole manifests,
//! renaming, deletion, and manifest download. Mutations of applications and
//! their environments are submitted through a [`Mutator`].
use super::access::Audited;
use super::extract::{IDEMPOTENCY_KEY, IdempotencyKey, ManifestUpload, optional_header};
use super::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams, accepted, ok, openapi::ApiErrors,
};
use crate::api::{Mutation, MutationResponse, Restore};
use crate::auth::Identity;
use axum::{
    Extension,
    extract::{FromRequestParts, State, rejection::ExtensionRejection},
    http::{HeaderMap, header, request::Parts},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    ApplicationSummary, ApplicationView, ApplyApplicationRequest, DeletedApplication, Envelope,
    Page, PlanView, RenameApplicationRequest, RenamedApplication, SavedApplication,
};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName};
use piqueld_store::Author;
use serde::Deserialize;

/// Cursor pagination, and a name filter, for the application list.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
    /// Only the application with exactly this name, if any.
    name: Option<String>,
    /// Page size.
    #[param(minimum = 1, maximum = 100, default = 100)]
    limit: Option<u16>,
}

impl QueryParams for ListQuery {
    // The same error as an out-of-range `limit`.
    const INVALID_CODE: &'static str = "pagination_invalid";
    const INVALID_MESSAGE: &'static str = "pagination parameters are invalid";
}

#[utoipa::path(
    get,
    path = "/api/v1/applications",
    operation_id = "listApplications",
    summary = "List applications",
    params(ListQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<ApplicationSummary>>),
        ApiErrors<400, 500, 503>,
    )
)]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .applications(
            &identity
                .grants
                .app_scope(piqueld_core::access::AppPermission::Read),
            query.name.as_deref(),
            query.cursor.as_deref(),
            query.limit,
        )
        .await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}",
    operation_id = "getApplication",
    summary = "Get an application and its environments",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<ApplicationView>),
        ApiErrors<400, 404, 500, 503>,
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.application(&ApplicationId::parse(id)?).await?))
}

// Saves a whole manifest (JSON or TOML, see `ManifestUpload`) by name,
// optionally accepting a deployment of it in the same transaction.
#[utoipa::path(
    post, path = "/api/v1/applications/apply", operation_id = "applyApplication",
    summary = "Apply an application manifest",
    params(ApplyQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: required unless forced; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body."),IdempotencyKey),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
        ApiErrors<400, 409, 413, 415, 422, 500, 502, 503>)
)]
pub(super) async fn apply(
    mutator: Mutator,
    ApiQuery(query): ApiQuery<ApplyQuery>,
    upload: ManifestUpload,
) -> Result<Response, ApiError> {
    mutator
        .accept(
            Mutation::save(
                upload.manifest,
                upload.expected_application_id,
                query.deploy,
            ),
            upload.expected_generation,
            query.force,
        )
        .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ApplyQuery {
    /// Explicitly bypass revision and identity preconditions.
    pub(super) force: bool,
    /// Deploy the saved configuration to the application's only environment or,
    /// when its manifest declares environments, every declared one not promoted
    /// from another; omission saves only.
    deploy: bool,
}

impl QueryParams for ApplyQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str = "force and deploy must be true or false";
}

/// Application deletion preconditions and confirmation.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct DeleteApplicationQuery {
    /// Current application revision; required unless forced.
    expected_generation: Option<u64>,
    /// Explicitly bypass the revision precondition. Never skips confirmation.
    #[serde(default)]
    force: bool,
    /// Comma-separated names of every environment; required when there are several.
    environments: Option<String>,
}

impl QueryParams for DeleteApplicationQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str =
        "expected_generation must be an integer and force must be true or false";
}

// Accepts a deletion operation for every environment, whose named volumes are
// retained, and every preview, whose volumes are removed.
#[utoipa::path(
    delete, path = "/api/v1/applications/{id}", operation_id = "deleteApplication",
    summary = "Delete an application with its environments, retaining their volumes, and its previews",
    params(("id" = String, Path, min_length = 8, max_length = 64), DeleteApplicationQuery,IdempotencyKey),
    responses(
        (status = 202, description = "Deletion operations", body = Envelope<DeletedApplication>),
        ApiErrors<400, 404, 409, 500, 503>)
)]
pub(super) async fn delete(
    mutator: Mutator,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<DeleteApplicationQuery>,
) -> Result<Response, ApiError> {
    let environments = query
        .environments
        .iter()
        .flat_map(|names| names.split(','))
        .filter(|name| !name.is_empty())
        .map(EnvironmentName::parse)
        .collect::<Result<_, _>>()?;
    mutator
        .accept(
            Mutation::DeleteApplication {
                id: ApplicationId::parse(id)?,
                environments,
            },
            query.expected_generation,
            query.force,
        )
        .await
}

// Dry-runs `apply` with the same body; supplied preconditions are checked, but
// none are required and nothing is saved.
#[utoipa::path(
    post, path = "/api/v1/applications/plan", operation_id = "planApplication",
    summary = "Preview an application manifest",
    params(PlanQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: inspected intent revision; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body.")),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Preview", body = Envelope<PlanView>),
        ApiErrors<400, 409, 413, 415, 422, 500, 502, 503>)
)]
pub(super) async fn plan(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<PlanQuery>,
    upload: ManifestUpload,
) -> Result<impl IntoResponse, ApiError> {
    let environment = query.environment.map(EnvironmentId::parse).transpose()?;
    Ok(ok(state
        .plan(
            &identity.grants,
            upload.manifest,
            upload.expected_generation,
            upload.expected_application_id,
            environment.as_ref(),
        )
        .await?))
}

/// Environment a preview compares against.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct PlanQuery {
    /// Environment of the application whose latest deployment and runtime the
    /// preview compares against; defaults to its only environment.
    environment: Option<String>,
}

impl QueryParams for PlanQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str = "environment must appear at most once";
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct GenerationQuery {
    /// Current intent revision; optional for reconcile, required for deletion unless forced.
    pub(super) expected_generation: Option<u64>,
    /// Explicitly bypass intent preconditions.
    #[serde(default)]
    pub(super) force: bool,
}

impl QueryParams for GenerationQuery {
    const INVALID_CODE: &'static str = "generation_invalid";
    const INVALID_MESSAGE: &'static str = "invalid expected generation";
}

/// The request of a handler submitting a mutation: the caller, acting
/// through its audited request, and the request's `Idempotency-Key`. It
/// extracts what the handler's leading `State`, `Extension<Identity>`,
/// `Audited` and `HeaderMap` would, failing as they would.
pub(super) struct Mutator {
    pub(super) state: ApiState,
    identity: Identity,
    audited: Audited,
    headers: HeaderMap,
}

impl FromRequestParts<ApiState> for Mutator {
    type Rejection = ExtensionRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, ExtensionRejection> {
        let Extension(identity) = Extension::from_request_parts(parts, state).await?;
        let Ok(audited) = Audited::from_request_parts(parts, state).await;
        Ok(Self {
            state: state.clone(),
            identity,
            audited,
            headers: parts.headers.clone(),
        })
    }
}

impl Mutator {
    /// The caller acting through this request.
    pub(super) fn author(&self) -> Author<'_> {
        self.audited.author(&self.identity)
    }

    /// The request's `Idempotency-Key`, refused when repeated or not ASCII.
    pub(super) fn idempotency_key(&self) -> Result<Option<String>, ApiError> {
        optional_header(&self.headers, IDEMPOTENCY_KEY)
    }

    /// Submits `mutation` and renders the acceptance: 202 when a durable
    /// operation was started (including a save that also deploys, a created
    /// preview, or a deletion), 200 for a plain save, rename, environment
    /// change, or an existing preview.
    pub(super) async fn accept(
        &self,
        mutation: Mutation,
        expected: Option<u64>,
        force: bool,
    ) -> Result<Response, ApiError> {
        let request_id = self.idempotency_key()?;
        let response = self
            .state
            .accept(
                self.author(),
                mutation,
                expected,
                force,
                request_id.as_deref(),
            )
            .await?;
        let started = response.started();
        Ok(match response {
            MutationResponse::Saved(saved) => reply(started, saved),
            MutationResponse::Preview(preview) => reply(started, preview),
            MutationResponse::Rename(renamed) => reply(started, renamed),
            MutationResponse::Environment(environment) => reply(started, environment),
            MutationResponse::Operation(operation) => reply(started, operation),
            MutationResponse::Promotion(promotion) => reply(started, promotion),
            MutationResponse::Deleted(deleted) => reply(started, deleted),
            // Only sync submits the mutations answered with this.
            MutationResponse::Synced(_) => return Err(piqueld_store::StoreError::Corrupt.into()),
        })
    }

    /// Restores an earlier deployment of the environment or preview `id`, as
    /// `restore` names it, answering 202 with the restore's operation.
    pub(super) async fn restore(
        &self,
        id: String,
        restore: impl FnOnce(EnvironmentId) -> Restore,
    ) -> Result<Response, ApiError> {
        let request_id = self.idempotency_key()?;
        let restore = restore(EnvironmentId::parse(id)?);
        Ok(accepted(
            self.state
                .restore(self.author(), restore, request_id.as_deref())
                .await?,
        ))
    }
}

/// Answers `data` with 202 Accepted when its mutation `started` durable
/// work, else 200 OK (see `MutationResponse::started`).
fn reply<T: serde::Serialize>(started: bool, data: T) -> Response {
    if started { accepted(data) } else { ok(data) }
}

/// Renames an application.
///
/// Changes only the user-facing name, without redeploying. The inspected
/// `expected_generation` goes in the JSON body; the new name must not already be
/// in use, even with `force=true`.
#[utoipa::path(post,path="/api/v1/applications/{id}/rename",operation_id="renameApplication",
    params(("id"=String,Path),ForceQuery,IdempotencyKey),
    request_body=RenameApplicationRequest,
    responses((status=200,description="Application renamed without redeployment",body=Envelope<RenamedApplication>),
    ApiErrors<400, 404, 409, 500, 503>))]
pub(super) async fn rename(
    mutator: Mutator,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<RenameApplicationRequest>,
) -> Result<Response, ApiError> {
    mutator
        .accept(
            Mutation::Rename {
                id: ApplicationId::parse(id)?,
                name: request.name,
            },
            request.expected_generation,
            query.force,
        )
        .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ForceQuery {
    /// Explicitly bypass the intent revision precondition. Name availability is always enforced.
    pub(super) force: bool,
}

impl QueryParams for ForceQuery {
    const INVALID_CODE: &'static str = "force_invalid";
    const INVALID_MESSAGE: &'static str = "force must be true or false";
}

/// Downloads the saved configuration as a TOML manifest.
///
/// Reads only saved configuration, so it works while the runtime is unavailable.
#[utoipa::path(get,path="/api/v1/applications/{id}/manifest",operation_id="downloadApplicationManifest",
    params(("id"=String,Path)),
    responses((status=200,description="Saved application configuration",body=String,content_type="application/toml",
        headers(("Content-Disposition"=String,description="Attachment filename for the saved TOML manifest"),
                ("Cache-Control"=String,description="no-store"))),
    ApiErrors<400, 404, 500, 503>))]
pub(super) async fn manifest_download(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let manifest = state.manifest(&ApplicationId::parse(id)?).await?;
    let filename = format!("attachment; filename=\"{}\"", manifest.filename);
    Ok((
        [
            (header::CONTENT_TYPE, "application/toml".to_owned()),
            (header::CONTENT_DISPOSITION, filename),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        manifest.contents,
    ))
}
