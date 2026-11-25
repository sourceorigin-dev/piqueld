//! Authentication HTTP boundary shared by both daemon transports.
use super::access::Audited;
use super::{ApiError, ApiJson, openapi::ApiErrors};
use crate::auth::{Auth, AuthCookie, AuthError, Identity};
use crate::tailnet::{LookupTimedOut, TailnetLookup};
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use piqueld_core::ApiErrorCode;
use piqueld_core::auth::{
    AuthStatus, Ceremony, CeremonyFinish, DeviceApprove, DeviceConfirmRequest, DevicePoll,
    DeviceRequest, DeviceStart, DeviceStartRequest, DeviceToken, Directory, HostOperator, Manage,
    Managed, OperatorLink, OperatorSignIn, Principal, RegistrationStart, Session, SetupLink, User,
};
use piqueld_core::tailnet::TailnetPeer;
use piqueld_store::StoreError;
use std::{net::SocketAddr, sync::Arc};
use utoipa_axum::router::UtoipaMethodRouter;

/// Authentication boundary that every API router requires and applies itself.
/// Daemon listeners use the passkey [`Auth`] service; tests may supply a fake.
pub trait Authenticator: Clone + Send + Sync + 'static {
    /// Wraps every route and fallback already registered on `router`.
    fn guard<S: Clone + Send + Sync + 'static>(self, router: Router<S>) -> Router<S>;
}

impl Authenticator for Auth {
    /// Installs the `authenticate` middleware and exposes `Auth` to handlers.
    fn guard<S: Clone + Send + Sync + 'static>(self, router: Router<S>) -> Router<S> {
        router
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .layer(Extension(self))
    }
}

impl From<AuthError> for ApiError {
    /// Maps authentication failures. Passkey verification errors are logged
    /// and reported as a plain 401 so clients learn nothing about the cause.
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Webauthn(ref source) => {
                // Includes counter regressions that can indicate a cloned authenticator.
                tracing::warn!(error = %source, "passkey verification failed");
                Self::from(AuthError::Unauthorized)
            }
            AuthError::Unauthorized => Self::new(
                StatusCode::UNAUTHORIZED,
                ApiErrorCode::AuthenticationRequired,
                "Sign in with a passkey or run piquelctl login",
            ),
            AuthError::Invalid(message) => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCode::AuthenticationInvalid,
                message,
            ),
            AuthError::Busy => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                ApiErrorCode::AuthenticationBusy,
                "Too many authentication requests; try again shortly",
            ),
            AuthError::SetupCompleted => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SetupCompleted,
                "First-account setup is already completed; run piquelctl login",
            ),
            AuthError::SetupPending => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::SetupPending,
                "First-account setup is not completed; run piquelctl setup-link instead",
            ),
            AuthError::Store(StoreError::AlreadyExists) => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::AccountConflict,
                "Account name or passkey is already registered",
            ),
            AuthError::Store(source) => source.into(),
            AuthError::Denied(denied) => denied.into(),
            error => {
                tracing::error!(error = ?error, "authentication operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorCode::AuthenticationFailed,
                    "Authentication operation failed",
                )
            }
        }
    }
}

/// Reads one of `auth`'s cookies from the `Cookie` headers, all of them.
///
/// ```text
/// "a=1; piqueld_session=abc" + Session -> Some("abc")
/// ```
fn cookie<'a>(auth: &Auth, headers: &'a HeaderMap, cookie: AuthCookie) -> Option<&'a str> {
    let name = auth.cookie_name(cookie);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|entry| {
            let (key, value) = entry.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}
/// Authentication middleware for API paths; other paths pass through.
async fn authenticate(
    axum::extract::State(auth): axum::extract::State<Auth>,
    request: Request,
    next: Next,
) -> Response {
    if !super::ui::is_api_path(request.uri().path()) {
        return next.run(request).await;
    }
    authorize(&auth, request, next).await
}
/// Authenticates one API request before running the handler. On TCP, the
/// browser trust policy has already refused untrusted hosts and cross-site
/// fetches (see `browser`). Refusals come before anything is spent on the
/// request:
///
/// 1. Takes the credential from `Authorization: Bearer`, else the session cookie.
/// 2. Blocks cross-site mutations and upgrades: any mismatched `Origin` is
///    rejected, and cookie-authenticated mutations, and ceremony mutations
///    without a bearer token that authenticates, must send the configured
///    origin.
/// 3. Throttles ceremony start endpoints and host operator sign-in link
///    redemption per peer IP (429 with `Retry-After`), counting only
///    requests that passed the checks before.
/// 4. Without any `Authorization` or `Cookie` header, a Unix socket request
///    from root or the daemon's own user acts as the host operator (see
///    `UnixPeer`), also in refusals before this. Any such header, even one
///    that is unusable or cannot be parsed, rules it out.
/// 5. On the tailnet node's listener, looks the peer up once: afresh for a
///    token bound to a tailnet identity, which is refused unless the peer
///    matches it (see [`FreshTailnet::verify`]), and from the cache otherwise,
///    only to describe the request. Bound tokens are refused on other
///    listeners, too.
/// 6. Inserts the resolved `Identity` and tailnet peer as extensions, on the
///    request for handlers and on the response for outer layers, like the
///    audit trail. Missing or invalid credentials leave the identity out; the
///    route's access requirement then decides whether the request needs one
///    (see `access::enforce`).
async fn authorize(auth: &Auth, mut request: Request, next: Next) -> Response {
    let authorization = request.headers().get(header::AUTHORIZATION);
    // The scheme is case-insensitive (RFC 9110).
    let bearer = authorization
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    let session = cookie(auth, request.headers(), AuthCookie::Session);
    let secret = bearer.or(session);
    let operator = (authorization.is_none() && !request.headers().contains_key(header::COOKIE))
        .then(|| super::UnixPeer::host_operator(request.extensions()))
        .flatten();
    let target = (request.method(), request.headers(), request.uri().path());
    if cross_site(auth, target, bearer, session).await {
        let response = ApiError::new(
            StatusCode::FORBIDDEN,
            ApiErrorCode::OriginMismatch,
            "Request origin does not match the configured website",
        )
        .into_response();
        return refused(auth, secret, operator, response).await;
    }
    if request.method() == Method::POST
        && matches!(
            request.uri().path(),
            "/api/v1/auth/login/start"
                | "/api/v1/auth/register/start"
                | "/api/v1/auth/reauthenticate/start"
                | "/api/v1/auth/device/confirm"
                | "/api/v1/auth/device/start"
                | "/api/v1/auth/login/operator"
        )
    {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip());
        if let Err(error) = auth.admit_start(peer).await {
            let mut response = ApiError::from(error).into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
            return refused(auth, secret, operator, response).await;
        }
    }
    let mut identity = operator.map(|operator| Identity::operator(operator, None));
    if let Some(secret) = secret {
        match auth.authenticate(secret).await {
            Ok(resolved) => identity = Some(resolved),
            Err(AuthError::Unauthorized) => {}
            Err(error) => return ApiError::from(error).into_response(),
        }
    }
    // A bound token works only from a matching tailnet peer, checked with a
    // fresh lookup, which then also describes the request. Others are only
    // described, from the cache.
    let source = TailnetSource::of(request.extensions());
    let fresh = match &identity {
        Some(resolved) => FreshTailnet::check(resolved, source.as_ref()).await,
        None => None,
    };
    let tailnet = match (&fresh, &source) {
        (Some(fresh), _) => fresh.peer().cloned(),
        // The cached identity only describes the request, so a lookup that
        // timed out leaves it undescribed.
        (None, Some(source)) => source.whois(false).await.ok().flatten(),
        (None, None) => None,
    };
    if let (Some(fresh), Some(resolved)) = (&fresh, &identity)
        && let Err(error) = fresh.verify(resolved)
    {
        let mut response = error.into_response();
        response.extensions_mut().insert(resolved.clone());
        if let Some(peer) = tailnet {
            response.extensions_mut().insert(peer);
        }
        return response;
    }
    if let Some(peer) = &tailnet {
        request.extensions_mut().insert(peer.clone());
    }
    if let Some(identity) = &identity {
        request.extensions_mut().insert(identity.clone());
    }
    let mut response = next.run(request).await;
    // Outer layers, like the audit trail, see who made the request.
    if let Some(identity) = identity {
        response.extensions_mut().insert(identity);
    }
    if let Some(peer) = tailnet {
        response.extensions_mut().insert(peer);
    }
    response
}
/// Whether a `method` request to `path` with `headers`, carrying `bearer` or
/// `session`, is a mutation or upgrade that may come from another site. Any
/// foreign `Origin` is, and cookies must come with the configured one. So
/// must ceremonies, unless a bearer token that authenticates shows the
/// caller is no browser. A token that could not be checked, e.g. while
/// storage fails, is left for authentication to report.
async fn cross_site(
    auth: &Auth,
    (method, headers, path): (&Method, &HeaderMap, &str),
    bearer: Option<&str>,
    session: Option<&str>,
) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    // A WebSocket opens with GET, but browsers send cookies on cross-site
    // handshakes, so it is checked like a mutation.
    let mutation = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || headers.contains_key(header::UPGRADE);
    if !mutation || origin == Some(auth.origin()) {
        return false;
    }
    // Confirming a session with a passkey is no exception: it needs the
    // session cookie, so the cookie rule already asks for the Origin.
    let ceremony = path.contains("/auth/register/") || path.contains("/auth/login/");
    origin.is_some()
        || (bearer.is_none() && session.is_some())
        || (ceremony
            && match bearer {
                Some(token) => matches!(auth.identify(token).await, Err(AuthError::Unauthorized)),
                None => true,
            })
}
/// A fresh lookup of a request's tailnet peer, made to check a token bound
/// to a tailnet identity.
pub(super) struct FreshTailnet(Result<Option<TailnetPeer>, LookupTimedOut>);

impl FreshTailnet {
    /// Looks the request's peer up afresh when `identity` is bound to a
    /// tailnet identity; `None` for unbound identities. Requests from outside
    /// the tailnet node have no peer.
    pub(super) async fn check(identity: &Identity, source: Option<&TailnetSource>) -> Option<Self> {
        identity.tailnet.as_ref()?;
        Some(Self(match source {
            Some(source) => source.whois(true).await,
            None => Ok(None),
        }))
    }

    /// The peer, when the lookup identified one.
    pub(super) fn peer(&self) -> Option<&TailnetPeer> {
        self.0.as_ref().ok()?.as_ref()
    }

    /// Refuses `identity` unless the peer satisfies its binding: 503
    /// `tailnet_unavailable` when the lookup timed out, and 401
    /// `tailnet_binding_mismatch` from anywhere but a matching tailnet peer.
    pub(super) fn verify(&self, identity: &Identity) -> Result<(), ApiError> {
        let peer = self.0.as_ref().map_err(|LookupTimedOut| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCode::TailnetUnavailable,
                "The tailnet peer could not be identified in time; try again",
            )
        })?;
        let matches = identity
            .tailnet
            .as_ref()
            .zip(peer.as_ref())
            .is_some_and(|(binding, peer)| binding.matches(peer));
        if !matches {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                ApiErrorCode::TailnetBindingMismatch,
                "This token is bound to a tailnet identity; use it through the daemon's tailnet node from a matching device",
            ));
        }
        Ok(())
    }
}

/// Where a request came from on the tailnet node's listener: the lookup and
/// the connection's address. Absent on other listeners.
#[derive(Clone)]
pub(super) struct TailnetSource {
    lookup: Arc<dyn TailnetLookup>,
    peer: SocketAddr,
}

impl TailnetSource {
    fn of(extensions: &http::Extensions) -> Option<Self> {
        let lookup = extensions.get::<Arc<dyn TailnetLookup>>().cloned()?;
        let ConnectInfo(peer) = *extensions.get::<ConnectInfo<SocketAddr>>()?;
        Some(Self { lookup, peer })
    }

    /// The peer's tailnet identity, from the cache unless `fresh`.
    async fn whois(&self, fresh: bool) -> Result<Option<TailnetPeer>, LookupTimedOut> {
        self.lookup.whois(self.peer, fresh).await
    }
}

impl<S: Send + Sync> axum::extract::OptionalFromRequestParts<S> for TailnetSource {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut http::request::Parts,
        _: &S,
    ) -> impl Future<Output = Result<Option<Self>, Self::Rejection>> + Send {
        std::future::ready(Ok(Self::of(&parts.extensions)))
    }
}
/// Attaches a valid caller's identity to a refusal made before
/// authentication, so the audit trail names it, without refreshing its
/// credential: a refused request must not keep a session alive.
async fn refused(
    auth: &Auth,
    secret: Option<&str>,
    operator: Option<HostOperator>,
    mut response: Response,
) -> Response {
    if let Some(secret) = secret
        && let Ok((identity, _)) = auth.identify(secret).await
    {
        response.extensions_mut().insert(identity);
    } else if let Some(operator) = operator {
        response
            .extensions_mut()
            .insert(Identity::operator(operator, None));
    }
    response
}
/// Returns the user as JSON, setting a seven-day session cookie when a new
/// session `token` was issued.
fn session_response(auth: &Auth, user: User, token: Option<String>) -> Response {
    let who = Principal::User(user.clone());
    let session = token.map(|token| (token, crate::auth::BROWSER_SESSION_LIFETIME));
    signed_in(auth, who, user, session)
}
/// Returns `body` as JSON, setting a session cookie that lasts `age`
/// seconds when a new session `(token, age)` was issued. The audit trail
/// then records `who` signed in; otherwise, e.g. adding a passkey to the
/// caller's account, the caller.
fn signed_in(
    auth: &Auth,
    who: Principal,
    body: impl serde::Serialize,
    session: Option<(String, i64)>,
) -> Response {
    let mut response = Json(body).into_response();
    if let Some((token, age)) = session {
        response
            .extensions_mut()
            .insert(super::access::SignedIn(who));
        set_cookie(&mut response, auth, AuthCookie::Session, &token, age);
    }
    response
}

/// Adds a `Set-Cookie` header setting `auth`'s `cookie` to `value` for `age`
/// seconds.
#[expect(
    clippy::expect_used,
    reason = "Cookie names and attributes are ASCII and values are base64url tokens, so the header always parses."
)]
fn set_cookie(response: &mut Response, auth: &Auth, cookie: AuthCookie, value: &str, age: i64) {
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(cookie, value, age).parse().expect(
            "cookie names and attributes are ASCII; values are generated base64url tokens or empty",
        ),
    );
}
/// The host operator making a Unix socket request without any credential
/// (see [`Identity::socket_operator`]): a credential keeps its meaning, even
/// an unusable one. Anyone else is answered as if the route did not exist.
pub(super) struct OperatorPeer(HostOperator);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for OperatorPeer {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut http::request::Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(
            parts
                .extensions
                .get::<Identity>()
                .and_then(Identity::socket_operator)
                .map(Self)
                .ok_or_else(hidden),
        )
    }
}
/// 404 for a route the caller may not know about, which looks absent but is
/// recorded as a refusal in the audit trail.
fn hidden() -> ApiError {
    let mut error = ApiError::endpoint_not_found();
    error.denied = Some(piqueld_core::access::Denied::Hidden);
    error
}
/// Reads the ceremony binding cookie that ties a passkey finish request to the
/// browser that started it. Without it, e.g. once it expired, the ceremony
/// cannot finish.
fn binding<'a>(auth: &Auth, headers: &'a HeaderMap) -> Result<&'a str, ApiError> {
    cookie(auth, headers, AuthCookie::Ceremony).ok_or_else(|| AuthError::stale_ceremony().into())
}
/// Returns a passkey challenge and sets its five-minute ceremony binding cookie.
fn challenge_response(auth: &Auth, ceremony: Ceremony, binding: &str) -> Response {
    let mut response = Json(ceremony).into_response();
    let age = crate::auth::CEREMONY_LIFETIME;
    set_cookie(&mut response, auth, AuthCookie::Ceremony, binding, age);
    response
}

/// Gets authentication status.
///
/// Public. Reports whether the first account exists and the website origin.
#[utoipa::path(get,path="/api/v1/auth/status",operation_id="authStatus",responses((status=200,body=AuthStatus)))]
pub(super) async fn status(Extension(auth): Extension<Auth>) -> Result<Json<AuthStatus>, ApiError> {
    Ok(Json(auth.status().await?))
}
/// Gets the first-account setup link.
///
/// Public, but served only over the Unix socket, whose group access is the trust boundary; other transports return 404. Returns 409 once the first account exists.
#[utoipa::path(get,path="/api/v1/auth/setup-link",operation_id="authSetupLink",responses((status=200,body=SetupLink),ApiErrors<404, 409>))]
pub(super) async fn setup_link(
    Extension(auth): Extension<Auth>,
    unix: Option<Extension<super::UnixSocket>>,
) -> Result<Json<SetupLink>, ApiError> {
    // Looks absent over TCP, but the audit trail records the refusal.
    if unix.is_none() {
        return Err(hidden());
    }
    Ok(Json(auth.setup_link().await?))
}
/// Issues a one-time admin recovery link.
///
/// Public, but only over the Unix socket and only to the host operator: root
/// or the daemon's own user, identified by the kernel (`SO_PEERCRED`), sending
/// no credential, which keeps its meaning; anyone else gets 404. The link is
/// valid for 24 hours, replaces any earlier one, and registers a new account
/// with `admin` on every application. Issuing it raises a `security`
/// notification.
#[utoipa::path(post,path="/api/v1/auth/recovery",operation_id="authRecoverAdmin",responses((status=200,body=OperatorLink),ApiErrors<404, 409>))]
pub(super) async fn recover_admin(
    Extension(auth): Extension<Auth>,
    OperatorPeer(operator): OperatorPeer,
    audited: Audited,
) -> Result<Json<OperatorLink>, ApiError> {
    Ok(Json(auth.recover_admin(operator, audited.request()).await?))
}
/// Issues a one-time host operator sign-in link.
///
/// Public, but only over the Unix socket and only to the host operator
/// sending no credential, like `authRecoverAdmin`; anyone else gets 404. The
/// link works once within ten minutes and signs a browser in as the host
/// operator, with `admin` on every application, for 12 hours. Issuing it
/// raises a `security` notification.
#[utoipa::path(post,path="/api/v1/auth/sign-in-link",operation_id="authSignInLink",responses((status=200,body=OperatorLink),ApiErrors<404>))]
pub(super) async fn sign_in_link(
    Extension(auth): Extension<Auth>,
    OperatorPeer(operator): OperatorPeer,
    audited: Audited,
) -> Result<Json<OperatorLink>, ApiError> {
    Ok(Json(auth.sign_in_link(operator, audited.request()).await?))
}
/// Redeems a host operator sign-in link.
///
/// Public. Signs the browser in as the host operator with a 12-hour session
/// cookie. Used, expired, and unknown links fail with 401. Rate limited per
/// client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/login/operator",operation_id="authOperatorSignIn",request_body=OperatorSignIn,responses((status=200,body=Session),ApiErrors<401>))]
pub(super) async fn operator_sign_in(
    Extension(auth): Extension<Auth>,
    audited: Audited,
    ApiJson(input): ApiJson<OperatorSignIn>,
) -> Result<Response, ApiError> {
    let (operator, token) = auth
        .operator_sign_in(&input.secret, audited.request())
        .await?;
    let session = Identity::operator(operator, None).session();
    let age = crate::auth::OPERATOR_SESSION_LIFETIME;
    Ok(signed_in(
        &auth,
        session.principal.clone(),
        session,
        Some((token, age)),
    ))
}
/// Gets who is signed in and what the current credential may do.
///
/// Token-less requests over the Unix socket from root or the daemon's own
/// user act as the host operator.
#[utoipa::path(get,path="/api/v1/auth/me",operation_id="authMe",responses((status=200,body=Session)))]
pub(super) async fn me(Extension(identity): Extension<Identity>) -> Json<Session> {
    Json(identity.session())
}
/// Starts passkey registration.
///
/// Public. Registers a new account by redeeming an invitation or setup secret,
/// adds a passkey to an existing account by redeeming an enrollment link, or,
/// when signed in, adds a passkey to the caller's own account. That needs a
/// passkey sign-in within the last 2 minutes (see `authReauthenticateStart`).
/// Sets a short-lived ceremony cookie that the finish request must present.
/// Rate limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/register/start",operation_id="authRegistrationStart",request_body=RegistrationStart,responses((status=200,body=Ceremony),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn register_start(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    ApiJson(input): ApiJson<RegistrationStart>,
) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth
        .registration_start(
            input,
            &binding,
            identity.as_ref().map(|Extension(identity)| identity),
        )
        .await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
/// Finishes passkey registration.
///
/// Public. Verifies the passkey against the ceremony started in the same
/// browser. Redeeming an invitation, setup, or enrollment secret signs the
/// account in with a session cookie; a signed-in caller adding a passkey to
/// itself gets none. A finish refused because the caller's passkey sign-in
/// lapsed meanwhile still consumes the ceremony: confirm, then start again.
#[utoipa::path(post,path="/api/v1/auth/register/finish",operation_id="authRegistrationFinish",request_body=CeremonyFinish,responses((status=200,body=User),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn register_finish(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    audited: Audited,
    headers: HeaderMap,
    ApiJson(input): ApiJson<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let caller = identity.as_ref().map(|Extension(identity)| identity);
    let binding = binding(&auth, &headers)?;
    let (user, token) = auth
        .registration_finish(input, binding, caller, audited.request())
        .await?;
    Ok(session_response(&auth, user, token))
}
/// Starts a passkey sign-in.
///
/// Public. Usernameless: any discoverable passkey for this site may answer.
/// Sets a short-lived ceremony cookie that the finish request must present.
/// Rate limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/login/start",operation_id="authLoginStart",responses((status=200,body=Ceremony)))]
pub(super) async fn login_start(Extension(auth): Extension<Auth>) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth.login_start(&binding).await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
/// Finishes a passkey sign-in.
///
/// Public. Verifies the passkey and sets a seven-day session cookie.
#[utoipa::path(post,path="/api/v1/auth/login/finish",operation_id="authLoginFinish",request_body=CeremonyFinish,responses((status=200,body=User)))]
pub(super) async fn login_finish(
    Extension(auth): Extension<Auth>,
    audited: Audited,
    headers: HeaderMap,
    ApiJson(input): ApiJson<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let binding = binding(&auth, &headers)?;
    let (user, token) = auth.login_finish(input, binding, audited.request()).await?;
    Ok(session_response(&auth, user, Some(token)))
}
/// Starts confirming the browser session with a passkey.
///
/// Sensitive actions, like adding a passkey, need a passkey sign-in within
/// the last 2 minutes and otherwise fail with 403 `reauthentication_required`.
/// This starts a passkey ceremony limited to the signed-in account's
/// passkeys; finishing it makes the current session fresh, then retry the
/// action. Only browser sessions confirm this way: API tokens are refused
/// with 403 `reauthentication_required`. Sets a short-lived ceremony cookie
/// that the finish request must present. Rate limited per client address
/// (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/reauthenticate/start",operation_id="authReauthenticateStart",responses((status=200,body=Ceremony),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn reauthenticate_start(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth.reauthenticate_start(&identity, &binding).await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
/// Finishes confirming the browser session with a passkey.
///
/// Verifies the passkey against the ceremony started by the same session in
/// the same browser, and marks that session fresh. It issues no new session
/// and changes nothing else about it.
#[utoipa::path(post,path="/api/v1/auth/reauthenticate/finish",operation_id="authReauthenticateFinish",request_body=CeremonyFinish,responses((status=200,body=Managed)))]
pub(super) async fn reauthenticate_finish(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    headers: HeaderMap,
    ApiJson(input): ApiJson<CeremonyFinish>,
) -> Result<Json<Managed>, ApiError> {
    let binding = binding(&auth, &headers)?;
    auth.reauthenticate_finish(input, binding, &identity, audited.request())
        .await?;
    Ok(Json(Managed::default()))
}
/// Signs out.
///
/// Revokes the credential used for this request and clears the session cookie.
#[utoipa::path(post,path="/api/v1/auth/logout",operation_id="authLogout",responses((status=200,body=Managed)))]
pub(super) async fn logout(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
) -> Result<Response, ApiError> {
    auth.logout(&identity, audited.request()).await?;
    let mut response = Json(Managed::default()).into_response();
    set_cookie(&mut response, &auth, AuthCookie::Session, "", 0);
    Ok(response)
}
/// Lists accounts, passkeys, credentials, and open invitations.
///
/// With `accounts:manage`, lists every account and invitation; otherwise only
/// the caller's own account. A scoped credential sees only itself there, and
/// its account with its own grants.
#[utoipa::path(get,path="/api/v1/auth/directory",operation_id="authDirectory",responses((status=200,body=Directory)))]
pub(super) async fn directory(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Directory>, ApiError> {
    Ok(Json(auth.directory(&identity).await?))
}
/// Applies one account management action as the signed-in user.
///
/// Anyone may manage their own account. Changing other accounts requires
/// `accounts:manage` and every grant the account holds; handing out grants
/// requires holding them (403 `permission_denied` or `permission_exceeded`).
/// Creating tokens, invitations or enrollment links, and handing out `admin`,
/// `accounts:manage` or `system:operate`, need a passkey sign-in within the
/// last 2 minutes.
#[utoipa::path(post,path="/api/v1/auth/manage",operation_id="authManage",request_body=Manage,responses((status=200,body=Managed),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn manage(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    audited: Audited,
    ApiJson(input): ApiJson<Manage>,
) -> Response {
    // A change records what it changed itself; the background record of a
    // refused, failed or unchanged one names what it attempted.
    let detail = super::access::AuditDetail(input.describe());
    let mut response = match auth.manage(&identity, audited.request(), input).await {
        Ok(managed) => Json(managed).into_response(),
        Err(error) => ApiError::from(error).into_response(),
    };
    response.extensions_mut().insert(detail);
    response
}
/// Starts a device sign-in for a command-line client.
///
/// Public. Returns a device code to poll with and a user code to approve in a
/// signed-in browser. A JSON body, which may be omitted, limits the issued session to
/// `grants`, which the approver must hold, and is echoed back so clients can
/// tell the limit applies. Pending logins are kept in memory, so bodies over
/// 16 KiB answer 413. Rate limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/device/start",operation_id="authDeviceStart",request_body=DeviceStartRequest,responses((status=200,body=DeviceStart),ApiErrors<413>))]
pub(super) async fn device_start(
    Extension(auth): Extension<Auth>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<DeviceStart>, ApiError> {
    let body = super::extract::request_body(body)?;
    let requester = peer.map(|Extension(ConnectInfo(peer))| peer.ip());
    let request = if body.is_empty() {
        DeviceStartRequest::default()
    } else {
        super::extract::decode_json::<DeviceStartRequest>(&body)?
    };
    Ok(Json(auth.device_start(requester, request.grants)?))
}
/// Starts confirming the calling CLI login with a passkey.
///
/// Sensitive actions need a passkey sign-in within the last 2 minutes and
/// otherwise fail with 403 `reauthentication_required`. A CLI login asks
/// here for a device request naming `action`, approved like a sign-in, on the
/// dashboard page `verification_uri_complete`, by its own account. Once
/// approved, polling answers `complete` without a token: the CLI keeps its
/// login, now confirmed, and retries the action. Only CLI logins confirm this
/// way; API tokens are refused with 403 `reauthentication_required`. Rate
/// limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/device/confirm",operation_id="authDeviceConfirm",request_body=DeviceConfirmRequest,responses((status=200,body=DeviceStart),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn device_confirm(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ApiJson(input): ApiJson<DeviceConfirmRequest>,
) -> Result<Json<DeviceStart>, ApiError> {
    let requester = peer.map(|Extension(ConnectInfo(peer))| peer.ip());
    Ok(Json(auth.device_confirm(
        &identity,
        requester,
        input.action,
    )?))
}
/// Bodies [`device_start`] accepts: pending logins are kept in memory.
const DEVICE_START_LIMIT_BYTES: usize = 16 * 1024;
/// Bodies the other sign-in and passkey ceremony endpoints accept: most are
/// public, and their JSON, passkey responses included, is small.
const SIGN_IN_LIMIT_BYTES: usize = 64 * 1024;
/// Caps the bodies `routes` accept at `bytes`; larger ones answer 413 while
/// they are read, before anything is buffered beyond it.
fn limit_body<S: Clone + Send + Sync + 'static>(
    (schemas, paths, router): UtoipaMethodRouter<S>,
    bytes: usize,
) -> UtoipaMethodRouter<S> {
    (schemas, paths, router.layer(DefaultBodyLimit::max(bytes)))
}
/// Registers sign-in and passkey ceremony `routes` with the
/// [`SIGN_IN_LIMIT_BYTES`] body limit.
pub(super) fn sign_in_route<S: Clone + Send + Sync + 'static>(
    routes: UtoipaMethodRouter<S>,
) -> UtoipaMethodRouter<S> {
    limit_body(routes, SIGN_IN_LIMIT_BYTES)
}
/// Registers [`device_start`] with its [`DEVICE_START_LIMIT_BYTES`] body
/// limit, marking the body optional in the `OpenAPI` document: older clients
/// send none.
pub(super) fn device_start_route<S: Clone + Send + Sync + 'static>(
    routes: UtoipaMethodRouter<S>,
) -> UtoipaMethodRouter<S> {
    let (schemas, mut paths, limited) = limit_body(routes, DEVICE_START_LIMIT_BYTES);
    for item in paths.paths.values_mut() {
        if let Some(body) = item
            .post
            .as_mut()
            .and_then(|operation| operation.request_body.as_mut())
        {
            body.required = Some(utoipa::openapi::Required::False);
        }
    }
    (schemas, paths, limited)
}
/// Polls a device sign-in.
///
/// Public. Poll at most every five seconds: the status is
/// `authorization_pending` until approval, `slow_down` when polled too fast, and
/// `complete` with a 30-day token exactly once after approval. Expired or unknown
/// codes fail with 401. An approval whose passkey sign-in is over 2 minutes
/// old by then fails with 403 `reauthentication_required`; sign in again.
#[utoipa::path(post,path="/api/v1/auth/device/poll",operation_id="authDevicePoll",request_body=DevicePoll,responses((status=200,body=DeviceToken),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn device_poll(
    Extension(auth): Extension<Auth>,
    audited: Audited,
    ApiJson(input): ApiJson<DevicePoll>,
) -> Result<Response, ApiError> {
    let token = auth
        .device_poll(&input.device_code, audited.request())
        .await?;
    let signed_in = token
        .user
        .clone()
        .map(|user| super::access::SignedIn(Principal::User(user)));
    let mut response = Json(token).into_response();
    if let Some(signed_in) = signed_in {
        response.extensions_mut().insert(signed_in);
    }
    Ok(response)
}
/// Inspects a pending device sign-in.
///
/// Shows the requesting address and remaining lifetime so the user can confirm
/// the request before approving it. A request confirming an existing CLI
/// login names its action and login, and only that login's account sees it.
#[utoipa::path(post,path="/api/v1/auth/device/inspect",operation_id="authDeviceInspect",request_body=DeviceApprove,responses((status=200,body=DeviceRequest)))]
pub(super) async fn device_inspect(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    ApiJson(input): ApiJson<DeviceApprove>,
) -> Result<Json<DeviceRequest>, ApiError> {
    Ok(Json(auth.device_inspect(&input.user_code, &identity)?))
}
/// Approves a pending device sign-in.
///
/// The device's next poll receives a token for the signed-in user, or, for a
/// request confirming an existing CLI login of the same account, marks that
/// login confirmed. Needs a passkey sign-in within the last 2 minutes; the
/// issued or confirmed login counts as signed in when that was.
#[utoipa::path(post,path="/api/v1/auth/device/approve",operation_id="authDeviceApprove",request_body=DeviceApprove,responses((status=200,body=Managed),(status=403,response=inline(super::openapi::ReauthenticationResponse))))]
pub(super) async fn device_approve(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    ApiJson(input): ApiJson<DeviceApprove>,
) -> Result<Json<Managed>, ApiError> {
    auth.device_approve(&input.user_code, &identity)?;
    Ok(Json(Managed::default()))
}
