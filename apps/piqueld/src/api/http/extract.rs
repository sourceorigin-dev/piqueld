//! Request decoding that keeps malformed input inside the API's structured
//! error contract: path parameters, query strings, bodies, and single-valued
//! headers each fail with a stable 4xx `ApiError` rather than Axum's plain
//! text rejections. A [`Mutator`] extracts the caller of a handler that
//! submits a change, and submits it.
use super::{ApiError, ApiState, JSON, TOML, accepted, access::Audited, ok};
use crate::api::{Mutation, MutationResponse, Restore};
use crate::auth::Identity;
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{
        FromRequest, FromRequestParts, Path, Query, Request,
        rejection::{BytesRejection, ExtensionRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts},
    response::Response,
};
use piqueld_core::ApiErrorCode;
use piqueld_core::api::{ApplyApplicationRequest, PromotionSelection};
use piqueld_core::manifest::ValidatedTemplate;
use piqueld_core::{ApplicationId, EnvironmentId, GitBranch, PreviewSlot};
use piqueld_store::Author;
use serde::de::DeserializeOwned;
use utoipa::openapi::path::{Parameter, ParameterBuilder, ParameterIn};
use utoipa::openapi::{ObjectBuilder, Required, Type};

/// Header carrying a mutation's idempotency key.
pub(super) const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Documents the optional [`IDEMPOTENCY_KEY`] header of a mutation that takes
/// one, when listed in its `#[utoipa::path]` `params(...)`.
pub(super) struct IdempotencyKey;

impl utoipa::IntoParams for IdempotencyKey {
    fn into_params(_: impl Fn() -> Option<ParameterIn>) -> Vec<Parameter> {
        vec![
            ParameterBuilder::new()
                .name("Idempotency-Key")
                .parameter_in(ParameterIn::Header)
                .required(Required::False)
                .schema(Some(ObjectBuilder::new().schema_type(Type::String)))
                .build(),
        ]
    }
}

/// The request of a handler submitting a mutation: the caller, acting
/// through its audited request, and the request's `Idempotency-Key`. It
/// extracts what the handler's leading `State`, `Extension<Identity>`,
/// `Audited` and `HeaderMap` would, failing as they would.
pub(super) struct Mutator {
    state: ApiState,
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
    /// This request, whose route answers every change it makes with
    /// `status` (see `Audited::answering`).
    pub(super) fn answering(self, status: StatusCode) -> Self {
        Self {
            audited: self.audited.answering(status),
            ..self
        }
    }

    /// The caller acting through this request.
    fn author(&self) -> Author<'_> {
        self.audited.author(&self.identity)
    }

    /// The request's headers.
    pub(super) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The request's `Idempotency-Key`, refused when repeated or not ASCII.
    fn idempotency_key(&self) -> Result<Option<String>, ApiError> {
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

    /// Promotes the release `selection` picks into the environment `id`,
    /// answering 202 with the pinned promotion.
    pub(super) async fn promote(
        &self,
        id: String,
        selection: impl FnOnce() -> Result<PromotionSelection, ApiError>,
        expected_generation: Option<u64>,
        force: bool,
    ) -> Result<Response, ApiError> {
        let request_id = self.idempotency_key()?;
        Ok(accepted(
            self.state
                .promote(
                    self.author(),
                    &EnvironmentId::parse(id)?,
                    selection()?,
                    expected_generation,
                    force,
                    request_id.as_deref(),
                )
                .await?,
        ))
    }

    /// Deploys the snapshot `bundle`, with its announced length, to
    /// `preview`, answering 202 with the preview.
    pub(super) async fn deploy_snapshot(
        &self,
        preview: (ApplicationId, GitBranch, Option<PreviewSlot>),
        (bundle, length): (Body, Option<u64>),
    ) -> Result<Response, ApiError> {
        let request_id = self.idempotency_key()?;
        let deployed = self.state.deploy_snapshot(
            self.author(),
            preview,
            (bundle.into_data_stream(), length),
            request_id.as_deref(),
        );
        match Box::pin(deployed).await? {
            MutationResponse::Preview(preview) => Ok(accepted(preview)),
            _ => Err(piqueld_store::StoreError::Corrupt.into()),
        }
    }

    /// Deletes those of `application`'s `previews` whose branch is gone,
    /// answering 200 with the previews whose deletion was accepted.
    pub(super) async fn prune_previews(
        &self,
        application: &ApplicationId,
        previews: &[EnvironmentId],
    ) -> Result<Response, ApiError> {
        Ok(ok(self
            .state
            .prune_previews(self.author(), application, previews)
            .await?))
    }
}

/// Answers `data` with 202 Accepted when its mutation `started` durable
/// work, else 200 OK (see `MutationResponse::started`).
fn reply<T: serde::Serialize>(started: bool, data: T) -> Response {
    if started { accepted(data) } else { ok(data) }
}

/// Path extractor rejecting undecodable parameters with 400 `path_invalid`.
pub(super) struct ApiPath<T>(pub(super) T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    ApiErrorCode::PathInvalid,
                    "invalid URL path parameter",
                )
            })
    }
}

/// Query string parameters, decoded by [`ApiQuery`].
pub(super) trait QueryParams: DeserializeOwned {
    /// Code of the 400 answering a query string that does not decode, e.g.
    /// `force_invalid`.
    const INVALID_CODE: ApiErrorCode;
    /// Message of that 400, e.g. `force must be true or false`.
    const INVALID_MESSAGE: &'static str;
}

/// Query extractor rejecting undecodable query strings with `T`'s
/// `INVALID_CODE` and `INVALID_MESSAGE`.
pub(super) struct ApiQuery<T>(pub(super) T);

impl<S: Send + Sync, T: QueryParams> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| {
                ApiError::new(StatusCode::BAD_REQUEST, T::INVALID_CODE, T::INVALID_MESSAGE)
            })
    }
}

/// Strict JSON body extractor: requires `Content-Type: application/json`
/// (415 otherwise), then reads the body (see [`request_body`]) and decodes
/// it with [`decode_json`].
pub(super) struct ApiJson<T>(pub(super) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        if content_type(request.headers()) != Some(JSON) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ApiErrorCode::ContentTypeUnsupported,
                "Content-Type must be application/json",
            ));
        }
        let body = request_body(Bytes::from_request(request, state).await)?;
        decode_json(&body).map(Self)
    }
}

/// A validated manifest upload with its optional preconditions, in either
/// supported encoding:
/// - JSON: an `ApplyApplicationRequest` carrying the preconditions as fields.
/// - TOML (`application/toml` or `text/toml`): the raw manifest, with
///   preconditions in the single-valued `x-expected-generation` and
///   `x-expected-application-id` headers.
///
/// Any other content type fails with 415.
pub(super) struct ManifestUpload {
    pub(super) manifest: ValidatedTemplate,
    /// The inspected application revision.
    pub(super) expected_generation: Option<u64>,
    /// The inspected application identity.
    pub(super) expected_application_id: Option<String>,
}

impl<S: Send + Sync> FromRequest<S> for ManifestUpload {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let headers = request.headers().clone();
        let body = request_body(Bytes::from_request(request, state).await)?;
        Self::decode(&headers, &body)
    }
}

impl ManifestUpload {
    /// Decodes and validates `body` in the encoding `headers` name.
    fn decode(headers: &HeaderMap, body: &[u8]) -> Result<Self, ApiError> {
        match content_type(headers) {
            Some(value) if value.eq_ignore_ascii_case(JSON) => {
                let request: ApplyApplicationRequest = decode_json(body)?;
                Ok(Self {
                    manifest: request.manifest.validate_template()?,
                    expected_generation: request.expected_generation,
                    expected_application_id: request.expected_application_id,
                })
            }
            Some(value)
                if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
            {
                let text = std::str::from_utf8(body).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        ApiErrorCode::TomlMalformed,
                        "request TOML is malformed",
                    )
                })?;
                let invalid = |message| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        ApiErrorCode::GenerationInvalid,
                        message,
                    )
                };
                let expected_generation = single_header(headers, "x-expected-generation", || {
                    invalid("expected generation must occur once")
                })?
                .map(|value| {
                    value
                        .to_str()
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or_else(|| invalid("expected generation must be an unsigned integer"))
                })
                .transpose()?;
                Ok(Self {
                    manifest: piqueld_core::manifest::parse_template_toml(text)?,
                    expected_generation,
                    expected_application_id: optional_header(headers, "x-expected-application-id")?,
                })
            }
            _ => Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ApiErrorCode::ContentTypeUnsupported,
                "Content-Type must be application/json or application/toml",
            )),
        }
    }
}

/// Returns the media type from `Content-Type`, without parameters.
///
/// ```text
/// "application/json; charset=utf-8" -> "application/json"
/// ```
pub(super) fn content_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim())
}

/// Unwraps a buffered body, reporting the size limit as 413
/// `request_body_too_large`, a body that stopped arriving (see
/// `BODY_IDLE_TIMEOUT`) as 408 `request_body_timeout`, and other read
/// failures as 400.
pub(super) fn request_body(body: Result<Bytes, BytesRejection>) -> Result<Bytes, ApiError> {
    body.map_err(|rejection| {
        if let Some(timed_out) = body_timeout(&rejection) {
            timed_out
        } else if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ApiErrorCode::RequestBodyTooLarge,
                "request body exceeds the maximum allowed size",
            )
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::RequestBodyUnreadable,
                "request body could not be read",
            )
        }
    })
}

/// 408 `request_body_timeout` when reading a body failed because it stopped
/// arriving (see `BODY_IDLE_TIMEOUT`); `None` for any other failure.
pub(super) fn body_timeout(rejection: &BytesRejection) -> Option<ApiError> {
    std::iter::successors(
        Some(rejection as &(dyn std::error::Error + 'static)),
        |error| error.source(),
    )
    .any(<dyn std::error::Error>::is::<tower_http::timeout::TimeoutError>)
    .then(|| {
        ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            ApiErrorCode::RequestBodyTimeout,
            "request body stopped arriving before it was complete",
        )
    })
}

/// Strictly decodes a JSON body, rejecting trailing data. The failing field
/// path is only logged at debug level (sanitized); clients get a generic
/// `json_malformed` error.
pub(super) fn decode_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    let malformed = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ApiErrorCode::JsonMalformed,
            "request JSON is malformed or contains unknown fields",
        )
    };
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = piqueld_core::manifest::safe_decode_path(&error.path().to_string());
        tracing::debug!(%path, "request JSON was rejected");
        malformed()
    })?;
    deserializer.end().map_err(|_| malformed())?;
    Ok(value)
}

/// Reads an optional single-valued header; repeated or non-ASCII values are
/// rejected with `header_invalid`.
pub(super) fn optional_header(headers: &HeaderMap, name: &str) -> Result<Option<String>, ApiError> {
    let invalid = |message| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            ApiErrorCode::HeaderInvalid,
            message,
        )
    };
    single_header(headers, name, || {
        invalid("mutation headers must occur once")
    })?
    .map(|value| {
        value
            .to_str()
            .map(str::to_owned)
            .map_err(|_| invalid("invalid mutation header"))
    })
    .transpose()
}

/// The value of header `name`, which may occur at most once; `repeated`
/// builds the error when it occurs more often.
pub(super) fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    repeated: impl FnOnce() -> ApiError,
) -> Result<Option<&'a HeaderValue>, ApiError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(repeated());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A body that stops arriving times out as 408 `request_body_timeout`,
    /// not as an unreadable body.
    #[tokio::test]
    async fn stalled_bodies_time_out() {
        let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let body = tower_http::timeout::TimeoutBody::new(
            Duration::from_millis(10),
            axum::body::Body::from_stream(stalled),
        );
        let request = Request::new(axum::body::Body::new(body));
        let error = request_body(Bytes::from_request(request, &()).await).unwrap_err();
        assert_eq!(error.status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(error.body().code, ApiErrorCode::RequestBodyTimeout);
    }
}
