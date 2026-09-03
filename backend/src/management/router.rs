//! Management API 路由、请求边界与认证 handler。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Request, State};
use axum::http::header::{CONTENT_LENGTH, COOKIE, ORIGIN, RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Version};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::assets;
use super::auth::{AuthError, AuthState, hash_password, validate_setup_credentials};
use super::query;
use super::query::ManagementQueryService;
use super::session::{SessionStore, SessionView};
use crate::config::store::{ConfigStore, ConfigStoreError};

const MAX_JSON_BODY_BYTES: usize = 16 * 1024;
const MAX_URI_BYTES: usize = 4 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_ATTEMPTS_PER_WINDOW: usize = 10;
const SETUP_ATTEMPTS_PER_WINDOW: usize = 5;
const MAX_ATTEMPT_KEYS: usize = 4096;

static REQUEST_ID_FALLBACK: AtomicU64 = AtomicU64::new(1);
static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Clone)]
pub(crate) struct AuthServices {
    pub(crate) auth: Arc<AuthState>,
    pub(crate) sessions: Arc<SessionStore>,
    pub(crate) config_store: Arc<ConfigStore>,
    pub(crate) queries: Option<Arc<ManagementQueryService>>,
    public_origin: String,
    attempts: Arc<AttemptLimiter>,
}

impl AuthServices {
    pub(crate) fn new(
        auth: Arc<AuthState>,
        sessions: Arc<SessionStore>,
        config_store: Arc<ConfigStore>,
        public_origin: String,
        queries: Option<Arc<ManagementQueryService>>,
    ) -> Self {
        Self {
            auth,
            sessions,
            config_store,
            queries,
            public_origin,
            attempts: Arc::new(AttemptLimiter::default()),
        }
    }
}

#[derive(Clone)]
pub(super) struct RequestId(pub(super) String);

#[derive(Default)]
struct AttemptLimiter {
    attempts: Mutex<HashMap<String, Vec<Instant>>>,
}

impl AttemptLimiter {
    fn allow(&self, kind: &str, peer: IpAddr, username: &str, limit: usize) -> bool {
        let username_digest = Sha256::digest(username.trim().as_bytes());
        let key = format!("{kind}:{peer}:{username_digest:x}");
        let now = Instant::now();
        let Ok(mut attempts) = self.attempts.lock() else {
            return false;
        };
        attempts.retain(|_, values| {
            values.retain(|value| now.duration_since(*value) < ATTEMPT_WINDOW);
            !values.is_empty()
        });
        if attempts.len() >= MAX_ATTEMPT_KEYS && !attempts.contains_key(&key) {
            return false;
        }
        let values = attempts.entry(key).or_default();
        if values.len() >= limit {
            return false;
        }
        values.push(now);
        true
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct SetupStatus {
    state: &'static str,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: String,
    retryable: bool,
}

struct BoundaryState {
    requests: Arc<tokio::sync::Semaphore>,
}

pub(crate) fn build_router(services: Arc<AuthServices>) -> Router {
    let protected = Router::new()
        .route("/api/v1/auth/session", get(get_session))
        .merge(query::routes())
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&services),
            require_session,
        ));
    let boundary = Arc::new(BoundaryState {
        requests: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS)),
    });

    Router::new()
        .route("/api/v1/auth/setup", get(get_setup).post(post_setup))
        .route("/api/v1/auth/login", post(post_login))
        .route("/api/v1/auth/logout", post(post_logout))
        .merge(protected)
        .fallback(fallback)
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
        .layer(middleware::from_fn_with_state(boundary, request_boundary))
        .with_state(services)
}

async fn request_boundary(
    State(state): State<Arc<BoundaryState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = RequestId(new_request_id());
    request.extensions_mut().insert(request_id.clone());
    if request.version() != Version::HTTP_11 {
        return error_response(
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            "HTTP_VERSION_NOT_SUPPORTED",
            "only HTTP/1.1 is supported",
            false,
            &request_id,
        );
    }
    if request
        .uri()
        .path_and_query()
        .is_some_and(|value| value.as_str().len() > MAX_URI_BYTES)
    {
        return error_response(
            StatusCode::URI_TOO_LONG,
            "URI_TOO_LONG",
            "request URI is too long",
            false,
            &request_id,
        );
    }
    if request.headers().len() > MAX_HEADER_COUNT
        || header_bytes(request.headers()) > MAX_HEADER_BYTES
    {
        return error_response(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "HEADERS_TOO_LARGE",
            "request headers are too large",
            false,
            &request_id,
        );
    }
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|value| value > MAX_JSON_BODY_BYTES)
    {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "request body is too large",
            false,
            &request_id,
        );
    }
    let Ok(_permit) = Arc::clone(&state.requests).try_acquire_owned() else {
        return error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "management request capacity is exhausted",
            true,
            &request_id,
        );
    };
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => error_response(
            StatusCode::REQUEST_TIMEOUT,
            "REQUEST_TIMEOUT",
            "management request timed out",
            true,
            &request_id,
        ),
    };
    with_request_id(response, &request_id)
}

async fn require_session(
    State(services): State<Arc<AuthServices>>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request_id(&request);
    let Some(token) = session_token(request.headers(), &services.sessions) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "session required",
            false,
            &request_id,
        );
    };
    match services.sessions.lookup(&token) {
        Ok(Some(session)) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        Ok(None) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_REQUIRED",
            "session required",
            false,
            &request_id,
        ),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "management service is unavailable",
            true,
            &request_id,
        ),
    }
}

async fn get_setup(
    State(services): State<Arc<AuthServices>>,
    Extension(_request_id): Extension<RequestId>,
) -> Response {
    let state = if services.auth.setup_required() {
        "required"
    } else {
        "ready"
    };
    (StatusCode::OK, Json(SetupStatus { state })).into_response()
}

async fn post_setup(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<CredentialsRequest>, JsonRejection>,
) -> Response {
    if let Some(response) =
        validate_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_json(&request_id),
    };
    let peer = peer.ip();
    if !services
        .attempts
        .allow("setup", peer, &payload.username, SETUP_ATTEMPTS_PER_WINDOW)
    {
        return rate_limited(&request_id);
    }
    let username = match validate_setup_credentials(&payload.username, &payload.password) {
        Ok(username) => username,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "VALIDATION_FAILED",
                "username or password does not satisfy setup policy",
                false,
                &request_id,
            );
        }
    };
    if !services.auth.setup_required() {
        return error_response(
            StatusCode::CONFLICT,
            "SETUP_ALREADY_COMPLETED",
            "WebUI setup has already completed",
            false,
            &request_id,
        );
    }

    let password = payload.password;
    let password_hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(hash)) => hash,
        _ => return internal_error(&request_id),
    };
    let store = Arc::clone(&services.config_store);
    let store_username = username.clone();
    let commit = match tokio::task::spawn_blocking(move || {
        store.create_initial_user(&store_username, &password_hash)
    })
    .await
    {
        Ok(Ok(commit)) => commit,
        Ok(Err(error)) => return config_store_error(error, &request_id),
        Err(_) => return internal_error(&request_id),
    };
    services.auth.replace(&commit.users);
    issue_session(
        &services.sessions,
        username,
        StatusCode::CREATED,
        &request_id,
    )
}

async fn post_login(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    payload: Result<Json<CredentialsRequest>, JsonRejection>,
) -> Response {
    if let Some(response) =
        validate_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_json(&request_id),
    };
    let peer = peer.ip();
    if !services
        .attempts
        .allow("login", peer, &payload.username, LOGIN_ATTEMPTS_PER_WINDOW)
    {
        return rate_limited(&request_id);
    }
    if services.auth.setup_required() {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_INVALID_CREDENTIALS",
            "invalid credentials",
            false,
            &request_id,
        );
    }
    let auth = Arc::clone(&services.auth);
    let username = payload.username;
    let password = payload.password;
    match tokio::task::spawn_blocking(move || auth.authenticate(&username, &password)).await {
        Ok(Ok(username)) => {
            issue_session(&services.sessions, username, StatusCode::OK, &request_id)
        }
        Ok(Err(AuthError::InvalidCredentials)) => error_response(
            StatusCode::UNAUTHORIZED,
            "AUTH_INVALID_CREDENTIALS",
            "invalid credentials",
            false,
            &request_id,
        ),
        _ => internal_error(&request_id),
    }
}

async fn post_logout(
    State(services): State<Arc<AuthServices>>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) =
        validate_mutating_request(&headers, &services.public_origin, &request_id)
    {
        return response;
    }
    if let Some(token) = session_token(&headers, &services.sessions) {
        services.sessions.revoke(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(value) = HeaderValue::from_str(&services.sessions.clear_cookie()) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    response
}

async fn get_session(Extension(session): Extension<SessionView>) -> Json<SessionView> {
    Json(session)
}

async fn fallback(request: Request<Body>) -> Response {
    if request.uri().path() == "/api" || request.uri().path().starts_with("/api/") {
        let request_id = request_id(&request);
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "API route was not found",
            false,
            &request_id,
        );
    }
    assets::fallback(request).await
}

fn validate_mutating_request(
    headers: &HeaderMap,
    public_origin: &str,
    request_id: &RequestId,
) -> Option<Response> {
    let mut origins = headers.get_all(ORIGIN).iter();
    let origin = origins.next().and_then(|value| value.to_str().ok());
    if origin != Some(public_origin) || origins.next().is_some() {
        return Some(error_response(
            StatusCode::BAD_REQUEST,
            "ORIGIN_REJECTED",
            "request origin was rejected",
            false,
            request_id,
        ));
    }
    let mut fetch_sites = headers.get_all("sec-fetch-site").iter();
    if fetch_sites
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
        || fetch_sites.next().is_some()
    {
        return Some(error_response(
            StatusCode::BAD_REQUEST,
            "ORIGIN_REJECTED",
            "request origin was rejected",
            false,
            request_id,
        ));
    }
    None
}

fn session_token(headers: &HeaderMap, sessions: &SessionStore) -> Option<String> {
    let mut values = headers.get_all(COOKIE).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    sessions.token_from_header(value)
}

fn issue_session(
    sessions: &SessionStore,
    username: String,
    status: StatusCode,
    request_id: &RequestId,
) -> Response {
    let issued = match sessions.issue(username) {
        Ok(issued) => issued,
        Err(_) => return internal_error(request_id),
    };
    let cookie = match HeaderValue::from_str(&sessions.set_cookie(issued.token)) {
        Ok(cookie) => cookie,
        Err(_) => return internal_error(request_id),
    };
    let mut response = (status, Json(issued.view)).into_response();
    response.headers_mut().insert(SET_COOKIE, cookie);
    response
}

fn config_store_error(error: ConfigStoreError, request_id: &RequestId) -> Response {
    match error {
        ConfigStoreError::AlreadyInitialized => error_response(
            StatusCode::CONFLICT,
            "SETUP_ALREADY_COMPLETED",
            "WebUI setup has already completed",
            false,
            request_id,
        ),
        ConfigStoreError::Conflict | ConfigStoreError::Busy => error_response(
            StatusCode::CONFLICT,
            "CONFIG_CONFLICT",
            "configuration changed while setup was in progress",
            true,
            request_id,
        ),
        _ => internal_error(request_id),
    }
}

fn invalid_json(request_id: &RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "VALIDATION_FAILED",
        "request body is invalid",
        false,
        request_id,
    )
}

pub(super) fn internal_error(request_id: &RequestId) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
        "management service is unavailable",
        true,
        request_id,
    )
}

pub(super) fn invalid_argument(request_id: &RequestId) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "INVALID_ARGUMENT",
        "query parameters are invalid",
        false,
        request_id,
    )
}

fn rate_limited(request_id: &RequestId) -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "RATE_LIMITED",
        "too many authentication attempts",
        true,
        request_id,
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("60"));
    response
}

fn error_response(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retryable: bool,
    request_id: &RequestId,
) -> Response {
    with_request_id(
        (
            status,
            Json(ErrorEnvelope {
                code,
                message,
                request_id: request_id.0.clone(),
                retryable,
            }),
        )
            .into_response(),
        request_id,
    )
}

fn with_request_id(mut response: Response, request_id: &RequestId) -> Response {
    if let Ok(value) = HeaderValue::from_str(&request_id.0) {
        response.headers_mut().insert(X_REQUEST_ID.clone(), value);
    }
    response
}

fn request_id(request: &Request) -> RequestId {
    request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(|| RequestId(new_request_id()))
}

fn new_request_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = REQUEST_ID_FALLBACK.fetch_add(1, Ordering::Relaxed);
    format!("fallback-{timestamp:x}-{sequence:x}")
}

fn header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len().saturating_add(value.as_bytes().len()))
        .sum()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::extract::ConnectInfo;
    use axum::http::header::{CONTENT_TYPE, COOKIE, ORIGIN, SET_COOKIE};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::{AuthServices, build_router};
    use crate::config::migrate::deterministic_hash;
    use crate::config::store::ConfigStore;
    use crate::config::{ConfigLoader, LoadOptions};
    use crate::management::ManagementRuntime;
    use crate::management::auth::AuthState;
    use crate::management::session::SessionStore;

    fn test_services() -> (Arc<AuthServices>, std::path::PathBuf, std::path::PathBuf) {
        let (source, work_path) = crate::config::test_support::portable_example();
        let root = work_path.with_extension("management-router");
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("source.yaml");
        std::fs::write(&source_path, source.as_bytes()).unwrap();
        let auth = Arc::new(AuthState::new(&[]).unwrap());
        let sessions = Arc::new(SessionStore::new(false));
        let config_store = Arc::new(ConfigStore::new(
            source_path.clone(),
            source_path.clone(),
            deterministic_hash(source.as_bytes()),
        ));
        (
            Arc::new(AuthServices::new(
                auth,
                sessions,
                config_store,
                "http://127.0.0.1:8080".to_owned(),
                None,
            )),
            root,
            source_path,
        )
    }

    fn post(path: &str, body: &'static str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(CONTENT_TYPE, "application/json")
            .header(ORIGIN, "http://127.0.0.1:8080")
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:42000".parse::<SocketAddr>().unwrap(),
        ));
        request
    }

    #[tokio::test]
    async fn setup_persists_hash_and_issues_usable_cookie_session() {
        let (services, root, source_path) = test_services();
        let runtime = ManagementRuntime::new(
            Arc::clone(&services.auth),
            Arc::clone(&services.sessions),
            Arc::clone(&services.config_store),
        );
        let app = build_router(Arc::clone(&services));

        let response = app
            .clone()
            .oneshot(post(
                "/api/v1/auth/setup",
                r#"{"username":"admin","password":"correct horse battery staple"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let set_cookie = response
            .headers()
            .get(SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let cookie = set_cookie.split(';').next().unwrap();
        let token = cookie.split_once('=').unwrap().1;
        let source = std::fs::read_to_string(&source_path).unwrap();
        assert!(source.contains("$argon2id$"));
        assert!(!source.contains("correct horse battery staple"));

        let loaded = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_from_path(&source_path)
            .unwrap();
        runtime.reconcile_users(&loaded.resolved.webui.users, &loaded.resolved.input_hash);
        assert!(services.sessions.lookup(token).unwrap().is_some());

        let request = Request::builder()
            .uri("/api/v1/auth/session")
            .header(COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["user"]["name"], "admin");

        runtime.reconcile_users(&loaded.resolved.webui.users, "external-fingerprint");
        assert!(services.sessions.lookup(token).unwrap().is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_api_never_falls_back_to_spa() {
        let (services, root, _) = test_services();
        let request = Request::builder()
            .uri("/api/v1/not-found")
            .body(Body::empty())
            .unwrap();
        let response = build_router(services).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "NOT_FOUND");

        let _ = std::fs::remove_dir_all(root);
    }
}
