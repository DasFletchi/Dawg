//! Local administrator authentication. Passwords and session tokens never enter logs.
use super::{ApiError, AppState};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore};

const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const MAX_LOGIN_ATTEMPTS: u32 = 5;
const MAX_SESSIONS: usize = 64;
const MAX_LOGIN_CLIENTS: usize = 1024;

#[derive(Serialize, Deserialize)]
struct PasswordRecord {
    version: u32,
    username: String,
    password_hash: String,
}

struct AttemptWindow {
    started: Instant,
    attempts: u32,
}

pub(super) struct AuthState {
    password_hash: Arc<str>,
    sessions: Mutex<HashMap<String, Instant>>,
    attempts: Mutex<HashMap<IpAddr, AttemptWindow>>,
    verifiers: Arc<Semaphore>,
    secure_cookie: bool,
}

impl AuthState {
    pub(super) async fn load(data_dir: &Path) -> Result<Self, String> {
        // Read only these specific bindings, never the rest of the process environment.
        let supplied = std::env::var("DAWG_WEB_PASSWORD").ok();
        let secure_cookie = std::env::var("DAWG_WEB_SECURE_COOKIE").is_ok_and(|v| v == "1");
        Self::initialize(data_dir, supplied, secure_cookie).await
    }

    async fn initialize(
        data_dir: &Path,
        supplied: Option<String>,
        secure_cookie: bool,
    ) -> Result<Self, String> {
        tokio::fs::create_dir_all(data_dir)
            .await
            .map_err(|e| format!("Cannot create web authentication directory: {e}"))?;
        let path = data_dir.join("web-auth.json");
        let initial_path = data_dir.join("web-admin-password");
        let password_hash = if let Some(password) = supplied {
            if password.len() < 12 || password.len() > 1024 {
                return Err("DAWG_WEB_PASSWORD must contain between 12 and 1024 bytes.".into());
            }
            let hashed = hash_password(password).await?;
            save_record(&path, hashed.clone()).await?;
            match tokio::fs::remove_file(&initial_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Cannot remove superseded bootstrap password: {e}")),
            }
            hashed
        } else {
            match read_record(&path).await {
                Ok(bytes) => {
                    if bytes.len() > 8192 {
                        return Err("Web authentication file exceeds its size limit.".into());
                    }
                    let record: PasswordRecord = serde_json::from_slice(&bytes)
                        .map_err(|_| "Web authentication file is invalid; use DAWG_WEB_PASSWORD to reset it.")?;
                    if record.version != 1 || record.username != "admin" {
                        return Err("Web authentication format is unsupported.".into());
                    }
                    PasswordHash::new(&record.password_hash)
                        .map_err(|_| "Saved administrator password hash is invalid.")?;
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .await
                        .map_err(|e| format!("Cannot protect web authentication file: {e}"))?;
                    record.password_hash
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Recover the same credential after an interrupted first
                    // startup rather than overwriting the private password.
                    let password = bootstrap_password(initial_path.clone()).await?;
                    let hashed = hash_password(password).await?;
                    save_record(&path, hashed.clone()).await?;
                    hashed
                }
                Err(e) => return Err(format!("Cannot read web authentication file: {e}")),
            }
        };
        Ok(Self {
            password_hash: password_hash.into(),
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            verifiers: Arc::new(Semaphore::new(2)),
            secure_cookie,
        })
    }

    #[cfg(test)]
    pub(super) async fn test_state(password: String) -> Self {
        Self {
            password_hash: hash_password(password).await.unwrap().into(),
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            verifiers: Arc::new(Semaphore::new(2)),
            secure_cookie: false,
        }
    }

    async fn allow_login(&self, client: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().await;
        attempts.retain(|_, entry| now.duration_since(entry.started) < LOGIN_WINDOW);
        if !attempts.contains_key(&client) && attempts.len() >= MAX_LOGIN_CLIENTS {
            return false;
        }
        let entry = attempts.entry(client).or_insert(AttemptWindow {
            started: now,
            attempts: 0,
        });
        if entry.attempts >= MAX_LOGIN_ATTEMPTS {
            return false;
        }
        entry.attempts += 1;
        true
    }

    pub(super) async fn authenticated(&self, headers: &HeaderMap) -> bool {
        let Some(token) = cookie_token(headers) else {
            return false;
        };
        let now = Instant::now();
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, expires| *expires > now);
        sessions.contains_key(token)
    }

    fn secure_for(&self, headers: &HeaderMap) -> bool {
        self.secure_cookie
            || headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("https://"))
    }

    fn session_cookie(&self, token: &str, headers: &HeaderMap) -> String {
        format!(
            "dawg_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200{}",
            if self.secure_for(headers) {
                "; Secure"
            } else {
                ""
            }
        )
    }
}

#[derive(Deserialize)]
pub(super) struct Login {
    username: String,
    password: String,
}

pub(super) async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(login): Json<Login>,
) -> Result<Response, ApiError> {
    if !state.auth.allow_login(peer.ip()).await {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many login attempts. Wait one minute before trying again.".into(),
        ));
    }
    if login.username != "admin" || login.password.len() > 1024 {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Invalid username or password.".into(),
        ));
    }
    let permit = state
        .auth
        .verifiers
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Authentication is busy. Try again shortly.".into(),
            )
        })?;
    let saved_hash = state.auth.password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        PasswordHash::new(&saved_hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(login.password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Authentication failed.".into(),
        )
    })?;
    if !valid {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Invalid username or password.".into(),
        ));
    }
    let token = random_secret();
    let now = Instant::now();
    let mut sessions = state.auth.sessions.lock().await;
    sessions.retain(|_, expires| *expires > now);
    if sessions.len() >= MAX_SESSIONS {
        let oldest = sessions
            .iter()
            .min_by_key(|(_, expires)| *expires)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(token.clone(), now + SESSION_LIFETIME);
    drop(sessions);
    let cookie =
        HeaderValue::from_str(&state.auth.session_cookie(&token, &headers)).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot establish session.".into(),
            )
        })?;
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"authenticated": true, "username": "admin"})),
    )
        .into_response())
}

pub(super) async fn session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({"authenticated": state.auth.authenticated(&headers).await}))
}

pub(super) async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_token(&headers) {
        state.auth.sessions.lock().await.remove(token);
    }
    let cookie = format!(
        "dawg_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if state.auth.secure_for(&headers) {
            "; Secure"
        } else {
            ""
        }
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"authenticated": false})),
    )
        .into_response()
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == "dawg_session"
                && value.len() == 64
                && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(value)
        })
}

fn random_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

async fn hash_password(password: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let salt =
            SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes()).map_err(|e| e.to_string())?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|value| value.to_string())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("Password hashing failed: {e}"))?
}

async fn read_record(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path)
        .await?
        .take(8193)
        .read_to_end(&mut bytes)
        .await?;
    Ok(bytes)
}

async fn bootstrap_password(path: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
            Ok(file) => {
                if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                    return Err("The bootstrap password is not a regular file; use DAWG_WEB_PASSWORD to reset authentication.".into());
                }
                let mut password = String::new();
                file.take(1025).read_to_string(&mut password).map_err(|_| "Cannot read bootstrap password; use DAWG_WEB_PASSWORD to reset authentication.")?;
                let password = password.trim_end_matches('\n');
                if password.len() != 64 || !password.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("Bootstrap password file is invalid; use DAWG_WEB_PASSWORD to reset authentication.".into());
                }
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("Cannot protect bootstrap password: {e}"))?;
                Ok(password.to_owned())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let password = random_secret();
                let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
                let result = (|| {
                    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)
                        .map_err(|e| format!("Cannot create private bootstrap password: {e}"))?;
                    file.write_all(format!("{password}\n").as_bytes()).and_then(|_| file.sync_all())
                        .map_err(|e| format!("Cannot save bootstrap password: {e}"))?;
                    // Activation is atomic and refuses to replace an existing file.
                    std::fs::hard_link(&temporary, &path).map_err(|e| format!("Cannot activate bootstrap password: {e}"))?;
                    Ok(password)
                })();
                let _ = std::fs::remove_file(temporary);
                result
            }
            Err(error) => Err(format!("Cannot read bootstrap password: {error}")),
        }
    }).await.map_err(|e| format!("Password initialization failed: {e}"))?
}

async fn save_record(path: &Path, password_hash: String) -> Result<(), String> {
    let path = PathBuf::from(path);
    let bytes = serde_json::to_vec_pretty(&PasswordRecord {
        version: 1,
        username: "admin".into(),
        password_hash,
    })
    .map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|e| format!("Cannot create web authentication file: {e}"))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|e| format!("Cannot save web authentication: {e}"))?;
            std::fs::rename(&temporary, &path)
                .map_err(|e| format!("Cannot activate web authentication: {e}"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    })
    .await
    .map_err(|e| format!("Authentication persistence failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> PathBuf {
        let root = std::env::temp_dir().join(format!("dawg-auth-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn bootstrap_is_private_and_survives_restart() {
        let root = fixture_dir();
        let first = AuthState::initialize(&root, None, false).await.unwrap();
        let password_path = root.join("web-admin-password");
        let record_path = root.join("web-auth.json");
        let password = std::fs::read_to_string(&password_path).unwrap();
        assert_eq!(password.trim().len(), 64);
        for path in [&password_path, &record_path] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let record = std::fs::read_to_string(&record_path).unwrap();
        assert!(!record.contains(password.trim()));
        let hash = PasswordHash::new(&first.password_hash).unwrap();
        assert!(
            Argon2::default()
                .verify_password(password.trim().as_bytes(), &hash)
                .is_ok()
        );
        let restarted = AuthState::initialize(&root, None, false).await.unwrap();
        assert!(first.password_hash == restarted.password_hash);
        assert!(password == std::fs::read_to_string(&password_path).unwrap());
        // Simulate a crash after the credential became durable but before the
        // authentication record was activated.
        std::fs::remove_file(record_path).unwrap();
        let recovered = AuthState::initialize(&root, None, false).await.unwrap();
        assert!(
            Argon2::default()
                .verify_password(
                    password.trim().as_bytes(),
                    &PasswordHash::new(&recovered.password_hash).unwrap()
                )
                .is_ok()
        );
        assert!(password == std::fs::read_to_string(password_path).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn explicit_password_rotates_hash_and_removes_bootstrap() {
        let root = fixture_dir();
        let first = AuthState::initialize(&root, None, false).await.unwrap();
        let reset_password = "a new administrator password";
        let reset = AuthState::initialize(&root, Some(reset_password.into()), true)
            .await
            .unwrap();
        assert!(first.password_hash != reset.password_hash);
        assert!(!root.join("web-admin-password").exists());
        let hash = PasswordHash::new(&reset.password_hash).unwrap();
        assert!(
            Argon2::default()
                .verify_password(reset_password.as_bytes(), &hash)
                .is_ok()
        );
        assert!(
            reset
                .session_cookie("token", &HeaderMap::new())
                .contains("; Secure")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn login_limits_and_session_expiry_are_enforced() {
        let auth = AuthState::test_state("a sufficiently long test password".into()).await;
        let client = "127.0.0.1".parse().unwrap();
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            assert!(auth.allow_login(client).await);
        }
        assert!(!auth.allow_login(client).await);
        let token = random_secret();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("dawg_session={token}").parse().unwrap(),
        );
        auth.sessions
            .lock()
            .await
            .insert(token.clone(), Instant::now() + SESSION_LIFETIME);
        assert!(auth.authenticated(&headers).await);
        auth.sessions
            .lock()
            .await
            .insert(token, Instant::now() - Duration::from_secs(1));
        assert!(!auth.authenticated(&headers).await);
    }

    #[tokio::test]
    async fn oversized_password_record_is_rejected() {
        let root = fixture_dir();
        std::fs::write(root.join("web-auth.json"), vec![b'x'; 65536]).unwrap();
        assert!(AuthState::initialize(&root, None, false).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
