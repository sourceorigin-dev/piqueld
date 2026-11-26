//! The published `OpenAPI` 3.0 contract, generated from the routes' Utoipa
//! metadata and served at `/api/v1/openapi.json`.
use axum::{Extension, body::Bytes, response::IntoResponse};
use http::header;
use piqueld_core::api::{Envelope, ErrorBody, SecretMetadata};
use piqueld_core::{edit::RoutesValue, manifest::Route};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use utoipa::openapi::{RefOr, Response};
use utoipa::{OpenApi, ToResponse};

/// The `OpenAPI` document's static parts: its info, tags and schemas.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "piqueld API",
        version = "v1",
        description = "piqueld control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    components(schemas(
        ErrorBody,
        piqueld_core::api::ApiErrorCode,
        piqueld_core::auth::User,
        piqueld_core::auth::Session,
        piqueld_core::auth::Account,
        piqueld_core::access::Grant,
        piqueld_core::access::Permission,
        piqueld_core::auth::AuthStatus,
        piqueld_core::auth::SetupLink,
        piqueld_core::auth::RegistrationStart,
        piqueld_core::auth::Ceremony,
        piqueld_core::auth::CeremonyFinish,
        piqueld_core::auth::PasskeyView,
        piqueld_core::auth::CredentialView,
        piqueld_core::auth::InvitationView,
        piqueld_core::auth::Directory,
        piqueld_core::auth::Manage,
        piqueld_core::auth::Managed,
        piqueld_core::auth::DeviceStart,
        piqueld_core::auth::DevicePoll,
        piqueld_core::auth::DeviceApprove,
        piqueld_core::auth::DeviceToken,
        SecretMetadata,
        piqueld_core::api::SecretKeyRecovery,
        Envelope<Vec<SecretMetadata>>,
        piqueld_core::api::StoredSecret,
        piqueld_core::api::SecretAccess,
        piqueld_core::api::SecretAccessChange,
        piqueld_core::api::EnvironmentAccess,
        Envelope<Vec<piqueld_core::api::StoredSecret>>,
        Envelope<piqueld_core::api::StoredSecret>,
        Envelope<bool>,
        piqueld_core::api::SettingChange,
        Route,
        RoutesValue,
        piqueld_core::api::PreviewLimitReached,
        piqueld_core::exec::ExecRequest,
        piqueld_core::api::DeploymentListView
    ))
)]
struct ApiDoc;

/// Returns the path-less base document that `documented_router` extends.
pub(super) fn base_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// What the structured error response every operation documents says.
const GENERIC_ERROR: &str = "Structured, sanitized error";

/// The generic structured error response, documented in the `OpenAPI` document.
#[expect(
    dead_code,
    reason = "Utoipa reads the field's type as the body schema; no value is ever built"
)]
#[derive(ToResponse)]
#[response(description = GENERIC_ERROR)]
struct ApiErrorResponse(ErrorBody);

/// The structured error responses an operation documents, one per status,
/// listed in its `#[utoipa::path]` beside its success responses:
///
/// ```text
/// responses((status = 200, body = Envelope<View>), ApiErrors<400, 404, 500, 503>)
/// ```
///
/// Statuses are ascending 4xx and 5xx codes, checked when the crate is built
/// (not by `cargo check` or Clippy); unused trailing parameters stay 0.
pub(super) struct ApiErrors<
    const A: u16,
    const B: u16 = 0,
    const C: u16 = 0,
    const D: u16 = 0,
    const E: u16 = 0,
    const F: u16 = 0,
    const G: u16 = 0,
    const H: u16 = 0,
    const I: u16 = 0,
    const J: u16 = 0,
>;

impl<
    const A: u16,
    const B: u16,
    const C: u16,
    const D: u16,
    const E: u16,
    const F: u16,
    const G: u16,
    const H: u16,
    const I: u16,
    const J: u16,
> utoipa::IntoResponses for ApiErrors<A, B, C, D, E, F, G, H, I, J>
{
    fn responses() -> BTreeMap<String, RefOr<Response>> {
        let statuses = const {
            let statuses = [A, B, C, D, E, F, G, H, I, J];
            let mut index = 0;
            while index < statuses.len() {
                let status = statuses[index];
                let previous = if index == 0 { 399 } else { statuses[index - 1] };
                assert!(
                    if status == 0 {
                        index > 0
                    } else {
                        previous != 0 && previous < status && status < 600
                    },
                    "ApiErrors statuses must be ascending 4xx and 5xx codes"
                );
                index += 1;
            }
            statuses
        };
        let (_, response) = ApiErrorResponse::response();
        statuses
            .into_iter()
            .take_while(|status| *status != 0)
            .map(|status| (status.to_string(), response.clone()))
            .collect()
    }
}

/// The `reauthentication_required` response, documented in the `OpenAPI` document.
#[expect(
    dead_code,
    reason = "Utoipa reads the field's type as the body schema; no value is ever built"
)]
#[derive(ToResponse)]
#[response(
    description = "`reauthentication_required`: the action needs a passkey sign-in within the last 2 minutes. When `details.step_up` is true, confirm with a passkey (`authReauthenticateStart`) and send the request again, except a refused registration finish, which consumed its ceremony: start again from `authRegistrationStart`. API tokens never can"
)]
pub(super) struct ReauthenticationResponse(ErrorBody);

/// Serves the published `OpenAPI` document.
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openApiDocument",
    summary = "Get the `OpenAPI` document",
    responses(
        (status = 200, description = "OpenAPI 3.0 document", body = Object, content_type = "application/vnd.oai.openapi+json")
    )
)]
pub(super) async fn openapi(
    Extension(PublishedDocument(document)): Extension<PublishedDocument>,
) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.0",
        )],
        document,
    )
}

/// The published `OpenAPI` 3.0 document, serialized once when the router is
/// built rather than on every request.
#[derive(Clone)]
pub(super) struct PublishedDocument(Bytes);

impl PublishedDocument {
    /// Publishes `document` (see [`openapi_30_document`]).
    pub(super) fn new(document: &utoipa::openapi::OpenApi) -> Self {
        Self(openapi_30_document(document).to_string().into())
    }
}

/// Generates the `OpenAPI` contract from Utoipa endpoint metadata.
///
/// # Panics
///
/// Panics if Utoipa's generated document cannot be serialized.
#[must_use]
pub fn openapi_document() -> Value {
    openapi_30_document(&super::documented_router().into_openapi())
}

/// Serializes Utoipa's `OpenAPI` 3.1 output as the published 3.0.3 contract.
///
/// Besides schema downgrading, it drops the license `identifier` (3.1 only),
/// declares bearer-token and session-cookie security as the default (public
/// endpoints clear it, see `access::Access::declare`), strips
/// `nullable` from optional parameters, and documents the TCP 403 and body
/// timeout 408 responses.
#[expect(
    clippy::expect_used,
    reason = "Utoipa documents have string keys and JSON-compatible fields."
)]
fn openapi_30_document(document: &utoipa::openapi::OpenApi) -> Value {
    let mut document = serde_json::to_value(document)
        .expect("Utoipa OpenAPI fields and string-keyed maps are JSON-compatible");
    convert_to_openapi_30(&mut document);
    document["openapi"] = Value::String("3.0.3".into());
    document["info"]["license"]
        .as_object_mut()
        .expect("ApiDoc declares a license, which Utoipa serializes as an object")
        .remove("identifier");
    document["components"]["securitySchemes"] = serde_json::json!({
        "bearerAuth": {"type":"http", "scheme":"bearer"},
        "browserSession": {"type":"apiKey", "in":"cookie", "name":"piqueld_session"}
    });
    document["security"] = serde_json::json!([{"bearerAuth":[]},{"browserSession":[]}]);
    remove_nullable_parameters(&mut document);
    complete_http_contract(&mut document);
    document
}

/// Converts Utoipa's JSON Schema output to its `OpenAPI` 3.0 equivalent.
///
/// Recursively rewrites nullability, since 3.0 has no `null` type:
///
/// ```text
/// {"type": ["string", "null"]}              -> {"type": "string", "nullable": true}
/// {"oneOf": [{"$ref": R}, {"type": "null"}]} -> {"oneOf": [{"$ref": R}, <nullable null enum>]}
/// {"oneOf": [{inline}, {"type": "null"}]}   -> {inline..., "nullable": true}
/// ```
///
/// References cannot carry `nullable` in 3.0 (siblings of `$ref` are ignored),
/// so they keep a `oneOf` with a null-only alternative.
fn convert_to_openapi_30(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                convert_to_openapi_30(value);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                convert_to_openapi_30(value);
            }
            object.remove("propertyNames");

            if let Some(Value::Array(types)) = object.get("type") {
                let non_null = types
                    .iter()
                    .filter(|value| value.as_str() != Some("null"))
                    .cloned()
                    .collect::<Vec<_>>();
                if non_null.len() == 1 && non_null.len() != types.len() {
                    object.insert("type".into(), non_null[0].clone());
                    object.insert("nullable".into(), Value::Bool(true));
                }
            }

            let nullable_schema =
                object
                    .get("oneOf")
                    .and_then(Value::as_array)
                    .and_then(|schemas| {
                        let non_null = schemas
                            .iter()
                            .filter(|schema| {
                                schema.get("type").and_then(Value::as_str) != Some("null")
                            })
                            .collect::<Vec<_>>();
                        (non_null.len() == 1 && non_null.len() != schemas.len())
                            .then(|| non_null[0].clone())
                    });
            if let Some(mut schema) = nullable_schema {
                object.remove("oneOf");
                if let Some(schema) = schema.as_object_mut()
                    && let Some(reference) = schema.remove("$ref")
                {
                    let mut referenced = serde_json::json!({ "$ref": reference });
                    if !schema.is_empty() {
                        schema.insert("allOf".into(), serde_json::json!([referenced]));
                        referenced = Value::Object(schema.clone());
                    }
                    object.insert(
                        "oneOf".into(),
                        serde_json::json!([
                            referenced,
                            { "type": "string", "nullable": true, "enum": [null] }
                        ]),
                    );
                } else if let Some(schema) = schema.as_object() {
                    object.extend(schema.clone());
                    object.insert("nullable".into(), Value::Bool(true));
                }
            }
        }
        _ => {}
    }
}

/// Adds the 403 returned by the TCP browser trust middleware
/// (`browser::BrowserPolicy`) to every documented operation, after any 403
/// it documents itself, like [`ReauthenticationResponse`], and the 408 of a
/// request body that stopped arriving to those that take one.
fn complete_http_contract(document: &mut Value) {
    let error = |description: &str| {
        serde_json::json!({
            "description": description,
            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorBody"}}}
        })
    };
    for operation in operations_mut(document) {
        let has_body = operation.contains_key("requestBody");
        if let Some(responses) = operation
            .get_mut("responses")
            .and_then(Value::as_object_mut)
        {
            let untrusted = "TCP authority or browser origin is not trusted";
            let own = responses
                .get("403")
                .and_then(|response| response.get("description"))
                .and_then(Value::as_str)
                .filter(|own| *own != GENERIC_ERROR);
            let description = match own {
                Some(own) => format!("{own}. Also returned when the {untrusted}"),
                None => untrusted.into(),
            };
            responses.insert("403".into(), error(&description));
            if has_body {
                responses
                    .entry("408")
                    .or_insert_with(|| error("The request body stopped arriving"));
            }
        }
    }
}

/// Optional HTTP parameters are absent rather than represented as JSON null.
fn remove_nullable_parameters(document: &mut Value) {
    for operation in operations_mut(document) {
        let Some(parameters) = operation
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for parameter in parameters.iter_mut().filter_map(Value::as_object_mut) {
            if parameter.get("required").and_then(Value::as_bool) != Some(true)
                && let Some(schema) = parameter.get_mut("schema").and_then(Value::as_object_mut)
            {
                schema.remove("nullable");
            }
        }
    }
}

/// Every operation object under the serialized document's `paths`.
fn operations_mut(document: &mut Value) -> impl Iterator<Item = &mut Map<String, Value>> {
    document
        .get_mut("paths")
        .and_then(Value::as_object_mut)
        .into_iter()
        .flat_map(|paths| paths.values_mut().filter_map(Value::as_object_mut))
        .flat_map(|item| item.values_mut().filter_map(Value::as_object_mut))
}
