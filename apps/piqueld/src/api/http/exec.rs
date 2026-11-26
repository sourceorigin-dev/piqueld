//! `WebSocket` connections streaming one-off commands. See `piqueld_core::exec`.
use super::{
    ApiError, ApiPath, ApiState, ConnectionTiming,
    access::Audit,
    auth::{FreshTailnet, TailnetSource},
    extract::decode_json,
    lease::{Ended, Lease},
    openapi::ApiErrors,
};
use crate::{
    api::{ExecSession, Expensive},
    auth::Identity,
};
use axum::{
    Extension,
    body::Bytes,
    extract::{
        FromRequestParts, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, request::Parts},
    response::Response,
};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use piqueld_core::ApiErrorCode;
use piqueld_core::{
    EnvironmentId,
    exec::{ExecFrame, ExecInput, ExecOutput, ExecRequest, MAX_MESSAGE_BYTES},
};
use piqueld_docker::ExecIo;
use std::time::Duration;
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

/// How long a client may take to send its `ExecRequest` once connected, so
/// a connection that sends nothing never keeps its exec slot.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A WebSocket upgrade whose rejections use the API's JSON errors.
pub(super) struct ExecUpgrade(WebSocketUpgrade);

impl<S: Send + Sync> FromRequestParts<S> for ExecUpgrade {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        WebSocketUpgrade::from_request_parts(parts, state)
            .await
            .map(Self)
            .map_err(|error| {
                tracing::debug!(%error, "exec request is not a WebSocket upgrade");
                ApiError::new(
                    StatusCode::UPGRADE_REQUIRED,
                    ApiErrorCode::UpgradeRequired,
                    "Open a WebSocket to stream a command",
                )
            })
    }
}

/// Runs an interactive command in a service's task over a WebSocket.
#[utoipa::path(get,path="/api/v1/environments/{id}/exec",operation_id="execEnvironmentCommand",params(("id"=String,Path)),
 description="Opens a WebSocket whose first message is a JSON ExecRequest; see piqueld_core::exec.",
 responses((status=101,description="Switched to a WebSocket streaming the command"),
 ApiErrors<400, 426, 503>))]
pub(super) async fn exec(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ExecUpgrade(upgrade): ExecUpgrade,
    Extension(identity): Extension<Identity>,
    Extension(audit): Extension<Audit>,
    tailnet: Option<Extension<piqueld_core::tailnet::TailnetPeer>>,
    source: Option<TailnetSource>,
) -> Result<Response, ApiError> {
    let mut tailnet = tailnet.map(|Extension(peer)| peer);
    let id = EnvironmentId::parse(id)?;
    let request_id = audit.request_id().map(str::to_owned);
    let admitted = state.admit(Expensive::Exec)?;
    let timing = state.connection_timing();
    Ok(upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .on_failed_upgrade(|error| tracing::warn!(?error, "exec connection upgrade failed"))
        .on_upgrade(move |socket| async move {
            let _admitted = admitted;
            let (mut writer, mut reader) = socket.split();
            let result = async {
                let request = read_request(&mut reader).await?;
                if let Some(fresh) = FreshTailnet::check(&identity, source.as_ref()).await {
                    tailnet = fresh.peer().cloned();
                    fresh.verify(&identity)?;
                }
                let mut lease = Lease::new(state.clone(), identity.clone(), source);
                let environment = &id;
                let mut check = move |state: ApiState, identity: Identity| async move {
                    Ok(state.may_exec(identity.actor(), environment).await?)
                };
                let caller = identity.actor();
                let session = state.exec(caller, &id, &request, &identity.who()).await?;
                match lease.recheck(&mut check).await {
                    Ok(()) => {}
                    Err(Ended::Refused(error)) => return Err(error),
                    Err(Ended::Shutdown) => return Ok(None),
                }
                let ended = lease.ended(check);
                Ok(relay(session, reader, &mut writer, ended, timing).await)
            }
            .await;
            let last = match result.unwrap_or_else(|error| Some(Err(error))) {
                None => None,
                Some(Ok(code)) => Some(ExecOutput::Exit(code)),
                Some(Err(error)) => {
                    if error.denied.is_some() || error.status == StatusCode::UNAUTHORIZED {
                        let tailnet = tailnet.as_ref();
                        audit.refused_later(&state, identity.clone(), tailnet, &error);
                    }
                    let mut body = error.body();
                    if let Some(request_id) = request_id {
                        body.request_id = request_id;
                    }
                    let diagnostic = error.diagnostic.map(|diagnostic| *diagnostic);
                    let caller = identity.actor();
                    state
                        .record_failure(
                            error.status,
                            &mut body,
                            diagnostic,
                            Some(&id),
                            caller.attribution(),
                        )
                        .await;
                    Some(ExecOutput::Failed {
                        status: error.status.as_u16(),
                        error: body,
                    })
                }
            };
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a client that stopped reading cannot be told, and the bound below ends the wait"
            )]
            let finish = async {
                if let Some(last) = last {
                    let _ = writer.send(Message::Binary(last.encode().into())).await;
                }
                let _ = writer.close().await;
            };
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the close is best effort; the connection ends either way"
            )]
            let _ = tokio::time::timeout(timing.close, finish).await;
        }))
}

/// Reads the first message, which names the command to start, failing with
/// 408 `exec_request_timeout` when none arrives within [`REQUEST_TIMEOUT`].
async fn read_request(reader: &mut SplitStream<WebSocket>) -> Result<ExecRequest, ApiError> {
    let first = async {
        loop {
            match reader.next().await {
                Some(Ok(Message::Text(text))) => return decode_json(text.as_bytes()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        ApiErrorCode::ExecRequestMissing,
                        "The first exec message must be a JSON ExecRequest",
                    ));
                }
            }
        }
    };
    tokio::time::timeout(REQUEST_TIMEOUT, first)
        .await
        .unwrap_or_else(|_| {
            Err(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                ApiErrorCode::ExecRequestTimeout,
                "No ExecRequest arrived within 30 seconds of connecting",
            ))
        })
}

/// Relays messages between the client and the running command until it
/// exits, the client leaves, or `ended` resolves, paced by `timing`. Returns `None` when the
/// command failed because the client left, since that is neither a failure to
/// record nor one anybody can receive, and when the daemon shuts down.
async fn relay(
    session: ExecSession,
    mut reader: SplitStream<WebSocket>,
    writer: &mut SplitSink<WebSocket, Message>,
    ended: impl Future<Output = Ended>,
    timing: ConnectionTiming,
) -> Option<Result<i64, ApiError>> {
    let (input, input_rx) = mpsc::channel(16);
    let (output_tx, mut output) = mpsc::channel::<ExecOutput>(16);
    let stop = CancellationToken::new();
    let reading = tokio::spawn({
        let stop = stop.clone();
        async move {
            let left = read_input(&mut reader, input).await;
            if left {
                stop.cancel();
            }
            left
        }
    });
    let writing = async {
        let mut ping = tokio::time::interval_at(Instant::now() + timing.ping, timing.ping);
        loop {
            let message = tokio::select! {
                frame = output.recv() => match frame {
                    Some(frame) => Message::Binary(frame.encode().into()),
                    None => return true,
                },
                _ = ping.tick() => Message::Ping(Bytes::new()),
            };
            let sent = tokio::select! {
                biased;
                () = stop.cancelled() => false,
                sent = writer.send(message) => sent.is_ok(),
            };
            if !sent {
                stop.cancel();
                return false;
            }
        }
    };
    let io = ExecIo {
        input: input_rx,
        output: output_tx,
        stop: stop.clone(),
    };
    let running = async { tokio::join!(session.run(io), writing) };
    tokio::pin!(running);
    let mut early = None;
    let mut finished = tokio::select! {
        biased;
        finished = &mut running => Some(finished),
        ended = ended => {
            early = Some(ended);
            None
        }
        () = stop.cancelled() => None,
    };
    reading.abort();
    if finished.is_none() {
        stop.cancel();
        finished = tokio::time::timeout(timing.close, &mut running).await.ok();
    }
    match early {
        Some(Ended::Refused(error)) => return Some(Err(error)),
        Some(Ended::Shutdown) => return None,
        None => {}
    }
    let (result, delivered) = finished?;
    let left = !delivered || matches!(reading.await, Ok(true));
    match result {
        Err(_) if left => None,
        result => Some(result.map_err(ApiError::from)),
    }
}

/// Forwards the client's input to `input` until the client ends it or the
/// command no longer reads it. Returns whether the client ended it.
async fn read_input(reader: &mut SplitStream<WebSocket>, input: mpsc::Sender<ExecInput>) -> bool {
    while let Some(Ok(message)) = reader.next().await {
        let data = match message {
            Message::Binary(data) => data,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Text(_) | Message::Close(_) => return true,
        };
        match ExecInput::decode(&data) {
            Ok(frame) => {
                if input.send(frame).await.is_err() {
                    return false;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "rejected exec input");
                return true;
            }
        }
    }
    true
}
