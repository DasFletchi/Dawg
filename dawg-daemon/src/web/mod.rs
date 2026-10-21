//! Authenticated browser access to the same manager used by the desktop client.
mod auth;
mod files;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dawg_core::{MAX_FRAME_BYTES, Request as DaemonRequest, Response as DaemonResponse};
use sha2::{Digest, Sha256};
use std::{future::Future, net::SocketAddr, path::PathBuf, sync::Arc};

use crate::server::manager::ServerManager;

pub(super) struct AppState {
    pub(super) manager: Arc<ServerManager>,
    auth: auth::AuthState,
}

#[derive(Debug)]
pub(super) struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

/// Bind only loopback by default in the caller. Public deployments should use an
/// HTTPS reverse proxy (or SSH/VPN access) and the generated administrator login.
pub async fn serve(
    manager: Arc<ServerManager>,
    bind: SocketAddr,
    data_dir: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("Cannot bind the web listener at {bind}: {e}"))?;
    let state = Arc::new(AppState {
        manager,
        auth: auth::AuthState::load(&data_dir).await?,
    });
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| format!("Web server failed: {e}"))
}

fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/action", post(action))
        .route(
            "/api/servers/{id}/files",
            get(files::list).delete(files::delete),
        )
        .route(
            "/api/servers/{id}/files/content",
            get(files::download)
                .put(files::upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/servers/{id}/files/directory",
            post(files::create_directory),
        )
        .route("/api/servers/{id}/files/rename", post(files::rename))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(stylesheet))
        .route("/assets/app.js", get(script))
        .route("/assets/file-manager.js", get(file_manager_script))
        .route("/assets/fonts/SourceSans3-Regular.ttf", get(font_regular))
        .route("/assets/fonts/SourceSans3-Semibold.ttf", get(font_semibold))
        .route("/assets/fonts/LICENSE.md", get(font_license))
        .route("/api/session", get(auth::session))
        .route(
            "/api/login",
            post(auth::login).layer(DefaultBodyLimit::max(8192)),
        )
        .route("/api/logout", post(auth::logout))
        .merge(protected)
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "This route does not exist.".into()) })
        .layer(DefaultBodyLimit::max(MAX_FRAME_BYTES))
        .layer(middleware::from_fn(request_security))
        .with_state(state)
}

async fn action(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DaemonRequest>,
) -> Result<Response, ApiError> {
    let response = run_action(state.manager.clone(), request).await?;
    let status = if matches!(response, DaemonResponse::Error(_)) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let bytes = serde_json::to_vec(&response).map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot encode the server response.".into(),
        )
    })?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The server response exceeded the size limit.".into(),
        ));
    }
    Ok((status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response())
}

async fn run_action(
    manager: Arc<ServerManager>,
    request: DaemonRequest,
) -> Result<DaemonResponse, ApiError> {
    // A browser closing a tab must not cancel an accepted manager transaction
    // halfway through an atomic save or leave its runtime reservation set.
    tokio::spawn(async move { manager.handle(request).await })
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The server operation could not complete.".into(),
            )
        })
}

async fn require_auth(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !state.auth.authenticated(request.headers()).await {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "Sign in to manage this server.".into(),
        )
        .into_response();
    }
    next.run(request).await
}

fn check_request(method: &Method, headers: &HeaderMap) -> Result<(), ApiError> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    if headers
        .get("x-dawg-request")
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "This request is missing its same-origin protection header.".into(),
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let uri: axum::http::Uri = origin
            .to_str()
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| ApiError(StatusCode::FORBIDDEN, "Invalid request origin.".into()))?;
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                ApiError(StatusCode::FORBIDDEN, "The request host is missing.".into())
            })?;
        let allowed = matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.authority().is_some_and(|authority| {
                !authority.as_str().contains('@') && authority.as_str().eq_ignore_ascii_case(host)
            })
            && uri.path_and_query().is_none_or(|part| part.as_str() == "/");
        if !allowed {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                "Cross-origin management requests are not allowed.".into(),
            ));
        }
    }
    Ok(())
}

async fn request_security(request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = match check_request(request.method(), request.headers()) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'self'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn asset(headers: HeaderMap, bytes: &'static [u8], mime: &'static str, cache: bool) -> Response {
    let etag = format!("\"{:x}\"", Sha256::digest(bytes));
    let unchanged = cache
        && headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == etag || value == "*");
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Body::from(bytes).into_response()
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if cache {
            "public, max-age=0, must-revalidate"
        } else {
            "no-store"
        }),
    );
    response
}

async fn index(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/index.html"),
        "text/html; charset=utf-8",
        false,
    )
}
async fn stylesheet(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/app.css"),
        "text/css; charset=utf-8",
        true,
    )
}
async fn script(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/app.js"),
        "text/javascript; charset=utf-8",
        true,
    )
}
async fn file_manager_script(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/file-manager.js"),
        "text/javascript; charset=utf-8",
        true,
    )
}
async fn font_regular(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/fonts/SourceSans3-Regular.ttf"),
        "font/ttf",
        true,
    )
}
async fn font_semibold(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/fonts/SourceSans3-Semibold.ttf"),
        "font/ttf",
        true,
    )
}
async fn font_license(headers: HeaderMap) -> Response {
    asset(
        headers,
        include_bytes!("../../web/fonts/LICENSE.md"),
        "text/plain; charset=utf-8",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr},
    };
    use tower::ServiceExt;

    fn fixture_dir() -> PathBuf {
        let root = std::env::temp_dir().join(format!("dawg-web-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    async fn test_app(root: &std::path::Path) -> Router {
        let manager = Arc::new(ServerManager::load(root.to_owned()).await.unwrap());
        let auth = auth::AuthState::test_state("a sufficiently long test password".into()).await;
        router(Arc::new(AppState { manager, auth })).layer(axum::Extension(
            axum::extract::ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345)),
        ))
    }

    fn request(method: &str, uri: &str, body: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost:8080")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-dawg-request", "1")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    async fn login_cookie(app: &Router) -> String {
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/login",
                r#"{"username":"admin","password":"a sufficiently long test password"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        cookie.split(';').next().unwrap().to_owned()
    }

    #[test]
    fn mutations_require_custom_header_and_matching_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8080"));
        assert!(check_request(&Method::POST, &headers).is_err());
        headers.insert("x-dawg-request", HeaderValue::from_static("1"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.invalid"),
        );
        assert!(check_request(&Method::PUT, &headers).is_err());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8080"),
        );
        assert!(check_request(&Method::PUT, &headers).is_ok());
        assert!(check_request(&Method::GET, &HeaderMap::new()).is_ok());
    }

    #[tokio::test]
    async fn protected_actions_require_login_and_logout_revokes_session() {
        let root = fixture_dir();
        let app = test_app(&root).await;
        let denied = app
            .clone()
            .oneshot(request("POST", "/api/action", r#""Snapshot""#))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let cookie = login_cookie(&app).await;
        let mut action = request("POST", "/api/action", r#""Snapshot""#);
        action
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        let allowed = app.clone().oneshot(action).await.unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        let body = allowed.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["Snapshot"]["servers"].as_array().unwrap().is_empty());
        let mut logout = request("POST", "/api/logout", "");
        logout
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(logout).await.unwrap().status(),
            StatusCode::OK
        );
        let mut action = request("POST", "/api/action", r#""Snapshot""#);
        action
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        assert_eq!(
            app.oneshot(action).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn unauthenticated_files_and_cross_origin_login_are_rejected() {
        let root = fixture_dir();
        let app = test_app(&root).await;
        let files = app
            .clone()
            .oneshot(request("GET", "/api/servers/missing/files?path=", ""))
            .await
            .unwrap();
        assert_eq!(files.status(), StatusCode::UNAUTHORIZED);
        let mut login = request(
            "POST",
            "/api/login",
            r#"{"username":"admin","password":"a sufficiently long test password"}"#,
        );
        login.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.invalid"),
        );
        assert_eq!(
            app.oneshot(login).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn downloading_a_growing_log_keeps_the_advertised_length() {
        use std::io::Write;
        let root = fixture_dir();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.join("servers").join(&id);
        fs::create_dir_all(&directory).unwrap();
        let config = dawg_core::ServerConfig {
            id: id.clone(),
            name: "Download fixture".into(),
            directory: directory.to_string_lossy().into(),
            kind: dawg_core::ServerKind::Vanilla,
            minecraft_version: "1.21.11".into(),
            memory_mb: 512,
            port: 25565,
            java_path: "/bin/false".into(),
            eula_accepted: false,
            network: Default::default(),
        };
        fs::write(
            root.join("servers.json"),
            serde_json::to_vec(&vec![config]).unwrap(),
        )
        .unwrap();
        let logfile = directory.join("latest.log");
        fs::write(&logfile, b"initial log\n").unwrap();
        let app = test_app(&root).await;
        let cookie = login_cookie(&app).await;
        let mut download = request(
            "GET",
            &format!("/api/servers/{id}/files/content?path=latest.log"),
            "",
        );
        download
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        let response = app.oneshot(download).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "12");
        fs::OpenOptions::new()
            .append(true)
            .open(logfile)
            .unwrap()
            .write_all(b"new log line\n")
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), b"initial log\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn static_assets_are_cache_validated_and_not_inline_executable() {
        let first = asset(HeaderMap::new(), b"body{}", "text/css", true);
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, first.headers()[header::ETAG].clone());
        assert_eq!(
            asset(headers, b"body{}", "text/css", true).status(),
            StatusCode::NOT_MODIFIED
        );
    }
}
