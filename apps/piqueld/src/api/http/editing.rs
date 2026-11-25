//! The field endpoints, one per entry of `piqueld_core::field_endpoints!`,
//! and `create_application`. Persistence applies changes inside its writer
//! transaction.
use super::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams,
    extract::{IdempotencyKey, Mutator},
    openapi::ApiErrors,
};
use crate::api::Mutation;
use axum::response::Response;
use piqueld_core::ApiErrorCode;
use piqueld_core::{
    ApplicationId,
    api::{Envelope, SavedApplication},
    edit::{
        ApplicationEdit, BoolValue, CountValue, CpuValue, EditOptions, EnvironmentValue,
        ExportsValue, FilesValue, GeneratedSecretsValue, HealthValue, ImportsValue, JobsValue,
        MemoryValue, MountsValue, OptionalStringValue, ReplicasValue, RepositorySyncValue,
        RepositoryValue, ResourcesValue, RollbackValue, RolloutValue, RoutesValue, SecondsValue,
        SecretsValue, SecurityValue, ServiceEdit, ServiceGeneral, ServiceProcess, SourceValue,
        StringValue, StringsValue, TemplateValue, TemplatesValue, VariablesValue, VisibilityValue,
        VolumesValue,
    },
    manifest::{Mount, Service, ServiceFile, Volume},
};
use utoipa_axum::router::OpenApiRouter;

impl QueryParams for EditOptions {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::QueryInvalid;
    const INVALID_MESSAGE: &'static str =
        "expected_generation must be an integer; force and deploy must be true or false";
}

/// Submits `edit` of application `id` under the shared query `options`,
/// answering 200 when saved or 202 when `deploy=true` also accepted a
/// deployment.
async fn submit(
    mutator: &Mutator,
    id: String,
    edit: ApplicationEdit,
    options: EditOptions,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Edit {
        id: ApplicationId::parse(id)?,
        edit: Box::new(edit),
        deploy: options.deploy,
    };
    let (expected, force) = (options.expected_generation, options.force);
    mutator.accept(mutation, expected, force).await
}

macro_rules! edit_endpoints {
    ($($(#[$doc:meta])* $name:ident: $method:ident $path:literal ($($param:ident = $label:literal),*) $body:tt => { $($edit:tt)* };)*) => {
        $(edit_endpoints!(@handler $(#[$doc])* $name $method $path ($($param: String = $label),*) $body { $($edit)* });)*

        /// Registers every field endpoint, each requiring `apps:write` on its
        /// application.
        fn field_routes(router: OpenApiRouter<ApiState>) -> OpenApiRouter<ApiState> {
            router $(.routes(granted!(App(Write) => $name)))*
        }
    };
    (@handler $(#[$doc:meta])* $name:ident $method:ident $path:literal ($($param:ident: $param_ty:ty = $label:literal),*) [] { $($edit:tt)* }) => {
        $(#[$doc])*
        #[utoipa::path($method, path = $path, operation_id = stringify!($name),
            params(("id" = String, Path), $(($label = String, Path),)* EditOptions, IdempotencyKey),
            responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
                (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
                ApiErrors<400, 404, 409, 422, 500, 503>))]
        async fn $name(mutator: Mutator, ApiPath((id, $($param,)*)): ApiPath<(String, $($param_ty,)*)>, ApiQuery(options): ApiQuery<EditOptions>) -> Result<Response, ApiError> {
            submit(&mutator, id, $($edit)*, options).await
        }
    };
    (@handler $(#[$doc:meta])* $name:ident $method:ident $path:literal ($($param:ident: $param_ty:ty = $label:literal),*) [$body:ident { $value:ident }] { $($edit:tt)* }) => {
        edit_endpoints!(@handler $(#[$doc])* $name $method $path ($($param: $param_ty = $label),*) ($body, $body { $value }) { $($edit)* });
    };
    (@handler $(#[$doc:meta])* $name:ident $method:ident $path:literal ($($param:ident: $param_ty:ty = $label:literal),*) [$binding:ident: $body:ty] { $($edit:tt)* }) => {
        edit_endpoints!(@handler $(#[$doc])* $name $method $path ($($param: $param_ty = $label),*) ($body, $binding) { $($edit)* });
    };
    (@handler $(#[$doc:meta])* $name:ident $method:ident $path:literal ($($param:ident: $param_ty:ty = $label:literal),*) ($body:ty, $($bind:tt)*) { $($edit:tt)* }) => {
        $(#[$doc])*
        #[utoipa::path($method, path = $path, operation_id = stringify!($name),
            params(("id" = String, Path), $(($label = String, Path),)* EditOptions, IdempotencyKey),
            request_body = $body,
            responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
                (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
                ApiErrors<400, 404, 409, 413, 415, 422, 500, 503>))]
        async fn $name(mutator: Mutator, ApiPath((id, $($param,)*)): ApiPath<(String, $($param_ty,)*)>, ApiQuery(options): ApiQuery<EditOptions>, ApiJson($($bind)*): ApiJson<$body>) -> Result<Response, ApiError> {
            submit(&mutator, id, $($edit)*, options).await
        }
    };
}
piqueld_core::field_endpoints!(edit_endpoints);

/// Query options for `create_application`; `deploy` defaults to false.
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CreateQuery {
    deploy: bool,
}

impl QueryParams for CreateQuery {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::QueryInvalid;
    const INVALID_MESSAGE: &'static str = "deploy must be true or false";
}

/// Creates an empty application.
///
/// The body holds only the name, which must not already be in use (409
/// otherwise). Pass `deploy=true` to also deploy it (202 instead of 200).
#[utoipa::path(post, path = "/api/v1/applications", operation_id = "create_application",
    params(("deploy" = Option<bool>, Query), IdempotencyKey), request_body = StringValue,
    responses((status = 200, description = "Empty application saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        ApiErrors<400, 409, 413, 415, 422, 500, 503>))]
async fn create_application(
    mutator: Mutator,
    ApiQuery(query): ApiQuery<CreateQuery>,
    ApiJson(name): ApiJson<StringValue>,
) -> Result<Response, ApiError> {
    let manifest = piqueld_core::manifest::ApplicationManifest {
        api_version: piqueld_core::manifest::APPLICATION_API_VERSION.into(),
        kind: piqueld_core::manifest::APPLICATION_KIND.into(),
        metadata: piqueld_core::manifest::Metadata { name: name.value },
        spec: piqueld_core::manifest::ApplicationSpec::default(),
    }
    .validate_template()?;
    mutator
        .accept(Mutation::save(manifest, None, query.deploy), Some(0), false)
        .await
}

/// Registers every field endpoint plus `create_application`.
pub(super) fn router() -> OpenApiRouter<ApiState> {
    field_routes(OpenApiRouter::new().routes(granted!(Global(AppsCreate) => create_application)))
}
