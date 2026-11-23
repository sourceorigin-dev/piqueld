//! Container output of an environment's services: recent output read once,
//! or followed live as server-sent events.
use super::{
    ApiError, ApiPath, ApiQuery, ApiState, QueryParams,
    auth::TailnetSource,
    lease::Lease,
    live::{self, Follow},
    ok,
    openapi::ApiErrors,
};
use crate::{
    api::{ApplicationError, Expensive, LogFollower},
    auth::Identity,
};
use axum::{
    Extension,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, sse::Event as ServerEvent},
};
use piqueld_core::ApiErrorCode;
use piqueld_core::{
    EnvironmentId,
    access::AppPermission,
    api::{ApplicationLogs, Envelope, LogTime, StreamMessage},
};
/// Query parameters for recent container output.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LogQuery {
    /// Only include output from this service (1–63 characters); all services when omitted.
    service: Option<String>,
    /// Only include this output stream; merged output when omitted.
    stream: Option<piqueld_core::api::LogStream>,
    /// Maximum number of most recent lines, merged across the selected services.
    #[param(minimum = 1, maximum = 1000, default = 200)]
    tail: u16,
    /// Only include output from this many seconds ago onward.
    #[param(minimum = 1, maximum = 86400, default = 3600)]
    since_seconds: u32,
}
impl Default for LogQuery {
    /// Matches the documented `tail` and `since_seconds` parameter defaults.
    fn default() -> Self {
        Self {
            service: None,
            stream: None,
            tail: 200,
            since_seconds: 3600,
        }
    }
}

impl QueryParams for LogQuery {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::LogsQueryInvalid;
    const INVALID_MESSAGE: &'static str = "Invalid log query";
}

/// Gets recent container output for an environment.
///
/// Output can be narrowed to one service and one stream. Unknown or malformed
/// parameters, out-of-range `tail` or `since_seconds` values, and `service`
/// names outside 1–63 characters fail with 400 `logs_query_invalid`.
#[utoipa::path(get,path="/api/v1/environments/{id}/logs",operation_id="environmentLogs",params(("id"=String,Path),LogQuery),
 responses((status=200,description="Recent Docker output",body=Envelope<ApplicationLogs>),
 ApiErrors<400, 404, 502, 503>))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<LogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let id = EnvironmentId::parse(id)?;
    let _admitted = state.admit(crate::api::Expensive::Logs)?;
    Ok(ok(state
        .logs(
            &id,
            query.service.as_deref(),
            query.tail,
            query.since_seconds,
            query.stream,
        )
        .await?))
}

/// Query parameters for following container output.
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LogStreamQuery {
    /// Only include output from this service (1–63 characters); all services when omitted.
    service: Option<String>,
    /// Only include this output stream; both when omitted.
    stream: Option<piqueld_core::api::LogStream>,
    /// Only include output written after this RFC 3339 UTC timestamp, at most a
    /// day back; from when the stream opens when omitted. A `Last-Event-ID`
    /// header takes its place.
    after: Option<String>,
}

impl QueryParams for LogStreamQuery {
    const INVALID_CODE: ApiErrorCode = ApiErrorCode::LogsQueryInvalid;
    const INVALID_MESSAGE: &'static str = "Invalid log query";
}

/// Follows container output of an environment's services as server-sent
/// events.
///
/// Sends a `log` message for each line as Docker writes it, its data a
/// `LogRecord` and its ID the newest timestamp sent so far, which a
/// reconnecting client resumes after. Each container's lines come in order;
/// lines of several containers interleave as they arrive, so a resume can
/// repeat or skip lines written within a moment of the newest one sent,
/// such as during the first replayed lines. Tasks that start while the
/// stream is open, such as a rollout's, are followed too.
/// Malformed parameters and timestamps fail with 400 `logs_query_invalid`.
/// The stream ends with a `stream_error` message once Docker can no longer
/// be read, with an `access_ended` message whose data is the `ErrorBody` of
/// the request that would now be refused once the caller loses access, and
/// without a message when the daemon shuts down.
#[utoipa::path(get,path="/api/v1/environments/{id}/logs/stream",operation_id="streamEnvironmentLogs",params(("id"=String,Path),LogStreamQuery),
 responses((status=200,description="SSE of `log` messages, each a `LogRecord`; IDs are the newest timestamp sent",body=String,content_type="text/event-stream"),
 ApiErrors<400, 404, 502, 503>))]
pub(super) async fn stream(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    source: Option<TailnetSource>,
    headers: HeaderMap,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<LogStreamQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let id = EnvironmentId::parse(id)?;
    let after = headers
        .get("last-event-id")
        .map(|value| value.to_str().ok().map(str::to_owned))
        .or(query.after.map(Some))
        .map(|after| {
            after
                .as_deref()
                .and_then(LogTime::parse)
                .ok_or(ApplicationError::InvalidLogQuery)
        })
        .transpose()?;
    let admitted = state.admit(Expensive::LogStream)?;
    let logs = state
        .follow_logs(
            &id,
            query.service.as_deref(),
            query.stream,
            after.map(|after| after.0),
        )
        .await?;
    let follow = FollowLogs {
        _admitted: admitted,
        lease: Lease::new(state.clone(), identity, source),
        environment: id,
        logs,
        newest: String::new(),
        done: false,
    };
    Ok(follow.serve())
}

/// A live log stream: its environment's output, and the caller's access.
struct FollowLogs {
    /// The stream's share of the log stream budget, while it is open.
    _admitted: tokio::sync::OwnedSemaphorePermit,
    lease: Lease,
    environment: EnvironmentId,
    logs: LogFollower,
    /// The newest timestamp sent, each message's ID. Timestamps share one
    /// fixed-width format, so they compare as text.
    newest: String,
    /// Whether a final message was sent.
    done: bool,
}

impl Follow for FollowLogs {
    /// Access is rechecked before every message once due, and while no
    /// output comes.
    async fn next(&mut self) -> Option<ServerEvent> {
        if self.done {
            return None;
        }
        loop {
            if self.lease.due() {
                let environment = self.environment.clone();
                let checked = self
                    .lease
                    .recheck(|state: ApiState, identity: Identity| async move {
                        let grants = state.grants(identity.actor()).await?;
                        let owner = state.environment_owner(&environment).await?;
                        Ok(grants.require_change(
                            &[AppPermission::LogsRead],
                            piqueld_store::EnvironmentOwner::target(owner.as_ref()),
                        )?)
                    })
                    .await;
                if let Err(ended) = checked {
                    self.done = true;
                    return ended.message();
                }
            }
            tokio::select! {
                record = self.logs.next() => return Some(match record {
                    Ok(record) => {
                        if record.timestamp > self.newest {
                            self.newest.clone_from(&record.timestamp);
                        }
                        let message = ServerEvent::default().id(self.newest.clone());
                        live::json(message, StreamMessage::Log, record)
                    }
                    Err(error) => {
                        tracing::warn!(%error, environment = %self.environment, "live log stream ended");
                        self.done = true;
                        live::named(StreamMessage::StreamError)
                            .data("Docker output could no longer be followed")
                    }
                }),
                () = self.lease.wait(std::future::pending::<()>()) => {}
            }
        }
    }
}
