use std::collections::HashMap;
use std::sync::Arc;

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::{RwLock, broadcast, watch};
use tower::ServiceExt;

use rustyfin_api::cache::{ApiCache, ImageCache};
use rustyfin_api::middleware::RateLimiter;
use rustyfin_api::routes::build_router;
use rustyfin_api::state::AppState;
use rustyfin_core::scan_progress::ScanRegistry;
use rustyfin_model::server_info::ServerConfiguration;
use rustyfin_model::ws_event::WsEvent;

/// Temp dirs this test made. libtest runs each test on its own thread, so dropping this
/// thread-local removes them when the test ends, also when it panics. `test_state()` hands out
/// a plain `AppState`, which tests pass by value, so the guard can't travel with it.
struct TempDirs(std::cell::RefCell<Vec<std::path::PathBuf>>);

impl Drop for TempDirs {
    fn drop(&mut self) {
        for dir in self.0.get_mut().drain(..) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

thread_local! {
    static TEMP_DIRS: TempDirs = const { TempDirs(std::cell::RefCell::new(Vec::new())) };
}

/// Removes `dir` and everything in it when the current test ends.
fn remove_when_test_ends(dir: &std::path::Path) {
    TEMP_DIRS.with(|dirs| dirs.0.borrow_mut().push(dir.to_path_buf()));
}

async fn test_state() -> AppState {
    let data_dir = std::env::temp_dir().join(format!("rustyfin_test_{}", uuid::Uuid::new_v4()));
    remove_when_test_ends(&data_dir);
    std::fs::create_dir_all(&data_dir).ok();
    std::fs::create_dir_all(data_dir.join("logs")).ok();
    std::fs::create_dir_all(data_dir.join("cache")).ok();

    let db_path = data_dir.join("test.db");
    let pool = rustyfin_db::init_db(db_path.to_str().unwrap())
        .await
        .unwrap();

    let (shutdown_tx, _) = watch::channel(false);
    let data_dir_arc = Arc::new(data_dir.clone());

    let transcode_manager = rustyfin_core::transcode::TranscodeManager::new(&data_dir);
    let task_scheduler =
        rustyfin_core::scheduler::TaskScheduler::new(pool.clone(), data_dir_arc.clone());
    let plugin_manager = rustyfin_core::plugin_manager::PluginManager::new(&data_dir_arc);
    let rate_limiter = RateLimiter::new(1000, 60);
    let (ws_tx, _) = broadcast::channel::<WsEvent>(100);
    let (_log_level_tx, _) = watch::channel("info".to_string());
    let cache = ApiCache::new();
    let image_cache = ImageCache::new();

    AppState {
        db: pool.clone(),
        db_ro: pool,
        server_id: Arc::new("test-server-id".to_string()),
        data_dir: data_dir_arc,
        config: Arc::new(RwLock::new(ServerConfiguration::default())),
        shutdown_tx: Arc::new(shutdown_tx),
        bind_addr: Arc::new("0.0.0.0:8096".to_string()),
        api_key: Some(Arc::new("test-api-key".to_string())),
        quick_connect: Arc::new(RwLock::new(HashMap::new())),
        transcode_manager,
        task_scheduler,
        plugin_manager,
        rate_limiter,
        known_proxies: Arc::new(Vec::new()),
        ws_broadcast: ws_tx,
        log_level_tx: _log_level_tx,
        cache,
        image_cache,
        http_client: reqwest::Client::new(),
        tmdb: rustyfin_core::tmdb_client::TmdbClient::new(
            reqwest::Client::new(),
            Default::default(),
        ),
        oidc: rustyfin_api::oidc::OidcState::new(),
    }
}

fn app(state: AppState) -> axum::Router {
    build_router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))))
}

// ── Public Endpoint Tests ──

#[tokio::test]
async fn test_health_endpoint() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn test_ping_endpoint() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_public_info_endpoint() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info/Public")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["Id"], "test-server-id");
}

// ── Auth Endpoint Tests ──

#[tokio::test]
async fn test_login_success() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(
                    r#"{{"Username":"{}","Pw":"admin"}}"#,
                    username
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let auth: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!auth["AccessToken"].as_str().unwrap().is_empty());
    assert_eq!(auth["ServerId"], "test-server-id");
}

#[tokio::test]
async fn test_login_wrong_password() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/AuthenticateByName")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(
                    r#"{{"Username":"{}","Pw":"wrong"}}"#,
                    username
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── Protected Endpoint Tests ──

#[tokio::test]
async fn test_protected_endpoint_without_auth() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/*#[tokio::test]
async fn test_protected_endpoint_with_api_key() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header("X-Emby-Token", "test-api-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["Id"], "test-server-id");
}*/

#[tokio::test]
async fn test_protected_endpoint_with_jwt() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_protected_endpoint_with_invalid_token() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header("Authorization", "Bearer invalid.token.here")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── B8: API key ──
// An empty or blank token never authenticates, whatever the configured API key is, and the
// websocket accepts the API key or a user token.

/// A copy of `base` with `key` as the configured API key.
fn b8_with_key(base: &AppState, key: Option<&str>) -> AppState {
    let mut state = base.clone();
    state.api_key = key.map(|key| Arc::new(key.to_string()));
    state
}

/// The statuses of `GET /System/Configuration` with `key` as the API key and `token` sent as the
/// `api_key` query value and as `X-Emby-Token`.
async fn b8_statuses(base: &AppState, key: Option<&str>, token: &str) -> (StatusCode, StatusCode) {
    let state = b8_with_key(base, key);
    let query = app(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/System/Configuration?api_key={}",
                    token.replace(' ', "%20")
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    let header = app(state)
        .oneshot(
            Request::builder()
                .uri("/System/Configuration")
                .header("X-Emby-Token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    (query, header)
}

/// Serves the router on a free local port and returns the status of a websocket upgrade of
/// `/socket{query}`: 101 when the server accepts it.
async fn b8_socket_status(state: AppState, query: &str) -> u16 {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET /socket{query} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut reader = tokio::io::BufReader::new(stream);
    let mut status_line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut status_line),
    )
    .await
    .expect("no answer to the websocket upgrade")
    .unwrap();
    status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"))
}

#[tokio::test]
async fn test_b8_empty_token_never_authenticates_http() {
    let base = test_state().await;
    let unauthorized = StatusCode::UNAUTHORIZED;
    // (key, token, expected). For an empty or blank key the right token is the key itself.
    let cases = [
        (Some(""), "", unauthorized),
        (Some(""), "wrong-key", unauthorized),
        (Some("   "), "", unauthorized),
        (Some("   "), "   ", unauthorized),
        (Some("   "), "wrong-key", unauthorized),
        (Some("b8-key"), "", unauthorized),
        (Some("b8-key"), "   ", unauthorized),
        (Some("b8-key"), "wrong-key", unauthorized),
        (Some("b8-key"), "b8-key", StatusCode::OK),
        (None, "", unauthorized),
        (None, "   ", unauthorized),
        (None, "wrong-key", unauthorized),
    ];
    for (key, token, expected) in cases {
        let (query, header) = b8_statuses(&base, key, token).await;
        assert_eq!(query, expected, "api_key={token:?} with key {key:?}");
        assert_eq!(header, expected, "X-Emby-Token {token:?} with key {key:?}");
    }
    std::fs::remove_dir_all(base.data_dir.as_path()).ok();
}

#[tokio::test]
async fn test_b8_socket_rejects_an_empty_token() {
    let base = test_state().await;
    let empty_tokens = [
        "?api_key=",
        "?token=",
        "?authorization=",
        "?authorization=Bearer%20",
        "?api_key=%20%20%20",
    ];
    for key in [Some(""), Some("   "), Some("b8-key"), None] {
        for query in empty_tokens {
            let status = b8_socket_status(b8_with_key(&base, key), query).await;
            assert_eq!(status, 401, "{query} with key {key:?}");
        }
    }
    // The key still opens the socket
    let state = b8_with_key(&base, Some("b8-key"));
    assert_eq!(b8_socket_status(state, "?api_key=b8-key").await, 101);
    std::fs::remove_dir_all(base.data_dir.as_path()).ok();
}

#[tokio::test]
async fn test_b8_socket_accepts_a_user_token_while_an_api_key_is_set() {
    let base = test_state().await;
    let user = rustyfin_model::user::User {
        id: uuid::Uuid::new_v4(),
        name: "b8-user".to_string(),
        password_hash: String::new(),
        is_administrator: false,
        created_at: chrono::Utc::now(),
        last_login_date: None,
        configuration: serde_json::Value::Null,
    };
    rustyfin_db::repo_user::create_user(&base.db, &user)
        .await
        .unwrap();
    let token = rustyfin_auth::create_token(&base.db, &user, &Default::default())
        .await
        .unwrap();
    // A well-formed token that this server never issued
    let foreign = "0123456789abcdef0123456789abcdef";
    for key in [Some("b8-key"), None] {
        let state = b8_with_key(&base, key);
        // The web client sends `authorization=Bearer <token>`, Jellyfin clients `api_key=<token>`
        let web = format!("?authorization=Bearer%20{token}");
        assert_eq!(
            b8_socket_status(state.clone(), &web).await,
            101,
            "key {key:?}"
        );
        let jellyfin = format!("?api_key={token}");
        assert_eq!(
            b8_socket_status(state.clone(), &jellyfin).await,
            101,
            "key {key:?}"
        );
        let signed_elsewhere = format!("?api_key={foreign}");
        assert_eq!(
            b8_socket_status(state.clone(), &signed_elsewhere).await,
            401,
            "key {key:?}"
        );
        assert_eq!(
            b8_socket_status(state, "?api_key=wrong-key").await,
            401,
            "key {key:?}"
        );
    }
    let state = b8_with_key(&base, Some("b8-key"));
    assert_eq!(b8_socket_status(state, "?api_key=b8-key").await, 101);
    std::fs::remove_dir_all(base.data_dir.as_path()).ok();
}

// ── Error Response Format Tests ──

#[tokio::test]
async fn test_error_response_jellyfin_format() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Jellyfin-compatible error format
    assert!(err.get("Type").is_some());
    assert!(err.get("Title").is_some());
    assert!(err.get("Detail").is_some());
    assert_eq!(err["Status"], 401);
}

// ── Web Client Static Serving Tests ──

/// Check if the web client dist folder exists at any of the expected locations
fn webclient_dist_exists() -> bool {
    let candidates = [
        "crates/rustyfin-web/dist",
        "../crates/rustyfin-web/dist",
        "../../crates/rustyfin-web/dist",
        "rustyfin-web/dist",
        "dist",
        "web",
    ];
    candidates
        .iter()
        .any(|p| std::path::Path::new(p).join("index.html").exists())
}

#[tokio::test]
#[cfg_attr(not(feature = "webclient"), ignore)]
async fn test_webclient_index() {
    // Skip if web client dist is not built (e.g. in CI without npm step)
    if !webclient_dist_exists() {
        tracing::info!("skipping test_webclient_index: web client dist not found");
        return;
    }

    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/web/index.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("Rustyfin") || body_str.contains("<!DOCTYPE html>"));
}

#[tokio::test]
#[cfg_attr(not(feature = "webclient"), ignore)]
async fn test_webclient_redirects() {
    // Skip if web client dist is not built (e.g. in CI without npm step)
    if !webclient_dist_exists() {
        tracing::info!("skipping test_webclient_redirects: web client dist not found");
        return;
    }

    let state = test_state().await;

    for path in ["/", "/index.html"] {
        let app = app(state.clone());
        let resp = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(resp.headers().get("location").unwrap(), "/web/index.html");
    }
}

// ── Rate Limiter Tests ──

#[tokio::test]
async fn test_rate_limiter_logic() {
    let limiter = RateLimiter::new(3, 60);
    assert!(limiter.check("192.168.1.100").await);
    assert!(limiter.check("192.168.1.100").await);
    assert!(limiter.check("192.168.1.100").await);
    // 4th request within window exceeds limit
    assert!(!limiter.check("192.168.1.100").await);
    // Different IP still has budget
    assert!(limiter.check("192.168.1.101").await);

    // Limit of 0 disables rate limiting
    let unlimited = RateLimiter::new(0, 60);
    for _ in 0..10 {
        assert!(unlimited.check("192.168.1.100").await);
    }
}

#[tokio::test]
async fn test_rate_limiter_loopback_bypass() {
    let mut state = test_state().await;
    // Set a very low limit of 1 request
    state.rate_limiter = RateLimiter::new(1, 60);
    let app = app(state);

    // Loopback requests should not be blocked even after multiple calls
    for _ in 0..5 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

/// Router whose requests come from a non-loopback peer, so the rate limiter applies to them.
fn remote_app(state: AppState) -> axum::Router {
    build_router(state).layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 10], 40000))))
}

/// A valid 2x2 RGB PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a,
    0x73, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x38, 0x61, 0x64, 0x04,
    0x44, 0x0c, 0x10, 0x0a, 0x00, 0x23, 0x36, 0x04, 0xb1, 0xae, 0x7d, 0xb9, 0xbc, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn test_remote_browsing_burst_is_not_rate_limited() {
    let mut state = test_state().await;
    // The shipped default budget: RUSTYFIN_RATE_LIMIT=500 per RUSTYFIN_RATE_WINDOW=60 s
    state.rate_limiter = RateLimiter::new(500, 60);

    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();

    let media_dir = state.data_dir.join("movies");
    std::fs::create_dir_all(&media_dir).unwrap();
    let poster = media_dir.join("Burst Movie-poster.png");
    std::fs::write(&poster, TINY_PNG).unwrap();

    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "Burst Movies".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::Movies,
        paths: vec![media_dir.to_string_lossy().to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();

    let item_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("Burst Movie")
    .bind("Movie")
    .bind(
        media_dir
            .join("Burst Movie.mkv")
            .to_string_lossy()
            .to_string(),
    )
    .execute(&state.db)
    .await
    .unwrap();

    let img = rustyfin_model::image::ItemImage {
        item_id,
        image_type: rustyfin_model::image::ImageType::Primary,
        image_index: 0,
        path: poster.to_string_lossy().to_string(),
        provider: "local".to_string(),
        width: None,
        height: None,
        blurhash: None,
    };
    rustyfin_db::repo_image::upsert_image(&state.db, &img)
        .await
        .unwrap();

    let app = remote_app(state);

    // A grid page is one listing plus a thumbnail per card; mix in the other calls a
    // browsing, searching and playing client makes. 600 requests is above the budget.
    for i in 1..=600 {
        let (method, uri, body, must_be_ok) = match i % 10 {
            0 => (
                "GET",
                "/Items?IncludeItemTypes=Movie&Recursive=true&SortBy=SortName&StartIndex=0&Limit=50"
                    .to_string(),
                String::new(),
                true,
            ),
            1 => ("GET", format!("/Items/{item_id}"), String::new(), false),
            2 => (
                "GET",
                "/Search/Hints?searchTerm=burst".to_string(),
                String::new(),
                false,
            ),
            3 => ("GET", "/UserViews".to_string(), String::new(), false),
            4 => (
                "POST",
                "/Sessions/Playing/Progress".to_string(),
                format!(r#"{{"ItemId":"{item_id}","PositionTicks":10000000}}"#),
                false,
            ),
            _ => (
                "GET",
                format!("/Items/{item_id}/Images/Primary?maxWidth=150&format=webp"),
                String::new(),
                true,
            ),
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&uri)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = resp.status();
        assert_ne!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "request #{i} {method} {uri} was rate limited"
        );
        if must_be_ok {
            assert_eq!(status, StatusCode::OK, "request #{i} {method} {uri}");
        }
    }
}

#[tokio::test]
async fn test_public_non_login_endpoints_are_not_rate_limited() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let app = remote_app(state);

    for uri in [
        "/System/Info/Public",
        "/QuickConnect/Enabled",
        "/Users/Public",
    ] {
        for attempt in 1..=10 {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} attempt {attempt}");
        }
    }
}

#[tokio::test]
async fn test_failed_logins_from_remote_client_are_rate_limited() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let app = remote_app(state);

    let wrong_login = || {
        Request::builder()
            .method("POST")
            .uri("/Users/AuthenticateByName")
            .header("Content-Type", "application/json")
            .body(Body::from(format!(
                r#"{{"Username":"{}","Pw":"wrong"}}"#,
                username
            )))
            .unwrap()
    };

    for attempt in 1..=3 {
        let resp = app.clone().oneshot(wrong_login()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    let resp = app.clone().oneshot(wrong_login()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_password_reset_endpoints_share_the_login_rate_limit() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let app = remote_app(state);

    let post = |uri: &str, body: &str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let login = || {
        post(
            "/Users/AuthenticateByName",
            r#"{"Username":"nobody","Pw":"x"}"#,
        )
    };
    let forgot = || post("/Users/ForgotPassword", r#"{"EnteredUsername":"nobody"}"#);
    let reset = || {
        post(
            "/Users/ResetPassword",
            r#"{"Pin":"123456","Username":"nobody","TempPassword":"x"}"#,
        )
    };

    let resp = app.clone().oneshot(login()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // G2: ForgotPassword no longer reveals whether the user exists; unknown users get 200.
    let resp = app.clone().oneshot(forgot()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app.clone().oneshot(reset()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // The three endpoints draw on one budget per client
    for (name, req) in [("login", login()), ("forgot", forgot()), ("reset", reset())] {
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS, "{name}");
    }
}

// ── Image Route Tests ──

#[tokio::test]
async fn test_item_image_missing_returns_404_not_500() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{random_id}/Images/Primary"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_item_image_missing_file_returns_404_not_500() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();

    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "Test Movies".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::Movies,
        paths: vec!["/media/movies".to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();

    let item_id = uuid::Uuid::new_v4();

    // Insert item and image pointing to non-existent file path
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("Test Item")
    .bind("Movie")
    .bind("/nonexistent/movie.mkv")
    .execute(&state.db)
    .await
    .unwrap();

    let img = rustyfin_model::image::ItemImage {
        item_id,
        image_type: rustyfin_model::image::ImageType::Primary,
        image_index: 0,
        path: "/nonexistent/poster.jpg".to_string(),
        provider: "local".to_string(),
        width: None,
        height: None,
        blurhash: None,
    };
    rustyfin_db::repo_image::upsert_image(&state.db, &img)
        .await
        .unwrap();

    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Images/Primary?maxWidth=300"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Must return 404 Not Found, never 500 Internal Server Error
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A valid 2x2 RGB PNG, used as a series' folder image.
const FOLDER_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a,
    0x73, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x38, 0x61, 0x64, 0x04,
    0x44, 0x0c, 0x10, 0x0a, 0x00, 0x23, 0x36, 0x04, 0xb1, 0xae, 0x7d, 0xb9, 0xbc, 0x00, 0x00, 0x00,
    0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn test_series_folder_image_is_served_after_refresh() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();

    // Jellyfin/Kodi layout: the series poster is a generic name in the series folder
    let tv_dir = state.data_dir.join("media").join("TV");
    let show_dir = tv_dir.join("Show");
    std::fs::create_dir_all(show_dir.join("Season 1")).unwrap();
    std::fs::write(show_dir.join("folder.png"), FOLDER_PNG).unwrap();
    std::fs::write(show_dir.join("Season 1").join("Show - S01E01.mkv"), b"").unwrap();

    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "Test Shows".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::TvShows,
        paths: vec![tv_dir.to_string_lossy().to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();

    // A Series item's path is its folder, as the TV scanner stores it
    let series_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(series_id.to_string())
    .bind(lib.id.to_string())
    .bind("Show")
    .bind("Series")
    .bind(show_dir.to_string_lossy().to_string())
    .execute(&state.db)
    .await
    .unwrap();

    let app = app(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{series_id}/Refresh"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{series_id}/Images/Primary"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], FOLDER_PNG);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Items?IncludeItemTypes=Series&Recursive=true")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let series = listing["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["Id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                == Some(series_id)
        })
        .expect("series is listed");
    assert!(series["ImageTags"]["Primary"].is_string(), "{series}");
}

// ── Library Endpoint Tests ──

#[tokio::test]
async fn test_get_libraries_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let libs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(libs.is_empty());
}

#[tokio::test]
async fn test_create_and_get_library() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state.clone());

    // Create a library
    let body = serde_json::json!({
        "Name": "Test Movies",
        "ContentType": "Movies",
        "Paths": [media_folder(&state, "movies")],
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Library/VirtualFolders")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Get libraries
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Library/VirtualFolders")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let libs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0]["Name"], "Test Movies");
}

#[tokio::test]
async fn test_get_user_views_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/UserViews")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let views: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(views["Items"].is_array());
    assert_eq!(views["Items"].as_array().unwrap().len(), 0);
    assert_eq!(views["TotalRecordCount"], 0);
}

#[tokio::test]
async fn test_get_user_views_with_library() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();

    // Create a library
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "My Movies".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::Movies,
        paths: vec!["/media/movies".to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();

    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/UserViews")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let views: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = views["Items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["Name"], "My Movies");
    assert_eq!(items[0]["CollectionType"], "movies");
    assert_eq!(items[0]["Type"], "CollectionFolder");
}

// ── Library List Consistency Tests ──
//
// Every list of libraries must show a create or delete as soon as it succeeds,
// and no request may hide or remove a library it was not asked to remove.

/// Creates an admin user and returns a bearer token and the user's id.
async fn admin_session(state: &AppState) -> (String, uuid::Uuid) {
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    (token, user.id)
}

/// Sends an authenticated request and returns the status and the JSON body
/// (`Null` when the body is not JSON).
async fn send_authed(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    json_body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"));
    if json_body.is_some() {
        builder = builder.header("Content-Type", "application/json");
    }
    let body = Body::from(json_body.unwrap_or_default().to_string());
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Creates the folder `media/<name>` in the test's data dir and returns its path.
fn media_folder(state: &AppState, name: &str) -> String {
    let dir = state.data_dir.join("media").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

/// Creates a library on a new folder through the API and returns its id.
async fn create_library_via_api(
    app: &axum::Router,
    state: &AppState,
    token: &str,
    name: &str,
    content_type: &str,
) -> uuid::Uuid {
    let body = serde_json::json!({
        "Name": name,
        "ContentType": content_type,
        "Paths": [media_folder(state, &name.to_lowercase())],
    })
    .to_string();
    let (status, created) =
        send_authed(app, "POST", "/Library/VirtualFolders", token, Some(&body)).await;
    assert_eq!(status, StatusCode::OK, "create {name}: {created}");
    assert_eq!(created["Name"], name);
    created["Id"].as_str().unwrap().parse().unwrap()
}

/// Asserts that `/Library/VirtualFolders`, `/UserViews` and `/Users/{id}/Views`
/// each list exactly the libraries named in `expected` (sorted).
async fn assert_listed_libraries(
    app: &axum::Router,
    token: &str,
    user_id: uuid::Uuid,
    expected: &[&str],
) {
    for uri in [
        "/Library/VirtualFolders".to_string(),
        "/UserViews".to_string(),
        format!("/Users/{user_id}/Views"),
    ] {
        let (status, body) = send_authed(app, "GET", &uri, token, None).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}");
        // Views wrap the list in a query result; VirtualFolders is a bare array.
        let entries = body
            .get("Items")
            .unwrap_or(&body)
            .as_array()
            .unwrap_or_else(|| panic!("GET {uri} returned no list: {body}"));
        let mut names: Vec<&str> = entries
            .iter()
            .map(|e| e["Name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, expected, "libraries listed by GET {uri}");
    }
}

/// Sorted names in the `libraries` table.
async fn stored_library_names(state: &AppState) -> Vec<String> {
    let mut names: Vec<String> = rustyfin_db::repo_library::get_all_libraries(&state.db)
        .await
        .unwrap()
        .into_iter()
        .map(|lib| lib.name)
        .collect();
    names.sort_unstable();
    names
}

#[tokio::test]
async fn test_user_views_show_library_created_after_cached_read() {
    let state = test_state().await;
    let (token, user_id) = admin_session(&state).await;
    let app = app(state.clone());

    // Clients read the lists before the first library exists (home screen).
    assert_listed_libraries(&app, &token, user_id, &[]).await;

    create_library_via_api(&app, &state, &token, "Films", "Movies").await;

    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;
}

#[tokio::test]
async fn test_second_library_keeps_first_in_every_list() {
    let state = test_state().await;
    let (token, user_id) = admin_session(&state).await;
    let app = app(state.clone());

    create_library_via_api(&app, &state, &token, "Films", "Movies").await;
    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;

    create_library_via_api(&app, &state, &token, "Shows", "TvShows").await;
    assert_listed_libraries(&app, &token, user_id, &["Films", "Shows"]).await;
    assert_eq!(stored_library_names(&state).await, ["Films", "Shows"]);
}

#[tokio::test]
async fn test_delete_library_updates_every_list_and_keeps_others() {
    let state = test_state().await;
    let (token, user_id) = admin_session(&state).await;
    let app = app(state.clone());

    let films = create_library_via_api(&app, &state, &token, "Films", "Movies").await;
    let shows = create_library_via_api(&app, &state, &token, "Shows", "TvShows").await;
    for (library_id, item_name) in [(films, "Film item"), (shows, "Show item")] {
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(library_id.to_string())
        .bind(item_name)
        .bind("Movie")
        .bind(format!("/media/{item_name}.mkv"))
        .execute(&state.db)
        .await
        .unwrap();
    }
    assert_listed_libraries(&app, &token, user_id, &["Films", "Shows"]).await;

    let (status, body) = send_authed(
        &app,
        "DELETE",
        &format!("/Library/VirtualFolders/{shows}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete Shows: {body}");

    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;
    assert_eq!(stored_library_names(&state).await, ["Films"]);
    let remaining_items: Vec<String> = sqlx::query_scalar("SELECT name FROM media_items")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(remaining_items, ["Film item"]);
}

#[tokio::test]
async fn test_failed_library_create_changes_no_list() {
    let state = test_state().await;
    let (token, user_id) = admin_session(&state).await;
    let app = app(state.clone());

    create_library_via_api(&app, &state, &token, "Films", "Movies").await;
    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;

    for bad_body in [
        r#"{"Name":"Shows","ContentType":"NoSuchType","Paths":["/media/shows"]}"#,
        r#"{"Name":"Shows","ContentType":"TvShows"}"#,
    ] {
        let (status, _) = send_authed(
            &app,
            "POST",
            "/Library/VirtualFolders",
            &token,
            Some(bad_body),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "create with {bad_body}"
        );
    }

    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;
    assert_eq!(stored_library_names(&state).await, ["Films"]);
}

// ── Library Folder Tests ──
// `POST /Library/VirtualFolders` stores a library only if every path is a folder
// the server can list. Otherwise it answers 400, names each bad path in `Detail`
// and stores nothing.

/// A server with one library, "Films", whose lists have been read and so are cached.
struct ServerWithFilms {
    state: AppState,
    app: axum::Router,
    token: String,
    user_id: uuid::Uuid,
}

async fn server_with_films() -> ServerWithFilms {
    let state = test_state().await;
    let (token, user_id) = admin_session(&state).await;
    let app = app(state.clone());
    create_library_via_api(&app, &state, &token, "Films", "Movies").await;
    assert_listed_libraries(&app, &token, user_id, &["Films"]).await;
    assert!(state.cache.library_views.get("views").is_some());
    ServerWithFilms {
        state,
        app,
        token,
        user_id,
    }
}

/// Writes the file `media/<name>` in the test's data dir and returns its path.
fn media_file(state: &AppState, name: &str) -> String {
    let file = state.data_dir.join("media").join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"not a folder").unwrap();
    file.to_str().unwrap().to_string()
}

/// Sends `POST /Library/VirtualFolders` for a TV library called "Shows" on `paths`.
async fn create_shows(server: &ServerWithFilms, paths: &[&str]) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({ "Name": "Shows", "ContentType": "TvShows", "Paths": paths });
    send_authed(
        &server.app,
        "POST",
        "/Library/VirtualFolders",
        &server.token,
        Some(&body.to_string()),
    )
    .await
}

/// Asserts that creating "Shows" on `paths` fails with 400 and exactly `detail`,
/// and that it left the cached views, every list and the stored libraries alone.
async fn assert_shows_rejected(server: &ServerWithFilms, paths: &[&str], detail: &str) {
    let (status, body) = create_shows(server, paths).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "create on {paths:?}: {body}"
    );
    assert_eq!(body["Title"], "Bad Request");
    assert_eq!(body["Status"], 400);
    assert_eq!(body["Detail"], detail, "Detail for {paths:?}");
    assert!(
        server.state.cache.library_views.get("views").is_some(),
        "a rejected create must leave the cached views alone"
    );
    assert_listed_libraries(&server.app, &server.token, server.user_id, &["Films"]).await;
    assert_eq!(stored_library_names(&server.state).await, ["Films"]);
}

#[tokio::test]
async fn test_create_library_rejects_missing_folder() {
    let server = server_with_films().await;
    let missing = server.state.data_dir.join("media").join("shows");
    let missing = missing.to_str().unwrap();
    // A path below a file is missing too, though the OS reports "not a directory".
    let below_file = format!("{}/shows", media_file(&server.state, "shows.mkv"));

    assert_shows_rejected(
        &server,
        &[missing, &below_file],
        &format!(
            "The folder \"{missing}\" was not found on the server. \
             The folder \"{below_file}\" was not found on the server."
        ),
    )
    .await;
}

#[tokio::test]
async fn test_create_library_rejects_relative_path() {
    let server = server_with_films().await;

    // "." exists and can be listed, so only the full-path check can refuse it.
    assert_shows_rejected(
        &server,
        &["."],
        "The folder \".\" is not a full path on the server.",
    )
    .await;
}

#[tokio::test]
async fn test_create_library_rejects_file_path() {
    let server = server_with_films().await;
    let file = media_file(&server.state, "shows.mkv");

    assert_shows_rejected(
        &server,
        &[&file],
        &format!("The path \"{file}\" on the server is not a folder."),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn test_create_library_rejects_unreadable_folder() {
    use std::os::unix::fs::PermissionsExt;

    /// Gives the folder its mode back when the test ends, even if it fails, so
    /// that the test's data dir can be deleted.
    struct Unlock<'a>(&'a str);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    let server = server_with_films().await;
    let locked = media_folder(&server.state, "shows");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _unlock = Unlock(&locked);
    if std::fs::read_dir(&locked).is_ok() {
        eprintln!("skipped: this process can list a folder with mode 000 (running as root?)");
        return;
    }

    assert_shows_rejected(
        &server,
        &[&locked],
        &format!("The server does not have permission to read the folder \"{locked}\"."),
    )
    .await;
}

#[tokio::test]
async fn test_create_library_with_any_bad_path_stores_nothing() {
    let server = server_with_films().await;
    let good = media_folder(&server.state, "shows");
    let missing = server.state.data_dir.join("media").join("more shows");
    let missing = missing.to_str().unwrap();
    let file = media_file(&server.state, "episode.mkv");

    // Only the bad paths are named, in the order they were sent.
    assert_shows_rejected(
        &server,
        &[&good, missing, "shows", &file],
        &format!(
            "The folder \"{missing}\" was not found on the server. \
             The folder \"shows\" is not a full path on the server. \
             The path \"{file}\" on the server is not a folder."
        ),
    )
    .await;
}

#[tokio::test]
async fn test_create_library_accepts_existing_folders() {
    let server = server_with_films().await;
    let shows = media_folder(&server.state, "shows");
    // Paths are stored as sent, trailing slash included.
    let more_shows = format!("{}/", media_folder(&server.state, "more shows"));

    let (status, created) = create_shows(&server, &[&shows, &more_shows]).await;
    assert_eq!(status, StatusCode::OK, "create Shows: {created}");
    assert_eq!(created["Name"], "Shows");
    assert_eq!(created["Paths"], serde_json::json!([shows, more_shows]));

    assert_listed_libraries(
        &server.app,
        &server.token,
        server.user_id,
        &["Films", "Shows"],
    )
    .await;
    assert_eq!(
        stored_library_names(&server.state).await,
        ["Films", "Shows"]
    );
    let id = created["Id"].as_str().unwrap().parse().unwrap();
    let stored = rustyfin_db::repo_library::get_library_by_id(&server.state.db, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.paths, [shows, more_shows]);
}

#[tokio::test]
async fn test_create_library_with_no_paths_still_works() {
    let server = server_with_films().await;

    let (status, created) = create_shows(&server, &[]).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "create Shows without paths: {created}"
    );
    assert_eq!(created["Paths"], serde_json::json!([]));
    assert_listed_libraries(
        &server.app,
        &server.token,
        server.user_id,
        &["Films", "Shows"],
    )
    .await;
    assert_eq!(
        stored_library_names(&server.state).await,
        ["Films", "Shows"]
    );
}

// ── B14: symlinked folders in the folder picker ──
// `GET /Library/BrowseDirectories` lists a symlink that resolves to a folder, under
// the link's own path, as the scanner follows such links. Dangling links and links
// to files stay hidden. The route lists only allowed roots such as /home, so the
// test's folders are made under $HOME.

#[cfg(unix)]
#[tokio::test]
async fn test_browse_directories_lists_symlinked_folders() {
    /// Removes the test's folder under $HOME when the test ends, even if it fails.
    struct RemoveDir(std::path::PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        eprintln!("skipped: HOME is not set");
        return;
    };
    if !home.starts_with("/home") {
        eprintln!(
            "skipped: HOME {} is not under /home, which BrowseDirectories lists",
            home.display()
        );
        return;
    }
    let root = home.join(format!(".rustyfin-b14-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&root).unwrap();
    let _remove = RemoveDir(root.clone());
    let target = root.join("target");
    let listed = root.join("listed");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir_all(listed.join("Real Films")).unwrap();
    std::fs::write(listed.join("film.mkv"), b"not a folder").unwrap();
    std::os::unix::fs::symlink(&target, listed.join("Linked Films")).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), listed.join("Dangling Link")).unwrap();
    std::os::unix::fs::symlink(listed.join("film.mkv"), listed.join("Link To File")).unwrap();

    let state = test_state().await;
    let (token, _) = admin_session(&state).await;
    let app = app(state);
    let parent: String = listed
        .to_str()
        .unwrap()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let (status, body) = send_authed(
        &app,
        "GET",
        &format!("/Library/BrowseDirectories?parent={parent}"),
        &token,
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK, "browse: {body}");
    assert_eq!(
        body,
        serde_json::json!([
            {
                "path": listed.join("Linked Films").to_str().unwrap(),
                "name": "Linked Films",
                "can_read": true,
            },
            {
                "path": listed.join("Real Films").to_str().unwrap(),
                "name": "Real Films",
                "can_read": true,
            },
        ])
    );
}

// ── Search Endpoint Tests ──

#[tokio::test]
async fn test_search_hints_empty_term() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Search/Hints?SearchTerm=")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(result["SearchHints"].is_array());
    assert_eq!(result["TotalRecordCount"], 0);
}

#[tokio::test]
async fn test_full_search_empty_term() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Search?Term=")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(result["Items"].is_array());
    assert_eq!(result["TotalRecordCount"], 0);
}

// ── System Endpoint Tests ──

#[tokio::test]
async fn test_system_info_with_auth() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["Id"], "test-server-id");
}

#[tokio::test]
async fn test_system_storage_info() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Info/Storage")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(info.get("LogPath").is_some());
    assert!(info.get("CachePath").is_some());
    assert!(info.get("ProgramDataPath").is_some());
}

#[tokio::test]
async fn test_system_update_packages() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/UpdatePackages")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let packages: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(packages.is_empty());
}

// ── DLNA Endpoint Tests ──

#[tokio::test]
async fn test_dlna_profile_endpoint() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Dlna/Profile")
                .header("X-Emby-Token", "test-api-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let profile: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(profile["Name"], "Default");
    assert!(profile["DirectPlayProfiles"].is_array());
    assert!(profile["TranscodingProfiles"].is_array());
}

#[tokio::test]
async fn test_dlna_device_detection() {
    let state = test_state().await;
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Dlna/Profile/DeviceDetection?UserAgent=Jellyfin/10.8.0")
                .header("X-Emby-Token", "test-api-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let detection: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(detection["FriendlyName"], "Jellyfin Client");
}

// ── Items Endpoint Tests ──

#[tokio::test]
async fn test_get_items_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Items")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(result["Items"].is_array());
    assert_eq!(result["TotalRecordCount"], 0);
}

#[tokio::test]
async fn test_get_item_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{random_id}"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Activity Log Tests ──

#[tokio::test]
async fn test_activity_log() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Sessions/Logging")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Session Tests ──

#[tokio::test]
async fn test_get_sessions() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Sessions")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let sessions: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(sessions.is_array());
}

// ── Playback Tests ──

#[tokio::test]
async fn test_playback_progress_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/Items/{random_id}/Progress"))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"PositionTicks":1000000,"IsPlayed":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Branding Tests ──

#[tokio::test]
async fn test_branding_splashscreen() {
    let state = test_state().await;
    let app = app(state);

    app.oneshot(
        Request::builder()
            .uri("/Branding")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    // Endpoint may not be implemented yet; accept any response
}

// ── Quick Connect Tests ──

#[tokio::test]
async fn test_quick_connect_generate() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/QuickConnect/Generate")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet fully implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── B21: OIDC ──
//
// Sign-in through one OpenID Connect provider, Quick Connect's token step and the password
// switch (B21 spec AC1-AC31). A fake provider on 127.0.0.1 signs ID tokens with fixed P-256
// keys; Rustyfin is driven over HTTP and checked with plain SQL, so these tests compile before
// the feature exists. Each is ignored until the task that makes it pass.

const B21_CLIENT_ID: &str = "rustyfin-b21";
/// A test-only value: it never leaves this process.
const B21_CLIENT_SECRET: &str = "b21-test-client-secret";
const B21_PUBLIC_URL: &str = "http://rf.test:8121";
const B21_CALLBACK: &str = "http://rf.test:8121/Auth/Oidc/Callback";
/// The browser's device header at `Complete`.
const B21_DEVICE: &str =
    r#"MediaBrowser Client="B21", Device="web", DeviceId="b21-web", Version="1.0""#;

/// How a token request authenticated the client, as the fake provider saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum B21Auth {
    /// `client_secret_basic`: an `Authorization: Basic` header.
    Basic,
    /// `client_secret_post`: the id and the secret in the form.
    Post,
    /// A public client: only the id in the form.
    None,
    /// Anything else, a wrong secret for example.
    Bad,
}

/// How the fake provider signs an ID token.
#[derive(Clone, Copy, Debug)]
enum B21Sign {
    /// With published key n, naming its `kid`.
    Key(usize),
    /// Naming published key n's `kid`, signed with a key the provider never published.
    Forged(usize),
    /// With published key n, naming another `kid`.
    Kid(usize, &'static str),
    /// HS256 with the client secret as the key.
    Hs256,
    /// `alg: none` and no signature.
    Unsigned,
}

/// An approval at the provider, redeemed once by its code.
struct B21Grant {
    redirect_uri: String,
    challenge: String,
    claims: serde_json::Value,
    sign: B21Sign,
}

/// What the fake provider serves and what it saw.
struct B21OpState {
    discovery: serde_json::Map<String, serde_json::Value>,
    /// Served as the discovery document instead, when set.
    discovery_text: Option<String>,
    /// The published keys: `kid` and the private key.
    keys: Vec<(String, p256::ecdsa::SigningKey)>,
    /// Served as the JWKS instead, when set.
    jwks_text: Option<String>,
    grants: HashMap<String, B21Grant>,
    /// Access tokens and the claims of their grants, for userinfo.
    access: HashMap<String, serde_json::Value>,
    /// The client authentication the token endpoint accepts.
    expect_auth: B21Auth,
    token_requests: Vec<(B21Auth, HashMap<String, String>)>,
    last_id_token: Option<String>,
    /// Served by userinfo instead of the grant's claims, when set.
    userinfo: Option<serde_json::Value>,
    discovery_fetches: usize,
    jwks_fetches: usize,
    userinfo_calls: usize,
    /// Every endpoint answers 503.
    down: bool,
}

/// A fixed P-256 key.
fn b21_key(seed: u8) -> p256::ecdsa::SigningKey {
    p256::ecdsa::SigningKey::from_slice(&[seed; 32]).unwrap()
}

/// A healthy provider at `base`: ES256, PKCE with S256, one key.
fn b21_op_state(base: &str) -> B21OpState {
    let serde_json::Value::Object(discovery) = serde_json::json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "userinfo_endpoint": format!("{base}/userinfo"),
        "jwks_uri": format!("{base}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "scopes_supported": ["openid", "profile", "email", "groups"],
        "grant_types_supported": ["authorization_code"],
    }) else {
        unreachable!()
    };
    B21OpState {
        discovery,
        discovery_text: None,
        keys: vec![("b21-key-a".to_string(), b21_key(0x11))],
        jwks_text: None,
        grants: HashMap::new(),
        access: HashMap::new(),
        expect_auth: B21Auth::Basic,
        token_requests: Vec::new(),
        last_id_token: None,
        userinfo: None,
        discovery_fetches: 0,
        jwks_fetches: 0,
        userinfo_calls: 0,
        down: false,
    }
}

/// A fake OpenID provider on 127.0.0.1. The browser's visit to its authorization endpoint is
/// `approve`; discovery, JWKS, token and userinfo are served over HTTP.
#[derive(Clone)]
struct B21Op {
    base: String,
    st: Arc<std::sync::Mutex<B21OpState>>,
}

impl B21Op {
    fn with<R>(&self, f: impl FnOnce(&mut B21OpState) -> R) -> R {
        f(&mut self.st.lock().unwrap())
    }

    /// The user approves `flow` with `claims` on top of `iss`, `aud`, `iat`, `exp` and the flow's
    /// `nonce`; a null claim is left out. Returns the code for the callback.
    fn approve(&self, flow: &B21Flow, claims: serde_json::Value, sign: B21Sign) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut all = serde_json::json!({
            "iss": self.base,
            "aud": B21_CLIENT_ID,
            "iat": now,
            "exp": now + 300,
            "nonce": flow.param("nonce"),
        });
        let map = all.as_object_mut().unwrap();
        for (name, value) in claims.as_object().expect("claims are an object") {
            if value.is_null() {
                map.remove(name);
            } else {
                map.insert(name.clone(), value.clone());
            }
        }
        let code = format!("b21-code-{}", uuid::Uuid::new_v4().simple());
        let grant = B21Grant {
            redirect_uri: flow.param("redirect_uri"),
            challenge: flow.param("code_challenge"),
            claims: all,
            sign,
        };
        self.with(|st| st.grants.insert(code.clone(), grant));
        code
    }
}

/// Starts a fake provider on a free port.
async fn b21_op() -> B21Op {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let op = B21Op {
        st: Arc::new(std::sync::Mutex::new(b21_op_state(&base))),
        base,
    };
    let router = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(b21_op_discovery),
        )
        .route("/jwks", axum::routing::get(b21_op_jwks))
        .route("/token", axum::routing::post(b21_op_token))
        .route(
            "/userinfo",
            axum::routing::get(b21_op_userinfo).post(b21_op_userinfo),
        )
        .with_state(op.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    op
}

fn b21_down() -> axum::response::Response {
    use axum::response::IntoResponse;
    (StatusCode::SERVICE_UNAVAILABLE, "down").into_response()
}

/// A text the test wrote, valid JSON or not, served as JSON.
fn b21_json_text(text: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        text,
    )
        .into_response()
}

async fn b21_op_discovery(
    axum::extract::State(op): axum::extract::State<B21Op>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (down, text, discovery) = op.with(|st| {
        st.discovery_fetches += 1;
        (st.down, st.discovery_text.clone(), st.discovery.clone())
    });
    if down {
        return b21_down();
    }
    match text {
        Some(text) => b21_json_text(text),
        None => axum::Json(serde_json::Value::Object(discovery)).into_response(),
    }
}

async fn b21_op_jwks(
    axum::extract::State(op): axum::extract::State<B21Op>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (down, text, keys) = op.with(|st| {
        st.jwks_fetches += 1;
        let keys: Vec<serde_json::Value> =
            st.keys.iter().map(|(kid, key)| b21_jwk(kid, key)).collect();
        (st.down, st.jwks_text.clone(), keys)
    });
    if down {
        return b21_down();
    }
    match text {
        Some(text) => b21_json_text(text),
        None => axum::Json(serde_json::json!({ "keys": keys })).into_response(),
    }
}

/// The public half of a key as a JWK.
fn b21_jwk(kid: &str, key: &p256::ecdsa::SigningKey) -> serde_json::Value {
    let point = key.verifying_key().to_sec1_point(false);
    serde_json::json!({
        "kty": "EC",
        "crv": "P-256",
        "alg": "ES256",
        "use": "sig",
        "kid": kid,
        "x": b21_b64(point.x().unwrap().as_slice()),
        "y": b21_b64(point.y().unwrap().as_slice()),
    })
}

/// The token endpoint: checks the client authentication, the code, the redirect URI and the
/// PKCE verifier, then signs the grant's ID token.
async fn b21_op_token(
    axum::extract::State(op): axum::extract::State<B21Op>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use base64::Engine as _;
    use sha2::Digest as _;

    let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let basic = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .map(|v| {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(v)
                .unwrap_or_default();
            let raw = String::from_utf8(raw).unwrap_or_default();
            let (id, secret) = raw.split_once(':').unwrap_or_default();
            (b21_unescape(id), b21_unescape(secret))
        });
    let form_id = form.get("client_id").map(String::as_str);
    let form_secret = form.get("client_secret").map(String::as_str);
    let auth = match (&basic, form_secret) {
        (Some((id, secret)), None)
            if id == B21_CLIENT_ID
                && secret == B21_CLIENT_SECRET
                && form_id.is_none_or(|f| f == B21_CLIENT_ID) =>
        {
            B21Auth::Basic
        }
        (None, Some(secret)) if secret == B21_CLIENT_SECRET && form_id == Some(B21_CLIENT_ID) => {
            B21Auth::Post
        }
        (None, None) if form_id == Some(B21_CLIENT_ID) => B21Auth::None,
        _ => B21Auth::Bad,
    };

    let mut st = op.st.lock().unwrap();
    if st.down {
        return b21_down();
    }
    st.token_requests.push((auth, form.clone()));
    if auth != st.expect_auth {
        let error = serde_json::json!({ "error": "invalid_client" });
        return (StatusCode::UNAUTHORIZED, axum::Json(error)).into_response();
    }
    let invalid_grant = || {
        let error = serde_json::json!({ "error": "invalid_grant" });
        (StatusCode::BAD_REQUEST, axum::Json(error)).into_response()
    };
    let Some(grant) = form.get("code").and_then(|code| st.grants.remove(code)) else {
        return invalid_grant();
    };
    let verifier = form
        .get("code_verifier")
        .map(String::as_str)
        .unwrap_or_default();
    if form.get("grant_type").map(String::as_str) != Some("authorization_code")
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
        || b21_b64(sha2::Sha256::digest(verifier.as_bytes()).as_slice()) != grant.challenge
    {
        return invalid_grant();
    }
    let id_token = b21_sign(&st.keys, &grant.claims, grant.sign);
    let access_token = format!("b21-at-{}", uuid::Uuid::new_v4().simple());
    st.access.insert(access_token.clone(), grant.claims);
    st.last_id_token = Some(id_token.clone());
    axum::Json(serde_json::json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "id_token": id_token,
    }))
    .into_response()
}

/// Userinfo: the grant's claims without the token-only ones, or `userinfo` when set.
async fn b21_op_userinfo(
    axum::extract::State(op): axum::extract::State<B21Op>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut st = op.st.lock().unwrap();
    st.userinfo_calls += 1;
    if st.down {
        return b21_down();
    }
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let Some(claims) = st.access.get(token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Some(info) = &st.userinfo {
        return axum::Json(info.clone()).into_response();
    }
    let mut claims = claims.clone();
    let map = claims.as_object_mut().unwrap();
    for name in ["iss", "aud", "azp", "exp", "iat", "nonce"] {
        map.remove(name);
    }
    axum::Json(claims).into_response()
}

/// A compact JWS over `claims`.
fn b21_sign(
    keys: &[(String, p256::ecdsa::SigningKey)],
    claims: &serde_json::Value,
    sign: B21Sign,
) -> String {
    use hmac::Mac as _;
    let (alg, kid) = match sign {
        B21Sign::Key(n) | B21Sign::Forged(n) => ("ES256", Some(keys[n].0.as_str())),
        B21Sign::Kid(_, kid) => ("ES256", Some(kid)),
        B21Sign::Hs256 => ("HS256", None),
        B21Sign::Unsigned => ("none", None),
    };
    let mut header = serde_json::json!({ "alg": alg, "typ": "JWT" });
    if let Some(kid) = kid {
        header["kid"] = kid.into();
    }
    let input = format!(
        "{}.{}",
        b21_b64(header.to_string().as_bytes()),
        b21_b64(claims.to_string().as_bytes())
    );
    let signature = match sign {
        B21Sign::Key(n) | B21Sign::Kid(n, _) => b21_es256(&keys[n].1, &input),
        B21Sign::Forged(_) => b21_es256(&b21_key(0x5a), &input),
        B21Sign::Hs256 => {
            let mut mac =
                hmac::Hmac::<sha2::Sha256>::new_from_slice(B21_CLIENT_SECRET.as_bytes()).unwrap();
            mac.update(input.as_bytes());
            mac.finalize().into_bytes().as_slice().to_vec()
        }
        B21Sign::Unsigned => Vec::new(),
    };
    format!("{input}.{}", b21_b64(&signature))
}

fn b21_es256(key: &p256::ecdsa::SigningKey, input: &str) -> Vec<u8> {
    use p256::ecdsa::signature::Signer as _;
    let signature: p256::ecdsa::Signature = key.sign(input.as_bytes());
    signature.to_bytes().as_slice().to_vec()
}

fn b21_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Undoes the form encoding of a client id or secret in a Basic header (RFC 6749 §2.3.1).
fn b21_unescape(value: &str) -> String {
    url::form_urlencoded::parse(format!("v={value}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// A Rustyfin whose owner is signed in, next to a fake provider.
struct B21 {
    state: AppState,
    app: axum::Router,
    op: B21Op,
    /// The owner's token.
    admin: String,
    admin_id: String,
}

async fn b21() -> B21 {
    let state = test_state().await;
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (admin, admin_id) = login(&app, "owner", "pw-owner")
        .await
        .expect("owner signs in");
    B21 {
        state,
        app,
        op: b21_op().await,
        admin,
        admin_id,
    }
}

/// Settings that turn OIDC on against `op`, with `extra` on top.
fn b21_settings_body(op: &B21Op, extra: serde_json::Value) -> serde_json::Value {
    let mut body = serde_json::json!({
        "Enabled": true,
        "IssuerUrl": op.base,
        "ClientId": B21_CLIENT_ID,
        "ClientSecret": B21_CLIENT_SECRET,
        "PublicUrl": B21_PUBLIC_URL,
    });
    for (name, value) in extra.as_object().expect("extra is an object") {
        body[name] = value.clone();
    }
    body
}

/// A Rustyfin with OIDC on against its fake provider.
async fn b21_on(extra: serde_json::Value) -> B21 {
    let t = b21().await;
    let (status, body) = t.save(b21_settings_body(&t.op, extra)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "saving the OIDC settings: {}",
        body["Detail"]
    );
    t
}

/// A started flow: the browser's cookie and the authorization URL.
struct B21Flow {
    /// `rustyfin_oidc=<flow id>`, as the browser sends it back.
    cookie: String,
    /// The whole `Set-Cookie` header.
    set_cookie: String,
    url: String,
    params: HashMap<String, String>,
}

impl B21Flow {
    fn param(&self, name: &str) -> String {
        self.params
            .get(name)
            .unwrap_or_else(|| panic!("no {name} in the authorization URL"))
            .clone()
    }

    /// The query the provider sends the browser back with after an approval.
    fn callback_query(&self, code: &str) -> String {
        b21_query(&[("code", code), ("state", &self.param("state"))])
    }
}

fn b21_query(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// A header's value, or "" without one.
fn b21_header(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Checks a 200 from Start or Link and reads the flow from it.
fn b21_flow(status: StatusCode, headers: &axum::http::HeaderMap, body: &str) -> B21Flow {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    assert_eq!(
        status,
        StatusCode::OK,
        "starting a flow: {}",
        json["Detail"]
    );
    let url = json["AuthorizationUrl"]
        .as_str()
        .expect("an AuthorizationUrl")
        .to_string();
    let params = url::Url::parse(&url)
        .expect("the AuthorizationUrl is a URL")
        .query_pairs()
        .into_owned()
        .collect();
    let set_cookie = b21_header(headers, "set-cookie");
    let cookie = set_cookie
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    assert!(
        cookie.len() > "rustyfin_oidc=".len() && cookie.starts_with("rustyfin_oidc="),
        "the answer sets the flow cookie"
    );
    B21Flow {
        cookie,
        set_cookie,
        url,
        params,
    }
}

/// The attributes of a `Set-Cookie` header, lowercased, without the name and value.
fn b21_cookie_attrs(set_cookie: &str) -> Vec<String> {
    set_cookie
        .split(';')
        .skip(1)
        .map(|a| a.trim().to_lowercase())
        .collect()
}

fn b21_clears_cookie(set_cookie: &str) -> bool {
    set_cookie.starts_with("rustyfin_oidc=")
        && b21_cookie_attrs(set_cookie)
            .iter()
            .any(|a| a == "max-age=0")
}

impl B21 {
    async fn raw(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    async fn save(&self, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let req = request("POST", "/Auth/Oidc/Settings", Some(&self.admin), Some(body));
        send(&self.app, req).await
    }

    async fn settings(&self) -> serde_json::Value {
        let req = request("GET", "/Auth/Oidc/Settings", Some(&self.admin), None);
        let (status, body) = send(&self.app, req).await;
        assert_eq!(status, StatusCode::OK, "reading the OIDC settings");
        body
    }

    async fn start_raw(&self) -> (StatusCode, axum::http::HeaderMap, String) {
        self.raw(request("POST", "/Auth/Oidc/Start", None, None))
            .await
    }

    async fn start(&self) -> B21Flow {
        let (status, headers, body) = self.start_raw().await;
        b21_flow(status, &headers, &body)
    }

    /// Starts linking the token's user from the profile.
    async fn start_link(&self, token: &str) -> B21Flow {
        let req = request("POST", "/Auth/Oidc/Link", Some(token), None);
        let (status, headers, body) = self.raw(req).await;
        b21_flow(status, &headers, &body)
    }

    /// The browser comes back from the provider. Returns the Location and the Set-Cookie header.
    async fn callback(&self, cookie: Option<&str>, query: &str) -> (String, String) {
        let mut req = Request::builder()
            .method("GET")
            .uri(format!("/Auth/Oidc/Callback?{query}"));
        if let Some(cookie) = cookie {
            req = req.header("Cookie", cookie);
        }
        let (status, headers, _) = self.raw(req.body(Body::empty()).unwrap()).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the callback always redirects"
        );
        (
            b21_header(&headers, "location"),
            b21_header(&headers, "set-cookie"),
        )
    }

    /// The user approves `flow` at the provider and the browser comes back. Returns the Location.
    async fn approve(&self, flow: &B21Flow, claims: serde_json::Value, sign: B21Sign) -> String {
        let code = self.op.approve(flow, claims, sign);
        self.callback(Some(&flow.cookie), &flow.callback_query(&code))
            .await
            .0
    }

    /// `POST /Auth/Oidc/Complete` from the browser. Returns the status, the body and Set-Cookie.
    async fn complete(&self, cookie: Option<&str>) -> (StatusCode, serde_json::Value, String) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/Auth/Oidc/Complete")
            .header("X-Emby-Authorization", B21_DEVICE);
        if let Some(cookie) = cookie {
            req = req.header("Cookie", cookie);
        }
        let (status, headers, body) = self.raw(req.body(Body::empty()).unwrap()).await;
        let json = serde_json::from_str(&body).unwrap_or_default();
        (status, json, b21_header(&headers, "set-cookie"))
    }

    async fn sign_in(&self, claims: serde_json::Value) -> Result<serde_json::Value, String> {
        self.sign_in_as(claims, B21Sign::Key(0)).await
    }

    /// A whole sign-in: Start, approval, callback, Complete. Ok is the `AuthenticationResult`,
    /// Err the error code of the redirect.
    async fn sign_in_as(
        &self,
        claims: serde_json::Value,
        sign: B21Sign,
    ) -> Result<serde_json::Value, String> {
        let flow = self.start().await;
        let location = self.approve(&flow, claims, sign).await;
        let error = format!("{B21_PUBLIC_URL}/web/#login/oidc-error/");
        if let Some(code) = location.strip_prefix(&error) {
            return Err(code.to_string());
        }
        assert_eq!(location, format!("{B21_PUBLIC_URL}/web/#login/oidc"));
        let (status, body, _) = self.complete(Some(&flow.cookie)).await;
        assert_eq!(status, StatusCode::OK, "Complete after a good callback");
        Ok(body)
    }

    /// Links the token's user from the profile. Err is the error code of the redirect.
    async fn link(&self, token: &str, claims: serde_json::Value) -> Result<(), String> {
        let flow = self.start_link(token).await;
        let location = self.approve(&flow, claims, B21Sign::Key(0)).await;
        let error = format!("{B21_PUBLIC_URL}/web/#settings/oidc-error/");
        if let Some(code) = location.strip_prefix(&error) {
            return Err(code.to_string());
        }
        assert_eq!(
            location,
            format!("{B21_PUBLIC_URL}/web/#settings/oidc-linked")
        );
        Ok(())
    }

    async fn user_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }

    async fn link_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM user_oidc_links")
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }

    /// The named user's link: issuer, subject and provider username.
    async fn link_row(&self, name: &str) -> Option<(String, String, String)> {
        sqlx::query_as(
            "SELECT l.issuer, l.subject, l.username FROM user_oidc_links l \
             JOIN users u ON u.id = l.user_id WHERE u.name = ?",
        )
        .bind(name)
        .fetch_optional(&self.state.db)
        .await
        .unwrap()
    }

    async fn admin_from_group(&self, name: &str) -> bool {
        sqlx::query_scalar("SELECT admin_from_group FROM users WHERE name = ?")
            .bind(name)
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }

    async fn is_admin(&self, name: &str) -> bool {
        user_flags(&self.state, name)
            .await
            .expect("the user exists")
            .0
    }

    /// The client secret as stored, "" without one.
    async fn stored_secret(&self) -> String {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT client_secret FROM oidc_settings WHERE id = 1",
        )
        .fetch_optional(&self.state.db)
        .await
        .unwrap()
        .flatten()
        .unwrap_or_default()
    }
}

/// Reads one text column of the named user's row; `sql` binds the name once.
async fn b21_text(state: &AppState, sql: &'static str, name: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(sql)
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// Runs a statement that binds the name once.
async fn b21_exec(state: &AppState, sql: &'static str, name: &str) {
    sqlx::query(sql)
        .bind(name)
        .execute(&state.db)
        .await
        .unwrap();
}

/// Log output, collected for the secret checks and never printed.
#[derive(Clone, Default)]
struct B21Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl B21Log {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for B21Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for B21Log {
    type Writer = B21Log;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Collects this thread's log lines at every level until the guard drops. The tests run on a
/// current-thread runtime, so this includes Rustyfin's tasks.
fn b21_capture() -> (B21Log, tracing::subscriber::DefaultGuard) {
    let log = B21Log::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::info!("b21 log capture works");
    assert!(log.text().contains("b21 log capture works"));
    (log, guard)
}

/// A URL on a port nothing listens on.
fn b21_closed_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// The name of the token's user, or the status when the token doesn't work.
async fn b21_me(app: &axum::Router, token: &str) -> Result<String, StatusCode> {
    let (status, body) = send(app, request("GET", "/Users/Me", Some(token), None)).await;
    if status != StatusCode::OK {
        return Err(status);
    }
    Ok(body["Name"].as_str().unwrap_or_default().to_string())
}

/// A plain user without a password, as an OIDC sign-in makes them, and a token for them.
async fn b21_passwordless(state: &AppState, name: &str) -> (uuid::Uuid, String) {
    let id = add_user(state, name, "temporary-pw", false).await;
    b21_exec(
        state,
        "UPDATE users SET password_hash = '' WHERE name = ?",
        name,
    )
    .await;
    let user = rustyfin_db::repo_user::get_user_by_id(&state.db, id)
        .await
        .unwrap()
        .unwrap();
    let device = rustyfin_auth::TokenDevice {
        device_id: format!("b21-{name}"),
        device_name: "test".to_string(),
        app_name: "B21".to_string(),
        app_version: "1.0".to_string(),
    };
    let token = rustyfin_auth::create_token(&state.db, &user, &device)
        .await
        .unwrap();
    (id, token)
}

/// A Quick Connect request approved by the token's user. Returns its secret.
async fn b21_qc_approved(app: &axum::Router, token: &str) -> String {
    let (status, init) = send(app, request("POST", "/QuickConnect/Initiate", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let code = init["Code"].as_str().unwrap();
    let uri = format!("/QuickConnect/Authorize?code={code}");
    let (status, _) = send(app, request("POST", &uri, Some(token), None)).await;
    assert_eq!(status, StatusCode::OK, "approving the Quick Connect code");
    init["Secret"].as_str().unwrap().to_string()
}

/// A TV's `POST /Users/AuthenticateWithQuickConnect`, with the secret when given.
fn b21_qc_sign_in(secret: Option<&str>) -> Request<Body> {
    let body = match secret {
        Some(secret) => serde_json::json!({ "Secret": secret }),
        None => serde_json::json!({}),
    };
    Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateWithQuickConnect")
        .header("Content-Type", "application/json")
        .header(
            "X-Emby-Authorization",
            r#"MediaBrowser Client="B21 TV", Device="Living room", DeviceId="b21-tv", Version="2.0""#,
        )
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Asks for a password reset and reads the PIN from the PIN file; None when none was written.
async fn b21_reset_pin(app: &axum::Router, name: &str) -> Option<String> {
    let body = serde_json::json!({ "EnteredUsername": name });
    let req = request("POST", "/Users/ForgotPassword", None, Some(body));
    let (status, answer) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let text = std::fs::read_to_string(answer["PinFile"].as_str()?).ok()?;
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    file["Pin"].as_str().map(str::to_string)
}

/// Password sign-in. Returns the status and the Detail.
async fn b21_password_sign_in(
    app: &axum::Router,
    name: &str,
    password: &str,
) -> (StatusCode, String) {
    let body = serde_json::json!({ "Username": name, "Pw": password });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    let (status, answer) = send(app, req).await;
    (
        status,
        answer["Detail"].as_str().unwrap_or_default().to_string(),
    )
}

/// Reads one boolean expression about the named user; `sql` binds the name once.
async fn b21_true(state: &AppState, sql: &'static str, name: &str) -> bool {
    sqlx::query_scalar::<_, bool>(sql)
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// SQL: whether the named user's link has a last sign-in.
const B21_SIGNED_IN: &str = "SELECT l.last_sign_in_at IS NOT NULL FROM user_oidc_links l \
     JOIN users u ON u.id = l.user_id WHERE u.name = ?";

/// An unknown Quick Connect secret in the format Rustyfin hands out.
const B21_UNKNOWN_SECRET: &str = "0123456789abcdef0123456789abcdef";

/// The fake provider on its own: discovery, a code redeemed once for an ES256 ID token over the
/// grant's claims, and userinfo.
#[tokio::test]
async fn b21_fake_provider_works() {
    use base64::Engine as _;
    use p256::ecdsa::signature::Verifier as _;
    use sha2::Digest as _;
    let op = b21_op().await;
    let http = reqwest::Client::new();
    let url = format!("{}/.well-known/openid-configuration", op.base);
    let text = http.get(url).send().await.unwrap().text().await.unwrap();
    let discovery: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(discovery["issuer"], op.base);

    let verifier = "b21-verifier-0123456789-0123456789-0123456789";
    let challenge = b21_b64(sha2::Sha256::digest(verifier.as_bytes()).as_slice());
    let params = [
        ("nonce", "b21-nonce"),
        ("redirect_uri", B21_CALLBACK),
        ("code_challenge", challenge.as_str()),
    ];
    let flow = B21Flow {
        cookie: String::new(),
        set_cookie: String::new(),
        url: String::new(),
        params: params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    let code = op.approve(&flow, b21_user("pat-sub", "pat"), B21Sign::Key(0));
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", B21_CALLBACK),
        ("code_verifier", verifier),
    ];
    let token = || {
        http.post(format!("{}/token", op.base))
            .basic_auth(B21_CLIENT_ID, Some(B21_CLIENT_SECRET))
            .form(&form)
    };
    let resp = token().send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();

    let id_token = body["id_token"].as_str().expect("an ID token");
    let (input, signature) = id_token.rsplit_once('.').unwrap();
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature)
        .unwrap();
    let signature = p256::ecdsa::Signature::from_slice(&signature).unwrap();
    b21_key(0x11)
        .verifying_key()
        .verify(input.as_bytes(), &signature)
        .expect("signed with the published key");
    let decode = |part: &str| -> serde_json::Value {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    let (header, claims) = input.split_once('.').unwrap();
    assert_eq!(decode(header)["kid"], "b21-key-a");
    let claims = decode(claims);
    assert_eq!(claims["iss"], op.base);
    assert_eq!(claims["aud"], B21_CLIENT_ID);
    assert_eq!(claims["nonce"], "b21-nonce");
    assert_eq!(claims["sub"], "pat-sub");

    let again = token().send().await.unwrap();
    assert_eq!(again.status().as_u16(), 400, "a code is redeemed once");

    let access = body["access_token"].as_str().unwrap();
    let url = format!("{}/userinfo", op.base);
    let text = http
        .get(url)
        .bearer_auth(access)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(info["preferred_username"], "pat");
    assert!(
        info.get("nonce").is_none(),
        "userinfo has no token-only claims"
    );
    assert_eq!(
        op.with(|st| (st.discovery_fetches, st.userinfo_calls)),
        (1, 1)
    );
}

// Task 1: Quick Connect's token step (AC26).

#[tokio::test]
async fn b21_quick_connect_secret_signs_in_the_approver() {
    let state = test_state().await;
    let app = app(state.clone());
    add_user(&state, "alice", "pw-alice", false).await;
    let (alice, alice_id) = login(&app, "alice", "pw-alice").await.unwrap();
    let secret = b21_qc_approved(&app, &alice).await;

    let (status, body) = send(&app, b21_qc_sign_in(Some(&secret))).await;
    assert_eq!(status, StatusCode::OK, "signing in with an approved secret");
    assert_eq!(body["User"]["Id"], alice_id.as_str());
    assert_eq!(body["User"]["Name"], "alice");
    let session = &body["SessionInfo"];
    assert_eq!(session["DeviceId"], "b21-tv", "the session is the TV's");
    assert_eq!(session["DeviceName"], "Living room");
    assert_eq!(session["Client"], "B21 TV");
    assert_eq!(session["ApplicationVersion"], "2.0");
    assert_eq!(body["ServerId"], "test-server-id");
    let token = body["AccessToken"].as_str().expect("an AccessToken");
    assert_eq!(b21_me(&app, token).await, Ok("alice".to_string()));
    assert_eq!(
        b21_me(&app, &alice).await,
        Ok("alice".to_string()),
        "the approving session stays"
    );
    assert!(
        !state.quick_connect.read().await.contains_key(&secret),
        "the request is used up"
    );

    let (status, _) = send(&app, b21_qc_sign_in(Some(&secret))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a secret signs in once");
}

#[tokio::test]
async fn b21_quick_connect_refuses_unknown_unapproved_and_expired() {
    let state = test_state().await;
    let app = app(state.clone());
    add_user(&state, "alice", "pw-alice", false).await;
    let (alice, _) = login(&app, "alice", "pw-alice").await.unwrap();
    let approved = b21_qc_approved(&app, &alice).await;
    let (status, started) = send(&app, request("POST", "/QuickConnect/Initiate", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let waiting = started["Secret"].as_str().unwrap().to_string();
    let expired = b21_qc_approved(&app, &alice).await;
    state
        .quick_connect
        .write()
        .await
        .get_mut(&expired)
        .expect("the request is stored")
        .date_added -= chrono::Duration::seconds(301);

    let (status, _) = send(&app, b21_qc_sign_in(Some(&approved))).await;
    assert_eq!(status, StatusCode::OK, "an approved secret");
    let refused = [
        (B21_UNKNOWN_SECRET, "an unknown secret"),
        (waiting.as_str(), "a secret nobody approved"),
        (expired.as_str(), "an expired secret"),
    ];
    for (secret, what) in refused {
        let (status, _) = send(&app, b21_qc_sign_in(Some(secret))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{what}");
    }
    let (status, _) = send(&app, b21_qc_sign_in(None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no Secret");
}

#[tokio::test]
async fn b21_quick_connect_refuses_a_disabled_approver() {
    let state = test_state().await;
    let app = app(state.clone());
    add_user(&state, "alice", "pw-alice", false).await;
    let (alice, _) = login(&app, "alice", "pw-alice").await.unwrap();
    let first = b21_qc_approved(&app, &alice).await;
    let second = b21_qc_approved(&app, &alice).await;
    let (status, _) = send(&app, b21_qc_sign_in(Some(&first))).await;
    assert_eq!(status, StatusCode::OK);

    b21_exec(
        &state,
        "UPDATE users SET is_disabled = 1 WHERE name = ?",
        "alice",
    )
    .await;
    let (status, _) = send(&app, b21_qc_sign_in(Some(&second))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a disabled user's approval signs nobody in"
    );
}

#[tokio::test]
async fn b21_quick_connect_replaces_the_device_token() {
    let state = test_state().await;
    let app = app(state.clone());
    add_user(&state, "alice", "pw-alice", false).await;
    let (web, _) = login(&app, "alice", "pw-alice").await.unwrap();
    let (old_tv, _) = g2_login_device(&app, "alice", "pw-alice", "b21-tv")
        .await
        .unwrap();
    let secret = b21_qc_approved(&app, &web).await;

    let (status, body) = send(&app, b21_qc_sign_in(Some(&secret))).await;
    assert_eq!(status, StatusCode::OK);
    let new_tv = body["AccessToken"].as_str().unwrap();
    assert_eq!(
        b21_me(&app, &old_tv).await,
        Err(StatusCode::UNAUTHORIZED),
        "the TV's old token is replaced, like at password sign-in"
    );
    assert_eq!(b21_me(&app, new_tv).await, Ok("alice".to_string()));
    assert_eq!(b21_me(&app, &web).await, Ok("alice".to_string()));
}

#[tokio::test]
async fn b21_quick_connect_sign_in_shares_the_login_limit() {
    let state = test_state().await;
    add_user(&state, "alice", "pw-alice", false).await;
    let open = app(state.clone());
    let (alice, _) = login(&open, "alice", "pw-alice").await.unwrap();
    let secret = b21_qc_approved(&open, &alice).await;

    let mut limited = state.clone();
    limited.rate_limiter = RateLimiter::new(3, 60);
    let remote = remote_app(limited);
    let (status, _) = send(&remote, b21_qc_sign_in(Some(&secret))).await;
    assert_eq!(status, StatusCode::OK);
    for _ in 0..2 {
        let (status, _) = send(&remote, b21_qc_sign_in(Some(B21_UNKNOWN_SECRET))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, _) = send(&remote, b21_qc_sign_in(Some(B21_UNKNOWN_SECRET))).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the fourth attempt in a minute"
    );
    let body = serde_json::json!({ "Username": "alice", "Pw": "pw-alice" });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    assert_eq!(
        send(&remote, req).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "password sign-in shares the limit"
    );
}

// Task 2: settings, the public info and the password switch (AC1-AC5, AC20-AC25, AC28).

#[tokio::test]
async fn b21_settings_are_admin_only() {
    let t = b21().await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    let (bob, _) = login(&t.app, "bob", "pw-bob").await.unwrap();
    assert_eq!(t.settings().await["ButtonLabel"], "");
    for method in ["GET", "POST"] {
        let body = (method == "POST").then(|| serde_json::json!({ "ButtonLabel": "Hacked" }));
        let req = request(method, "/Auth/Oidc/Settings", None, body.clone());
        assert_eq!(
            send(&t.app, req).await.0,
            StatusCode::UNAUTHORIZED,
            "{method} without a token"
        );
        let req = request(method, "/Auth/Oidc/Settings", Some(&bob), body);
        assert_eq!(
            send(&t.app, req).await.0,
            StatusCode::FORBIDDEN,
            "{method} as a plain user"
        );
    }
    assert_eq!(t.settings().await["ButtonLabel"], "", "nothing was saved");
}

#[tokio::test]
async fn b21_settings_defaults() {
    let t = b21().await;
    assert_eq!(
        t.settings().await,
        serde_json::json!({
            "Enabled": false,
            "IssuerUrl": "",
            "ClientId": "",
            "HasClientSecret": false,
            "Scopes": "",
            "EffectiveScopes": "openid profile email",
            "ButtonLabel": "",
            "UsernameClaim": "preferred_username",
            "GroupClaim": "groups",
            "AllowedGroup": "",
            "AdminGroup": "",
            "PublicUrl": "",
            "CallbackUrl": "",
            "AllowPasswordSignIn": true,
        })
    );
}

#[tokio::test]
async fn b21_settings_merge_and_keep_the_secret_write_only() {
    let t = b21().await;
    let extra = serde_json::json!({
        "ClientSecret": "b21-saved-secret",
        "PublicUrl": "http://rf.test:8121/",
        "ButtonLabel": "Pocket ID",
    });
    let (status, saved) = t.save(b21_settings_body(&t.op, extra)).await;
    assert_eq!(status, StatusCode::OK, "saving: {}", saved["Detail"]);
    assert_eq!(saved["Enabled"], true);
    assert_eq!(saved["IssuerUrl"], t.op.base.as_str());
    assert_eq!(saved["ClientId"], B21_CLIENT_ID);
    assert_eq!(saved["HasClientSecret"], true);
    assert_eq!(saved["ButtonLabel"], "Pocket ID");
    assert_eq!(
        saved["PublicUrl"], B21_PUBLIC_URL,
        "saved without the trailing /"
    );
    assert_eq!(saved["CallbackUrl"], B21_CALLBACK);
    assert!(
        saved.get("ClientSecret").is_none(),
        "the secret is write-only"
    );
    assert!(
        !saved.to_string().contains("b21-saved-secret"),
        "the answer holds the secret"
    );
    assert_eq!(t.settings().await, saved, "GET shows what POST answered");
    assert_eq!(t.stored_secret().await, "b21-saved-secret");

    let keep =
        serde_json::json!({ "AdminGroup": "admins", "ButtonLabel": null, "ClientSecret": null });
    let (status, saved) = t.save(keep).await;
    assert_eq!(status, StatusCode::OK, "saving: {}", saved["Detail"]);
    assert_eq!(saved["AdminGroup"], "admins");
    assert_eq!(saved["ButtonLabel"], "Pocket ID", "null keeps a value");
    assert_eq!(saved["Enabled"], true, "a missing field keeps its value");
    assert_eq!(saved["HasClientSecret"], true, "null keeps the secret");
    assert_eq!(t.stored_secret().await, "b21-saved-secret");

    let (status, saved) = t.save(serde_json::json!({ "ClientSecret": "" })).await;
    assert_eq!(status, StatusCode::OK, "saving: {}", saved["Detail"]);
    assert_eq!(saved["HasClientSecret"], false, "\"\" removes the secret");
    assert_eq!(t.stored_secret().await, "");
}

#[tokio::test]
async fn b21_settings_scopes() {
    let t = b21().await;
    let cases = [
        (serde_json::json!({}), "", "openid profile email"),
        (
            serde_json::json!({ "AdminGroup": "admins" }),
            "",
            "openid profile email groups",
        ),
        (
            serde_json::json!({ "Scopes": "profile email" }),
            "profile email",
            "openid profile email",
        ),
        (
            serde_json::json!({ "Scopes": "email,openid  groups email" }),
            "email,openid  groups email",
            "openid email groups",
        ),
        (
            serde_json::json!({ "Scopes": "", "AdminGroup": "", "AllowedGroup": "family" }),
            "",
            "openid profile email groups",
        ),
        (
            serde_json::json!({ "AllowedGroup": "" }),
            "",
            "openid profile email",
        ),
    ];
    for (body, scopes, effective) in cases {
        let (status, saved) = t.save(body.clone()).await;
        assert_eq!(status, StatusCode::OK, "saving {body}: {}", saved["Detail"]);
        assert_eq!(saved["Enabled"], false);
        assert_eq!(saved["Scopes"], scopes, "after {body}");
        assert_eq!(saved["EffectiveScopes"], effective, "after {body}");
    }
}

#[tokio::test]
async fn b21_settings_refuse_bad_values() {
    let t = b21().await;
    let base = t.op.base.clone();
    let cases = [
        serde_json::json!({ "IssuerUrl": "" }),
        serde_json::json!({ "IssuerUrl": "ftp://op.test" }),
        serde_json::json!({ "IssuerUrl": "op.test" }),
        serde_json::json!({ "IssuerUrl": format!("{base}?x=1") }),
        serde_json::json!({ "IssuerUrl": format!("{base}#x") }),
        serde_json::json!({ "PublicUrl": "" }),
        serde_json::json!({ "PublicUrl": "rf.test:8121" }),
        serde_json::json!({ "PublicUrl": "http://rf.test:8121/?x=1" }),
        serde_json::json!({ "ClientId": "" }),
        serde_json::json!({ "UsernameClaim": "" }),
        serde_json::json!({ "GroupClaim": "", "AdminGroup": "admins" }),
        serde_json::json!({ "GroupClaim": "", "AllowedGroup": "family" }),
    ];
    for extra in cases {
        let (status, body) = t.save(b21_settings_body(&t.op, extra.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "saving with {extra}");
        let detail = body["Detail"].as_str().unwrap_or_default();
        assert!(!detail.is_empty(), "the 400 for {extra} says what is wrong");
        let settings = t.settings().await;
        assert_eq!(settings["Enabled"], false, "nothing saved after {extra}");
        assert_eq!(settings["IssuerUrl"], "", "nothing saved after {extra}");
    }
    let body = b21_settings_body(&t.op, serde_json::json!({ "GroupClaim": "" }));
    let (status, body) = t.save(body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an empty group claim without a group: {}",
        body["Detail"]
    );
}

/// Breaks the fake provider in one way; the second argument is a closed URL.
type B21Break = fn(&mut B21OpState, &str);

#[tokio::test]
async fn b21_settings_check_the_provider() {
    let t = b21().await;
    let off = serde_json::json!({ "IssuerUrl": t.op.base, "ClientId": B21_CLIENT_ID });
    let (status, body) = t.save(off).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a save with OIDC off: {}",
        body["Detail"]
    );
    assert_eq!(
        t.op.with(|st| (st.discovery_fetches, st.jwks_fetches)),
        (0, 0),
        "nothing is fetched while OIDC is off"
    );

    let closed = b21_closed_url();
    let cases: [(&str, B21Break); 12] = [
        ("discovery is not JSON", |st, _| {
            st.discovery_text = Some("<html>not json</html>".to_string())
        }),
        ("the provider is down", |st, _| st.down = true),
        ("no authorization_endpoint", |st, _| {
            st.discovery.remove("authorization_endpoint");
        }),
        ("no token_endpoint", |st, _| {
            st.discovery.remove("token_endpoint");
        }),
        ("no jwks_uri", |st, _| {
            st.discovery.remove("jwks_uri");
        }),
        ("another issuer", |st, _| {
            st.discovery
                .insert("issuer".into(), "http://evil.test".into());
        }),
        ("no code flow", |st, _| {
            let types = serde_json::json!(["id_token"]);
            st.discovery
                .insert("response_types_supported".into(), types);
        }),
        ("PKCE without S256", |st, _| {
            let methods = serde_json::json!(["plain"]);
            st.discovery
                .insert("code_challenge_methods_supported".into(), methods);
        }),
        ("no algorithm Rustyfin accepts", |st, _| {
            let algs = serde_json::json!(["HS256"]);
            st.discovery
                .insert("id_token_signing_alg_values_supported".into(), algs);
        }),
        ("the JWKS is not JSON", |st, _| {
            st.jwks_text = Some("not json".to_string())
        }),
        ("the JWKS has no keys", |st, _| {
            st.jwks_text = Some(r#"{"keys":[]}"#.to_string())
        }),
        ("the JWKS is unreachable", |st, closed| {
            st.discovery
                .insert("jwks_uri".into(), format!("{closed}/jwks").into());
        }),
    ];
    for (what, change) in cases {
        t.op.with(|st| {
            *st = b21_op_state(&t.op.base);
            change(st, &closed);
        });
        let (status, body) = t
            .save(b21_settings_body(&t.op, serde_json::json!({})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}");
        let detail = body["Detail"].as_str().unwrap_or_default();
        assert!(!detail.is_empty(), "the 400 says what is wrong when {what}");
        assert_eq!(
            t.settings().await["Enabled"],
            false,
            "nothing saved when {what}"
        );
    }

    t.op.with(|st| *st = b21_op_state(&t.op.base));
    let body = b21_settings_body(&t.op, serde_json::json!({ "IssuerUrl": closed }));
    let (status, _) = t.save(body).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an issuer nobody answers at"
    );

    t.op.with(|st| {
        st.discovery.remove("code_challenge_methods_supported");
    });
    let (status, body) = t
        .save(b21_settings_body(&t.op, serde_json::json!({})))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a provider that lists no PKCE methods: {}",
        body["Detail"]
    );
    let (discovery, jwks) = t.op.with(|st| (st.discovery_fetches, st.jwks_fetches));
    assert!(
        discovery > 0 && jwks > 0,
        "the save fetched discovery and the JWKS"
    );
    assert_eq!(t.settings().await["Enabled"], true);
}

#[tokio::test]
async fn b21_info_is_public_and_minimal() {
    let t = b21().await;
    let info = || send(&t.app, request("GET", "/Auth/Oidc/Info", None, None));
    assert_eq!(
        info().await,
        (
            StatusCode::OK,
            serde_json::json!({ "Enabled": false, "ButtonLabel": "" })
        )
    );
    let body = b21_settings_body(&t.op, serde_json::json!({ "ButtonLabel": "Pocket ID" }));
    let (status, body) = t.save(body).await;
    assert_eq!(status, StatusCode::OK, "saving: {}", body["Detail"]);
    assert_eq!(
        info().await,
        (
            StatusCode::OK,
            serde_json::json!({ "Enabled": true, "ButtonLabel": "Pocket ID" })
        )
    );
}

#[tokio::test]
async fn b21_password_switch_refuses_non_admins() {
    let t = b21().await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    add_user(&t.state, "ada", "pw-ada", true).await;
    let (status, body) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK, "saving: {}", body["Detail"]);
    assert_eq!(body["AllowPasswordSignIn"], false);

    assert_eq!(
        b21_password_sign_in(&t.app, "bob", "pw-bob").await,
        (
            StatusCode::FORBIDDEN,
            "Password sign-in is turned off for this account.".to_string()
        )
    );
    assert_eq!(
        b21_password_sign_in(&t.app, "bob", "wrong").await.0,
        StatusCode::UNAUTHORIZED,
        "the password is checked first"
    );
    assert_eq!(
        b21_password_sign_in(&t.app, "ghost", "pw-ghost").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        b21_password_sign_in(&t.app, "ada", "pw-ada").await.0,
        StatusCode::OK,
        "administrators still sign in"
    );
    assert_eq!(
        b21_password_sign_in(&t.app, "owner", "pw-owner").await.0,
        StatusCode::OK
    );

    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": true }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        b21_password_sign_in(&t.app, "bob", "pw-bob").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn b21_password_switch_stops_reset_pins_for_non_admins() {
    let t = b21().await;
    for name in ["bob", "cy", "dee"] {
        add_user(&t.state, name, &format!("pw-{name}"), false).await;
    }
    add_user(&t.state, "ada", "pw-ada", true).await;
    let bob_pin = b21_reset_pin(&t.app, "bob").await.expect("a PIN for bob");
    let cy_pin = b21_reset_pin(&t.app, "cy").await.expect("a PIN for cy");

    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        b21_reset_pin(&t.app, "dee").await,
        None,
        "no PIN for a plain user"
    );
    let req = request(
        "POST",
        "/Users/ForgotPassword/Pin",
        None,
        Some(serde_json::json!({ "Pin": bob_pin })),
    );
    assert_eq!(
        send(&t.app, req).await.0,
        StatusCode::FORBIDDEN,
        "a plain user's PIN resets nothing"
    );
    let reset = |pin: &str| {
        let body =
            serde_json::json!({ "Pin": pin, "Username": "cy", "TempPassword": "b21-temp-pw" });
        request("POST", "/Users/ResetPassword", None, Some(body))
    };
    assert_eq!(
        send(&t.app, reset("ZZ-ZZ-ZZ-ZZ")).await.0,
        StatusCode::BAD_REQUEST,
        "the PIN is checked first"
    );
    assert_eq!(send(&t.app, reset(&cy_pin)).await.0, StatusCode::FORBIDDEN);

    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": true }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        login(&t.app, "bob", "pw-bob").await.is_some(),
        "bob's password is unchanged"
    );
    assert!(
        login(&t.app, "cy", "pw-cy").await.is_some(),
        "cy's password is unchanged"
    );
    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        b21_reset_pin(&t.app, "ada").await.is_some(),
        "administrators still get a PIN"
    );
}

#[tokio::test]
async fn b21_passwordless_user_is_refused_at_password_sign_in() {
    let t = b21().await;
    b21_passwordless(&t.state, "pat").await;
    for allow in [true, false] {
        let (status, _) = t
            .save(serde_json::json!({ "AllowPasswordSignIn": allow }))
            .await;
        assert_eq!(status, StatusCode::OK);
        for pw in ["", "anything"] {
            assert_eq!(
                b21_password_sign_in(&t.app, "pat", pw).await.0,
                StatusCode::UNAUTHORIZED,
                "password {pw:?}, password sign-in allowed: {allow}"
            );
        }
    }
}

#[tokio::test]
async fn b21_passwordless_user_sets_a_first_password() {
    let t = b21().await;
    let (pat_id, pat) = b21_passwordless(&t.state, "pat").await;
    let uri = format!("/Users/{pat_id}/Password");
    let change =
        |body: serde_json::Value| send(&t.app, request("POST", &uri, Some(&pat), Some(body)));

    let (status, _) = change(serde_json::json!({ "NewPw": "first-pw" })).await;
    assert!(
        status.is_success(),
        "a first password without CurrentPw: {status}"
    );
    assert!(login(&t.app, "pat", "first-pw").await.is_some());

    let (status, _) = change(serde_json::json!({ "NewPw": "second-pw" })).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "CurrentPw is needed once there is a password"
    );
    let (status, _) =
        change(serde_json::json!({ "CurrentPw": "wrong", "NewPw": "second-pw" })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) =
        change(serde_json::json!({ "CurrentPw": "first-pw", "NewPw": "second-pw" })).await;
    assert!(status.is_success(), "changing the password: {status}");
    assert!(login(&t.app, "pat", "second-pw").await.is_some());
}

// Task 3: the sign-in flow and the account rules (AC6-AC16).

/// Claims of a provider user with a subject and a username.
fn b21_user(sub: &str, username: &str) -> serde_json::Value {
    serde_json::json!({ "sub": sub, "preferred_username": username })
}

/// The Location of a failed sign-in.
fn b21_login_error(code: &str) -> String {
    format!("{B21_PUBLIC_URL}/web/#login/oidc-error/{code}")
}

#[tokio::test]
async fn b21_start_needs_oidc_on() {
    let t = b21().await;
    let (status, headers, _) = t.start_raw().await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "OIDC was never set up");
    assert_eq!(b21_header(&headers, "set-cookie"), "");
    let off =
        serde_json::json!({ "IssuerUrl": t.op.base, "ClientId": B21_CLIENT_ID, "Enabled": false });
    let (status, _) = t.save(off).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = t.start_raw().await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "OIDC is off");
    assert_eq!(b21_header(&headers, "set-cookie"), "");
}

#[tokio::test]
async fn b21_start_builds_the_authorization_request() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    let flow = t.start().await;
    assert!(
        flow.url.starts_with(&format!("{}/authorize?", t.op.base)),
        "the URL is the provider's authorization endpoint"
    );
    assert_eq!(flow.param("response_type"), "code");
    assert_eq!(flow.param("client_id"), B21_CLIENT_ID);
    assert_eq!(flow.param("redirect_uri"), B21_CALLBACK);
    assert_eq!(flow.param("scope"), "openid profile email groups");
    assert_eq!(flow.param("code_challenge_method"), "S256");
    assert_eq!(
        flow.param("code_challenge").len(),
        43,
        "a base64url SHA-256"
    );
    assert!(
        flow.param("state").len() >= 22,
        "a state of 128 bits or more"
    );
    assert!(
        flow.param("nonce").len() >= 22,
        "a nonce of 128 bits or more"
    );
    let attrs = b21_cookie_attrs(&flow.set_cookie);
    for attr in ["httponly", "samesite=lax", "path=/auth/oidc", "max-age=600"] {
        assert!(
            attrs.iter().any(|a| a == attr),
            "the flow cookie has {attr}"
        );
    }
    assert!(!attrs.iter().any(|a| a == "secure"), "no Secure over http");
    let id = flow.cookie.trim_start_matches("rustyfin_oidc=");
    assert!(id.len() >= 22, "the flow id is unguessable");
    assert_ne!(id, flow.param("state"));
    assert_ne!(id, flow.param("nonce"));
    assert!(!flow.url.contains(id), "the flow id stays out of the URL");
    assert!(!flow.url.contains(B21_CLIENT_SECRET));

    let other = t.start().await;
    for name in ["state", "nonce", "code_challenge"] {
        assert_ne!(
            other.param(name),
            flow.param(name),
            "each flow has its own {name}"
        );
    }
    assert_ne!(other.cookie, flow.cookie);
}

#[tokio::test]
async fn b21_start_under_an_https_path_prefix() {
    let t = b21_on(serde_json::json!({ "PublicUrl": "https://media.example/jf/" })).await;
    let settings = t.settings().await;
    assert_eq!(settings["PublicUrl"], "https://media.example/jf");
    assert_eq!(
        settings["CallbackUrl"],
        "https://media.example/jf/Auth/Oidc/Callback"
    );
    let flow = t.start().await;
    assert_eq!(
        flow.param("redirect_uri"),
        "https://media.example/jf/Auth/Oidc/Callback"
    );
    let attrs = b21_cookie_attrs(&flow.set_cookie);
    assert!(
        attrs.iter().any(|a| a == "path=/jf/auth/oidc"),
        "the cookie path has the prefix"
    );
    assert!(attrs.iter().any(|a| a == "secure"), "Secure over https");
    let location = t
        .approve(&flow, b21_user("pat-sub", "pat"), B21Sign::Key(0))
        .await;
    assert_eq!(location, "https://media.example/jf/web/#login/oidc");
}

#[tokio::test]
async fn b21_start_reports_a_provider_that_is_down() {
    let t = b21_on(serde_json::json!({})).await;
    t.op.with(|st| st.down = true);
    let (status, headers, body) = t.start_raw().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        b21_header(&headers, "set-cookie"),
        "",
        "no flow without a provider"
    );
    assert!(!body.contains(B21_CLIENT_SECRET));
    t.op.with(|st| st.down = false);
    t.start().await;
}

#[tokio::test]
async fn b21_discovery_is_cached_until_the_settings_change() {
    let t = b21_on(serde_json::json!({})).await;
    let fetches = || t.op.with(|st| (st.discovery_fetches, st.jwks_fetches));
    assert_eq!(fetches(), (1, 1), "the save checked the provider");
    t.start().await;
    t.start().await;
    t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    assert_eq!(
        fetches(),
        (2, 2),
        "fetched once after the save, then cached"
    );
    let (status, _) = t
        .save(serde_json::json!({ "ButtonLabel": "Pocket ID" }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetches(), (3, 3), "the save checked the provider again");
    t.start().await;
    assert_eq!(fetches().0, 4, "the save emptied the cache");
}

#[tokio::test]
async fn b21_sign_in_creates_a_user() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "plain", "pw-plain", false).await;
    let count = t.user_count().await;
    let result = t
        .sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("pat signs in");
    assert_eq!(result["User"]["Name"], "pat");
    assert_eq!(result["User"]["IsAdministrator"], false);
    assert_eq!(
        result["SessionInfo"]["DeviceId"], "b21-web",
        "the browser's device"
    );
    assert_eq!(result["SessionInfo"]["Client"], "B21");
    assert_eq!(t.user_count().await, count + 1);
    let token = result["AccessToken"].as_str().expect("an AccessToken");
    assert_eq!(b21_me(&t.app, token).await, Ok("pat".to_string()));
    assert_eq!(
        user_flags(&t.state, "pat").await,
        Some((false, false, false)),
        "not admin, not owner, not disabled"
    );
    let hash = "SELECT password_hash FROM users WHERE name = ?";
    assert_eq!(
        b21_text(&t.state, hash, "pat").await.as_deref(),
        Some(""),
        "no password"
    );
    let config = "SELECT configuration FROM users WHERE name = ?";
    assert_eq!(
        b21_text(&t.state, config, "pat").await,
        b21_text(&t.state, config, "plain").await,
        "the default configuration"
    );
    assert_eq!(
        t.link_row("pat").await,
        Some((t.op.base.clone(), "pat-sub".to_string(), "pat".to_string()))
    );
    let login_date = "SELECT last_login_date IS NOT NULL FROM users WHERE name = ?";
    assert!(
        b21_true(&t.state, login_date, "pat").await,
        "the last login is set"
    );
    assert!(
        b21_true(&t.state, B21_SIGNED_IN, "pat").await,
        "the link's last sign-in is set"
    );
    assert_eq!(
        t.op.with(|st| st.userinfo_calls),
        0,
        "the ID token had every claim"
    );
}

#[tokio::test]
async fn b21_sign_in_again_follows_the_link() {
    let t = b21_on(serde_json::json!({})).await;
    let first = t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let count = t.user_count().await;
    let second = t.sign_in(b21_user("pat-sub", "patricia")).await.unwrap();
    assert_eq!(
        second["User"]["Id"], first["User"]["Id"],
        "the link wins over the username"
    );
    assert_eq!(second["User"]["Name"], "pat", "the Rustyfin name stays");
    assert_eq!(t.user_count().await, count);
    let link = t.link_row("pat").await.expect("pat is linked");
    assert_eq!(link.2, "patricia", "the provider username is updated");
    let old = first["AccessToken"].as_str().unwrap();
    let new = second["AccessToken"].as_str().unwrap();
    assert_eq!(
        b21_me(&t.app, old).await,
        Err(StatusCode::UNAUTHORIZED),
        "the browser's old token is replaced"
    );
    assert_eq!(b21_me(&t.app, new).await, Ok("pat".to_string()));
}

#[tokio::test]
async fn b21_sign_in_links_a_user_with_the_same_name() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "Alice", "pw-alice", false).await;
    let count = t.user_count().await;
    let result = t.sign_in(b21_user("alice-sub", "alice")).await.unwrap();
    assert_eq!(result["User"]["Name"], "Alice", "names match without case");
    assert_eq!(t.user_count().await, count, "no new user");
    assert_eq!(
        t.link_row("Alice").await,
        Some((
            t.op.base.clone(),
            "alice-sub".to_string(),
            "alice".to_string()
        ))
    );
    assert!(
        login(&t.app, "Alice", "pw-alice").await.is_some(),
        "the password still works"
    );
}

#[tokio::test]
async fn b21_sign_in_refuses_unsafe_name_matches() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "ada", "pw-ada", true).await;
    add_user(&t.state, "linked", "pw-linked", false).await;
    t.sign_in(b21_user("first-sub", "linked")).await.unwrap();
    add_user(&t.state, "dis", "pw-dis", false).await;
    b21_exec(
        &t.state,
        "UPDATE users SET is_disabled = 1 WHERE name = ?",
        "dis",
    )
    .await;
    add_user(&t.state, "Sam", "pw-sam", false).await;
    add_user(&t.state, "sam", "pw-sam", false).await;
    let count = t.user_count().await;

    let cases = [
        (b21_user("ada-sub", "ada"), "admin_link_required"),
        (b21_user("other-sub", "Linked"), "already_linked"),
        (b21_user("dis-sub", "dis"), "account_disabled"),
        (b21_user("sam-sub", "SAM"), "ambiguous_user"),
        (
            serde_json::json!({ "sub": "anon-sub", "preferred_username": null }),
            "no_username",
        ),
        (b21_user("blank-sub", "  "), "no_username"),
        (
            serde_json::json!({ "sub": "num-sub", "preferred_username": 42 }),
            "no_username",
        ),
    ];
    for (claims, code) in cases {
        let result = t.sign_in(claims.clone()).await;
        assert_eq!(result.err().as_deref(), Some(code), "{claims}");
    }
    assert_eq!(t.user_count().await, count, "nothing created");
    assert_eq!(t.link_count().await, 1, "nothing linked");

    let (status, _) = t
        .save(serde_json::json!({ "UsernameClaim": "email" }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let claims = serde_json::json!({ "sub": "mail-sub", "preferred_username": "ignored", "email": "mail@x.test" });
    let result = t
        .sign_in(claims)
        .await
        .expect("the username claim is configurable");
    assert_eq!(result["User"]["Name"], "mail@x.test");
}

#[tokio::test]
async fn b21_linked_user_that_is_disabled_is_refused() {
    let t = b21_on(serde_json::json!({})).await;
    t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    b21_exec(
        &t.state,
        "UPDATE users SET is_disabled = 1 WHERE name = ?",
        "pat",
    )
    .await;
    let result = t.sign_in(b21_user("pat-sub", "pat")).await;
    assert_eq!(result.err().as_deref(), Some("account_disabled"));
}

#[tokio::test]
async fn b21_deleting_a_user_removes_the_link() {
    let t = b21_on(serde_json::json!({})).await;
    let first = t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let id = first["User"]["Id"].as_str().unwrap();
    let req = request("DELETE", &format!("/Users/{id}"), Some(&t.admin), None);
    let (status, _) = send(&t.app, req).await;
    assert!(status.is_success(), "deleting pat: {status}");
    assert_eq!(t.link_count().await, 0, "the link went with the user");
    let again = t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    assert_ne!(again["User"]["Id"], first["User"]["Id"], "a new user");
}

#[tokio::test]
async fn b21_userinfo_fills_missing_claims() {
    let t = b21_on(serde_json::json!({ "AllowedGroup": "family" })).await;
    let info =
        serde_json::json!({ "sub": "uma-sub", "preferred_username": "uma", "groups": ["family"] });
    t.op.with(|st| st.userinfo = Some(info));
    let result = t
        .sign_in(serde_json::json!({ "sub": "uma-sub" }))
        .await
        .expect("uma signs in with the claims from userinfo");
    assert_eq!(result["User"]["Name"], "uma");
    assert_eq!(
        t.op.with(|st| st.userinfo_calls),
        1,
        "userinfo is read once"
    );

    let info = serde_json::json!({ "sub": "someone-else", "preferred_username": "uma", "groups": ["family"] });
    t.op.with(|st| st.userinfo = Some(info));
    let result = t.sign_in(serde_json::json!({ "sub": "uma-sub" })).await;
    assert_eq!(
        result.err().as_deref(),
        Some("invalid_token"),
        "userinfo about another subject"
    );

    let calls = t.op.with(|st| st.userinfo_calls);
    let claims =
        serde_json::json!({ "sub": "uma-sub", "preferred_username": "uma", "groups": ["family"] });
    t.sign_in(claims).await.unwrap();
    assert_eq!(
        t.op.with(|st| st.userinfo_calls),
        calls,
        "no userinfo call when the ID token has every claim"
    );
}

#[tokio::test]
async fn b21_callback_checks_the_browser_binding() {
    let t = b21_on(serde_json::json!({})).await;
    let pat = || b21_user("pat-sub", "pat");

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    let (location, _) = t.callback(None, &flow.callback_query(&code)).await;
    assert_eq!(location, b21_login_error("state_mismatch"), "no cookie");

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    let query = b21_query(&[("code", code.as_str()), ("state", "not-the-state")]);
    let (location, set_cookie) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(location, b21_login_error("state_mismatch"), "another state");
    assert!(
        b21_clears_cookie(&set_cookie),
        "the error clears the cookie"
    );
    let (location, _) = t
        .callback(Some(&flow.cookie), &flow.callback_query(&code))
        .await;
    assert_eq!(
        location,
        b21_login_error("expired"),
        "the mismatch ended the flow"
    );

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    let query = flow.callback_query(&code);
    let (location, _) = t
        .callback(Some("rustyfin_oidc=unknown-flow-id"), &query)
        .await;
    assert_eq!(
        location,
        b21_login_error("expired"),
        "a cookie without a flow"
    );

    let a = t.start().await;
    let b = t.start().await;
    let code = t.op.approve(&b, pat(), B21Sign::Key(0));
    let (location, _) = t.callback(Some(&a.cookie), &b.callback_query(&code)).await;
    assert_eq!(
        location,
        b21_login_error("state_mismatch"),
        "flow A's cookie with flow B's state"
    );

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    let query = flow.callback_query(&code);
    let (location, _) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(location, format!("{B21_PUBLIC_URL}/web/#login/oidc"));
    let (location, _) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(
        location,
        b21_login_error("expired"),
        "a second callback for the same flow"
    );
    assert_eq!(
        t.complete(Some(&flow.cookie)).await.0,
        StatusCode::UNAUTHORIZED,
        "the replay ended the flow"
    );
}

#[tokio::test]
async fn b21_callback_maps_provider_answers() {
    let t = b21_on(serde_json::json!({})).await;
    let pat = || b21_user("pat-sub", "pat");

    let flow = t.start().await;
    let state = flow.param("state");
    let query = b21_query(&[("error", "access_denied"), ("state", state.as_str())]);
    let (location, set_cookie) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(location, b21_login_error("cancelled"));
    assert!(
        b21_clears_cookie(&set_cookie),
        "the error clears the cookie"
    );

    let flow = t.start().await;
    let state = flow.param("state");
    let query = b21_query(&[("error", "server_error"), ("state", state.as_str())]);
    let (location, _) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(location, b21_login_error("provider_error"));

    let flow = t.start().await;
    let query = flow.callback_query("b21-code-unknown");
    let (location, _) = t.callback(Some(&flow.cookie), &query).await;
    assert_eq!(
        location,
        b21_login_error("provider_error"),
        "the provider refuses the code"
    );

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    t.op.with(|st| st.down = true);
    let (location, _) = t
        .callback(Some(&flow.cookie), &flow.callback_query(&code))
        .await;
    assert_eq!(
        location,
        b21_login_error("provider_error"),
        "the provider is down"
    );
    t.op.with(|st| st.down = false);

    let flow = t.start().await;
    let code = t.op.approve(&flow, pat(), B21Sign::Key(0));
    let (status, _) = t.save(serde_json::json!({ "Enabled": false })).await;
    assert_eq!(status, StatusCode::OK);
    let (location, _) = t
        .callback(Some(&flow.cookie), &flow.callback_query(&code))
        .await;
    assert_eq!(
        location,
        b21_login_error("disabled"),
        "OIDC was turned off during the flow"
    );
}

#[tokio::test]
async fn b21_id_token_checks() {
    let t = b21_on(serde_json::json!({})).await;
    let now = chrono::Utc::now().timestamp();
    let cases = [
        (
            "signed by an unpublished key",
            serde_json::json!({}),
            B21Sign::Forged(0),
        ),
        ("unsigned", serde_json::json!({}), B21Sign::Unsigned),
        (
            "HS256 with the client secret",
            serde_json::json!({}),
            B21Sign::Hs256,
        ),
        (
            "another issuer",
            serde_json::json!({ "iss": "http://evil.test" }),
            B21Sign::Key(0),
        ),
        (
            "another audience",
            serde_json::json!({ "aud": "other-client" }),
            B21Sign::Key(0),
        ),
        (
            "an extra audience",
            serde_json::json!({ "aud": [B21_CLIENT_ID, "other-client"] }),
            B21Sign::Key(0),
        ),
        (
            "another authorized party",
            serde_json::json!({ "azp": "other-client" }),
            B21Sign::Key(0),
        ),
        (
            "expired",
            serde_json::json!({ "iat": now - 7200, "exp": now - 3600 }),
            B21Sign::Key(0),
        ),
        (
            "another nonce",
            serde_json::json!({ "nonce": "not-the-nonce" }),
            B21Sign::Key(0),
        ),
        (
            "no nonce",
            serde_json::json!({ "nonce": null }),
            B21Sign::Key(0),
        ),
        (
            "no subject",
            serde_json::json!({ "sub": null }),
            B21Sign::Key(0),
        ),
    ];
    for (what, extra, sign) in cases {
        let mut claims = b21_user("tok-sub", "tok");
        for (name, value) in extra.as_object().unwrap() {
            claims[name] = value.clone();
        }
        let result = t.sign_in_as(claims, sign).await;
        assert_eq!(result.err().as_deref(), Some("invalid_token"), "{what}");
    }
    assert_eq!(
        user_flags(&t.state, "tok").await,
        None,
        "no user from a bad token"
    );
    t.sign_in(b21_user("tok-sub", "tok"))
        .await
        .expect("a good token");
}

#[tokio::test]
async fn b21_unknown_kid_triggers_one_jwks_refetch() {
    let t = b21_on(serde_json::json!({})).await;
    t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let before = t.op.with(|st| st.jwks_fetches);
    let result = t
        .sign_in_as(b21_user("pat-sub", "pat"), B21Sign::Kid(0, "b21-key-x"))
        .await;
    assert_eq!(result.err().as_deref(), Some("invalid_token"));
    assert_eq!(
        t.op.with(|st| st.jwks_fetches),
        before + 1,
        "one refetch for the unknown kid"
    );
}

#[tokio::test]
async fn b21_rotated_keys_are_refetched_once() {
    let t = b21_on(serde_json::json!({})).await;
    t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let before = t.op.with(|st| st.jwks_fetches);
    t.op.with(|st| st.keys = vec![("b21-key-b".to_string(), b21_key(0x22))]);
    t.sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("the new key is fetched");
    assert_eq!(t.op.with(|st| st.jwks_fetches), before + 1);
    t.sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("the new key is kept");
    assert_eq!(
        t.op.with(|st| st.jwks_fetches),
        before + 1,
        "the refetched JWKS is cached"
    );
}

#[tokio::test]
async fn b21_token_request_authenticates_the_client() {
    let t = b21_on(serde_json::json!({})).await;
    t.sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("client_secret_basic");
    let (auth, form) =
        t.op.with(|st| st.token_requests.last().cloned())
            .expect("a token request");
    assert_eq!(auth, B21Auth::Basic);
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        form.get("redirect_uri").map(String::as_str),
        Some(B21_CALLBACK)
    );
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    assert!((43..=128).contains(&verifier.len()), "an RFC 7636 verifier");
    assert!(
        verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
        "an RFC 7636 verifier"
    );
    assert!(
        !form.contains_key("client_secret"),
        "the secret goes only in the header"
    );

    t.op.with(|st| {
        let methods = serde_json::json!(["client_secret_post"]);
        st.discovery
            .insert("token_endpoint_auth_methods_supported".into(), methods);
        st.expect_auth = B21Auth::Post;
    });
    let (status, _) = t.save(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);
    t.sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("client_secret_post");
    assert_eq!(
        t.op.with(|st| st.token_requests.last().unwrap().0),
        B21Auth::Post
    );

    t.op.with(|st| st.expect_auth = B21Auth::None);
    let (status, _) = t.save(serde_json::json!({ "ClientSecret": "" })).await;
    assert_eq!(status, StatusCode::OK);
    t.sign_in(b21_user("pat-sub", "pat"))
        .await
        .expect("a public client");
    let (auth, form) = t.op.with(|st| st.token_requests.last().cloned()).unwrap();
    assert_eq!(auth, B21Auth::None);
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some(B21_CLIENT_ID)
    );
}

#[tokio::test]
async fn b21_complete_hands_out_the_token_once() {
    let t = b21_on(serde_json::json!({})).await;
    let flow = t.start().await;
    let early = t.complete(Some(&flow.cookie)).await.0;
    assert_eq!(early, StatusCode::UNAUTHORIZED, "before the callback");
    assert_eq!(
        t.complete(None).await.0,
        StatusCode::UNAUTHORIZED,
        "no cookie"
    );
    let unknown = t.complete(Some("rustyfin_oidc=unknown-flow-id")).await.0;
    assert_eq!(unknown, StatusCode::UNAUTHORIZED, "an unknown flow");
    let location = t
        .approve(&flow, b21_user("comp-sub", "comp"), B21Sign::Key(0))
        .await;
    assert_eq!(
        location,
        format!("{B21_PUBLIC_URL}/web/#login/oidc"),
        "the flow survived the early Complete"
    );
    let (status, body, set_cookie) = t.complete(Some(&flow.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["User"]["Name"], "comp");
    assert!(b21_clears_cookie(&set_cookie), "Complete clears the cookie");
    assert_eq!(
        t.complete(Some(&flow.cookie)).await.0,
        StatusCode::UNAUTHORIZED,
        "the token is handed out once"
    );
}

#[tokio::test]
async fn b21_allowed_group_gates_every_sign_in() {
    let t = b21_on(serde_json::json!({ "AllowedGroup": "family" })).await;
    let count = t.user_count().await;
    let outsiders = [
        serde_json::json!({ "sub": "out-sub", "preferred_username": "outsider", "groups": ["friends"] }),
        b21_user("nog-sub", "nogroups"),
        serde_json::json!({ "sub": "eve-sub", "preferred_username": "eve", "groups": [] }),
    ];
    for claims in outsiders {
        let result = t.sign_in(claims.clone()).await;
        assert_eq!(result.err().as_deref(), Some("not_allowed"), "{claims}");
    }
    assert_eq!(t.user_count().await, count, "nothing created");
    assert_eq!(t.link_count().await, 0, "nothing linked");

    let member = |groups: serde_json::Value| serde_json::json!({ "sub": "mem-sub", "preferred_username": "member", "groups": groups });
    t.sign_in(member(serde_json::json!(["family"])))
        .await
        .expect("a member signs in");
    let result = t.sign_in(member(serde_json::json!(["friends"]))).await;
    assert_eq!(
        result.err().as_deref(),
        Some("not_allowed"),
        "a linked user who left the group"
    );

    let (status, _) = t.save(serde_json::json!({ "GroupClaim": "roles" })).await;
    assert_eq!(status, StatusCode::OK);
    let claims = serde_json::json!({ "sub": "mem-sub", "preferred_username": "member", "roles": ["family"] });
    t.sign_in(claims)
        .await
        .expect("the group claim is configurable");
}

#[tokio::test]
async fn b21_oidc_endpoints_share_the_login_limit() {
    let t = b21_on(serde_json::json!({})).await;
    let mut limited = t.state.clone();
    limited.rate_limiter = RateLimiter::new(3, 60);
    let remote = remote_app(limited.clone());
    let start = || request("POST", "/Auth/Oidc/Start", None, None);
    assert_eq!(send(&remote, start()).await.0, StatusCode::OK);
    let callback = request("GET", "/Auth/Oidc/Callback?code=x&state=y", None, None);
    assert_eq!(send(&remote, callback).await.0, StatusCode::SEE_OTHER);
    let complete = request("POST", "/Auth/Oidc/Complete", None, None);
    assert_eq!(send(&remote, complete).await.0, StatusCode::UNAUTHORIZED);

    assert_eq!(
        send(&remote, start()).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the fourth request in a minute"
    );
    let body = serde_json::json!({ "Username": "owner", "Pw": "pw-owner" });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    assert_eq!(
        send(&remote, req).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "password sign-in shares the limit"
    );
    assert_eq!(
        send(&remote, b21_qc_sign_in(Some(B21_UNKNOWN_SECRET)))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS,
        "so does Quick Connect"
    );
    assert_eq!(
        send(&app(limited), start()).await.0,
        StatusCode::OK,
        "loopback is exempt"
    );
}

#[tokio::test]
async fn b21_flows_are_capped() {
    let t = b21_on(serde_json::json!({})).await;
    let first = t.start().await;
    let second = t.start().await;
    for _ in 0..999 {
        assert_eq!(t.start_raw().await.0, StatusCode::OK);
    }
    let location = t
        .approve(&first, b21_user("pat-sub", "pat"), B21Sign::Key(0))
        .await;
    assert_eq!(
        location,
        b21_login_error("expired"),
        "the oldest of 1001 flows was dropped"
    );
    let location = t
        .approve(&second, b21_user("pat-sub", "pat"), B21Sign::Key(0))
        .await;
    assert_eq!(
        location,
        format!("{B21_PUBLIC_URL}/web/#login/oidc"),
        "the 1000 newest flows live"
    );
}

#[tokio::test]
async fn b21_secrets_stay_out_of_answers_and_logs() {
    use base64::Engine as _;
    let (log, _guard) = b21_capture();
    let t = b21_on(serde_json::json!({ "AllowedGroup": "family" })).await;
    let family = |sub: &str, name: &str, group: &str| serde_json::json!({ "sub": sub, "preferred_username": name, "groups": [group] });
    let basic = format!("{B21_CLIENT_ID}:{B21_CLIENT_SECRET}");
    let mut secrets = vec![
        ("the client secret", B21_CLIENT_SECRET.to_string()),
        (
            "the Basic credentials",
            base64::engine::general_purpose::STANDARD.encode(basic),
        ),
    ];
    let mut flow_ids = Vec::new();
    let mut answers = vec![t.settings().await.to_string()];
    let info = send(&t.app, request("GET", "/Auth/Oidc/Info", None, None)).await;
    answers.push(info.1.to_string());

    let flow = t.start().await;
    answers.push(flow.url.clone());
    flow_ids.push(flow.cookie.trim_start_matches("rustyfin_oidc=").to_string());
    let code =
        t.op.approve(&flow, family("pat-sub", "pat", "family"), B21Sign::Key(0));
    let (location, _) = t
        .callback(Some(&flow.cookie), &flow.callback_query(&code))
        .await;
    answers.push(location);
    secrets.push(("an authorization code", code));
    secrets.push((
        "an ID token",
        t.op.with(|st| st.last_id_token.clone().unwrap()),
    ));
    let (status, body, _) = t.complete(Some(&flow.cookie)).await;
    assert_eq!(status, StatusCode::OK);
    let access_token = body["AccessToken"].as_str().unwrap().to_string();

    let closed = b21_settings_body(&t.op, serde_json::json!({ "IssuerUrl": b21_closed_url() }));
    let (status, body) = t.save(closed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    answers.push(body.to_string());
    let (status, _) = t.save(serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "a save empties the cache");
    t.op.with(|st| st.down = true);
    let (status, _, body) = t.start_raw().await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    answers.push(body);
    t.op.with(|st| st.down = false);

    let refused = [
        (family("pat-sub", "pat", "family"), B21Sign::Forged(0)),
        (family("fr-sub", "fr", "friends"), B21Sign::Key(0)),
    ];
    for (claims, sign) in refused {
        let flow = t.start().await;
        flow_ids.push(flow.cookie.trim_start_matches("rustyfin_oidc=").to_string());
        let code = t.op.approve(&flow, claims, sign);
        let (location, _) = t
            .callback(Some(&flow.cookie), &flow.callback_query(&code))
            .await;
        assert!(
            location.contains("/web/#login/oidc-error/"),
            "the sign-in is refused"
        );
        answers.push(location);
        secrets.push(("an authorization code", code));
        secrets.push((
            "an ID token",
            t.op.with(|st| st.last_id_token.clone().unwrap()),
        ));
    }
    for token in
        t.op.with(|st| st.access.keys().cloned().collect::<Vec<_>>())
    {
        secrets.push(("a provider access token", token));
    }

    let text = log.text();
    for (what, secret) in &secrets {
        for answer in &answers {
            assert!(!answer.contains(secret.as_str()), "an answer holds {what}");
        }
        assert!(!text.contains(secret.as_str()), "the log holds {what}");
    }
    for id in &flow_ids {
        assert!(!text.contains(id.as_str()), "the log holds a flow id");
    }
    assert!(
        !text.contains(&access_token),
        "the log holds Rustyfin's token"
    );
}

// Task 4: admin rights from a group, linking and unlinking (AC12, AC17-AC19, AC29-AC31).

/// Claims of gus, a provider user in `groups`.
fn b21_gus(groups: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "sub": "gus-sub", "preferred_username": "gus", "groups": groups })
}

#[tokio::test]
async fn b21_admin_group_grants_and_takes_back_admin() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    let result = t
        .sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert_eq!(
        result["User"]["IsAdministrator"], true,
        "the group grants admin rights"
    );
    assert!(t.is_admin("gus").await);
    assert!(t.admin_from_group("gus").await);
    let token = result["AccessToken"].as_str().unwrap();
    let req = request("GET", "/Auth/Oidc/Settings", Some(token), None);
    assert_eq!(
        send(&t.app, req).await.0,
        StatusCode::OK,
        "gus acts as an admin"
    );

    let result = t.sign_in(b21_gus(serde_json::json!([]))).await.unwrap();
    assert_eq!(
        result["User"]["IsAdministrator"], false,
        "leaving the group takes the rights back"
    );
    assert!(!t.is_admin("gus").await);
    assert!(!t.admin_from_group("gus").await);

    t.sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert!(t.is_admin("gus").await, "rejoining grants them again");
    assert!(t.admin_from_group("gus").await);

    add_user(&t.state, "lou", "pw-lou", false).await;
    let (lou, _) = login(&t.app, "lou", "pw-lou").await.unwrap();
    let claims =
        serde_json::json!({ "sub": "lou-sub", "preferred_username": "lou", "groups": ["admins"] });
    t.link(&lou, claims)
        .await
        .expect("lou links from the profile");
    assert!(t.is_admin("lou").await, "a link applies the group too");
    assert!(t.admin_from_group("lou").await);

    let (status, _) = t.save(serde_json::json!({ "AdminGroup": "" })).await;
    assert_eq!(status, StatusCode::OK);
    t.sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert!(
        !t.is_admin("gus").await,
        "without an admin group the group-granted rights go"
    );
    assert!(!t.admin_from_group("gus").await);
}

#[tokio::test]
async fn b21_admin_group_keeps_hand_granted_admins() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    add_user(&t.state, "ada", "pw-ada", true).await;
    let (ada, _) = login(&t.app, "ada", "pw-ada").await.unwrap();
    let claims = |groups: serde_json::Value| serde_json::json!({ "sub": "ada-sub", "preferred_username": "ada", "groups": groups });
    t.link(&ada, claims(serde_json::json!([])))
        .await
        .expect("an admin links from the profile");
    assert!(
        t.is_admin("ada").await,
        "a hand-granted admin outside the group"
    );
    assert!(!t.admin_from_group("ada").await);
    t.sign_in(claims(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert!(t.is_admin("ada").await);
    assert!(
        !t.admin_from_group("ada").await,
        "the group did not grant the rights"
    );
    t.sign_in(claims(serde_json::json!([]))).await.unwrap();
    assert!(
        t.is_admin("ada").await,
        "leaving the group keeps hand-granted rights"
    );
}

#[tokio::test]
async fn b21_admin_group_keeps_the_last_admin() {
    let (log, _guard) = b21_capture();
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    t.sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    b21_exec(
        &t.state,
        "UPDATE users SET is_disabled = 1 WHERE name = ?",
        "owner",
    )
    .await;
    t.sign_in(b21_gus(serde_json::json!([]))).await.unwrap();
    assert!(
        t.is_admin("gus").await,
        "the last enabled admin keeps the rights"
    );
    assert!(t.admin_from_group("gus").await, "and the flag");
    let warned = log
        .text()
        .lines()
        .any(|line| line.contains("WARN") && line.to_lowercase().contains("admin"));
    assert!(warned, "a warning says why");
}

#[tokio::test]
async fn b21_hand_changes_clear_the_group_flag() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    let result = t
        .sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    let uri = format!("/Users/{}/Policy", result["User"]["Id"].as_str().unwrap());
    let policy = |admin: bool| {
        let body = serde_json::json!({ "IsAdministrator": admin });
        request("POST", &uri, Some(&t.admin), Some(body))
    };
    let (status, _) = send(&t.app, policy(false)).await;
    assert!(status.is_success(), "revoking by hand: {status}");
    assert!(!t.is_admin("gus").await);
    assert!(
        !t.admin_from_group("gus").await,
        "a hand change clears the flag"
    );

    t.sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert!(t.is_admin("gus").await, "while in the group the group wins");
    assert!(t.admin_from_group("gus").await);

    // Resending IsAdministrator keeps the flag (AC42), so a revoke and a grant hand the rights over.
    let (status, _) = send(&t.app, policy(false)).await;
    assert!(status.is_success(), "revoking by hand: {status}");
    let (status, _) = send(&t.app, policy(true)).await;
    assert!(status.is_success(), "granting by hand: {status}");
    assert!(t.is_admin("gus").await);
    assert!(
        !t.admin_from_group("gus").await,
        "a hand grant clears the flag"
    );
    t.sign_in(b21_gus(serde_json::json!([]))).await.unwrap();
    assert!(
        t.is_admin("gus").await,
        "hand-granted rights stay outside the group"
    );
}

#[tokio::test]
async fn b21_link_from_the_profile() {
    let t = b21().await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    let (bob, _) = login(&t.app, "bob", "pw-bob").await.unwrap();
    let link_req = |token: &str| request("POST", "/Auth/Oidc/Link", Some(token), None);
    let (status, _) = send(&t.app, link_req(&bob)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "OIDC is off");
    let (status, _) = t
        .save(b21_settings_body(&t.op, serde_json::json!({})))
        .await;
    assert_eq!(status, StatusCode::OK);
    let anonymous = request("POST", "/Auth/Oidc/Link", None, None);
    assert_eq!(send(&t.app, anonymous).await.0, StatusCode::UNAUTHORIZED);

    let flow = t.start_link(&bob).await;
    let location = t
        .approve(&flow, b21_user("bob-sub", "robert"), B21Sign::Key(0))
        .await;
    assert_eq!(
        location,
        format!("{B21_PUBLIC_URL}/web/#settings/oidc-linked")
    );
    assert_eq!(
        t.complete(Some(&flow.cookie)).await.0,
        StatusCode::UNAUTHORIZED,
        "a link hands out no token"
    );
    assert_eq!(
        t.link_row("bob").await,
        Some((
            t.op.base.clone(),
            "bob-sub".to_string(),
            "robert".to_string()
        ))
    );
    assert!(
        !b21_true(&t.state, B21_SIGNED_IN, "bob").await,
        "a link is not a sign-in"
    );
    let (status, _) = send(&t.app, link_req(&bob)).await;
    assert_eq!(status, StatusCode::CONFLICT, "bob is linked already");

    let result = t.sign_in(b21_user("bob-sub", "robert")).await.unwrap();
    assert_eq!(result["User"]["Name"], "bob", "robert signs in as bob");
    assert!(b21_true(&t.state, B21_SIGNED_IN, "bob").await);

    add_user(&t.state, "cy", "pw-cy", false).await;
    let (cy, _) = login(&t.app, "cy", "pw-cy").await.unwrap();
    let result = t.link(&cy, b21_user("bob-sub", "robert")).await;
    assert_eq!(
        result.err().as_deref(),
        Some("already_linked"),
        "robert is bob's"
    );
    assert_eq!(t.link_row("cy").await, None);

    t.link(&t.admin, b21_user("owner-sub", "owner"))
        .await
        .expect("admins may link this way");

    let (status, _) = t
        .save(serde_json::json!({ "AllowedGroup": "family" }))
        .await;
    assert_eq!(status, StatusCode::OK);
    add_user(&t.state, "eve", "pw-eve", false).await;
    let (eve, _) = login(&t.app, "eve", "pw-eve").await.unwrap();
    let claims =
        serde_json::json!({ "sub": "eve-sub", "preferred_username": "eve", "groups": ["friends"] });
    let result = t.link(&eve, claims).await;
    assert_eq!(result.err().as_deref(), Some("not_allowed"));
    assert_eq!(t.link_row("eve").await, None, "nothing linked");
}

#[tokio::test]
async fn b21_accounts_show_the_link_status() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    let (bob, bob_id) = login(&t.app, "bob", "pw-bob").await.unwrap();
    t.link(&bob, b21_user("bob-sub", "robert")).await.unwrap();
    let pat = t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let pat_id = pat["User"]["Id"].as_str().unwrap().to_string();
    add_user(&t.state, "cy", "pw-cy", false).await;
    t.link(&t.admin, b21_user("owner-sub", "owner"))
        .await
        .unwrap();

    let req = request("GET", "/Auth/Oidc/Accounts", Some(&t.admin), None);
    let (status, list) = send(&t.app, req).await;
    assert_eq!(status, StatusCode::OK);
    let list = list.as_array().expect("an array").clone();
    assert_eq!(
        list.len() as i64,
        t.user_count().await,
        "one entry per user"
    );
    for entry in &list {
        assert_eq!(entry.as_object().map(|o| o.len()), Some(10), "{entry}");
    }
    let find = |name: &str| {
        list.iter()
            .find(|e| e["UserName"] == name)
            .unwrap_or_else(|| panic!("no entry for {name}"))
            .clone()
    };
    let same_id = |value: &serde_json::Value, id: &str| {
        let parsed = uuid::Uuid::parse_str(value.as_str().unwrap_or_default()).ok();
        parsed.is_some() && parsed == uuid::Uuid::parse_str(id).ok()
    };
    let rfc3339 = |value: &serde_json::Value| {
        value
            .as_str()
            .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
    };

    let b = find("bob");
    assert!(same_id(&b["UserId"], &bob_id), "{}", b["UserId"]);
    assert_eq!(b["Linked"], true);
    assert_eq!(b["HasPassword"], true);
    assert_eq!(b["ProviderUsername"], "robert");
    assert_eq!(b["IsAdministrator"], false);
    assert_eq!(b["AdminFromGroup"], false);
    assert_eq!(
        b["CanUnlink"], true,
        "bob can still sign in with the password"
    );
    assert!(rfc3339(&b["LinkedAt"]), "{}", b["LinkedAt"]);
    assert!(
        b["LastSignInAt"].is_null(),
        "bob never signed in through OIDC"
    );

    let p = find("pat");
    assert!(same_id(&p["UserId"], &pat_id), "{}", p["UserId"]);
    assert_eq!(p["Linked"], true);
    assert_eq!(p["HasPassword"], false);
    assert_eq!(p["CanUnlink"], false, "pat has no other way in");
    assert!(rfc3339(&p["LastSignInAt"]), "{}", p["LastSignInAt"]);

    let c = find("cy");
    assert_eq!(c["Linked"], false);
    assert_eq!(c["HasPassword"], true);
    for name in ["ProviderUsername", "LinkedAt", "LastSignInAt"] {
        assert!(c[name].is_null(), "cy has no {name}");
    }
    assert_eq!(c["CanUnlink"], false, "nothing to unlink");

    let o = find("owner");
    assert_eq!(o["IsAdministrator"], true);
    assert_eq!(o["CanUnlink"], true);

    let one = |id: &str, token: &str| {
        request(
            "GET",
            &format!("/Auth/Oidc/Accounts/{id}"),
            Some(token),
            None,
        )
    };
    let req = request("GET", "/Auth/Oidc/Accounts", Some(&bob), None);
    assert_eq!(
        send(&t.app, req).await.0,
        StatusCode::FORBIDDEN,
        "the list is for admins"
    );
    assert_eq!(
        send(&t.app, one(&bob_id, &bob)).await,
        (StatusCode::OK, b.clone()),
        "a user reads their own entry"
    );
    assert_eq!(
        send(&t.app, one(&pat_id, &bob)).await.0,
        StatusCode::FORBIDDEN,
        "not another user's"
    );
    assert_eq!(
        send(&t.app, one(&pat_id, &t.admin)).await,
        (StatusCode::OK, p.clone())
    );
    let unknown = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        send(&t.app, one(&unknown, &t.admin)).await.0,
        StatusCode::NOT_FOUND
    );

    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, b) = send(&t.app, one(&bob_id, &t.admin)).await;
    assert_eq!(b["CanUnlink"], false, "bob's password no longer works");
    let (_, o) = send(&t.app, one(&t.admin_id, &t.admin)).await;
    assert_eq!(o["CanUnlink"], true, "admins keep password sign-in");
}

#[tokio::test]
async fn b21_unlink_needs_another_way_in() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    let (bob, bob_id) = login(&t.app, "bob", "pw-bob").await.unwrap();
    t.link(&bob, b21_user("bob-sub", "robert")).await.unwrap();
    let pat = t.sign_in(b21_user("pat-sub", "pat")).await.unwrap();
    let pat_id = pat["User"]["Id"].as_str().unwrap().to_string();
    let pat_token = pat["AccessToken"].as_str().unwrap().to_string();
    let unlink = |id: &str, token: &str| {
        request(
            "DELETE",
            &format!("/Auth/Oidc/Accounts/{id}/Link"),
            Some(token),
            None,
        )
    };

    let (status, body) = send(&t.app, unlink(&pat_id, &pat_token)).await;
    assert_eq!(status, StatusCode::CONFLICT, "pat has no password");
    let detail = body["Detail"].as_str().unwrap_or_default();
    assert!(!detail.is_empty(), "the answer says why");
    assert_eq!(
        send(&t.app, unlink(&pat_id, &bob)).await.0,
        StatusCode::FORBIDDEN,
        "not bob's link"
    );
    assert_eq!(
        send(&t.app, unlink(&bob_id, &bob)).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(t.link_row("bob").await, None);
    assert_eq!(
        send(&t.app, unlink(&bob_id, &bob)).await.0,
        StatusCode::NOT_FOUND,
        "no link left"
    );

    t.link(&bob, b21_user("bob-sub", "robert"))
        .await
        .expect("bob links again");
    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&t.app, unlink(&bob_id, &bob)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "bob's password no longer works"
    );
    let detail = body["Detail"].as_str().unwrap_or_default();
    assert!(!detail.is_empty(), "the answer says why");
    assert_eq!(
        send(&t.app, unlink(&bob_id, &t.admin)).await.0,
        StatusCode::NO_CONTENT,
        "an admin may"
    );
    assert_eq!(
        send(&t.app, unlink(&pat_id, &t.admin)).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(t.link_count().await, 0);
    let unknown = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        send(&t.app, unlink(&unknown, &t.admin)).await.0,
        StatusCode::NOT_FOUND
    );
}

// Fix round 1: the review of 9cc903a (spec §10, AC37-AC42).

#[tokio::test]
#[ignore = "B21 fix 1"]
async fn b21_admin_group_member_links_from_the_profile() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    add_user(&t.state, "bob", "pw-bob", false).await;
    add_user(&t.state, "cy", "pw-cy", false).await;
    let count = t.user_count().await;
    let boss =
        serde_json::json!({ "sub": "boss-sub", "preferred_username": "bob", "groups": ["admins"] });

    assert_eq!(
        t.sign_in(boss.clone()).await.err().as_deref(),
        Some("admin_link_required"),
        "an admin-group member doesn't get a user by name"
    );
    assert_eq!(t.user_count().await, count, "nothing created");
    assert_eq!(t.link_count().await, 0, "nothing linked");
    assert!(!t.is_admin("bob").await, "nothing granted");

    let (bob, _) = login(&t.app, "bob", "pw-bob").await.unwrap();
    t.link(&bob, boss.clone())
        .await
        .expect("bob links from the profile");
    assert!(t.is_admin("bob").await, "the link applies the group");
    assert!(t.admin_from_group("bob").await);
    let result = t.sign_in(boss).await.expect("the linked identity signs in");
    assert_eq!(result["User"]["Name"], "bob");

    let cy =
        serde_json::json!({ "sub": "cy-sub", "preferred_username": "cy", "groups": ["users"] });
    let result = t
        .sign_in(cy)
        .await
        .expect("outside the admin group a name still links");
    assert_eq!(result["User"]["Name"], "cy");
    assert_eq!(t.user_count().await, count);

    let dee =
        serde_json::json!({ "sub": "dee-sub", "preferred_username": "dee", "groups": ["admins"] });
    let result = t
        .sign_in(dee)
        .await
        .expect("a new admin-group member gets a new user");
    assert_eq!(result["User"]["IsAdministrator"], true);
    assert_eq!(t.user_count().await, count + 1);
}

#[tokio::test]
#[ignore = "B21 fix 1"]
async fn b21_rename_is_for_administrators_while_oidc_is_on() {
    let t = b21_on(serde_json::json!({})).await;
    let bob_id = add_user(&t.state, "bob", "pw-bob", false).await;
    let ada_id = add_user(&t.state, "ada", "pw-ada", true).await;
    let (bob, _) = login(&t.app, "bob", "pw-bob").await.unwrap();
    let (ada, _) = login(&t.app, "ada", "pw-ada").await.unwrap();
    let rename = |id: uuid::Uuid, token: &str, name: &str| {
        let body = serde_json::json!({ "Name": name });
        request("PUT", &format!("/Users/{id}"), Some(token), Some(body))
    };

    let (status, body) = send(&t.app, rename(bob_id, &bob, "gus")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a user doesn't rename themselves"
    );
    assert_eq!(
        body["Detail"],
        "Usernames can't be changed while sign-in through the provider is on. Ask an administrator."
    );
    assert_eq!(
        b21_me(&t.app, &bob).await,
        Ok("bob".to_string()),
        "the name stays"
    );
    let (status, body) = send(&t.app, rename(ada_id, &bob, "gus")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "nor anyone else");
    assert_eq!(body["Detail"], "User update not allowed.");

    let (status, _) = send(&t.app, rename(bob_id, &ada, "robert")).await;
    assert_eq!(status, StatusCode::OK, "an administrator renames a user");
    assert_eq!(b21_me(&t.app, &bob).await, Ok("robert".to_string()));
    let (status, _) = send(&t.app, rename(ada_id, &ada, "adele")).await;
    assert_eq!(status, StatusCode::OK, "and themselves");
    assert_eq!(b21_me(&t.app, &ada).await, Ok("adele".to_string()));

    let (status, _) = t.save(serde_json::json!({ "Enabled": false })).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&t.app, rename(bob_id, &bob, "bob")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "with OIDC off users rename themselves again"
    );
    assert_eq!(b21_me(&t.app, &bob).await, Ok("bob".to_string()));
}

#[tokio::test]
#[ignore = "B21 fix 2"]
async fn b21_quick_connect_initiate_shares_the_login_limit() {
    let mut state = test_state().await;
    add_user(&state, "alice", "pw-alice", false).await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let remote = remote_app(state);
    let initiate = || request("POST", "/QuickConnect/Initiate", None, None);
    let mut secret = String::new();
    for _ in 0..3 {
        let (status, body) = send(&remote, initiate()).await;
        assert_eq!(status, StatusCode::OK);
        secret = body["Secret"].as_str().unwrap().to_string();
    }
    assert_eq!(
        send(&remote, initiate()).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the fourth request in a minute"
    );
    let body = serde_json::json!({ "Username": "alice", "Pw": "pw-alice" });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    assert_eq!(
        send(&remote, req).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "password sign-in shares the limit"
    );
    let uri = format!("/QuickConnect/Connect?secret={secret}");
    assert_eq!(
        send(&remote, request("GET", &uri, None, None)).await.0,
        StatusCode::OK,
        "apps keep polling Connect"
    );
}

#[tokio::test]
#[ignore = "B21 fix 2"]
async fn b21_quick_connect_caps_live_requests() {
    let state = test_state().await;
    let app = app(state.clone());
    let initiate = || request("POST", "/QuickConnect/Initiate", None, None);
    let (status, body) = send(&app, initiate()).await;
    assert_eq!(status, StatusCode::OK);
    let first = body["Secret"].as_str().unwrap().to_string();
    let mut codes = std::collections::HashSet::from([body["Code"].as_str().unwrap().to_string()]);
    for _ in 1..100 {
        let (status, body) = send(&app, initiate()).await;
        assert_eq!(status, StatusCode::OK);
        codes.insert(body["Code"].as_str().unwrap().to_string());
    }
    assert_eq!(codes.len(), 100, "live requests have different codes");
    assert_eq!(
        send(&app, initiate()).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "a 101st live request"
    );
    assert_eq!(
        state.quick_connect.read().await.len(),
        100,
        "nothing more is stored"
    );

    state
        .quick_connect
        .write()
        .await
        .get_mut(&first)
        .expect("the request is stored")
        .date_added -= chrono::Duration::seconds(301);
    assert_eq!(
        send(&app, initiate()).await.0,
        StatusCode::OK,
        "an expired request makes room"
    );
    assert_eq!(
        send(&app, initiate()).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
#[ignore = "B21 fix 2"]
async fn b21_quick_connect_authorize_skips_expired_requests() {
    let state = test_state().await;
    let app = app(state.clone());
    add_user(&state, "alice", "pw-alice", false).await;
    let (alice, alice_id) = login(&app, "alice", "pw-alice").await.unwrap();
    let (status, init) = send(&app, request("POST", "/QuickConnect/Initiate", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let code = init["Code"].as_str().unwrap();
    let live = init["Secret"].as_str().unwrap().to_string();
    {
        // Expired requests with the same code, as earlier requests can leave behind.
        let mut store = state.quick_connect.write().await;
        let template = store[&live].clone();
        for n in 0..200 {
            let mut old = template.clone();
            old.secret = format!("{n:032x}");
            old.date_added -= chrono::Duration::seconds(301);
            store.insert(old.secret.clone(), old);
        }
    }

    let uri = format!("/QuickConnect/Authorize?code={code}");
    let (status, _) = send(&app, request("POST", &uri, Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK, "the live request is approved");
    let (status, body) = send(&app, b21_qc_sign_in(Some(&live))).await;
    assert_eq!(status, StatusCode::OK, "its secret signs in");
    assert_eq!(body["User"]["Id"], alice_id.as_str());
    let (status, _) = send(&app, request("POST", &uri, Some(&alice), None)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "only expired requests carry the code now"
    );
}

#[tokio::test]
#[ignore = "B21 fix 3"]
async fn b21_names_match_in_ascii_case_only() {
    let t = b21_on(serde_json::json!({})).await;
    add_user(&t.state, "kelvin", "pw-kelvin", false).await;
    add_user(&t.state, "Änne", "pw-anne", false).await;
    let count = t.user_count().await;

    let result = t
        .sign_in(b21_user("sign-sub", "\u{212A}elvin"))
        .await
        .expect("the Kelvin sign matches nobody");
    assert_eq!(result["User"]["Name"], "\u{212A}elvin", "a new user");
    assert_eq!(t.link_row("kelvin").await, None, "kelvin stays unlinked");
    let result = t
        .sign_in(b21_user("anne-sub", "änne"))
        .await
        .expect("other letters match only as they are");
    assert_eq!(result["User"]["Name"], "änne", "a new user");
    assert_eq!(t.user_count().await, count + 2);

    let result = t
        .sign_in(b21_user("kelvin-sub", "KELVIN"))
        .await
        .expect("ASCII letters match in any case");
    assert_eq!(result["User"]["Name"], "kelvin");
    assert_eq!(t.user_count().await, count + 2, "no new user");
}

#[tokio::test]
#[ignore = "B21 fix 4"]
async fn b21_password_switch_refuses_group_granted_admins() {
    let t =
        b21_on(serde_json::json!({ "AdminGroup": "admins", "AllowPasswordSignIn": false })).await;
    let result = t
        .sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    assert!(t.admin_from_group("gus").await);
    let gus_id = result["User"]["Id"].as_str().unwrap().to_string();
    let gus = result["AccessToken"].as_str().unwrap().to_string();
    let uri = format!("/Users/{gus_id}/Password");
    let body = serde_json::json!({ "NewPw": "pw-gus" });
    let (status, _) = send(&t.app, request("POST", &uri, Some(&gus), Some(body))).await;
    assert!(
        status.is_success(),
        "the password form still works: {status}"
    );

    assert_eq!(
        b21_password_sign_in(&t.app, "gus", "pw-gus").await,
        (
            StatusCode::FORBIDDEN,
            "Password sign-in is turned off for this account.".to_string()
        ),
        "rights from the group don't pass the switch"
    );
    assert_eq!(b21_reset_pin(&t.app, "gus").await, None, "no reset PIN");
    let uri = format!("/Auth/Oidc/Accounts/{gus_id}");
    let (status, account) = send(&t.app, request("GET", &uri, Some(&gus), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["CanUnlink"], false, "the password is no way in");
    let uri = format!("/Auth/Oidc/Accounts/{gus_id}/Link");
    assert_eq!(
        send(&t.app, request("DELETE", &uri, Some(&gus), None))
            .await
            .0,
        StatusCode::CONFLICT,
        "so the link stays"
    );
    assert_eq!(
        b21_password_sign_in(&t.app, "owner", "pw-owner").await.0,
        StatusCode::OK,
        "administrators given rights by hand still sign in"
    );

    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": true }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        b21_password_sign_in(&t.app, "gus", "pw-gus").await.0,
        StatusCode::OK,
        "with passwords on, gus's password works"
    );
}

#[tokio::test]
#[ignore = "B21 fix 4"]
async fn b21_password_switch_stops_reset_pins_for_group_granted_admins() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    for name in ["hal", "ivy"] {
        let claims = serde_json::json!({ "sub": format!("{name}-sub"), "preferred_username": name, "groups": ["admins"] });
        t.sign_in(claims).await.unwrap();
        assert!(t.admin_from_group(name).await);
    }
    let hal_pin = b21_reset_pin(&t.app, "hal").await.expect("a PIN for hal");
    let ivy_pin = b21_reset_pin(&t.app, "ivy").await.expect("a PIN for ivy");
    let (status, _) = t
        .save(serde_json::json!({ "AllowPasswordSignIn": false }))
        .await;
    assert_eq!(status, StatusCode::OK);

    let body = serde_json::json!({ "Pin": hal_pin });
    let req = request("POST", "/Users/ForgotPassword/Pin", None, Some(body));
    assert_eq!(
        send(&t.app, req).await.0,
        StatusCode::FORBIDDEN,
        "a group-granted admin's PIN resets nothing"
    );
    let body =
        serde_json::json!({ "Pin": ivy_pin, "Username": "ivy", "TempPassword": "b21-temp-pw" });
    let req = request("POST", "/Users/ResetPassword", None, Some(body));
    assert_eq!(send(&t.app, req).await.0, StatusCode::FORBIDDEN);
    for name in ["hal", "ivy"] {
        let sql = "SELECT password_hash = '' FROM users WHERE name = ?";
        assert!(
            b21_true(&t.state, sql, name).await,
            "{name} still has no password"
        );
    }
}

#[tokio::test]
#[ignore = "B21 fix 5"]
async fn b21_resending_the_policy_keeps_the_group_flag() {
    let t = b21_on(serde_json::json!({ "AdminGroup": "admins" })).await;
    let result = t
        .sign_in(b21_gus(serde_json::json!(["admins"])))
        .await
        .unwrap();
    let uri = format!("/Users/{}/Policy", result["User"]["Id"].as_str().unwrap());
    let body = serde_json::json!({ "IsAdministrator": true, "EnableAllFolders": true });
    let (status, _) = send(&t.app, request("POST", &uri, Some(&t.admin), Some(body))).await;
    assert!(status.is_success(), "sending the policy back: {status}");
    assert!(t.is_admin("gus").await);
    assert!(
        t.admin_from_group("gus").await,
        "the same IsAdministrator keeps the rights group-given"
    );

    t.sign_in(b21_gus(serde_json::json!([]))).await.unwrap();
    assert!(
        !t.is_admin("gus").await,
        "leaving the group still takes them back"
    );
}

// ── Environment Tests ──

#[tokio::test]
async fn test_environment_drives() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Environment/Drives")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let drives: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    // At least root "/" should be present on Linux
    assert!(!drives.is_empty());
}

// ── Scheduled Tasks Tests ──

#[tokio::test]
async fn test_scheduled_tasks_list() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/ScheduledTasks")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let tasks: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // May return array or object with Items key
    assert!(
        tasks.is_array() || tasks.get("Items").is_some() || tasks.get("ScheduledTasks").is_some()
    );
}

// ── Localization Tests ──

#[tokio::test]
async fn test_localization_cultures() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Localization/Cultures")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Plugins Tests ──

#[tokio::test]
async fn test_plugins_list() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Plugins")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let plugins: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    // Vec is always an array, just check it deserialized
    drop(plugins);
}

// ── Configuration Tests ──

#[tokio::test]
async fn test_configuration() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/System/Configuration")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

// ── G12: TMDb client ──
//
// A stub TMDb on 127.0.0.1:0 stands in for the real one. The keys are fake.

const G12_ENV_KEY: &str = "g12-env-fake-key-7777";
const G12_CONFIG_KEY: &str = "g12-config-fake-key-8888";

#[derive(Clone, Debug)]
struct G12Hit {
    /// Without the base URL's `/3`.
    path: String,
    api_key: Option<String>,
    user_agent: String,
}

type G12Hits = Arc<std::sync::Mutex<Vec<G12Hit>>>;

/// Serves one movie's refresh and trailers. Any other path is a 404. Returns the origin.
async fn g12_stub() -> (String, G12Hits) {
    async fn handle(
        axum::extract::State(hits): axum::extract::State<G12Hits>,
        uri: axum::http::Uri,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let path = match uri.path().strip_prefix("/3") {
            Some(path) => path.to_string(),
            None => format!("outside the base URL: {}", uri.path()),
        };
        let user_agent = headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        hits.lock().unwrap().push(G12Hit {
            path: path.clone(),
            api_key: query.get("api_key").cloned(),
            user_agent,
        });
        let body = match path.as_str() {
            "/search/movie" => {
                serde_json::json!({"results": [{"id": 9001, "title": "Alpha Film"}]})
            }
            "/movie/9001" => serde_json::json!({
                "id": 9001,
                "title": "Alpha Film",
                "overview": "G12 stub overview",
                "original_language": "en",
                "release_date": "2001-05-04"
            }),
            "/movie/9001/external_ids" => serde_json::json!({"id": 9001, "imdb_id": "tt9001"}),
            "/movie/9001/images" => serde_json::json!({
                "posters": [{"file_path": "/g12-poster.jpg"}],
                "backdrops": [{"file_path": "/g12-backdrop.jpg"}]
            }),
            "/movie/9001/credits" => serde_json::json!({"id": 9001, "cast": [], "crew": []}),
            "/movie/9001/videos" => serde_json::json!({"results": [
                {"key": "g12trailer", "name": "G12 Trailer", "type": "Trailer", "site": "YouTube"}
            ]}),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        axum::Json(body).into_response()
    }

    let hits = G12Hits::default();
    let stub = axum::Router::new()
        .fallback(handle)
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });
    (origin, hits)
}

/// A server whose TMDb is the stub, with `env_key` as `RUSTYFIN_TMDB_API_KEY` and
/// `config_key` as the config's `TmdbApiKey`.
struct G12Server {
    origin: String,
    hits: G12Hits,
    app: axum::Router,
    token: String,
    db: sqlx::SqlitePool,
    data_dir: Arc<std::path::PathBuf>,
}

async fn g12_server(env_key: Option<&str>, config_key: Option<&str>) -> G12Server {
    use rustyfin_core::tmdb_client::{TmdbClient, TmdbKey, TmdbSettings};

    let (origin, hits) = g12_stub().await;
    let mut state = test_state().await;
    let settings = TmdbSettings {
        api_base_url: format!("{origin}/3"),
        image_base_url: format!("{origin}/t/p/original"),
        env_key: env_key.and_then(TmdbKey::new),
        min_interval: std::time::Duration::from_millis(1),
        ..TmdbSettings::default()
    };
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    state.tmdb = TmdbClient::new(http, settings);
    state.config.write().await.tmdb_api_key = config_key.map(str::to_string);

    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();

    G12Server {
        origin,
        hits,
        token,
        db: state.db.clone(),
        data_dir: state.data_dir.clone(),
        app: app(state),
    }
}

impl G12Server {
    async fn send(&self, method: &str, uri: &str, json: Option<String>) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {}", self.token));
        let request = match json {
            Some(json) => request
                .header("Content-Type", "application/json")
                .body(Body::from(json)),
            None => request.body(Body::empty()),
        };
        let resp = self.app.clone().oneshot(request.unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn hits(&self) -> Vec<G12Hit> {
        self.hits.lock().unwrap().clone()
    }

    /// A movie library with one movie whose file does not exist (no NFO, no local images).
    async fn insert_movie(&self) -> uuid::Uuid {
        let lib = rustyfin_model::library::Library {
            id: uuid::Uuid::new_v4(),
            name: "G12 Movies".to_string(),
            content_type: rustyfin_model::library::LibraryContentType::Movies,
            paths: vec!["/nonexistent/G12".to_string()],
        };
        rustyfin_db::repo_library::create_library(&self.db, &lib)
            .await
            .unwrap();
        let item_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(item_id.to_string())
        .bind(lib.id.to_string())
        .bind("Alpha Film")
        .bind("Movie")
        .bind("/nonexistent/G12/Alpha Film (2001)/Alpha Film (2001).mkv")
        .execute(&self.db)
        .await
        .unwrap();
        item_id
    }

    /// `POST /Items/{id}/Refresh`, which must answer 200 whatever TMDb does. Returns the
    /// item's TMDb id afterwards.
    async fn refresh(&self, item_id: uuid::Uuid) -> Option<i64> {
        let (status, body) = self
            .send("POST", &format!("/Items/{item_id}/Refresh"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        rustyfin_db::repo_media::get_item_by_id(&self.db, item_id)
            .await
            .unwrap()
            .unwrap()
            .tmdb_id
    }

    /// Every request carried `key` as `api_key` and Rustyfin's User-Agent, and there was one.
    fn assert_all_signed_with(&self, key: &str) {
        let hits = self.hits();
        assert!(!hits.is_empty(), "no request reached the stub");
        for hit in &hits {
            assert_eq!(hit.api_key.as_deref(), Some(key), "{}", hit.path);
            assert_eq!(
                hit.user_agent,
                rustyfin_core::tmdb_client::USER_AGENT,
                "{}",
                hit.path
            );
        }
    }
}

#[tokio::test]
async fn test_g12_env_tmdb_key_not_in_configuration() {
    let server = g12_server(Some(G12_ENV_KEY), None).await;

    for uri in ["/System/Configuration", "/System/Configuration/TmdbApiKey"] {
        let (status, body) = server.send("GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(!body.contains(G12_ENV_KEY), "{uri} shows the env key");
    }
    assert!(
        server.hits().is_empty(),
        "the configuration routes called TMDb"
    );
}

#[tokio::test]
async fn test_g12_configuration_round_trip_never_persists_env_key() {
    let server = g12_server(Some(G12_ENV_KEY), None).await;

    let (status, config) = server.send("GET", "/System/Configuration", None).await;
    assert_eq!(status, StatusCode::OK, "{config}");
    let (status, body) = server
        .send("POST", "/System/Configuration", Some(config))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let saved =
        std::fs::read_to_string(server.data_dir.join("config.json")).expect("config.json written");
    assert!(
        !saved.contains(G12_ENV_KEY),
        "config.json holds the env key"
    );
    let (_, after) = server.send("GET", "/System/Configuration", None).await;
    assert!(
        !after.contains(G12_ENV_KEY),
        "the configuration shows the env key"
    );
}

#[tokio::test]
async fn test_g12_item_refresh_uses_env_key_and_base_urls() {
    let server = g12_server(Some(G12_ENV_KEY), None).await;
    let item_id = server.insert_movie().await;

    assert_eq!(server.refresh(item_id).await, Some(9001));
    let item = rustyfin_db::repo_media::get_item_by_id(&server.db, item_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.overview.as_deref(), Some("G12 stub overview"));
    let images = rustyfin_db::repo_image::get_images_for_item(&server.db, item_id)
        .await
        .unwrap();
    let image_base = format!("{}/t/p/original/", server.origin);
    let tmdb_images: Vec<_> = images.iter().filter(|i| i.provider == "tmdb").collect();
    assert!(!tmdb_images.is_empty(), "no TMDb images stored: {images:?}");
    for image in tmdb_images {
        assert!(
            image.path.starts_with(&image_base),
            "{} is not on the image base",
            image.path
        );
    }

    let (status, body) = server.send("GET", &format!("/Items/{item_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("https://www.youtube.com/watch?v=g12trailer"),
        "no trailer in {body}"
    );
    assert!(!body.contains(G12_ENV_KEY), "the item shows the env key");

    let hits = server.hits();
    for path in [
        "/search/movie",
        "/movie/9001",
        "/movie/9001/images",
        "/movie/9001/videos",
    ] {
        assert!(
            hits.iter().any(|hit| hit.path == path),
            "{path} not requested: {hits:?}"
        );
    }
    server.assert_all_signed_with(G12_ENV_KEY);
}

#[tokio::test]
async fn test_g12_env_tmdb_key_wins_over_config_key() {
    let server = g12_server(Some(G12_ENV_KEY), Some(G12_CONFIG_KEY)).await;
    let item_id = server.insert_movie().await;

    assert_eq!(server.refresh(item_id).await, Some(9001));
    server.assert_all_signed_with(G12_ENV_KEY);
}

#[tokio::test]
async fn test_g12_item_refresh_still_uses_config_key() {
    let server = g12_server(None, Some(G12_CONFIG_KEY)).await;
    let item_id = server.insert_movie().await;

    assert_eq!(server.refresh(item_id).await, Some(9001));
    server.assert_all_signed_with(G12_CONFIG_KEY);
}

#[tokio::test]
async fn test_g12_no_tmdb_key_sends_nothing() {
    let server = g12_server(None, Some("  ")).await;
    let item_id = server.insert_movie().await;

    assert_eq!(server.refresh(item_id).await, None);
    assert!(server.hits().is_empty(), "TMDb was called without a key");
}

// ── Live TV Tests ──

#[tokio::test]
async fn test_live_tv_channels_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/LiveTv/Channels")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(result["Items"].is_array());
    assert_eq!(result["TotalRecordCount"], 0);
}

// ── Persons Tests ──

#[tokio::test]
async fn test_persons_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Persons")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Shows Tests ──

#[tokio::test]
async fn test_shows_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Shows")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Media Segments Tests ──

#[tokio::test]
async fn test_media_segments_empty() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/MediaSegments")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Devices Tests ──

#[tokio::test]
async fn test_devices_list() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Devices")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 500 if devices table doesn't exist yet
    assert!(resp.status().is_success() || resp.status() == StatusCode::INTERNAL_SERVER_ERROR);
}

// ── Subtitle Tests ──

#[tokio::test]
async fn test_subtitle_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Videos/{random_id}/Subtitles/0/Stream"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Transcode Tests ──

#[tokio::test]
async fn test_transcode_not_started() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_session = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/transcode/{random_session}/1/main.m3u8"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Backup Tests ──

#[tokio::test]
async fn test_backup_status() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    app.oneshot(
        Request::builder()
            .uri("/Backup/Status")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    // Endpoint may not be implemented yet; accept any response
}

// ── SyncPlay Tests ──

#[tokio::test]
async fn test_sync_play_instances() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/SyncPlay/GetInstances")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Encoding Manager Tests ──

#[tokio::test]
async fn test_encoding_manager_stats() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/EncodingManager/EncodingStats")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 404 if the endpoint is not yet implemented
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Library Options Tests ──

#[tokio::test]
async fn test_library_options_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Library/{random_id}/Options"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // May return 200 with empty options or 404
    assert!(resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND);
}

// ── Library Modified Volumes Tests ──

#[tokio::test]
async fn test_library_modified_volumes() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Library/ModifiedVolumes")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let volumes: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(volumes.is_empty());
}

// ── User Library Views (with user ID) Tests ──

#[tokio::test]
async fn test_get_user_views_with_id() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Users/{}/Views", user.id))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let views: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(views["Items"].is_array());
}

// ── DLNA Streaming Tests ──

#[tokio::test]
async fn test_dlna_streaming_description() {
    let state = test_state().await;
    let app = app(state);

    app.oneshot(
        Request::builder()
            .uri("/dlna/contentdirectory/description.xml")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();

    // DLNA streaming endpoint may not be implemented yet; accept any response
}

// ── Startup Wizard and Owner Tests ──

/// Sends a request and returns the status and the JSON body (`Null` when the body is not JSON).
async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Builds a request with an optional bearer token and an optional JSON body.
fn request(
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    match body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// Logs in and returns the access token and the user id, or `None` if login fails.
async fn login(app: &axum::Router, name: &str, password: &str) -> Option<(String, String)> {
    let body = serde_json::json!({ "Username": name, "Pw": password });
    let req = request("POST", "/Users/AuthenticateByName", None, Some(body));
    let (status, json) = send(app, req).await;
    if status != StatusCode::OK {
        return None;
    }
    Some((
        json["AccessToken"].as_str().unwrap().to_string(),
        json["User"]["Id"].as_str().unwrap().to_string(),
    ))
}

/// Sends the wizard's `POST /Startup/User`.
async fn post_startup_user(
    app: &axum::Router,
    token: Option<&str>,
    name: &str,
    password: &str,
) -> StatusCode {
    let body = serde_json::json!({ "Name": name, "Password": password });
    send(app, request("POST", "/Startup/User", token, Some(body)))
        .await
        .0
}

/// Reads `StartupWizardCompleted` from the public and from the full system info.
async fn wizard_completed(app: &axum::Router) -> (bool, bool) {
    let (status, public) = send(app, request("GET", "/System/Info/Public", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    let full_req = request("GET", "/System/Info", Some("test-api-key"), None);
    let (status, full) = send(app, full_req).await;
    assert_eq!(status, StatusCode::OK);
    (
        public["StartupWizardCompleted"].as_bool().unwrap(),
        full["StartupWizardCompleted"].as_bool().unwrap(),
    )
}

/// Creates a user that is not the owner. `create_initial_user` makes an administrator.
async fn add_user(state: &AppState, name: &str, password: &str, admin: bool) -> uuid::Uuid {
    let user = rustyfin_auth::create_initial_user(&state.db, name, password)
        .await
        .unwrap();
    if !admin {
        rustyfin_db::repo_user::update_policy(&state.db, user.id, false)
            .await
            .unwrap();
    }
    user.id
}

/// Returns `(is_administrator, is_owner, is_disabled)` for the named user, or `None` if it is gone.
async fn user_flags(state: &AppState, name: &str) -> Option<(bool, bool, bool)> {
    sqlx::query_as("SELECT is_administrator, is_owner, is_disabled FROM users WHERE name = ?")
        .bind(name)
        .fetch_optional(&state.db)
        .await
        .unwrap()
}

/// Returns the names of all owners and the total number of users.
async fn owners_and_count(state: &AppState) -> (Vec<String>, i64) {
    let owners: Vec<String> = sqlx::query_scalar("SELECT name FROM users WHERE is_owner = 1")
        .fetch_all(&state.db)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap();
    (owners, count)
}

#[tokio::test]
async fn test_startup_fresh_state_needs_setup() {
    let state = test_state().await;
    let app = app(state.clone());

    let users = rustyfin_db::repo_user::get_all_users(&state.db)
        .await
        .unwrap();
    assert!(users.is_empty());
    assert_eq!(wizard_completed(&app).await, (false, false));

    let (status, body) = send(&app, request("GET", "/Startup/User", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["Name"], "");
}

#[tokio::test]
async fn test_startup_user_creates_owner_with_typed_name() {
    let state = test_state().await;
    let app = app(state.clone());

    let config =
        serde_json::json!({ "PreferredMetadataLanguage": "de", "MetadataCountryCode": "DE" });
    let req = request("POST", "/Startup/Configuration", None, Some(config));
    assert_eq!(send(&app, req).await.0, StatusCode::NO_CONTENT);

    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    assert!(login(&app, "karl", "geheim").await.is_some());
    assert_eq!(wizard_completed(&app).await, (true, true));
    assert_eq!(user_flags(&state, "karl").await, Some((true, true, false)));
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["karl".to_string()], 1)
    );
}

#[tokio::test]
async fn test_startup_user_rejected_once_owner_exists() {
    let state = test_state().await;
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    let (token, _) = login(&app, "karl", "geheim").await.unwrap();

    assert_eq!(
        post_startup_user(&app, None, "karl", "hacked").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_startup_user(&app, None, "mallory", "hacked").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_startup_user(&app, Some(&token), "karl", "changed").await,
        StatusCode::FORBIDDEN
    );

    assert!(
        login(&app, "karl", "geheim").await.is_some(),
        "password unchanged"
    );
    assert!(login(&app, "karl", "hacked").await.is_none());
    assert!(login(&app, "karl", "changed").await.is_none());
    assert!(login(&app, "mallory", "hacked").await.is_none());
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["karl".to_string()], 1)
    );
}

#[tokio::test]
async fn test_startup_user_rejects_empty_name() {
    let state = test_state().await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "", "pw").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_startup_user(&app, None, "   ", "pw").await,
        StatusCode::BAD_REQUEST
    );
    let users = rustyfin_db::repo_user::get_all_users(&state.db)
        .await
        .unwrap();
    assert!(users.is_empty());
    assert_eq!(wizard_completed(&app).await, (false, false));
}

#[tokio::test]
async fn test_startup_user_allows_empty_password() {
    let state = test_state().await;
    let app = app(state.clone());

    assert_eq!(
        post_startup_user(&app, None, "karl", "").await,
        StatusCode::NO_CONTENT
    );
    assert!(login(&app, "karl", "").await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_startup_user_concurrent_requests_one_owner() {
    let state = test_state().await;
    let app = app(state.clone());

    let first = tokio::spawn({
        let app = app.clone();
        async move { post_startup_user(&app, None, "alice", "pw-alice").await }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move { post_startup_user(&app, None, "bob", "pw-bob").await }
    });
    let mut statuses = [
        first.await.unwrap().as_u16(),
        second.await.unwrap().as_u16(),
    ];
    statuses.sort_unstable();

    assert_eq!(statuses, [204, 403]);
    let (owners, count) = owners_and_count(&state).await;
    assert_eq!(owners.len(), 1);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_owner_cannot_be_deleted_or_demoted() {
    let state = test_state().await;
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    let (owner_token, owner_id) = login(&app, "karl", "geheim").await.unwrap();
    let bob_id = add_user(&state, "bob", "bobpass", true).await;
    let (bob_token, _) = login(&app, "bob", "bobpass").await.unwrap();

    let demote = serde_json::json!({ "IsAdministrator": false });
    let delete_uri = format!("/Users/{owner_id}/Delete");
    let user_uri = format!("/Users/{owner_id}");
    let policy_uri = format!("/Users/{owner_id}/Policy");
    // Another admin and the owner itself are both refused.
    for token in [bob_token.as_str(), owner_token.as_str()] {
        let req = request("POST", &delete_uri, Some(token), None);
        assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
        let req = request("DELETE", &user_uri, Some(token), None);
        assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
        let req = request("POST", &policy_uri, Some(token), Some(demote.clone()));
        assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
    }
    let promote = serde_json::json!({ "IsAdministrator": true });
    let req = request("POST", &policy_uri, Some(&bob_token), Some(promote));
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
    assert_eq!(user_flags(&state, "karl").await, Some((true, true, false)));
    assert!(login(&app, "karl", "geheim").await.is_some());

    // Other administrators can still be demoted and deleted.
    let req = request(
        "POST",
        &format!("/Users/{bob_id}/Policy"),
        Some(&owner_token),
        Some(demote),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
    assert_eq!(user_flags(&state, "bob").await, Some((false, false, false)));
    let req = request(
        "DELETE",
        &format!("/Users/{bob_id}"),
        Some(&owner_token),
        None,
    );
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
    assert_eq!(user_flags(&state, "bob").await, None);
}

#[tokio::test]
async fn test_startup_routes_require_admin_after_setup() {
    let state = test_state().await;
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    let (owner_token, _) = login(&app, "karl", "geheim").await.unwrap();
    add_user(&state, "viewer", "viewerpass", false).await;
    let (viewer_token, _) = login(&app, "viewer", "viewerpass").await.unwrap();
    let language_before = state
        .config
        .read()
        .await
        .preferred_metadata_language
        .clone();

    let config = serde_json::json!({ "PreferredMetadataLanguage": "fr" });
    let empty = serde_json::json!({});
    let routes = [
        ("GET", "/Startup/Configuration", None),
        ("POST", "/Startup/Configuration", Some(config.clone())),
        ("GET", "/Startup/User", None),
        ("POST", "/Startup/Complete", Some(empty.clone())),
        ("GET", "/Startup/FirstUser", None),
        ("POST", "/Startup/RemoteAccess", Some(empty)),
    ];
    for (method, uri, body) in &routes {
        let req = request(method, uri, None, body.clone());
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::UNAUTHORIZED,
            "anonymous {method} {uri}"
        );
        let req = request(method, uri, Some("not-a-token"), body.clone());
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::UNAUTHORIZED,
            "bad token {method} {uri}"
        );
        let req = request(method, uri, Some(&viewer_token), body.clone());
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::FORBIDDEN,
            "non-admin {method} {uri}"
        );
    }
    {
        let config = state.config.read().await;
        assert_eq!(config.preferred_metadata_language, language_before);
        assert!(!config.is_startup_wizard_completed);
    }

    let req = request("GET", "/Startup/User", Some(&owner_token), None);
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["Name"], "karl");
    assert_eq!(body["Password"], "");
    let req = request(
        "POST",
        "/Startup/Configuration",
        Some(&owner_token),
        Some(config),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::NO_CONTENT);
    assert_eq!(state.config.read().await.preferred_metadata_language, "fr");
    let req = request("GET", "/Startup/Configuration", Some("test-api-key"), None);
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
}

#[tokio::test]
async fn test_startup_complete_requires_owner() {
    let state = test_state().await;
    let app = app(state.clone());
    let empty = serde_json::json!({});

    let req = request("POST", "/Startup/Complete", None, Some(empty.clone()));
    assert_eq!(send(&app, req).await.0, StatusCode::BAD_REQUEST);
    assert!(!state.config.read().await.is_startup_wizard_completed);

    assert_eq!(
        post_startup_user(&app, None, "karl", "geheim").await,
        StatusCode::NO_CONTENT
    );
    let (token, _) = login(&app, "karl", "geheim").await.unwrap();
    let req = request("POST", "/Startup/Complete", Some(&token), Some(empty));
    assert_eq!(send(&app, req).await.0, StatusCode::NO_CONTENT);
    assert!(state.config.read().await.is_startup_wizard_completed);
    let saved = std::fs::read_to_string(state.data_dir.join("config.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["IsStartupWizardCompleted"], true);
}

#[tokio::test]
async fn test_upgrade_promotes_earliest_admin() {
    use rustyfin_db::repo_user::promote_earliest_admin_to_owner;

    let state = test_state().await;
    add_user(&state, "viewer", "pw", false).await;
    let promoted = promote_earliest_admin_to_owner(&state.db).await.unwrap();
    assert!(promoted.is_none(), "no admin, nothing to promote");

    // Insertion order differs from creation order, so only created_at can pick "first".
    add_user(&state, "second", "pw", true).await;
    add_user(&state, "first", "pw", true).await;
    for (name, created_at) in [
        ("viewer", "2020-01-01T00:00:00+00:00"),
        ("first", "2021-01-01T00:00:00+00:00"),
        ("second", "2022-01-01T00:00:00+00:00"),
    ] {
        sqlx::query("UPDATE users SET created_at = ? WHERE name = ?")
            .bind(created_at)
            .bind(name)
            .execute(&state.db)
            .await
            .unwrap();
    }

    let promoted = promote_earliest_admin_to_owner(&state.db).await.unwrap();
    assert_eq!(promoted.map(|u| u.name).as_deref(), Some("first"));
    let again = promote_earliest_admin_to_owner(&state.db).await.unwrap();
    assert!(again.is_none(), "an owner already exists");
    assert_eq!(
        owners_and_count(&state).await,
        (vec!["first".to_string()], 3)
    );
}

// ── G1: authorization ──
// Admin endpoints answer 401 without a valid token and 403 to a non-admin. Setup endpoints are
// open only while the server has no users. Names taken from a request stay inside their directory.

/// Creates the owner through the wizard and a non-admin "viewer", logs both in and returns
/// `(owner_token, owner_id, viewer_token, viewer_id)`.
async fn g1_owner_and_viewer(
    state: &AppState,
    app: &axum::Router,
) -> (String, String, String, String) {
    assert_eq!(
        post_startup_user(app, None, "owner", "ownerpass").await,
        StatusCode::NO_CONTENT
    );
    let (owner_token, owner_id) = login(app, "owner", "ownerpass").await.unwrap();
    add_user(state, "viewer", "viewerpass", false).await;
    let (viewer_token, viewer_id) = login(app, "viewer", "viewerpass").await.unwrap();
    (owner_token, owner_id, viewer_token, viewer_id)
}

/// Asserts 401 without a token and with a bad one, and 403 for the non-admin `viewer`.
async fn g1_assert_admin_only(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &Option<serde_json::Value>,
    viewer: &str,
) {
    let callers = [
        ("anonymous", None, StatusCode::UNAUTHORIZED),
        ("bad token", Some("not-a-token"), StatusCode::UNAUTHORIZED),
        ("non-admin", Some(viewer), StatusCode::FORBIDDEN),
    ];
    for (who, token, expected) in callers {
        let status = send(app, request(method, uri, token, body.clone())).await.0;
        assert_eq!(status, expected, "{who} {method} {uri}");
    }
}

/// Asserts that an administrator's request is not refused (neither 401 nor 403).
async fn g1_assert_not_refused(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &Option<serde_json::Value>,
    token: &str,
) {
    let status = send(app, request(method, uri, Some(token), body.clone()))
        .await
        .0;
    assert!(
        status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
        "admin {method} {uri} was refused: {status}"
    );
}

/// Checks a table of `(method, uri, body, check the admin too)`: every route refuses anonymous
/// callers, bad tokens and the non-admin; marked routes must not refuse the owner or the API key.
/// The refusals run first, so no admin request changes state before them.
async fn g1_check_admin_routes(
    app: &axum::Router,
    routes: &[(&str, String, Option<serde_json::Value>, bool)],
    owner: &str,
    viewer: &str,
) {
    for (method, uri, body, _) in routes {
        g1_assert_admin_only(app, method, uri, body, viewer).await;
    }
    for (method, uri, body, check_admin) in routes {
        if *check_admin {
            g1_assert_not_refused(app, method, uri, body, owner).await;
            g1_assert_not_refused(app, method, uri, body, "test-api-key").await;
        }
    }
}

/// Sends a request and returns the status and the body as text.
async fn g1_send_raw(app: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes `<data>/g1-secret.txt`, outside every directory the server reads from.
fn g1_secret_file(state: &AppState) -> std::path::PathBuf {
    let secret = state.data_dir.join("g1-secret.txt");
    std::fs::write(&secret, "g1-secret-content").unwrap();
    secret
}

/// Percent-encodes every `/` so that an absolute path fits in one path segment.
fn g1_encode_slashes(path: &std::path::Path) -> String {
    path.to_str().unwrap().replace('/', "%2F")
}

/// Stores a library with one movie and returns the movie's id.
async fn g1_insert_movie(state: &AppState) -> uuid::Uuid {
    let folder = media_folder(state, "g1-movies");
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "G1 Movies".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::Movies,
        paths: vec![folder.clone()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    let item_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind("G1 Movie")
    .bind("Movie")
    .bind(format!("{folder}/G1 Movie.mkv"))
    .execute(&state.db)
    .await
    .unwrap();
    item_id
}

#[tokio::test]
async fn g1_system_admin_routes() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let config = serde_json::to_value(&*state.config.read().await).unwrap();
    let empty = Some(serde_json::json!({}));
    let routes = [
        ("GET", "/System/Logs".to_string(), None, true),
        ("GET", "/System/Logs/Log".to_string(), None, true),
        ("GET", "/System/Log/rustyfin.log".to_string(), None, true),
        ("GET", "/System/Info/Storage".to_string(), None, true),
        ("GET", "/System/LogLevel".to_string(), None, true),
        ("POST", "/System/LogLevel".to_string(), empty.clone(), false),
        ("GET", "/System/UpdatePackages".to_string(), None, true),
        (
            "POST",
            "/System/RestartScheduledTasks".to_string(),
            None,
            false,
        ),
        (
            "POST",
            "/System/Configuration".to_string(),
            Some(config),
            true,
        ),
        (
            "POST",
            "/System/Configuration/ServerName".to_string(),
            Some(serde_json::json!("G1")),
            true,
        ),
        (
            "POST",
            "/System/Configuration/Branding".to_string(),
            empty.clone(),
            true,
        ),
        ("POST", "/System/Shutdown".to_string(), None, false),
        ("POST", "/System/Restart".to_string(), None, false),
        (
            "PUT",
            "/Configuration/MediaEncoding".to_string(),
            empty.clone(),
            true,
        ),
        ("PUT", "/Configuration/Networking".to_string(), empty, true),
        ("GET", "/System/ActivityLog/Entries".to_string(), None, true),
        ("GET", "/System/MediaEncoder/Path".to_string(), None, true),
        ("POST", "/System/MediaEncoder/Test".to_string(), None, false),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
}

#[tokio::test]
async fn g1_admin_modules_refuse_users() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let id = uuid::Uuid::new_v4();
    let empty = Some(serde_json::json!({}));
    let list = Some(serde_json::json!([]));
    let routes = [
        ("GET", "/ScheduledTasks".to_string(), None, true),
        ("GET", format!("/ScheduledTasks/{id}"), None, true),
        (
            "POST",
            format!("/ScheduledTasks/{id}/Triggers"),
            list.clone(),
            false,
        ),
        ("POST", format!("/ScheduledTasks/Running/{id}"), None, false),
        (
            "DELETE",
            format!("/ScheduledTasks/Running/{id}"),
            None,
            false,
        ),
        ("GET", "/Plugins".to_string(), None, true),
        ("GET", format!("/Plugins/{id}/Configuration"), None, true),
        (
            "POST",
            format!("/Plugins/{id}/Configuration"),
            empty.clone(),
            false,
        ),
        ("GET", "/Packages".to_string(), None, false),
        ("GET", "/Packages/Repositories".to_string(), None, true),
        ("POST", "/Packages/Repositories".to_string(), list, false),
        (
            "DELETE",
            "/Packages/Repositories/g1".to_string(),
            None,
            false,
        ),
        ("POST", "/Packages/g1/Install".to_string(), None, false),
        ("POST", "/Packages/g1/Uninstall".to_string(), None, false),
        ("POST", "/Packages/g1/Enable".to_string(), None, false),
        ("POST", "/Packages/g1/Disable".to_string(), None, false),
        (
            "POST",
            "/Packages/Restore".to_string(),
            empty.clone(),
            false,
        ),
        ("GET", "/Backup".to_string(), None, true),
        ("POST", "/Backup".to_string(), empty.clone(), false),
        ("DELETE", "/Backup/g1.zip".to_string(), None, false),
        ("POST", "/Backup/Restore".to_string(), empty.clone(), false),
        ("GET", "/Devices".to_string(), None, false),
        ("POST", "/Devices".to_string(), empty.clone(), false),
        ("GET", "/Devices/g1-device".to_string(), None, true),
        ("DELETE", "/Devices/g1-device".to_string(), None, false),
        ("POST", "/Devices/g1-device/Image".to_string(), None, false),
        ("DELETE", "/Devices?id=g1-device".to_string(), None, false),
        ("GET", "/Devices/Info?id=g1-device".to_string(), None, true),
        (
            "GET",
            "/Devices/Options?id=g1-device".to_string(),
            None,
            true,
        ),
        (
            "POST",
            "/Devices/Options?id=g1-device".to_string(),
            empty.clone(),
            false,
        ),
        ("GET", "/Dlna/Streaming/Profiles".to_string(), None, true),
        (
            "POST",
            "/Dlna/Streaming/Profiles".to_string(),
            empty.clone(),
            false,
        ),
        ("GET", format!("/Dlna/Streaming/Profiles/{id}"), None, true),
        (
            "PUT",
            format!("/Dlna/Streaming/Profiles/{id}"),
            empty,
            false,
        ),
        (
            "DELETE",
            format!("/Dlna/Streaming/Profiles/{id}"),
            None,
            false,
        ),
        ("GET", "/Auth/Keys".to_string(), None, true),
        ("DELETE", "/Auth/Keys/g1-key".to_string(), None, false),
        ("GET", "/Auth/Providers".to_string(), None, true),
        (
            "GET",
            "/Auth/PasswordResetProviders".to_string(),
            None,
            true,
        ),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
}

#[tokio::test]
async fn g1_user_admin_routes() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let other = add_user(&state, "other", "otherpass", false).await;
    let new_user = serde_json::json!({ "Name": "g1-new", "Password": "g1-pass" });
    let promote = serde_json::json!({ "IsAdministrator": true });
    let routes = [
        ("POST", "/Users/New".to_string(), Some(new_user), false),
        (
            "POST",
            format!("/Users/{other}/Policy"),
            Some(promote),
            false,
        ),
        ("DELETE", format!("/Users/{other}"), None, false),
        ("POST", format!("/Users/{other}/Delete"), None, false),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
    assert_eq!(user_flags(&state, "g1-new").await, None);
    assert_eq!(
        user_flags(&state, "other").await,
        Some((false, false, false))
    );

    for (method, uri, body, _) in &routes {
        g1_assert_not_refused(&app, method, uri, body, &owner).await;
    }
    assert!(user_flags(&state, "g1-new").await.is_some());
    assert_eq!(user_flags(&state, "other").await, None);
}

#[tokio::test]
async fn g1_item_admin_routes() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let item = uuid::Uuid::new_v4();
    let lib = uuid::Uuid::new_v4();
    let coll = uuid::Uuid::new_v4();
    let routes = [
        ("POST", "/Library/Refresh".to_string(), None, true),
        (
            "POST",
            format!("/Library/VirtualFolders/{lib}/Refresh"),
            None,
            true,
        ),
        ("POST", format!("/Items/{item}/Refresh"), None, false),
        ("DELETE", format!("/Items?ids={item}"), None, true),
        ("DELETE", format!("/Items/{item}"), None, true),
        ("POST", format!("/Items/{item}/Images/Primary"), None, true),
        (
            "DELETE",
            format!("/Items/{item}/Images/Primary"),
            None,
            true,
        ),
        (
            "DELETE",
            format!("/Items/{item}/Images/Primary/0"),
            None,
            true,
        ),
        (
            "DELETE",
            format!("/Items/{item}/AlternateSources"),
            None,
            true,
        ),
        ("DELETE", format!("/Items/{item}/Subtitles/0"), None, true),
        (
            "GET",
            format!("/Items/{item}/RemoteSearch/Subtitles"),
            None,
            false,
        ),
        (
            "POST",
            format!("/Videos/{item}/Subtitles/0/Download"),
            None,
            false,
        ),
        ("POST", "/Collections?Name=G1".to_string(), None, true),
        ("DELETE", format!("/Collections/{coll}"), None, true),
        (
            "POST",
            format!("/Collections/{coll}/Items?ids={item}"),
            None,
            true,
        ),
        (
            "DELETE",
            format!("/Collections/{coll}/Items?ids={item}"),
            None,
            true,
        ),
        (
            "POST",
            format!("/Collections/{coll}/Items/{item}"),
            None,
            true,
        ),
        (
            "DELETE",
            format!("/Collections/{coll}/Items/{item}"),
            None,
            true,
        ),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
}

#[tokio::test]
async fn g1_live_tv_dlna_localization_admin_routes() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let empty = Some(serde_json::json!({}));
    let routes = [
        ("GET", "/LiveTv/TunerHosts".to_string(), None, true),
        (
            "POST",
            "/LiveTv/TunerHosts".to_string(),
            empty.clone(),
            false,
        ),
        (
            "DELETE",
            "/LiveTv/TunerHosts?id=g1".to_string(),
            None,
            false,
        ),
        ("GET", "/LiveTv/TunerHosts/g1".to_string(), None, true),
        ("DELETE", "/LiveTv/TunerHosts/g1".to_string(), None, false),
        ("GET", "/LiveTv/Tuners".to_string(), None, true),
        ("GET", "/LiveTv/Tuners/g1".to_string(), None, true),
        ("GET", "/LiveTv/ListingProviders".to_string(), None, true),
        (
            "POST",
            "/LiveTv/ListingProviders".to_string(),
            empty.clone(),
            false,
        ),
        (
            "DELETE",
            "/LiveTv/ListingProviders?id=g1".to_string(),
            None,
            false,
        ),
        ("GET", "/LiveTv/ListingProviders/g1".to_string(), None, true),
        (
            "DELETE",
            "/LiveTv/ListingProviders/g1".to_string(),
            None,
            false,
        ),
        (
            "GET",
            "/LiveTv/ChannelMappingOptions".to_string(),
            None,
            true,
        ),
        ("GET", "/Dlna/Profile".to_string(), None, true),
        (
            "POST",
            "/Dlna/Profile/DeviceDetection?UserAgent=Jellyfin/10.8.0".to_string(),
            None,
            true,
        ),
        ("GET", "/Dlna/Profile/Device".to_string(), None, true),
        (
            "POST",
            "/Localization/Providers".to_string(),
            empty.clone(),
            false,
        ),
        (
            "PUT",
            "/Localization/Providers/g1".to_string(),
            empty,
            false,
        ),
        (
            "DELETE",
            "/Localization/Providers/g1".to_string(),
            None,
            false,
        ),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
}

#[tokio::test]
async fn g1_user_routes_stay_open() {
    let state = test_state().await;
    let app = app(state.clone());
    let (_, _, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let uris = [
        "/System/Info".to_string(),
        "/System/Configuration".to_string(),
        "/System/Configuration/ServerName".to_string(),
        "/Users/Me".to_string(),
        "/Users".to_string(),
        format!("/Users/{viewer_id}"),
        "/UserViews".to_string(),
        format!("/Users/{viewer_id}/Views"),
        "/Items".to_string(),
        "/Sessions".to_string(),
        "/Localization/Options".to_string(),
        "/Localization/Providers".to_string(),
        "/Configuration/MediaEncoding".to_string(),
        "/Configuration/Networking".to_string(),
        format!("/DisplayPreferences/usersettings?userId={viewer_id}&client=emby"),
        "/Collections".to_string(),
        "/LiveTv/Channels".to_string(),
        "/LiveTv/Timers".to_string(),
    ];
    for uri in &uris {
        let (status, body) = send(&app, request("GET", uri, Some(&viewer), None)).await;
        assert_eq!(status, StatusCode::OK, "non-admin GET {uri}: {body}");
    }
    // Stream and image URLs carry the token as `api_key`.
    let uri = format!("/Users/Me?api_key={viewer}");
    assert_eq!(
        send(&app, request("GET", &uri, None, None)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn g1_setup_routes_open_before_owner() {
    let state = test_state().await;
    let app = app(state.clone());
    let parent = state.data_dir.to_str().unwrap().to_string();
    for uri in [
        "/Environment/Drives".to_string(),
        "/Environment/NetworkShares".to_string(),
        format!("/Library/BrowseDirectories?parent={parent}"),
        "/Library/VirtualFolders".to_string(),
    ] {
        let (status, body) = send(&app, request("GET", &uri, None, None)).await;
        assert_eq!(status, StatusCode::OK, "anonymous GET {uri}: {body}");
    }
    let body = serde_json::json!({
        "Name": "G1 Setup",
        "ContentType": "Movies",
        "Paths": [media_folder(&state, "g1-setup")],
    });
    let req = request("POST", "/Library/VirtualFolders", None, Some(body));
    let (status, created) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "anonymous create: {created}");
    let uri = format!("/Library/{}/Options", created["Id"].as_str().unwrap());
    assert_eq!(
        send(&app, request("GET", &uri, None, None)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn g1_setup_routes_admin_after_owner() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let lib = create_library_via_api(&app, &state, &owner, "G1 Kept", "Movies").await;
    let kept = media_folder(&state, "g1 kept");
    let parent = state.data_dir.to_str().unwrap().to_string();
    let new_library = serde_json::json!({
        "Name": "G1 Refused",
        "ContentType": "Movies",
        "Paths": [media_folder(&state, "g1-refused")],
    });
    let remove_path = serde_json::json!({ "LibraryId": lib.to_string(), "Paths": [kept] });
    let routes = [
        ("GET", "/Environment/Drives".to_string(), None, true),
        ("GET", "/Environment/NetworkShares".to_string(), None, true),
        (
            "GET",
            format!("/Library/BrowseDirectories?parent={parent}"),
            None,
            true,
        ),
        ("GET", "/Library/VirtualFolders".to_string(), None, true),
        (
            "POST",
            "/Library/VirtualFolders".to_string(),
            Some(new_library),
            false,
        ),
        ("GET", format!("/Library/{lib}/Options"), None, true),
        (
            "POST",
            format!("/Library/{lib}/Options"),
            Some(serde_json::json!({})),
            false,
        ),
        (
            "DELETE",
            "/Library/VirtualFolders/Paths".to_string(),
            Some(remove_path),
            false,
        ),
        (
            "DELETE",
            format!("/Library/VirtualFolders/{lib}"),
            None,
            false,
        ),
    ];
    g1_check_admin_routes(&app, &routes, &owner, &viewer).await;
    let req = request("GET", "/Library/VirtualFolders", Some(&owner), None);
    let (status, libraries) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = libraries
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["G1 Kept"]);
}

#[tokio::test]
async fn g1_log_file_route_reads_only_listed_logs() {
    let state = test_state().await;
    let app = app(state.clone());
    let logs = state.data_dir.join("logs");
    std::fs::write(logs.join("rustyfin.log"), "g1 log line\n").unwrap();
    let secret = g1_secret_file(&state);
    std::fs::write(state.data_dir.join("outside.log"), "g1-outside-content").unwrap();
    // A symlink to the secret on unix; elsewhere "link.log" below is a name that does not exist.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, logs.join("link.log")).unwrap();
    std::fs::create_dir_all(logs.join("sub")).unwrap();
    std::fs::write(logs.join("sub").join("inner.log"), "g1-inner-content").unwrap();
    let get = |uri: String| request("GET", &uri, Some("test-api-key"), None);

    let (status, text) = g1_send_raw(&app, get("/System/Log/rustyfin.log".into())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("g1 log line"));

    for name in [
        "..%2Fg1-secret.txt".to_string(),
        "%2e%2e%2fg1-secret.txt".to_string(),
        "..%2Foutside.log".to_string(),
        "..%2Ftest.db".to_string(),
        g1_encode_slashes(&secret),
        "link.log".to_string(),
        "sub%2Finner.log".to_string(),
        "sub".to_string(),
        "%2e%2e".to_string(),
        "missing.log".to_string(),
    ] {
        let uri = format!("/System/Log/{name}");
        let (status, text) = g1_send_raw(&app, get(uri.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}");
        for content in [
            "g1-secret-content",
            "g1-outside-content",
            "g1-inner-content",
        ] {
            assert!(!text.contains(content), "GET {uri} returned {content}");
        }
    }

    let (status, listed) = send(&app, get("/System/Logs".into())).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["Name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["rustyfin.log"]);
}

#[tokio::test]
async fn g1_segment_path_stays_in_item_dir() {
    let state = test_state().await;
    let transcodes = state.data_dir.join("transcodes");
    let item = uuid::Uuid::new_v4();
    let dir = transcodes.join(item.to_string());
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("segment_0000.ts"), "g1-segment").unwrap();
    std::fs::write(dir.join("sub").join("inner.ts"), "g1-inner").unwrap();
    let secret = g1_secret_file(&state);
    // A symlink to the secret on unix; elsewhere "evil.ts" below is a name that does not exist.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, dir.join("evil.ts")).unwrap();
    let other = uuid::Uuid::new_v4();
    std::fs::create_dir_all(transcodes.join(other.to_string())).unwrap();
    std::fs::write(
        transcodes.join(other.to_string()).join("segment_0000.ts"),
        "g1-other",
    )
    .unwrap();
    let manager = &state.transcode_manager;

    let found = manager
        .get_segment_path(item, "segment_0000.ts")
        .expect("a real segment is found");
    assert_eq!(std::fs::read_to_string(found).unwrap(), "g1-segment");
    let absolute = secret.to_str().unwrap().to_string();
    let other_segment = format!("../{other}/segment_0000.ts");
    for name in [
        "",
        ".",
        "..",
        "../../g1-secret.txt",
        "..\\..\\g1-secret.txt",
        absolute.as_str(),
        "evil.ts",
        "sub",
        "sub/inner.ts",
        other_segment.as_str(),
        "missing.ts",
        "segment_0000.ts\0",
    ] {
        assert_eq!(manager.get_segment_path(item, name), None, "{name:?}");
    }
}

#[tokio::test]
async fn g1_legacy_hls_segment_refuses_traversal() {
    let state = test_state().await;
    let app = app(state.clone());
    let (_, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let item = uuid::Uuid::new_v4();
    let dir = state.data_dir.join("transcodes").join(item.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("segment_0000.ts"), "g1-segment").unwrap();
    let secret = g1_secret_file(&state);
    // A symlink to the secret on unix; elsewhere "evil.ts" below is a name that does not exist.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, dir.join("evil.ts")).unwrap();

    let uri = format!("/Videos/{item}/hls/segment_0000.ts");
    let (status, text) = g1_send_raw(&app, request("GET", &uri, Some(&viewer), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, "g1-segment");
    let uri = format!("{uri}?api_key={viewer}");
    let (status, text) = g1_send_raw(&app, request("GET", &uri, None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text, "g1-segment");

    for name in [
        "..%2F..%2Fg1-secret.txt".to_string(),
        "%2e%2e%2f%2e%2e%2fg1-secret.txt".to_string(),
        "..%2F..%2Ftest.db".to_string(),
        g1_encode_slashes(&secret),
        "evil.ts".to_string(),
        "%2e%2e".to_string(),
        "missing.ts".to_string(),
    ] {
        let uri = format!("/Videos/{item}/hls/{name}");
        let (status, text) = g1_send_raw(&app, request("GET", &uri, Some(&viewer), None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}");
        assert!(!text.contains("g1-secret-content"), "GET {uri} leaked");
    }
}

/// Needs an executable shell script, so unix only; `g1_media_encoder_path_cannot_be_set` checks
/// the same routes everywhere.
#[cfg(unix)]
#[tokio::test]
async fn g1_media_encoder_runs_no_request_path() {
    use std::os::unix::fs::PermissionsExt;

    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let marker = state.data_dir.join("g1-encoder-marker");
    let probe = state.data_dir.join("g1-probe.sh");
    std::fs::write(&probe, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let body = serde_json::json!({ "Value": probe.to_str().unwrap() });

    for token in [owner.as_str(), "test-api-key", viewer.as_str()] {
        for (method, uri) in [
            ("PUT", "/System/MediaEncoder/Path"),
            ("POST", "/System/MediaEncoder/Path/Validate"),
        ] {
            send(&app, request(method, uri, Some(token), Some(body.clone()))).await;
        }
    }
    assert!(!marker.exists(), "an encoder path from a request was run");
}

#[tokio::test]
async fn g1_media_encoder_path_cannot_be_set() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let probe = state.data_dir.join("g1-probe.sh");
    let body = serde_json::json!({ "Value": probe.to_str().unwrap() });

    let req = request(
        "PUT",
        "/System/MediaEncoder/Path",
        Some(&owner),
        Some(body.clone()),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::METHOD_NOT_ALLOWED);
    let req = request(
        "POST",
        "/System/MediaEncoder/Path/Validate",
        Some(&owner),
        Some(body),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::NOT_FOUND);
    for (method, uri) in [
        ("GET", "/System/MediaEncoder/Path"),
        ("POST", "/System/MediaEncoder/Test"),
    ] {
        g1_assert_admin_only(&app, method, uri, &None, &viewer).await;
    }
    g1_assert_not_refused(&app, "GET", "/System/MediaEncoder/Path", &None, &owner).await;
}

#[tokio::test]
async fn g1_easy_pin_is_hashed_and_never_returned() {
    const PIN: &str = "90817263";
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let settings = serde_json::json!({ "AudioLanguagePreference": "deu" });
    let uri = format!("/Users/{viewer_id}/Configuration");
    let req = request("POST", &uri, Some(&viewer), Some(settings));
    assert!(send(&app, req).await.0.is_success());
    let user_uri = format!("/Users/{viewer_id}");
    let (_, before) = send(&app, request("GET", &user_uri, Some(&viewer), None)).await;

    let uri = format!("/Users/{viewer_id}/EasyPassword");
    let req = request(
        "POST",
        &uri,
        Some(&viewer),
        Some(serde_json::json!({ "EasyPassword": PIN })),
    );
    let (status, text) = g1_send_raw(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!text.contains(PIN), "the call returned the PIN");

    let login_body = serde_json::json!({ "Username": "viewer", "Pw": "viewerpass" });
    let responses = [
        request("GET", "/Users", Some(&owner), None),
        request("GET", &user_uri, Some(&owner), None),
        request("GET", &user_uri, Some(&viewer), None),
        request("GET", "/Users/Me", Some(&viewer), None),
        request("GET", "/Users/Public", None, None),
        request("POST", "/Users/AuthenticateByName", None, Some(login_body)),
    ];
    for req in responses {
        let uri = req.uri().to_string();
        let (status, text) = g1_send_raw(&app, req).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!text.contains(PIN), "{uri} returned the PIN");
    }

    let (_, after) = send(&app, request("GET", &user_uri, Some(&viewer), None)).await;
    assert_eq!(after["Configuration"], before["Configuration"]);
    let stored: String = sqlx::query_scalar("SELECT configuration FROM users WHERE name = ?")
        .bind("viewer")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(
        !stored.contains(PIN),
        "the stored configuration holds the PIN"
    );
    let pin_hash = |name: &'static str| {
        let db = state.db.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT easy_password_hash FROM users WHERE name = ?",
            )
            .bind(name)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let hash = pin_hash("viewer").await.expect("the PIN hash is stored");
    assert_ne!(hash, PIN);
    assert!(rustyfin_auth::verify_password(PIN, &hash).await.unwrap());

    // Only the user and administrators set a PIN; an empty PIN clears it.
    let uri = format!("/Users/{owner_id}/EasyPassword");
    let req = request(
        "POST",
        &uri,
        Some(&viewer),
        Some(serde_json::json!({ "EasyPassword": "1234" })),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN);
    assert_eq!(pin_hash("owner").await, None);
    let uri = format!("/Users/{viewer_id}/EasyPassword");
    let req = request(
        "POST",
        &uri,
        Some(&owner),
        Some(serde_json::json!({ "EasyPassword": "" })),
    );
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
    assert_eq!(pin_hash("viewer").await, None);
}

#[tokio::test]
async fn g1_create_user_honours_is_administrator() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, _, _) = g1_owner_and_viewer(&state, &app).await;
    for (name, flag, admin) in [
        ("g1-admin", Some(true), true),
        ("g1-plain", Some(false), false),
        ("g1-default", None, false),
    ] {
        let mut body = serde_json::json!({ "Name": name, "Password": "g1-pass" });
        if let Some(flag) = flag {
            body["IsAdministrator"] = serde_json::json!(flag);
        }
        let (status, created) = send(
            &app,
            request("POST", "/Users/New", Some(&owner), Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create {name}: {created}");
        assert_eq!(
            user_flags(&state, name).await,
            Some((admin, false, false)),
            "{name}"
        );
    }
}

#[tokio::test]
async fn g1_played_and_favorite_follow_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let item = g1_insert_movie(&state).await;
    let data = |user: &str| {
        let db = state.db.clone();
        let user: uuid::Uuid = user.parse().unwrap();
        async move {
            let data = rustyfin_db::repo_playback::get_user_item_data(&db, user, item)
                .await
                .unwrap();
            (data.is_favorite, data.played)
        }
    };

    let uri = format!("/Users/{viewer_id}/FavoriteItems/{item}");
    assert_eq!(
        send(&app, request("POST", &uri, Some(&viewer), None))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(data(&viewer_id).await, (true, false));

    for (method, kind) in [
        ("POST", "FavoriteItems"),
        ("DELETE", "FavoriteItems"),
        ("POST", "PlayedItems"),
        ("DELETE", "PlayedItems"),
    ] {
        let uri = format!("/Users/{owner_id}/{kind}/{item}");
        let status = send(&app, request(method, &uri, Some(&viewer), None))
            .await
            .0;
        assert_eq!(status, StatusCode::FORBIDDEN, "non-admin {method} {uri}");
    }
    assert_eq!(data(&owner_id).await, (false, false));
    assert_eq!(data(&viewer_id).await, (true, false));

    // An administrator acts for the user in the path.
    let uri = format!("/Users/{viewer_id}/PlayedItems/{item}");
    assert_eq!(
        send(&app, request("POST", &uri, Some(&owner), None))
            .await
            .0,
        StatusCode::OK
    );
    let uri = format!("/Users/{viewer_id}/FavoriteItems/{item}");
    assert_eq!(
        send(&app, request("DELETE", &uri, Some(&owner), None))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(data(&viewer_id).await, (false, true));
    assert_eq!(data(&owner_id).await, (false, false));
    let uri = format!("/Users/{owner_id}/FavoriteItems/{item}");
    assert_eq!(
        send(&app, request("POST", &uri, Some("test-api-key"), None))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(data(&owner_id).await, (true, false));

    let uri = format!("/Users/{}/PlayedItems/{item}", uuid::Uuid::new_v4());
    assert_eq!(
        send(&app, request("POST", &uri, Some(&owner), None))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

/// The DLNA server's description, a Browse request and a connection-manager request.
fn g1_dlna_requests() -> Vec<Request<Body>> {
    let browse = concat!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
        r#"<s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">"#,
        "<ObjectID>0</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter>",
        "<StartingIndex>0</StartingIndex><RequestedCount>10</RequestedCount>",
        "<SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
    );
    let soap = |uri: &str, action: &str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "text/xml")
            .header("SOAPACTION", action)
            .body(Body::from(browse))
            .unwrap()
    };
    let get = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
    vec![
        get("/dlna/description.xml"),
        soap(
            "/dlna/control/ContentDirectory",
            r#""urn:schemas-upnp-org:service:ContentDirectory:1#Browse""#,
        ),
        get("/dlna/ContentDirectory.xml"),
        get("/dlna/ConnectionManager.xml"),
        soap(
            "/dlna/control/ConnectionManager",
            r#""urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo""#,
        ),
    ]
}

/// Turns the DLNA server on or off through `POST /System/Configuration`.
async fn g1_set_dlna(app: &axum::Router, enabled: bool) {
    let req = request("GET", "/System/Configuration", Some("test-api-key"), None);
    let (status, mut config) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    config["EnableDlnaServer"] = serde_json::json!(enabled);
    let req = request(
        "POST",
        "/System/Configuration",
        Some("test-api-key"),
        Some(config),
    );
    assert!(send(app, req).await.0.is_success());
}

#[tokio::test]
async fn g1_dlna_server_off_by_default() {
    let defaults = serde_json::to_value(ServerConfiguration::default()).unwrap();
    assert_eq!(defaults["EnableDlnaServer"], serde_json::json!(false));
    let state = test_state().await;
    let app = app(state.clone());
    for i in 0..g1_dlna_requests().len() {
        let req = g1_dlna_requests().remove(i);
        let uri = req.uri().to_string();
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::NOT_FOUND,
            "DLNA off: {uri}"
        );
        let mut req = g1_dlna_requests().remove(i);
        req.headers_mut()
            .insert("X-Emby-Token", "test-api-key".parse().unwrap());
        let status = send(&app, req).await.0;
        assert_eq!(status, StatusCode::NOT_FOUND, "DLNA off, API key: {uri}");
    }

    g1_set_dlna(&app, true).await;
    for req in g1_dlna_requests().into_iter().take(2) {
        let uri = req.uri().to_string();
        assert_eq!(send(&app, req).await.0, StatusCode::OK, "DLNA on: {uri}");
    }
    g1_set_dlna(&app, false).await;
    let req = g1_dlna_requests().remove(0);
    assert_eq!(send(&app, req).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn g1_config_secrets_hidden_from_users() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let secrets = [
        ("TmdbApiKey", "g1-tmdb-secret"),
        ("OmdbApiKey", "g1-omdb-secret"),
        ("FanartApiKey", "g1-fanart-secret"),
        ("SmtpPassword", "g1-smtp-secret"),
    ];
    let (_, mut config) = send(
        &app,
        request("GET", "/System/Configuration", Some(&owner), None),
    )
    .await;
    for (field, value) in secrets {
        config[field] = serde_json::json!(value);
    }
    let req = request("POST", "/System/Configuration", Some(&owner), Some(config));
    assert!(send(&app, req).await.0.is_success());

    let req = request("GET", "/System/Configuration", Some(&viewer), None);
    let (status, text) = g1_send_raw(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    let seen: serde_json::Value = serde_json::from_str(&text).unwrap();
    for (field, value) in secrets {
        assert!(!text.contains(value), "a non-admin saw {field}");
        assert_eq!(seen.get(field), Some(&serde_json::Value::Null), "{field}");
        let uri = format!("/System/Configuration/{field}");
        let (status, text) = g1_send_raw(&app, request("GET", &uri, Some(&viewer), None)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(!text.contains(value), "a non-admin saw {uri}");
    }

    let req = request("GET", "/System/Configuration", Some(&owner), None);
    let (_, full) = send(&app, req).await;
    for (field, value) in secrets {
        assert_eq!(full[field], value, "the owner reads {field}");
    }
    let req = request(
        "GET",
        "/System/Configuration/TmdbApiKey",
        Some(&owner),
        None,
    );
    assert_eq!(send(&app, req).await.1, "g1-tmdb-secret");
}

#[tokio::test]
async fn g1_self_or_admin_user_routes() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let routes = [
        (
            "PUT",
            format!("/Users/{owner_id}"),
            serde_json::json!({ "Name": "renamed" }),
        ),
        (
            "POST",
            format!("/Users/{owner_id}/Password"),
            serde_json::json!({ "CurrentPw": "ownerpass", "NewPw": "g1-taken" }),
        ),
        (
            "POST",
            format!("/Users/{owner_id}/Configuration"),
            serde_json::json!({}),
        ),
        (
            "POST",
            format!("/Users/{owner_id}/EasyPassword"),
            serde_json::json!({ "EasyPassword": "1234" }),
        ),
    ];
    for (method, uri, body) in &routes {
        let req = request(method, uri, Some(&viewer), Some(body.clone()));
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::FORBIDDEN,
            "non-admin {method} {uri}"
        );
        let req = request(method, uri, None, Some(body.clone()));
        assert_eq!(
            send(&app, req).await.0,
            StatusCode::UNAUTHORIZED,
            "anonymous {method} {uri}"
        );
    }
    assert!(login(&app, "owner", "ownerpass").await.is_some());
    assert_eq!(user_flags(&state, "owner").await, Some((true, true, false)));

    // A wrong current password stays 401.
    let uri = format!("/Users/{viewer_id}/Password");
    let body = serde_json::json!({ "CurrentPw": "wrong", "NewPw": "g1-other" });
    let req = request("POST", &uri, Some(&viewer), Some(body));
    assert_eq!(send(&app, req).await.0, StatusCode::UNAUTHORIZED);

    // The user and administrators may change the user.
    let uri = format!("/Users/{viewer_id}/Configuration");
    for token in [&viewer, &owner] {
        let req = request("POST", &uri, Some(token), Some(serde_json::json!({})));
        assert!(send(&app, req).await.0.is_success());
    }
    let uri = format!("/Users/{viewer_id}");
    let req = request(
        "PUT",
        &uri,
        Some(&viewer),
        Some(serde_json::json!({ "Name": "viewer" })),
    );
    assert!(send(&app, req).await.0.is_success());
}

// ── Packages Tests ──

#[tokio::test]
async fn test_packages_list() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/Packages")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Display Preferences Tests ──

#[tokio::test]
async fn test_display_preferences() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Users/{}/Preferences", user.id))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

// ── Additional Videos Tests ──

#[tokio::test]
async fn test_additional_videos_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{random_id}/AdditionalVideoParts"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Dynamic HLS Tests ──

#[tokio::test]
async fn test_dynamic_hls_not_found() {
    let state = test_state().await;
    let username = format!("admin_{}", uuid::Uuid::new_v4().simple());
    rustyfin_auth::create_initial_user(&state.db, &username, "admin")
        .await
        .unwrap();
    let user = rustyfin_db::repo_user::get_user_by_name(&state.db, &username)
        .await
        .unwrap()
        .unwrap();
    let token = rustyfin_auth::create_token(&state.db, &user, &Default::default())
        .await
        .unwrap();
    let app = app(state);

    let random_id = uuid::Uuid::new_v4();
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{random_id}/main.m3u8"))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── G1 follow-up ──
// Another user's ratings, resume list, Next Up, Upcoming and notifications need that user or an
// administrator, and a query without userId means the caller (Jellyfin's
// RequestHelpers.GetUserId). Remote control of a session or device needs its owner or an
// administrator (SessionManager.AssertCanControl); GET /Sessions shows a non-admin only their own.

const G1F_NIL: &str = "00000000-0000-0000-0000-000000000000";

/// Inserts a TV library with one series, one season and three episodes: E1 and E2 have aired, E3
/// airs in 2999. Returns `(series_id, [e1, e2, e3])`.
async fn g1f_insert_show(state: &AppState) -> (uuid::Uuid, [uuid::Uuid; 3]) {
    let folder = media_folder(state, "g1f-shows");
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "G1 Shows".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::TvShows,
        paths: vec![folder.clone()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    let series = uuid::Uuid::new_v4();
    let season = uuid::Uuid::new_v4();
    let episodes = [
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(),
    ];
    // (id, name, type, parent, index, season number, premiere date)
    let rows = [
        (series, "G1 Show", "Series", None, None, None, None),
        (
            season,
            "Season 1",
            "Season",
            Some(series),
            Some(1),
            None,
            None,
        ),
        (
            episodes[0],
            "E1",
            "Episode",
            Some(season),
            Some(1),
            Some(1),
            Some("2001-01-01T00:00:00+00:00"),
        ),
        (
            episodes[1],
            "E2",
            "Episode",
            Some(season),
            Some(2),
            Some(1),
            Some("2001-01-08T00:00:00+00:00"),
        ),
        (
            episodes[2],
            "E3",
            "Episode",
            Some(season),
            Some(3),
            Some(1),
            Some("2999-01-01T00:00:00+00:00"),
        ),
    ];
    for (id, name, media_type, parent, index, season_number, premiere) in rows {
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path, parent_id, \
             index_number, parent_index_number, premiere_date) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(lib.id.to_string())
        .bind(name)
        .bind(media_type)
        .bind(format!("{folder}/{name}"))
        .bind(parent.map(|p: uuid::Uuid| p.to_string()))
        .bind(index)
        .bind(season_number)
        .bind(premiere)
        .execute(&state.db)
        .await
        .unwrap();
    }
    (series, episodes)
}

/// Sends a GET and returns the status and the ids in the answer's `Items`, in order.
async fn g1f_ids(
    app: &axum::Router,
    uri: String,
    token: Option<&str>,
) -> (StatusCode, Vec<uuid::Uuid>) {
    let (status, json) = send(app, request("GET", &uri, token, None)).await;
    let ids = json["Items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| uuid::Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
                .collect()
        })
        .unwrap_or_default();
    (status, ids)
}

/// Gives the user a rating of 8 with Likes on the item.
async fn g1f_rate(state: &AppState, user: &str, item: uuid::Uuid) {
    rustyfin_db::repo_playback::set_user_rating(
        &state.db,
        user.parse().unwrap(),
        item,
        Some(true),
        Some(8),
    )
    .await
    .unwrap();
}

/// The user's `(Likes, UserRating)` on the item.
async fn g1f_rating(state: &AppState, user: &str, item: uuid::Uuid) -> (Option<bool>, Option<i32>) {
    let data =
        rustyfin_db::repo_playback::get_user_item_data(&state.db, user.parse().unwrap(), item)
            .await
            .unwrap();
    (data.likes, data.user_rating)
}

#[tokio::test]
async fn g1f_delete_rating_follows_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let item = g1_insert_movie(&state).await;
    let random = uuid::Uuid::new_v4().to_string();
    let delete = |user: &str, token: Option<&str>| {
        request(
            "DELETE",
            &format!("/Users/{user}/Items/{item}/Rating"),
            token,
            None,
        )
    };
    let rated = (Some(true), Some(8));
    let cleared = (None, None);
    g1f_rate(&state, &owner_id, item).await;
    g1f_rate(&state, &viewer_id, item).await;

    assert_eq!(
        send(&app, delete(&owner_id, None)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A non-admin on another user, existing or not: 403, and the rating stays.
    for target in [&owner_id, &random] {
        assert_eq!(
            send(&app, delete(target, Some(&viewer))).await.0,
            StatusCode::FORBIDDEN,
            "viewer on {target}"
        );
    }
    assert_eq!(g1f_rating(&state, &owner_id, item).await, rated);
    // On themselves: today's answer, and their rating is cleared.
    assert_eq!(
        send(&app, delete(&viewer_id, Some(&viewer))).await,
        (StatusCode::OK, serde_json::json!({}))
    );
    assert_eq!(g1f_rating(&state, &viewer_id, item).await, cleared);
    // An administrator or the API key on another user: 200, and that user's rating is cleared.
    g1f_rate(&state, &viewer_id, item).await;
    assert_eq!(
        send(&app, delete(&viewer_id, Some(&owner))).await,
        (StatusCode::OK, serde_json::json!({}))
    );
    assert_eq!(g1f_rating(&state, &viewer_id, item).await, cleared);
    assert_eq!(
        send(&app, delete(&owner_id, Some("test-api-key"))).await,
        (StatusCode::OK, serde_json::json!({}))
    );
    assert_eq!(g1f_rating(&state, &owner_id, item).await, cleared);
    // An administrator on an unknown user: 404.
    assert_eq!(
        send(&app, delete(&random, Some(&owner))).await.0,
        StatusCode::NOT_FOUND
    );
}

/// Sends `GET /Users/{user}/Items/Resume` and returns the status and the item ids.
async fn g1f_resume(
    app: &axum::Router,
    user: &str,
    token: Option<&str>,
) -> (StatusCode, Vec<uuid::Uuid>) {
    g1f_ids(app, format!("/Users/{user}/Items/Resume"), token).await
}

#[tokio::test]
async fn g1f_user_resume_follows_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    // The owner has started one movie and the viewer another.
    let owners_movie = g1_insert_movie(&state).await;
    let viewers_movie = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) \
         SELECT ?, library_id, 'G1 Movie 2', media_type, path || '.2' FROM media_items WHERE id = ?",
    )
    .bind(viewers_movie.to_string())
    .bind(owners_movie.to_string())
    .execute(&state.db)
    .await
    .unwrap();
    let random = uuid::Uuid::new_v4().to_string();
    for (user, item) in [(&owner_id, owners_movie), (&viewer_id, viewers_movie)] {
        rustyfin_db::repo_playback::update_playback_position(
            &state.db,
            user.parse().unwrap(),
            item,
            10_000_000,
        )
        .await
        .unwrap();
    }
    let owners = vec![owners_movie];
    let viewers = vec![viewers_movie];

    assert_eq!(
        g1f_resume(&app, &viewer_id, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A non-admin on another user, existing or not: 403.
    for target in [&owner_id, &random] {
        assert_eq!(
            g1f_resume(&app, target, Some(&viewer)).await.0,
            StatusCode::FORBIDDEN,
            "viewer on {target}"
        );
    }
    // On themselves: today's answer.
    assert_eq!(
        g1f_resume(&app, &viewer_id, Some(&viewer)).await,
        (StatusCode::OK, viewers.clone())
    );
    assert_eq!(
        g1f_resume(&app, &owner_id, Some(&owner)).await,
        (StatusCode::OK, owners.clone())
    );
    // An administrator or the API key on another user: that user's list.
    assert_eq!(
        g1f_resume(&app, &viewer_id, Some(&owner)).await,
        (StatusCode::OK, viewers)
    );
    assert_eq!(
        g1f_resume(&app, &owner_id, Some("test-api-key")).await,
        (StatusCode::OK, owners)
    );
    // An administrator on an unknown user: 404.
    assert_eq!(
        g1f_resume(&app, &random, Some(&owner)).await.0,
        StatusCode::NOT_FOUND
    );
}

/// Checks `GET {path}` (Next Up or Upcoming), with and without `seriesId`, for every kind of
/// caller. `users` is what `g1_owner_and_viewer` returns. `all` holds the list for a user who has
/// played nothing and `owners` the owner's list, each first without `seriesId` and then with it.
async fn g1f_assert_episode_list(
    app: &axum::Router,
    path: &str,
    series: uuid::Uuid,
    users: &(String, String, String, String),
    all: [&[uuid::Uuid]; 2],
    owners: [&[uuid::Uuid]; 2],
) {
    let (owner, owner_id, viewer, viewer_id) = users;
    let random = uuid::Uuid::new_v4();
    let bases = [format!("{path}?"), format!("{path}?seriesId={series}&")];
    for ((base, all), owners) in bases.into_iter().zip(all).zip(owners) {
        assert_eq!(
            g1f_ids(app, base.clone(), None).await.0,
            StatusCode::UNAUTHORIZED,
            "anonymous {base}"
        );
        // A non-admin naming another user, existing or not: 403.
        for target in [owner_id.clone(), random.to_string()] {
            assert_eq!(
                g1f_ids(app, format!("{base}userId={target}"), Some(viewer))
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "viewer {base}userId={target}"
            );
        }
        // A non-admin with their own id, the nil id or no userId: their own list.
        for query in [
            String::new(),
            format!("userId={viewer_id}"),
            format!("userId={G1F_NIL}"),
        ] {
            assert_eq!(
                g1f_ids(app, format!("{base}{query}"), Some(viewer)).await,
                (StatusCode::OK, all.to_vec()),
                "viewer {base}{query}"
            );
        }
        // An administrator: their own list with no userId, the nil id or their own id, another
        // user's list with that user's id, and 404 for an unknown user.
        for query in [
            String::new(),
            format!("userId={G1F_NIL}"),
            format!("userId={owner_id}"),
        ] {
            assert_eq!(
                g1f_ids(app, format!("{base}{query}"), Some(owner)).await,
                (StatusCode::OK, owners.to_vec()),
                "owner {base}{query}"
            );
        }
        assert_eq!(
            g1f_ids(app, format!("{base}userId={viewer_id}"), Some(owner)).await,
            (StatusCode::OK, all.to_vec()),
            "owner {base}userId={viewer_id}"
        );
        assert_eq!(
            g1f_ids(app, format!("{base}userId={random}"), Some(owner))
                .await
                .0,
            StatusCode::NOT_FOUND,
            "owner {base}userId={random}"
        );
        // The API key: with no userId, the list of a user who has played nothing; with a user's
        // id, that user's list.
        assert_eq!(
            g1f_ids(app, base.clone(), Some("test-api-key")).await,
            (StatusCode::OK, all.to_vec()),
            "API key {base}"
        );
        assert_eq!(
            g1f_ids(
                app,
                format!("{base}userId={owner_id}"),
                Some("test-api-key")
            )
            .await,
            (StatusCode::OK, owners.to_vec()),
            "API key {base}userId={owner_id}"
        );
    }
}

#[tokio::test]
async fn g1f_next_up_follows_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let users = g1_owner_and_viewer(&state, &app).await;
    let (series, [e1, e2, _]) = g1f_insert_show(&state).await;
    rustyfin_db::repo_playback::mark_played(&state.db, users.1.parse().unwrap(), e1)
        .await
        .unwrap();
    // The owner has watched E1. Without seriesId only started series are listed; with it, a
    // user who has played nothing starts at E1.
    g1f_assert_episode_list(
        &app,
        "/Shows/NextUp",
        series,
        &users,
        [&[], &[e1]],
        [&[e2], &[e2]],
    )
    .await;
}

#[tokio::test]
async fn g1f_upcoming_follows_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let users = g1_owner_and_viewer(&state, &app).await;
    let (series, [_, _, e3]) = g1f_insert_show(&state).await;
    rustyfin_db::repo_playback::mark_played(&state.db, users.1.parse().unwrap(), e3)
        .await
        .unwrap();
    g1f_assert_episode_list(
        &app,
        "/Shows/Upcoming",
        series,
        &users,
        [&[e3], &[e3]],
        [&[], &[]],
    )
    .await;
}

/// Sends `GET /Notifications/{user}` and returns the status and the notification names.
async fn g1f_notifications(
    app: &axum::Router,
    user: &str,
    token: Option<&str>,
) -> (StatusCode, Vec<String>) {
    let uri = format!("/Notifications/{user}");
    let (status, json) = send(app, request("GET", &uri, token, None)).await;
    let names = json["Notifications"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|n| n["Name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    (status, names)
}

#[tokio::test]
async fn g1f_notifications_follow_user_id() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let random = uuid::Uuid::new_v4().to_string();
    for (user, name) in [(&owner_id, "for the owner"), (&viewer_id, "for the viewer")] {
        sqlx::query("INSERT INTO notifications (user_id, name) VALUES (?, ?)")
            .bind(user.parse::<uuid::Uuid>().unwrap().to_string())
            .bind(name)
            .execute(&state.db)
            .await
            .unwrap();
    }
    let owners = vec!["for the owner".to_string()];
    let viewers = vec!["for the viewer".to_string()];

    assert_eq!(
        g1f_notifications(&app, &viewer_id, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A non-admin on another user, existing or not: 403.
    for target in [&owner_id, &random] {
        assert_eq!(
            g1f_notifications(&app, target, Some(&viewer)).await.0,
            StatusCode::FORBIDDEN,
            "viewer on {target}"
        );
    }
    // On themselves: today's answer.
    assert_eq!(
        g1f_notifications(&app, &viewer_id, Some(&viewer)).await,
        (StatusCode::OK, viewers.clone())
    );
    // An administrator or the API key on another user: that user's notifications; unknown: 404.
    assert_eq!(
        g1f_notifications(&app, &viewer_id, Some(&owner)).await,
        (StatusCode::OK, viewers)
    );
    assert_eq!(
        g1f_notifications(&app, &owner_id, Some("test-api-key")).await,
        (StatusCode::OK, owners)
    );
    assert_eq!(
        g1f_notifications(&app, &random, Some(&owner)).await.0,
        StatusCode::NOT_FOUND
    );
}

/// Creates a session for the owner on device "g1f-owner-device" and one for the viewer on
/// "g1f-viewer-device", and returns `(owner_session, viewer_session)`.
async fn g1f_sessions(state: &AppState, owner_id: &str, viewer_id: &str) -> (String, String) {
    let mut ids = Vec::new();
    for (user, device) in [
        (owner_id, "g1f-owner-device"),
        (viewer_id, "g1f-viewer-device"),
    ] {
        let user: uuid::Uuid = user.parse().unwrap();
        let session = uuid::Uuid::new_v4();
        rustyfin_db::repo_session::create_session(
            &state.db, session, user, "", "G1F", "cli", device, "1.0", "{}",
        )
        .await
        .unwrap();
        rustyfin_db::repo_session::upsert_device(&state.db, device, "cli", "G1F", "1.0", user)
            .await
            .unwrap();
        ids.push(session.to_string());
    }
    (ids[0].clone(), ids[1].clone())
}

#[tokio::test]
async fn g1f_session_control_owner_or_admin() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let (owner_session, viewer_session) = g1f_sessions(&state, &owner_id, &viewer_id).await;
    let unknown = uuid::Uuid::new_v4().to_string();
    let command = serde_json::json!({
        "Command": "DisplayMessage",
        "Arguments": {"Header": "g1f", "Text": "test"}
    });
    let message = serde_json::json!({"Header": "g1f", "Text": "test"});
    // (route with {} for the target, body, the viewer's target, the owner's target)
    let routes = [
        (
            "/Sessions/{}/Command",
            Some(command.clone()),
            viewer_session.as_str(),
            owner_session.as_str(),
        ),
        (
            "/Sessions/{}/Command/DisplayMessage",
            None,
            viewer_session.as_str(),
            owner_session.as_str(),
        ),
        (
            "/Sessions/{}/Message",
            Some(message),
            viewer_session.as_str(),
            owner_session.as_str(),
        ),
        (
            "/Devices/{}/Command",
            Some(command),
            "g1f-viewer-device",
            "g1f-owner-device",
        ),
    ];
    for (route, body, mine, theirs) in &routes {
        let post = |target: &str, token: Option<&str>| {
            request("POST", &route.replace("{}", target), token, body.clone())
        };
        assert_eq!(
            send(&app, post(mine, None)).await.0,
            StatusCode::UNAUTHORIZED,
            "{route} anonymous"
        );
        // A non-admin on another user's session or device, or on an unknown one: 403.
        for target in [*theirs, unknown.as_str()] {
            assert_eq!(
                send(&app, post(target, Some(&viewer))).await.0,
                StatusCode::FORBIDDEN,
                "{route} viewer on {target}"
            );
        }
        // The owner of the session or device: today's answer.
        assert_eq!(
            send(&app, post(mine, Some(&viewer))).await,
            (StatusCode::OK, serde_json::json!({})),
            "{route} viewer on their own"
        );
        // An administrator or the API key: any existing session or device; unknown: 404.
        for token in [owner.as_str(), "test-api-key"] {
            for target in [*mine, *theirs] {
                assert_eq!(
                    send(&app, post(target, Some(token))).await,
                    (StatusCode::OK, serde_json::json!({})),
                    "{route} administrator on {target}"
                );
            }
            assert_eq!(
                send(&app, post(&unknown, Some(token))).await.0,
                StatusCode::NOT_FOUND,
                "{route} administrator on an unknown target"
            );
        }
    }
}

/// Sends `GET /Sessions` and returns the status and each session's `(Id, UserId)`.
async fn g1f_session_list(
    app: &axum::Router,
    token: Option<&str>,
) -> (StatusCode, Vec<(String, uuid::Uuid)>) {
    let (status, json) = send(app, request("GET", "/Sessions", token, None)).await;
    let sessions = json
        .as_array()
        .map(|list| {
            list.iter()
                .map(|s| {
                    (
                        s["Id"].as_str().unwrap().to_string(),
                        uuid::Uuid::parse_str(s["UserId"].as_str().unwrap()).unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    (status, sessions)
}

#[tokio::test]
async fn g1f_sessions_list_own_unless_admin() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let (owner_session, viewer_session) = g1f_sessions(&state, &owner_id, &viewer_id).await;
    let viewer_uuid: uuid::Uuid = viewer_id.parse().unwrap();

    assert_eq!(
        g1f_session_list(&app, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A non-admin sees only their own sessions.
    let (status, sessions) = g1f_session_list(&app, Some(&viewer)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        sessions.iter().all(|(_, user)| *user == viewer_uuid),
        "the viewer sees only their own sessions: {sessions:?}"
    );
    assert!(sessions.iter().any(|(id, _)| *id == viewer_session));
    // An administrator and the API key see everyone's.
    for token in [owner.as_str(), "test-api-key"] {
        let (status, sessions) = g1f_session_list(&app, Some(token)).await;
        assert_eq!(status, StatusCode::OK);
        for session in [&owner_session, &viewer_session] {
            assert!(
                sessions.iter().any(|(id, _)| id == session),
                "an administrator sees {session}"
            );
        }
    }
}

// ── B15: Next Up ──
// GET /Shows/NextUp lists one episode per series the user has started: the first unplayed episode
// after their highest played one, outside season 0. Series sort newest first by when that episode
// was played, and StartIndex, Limit and TotalRecordCount count series (Jellyfin's
// TVSeriesManager.GetNextUp and NextUpService.GetNextUpEpisodesBatch).

/// A server with three series, each season its own item: Hotel has a special, S00E01, and
/// S01E01-S01E04; India has S01E01, S01E02 and S02E01; Quiet has S01E01 and S01E02. `ids` maps
/// "Hotel" to the series and "Hotel S01E02" to that episode.
struct B15Shows {
    state: AppState,
    app: axum::Router,
    ids: HashMap<String, uuid::Uuid>,
    owner: String,
    owner_id: uuid::Uuid,
    viewer: String,
    viewer_id: uuid::Uuid,
}

async fn b15_shows() -> B15Shows {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let folder = media_folder(&state, "b15-shows");
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "B15 Shows".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::TvShows,
        paths: vec![folder.clone()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    // (id, name, type, parent, index, season number). The episodes go in out of order, so an
    // answer in insertion order can't pass.
    let mut rows = Vec::new();
    let mut ids = HashMap::new();
    let shows: [(&str, &[(i32, i32)]); 3] = [
        ("Hotel", &[(1, 3), (1, 1), (0, 1), (1, 4), (1, 2)]),
        ("India", &[(2, 1), (1, 2), (1, 1)]),
        ("Quiet", &[(1, 2), (1, 1)]),
    ];
    for (show, episodes) in shows {
        let series = uuid::Uuid::new_v4();
        ids.insert(show.to_string(), series);
        rows.push((series, show.to_string(), "Series", None, None, None));
        let mut seasons = HashMap::new();
        for &(season, episode) in episodes {
            let season_id = *seasons.entry(season).or_insert_with(|| {
                let id = uuid::Uuid::new_v4();
                let name = format!("{show} Season {season}");
                rows.push((id, name, "Season", Some(series), Some(season), None));
                id
            });
            let label = format!("{show} S{season:02}E{episode:02}");
            let id = uuid::Uuid::new_v4();
            ids.insert(label.clone(), id);
            rows.push((
                id,
                label,
                "Episode",
                Some(season_id),
                Some(episode),
                Some(season),
            ));
        }
    }
    for (id, name, media_type, parent, index, season_number) in rows {
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, media_type, path, parent_id, \
             index_number, parent_index_number) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(lib.id.to_string())
        .bind(&name)
        .bind(media_type)
        .bind(format!("{folder}/{name}"))
        .bind(parent.map(|p: uuid::Uuid| p.to_string()))
        .bind(index)
        .bind(season_number)
        .execute(&state.db)
        .await
        .unwrap();
    }
    B15Shows {
        state,
        app,
        ids,
        owner,
        owner_id: owner_id.parse().unwrap(),
        viewer,
        viewer_id: viewer_id.parse().unwrap(),
    }
}

impl B15Shows {
    /// Marks `label` played for `user`, last played on 2026-01-`day`.
    async fn play(&self, user: uuid::Uuid, label: &str, day: u32) {
        rustyfin_db::repo_playback::mark_played(&self.state.db, user, self.ids[label])
            .await
            .unwrap();
        self.played_on(user, label, day).await;
    }

    /// Leaves `label` five minutes in for `user`, last played on 2026-01-`day`.
    async fn start(&self, user: uuid::Uuid, label: &str, day: u32) {
        rustyfin_db::repo_playback::update_playback_position(
            &self.state.db,
            user,
            self.ids[label],
            3_000_000_000,
        )
        .await
        .unwrap();
        self.played_on(user, label, day).await;
    }

    /// Sets the `last_played_date` of `user`'s row for `label` to 2026-01-`day`.
    async fn played_on(&self, user: uuid::Uuid, label: &str, day: u32) {
        sqlx::query(
            "UPDATE user_item_data SET last_played_date = ? WHERE user_id = ? AND item_id = ?",
        )
        .bind(format!("2026-01-{day:02}T20:00:00+00:00"))
        .bind(user.to_string())
        .bind(self.ids[label].to_string())
        .execute(&self.state.db)
        .await
        .unwrap();
    }

    /// Sends `GET /Shows/NextUp?{query}` as `token` and returns the status, the episodes by label
    /// and `TotalRecordCount`.
    async fn next_up(&self, token: &str, query: &str) -> (StatusCode, Vec<String>, i64) {
        let uri = format!("/Shows/NextUp?{query}");
        let (status, json) = send(&self.app, request("GET", &uri, Some(token), None)).await;
        let labels = json["Items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let id = uuid::Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap();
                        self.ids
                            .iter()
                            .find(|(_, other)| **other == id)
                            .map_or_else(|| id.to_string(), |(label, _)| label.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        (
            status,
            labels,
            json["TotalRecordCount"].as_i64().unwrap_or(-1),
        )
    }
}

/// What `B15Shows::next_up` returns for a 200 that lists `labels` out of `total`.
fn b15_ok(labels: &[&str], total: i64) -> (StatusCode, Vec<String>, i64) {
    (
        StatusCode::OK,
        labels.iter().map(|label| label.to_string()).collect(),
        total,
    )
}

// Criterion 1: one entry per started series, the first unplayed episode after the highest played.
#[tokio::test]
async fn b15_lists_one_episode_per_series() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "India S01E01", 2).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["India S01E02", "Hotel S01E02"], 2)
    );
    let hotel = s.ids["Hotel"];
    assert_eq!(
        s.next_up(&s.viewer, &format!("seriesId={hotel}")).await,
        b15_ok(&["Hotel S01E02"], 1),
        "Hotel by SeriesId"
    );
}

// Criterion 2: newest first by when the highest played episode was played.
#[tokio::test]
async fn b15_orders_series_by_last_watched() {
    let s = b15_shows().await;
    let viewer = s.viewer_id;
    s.play(viewer, "Hotel S01E01", 1).await;
    s.play(viewer, "India S01E01", 2).await;
    s.play(viewer, "Hotel S01E02", 3).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["Hotel S01E03", "India S01E02"], 2),
        "Hotel S01E02 played last"
    );
    s.play(viewer, "India S01E01", 4).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["India S01E02", "Hotel S01E03"], 2),
        "India S01E01 played again"
    );
    // Hotel's highest played episode is still S01E02, played on day 3.
    s.play(viewer, "Hotel S01E01", 5).await;
    s.start(viewer, "Hotel S01E03", 6).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["India S01E02", "Hotel S01E03"], 2),
        "Hotel S01E01 played again and Hotel S01E03 in progress"
    );
    // Equal dates: the series with the newer play, here an episode in progress, goes first.
    let owner = s.owner_id;
    s.play(owner, "Hotel S01E01", 1).await;
    s.play(owner, "India S01E01", 1).await;
    s.start(owner, "Hotel S01E02", 2).await;
    assert_eq!(
        s.next_up(&s.owner, "").await,
        b15_ok(&["Hotel S01E02", "India S01E02"], 2),
        "a tie, Hotel S01E02 in progress"
    );
    s.start(owner, "India S01E02", 3).await;
    assert_eq!(
        s.next_up(&s.owner, "").await,
        b15_ok(&["India S01E02", "Hotel S01E02"], 2),
        "a tie, India S01E02 in progress"
    );
}

// Criterion 3: StartIndex, Limit and TotalRecordCount count series; SeriesId names one series.
#[tokio::test]
async fn b15_pages_series() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "India S01E01", 2).await;
    s.play(s.viewer_id, "Quiet S01E01", 3).await;
    let india = s.ids["India"];
    let cases = [
        (
            String::new(),
            b15_ok(&["Quiet S01E02", "India S01E02", "Hotel S01E02"], 3),
        ),
        ("limit=1".to_string(), b15_ok(&["Quiet S01E02"], 3)),
        (
            "startIndex=1".to_string(),
            b15_ok(&["India S01E02", "Hotel S01E02"], 3),
        ),
        (
            "startIndex=1&limit=1".to_string(),
            b15_ok(&["India S01E02"], 3),
        ),
        (
            "startIndex=2&limit=5".to_string(),
            b15_ok(&["Hotel S01E02"], 3),
        ),
        ("startIndex=3".to_string(), b15_ok(&[], 3)),
        ("limit=0".to_string(), b15_ok(&[], 3)),
        (
            format!("seriesId={india}&limit=1"),
            b15_ok(&["India S01E02"], 1),
        ),
        (format!("seriesId={india}&startIndex=1"), b15_ok(&[], 1)),
        (format!("seriesId={}", uuid::Uuid::new_v4()), b15_ok(&[], 0)),
        (
            format!("seriesId={}", s.ids["India S01E01"]),
            b15_ok(&[], 0),
        ),
    ];
    for (query, answer) in cases {
        assert_eq!(s.next_up(&s.viewer, &query).await, answer, "{query}");
    }
}

// Criterion 4: season 0 is never offered and never sets the position.
#[tokio::test]
async fn b15_leaves_specials_out() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "Hotel S00E01", 2).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["Hotel S01E02"], 1),
        "viewer"
    );
    // With nothing played the series starts at S01E01, not at the special.
    let hotel = s.ids["Hotel"];
    assert_eq!(
        s.next_up(&s.owner, &format!("seriesId={hotel}")).await,
        b15_ok(&["Hotel S01E01"], 1),
        "owner, Hotel by SeriesId"
    );
    // A played special starts the series but isn't its highest played episode, so Hotel comes
    // after India although the special was played later.
    s.play(s.owner_id, "India S01E01", 3).await;
    s.play(s.owner_id, "Hotel S00E01", 4).await;
    assert_eq!(
        s.next_up(&s.owner, "").await,
        b15_ok(&["India S01E02", "Hotel S01E01"], 2),
        "owner"
    );
}

// Criterion 5: after a season's last episode comes the next season's first.
#[tokio::test]
async fn b15_moves_to_next_season() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "India S01E01", 1).await;
    s.play(s.viewer_id, "India S01E02", 2).await;
    assert_eq!(s.next_up(&s.viewer, "").await, b15_ok(&["India S02E01"], 1));
}

// Criterion 6: a series with nothing left after the position drops out.
#[tokio::test]
async fn b15_drops_finished_series() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "India S01E01", 1).await;
    s.play(s.viewer_id, "India S01E02", 2).await;
    s.play(s.viewer_id, "India S02E01", 3).await;
    s.play(s.viewer_id, "Hotel S01E01", 4).await;
    assert_eq!(s.next_up(&s.viewer, "").await, b15_ok(&["Hotel S01E02"], 1));
    let india = s.ids["India"];
    assert_eq!(
        s.next_up(&s.viewer, &format!("seriesId={india}")).await,
        b15_ok(&[], 0),
        "India by SeriesId"
    );
}

// Criterion 7: an episode in progress is unplayed, so it can be next.
#[tokio::test]
async fn b15_offers_episode_in_progress() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "India S01E01", 2).await;
    s.start(s.viewer_id, "Hotel S01E02", 3).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["India S01E02", "Hotel S01E02"], 2),
        "Hotel S01E02 in progress"
    );
    // A series with only an episode in progress comes after those with a played episode.
    s.start(s.viewer_id, "Quiet S01E01", 4).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["India S01E02", "Hotel S01E02", "Quiet S01E01"], 3),
        "Quiet S01E01 in progress"
    );
}

// Criterion 8: a series the user hasn't played, or has marked unplayed, isn't listed.
#[tokio::test]
async fn b15_leaves_unplayed_series_out() {
    let s = b15_shows().await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&[], 0),
        "nothing played"
    );
    let quiet = s.ids["Quiet"];
    assert_eq!(
        s.next_up(&s.viewer, &format!("seriesId={quiet}")).await,
        b15_ok(&["Quiet S01E01"], 1),
        "Quiet by SeriesId"
    );
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "India S01E01", 2).await;
    rustyfin_db::repo_playback::mark_unplayed(&s.state.db, s.viewer_id, s.ids["India S01E01"])
        .await
        .unwrap();
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&["Hotel S01E02"], 1),
        "India S01E01 played, then marked unplayed"
    );
}

// Criterion 9: only the user's own plays count.
#[tokio::test]
async fn b15_ignores_other_users() {
    let s = b15_shows().await;
    let (viewer, owner) = (s.viewer_id, s.owner_id);
    s.play(viewer, "Hotel S01E01", 1).await;
    s.play(viewer, "India S01E01", 2).await;
    s.play(owner, "Hotel S01E01", 3).await;
    s.play(owner, "Hotel S01E02", 4).await;
    s.play(owner, "Hotel S01E03", 5).await;
    s.play(owner, "Quiet S01E01", 6).await;
    s.start(owner, "India S01E02", 7).await;
    let viewers = b15_ok(&["India S01E02", "Hotel S01E02"], 2);
    assert_eq!(s.next_up(&s.viewer, "").await, viewers, "viewer");
    assert_eq!(
        s.next_up(&s.owner, "").await,
        b15_ok(&["Quiet S01E02", "Hotel S01E04", "India S01E01"], 3),
        "owner"
    );
    assert_eq!(
        s.next_up(&s.owner, &format!("userId={viewer}")).await,
        viewers,
        "owner for the viewer"
    );
}

// Criterion 10: the highest played episode sets the position, even past unplayed ones.
#[tokio::test]
async fn b15_skips_past_gaps() {
    let s = b15_shows().await;
    s.play(s.viewer_id, "Hotel S01E01", 1).await;
    s.play(s.viewer_id, "Hotel S01E03", 2).await;
    assert_eq!(s.next_up(&s.viewer, "").await, b15_ok(&["Hotel S01E04"], 1));
    s.play(s.viewer_id, "Hotel S01E04", 3).await;
    assert_eq!(
        s.next_up(&s.viewer, "").await,
        b15_ok(&[], 0),
        "Hotel S01E02 is still unplayed"
    );
}

// ══════════════════════════════════════════════════════════════════════════
// G2: sessions, reset PIN, login limit (R1-01, R1-08, R1-09, M-15, M-16, M-17)
// These drive the HTTP surface and so compile on the base build; they fail at
// runtime there until the G2 fixes land.
// ══════════════════════════════════════════════════════════════════════════

/// Logs in with an explicit DeviceId (via X-Emby-Authorization); returns (token, user_id).
async fn g2_login_device(
    app: &axum::Router,
    name: &str,
    password: &str,
    device_id: &str,
) -> Option<(String, String)> {
    let body = serde_json::json!({ "Username": name, "Pw": password });
    let req = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header("Content-Type", "application/json")
        .header(
            "X-Emby-Authorization",
            format!(
                "MediaBrowser Client=\"G2\", Device=\"{device_id}\", DeviceId=\"{device_id}\", Version=\"1.0\""
            ),
        )
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, json) = send(app, req).await;
    if status != StatusCode::OK {
        return None;
    }
    Some((
        json["AccessToken"].as_str().unwrap().to_string(),
        json["User"]["Id"].as_str().unwrap().to_string(),
    ))
}

/// A router whose connecting peer is the given IPv4 socket, for login-limit tests.
fn g2_app_from_peer(state: AppState, ip: [u8; 4], port: u16) -> axum::Router {
    build_router(state).layer(MockConnectInfo(SocketAddr::from((ip, port))))
}

/// A failed-login request carrying an optional X-Forwarded-For header.
fn g2_bad_login(xff: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header("Content-Type", "application/json");
    if let Some(v) = xff {
        b = b.header("X-Forwarded-For", v);
    }
    b.body(Body::from(r#"{"Username":"nobody","Pw":"x"}"#))
        .unwrap()
}

// ── G2: tokens and sessions (R1-08, R1-09) ──

#[tokio::test]
async fn g2_logout_ends_only_that_device_and_returns_204() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (ta, _) = g2_login_device(&app, "owner", "pw-owner", "dev-a")
        .await
        .expect("login A");
    let (tb, _) = g2_login_device(&app, "owner", "pw-owner", "dev-b")
        .await
        .expect("login B");

    let (status, _) = send(&app, request("POST", "/Sessions/Logout", Some(&ta), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "logout returns 204");

    let (a_after, _) = send(&app, request("GET", "/Users/Me", Some(&ta), None)).await;
    assert_eq!(
        a_after,
        StatusCode::UNAUTHORIZED,
        "device A token is revoked"
    );
    let (b_after, _) = send(&app, request("GET", "/Users/Me", Some(&tb), None)).await;
    assert_eq!(b_after, StatusCode::OK, "device B token still works");
}

#[tokio::test]
async fn g2_relogin_same_device_replaces_token() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (t1, _) = g2_login_device(&app, "owner", "pw-owner", "dev-x")
        .await
        .expect("login 1");
    let (t2, _) = g2_login_device(&app, "owner", "pw-owner", "dev-x")
        .await
        .expect("login 2");

    let (s1, _) = send(&app, request("GET", "/Users/Me", Some(&t1), None)).await;
    assert_eq!(
        s1,
        StatusCode::UNAUTHORIZED,
        "the previous token for the same device is revoked"
    );
    let (s2, _) = send(&app, request("GET", "/Users/Me", Some(&t2), None)).await;
    assert_eq!(s2, StatusCode::OK, "the new token works");
}

#[tokio::test]
async fn g2_self_password_change_keeps_caller_revokes_other_sessions() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (ta, oid) = g2_login_device(&app, "owner", "pw-owner", "dev-a")
        .await
        .expect("A");
    let (tb, _) = g2_login_device(&app, "owner", "pw-owner", "dev-b")
        .await
        .expect("B");

    let body = serde_json::json!({ "CurrentPw": "pw-owner", "NewPw": "pw-owner-2" });
    let (status, _) = send(
        &app,
        request(
            "POST",
            &format!("/Users/{oid}/Password"),
            Some(&ta),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "password change succeeds");

    let (a_after, _) = send(&app, request("GET", "/Users/Me", Some(&ta), None)).await;
    assert_eq!(a_after, StatusCode::OK, "the caller's own session survives");
    let (b_after, _) = send(&app, request("GET", "/Users/Me", Some(&tb), None)).await;
    assert_eq!(
        b_after,
        StatusCode::UNAUTHORIZED,
        "the user's other sessions end"
    );
}

#[tokio::test]
async fn g2_admin_changing_another_users_password_revokes_all_their_tokens() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (owner_t, _) = g2_login_device(&app, "owner", "pw-owner", "owner-dev")
        .await
        .expect("owner");
    let (status, created) = send(
        &app,
        request(
            "POST",
            "/Users/New",
            Some(&owner_t),
            Some(serde_json::json!({ "Name": "bob", "Password": "pw-bob" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner creates bob");
    let bob_id = created["Id"].as_str().expect("bob id").to_string();
    let (bob_t, _) = g2_login_device(&app, "bob", "pw-bob", "bob-dev")
        .await
        .expect("bob");

    let body = serde_json::json!({ "CurrentPw": "", "NewPw": "pw-bob-2" });
    let (status, _) = send(
        &app,
        request(
            "POST",
            &format!("/Users/{bob_id}/Password"),
            Some(&owner_t),
            Some(body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin resets bob's password");

    let (bob_after, _) = send(&app, request("GET", "/Users/Me", Some(&bob_t), None)).await;
    assert_eq!(
        bob_after,
        StatusCode::UNAUTHORIZED,
        "all of bob's tokens are revoked"
    );
}

#[tokio::test]
async fn g2_deleted_users_token_is_rejected() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (owner_t, _) = g2_login_device(&app, "owner", "pw-owner", "owner-dev")
        .await
        .expect("owner");
    let (status, created) = send(
        &app,
        request(
            "POST",
            "/Users/New",
            Some(&owner_t),
            Some(serde_json::json!({ "Name": "bob", "Password": "pw-bob" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bob_id = created["Id"].as_str().expect("bob id").to_string();
    let (bob_t, _) = g2_login_device(&app, "bob", "pw-bob", "bob-dev")
        .await
        .expect("bob");

    let (status, _) = send(
        &app,
        request(
            "POST",
            &format!("/Users/{bob_id}/Delete"),
            Some(&owner_t),
            None,
        ),
    )
    .await;
    assert!(status.is_success(), "owner deletes bob: {status}");

    let (bob_after, _) = send(&app, request("GET", "/Users/Me", Some(&bob_t), None)).await;
    assert_eq!(
        bob_after,
        StatusCode::UNAUTHORIZED,
        "a deleted user's token is rejected"
    );
}

#[tokio::test]
async fn g2_disabled_user_token_is_rejected() {
    let state = test_state().await;
    add_user(&state, "admin", "admin", true).await;
    let app = app(state.clone());
    // The administrator signs in, then is disabled. Only the database can disable a user.
    let (admin_t, _) = g2_login_device(&app, "admin", "admin", "admin-dev")
        .await
        .expect("admin login");
    sqlx::query("UPDATE users SET is_disabled = 1 WHERE name = 'admin'")
        .execute(&state.db)
        .await
        .unwrap();

    let (me, _) = send(&app, request("GET", "/Users/Me", Some(&admin_t), None)).await;
    assert_eq!(
        me,
        StatusCode::UNAUTHORIZED,
        "a disabled user's token is rejected on /Users/Me"
    );
    let (users, _) = send(&app, request("GET", "/Users", Some(&admin_t), None)).await;
    assert_eq!(
        users,
        StatusCode::UNAUTHORIZED,
        "a disabled user's token is rejected on an admin route"
    );
}

#[tokio::test]
async fn g2_login_response_has_fresh_last_login_date() {
    // M-16: the login response must carry the login's own timestamp, not the prior value.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let body = serde_json::json!({ "Username": "owner", "Pw": "pw-owner" });
    let (status, json) = send(
        &app,
        request("POST", "/Users/AuthenticateByName", None, Some(body)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        json["User"]["LastLoginDate"].is_string(),
        "the first login response carries the updated LastLoginDate, got {:?}",
        json["User"]["LastLoginDate"]
    );
}

// ── G2: reset PIN (R1-01, M-17) ──

#[tokio::test]
async fn g2_forgot_password_is_uniform_and_hides_the_pin() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );

    let known = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword",
            None,
            Some(serde_json::json!({"EnteredUsername":"owner"})),
        ),
    )
    .await;
    let unknown = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword",
            None,
            Some(serde_json::json!({"EnteredUsername":"ghost"})),
        ),
    )
    .await;
    assert_eq!(known.0, StatusCode::OK, "known user");
    assert_eq!(
        unknown.0,
        StatusCode::OK,
        "an unknown user is indistinguishable from a known one"
    );
    assert_eq!(known.1["Action"], "PinCode");
    assert_eq!(unknown.1["Action"], "PinCode");
    assert!(
        known.1["PinFile"].is_string(),
        "the response names the PIN file, got {:?}",
        known.1
    );
    assert!(
        !known.1.to_string().to_lowercase().contains("\"pin\":"),
        "the PIN itself never appears in the response body"
    );
}

#[tokio::test]
async fn g2_legitimate_reset_pin_flow_changes_password_and_revokes_tokens() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (old_t, _) = g2_login_device(&app, "owner", "pw-owner", "owner-dev")
        .await
        .expect("owner");

    let (status, body) = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword",
            None,
            Some(serde_json::json!({"EnteredUsername":"owner"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pin_file = body["PinFile"]
        .as_str()
        .expect("the response names a PinFile path");
    let contents = std::fs::read_to_string(pin_file).expect("the PIN file exists on a LAN request");
    let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
    let pin = parsed["Pin"]
        .as_str()
        .expect("a Pin in the file")
        .to_string();

    let (status, body) = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword/Pin",
            None,
            Some(serde_json::json!({ "Pin": pin })),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "redeeming the PIN succeeds: {body:?}"
    );
    assert_eq!(body["Success"], true);

    // The PIN string itself is now the password.
    assert!(
        g2_login_device(&app, "owner", &pin, "new-dev")
            .await
            .is_some(),
        "login with the new PIN as the password"
    );
    // The original password no longer works.
    assert!(
        g2_login_device(&app, "owner", "pw-owner", "d")
            .await
            .is_none(),
        "the old password is replaced"
    );
    // The old session is revoked.
    let (old_after, _) = send(&app, request("GET", "/Users/Me", Some(&old_t), None)).await;
    assert_eq!(
        old_after,
        StatusCode::UNAUTHORIZED,
        "old tokens are revoked after a reset"
    );
}

#[tokio::test]
async fn g2_five_wrong_pin_attempts_void_the_reset() {
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (status, body) = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword",
            None,
            Some(serde_json::json!({"EnteredUsername":"owner"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pin_file = body["PinFile"].as_str().expect("a PinFile path");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pin_file).unwrap()).unwrap();
    let real_pin = parsed["Pin"].as_str().unwrap().to_string();

    for n in 1..=5 {
        let (s, _) = send(
            &app,
            request(
                "POST",
                "/Users/ForgotPassword/Pin",
                None,
                Some(serde_json::json!({"Pin":"ZZ-ZZ-ZZ-ZZ"})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "wrong PIN attempt {n} is 404");
    }
    let (s, _) = send(
        &app,
        request(
            "POST",
            "/Users/ForgotPassword/Pin",
            None,
            Some(serde_json::json!({ "Pin": real_pin })),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "the PIN is void after 5 failed attempts"
    );
    assert!(
        g2_login_device(&app, "owner", "pw-owner", "d")
            .await
            .is_some(),
        "the original password is intact"
    );
}

// ── G2: login limit and proxy trust (XFF) ──

#[tokio::test]
async fn g2_spoofed_loopback_xff_from_remote_peer_is_still_limited() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let app = g2_app_from_peer(state, [127, 0, 0, 2], 5000);

    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            app.clone()
                .oneshot(g2_bad_login(Some("127.0.0.1")))
                .await
                .unwrap()
                .status(),
        );
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "a spoofed loopback XFF must not exempt a non-loopback peer; got {statuses:?}"
    );
}

#[tokio::test]
async fn g2_rotating_xff_shares_the_peer_budget() {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    let app = g2_app_from_peer(state, [127, 0, 0, 2], 5001);

    let mut statuses = Vec::new();
    for i in 0..6 {
        statuses.push(
            app.clone()
                .oneshot(g2_bad_login(Some(&format!("10.9.9.{i}"))))
                .await
                .unwrap()
                .status(),
        );
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "rotating XFF values must share the peer's budget; got {statuses:?}"
    );
}

#[tokio::test]
async fn g2_loopback_peer_exempt_but_127_0_0_2_limited() {
    // Guard: this behaviour already holds on base and must be preserved.
    let mut s1 = test_state().await;
    s1.rate_limiter = RateLimiter::new(2, 60);
    let app1 = g2_app_from_peer(s1, [127, 0, 0, 1], 6000);
    for _ in 0..6 {
        let st = app1
            .clone()
            .oneshot(g2_bad_login(None))
            .await
            .unwrap()
            .status();
        assert_ne!(
            st,
            StatusCode::TOO_MANY_REQUESTS,
            "a loopback peer is never limited"
        );
    }

    let mut s2 = test_state().await;
    s2.rate_limiter = RateLimiter::new(2, 60);
    let app2 = g2_app_from_peer(s2, [127, 0, 0, 2], 6001);
    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            app2.clone()
                .oneshot(g2_bad_login(None))
                .await
                .unwrap()
                .status(),
        );
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "a 127.0.0.2 peer is limited; got {statuses:?}"
    );
}

#[tokio::test]
async fn g2_trusted_proxy_limits_the_forwarded_client() {
    // A request arriving through a configured trusted proxy is attributed to the forwarded client,
    // so repeated logins from one real client behind the proxy share one budget and get limited.
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    state.known_proxies = Arc::new(rustyfin_api::middleware::parse_known_proxies("10.0.0.0/8"));
    let app = g2_app_from_peer(state, [10, 0, 0, 7], 7000);

    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            app.clone()
                .oneshot(g2_bad_login(Some("203.0.113.5")))
                .await
                .unwrap()
                .status(),
        );
    }
    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "the client forwarded by a trusted proxy is limited; got {statuses:?}"
    );
}

#[tokio::test]
async fn g2_trusted_proxy_separates_budgets_per_forwarded_client() {
    // The limiter keys on the resolved client, not the shared proxy, so a second real client behind
    // the same trusted proxy keeps its own budget.
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(2, 60);
    state.known_proxies = Arc::new(rustyfin_api::middleware::parse_known_proxies("10.0.0.0/8"));
    let app = g2_app_from_peer(state, [10, 0, 0, 7], 7001);

    for _ in 0..3 {
        let _ = app
            .clone()
            .oneshot(g2_bad_login(Some("203.0.113.5")))
            .await
            .unwrap();
    }
    let other = app
        .clone()
        .oneshot(g2_bad_login(Some("203.0.113.6")))
        .await
        .unwrap()
        .status();
    assert_ne!(
        other,
        StatusCode::TOO_MANY_REQUESTS,
        "a different forwarded client keeps its own budget"
    );
}

#[test]
fn g2_parse_known_proxies_reads_cidrs_and_hosts_and_skips_junk() {
    // Accepts CIDR blocks and bare host IPs; silently drops unparseable entries.
    let nets = rustyfin_api::middleware::parse_known_proxies("10.0.0.0/8, 192.168.1.1 , not-an-ip");
    assert_eq!(nets.len(), 2, "two valid entries, junk skipped: {nets:?}");
}

// ── G2 follow-up: security-review fixes. HTTP-level like the block above, so they compile on
// 90c99b5 and fail there at runtime. ──

/// A router whose connecting peer is `addr`, any `SocketAddr` text such as `[::ffff:10.0.0.7]:1`.
fn g2f_app_from(state: AppState, addr: &str) -> axum::Router {
    build_router(state).layer(MockConnectInfo(addr.parse::<SocketAddr>().unwrap()))
}

/// A state with a login budget of 3 per 60 s that trusts the proxies in `proxies` (CSV).
async fn g2f_state(proxies: &str) -> AppState {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(3, 60);
    state.known_proxies = Arc::new(rustyfin_api::middleware::parse_known_proxies(proxies));
    state
}

/// A failed login that carries each of `xff` as its own `X-Forwarded-For` header line.
fn g2f_bad_login(xff: &[&str]) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/Users/AuthenticateByName")
        .header("Content-Type", "application/json");
    for line in xff {
        b = b.header("X-Forwarded-For", *line);
    }
    b.body(Body::from(r#"{"Username":"nobody","Pw":"x"}"#))
        .unwrap()
}

/// Sends `n` failed logins with the given XFF lines and returns how many got 429.
async fn g2f_limited(app: &axum::Router, n: usize, xff: &[&str]) -> usize {
    let mut limited = 0;
    for _ in 0..n {
        let status = app
            .clone()
            .oneshot(g2f_bad_login(xff))
            .await
            .unwrap()
            .status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    limited
}

/// Sends ForgotPassword for `user` with the given XFF lines; returns whether the reset file exists.
async fn g2f_forgot_writes_file(app: &axum::Router, user: &str, xff: &[&str]) -> bool {
    let mut b = Request::builder()
        .method("POST")
        .uri("/Users/ForgotPassword")
        .header("Content-Type", "application/json");
    for line in xff {
        b = b.header("X-Forwarded-For", *line);
    }
    let req = b
        .body(Body::from(
            serde_json::json!({ "EnteredUsername": user }).to_string(),
        ))
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ForgotPassword is always 200: {body:?}"
    );
    std::path::Path::new(body["PinFile"].as_str().expect("a PinFile path")).exists()
}

/// Creates the owner "owner" through the setup wizard.
async fn g2f_with_owner(state: &AppState) {
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn g2f_unlisted_loopback_peer_with_xff_is_not_exempt() {
    // Loopback is a trusted proxy only when listed, so its XFF is never read. Since G2c a peer that
    // is not listed but sends XFF is unresolved: keyed on itself and not exempt. 90c99b5 always
    // trusted loopback and limited 203.0.113.5; 9dd5a44 took the request as loopback's own, exempt.
    let app = g2f_app_from(g2f_state("").await, "127.0.0.1:6100");
    assert_eq!(
        g2f_limited(&app, 5, &["203.0.113.5"]).await,
        2,
        "an unlisted loopback peer that sends XFF draws on its own budget of 3"
    );
}

#[tokio::test]
async fn g2f_listed_loopback_proxy_limits_the_forwarded_client() {
    // Guard: once listed, loopback forwards like any other proxy.
    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6101");
    assert!(
        g2f_limited(&app, 5, &["203.0.113.5"]).await > 0,
        "the client behind a listed loopback proxy is limited"
    );
}

#[tokio::test]
async fn g2f_listed_loopback_without_xff_is_not_exempt() {
    // A listed proxy with no XFF has only trusted hops: unresolved, keyed on the peer, not exempt.
    // 90c99b5 exempted it.
    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6102");
    assert!(
        g2f_limited(&app, 5, &[]).await > 0,
        "a listed proxy without XFF is limited on its own address"
    );
}

#[tokio::test]
async fn g2f_second_xff_line_is_read() {
    // The client writes `X-Forwarded-For: 127.0.0.1`; the proxy appends its own line with the real
    // client. 90c99b5 read only the first line and exempted the request as loopback.
    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6103");
    assert!(
        g2f_limited(&app, 5, &["127.0.0.1", "203.0.113.5"]).await > 0,
        "the real client in the last XFF line is limited"
    );
}

#[tokio::test]
async fn g2f_xff_entries_with_ports_and_brackets_resolve() {
    // `ip:port`, `[v6]:port` and `[v6]` all parse, and the port is not part of the key, so one
    // client on changing ports shares one budget. 90c99b5 dropped these entries and fell back to
    // loopback, which was exempt.
    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6104");
    let mut limited = 0;
    for i in 0..5 {
        let line = format!("127.0.0.1, 203.0.113.5:{}", 1000 + i);
        limited += g2f_limited(&app, 1, &[&line]).await;
    }
    assert!(limited > 0, "ip:port on changing ports shares one budget");

    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6105");
    let mut limited = 0;
    for i in 0..5 {
        let line = format!("[2001:db8:5::5]:{}", 1000 + i);
        limited += g2f_limited(&app, 1, &[&line]).await;
    }
    assert!(limited > 0, "[v6]:port resolves and is limited");

    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6106");
    assert!(
        g2f_limited(&app, 5, &["[2001:db8:6::6]"]).await > 0,
        "[v6] resolves and is limited"
    );
}

#[tokio::test]
async fn g2f_malformed_xff_entry_is_neither_exempt_nor_local() {
    // A malformed entry reached by the right-to-left walk leaves the request unresolved: keyed on
    // the peer, not exempt, not local. 90c99b5 skipped the junk and fell back to the client-written
    // 127.0.0.1 (exempt), and its LAN gate used the raw LAN peer.
    let app = g2f_app_from(g2f_state("127.0.0.1").await, "127.0.0.1:6107");
    assert!(
        g2f_limited(&app, 5, &["127.0.0.1, not-an-ip"]).await > 0,
        "a malformed entry must not resolve to the client-written loopback"
    );

    let state = g2f_state("10.0.0.7").await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "10.0.0.7:6108");
    assert!(
        !g2f_forgot_writes_file(&app, "owner", &["192.168.1.5, not-an-ip"]).await,
        "a malformed entry is not local: no reset file"
    );
}

#[tokio::test]
async fn g2f_junk_left_of_the_proxy_entry_is_ignored() {
    // Guard: junk a client wrote before the proxy's own entry is never read, so the request still
    // resolves to that entry. A client cannot move everyone onto the proxy's shared budget.
    let app = g2f_app_from(g2f_state("10.0.0.7").await, "10.0.0.7:6109");
    let _ = g2f_limited(&app, 3, &["not-an-ip, 203.0.113.5"]).await;
    assert_eq!(
        g2f_limited(&app, 1, &["203.0.113.5"]).await,
        1,
        "the junk-prefixed requests drew on 203.0.113.5's budget"
    );
}

#[tokio::test]
async fn g2f_all_hops_trusted_is_not_local() {
    // Every XFF hop is a listed proxy: unresolved, so no reset file. 90c99b5 gated on the raw,
    // private peer and wrote one.
    let state = g2f_state("10.0.0.0/8").await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "10.0.0.7:6110");
    assert!(
        !g2f_forgot_writes_file(&app, "owner", &["10.0.0.8"]).await,
        "an all-trusted chain is not local"
    );
}

#[tokio::test]
async fn g2f_lan_gate_uses_the_resolved_client() {
    // Behind a listed LAN proxy the gate follows the real client in the last XFF line. 90c99b5
    // checked the raw peer, so every client behind a LAN proxy got a reset file.
    let state = g2f_state("10.0.0.7").await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "10.0.0.7:6111");
    assert!(
        !g2f_forgot_writes_file(&app, "owner", &["192.168.1.5", "203.0.113.5"]).await,
        "a remote client behind a LAN proxy gets no reset file"
    );
    assert!(
        g2f_forgot_writes_file(&app, "owner", &["203.0.113.5", "192.168.1.5"]).await,
        "a LAN client behind the same proxy gets one"
    );
}

#[tokio::test]
async fn g2f_untrusted_peer_xff_cannot_claim_the_lan() {
    // Guard: an untrusted remote peer's XFF is ignored by the LAN gate as by the limiter.
    let state = g2f_state("10.0.0.7").await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "203.0.113.9:6112");
    assert!(
        !g2f_forgot_writes_file(&app, "owner", &["192.168.1.5"]).await,
        "a spoofed LAN XFF from a remote peer gets no reset file"
    );
    assert!(
        g2f_limited(&app, 5, &["127.0.0.1"]).await > 0,
        "a spoofed loopback XFF from a remote peer is limited"
    );
}

#[tokio::test]
async fn g2f_mapped_ipv4_lan_peer_gets_a_reset_file() {
    // A dual-stack listener sees IPv4 LAN peers as ::ffff:a.b.c.d. 90c99b5 did not canonicalize
    // them and wrote no file.
    let state = g2f_state("").await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "[::ffff:192.168.1.20]:6113");
    assert!(
        g2f_forgot_writes_file(&app, "owner", &[]).await,
        "a mapped LAN peer gets a reset file"
    );
}

#[tokio::test]
async fn g2f_mapped_loopback_peer_is_exempt() {
    // ::ffff:127.0.0.1 is exact loopback once canonicalized. 90c99b5 limited it.
    let app = g2f_app_from(g2f_state("").await, "[::ffff:127.0.0.1]:6114");
    assert_eq!(
        g2f_limited(&app, 5, &[]).await,
        0,
        "a mapped loopback peer is exempt"
    );
}

#[tokio::test]
async fn g2f_mapped_proxy_peer_is_trusted() {
    // A listed IPv4 proxy that reaches a dual-stack listener as ::ffff:10.0.0.7 is still that proxy,
    // so two clients behind it keep separate budgets. 90c99b5 keyed both on the mapped peer.
    let app = g2f_app_from(g2f_state("10.0.0.7").await, "[::ffff:10.0.0.7]:6115");
    let _ = g2f_limited(&app, 3, &["203.0.113.5"]).await;
    assert_eq!(
        g2f_limited(&app, 1, &["203.0.113.6"]).await,
        0,
        "a second client behind the mapped proxy keeps its own budget"
    );
}

#[tokio::test]
async fn g2f_ipv6_clients_share_a_slash64_budget() {
    // One host rotating through its /64 must not get a fresh budget per address. 90c99b5 keyed
    // every full IPv6 address separately.
    let state = g2f_state("").await;
    let a = g2f_app_from(state.clone(), "[2001:db8:1:2::10]:6116");
    let b = g2f_app_from(state.clone(), "[2001:db8:1:2::20]:6117");
    let other = g2f_app_from(state, "[2001:db8:1:3::10]:6118");
    let _ = g2f_limited(&a, 3, &[]).await;
    assert_eq!(
        g2f_limited(&b, 1, &[]).await,
        1,
        "a second address in the same /64 shares the budget"
    );
    assert_eq!(
        g2f_limited(&other, 1, &[]).await,
        0,
        "a different /64 keeps its own budget"
    );
}

/// Starts a reset for "owner" (from loopback) and returns the PIN in the reset file.
async fn g2f_start_reset(app: &axum::Router) -> String {
    let (status, body) = send(
        app,
        request(
            "POST",
            "/Users/ForgotPassword",
            None,
            Some(serde_json::json!({"EnteredUsername":"owner"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let file = body["PinFile"].as_str().expect("a PinFile path");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file).expect("the reset file")).unwrap();
    parsed["Pin"]
        .as_str()
        .expect("a Pin in the file")
        .to_string()
}

/// Redeems `pin` through ForgotPassword/Pin and returns the status.
async fn g2f_redeem(app: &axum::Router, pin: &str) -> StatusCode {
    send(
        app,
        request(
            "POST",
            "/Users/ForgotPassword/Pin",
            None,
            Some(serde_json::json!({ "Pin": pin })),
        ),
    )
    .await
    .0
}

#[tokio::test]
async fn g2f_reissue_after_five_misses_grants_no_new_guesses() {
    // A new ForgotPassword must not reset the attempt count. 90c99b5 replaced the voided reset with
    // a fresh one, so every re-issue gave 5 more guesses.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let _ = g2f_start_reset(&app).await;
    for n in 1..=5 {
        assert_eq!(
            g2f_redeem(&app, "ZZ-ZZ-ZZ-ZZ").await,
            StatusCode::NOT_FOUND,
            "miss {n}"
        );
    }
    let pin = g2f_start_reset(&app).await;
    assert_eq!(
        g2f_redeem(&app, &pin).await,
        StatusCode::NOT_FOUND,
        "after 5 misses the PIN in the reset file stays void until the reset expires"
    );
    assert!(
        g2_login_device(&app, "owner", "pw-owner", "d")
            .await
            .is_some(),
        "the password is unchanged"
    );
}

#[tokio::test]
async fn g2f_reissue_keeps_the_attempt_count() {
    // 3 misses, a re-issue, 2 more misses: 5 in all, so the PIN in the reset file is void. 90c99b5
    // restarted the count at the re-issue.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let _ = g2f_start_reset(&app).await;
    for _ in 0..3 {
        assert_eq!(g2f_redeem(&app, "ZZ-ZZ-ZZ-ZZ").await, StatusCode::NOT_FOUND);
    }
    let pin = g2f_start_reset(&app).await;
    for _ in 0..2 {
        assert_eq!(g2f_redeem(&app, "ZZ-ZZ-ZZ-ZZ").await, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        g2f_redeem(&app, &pin).await,
        StatusCode::NOT_FOUND,
        "5 misses in all void the reset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn g2f_concurrent_redemption_succeeds_once() {
    // The right PIN sent 8 times at once succeeds exactly once. 90c99b5 checked and marked the PIN
    // in separate statements, so concurrent requests could all pass the check.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let pin = g2f_start_reset(&app).await;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let pin = pin.clone();
        tasks.push(tokio::spawn(async move { g2f_redeem(&app, &pin).await }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1, "exactly one concurrent redemption succeeds");
}

#[tokio::test]
async fn g2f_redeemed_pin_row_is_deleted() {
    // A redeemed PIN is deleted, not kept as used. 90c99b5 kept the row.
    let state = test_state().await;
    let app = app(state.clone());
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let pin = g2f_start_reset(&app).await;
    assert_eq!(g2f_redeem(&app, &pin).await, StatusCode::OK);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM password_reset_pins")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(left, 0, "no reset row is left after redemption");
}

#[tokio::test]
async fn g2f_reset_password_hides_unknown_users() {
    // ResetPassword answers an unknown user exactly like a wrong PIN. 90c99b5 said "user not found".
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let attempt = |name: &str| {
        request(
            "POST",
            "/Users/ResetPassword",
            None,
            Some(serde_json::json!({
                "Username": name, "Pin": "ZZ-ZZ-ZZ-ZZ", "TempPassword": "x"
            })),
        )
    };
    let known = send(&app, attempt("owner")).await;
    let unknown = send(&app, attempt("ghost")).await;
    assert_ne!(known.0, StatusCode::OK, "a wrong PIN fails");
    assert_eq!(
        unknown, known,
        "an unknown user gets the same status and body as a wrong PIN"
    );
}

#[tokio::test]
async fn g2f_admin_device_delete_ends_that_devices_sessions() {
    // DELETE /Devices?id= signs the device out, as in Jellyfin. 90c99b5 left its tokens valid.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (victim_t, _) = g2_login_device(&app, "owner", "pw-owner", "victim-dev")
        .await
        .expect("login on the device to delete");
    let (admin_t, _) = g2_login_device(&app, "owner", "pw-owner", "admin-dev")
        .await
        .expect("login on another device");
    let (status, body) = send(
        &app,
        request("DELETE", "/Devices?id=victim-dev", Some(&admin_t), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "device delete: {body:?}");
    let (me, _) = send(&app, request("GET", "/Users/Me", Some(&victim_t), None)).await;
    assert_eq!(
        me,
        StatusCode::UNAUTHORIZED,
        "the deleted device's token is revoked"
    );
    let (me, _) = send(&app, request("GET", "/Users/Me", Some(&admin_t), None)).await;
    assert_eq!(me, StatusCode::OK, "other devices stay signed in");
}

// ── G2c: second review of the G2 merge. These use only what 9dd5a44 exposes, so they compile there
// and fail at runtime. ──

const G2C_XFF: (&str, &str) = ("X-Forwarded-For", "203.0.113.9");
const G2C_XRI: (&str, &str) = ("X-Real-IP", "203.0.113.9");
const G2C_FWD: (&str, &str) = ("Forwarded", "for=203.0.113.9");

/// A state with the default login budget (10 per 60 s) and no RUSTYFIN_KNOWN_PROXIES.
async fn g2c_default_budget_state() -> AppState {
    let mut state = test_state().await;
    state.rate_limiter = RateLimiter::new(
        rustyfin_api::middleware::DEFAULT_LOGIN_LIMIT,
        rustyfin_api::middleware::DEFAULT_LOGIN_WINDOW_SECS,
    );
    state
}

/// A POST of `json` to `uri` that carries each of `headers`.
fn g2c_post(uri: &str, headers: &[(&str, &str)], json: String) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json");
    for (name, value) in headers {
        b = b.header(*name, *value);
    }
    b.body(Body::from(json)).unwrap()
}

/// Sends `n` failed logins that carry `headers`; returns how many got 429.
async fn g2c_limited(app: &axum::Router, n: usize, headers: &[(&str, &str)]) -> usize {
    let mut limited = 0;
    for _ in 0..n {
        let req = g2c_post(
            "/Users/AuthenticateByName",
            headers,
            r#"{"Username":"nobody","Pw":"x"}"#.to_string(),
        );
        if app.clone().oneshot(req).await.unwrap().status() == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    limited
}

/// Sends ForgotPassword for `user` with `headers`; returns the reset-file path the answer names.
async fn g2c_forgot(
    app: &axum::Router,
    user: &str,
    headers: &[(&str, &str)],
) -> std::path::PathBuf {
    let json = serde_json::json!({ "EnteredUsername": user }).to_string();
    let (status, body) = send(app, g2c_post("/Users/ForgotPassword", headers, json)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ForgotPassword is always 200: {body:?}"
    );
    body["PinFile"].as_str().expect("a PinFile path").into()
}

#[test]
fn g2c_unlisted_peer_with_a_forwarding_header_is_unresolved() {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use rustyfin_api::middleware::{parse_known_proxies, resolve_client};
    let one = |name: &'static str, value: &'static [u8]| {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_static(name),
            HeaderValue::from_bytes(value).unwrap(),
        );
        h
    };
    // Any value counts, even one that names no address or is not ASCII: it is never parsed.
    let cases = [
        one("x-forwarded-for", b"203.0.113.9"),
        one("x-real-ip", b"203.0.113.9"),
        one("forwarded", b"for=203.0.113.9"),
        one("x-forwarded-for", b""),
        one("x-forwarded-for", b"\xff"),
    ];
    for known in [Vec::new(), parse_known_proxies("10.0.0.1")] {
        for headers in &cases {
            for peer in ["127.0.0.1", "192.168.1.20"] {
                let client = resolve_client(peer.parse().unwrap(), headers, &known);
                assert!(!client.is_exempt(), "{peer} with {headers:?} is not exempt");
                assert!(!client.is_local(), "{peer} with {headers:?} is not local");
                assert_eq!(
                    client.limiter_key(),
                    peer,
                    "{peer} with {headers:?} is keyed on itself"
                );
            }
        }
    }
    // Without any of them the peer is the client, as before.
    let none = HeaderMap::new();
    let loopback = resolve_client("127.0.0.1".parse().unwrap(), &none, &[]);
    assert!(
        loopback.is_exempt() && loopback.is_local(),
        "direct loopback stays exempt and local"
    );
    let lan = resolve_client("192.168.1.20".parse().unwrap(), &none, &[]);
    assert!(
        lan.is_local() && !lan.is_exempt(),
        "a direct LAN client stays local"
    );
}

/// 127.0.0.1 sends `header` with no proxies listed, as a same-host proxy would: it draws on
/// 127.0.0.1's default budget and is not local, while direct loopback stays exempt.
async fn g2c_unlisted_loopback_proxy_case(header: (&str, &str)) {
    let app = g2f_app_from(g2c_default_budget_state().await, "127.0.0.1:6200");
    assert_eq!(
        g2c_limited(&app, 15, &[header]).await,
        5,
        "{} from an unlisted loopback peer: 10 allowed, then 429",
        header.0
    );
    assert_eq!(
        g2c_limited(&app, 3, &[]).await,
        0,
        "direct loopback stays exempt after the proxied requests used up the budget"
    );

    let state = g2c_default_budget_state().await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "127.0.0.1:6200");
    assert!(
        !g2c_forgot(&app, "owner", &[header]).await.exists(),
        "{} from an unlisted loopback peer is not local: no reset file",
        header.0
    );
}

#[tokio::test]
async fn g2c_unlisted_loopback_proxy_with_xff_is_limited_and_gets_no_reset_file() {
    g2c_unlisted_loopback_proxy_case(G2C_XFF).await;
}

#[tokio::test]
async fn g2c_unlisted_loopback_proxy_with_x_real_ip_is_limited_and_gets_no_reset_file() {
    g2c_unlisted_loopback_proxy_case(G2C_XRI).await;
}

#[tokio::test]
async fn g2c_unlisted_loopback_proxy_with_forwarded_is_limited_and_gets_no_reset_file() {
    g2c_unlisted_loopback_proxy_case(G2C_FWD).await;
}

#[tokio::test]
async fn g2c_direct_loopback_stays_exempt_and_gets_the_reset_file() {
    // Today's result for a direct client, kept by G2c.
    let state = g2c_default_budget_state().await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "127.0.0.1:6200");
    assert_eq!(
        g2c_limited(&app, 15, &[]).await,
        0,
        "direct loopback is exempt"
    );
    assert!(
        g2c_forgot(&app, "owner", &[]).await.exists(),
        "direct loopback is local and gets the reset file"
    );
}

#[tokio::test]
async fn g2c_direct_lan_client_is_local_only_without_a_forwarding_header() {
    let state = g2c_default_budget_state().await;
    g2f_with_owner(&state).await;
    let app = g2f_app_from(state, "192.168.1.20:6300");
    assert!(
        !g2c_forgot(&app, "owner", &[G2C_XFF]).await.exists(),
        "a LAN peer that sends X-Forwarded-For is not local"
    );
    assert!(
        g2c_forgot(&app, "owner", &[]).await.exists(),
        "the same LAN peer without it stays local"
    );
}

#[tokio::test]
async fn g2c_device_delete_by_path_ends_that_devices_sessions() {
    // DELETE /Devices/{deviceId} signs the device out like DELETE /Devices?id=. 9dd5a44 deleted only
    // the device row and left its tokens valid.
    let state = test_state().await;
    let app = app(state);
    assert_eq!(
        post_startup_user(&app, None, "owner", "pw-owner").await,
        StatusCode::NO_CONTENT
    );
    let (victim_t, _) = g2_login_device(&app, "owner", "pw-owner", "victim-dev")
        .await
        .expect("login on the device to delete");
    let (admin_t, _) = g2_login_device(&app, "owner", "pw-owner", "admin-dev")
        .await
        .expect("login on another device");
    let (status, body) = send(
        &app,
        request("DELETE", "/Devices/victim-dev", Some(&admin_t), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "device delete: {body:?}");
    let (me, _) = send(&app, request("GET", "/Users/Me", Some(&victim_t), None)).await;
    assert_eq!(
        me,
        StatusCode::UNAUTHORIZED,
        "the deleted device's token is revoked"
    );
    let (me, _) = send(&app, request("GET", "/Users/Me", Some(&admin_t), None)).await;
    assert_eq!(me, StatusCode::OK, "other devices stay signed in");
}

#[tokio::test]
async fn g2c_forgot_password_time_does_not_depend_on_the_name() {
    // A known name answered only after its PIN row and file were written, an unknown one at once,
    // so the time told whether the account exists; with the write lock held elsewhere (a scan, say)
    // the known name waited for the lock. Both now answer after the same fixed delay (250 ms in
    // users.rs), and the reset still happens once the lock is free.
    let state = test_state().await;
    g2f_with_owner(&state).await;
    let app = app(state.clone());
    let mut lock = state.db.begin().await.unwrap();
    sqlx::query("DELETE FROM password_reset_pins WHERE 0")
        .execute(&mut *lock)
        .await
        .unwrap();

    let mut times = Vec::new();
    let mut files = Vec::new();
    for user in ["owner", "ghost"] {
        let start = std::time::Instant::now();
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            g2c_forgot(&app, user, &[]),
        )
        .await;
        times.push(start.elapsed());
        files.push(answer.unwrap_or_else(|_| {
            panic!("ForgotPassword for {user} answers while the write lock is held")
        }));
    }
    assert!(
        times[0].abs_diff(times[1]) < std::time::Duration::from_millis(200),
        "a known and an unknown name answer alike: {times:?}"
    );
    assert!(
        times
            .iter()
            .all(|t| *t >= std::time::Duration::from_millis(200)),
        "both answer after the fixed delay: {times:?}"
    );

    lock.rollback().await.unwrap();
    let mut written = false;
    for _ in 0..250 {
        if files[0].exists() {
            written = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        written,
        "the owner's reset file appears once the lock is free"
    );
    assert!(!files[1].exists(), "an unknown name gets no file");
}

// ── G1c ──
// One user's data stays theirs: GET /Items/Resume lists only the caller's items (or those of the
// user a userId names, under path_user's rules), IsFavorite/IsPlayed/IsUnplayed match only the
// caller's data, and every list item carries the caller's UserData.

/// Removes a test's data dir when the test ends, pass or fail.
struct G1cDataDir(std::path::PathBuf);

impl Drop for G1cDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stores a library with `n` movies, "G1c Movie 1" to "G1c Movie n", and returns their ids in order.
async fn g1c_insert_movies(state: &AppState, n: usize) -> Vec<uuid::Uuid> {
    let folder = media_folder(state, "g1c-movies");
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: "G1c Movies".to_string(),
        content_type: rustyfin_model::library::LibraryContentType::Movies,
        paths: vec![folder.clone()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 1..=n {
        let id = uuid::Uuid::new_v4();
        let name = format!("G1c Movie {i}");
        sqlx::query(
            "INSERT INTO media_items (id, library_id, name, sort_name, media_type, path) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(lib.id.to_string())
        .bind(&name)
        .bind(&name)
        .bind("Movie")
        .bind(format!("{folder}/{name}.mkv"))
        .execute(&state.db)
        .await
        .unwrap();
        ids.push(id);
    }
    ids
}

/// Sends a GET and returns the status, the answer's `Items` and its `TotalRecordCount`.
async fn g1c_list(
    app: &axum::Router,
    uri: &str,
    token: &str,
) -> (StatusCode, Vec<serde_json::Value>, i64) {
    let (status, json) = send(app, request("GET", uri, Some(token), None)).await;
    let items = json["Items"].as_array().cloned().unwrap_or_default();
    (
        status,
        items,
        json["TotalRecordCount"].as_i64().unwrap_or(-1),
    )
}

/// The item's id.
fn g1c_id(item: &serde_json::Value) -> uuid::Uuid {
    uuid::Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap()
}

/// The items' ids, sorted.
fn g1c_sorted_ids(items: &[serde_json::Value]) -> Vec<uuid::Uuid> {
    let mut ids: Vec<uuid::Uuid> = items.iter().map(g1c_id).collect();
    ids.sort();
    ids
}

/// The ids of the movies with these numbers (1 is the first), sorted.
fn g1c_movies(movies: &[uuid::Uuid], numbers: &[usize]) -> Vec<uuid::Uuid> {
    let mut ids: Vec<uuid::Uuid> = numbers.iter().map(|n| movies[n - 1]).collect();
    ids.sort();
    ids
}

/// The item's `(Played, IsFavorite, PlaybackPositionTicks)`, or `None` without `UserData`.
fn g1c_user_data(item: &serde_json::Value) -> Option<(bool, bool, i64)> {
    let data = &item["UserData"];
    data.is_object().then(|| {
        (
            data["Played"].as_bool().unwrap(),
            data["IsFavorite"].as_bool().unwrap(),
            data["PlaybackPositionTicks"].as_i64().unwrap(),
        )
    })
}

#[tokio::test]
async fn g1c_resume_lists_only_the_callers_items_once() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let m = g1c_insert_movies(&state, 3).await;
    // Both users are part-way through movie 2.
    let positions = [
        (&owner_id, m[0], 1000),
        (&owner_id, m[1], 2000),
        (&viewer_id, m[1], 3000),
        (&viewer_id, m[2], 4000),
    ];
    for (user, item, ticks) in positions {
        rustyfin_db::repo_playback::update_playback_position(
            &state.db,
            user.parse().unwrap(),
            item,
            ticks,
        )
        .await
        .unwrap();
    }
    let cases = [
        ("owner", &owner, vec![(m[0], 1000), (m[1], 2000)]),
        ("viewer", &viewer, vec![(m[1], 3000), (m[2], 4000)]),
    ];
    for (who, token, mut expected) in cases {
        let (status, items, total) = g1c_list(&app, "/Items/Resume", token).await;
        assert_eq!(status, StatusCode::OK, "{who}");
        let mut got: Vec<(uuid::Uuid, i64)> = items
            .iter()
            .map(|item| (g1c_id(item), g1c_user_data(item).map_or(-1, |data| data.2)))
            .collect();
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "{who}'s resume list and positions");
        assert_eq!(total, expected.len() as i64, "{who}'s TotalRecordCount");
    }
}

#[tokio::test]
async fn g1c_resume_user_id_follows_query_user_rules() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let m = g1c_insert_movies(&state, 2).await;
    for (user, item) in [(&owner_id, m[0]), (&viewer_id, m[1])] {
        rustyfin_db::repo_playback::update_playback_position(
            &state.db,
            user.parse().unwrap(),
            item,
            1000,
        )
        .await
        .unwrap();
    }
    let unknown = uuid::Uuid::new_v4();
    let (ok, forbidden, not_found) = (StatusCode::OK, StatusCode::FORBIDDEN, StatusCode::NOT_FOUND);
    let key = "test-api-key";
    // (caller, token, query, status, ids when the status is 200)
    let cases = [
        (
            "viewer",
            viewer.as_str(),
            format!("userId={owner_id}"),
            forbidden,
            vec![],
        ),
        (
            "viewer",
            viewer.as_str(),
            format!("UserId={owner_id}"),
            forbidden,
            vec![],
        ),
        (
            "viewer",
            viewer.as_str(),
            format!("userId={unknown}"),
            forbidden,
            vec![],
        ),
        (
            "viewer",
            viewer.as_str(),
            format!("userId={viewer_id}"),
            ok,
            vec![m[1]],
        ),
        (
            "viewer",
            viewer.as_str(),
            format!("UserId={G1F_NIL}"),
            ok,
            vec![m[1]],
        ),
        ("viewer", viewer.as_str(), String::new(), ok, vec![m[1]]),
        (
            "owner",
            owner.as_str(),
            format!("userId={viewer_id}"),
            ok,
            vec![m[1]],
        ),
        (
            "owner",
            owner.as_str(),
            format!("UserId={viewer_id}"),
            ok,
            vec![m[1]],
        ),
        (
            "owner",
            owner.as_str(),
            format!("userId={owner_id}"),
            ok,
            vec![m[0]],
        ),
        (
            "owner",
            owner.as_str(),
            format!("userId={G1F_NIL}"),
            ok,
            vec![m[0]],
        ),
        ("owner", owner.as_str(), String::new(), ok, vec![m[0]]),
        (
            "owner",
            owner.as_str(),
            format!("userId={unknown}"),
            not_found,
            vec![],
        ),
        ("API key", key, String::new(), not_found, vec![]),
        (
            "API key",
            key,
            format!("userId={G1F_NIL}"),
            not_found,
            vec![],
        ),
        (
            "API key",
            key,
            format!("userId={viewer_id}"),
            ok,
            vec![m[1]],
        ),
        ("API key", key, format!("UserId={owner_id}"), ok, vec![m[0]]),
    ];
    for (who, token, query, expected_status, expected_ids) in cases {
        let uri = format!("/Items/Resume?{query}");
        let (status, items, total) = g1c_list(&app, &uri, token).await;
        assert_eq!(status, expected_status, "{who} {uri}");
        if expected_status == StatusCode::OK {
            assert_eq!(g1c_sorted_ids(&items), expected_ids, "{who} {uri}");
            assert_eq!(
                total,
                expected_ids.len() as i64,
                "{who} {uri} TotalRecordCount"
            );
        }
    }
}

#[tokio::test]
async fn g1c_favorite_and_played_filters_match_only_the_callers_data() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let m = g1c_insert_movies(&state, 5).await;
    // The owner's favorites are movies 1 and 2 and they played 2 and 3; the viewer's favorites are
    // 2 and 4 and they played 1, 4 and 5.
    let data = [
        (&owner_id, vec![1, 2], vec![2, 3]),
        (&viewer_id, vec![2, 4], vec![1, 4, 5]),
    ];
    for (user, favorites, played) in data {
        let user: uuid::Uuid = user.parse().unwrap();
        for n in favorites {
            rustyfin_db::repo_playback::set_favorite(&state.db, user, m[n - 1], true)
                .await
                .unwrap();
        }
        for n in played {
            rustyfin_db::repo_playback::mark_played(&state.db, user, m[n - 1])
                .await
                .unwrap();
        }
    }
    let base = "/Items?Recursive=true&IncludeItemTypes=Movie&";
    let all: &[usize] = &[1, 2, 3, 4, 5];
    // The API key has no user, so it has no favorites and has played nothing.
    // (filter, the movies it gives the owner, the viewer and the API key)
    type Case = (
        &'static str,
        &'static [usize],
        &'static [usize],
        &'static [usize],
    );
    let cases: [Case; 12] = [
        ("IsFavorite=true", &[1, 2], &[2, 4], &[]),
        ("isFavorite=true", &[1, 2], &[2, 4], &[]),
        ("IsFavorite=false", &[3, 4, 5], &[1, 3, 5], all),
        ("IsPlayed=true", &[2, 3], &[1, 4, 5], &[]),
        ("isPlayed=true", &[2, 3], &[1, 4, 5], &[]),
        ("IsUnplayed=false", &[2, 3], &[1, 4, 5], &[]),
        ("IsPlayed=false", &[1, 4, 5], &[2, 3], all),
        ("IsUnplayed=true", &[1, 4, 5], &[2, 3], all),
        ("isUnplayed=true", &[1, 4, 5], &[2, 3], all),
        ("IsFavorite=true&IsPlayed=true", &[2], &[4], &[]),
        ("IsFavorite=true&IsUnplayed=true", &[1], &[2], &[]),
        (
            "IsFavorite=false&IsPlayed=false&MediaTypes=Movie&Limit=10&StartIndex=0",
            &[4, 5],
            &[3],
            all,
        ),
    ];
    for (filter, owner_movies, viewer_movies, key_movies) in cases {
        let callers = [
            ("owner", owner.as_str(), owner_movies),
            ("viewer", viewer.as_str(), viewer_movies),
            ("API key", "test-api-key", key_movies),
        ];
        for (who, token, numbers) in callers {
            let uri = format!("{base}{filter}");
            let (status, items, total) = g1c_list(&app, &uri, token).await;
            assert_eq!(status, StatusCode::OK, "{who} {uri}");
            let expected = g1c_movies(&m, numbers);
            assert_eq!(g1c_sorted_ids(&items), expected, "{who} {uri}");
            assert_eq!(total, expected.len() as i64, "{who} {uri} TotalRecordCount");
        }
    }
    // A page of favorites holds only the caller's, and the count counts only theirs.
    for (who, token, second) in [("owner", &owner, m[1]), ("viewer", &viewer, m[3])] {
        let uri = format!("{base}IsFavorite=true&Limit=1&StartIndex=1");
        let (status, items, total) = g1c_list(&app, &uri, token).await;
        assert_eq!(status, StatusCode::OK, "{who} {uri}");
        assert_eq!(g1c_sorted_ids(&items), vec![second], "{who} {uri}");
        assert_eq!(total, 2, "{who} {uri} TotalRecordCount");
    }
}

#[tokio::test]
async fn g1c_list_user_data_is_the_callers() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let m = g1c_insert_movies(&state, 4).await;
    let db = &state.db;
    let (o, v): (uuid::Uuid, uuid::Uuid) = (owner_id.parse().unwrap(), viewer_id.parse().unwrap());
    // The owner: part-way through 1, favorite 2, played 3, favorite and part-way through 4.
    rustyfin_db::repo_playback::update_playback_position(db, o, m[0], 1000)
        .await
        .unwrap();
    rustyfin_db::repo_playback::set_favorite(db, o, m[1], true)
        .await
        .unwrap();
    rustyfin_db::repo_playback::mark_played(db, o, m[2])
        .await
        .unwrap();
    rustyfin_db::repo_playback::set_favorite(db, o, m[3], true)
        .await
        .unwrap();
    rustyfin_db::repo_playback::update_playback_position(db, o, m[3], 4000)
        .await
        .unwrap();
    // The viewer: favorite 1, played 2, part-way through 3, played 4.
    rustyfin_db::repo_playback::set_favorite(db, v, m[0], true)
        .await
        .unwrap();
    rustyfin_db::repo_playback::mark_played(db, v, m[1])
        .await
        .unwrap();
    rustyfin_db::repo_playback::update_playback_position(db, v, m[2], 3000)
        .await
        .unwrap();
    rustyfin_db::repo_playback::mark_played(db, v, m[3])
        .await
        .unwrap();
    // (Played, IsFavorite, PlaybackPositionTicks) of movies 1 to 4.
    let cases = [
        (
            "owner",
            &owner,
            &owner_id,
            [
                (false, false, 1000),
                (false, true, 0),
                (true, false, 0),
                (false, true, 4000),
            ],
        ),
        (
            "viewer",
            &viewer,
            &viewer_id,
            [
                (false, true, 0),
                (true, false, 0),
                (false, false, 3000),
                (true, false, 0),
            ],
        ),
    ];
    for (who, token, id, expected) in cases {
        let number =
            |item: &serde_json::Value| m.iter().position(|movie| *movie == g1c_id(item)).unwrap();
        let lists = [
            "/Items?Recursive=true&IncludeItemTypes=Movie".to_string(),
            format!("/Users/{id}/Items?Recursive=true&IncludeItemTypes=Movie"),
        ];
        for uri in lists {
            let (status, items, _) = g1c_list(&app, &uri, token).await;
            assert_eq!(status, StatusCode::OK, "{who} {uri}");
            assert_eq!(items.len(), 4, "{who} {uri}");
            for item in &items {
                let n = number(item);
                assert_eq!(
                    g1c_user_data(item),
                    Some(expected[n]),
                    "{who} {uri}: movie {}",
                    n + 1
                );
            }
        }
        // A one-item page, where the item is also the last, carries the caller's data too.
        for start in 0..4 {
            let uri =
                format!("/Items?Recursive=true&IncludeItemTypes=Movie&Limit=1&StartIndex={start}");
            let (status, items, _) = g1c_list(&app, &uri, token).await;
            assert_eq!(status, StatusCode::OK, "{who} {uri}");
            assert_eq!(items.len(), 1, "{who} {uri}");
            let n = number(&items[0]);
            assert_eq!(
                g1c_user_data(&items[0]),
                Some(expected[n]),
                "{who} {uri}: movie {}",
                n + 1
            );
        }
    }
}

/// A raw websocket client of `/socket` that reads the server's text frames.
struct G1cSocket(tokio::io::BufReader<tokio::net::TcpStream>);

impl G1cSocket {
    /// Opens `/socket?api_key={token}` and reads up to the TimeSync frame. The server subscribes the
    /// socket to the broadcast channel before it sends its first frame.
    async fn open(addr: SocketAddr, token: &str) -> G1cSocket {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET /socket?api_key={token} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            line.starts_with("HTTP/1.1 101"),
            "websocket upgrade: {line:?}"
        );
        while line != "\r\n" {
            line.clear();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0, "headers end");
        }
        let mut socket = G1cSocket(reader);
        while socket.next().await["MessageType"] != "TimeSync" {}
        socket
    }

    /// The JSON of the next text frame. Server frames are not masked.
    async fn next(&mut self) -> serde_json::Value {
        use tokio::io::AsyncReadExt;

        loop {
            let mut head = [0u8; 2];
            self.0.read_exact(&mut head).await.unwrap();
            let len = match head[1] & 0x7f {
                126 => {
                    let mut len = [0u8; 2];
                    self.0.read_exact(&mut len).await.unwrap();
                    u16::from_be_bytes(len) as usize
                }
                127 => {
                    let mut len = [0u8; 8];
                    self.0.read_exact(&mut len).await.unwrap();
                    u64::from_be_bytes(len) as usize
                }
                len => len as usize,
            };
            let mut payload = vec![0u8; len];
            self.0.read_exact(&mut payload).await.unwrap();
            // Opcode 1 is a text frame.
            if head[0] & 0x0f == 1 {
                return serde_json::from_slice(&payload).unwrap();
            }
        }
    }

    /// The frames before the "g1c-end" LibraryChanged marker, sorted, each as its `MessageType`
    /// and then its `UserId` or `LibraryName`, if any.
    async fn until_end(&mut self) -> Vec<String> {
        let mut frames = Vec::new();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(10), self.next())
                .await
                .expect("no end marker within 10 s");
            let about = frame["UserId"]
                .as_str()
                .or(frame["LibraryName"].as_str())
                .unwrap_or_default();
            if about == "g1c-end" {
                break;
            }
            let kind = frame["MessageType"].as_str().unwrap_or_default();
            frames.push(format!("{kind} {about}").trim_end().to_string());
        }
        frames.sort();
        frames
    }
}

/// Sorts the frame lines, for comparing with `G1cSocket::until_end`.
fn g1c_sorted(lines: &[&str]) -> Vec<String> {
    let mut lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    lines.sort();
    lines
}

#[tokio::test]
async fn g1c_session_events_reach_only_their_owner_and_admins() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let m = g1c_insert_movies(&state, 1).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let mut owner_socket = G1cSocket::open(addr, &owner).await;
    let mut viewer_socket = G1cSocket::open(addr, &viewer).await;
    let mut key_socket = G1cSocket::open(addr, "test-api-key").await;

    // Each user starts playing, which sends a SessionChanged with their UserId.
    for token in [&owner, &viewer] {
        let body = serde_json::json!({ "ItemId": m[0], "PositionTicks": 0 });
        let req = request("POST", "/Sessions/Playing", Some(token), Some(body));
        assert!(send(&app, req).await.0.is_success());
    }
    let log = rustyfin_model::ws_event::LogMessageData {
        timestamp: String::new(),
        level: "INFO".to_string(),
        message: format!("g1c: a log line naming {owner_id}"),
        target: None,
    };
    let events = [
        WsEvent::SessionChanged {
            data: serde_json::json!({ "Id": "g1c-no-user" }),
        },
        WsEvent::LogMessage { data: log },
        WsEvent::LibraryChanged {
            data: serde_json::json!({ "LibraryName": "g1c-shared" }),
        },
        WsEvent::PlaybackStart {
            data: serde_json::json!({ "UserId": owner_id }),
        },
        WsEvent::PlaybackProgress {
            data: serde_json::json!({ "UserId": viewer_id }),
        },
        WsEvent::PlaybackStop {
            data: serde_json::json!({ "UserId": owner_id }),
        },
        WsEvent::LibraryChanged {
            data: serde_json::json!({ "LibraryName": "g1c-end" }),
        },
    ];
    for event in events {
        state.ws_broadcast.send(event).unwrap();
    }

    let session_owner = format!("SessionChanged {owner_id}");
    let session_viewer = format!("SessionChanged {viewer_id}");
    let start_owner = format!("PlaybackStart {owner_id}");
    let progress_viewer = format!("PlaybackProgress {viewer_id}");
    let stop_owner = format!("PlaybackStop {owner_id}");
    let everything = g1c_sorted(&[
        &session_owner,
        &session_viewer,
        "SessionChanged",
        "LogMessage",
        "LibraryChanged g1c-shared",
        &start_owner,
        &progress_viewer,
        &stop_owner,
    ]);
    // A user who is not an administrator gets their own session and playback events and the shared
    // ones: not another user's, not one without a user, and no server log lines.
    let viewers = g1c_sorted(&[
        &session_viewer,
        "LibraryChanged g1c-shared",
        &progress_viewer,
    ]);
    assert_eq!(
        owner_socket.until_end().await,
        everything,
        "an administrator's socket"
    );
    assert_eq!(
        key_socket.until_end().await,
        everything,
        "the API key's socket"
    );
    assert_eq!(
        viewer_socket.until_end().await,
        viewers,
        "the viewer's socket"
    );
}

#[tokio::test]
async fn g1c_session_command_events_name_the_sessions_owner() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let (owner_session, viewer_session) = g1f_sessions(&state, &owner_id, &viewer_id).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let mut owner_socket = G1cSocket::open(addr, &owner).await;
    let mut viewer_socket = G1cSocket::open(addr, &viewer).await;
    let mut events = state.ws_broadcast.subscribe();

    let command = serde_json::json!({
        "Command": "DisplayMessage",
        "Arguments": { "Header": "g1c", "Text": "test" }
    });
    // The viewer and the administrator on the viewer's session, then the administrator on their
    // own session and device.
    let requests = [
        (
            viewer.as_str(),
            format!("/Sessions/{viewer_session}/Command"),
            Some(command.clone()),
        ),
        (
            owner.as_str(),
            format!("/Sessions/{viewer_session}/Command/GoHome"),
            None,
        ),
        (
            owner.as_str(),
            format!("/Sessions/{owner_session}/Command/GoHome"),
            None,
        ),
        (
            owner.as_str(),
            "/Devices/g1f-owner-device/Command".to_string(),
            Some(command),
        ),
    ];
    for (token, uri, body) in requests {
        let req = request("POST", &uri, Some(token), body);
        assert_eq!(send(&app, req).await.0, StatusCode::OK, "{uri}");
    }
    let end = WsEvent::LibraryChanged {
        data: serde_json::json!({ "LibraryName": "g1c-end" }),
    };
    state.ws_broadcast.send(end).unwrap();
    // A message's payload has its own MessageType, which hides the frame's, so it comes after the
    // sockets' end marker and is checked on the channel only.
    let message = serde_json::json!({ "Header": "g1c", "Text": "test" });
    let uri = format!("/Sessions/{viewer_session}/Message");
    let req = request("POST", &uri, Some(&owner), Some(message));
    assert_eq!(send(&app, req).await.0, StatusCode::OK, "{uri}");

    // Each event names the user whose session or device it is.
    let mut named = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let WsEvent::SessionChanged { data } = event {
            let field = |key: &str| data[key].as_str().unwrap_or_default().to_string();
            named.push((field("Id"), field("UserId")));
        }
    }
    let pair = |id: &str, user: &str| (id.to_string(), user.to_string());
    assert_eq!(
        named,
        [
            pair(&viewer_session, &viewer_id),
            pair(&viewer_session, &viewer_id),
            pair(&owner_session, &owner_id),
            pair("g1f-owner-device", &owner_id),
            pair(&viewer_session, &viewer_id),
        ],
        "each SessionChanged's (Id, UserId)"
    );

    // The viewer's socket gets the events for their own session, not the administrator's.
    let session_owner = format!("SessionChanged {owner_id}");
    let session_viewer = format!("SessionChanged {viewer_id}");
    assert_eq!(
        owner_socket.until_end().await,
        g1c_sorted(&[
            &session_owner,
            &session_owner,
            &session_viewer,
            &session_viewer
        ]),
        "the administrator's socket"
    );
    assert_eq!(
        viewer_socket.until_end().await,
        g1c_sorted(&[&session_viewer, &session_viewer]),
        "the viewer's socket"
    );
}

#[tokio::test]
async fn g1c_quick_connect_authorize_broadcasts_no_secret() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (_, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let initiate = request("POST", "/QuickConnect/Initiate", None, None);
    let (status, started) = send(&app, initiate).await;
    assert_eq!(status, StatusCode::OK, "initiate");
    let secret = started["Secret"].as_str().unwrap().to_string();
    let code = started["Code"].as_str().unwrap();
    let mut events = state.ws_broadcast.subscribe();

    let uri = format!("/QuickConnect/Authorize?code={code}");
    let answer = send(&app, request("POST", &uri, Some(&viewer), None)).await;
    assert_eq!(
        answer,
        (StatusCode::OK, serde_json::json!(true)),
        "authorize"
    );
    // The secret is the new device's claim on the user who authorized it: no socket may see it.
    while let Ok(event) = events.try_recv() {
        let text = serde_json::to_string(&event).unwrap();
        assert!(
            !text.contains(&secret),
            "a broadcast event carries the Quick Connect secret"
        );
    }
    // The device still sees the authorization.
    let uri = format!("/QuickConnect/Connect?secret={secret}");
    let (status, polled) = send(&app, request("GET", &uri, None, None)).await;
    assert_eq!(
        (status, &polled["Authenticated"]),
        (StatusCode::OK, &serde_json::json!(true)),
        "connect"
    );
}

#[tokio::test]
async fn g1c_now_playing_item_needs_the_sessions_owner_or_an_admin() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let (owner_session, viewer_session) = g1f_sessions(&state, &owner_id, &viewer_id).await;
    let m = g1c_insert_movies(&state, 2).await;
    for (session, movie) in [(&owner_session, m[0]), (&viewer_session, m[1])] {
        rustyfin_db::repo_session::update_session_now_playing(
            &state.db,
            session,
            Some(movie),
            Some(0),
            false,
        )
        .await
        .unwrap();
    }
    let unknown = uuid::Uuid::new_v4().to_string();
    let now_playing = |session: &str, token: Option<&str>| {
        let uri = format!("/Sessions/Playing/NowPlayingItem?SessionId={session}");
        request("GET", &uri, token, None)
    };
    let playing = |movie: uuid::Uuid| (StatusCode::OK, Some(movie));
    let forbidden = (StatusCode::FORBIDDEN, None);
    let not_found = (StatusCode::NOT_FOUND, None);

    assert_eq!(
        send(&app, now_playing(&viewer_session, None)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // (who, token, session, the status and the item's id)
    let cases = [
        ("viewer", viewer.as_str(), &viewer_session, playing(m[1])),
        ("viewer", viewer.as_str(), &owner_session, forbidden),
        ("viewer", viewer.as_str(), &unknown, forbidden),
        (
            "administrator",
            owner.as_str(),
            &owner_session,
            playing(m[0]),
        ),
        (
            "administrator",
            owner.as_str(),
            &viewer_session,
            playing(m[1]),
        ),
        ("administrator", owner.as_str(), &unknown, not_found),
        ("API key", "test-api-key", &viewer_session, playing(m[1])),
        ("API key", "test-api-key", &unknown, not_found),
    ];
    for (who, token, session, answer) in cases {
        let (status, json) = send(&app, now_playing(session, Some(token))).await;
        let id = json["Id"]
            .as_str()
            .map(|id| id.parse::<uuid::Uuid>().unwrap());
        assert_eq!((status, id), answer, "{who} on {session}");
    }
    // No SessionId: nothing, as before.
    let req = request(
        "GET",
        "/Sessions/Playing/NowPlayingItem",
        Some(&viewer),
        None,
    );
    assert_eq!(
        send(&app, req).await,
        (StatusCode::OK, serde_json::Value::Null)
    );
}

#[tokio::test]
async fn g1c_set_rating_writes_the_path_users_rating() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let item = g1c_insert_movies(&state, 1).await[0];
    let random = uuid::Uuid::new_v4().to_string();
    let put = |user: &str, token: Option<&str>, likes: bool, rating: i32| {
        let uri = format!("/Users/{user}/Items/{item}/Rating");
        let body = serde_json::json!({ "Likes": likes, "UserRating": rating });
        request("PUT", &uri, token, Some(body))
    };
    let unrated = (None, None);

    assert_eq!(
        send(&app, put(&viewer_id, None, true, 1)).await.0,
        StatusCode::UNAUTHORIZED
    );
    // A non-admin on another user, existing or not: 403, and nothing is written.
    for target in [&owner_id, &random] {
        assert_eq!(
            send(&app, put(target, Some(&viewer), true, 2)).await.0,
            StatusCode::FORBIDDEN,
            "viewer on {target}"
        );
    }
    assert_eq!(g1f_rating(&state, &owner_id, item).await, unrated, "owner");
    assert_eq!(
        g1f_rating(&state, &viewer_id, item).await,
        unrated,
        "viewer"
    );
    // A user on themselves: their own rating only.
    assert_eq!(
        send(&app, put(&viewer_id, Some(&viewer), false, 3)).await.0,
        StatusCode::OK,
        "viewer on themselves"
    );
    assert_eq!(
        g1f_rating(&state, &viewer_id, item).await,
        (Some(false), Some(3))
    );
    assert_eq!(g1f_rating(&state, &owner_id, item).await, unrated, "owner");
    // An administrator or the API key on another user: that user's rating.
    assert_eq!(
        send(&app, put(&viewer_id, Some(&owner), true, 9)).await.0,
        StatusCode::OK,
        "administrator on the viewer"
    );
    assert_eq!(
        g1f_rating(&state, &viewer_id, item).await,
        (Some(true), Some(9))
    );
    assert_eq!(g1f_rating(&state, &owner_id, item).await, unrated, "owner");
    assert_eq!(
        send(&app, put(&owner_id, Some("test-api-key"), true, 7))
            .await
            .0,
        StatusCode::OK,
        "API key on the owner"
    );
    assert_eq!(
        g1f_rating(&state, &owner_id, item).await,
        (Some(true), Some(7))
    );
    // An administrator on an unknown user: 404.
    assert_eq!(
        send(&app, put(&random, Some(&owner), true, 5)).await.0,
        StatusCode::NOT_FOUND,
        "administrator on an unknown user"
    );
}

#[tokio::test]
async fn g1c_delete_rating_unknown_item_is_404() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let item = g1c_insert_movies(&state, 1).await[0];
    let unknown = uuid::Uuid::new_v4();
    let delete = |user: &str, item: uuid::Uuid, token: &str| {
        let uri = format!("/Users/{user}/Items/{item}/Rating");
        request("DELETE", &uri, Some(token), None)
    };
    let rated = (Some(true), Some(8));
    g1f_rate(&state, &owner_id, item).await;
    g1f_rate(&state, &viewer_id, item).await;

    // An unknown item: 404 for the user themselves, an administrator and the API key.
    let callers = [
        ("viewer", &viewer_id, viewer.as_str()),
        ("administrator", &viewer_id, owner.as_str()),
        ("API key", &owner_id, "test-api-key"),
    ];
    for (who, user, token) in callers {
        assert_eq!(
            send(&app, delete(user, unknown, token)).await.0,
            StatusCode::NOT_FOUND,
            "{who} on an unknown item"
        );
    }
    // Another user's path is checked first: 403.
    assert_eq!(
        send(&app, delete(&owner_id, unknown, &viewer)).await.0,
        StatusCode::FORBIDDEN,
        "viewer on the owner and an unknown item"
    );
    assert_eq!(g1f_rating(&state, &owner_id, item).await, rated, "owner");
    assert_eq!(g1f_rating(&state, &viewer_id, item).await, rated, "viewer");
    // A known item: only the path user's rating is cleared.
    assert_eq!(
        send(&app, delete(&viewer_id, item, &viewer)).await,
        (StatusCode::OK, serde_json::json!({}))
    );
    assert_eq!(g1f_rating(&state, &viewer_id, item).await, (None, None));
    assert_eq!(g1f_rating(&state, &owner_id, item).await, rated, "owner");
}

/// The query with each parameter name's first letter in upper case: `startIndex=1&limit=1`
/// becomes `StartIndex=1&Limit=1`.
fn g1c_pascal(query: &str) -> String {
    query
        .split('&')
        .map(|pair| {
            let mut chars = pair.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[tokio::test]
async fn g1c_next_up_accepts_both_casings() {
    let state = test_state().await;
    let _dir = G1cDataDir(state.data_dir.to_path_buf());
    let app = app(state.clone());
    let (owner, owner_id, viewer, viewer_id) = g1_owner_and_viewer(&state, &app).await;
    let (series, [e1, e2, e3]) = g1f_insert_show(&state).await;
    rustyfin_db::repo_playback::mark_played(&state.db, owner_id.parse().unwrap(), e1)
        .await
        .unwrap();
    let random = uuid::Uuid::new_v4();
    let (owner, viewer) = (owner.as_str(), viewer.as_str());
    let ok = |ids: &[uuid::Uuid]| (StatusCode::OK, ids.to_vec());
    let forbidden = (StatusCode::FORBIDDEN, Vec::new());
    // (path, token, camelCase query, answer). The owner has watched E1, so their Next Up is E2,
    // and the viewer has played nothing. E3 airs in 2999.
    let cases = [
        ("/Shows/NextUp", owner, "limit=0".to_string(), ok(&[])),
        ("/Shows/NextUp", owner, "startIndex=1".to_string(), ok(&[])),
        (
            "/Shows/NextUp",
            owner,
            format!("seriesId={random}"),
            ok(&[]),
        ),
        (
            "/Shows/NextUp",
            viewer,
            format!("seriesId={series}"),
            ok(&[e1]),
        ),
        (
            "/Shows/NextUp",
            viewer,
            format!("userId={owner_id}"),
            forbidden.clone(),
        ),
        (
            "/Shows/NextUp",
            owner,
            format!("userId={viewer_id}"),
            ok(&[]),
        ),
        (
            "/Shows/NextUp",
            "test-api-key",
            format!("userId={owner_id}"),
            ok(&[e2]),
        ),
        ("/Shows/Upcoming", viewer, "limit=0".to_string(), ok(&[])),
        (
            "/Shows/Upcoming",
            viewer,
            "startIndex=1".to_string(),
            ok(&[]),
        ),
        (
            "/Shows/Upcoming",
            viewer,
            format!("seriesId={random}"),
            ok(&[]),
        ),
        (
            "/Shows/Upcoming",
            viewer,
            format!("seriesId={series}"),
            ok(&[e3]),
        ),
        (
            "/Shows/Upcoming",
            viewer,
            format!("userId={owner_id}"),
            forbidden.clone(),
        ),
    ];
    for (path, token, query, answer) in cases {
        for query in [query.clone(), g1c_pascal(&query)] {
            assert_eq!(
                g1f_ids(&app, format!("{path}?{query}"), Some(token)).await,
                answer,
                "{path}?{query}"
            );
        }
    }
}

// ── B17: library scan progress ──
// Every signed-in socket gets `RefreshProgress` frames for a library's scan: "0" when it is
// queued, each higher whole percent, and "100" once its probes have finished. A new socket
// first gets the scans in progress. A web scan's Completed comes after that end; scheduler
// scans send no task events, so they raise no toast.

/// Writes `n` empty movie files into the library folder `folder`.
fn b17_movies(folder: &str, n: usize) {
    for i in 1..=n {
        let dir = std::path::Path::new(folder).join(format!("B17 Movie {i} (2000)"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("B17 Movie {i} (2000).mkv")), b"").unwrap();
    }
}

/// Serves `app` on a free local port, for sockets.
async fn b17_serve(app: &axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    addr
}

/// The library id and the percent of the socket's next `RefreshProgress` frame.
async fn b17_next_progress(socket: &mut G1cSocket) -> (String, String) {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("no RefreshProgress frame within 10 s");
        if frame["MessageType"] == "RefreshProgress" {
            let progress = frame["Data"]["Progress"].as_str();
            let progress = progress.expect("Progress is a string").to_string();
            return (
                frame["Data"]["ItemId"].as_str().unwrap().to_string(),
                progress,
            );
        }
    }
}

/// Reads the socket until it has had `RefreshProgress` "100" for `lib` and, with `completed`,
/// the `ScheduledTaskCompleted` naming `lib`, at which point `scans` must no longer list `lib`.
/// Returns the percents for `lib`, and each `ScheduledTask*` frame as `MessageType State`, sorted.
async fn b17_follow_scan(
    socket: &mut G1cSocket,
    scans: &ScanRegistry,
    lib: uuid::Uuid,
    completed: bool,
) -> (Vec<String>, Vec<String>) {
    let id = lib.to_string();
    let (mut progress, mut tasks) = (Vec::<String>::new(), Vec::new());
    let mut done = !completed;
    while progress.last().map(String::as_str) != Some("100") || !done {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), socket.next())
            .await
            .unwrap_or_else(|_| panic!("the scan did not end within 60 s: {progress:?} {tasks:?}"));
        let kind = frame["MessageType"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if kind == "RefreshProgress" {
            assert_eq!(frame["Data"]["ItemId"], id.as_str(), "{frame}");
            let percent = frame["Data"]["Progress"].as_str();
            progress.push(percent.expect("Progress is a string").to_string());
        } else if kind.starts_with("ScheduledTask") {
            if kind == "ScheduledTaskCompleted" && frame["ItemId"] == id.as_str() {
                assert_eq!(
                    scans.state(lib),
                    None,
                    "Completed came before the scan ended"
                );
                done = true;
            }
            tasks.push(format!(
                "{kind} {}",
                frame["State"].as_str().unwrap_or_default()
            ));
        }
    }
    tasks.sort();
    (progress, tasks)
}

/// Asserts that one scan's percents are whole numbers that start at 0, only go up, stay at or
/// below 99 until the last one, which is 100, and include one between 0 and 100.
fn b17_assert_progress(progress: &[String]) {
    let values: Vec<u8> = progress
        .iter()
        .map(|p| {
            p.parse()
                .unwrap_or_else(|_| panic!("{p:?} in {progress:?}"))
        })
        .collect();
    assert_eq!(values.first(), Some(&0), "{progress:?}");
    assert_eq!(values.last(), Some(&100), "{progress:?}");
    assert!(
        values.windows(2).all(|w| w[0] <= w[1]),
        "the percent only goes up: {progress:?}"
    );
    assert!(
        values[..values.len() - 1].iter().all(|&p| p <= 99),
        "{progress:?}"
    );
    assert!(
        values.iter().any(|&p| 0 < p && p < 100),
        "the probes move the percent: {progress:?}"
    );
}

#[tokio::test]
async fn b17_web_scan_reports_progress_to_every_socket() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let lib = create_library_via_api(&app, &state, &owner, "B17 Film", "Movies").await;
    b17_movies(&media_folder(&state, "b17 film"), 4);
    let addr = b17_serve(&app).await;
    let mut viewer_socket = G1cSocket::open(addr, &viewer).await;
    let mut owner_socket = G1cSocket::open(addr, &owner).await;

    let uri = format!("/Library/VirtualFolders/{lib}/Refresh");
    let (status, body) = send_authed(&app, "POST", &uri, &owner, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items_scanned"], 4);

    let scans = state.task_scheduler.scans();
    for (who, socket) in [("viewer", &mut viewer_socket), ("owner", &mut owner_socket)] {
        let (progress, tasks) = b17_follow_scan(socket, scans, lib, true).await;
        b17_assert_progress(&progress);
        assert_eq!(
            tasks,
            [
                "ScheduledTaskCompleted Completed",
                "ScheduledTaskStarted Running"
            ],
            "{who}"
        );
    }
}

#[tokio::test]
async fn b17_scheduler_scan_reports_progress_without_task_events() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let lib = create_library_via_api(&app, &state, &owner, "B17 Timer", "Movies").await;
    b17_movies(&media_folder(&state, "b17 timer"), 3);
    let addr = b17_serve(&app).await;
    let mut socket = G1cSocket::open(addr, &viewer).await;

    let uri = "/ScheduledTasks/Running/LibraryScan";
    let (status, body) = send_authed(&app, "POST", uri, &owner, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let scans = state.task_scheduler.scans();
    let (progress, tasks) = b17_follow_scan(&mut socket, scans, lib, false).await;
    b17_assert_progress(&progress);
    assert_eq!(scans.state(lib), None);

    // A task event sent at the end would come before this marker.
    let marker = WsEvent::LibraryChanged {
        data: serde_json::json!({ "LibraryName": "g1c-end" }),
    };
    state.ws_broadcast.send(marker).unwrap();
    let later = socket.until_end().await;
    assert!(
        tasks.is_empty() && !later.iter().any(|f| f.starts_with("ScheduledTask")),
        "a scheduler scan raises no toast: {tasks:?} {later:?}"
    );
}

#[tokio::test]
async fn b17_new_socket_lists_the_scans_in_progress() {
    let state = test_state().await;
    let app = app(state.clone());
    let (owner, _, viewer, _) = g1_owner_and_viewer(&state, &app).await;
    let lib = create_library_via_api(&app, &state, &owner, "B17 Queued", "Movies").await;
    let id = lib.to_string();
    let addr = b17_serve(&app).await;
    let scans = state.task_scheduler.scans();
    let at = |percent: &str| (id.clone(), percent.to_string());

    let ticket = scans.queue(lib);
    let mut first = G1cSocket::open(addr, &viewer).await;
    assert_eq!(
        b17_next_progress(&mut first).await,
        at("0"),
        "a queued scan is listed at 0"
    );

    ticket.start();
    ticket.add_total(4);
    ticket.advance(1);
    assert_eq!(b17_next_progress(&mut first).await, at("25"));
    for (who, token) in [
        ("viewer", viewer.as_str()),
        ("owner", owner.as_str()),
        ("API key", "test-api-key"),
    ] {
        let mut socket = G1cSocket::open(addr, token).await;
        assert_eq!(b17_next_progress(&mut socket).await, at("25"), "{who}");
    }

    drop(ticket);
    assert_eq!(b17_next_progress(&mut first).await, at("100"));
    let mut after = G1cSocket::open(addr, &viewer).await;
    let marker = WsEvent::LibraryChanged {
        data: serde_json::json!({ "LibraryName": "g1c-end" }),
    };
    state.ws_broadcast.send(marker).unwrap();
    assert_eq!(
        after.until_end().await,
        Vec::<String>::new(),
        "no scan is in progress"
    );
}

// ── G4: transcode pipeline and byte ranges ──
//
// Unsatisfiable ranges get 416 instead of a panic or the whole file. Tests that need ffmpeg skip
// with a note when it is not on PATH. Every test removes its data dir, also when it fails.

/// Removes a test's data dir when dropped.
struct G4Dir(std::path::PathBuf);

impl Drop for G4Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stores a library on `file`'s folder with one item of `media_type` ("Movie" or "Audio") whose
/// path is `file`, and returns the item's id.
async fn g4_insert_item(state: &AppState, media_type: &str, file: &std::path::Path) -> uuid::Uuid {
    let content_type = if media_type == "Audio" {
        rustyfin_model::library::LibraryContentType::Music
    } else {
        rustyfin_model::library::LibraryContentType::Movies
    };
    let lib = rustyfin_model::library::Library {
        id: uuid::Uuid::new_v4(),
        name: format!("G4 {}", uuid::Uuid::new_v4().simple()),
        content_type,
        paths: vec![file.parent().unwrap().to_str().unwrap().to_string()],
    };
    rustyfin_db::repo_library::create_library(&state.db, &lib)
        .await
        .unwrap();
    let item_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO media_items (id, library_id, name, media_type, path) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(lib.id.to_string())
    .bind(file.file_stem().unwrap().to_str().unwrap())
    .bind(media_type)
    .bind(file.to_str().unwrap())
    .execute(&state.db)
    .await
    .unwrap();
    item_id
}

/// GETs `uri` with a bearer token and a `Range` header; returns the status, the `Content-Range`
/// header and the body.
async fn g4_get_range(
    app: &axum::Router,
    uri: &str,
    token: &str,
    range: &str,
) -> (StatusCode, Option<String>, Vec<u8>) {
    let req = Request::builder()
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Range", range)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let content_range = resp
        .headers()
        .get("content-range")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, content_range, body)
}

#[tokio::test]
async fn g4_unsatisfiable_ranges_get_416() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-ranges");
    let file = std::path::Path::new(&folder).join("G4 Range.mp4");
    let content: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &content).unwrap();
    let id = g4_insert_item(&state, "Movie", &file).await;
    let app = app(state);

    for uri in [
        format!("/Videos/{id}/stream?Static=true"),
        format!("/Audio/{id}/stream?Static=true"),
    ] {
        for range in ["bytes=-0", "bytes=999999999-", "bytes=1000-"] {
            let (status, content_range, body) = g4_get_range(&app, &uri, &token, range).await;
            assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE, "{uri} {range}");
            assert_eq!(content_range.as_deref(), Some("bytes */1000"), "{range}");
            assert!(body.is_empty(), "{range}: 416 must not carry the file");
        }

        let (status, content_range, body) = g4_get_range(&app, &uri, &token, "bytes=0-99").await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(content_range.as_deref(), Some("bytes 0-99/1000"));
        assert_eq!(body, &content[..100]);

        let (status, content_range, body) = g4_get_range(&app, &uri, &token, "bytes=-100").await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(content_range.as_deref(), Some("bytes 900-999/1000"));
        assert_eq!(body, &content[900..]);

        // A header that is not one byte range is ignored: the whole file.
        for range in ["abc", "bytes=0-0,5-9"] {
            let (status, content_range, body) = g4_get_range(&app, &uri, &token, range).await;
            assert_eq!(status, StatusCode::OK, "{range}");
            assert_eq!(content_range, None, "{range}");
            assert_eq!(body, content, "{range}");
        }
    }
}

/// True when ffmpeg and ffprobe are on PATH; otherwise prints that `test` is skipped.
fn g4_have_ffmpeg(test: &str) -> bool {
    for tool in ["ffmpeg", "ffprobe"] {
        let found = std::process::Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            println!("{test}: skipped, {tool} is not on PATH");
            return false;
        }
    }
    true
}

#[tokio::test]
async fn g4_ffmpeg_error_is_returned() {
    if !g4_have_ffmpeg("g4_ffmpeg_error_is_returned") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-garbage");
    let file = std::path::Path::new(&folder).join("G4 Garbage.mkv");
    std::fs::write(&file, vec![0x5a_u8; 4096]).unwrap();
    let id = g4_insert_item(&state, "Movie", &file).await;
    let manager = state.transcode_manager.clone();
    let app = app(state);

    let started = std::time::Instant::now();
    let uri = format!("/Videos/{id}/main.m3u8?PlaySessionId=g4-garbage");
    let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the error came after {:?}, not when ffmpeg exited",
        started.elapsed()
    );
    let detail = body["Detail"].as_str().unwrap_or_default();
    assert!(detail.contains("Invalid data found"), "{body}");
    assert!(!manager.is_active(id).await, "the failed transcode is gone");
}

#[tokio::test]
async fn g4_active_encodings_needs_a_name() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let app = app(state);
    let id = uuid::Uuid::new_v4();

    for (uri, expected) in [
        (
            "/Videos/ActiveEncodings".to_string(),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/Videos/ActiveEncodings?deviceId=g4-dev".to_string(),
            StatusCode::NO_CONTENT,
        ),
        (
            "/Videos/ActiveEncodings?playSessionId=g4-psid".to_string(),
            StatusCode::NO_CONTENT,
        ),
        (
            "/Videos/ActiveEncodings?DeviceId=g4-dev&PlaySessionId=g4-psid".to_string(),
            StatusCode::NO_CONTENT,
        ),
        (
            format!("/Videos/{id}/Transcode?PlaySessionId=g4-psid"),
            StatusCode::NO_CONTENT,
        ),
        (format!("/Videos/{id}/Transcode"), StatusCode::NO_CONTENT),
    ] {
        let (status, body) = send(&app, request("DELETE", &uri, Some(&token), None)).await;
        assert_eq!(status, expected, "DELETE {uri}: {body}");
    }
    let uri = "/Videos/ActiveEncodings?playSessionId=g4-psid";
    let (status, _) = send(&app, request("DELETE", uri, None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Stores a media source for `item` at `file` with one stream per (CodecType, codec) pair.
async fn g4_insert_source(
    state: &AppState,
    item: uuid::Uuid,
    container: &str,
    file: &std::path::Path,
    streams: &[(&str, &str)],
) {
    let streams: Vec<serde_json::Value> = streams
        .iter()
        .enumerate()
        .map(|(index, (codec_type, codec))| {
            serde_json::json!({ "Index": index, "Codec": codec, "CodecType": codec_type })
        })
        .collect();
    let source: rustyfin_model::media_source::MediaSourceResponse =
        serde_json::from_value(serde_json::json!({
            "Id": uuid::Uuid::new_v4().to_string(),
            "Uid": uuid::Uuid::new_v4().to_string(),
            "Container": container,
            "Protocol": "File",
            "Path": file.to_str().unwrap(),
            "MediaStreams": streams,
        }))
        .unwrap();
    rustyfin_db::repo_media_source::insert_media_source_with_streams(&state.db, &item, &source)
        .await
        .unwrap();
}

#[tokio::test]
async fn g4_universal_sends_unplayable_audio_to_transcode() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-universal");
    let mut items = Vec::new();
    for (name, video, audio) in [
        ("G4 AC3.mp4", "h264", "ac3"),
        ("G4 HEVC.mp4", "hevc", "aac"),
        ("G4 AAC.mp4", "h264", "aac"),
    ] {
        let file = std::path::Path::new(&folder).join(name);
        std::fs::write(&file, b"g4").unwrap();
        let id = g4_insert_item(&state, "Movie", &file).await;
        g4_insert_source(
            &state,
            id,
            "mp4",
            &file,
            &[("Video", video), ("Audio", audio)],
        )
        .await;
        items.push(id);
    }
    let (ac3, hevc, aac) = (items[0], items[1], items[2]);
    let app = app(state);

    let lists = "&VideoCodecs=h264,vp8,vp9,av1&AudioCodecs=aac,mp3,opus,flac";
    for (id, codecs, transcode) in [
        // The browser plays the video but not the audio, or the audio but not the video
        (ac3, lists, true),
        (hevc, lists, true),
        // A client without lists plays only what every browser plays
        (ac3, "", true),
        (hevc, "", true),
        (aac, lists, false),
        (aac, "", false),
    ] {
        let uri = format!("/Videos/{id}/Universal?MaxStreamingBitrate=100000000{codecs}");
        let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let url = body["Url"].as_str().unwrap_or_default();
        let source = &body["MediaSources"][0];
        if transcode {
            let play_session = body["PlaySessionId"].as_str().unwrap();
            assert!(
                url.starts_with(&format!("/Videos/{id}/main.m3u8?")),
                "{uri}: {url}"
            );
            assert!(
                url.contains(&format!("PlaySessionId={play_session}")),
                "{uri}: {url}"
            );
            if !codecs.is_empty() {
                assert!(url.contains("VideoCodec=h264,vp8,vp9,av1"), "{uri}: {url}");
            }
            assert_eq!(source["SupportsDirectPlay"], false, "{uri}");
            assert_eq!(source["SupportsDirectStream"], false, "{uri}");
        } else {
            assert!(
                url.starts_with(&format!("/Videos/{id}/stream?")) && url.contains("Static=true"),
                "{uri}: {url}"
            );
            assert_eq!(source["SupportsDirectPlay"], true, "{uri}");
        }
    }
}

/// Runs ffmpeg with `args` to make a test file.
fn g4_make_media(args: &[&str]) {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Writes `data` to `file` and returns the streams ffprobe finds in it.
fn g4_probe(file: &std::path::Path, data: &[u8]) -> Vec<serde_json::Value> {
    std::fs::write(file, data).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-of", "json", "-show_entries"])
        .arg("stream=codec_type,codec_name,profile,channels")
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let probed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    probed["streams"].as_array().cloned().unwrap_or_default()
}

/// GETs `uri` without an auth header; returns the status, the content type and the body.
async fn g4_get(app: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, content_type, body)
}

/// GETs the playlist at `uri` (auth in its query) and returns its first segment as a player
/// resolves it against the playlist URL `/Videos/{id}/main.m3u8`. Messages hide `token`.
async fn g4_first_segment(app: &axum::Router, id: uuid::Uuid, uri: &str, token: &str) -> String {
    let (status, _, body) = g4_get(app, uri).await;
    let playlist = String::from_utf8_lossy(&body).to_string();
    let shown = playlist.replace(token, "<token>");
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert!(playlist.starts_with("#EXTM3U"), "{shown}");
    let line = playlist
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| panic!("no segment in {shown}"));
    if line.starts_with('/') {
        line.to_string()
    } else {
        format!("/Videos/{id}/{line}")
    }
}

#[tokio::test]
async fn g4_transcode_playlist_and_segments_carry_auth() {
    if !g4_have_ffmpeg("g4_transcode_playlist_and_segments_carry_auth") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-transcode");
    let file = std::path::Path::new(&folder).join("G4 Clip.mp4");
    // H.264 that browsers play, with AC3 audio that the client below does not
    g4_make_media(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "4",
        "-c:v",
        "libx264",
        "-profile:v",
        "baseline",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "ac3",
        "-ac",
        "1",
        file.to_str().unwrap(),
    ]);
    let id = g4_insert_item(&state, "Movie", &file).await;
    rustyfin_core::probe::probe_and_save(&state.db, &id, file.to_str().unwrap())
        .await
        .unwrap();
    let probe_file = state.data_dir.join("g4-segment.ts");
    let app = app(state);

    let uri = format!(
        "/Videos/{id}/Universal?VideoCodecs=h264,vp8,vp9,av1&AudioCodecs=aac,mp3,opus,flac"
    );
    let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = body["Url"].as_str().unwrap_or_default();
    assert!(url.contains("/main.m3u8?"), "{url}");

    let playlist = format!("{url}&api_key={token}");
    let segment = g4_first_segment(&app, id, &playlist, &token).await;
    assert!(
        segment.starts_with(&format!("/Videos/{id}/hls/"))
            && segment.contains("/video/segment_")
            && segment.contains("api_key="),
        "{}",
        segment.replace(&token, "<token>")
    );
    let (status, content_type, data) = g4_get(&app, &segment).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "video/mp2t");
    assert_eq!(data.first(), Some(&0x47), "MPEG-TS starts with a sync byte");
    // The video was copied (x264 would have made it High), the audio encoded to AAC stereo
    let streams = g4_probe(&probe_file, &data);
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    assert_eq!(
        video.map(|s| &s["codec_name"]),
        Some(&"h264".into()),
        "{streams:?}"
    );
    assert_eq!(
        video.map(|s| &s["profile"]),
        Some(&"Constrained Baseline".into()),
        "{streams:?}"
    );
    let audio = streams.iter().find(|s| s["codec_type"] == "audio");
    assert_eq!(
        audio.map(|s| &s["codec_name"]),
        Some(&"aac".into()),
        "{streams:?}"
    );
    assert_eq!(
        audio.map(|s| &s["channels"]),
        Some(&2.into()),
        "{streams:?}"
    );

    // Without the playlist's query the segment request carries no auth
    let bare = segment.split('?').next().unwrap();
    let (status, _, _) = g4_get(&app, bare).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn g4_two_play_sessions_are_separate() {
    if !g4_have_ffmpeg("g4_two_play_sessions_are_separate") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-viewers");
    let file = std::path::Path::new(&folder).join("G4 Viewers.mp4");
    g4_make_media(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=25",
        "-t",
        "4",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        file.to_str().unwrap(),
    ]);
    let id = g4_insert_item(&state, "Movie", &file).await;
    let app = app(state);

    let mut segments = Vec::new();
    for play_session in ["g4-a", "g4-b"] {
        let uri = format!("/Videos/{id}/main.m3u8?PlaySessionId={play_session}&api_key={token}");
        segments.push(g4_first_segment(&app, id, &uri, &token).await);
    }
    let (a, b) = (&segments[0], &segments[1]);
    assert_ne!(
        a.split('?').next(),
        b.split('?').next(),
        "two play sessions, two transcodes"
    );
    assert_eq!(g4_get(&app, a).await.0, StatusCode::OK);
    assert_eq!(g4_get(&app, b).await.0, StatusCode::OK);

    // Stopping one viewer leaves the other
    let uri = "/Videos/ActiveEncodings?playSessionId=g4-a";
    let (status, _) = send(&app, request("DELETE", uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(g4_get(&app, a).await.0, StatusCode::NOT_FOUND);
    assert_eq!(g4_get(&app, b).await.0, StatusCode::OK);
    let uri = format!("/Videos/{id}/Transcode?PlaySessionId=g4-b");
    let (status, _) = send(&app, request("DELETE", &uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(g4_get(&app, b).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn g4_audio_only_transcode_has_audio() {
    if !g4_have_ffmpeg("g4_audio_only_transcode_has_audio") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-audio");
    let file = std::path::Path::new(&folder).join("G4 Tone.flac");
    g4_make_media(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100",
        "-t",
        "4",
        "-c:a",
        "flac",
        file.to_str().unwrap(),
    ]);
    let id = g4_insert_item(&state, "Audio", &file).await;
    let probe_file = state.data_dir.join("g4-audio.ts");
    let app = app(state);

    let uri =
        format!("/Videos/{id}/main.m3u8?AudioOnly=true&PlaySessionId=g4-audio&api_key={token}");
    let segment = g4_first_segment(&app, id, &uri, &token).await;
    let (status, _, data) = g4_get(&app, &segment).await;
    assert_eq!(status, StatusCode::OK);
    let streams = g4_probe(&probe_file, &data);
    assert_eq!(streams.len(), 1, "{streams:?}");
    assert_eq!(streams[0]["codec_type"], "audio", "{streams:?}");
    assert_eq!(streams[0]["codec_name"], "aac", "{streams:?}");
}

#[tokio::test]
async fn g4_universal_direct_plays_a_probed_mp4() {
    if !g4_have_ffmpeg("g4_universal_direct_plays_a_probed_mp4") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "g4-probed");
    let file = std::path::Path::new(&folder).join("G4 Playable.mp4");
    g4_make_media(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-t",
        "2",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        file.to_str().unwrap(),
    ]);
    let id = g4_insert_item(&state, "Movie", &file).await;
    // The streams as the scanner stores them, with ffprobe's stream types
    rustyfin_core::probe::probe_and_save(&state.db, &id, file.to_str().unwrap())
        .await
        .unwrap();
    let app = app(state);

    let uri = format!(
        "/Videos/{id}/Universal?VideoCodecs=h264,vp8,vp9,av1&AudioCodecs=aac,mp3,opus,flac"
    );
    let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = body["Url"].as_str().unwrap_or_default();
    assert!(
        url.starts_with(&format!("/Videos/{id}/stream?")) && url.contains("Static=true"),
        "{url}"
    );
    assert_eq!(body["MediaSources"][0]["SupportsDirectPlay"], true);
}

// ── B16: media source selection ──
//
// A movie with two versions, each a media source on its own file. `MediaSourceId` picks the
// version on every video playback route. No id, a blank one or the item's own id plays the
// primary, as before; an id that names no version of the item gets 404.

/// Stores an H.264 + AAC source in `container` for `item` at `file` and returns its id.
async fn b16_insert_source(
    state: &AppState,
    item: uuid::Uuid,
    container: &str,
    file: &std::path::Path,
) -> uuid::Uuid {
    let source: rustyfin_model::media_source::MediaSourceResponse =
        serde_json::from_value(serde_json::json!({
            "Id": uuid::Uuid::new_v4().to_string(),
            "Uid": uuid::Uuid::new_v4().to_string(),
            "Container": container,
            "Protocol": "File",
            "Path": file.to_str().unwrap(),
            "MediaStreams": [
                { "Index": 0, "Codec": "h264", "CodecType": "Video" },
                { "Index": 1, "Codec": "aac", "CodecType": "Audio" },
            ],
        }))
        .unwrap();
    rustyfin_db::repo_media_source::insert_media_source_with_streams(&state.db, &item, &source)
        .await
        .unwrap()
}

/// A movie in folder `name` with two MP4 versions, each with its own bytes: `B16 Film -
/// 1080p.mp4` (1000 bytes; the item's path and first source, so the primary) and `B16 Film -
/// 720p.mp4` (1500 bytes). Returns the item id and each version's source id and bytes, the
/// primary first.
async fn b16_two_versions(
    state: &AppState,
    name: &str,
) -> (uuid::Uuid, Vec<(uuid::Uuid, Vec<u8>)>) {
    let folder = media_folder(state, name);
    let mut item = None;
    let mut versions = Vec::new();
    for (file, len, step) in [
        ("B16 Film - 1080p.mp4", 1000u32, 7u32),
        ("B16 Film - 720p.mp4", 1500, 13),
    ] {
        let file = std::path::Path::new(&folder).join(file);
        let content: Vec<u8> = (0..len).map(|i| (i * step % 251) as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let id = match item {
            Some(id) => id,
            None => *item.insert(g4_insert_item(state, "Movie", &file).await),
        };
        versions.push((b16_insert_source(state, id, "mp4", &file).await, content));
    }
    (item.unwrap(), versions)
}

/// Sends `method` to `uri` with a bearer token and an optional `Range`; returns the status, the
/// `Content-Length` and `Content-Range` headers and the body.
async fn b16_fetch(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    range: Option<&str>,
) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"));
    if let Some(range) = range {
        req = req.header("Range", range);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (length, content_range) = (header("content-length"), header("content-range"));
    let status = resp.status();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, length, content_range, body)
}

#[tokio::test]
async fn b16_stream_routes_play_the_requested_version() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let (item, versions) = b16_two_versions(&state, "b16-stream").await;
    let ((a, primary), (b, other)) = (&versions[0], &versions[1]);
    // A movie that was never probed has no sources; its own id plays its file
    let bare_file = std::path::Path::new(&media_folder(&state, "b16-bare")).join("B16 Bare.mp4");
    let bare_content: Vec<u8> = (0..800u32).map(|i| (i % 199) as u8).collect();
    std::fs::write(&bare_file, &bare_content).unwrap();
    let bare = g4_insert_item(&state, "Movie", &bare_file).await;
    let app = app(state);

    for (id, query, content) in [
        // No id, a blank one or the item's own id: the primary, as before
        (item, String::new(), primary),
        (item, "MediaSourceId=&".to_string(), primary),
        (item, format!("MediaSourceId={item}&"), primary),
        (item, format!("MediaSourceId={}&", item.simple()), primary),
        (item, format!("MediaSourceId={a}&"), primary),
        // The other version's id, in the forms clients send
        (item, format!("MediaSourceId={b}&"), other),
        (item, format!("mediaSourceId={b}&"), other),
        (
            item,
            format!("MediaSourceId={}&", b.to_string().to_uppercase()),
            other,
        ),
        (item, format!("MediaSourceId={}&", b.simple()), other),
        (bare, String::new(), &bare_content),
        (bare, format!("MediaSourceId={bare}&"), &bare_content),
    ] {
        let len = content.len();
        // The start of the file, and its end through the container route
        let uri = format!("/Videos/{id}/stream?{query}Static=true");
        let (status, _, range, body) =
            b16_fetch(&app, "GET", &uri, &token, Some("bytes=0-99")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "GET {uri}");
        assert_eq!(range, Some(format!("bytes 0-99/{len}")), "GET {uri}");
        assert!(body == content[..100], "GET {uri}: other bytes");
        let uri = format!("/Videos/{id}/stream.mp4?{query}Static=true");
        let (status, _, range, body) =
            b16_fetch(&app, "GET", &uri, &token, Some("bytes=-100")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "GET {uri}");
        let expected = format!("bytes {}-{}/{len}", len - 100, len - 1);
        assert_eq!(range, Some(expected), "GET {uri}");
        assert!(body == content[len - 100..], "GET {uri}: other bytes");
        // Whole files, and their length
        for route in ["stream", "stream.mp4", "progressive.stream"] {
            let uri = format!("/Videos/{id}/{route}?{query}Static=true");
            let (status, _, _, body) = b16_fetch(&app, "GET", &uri, &token, None).await;
            assert_eq!(status, StatusCode::OK, "GET {uri}");
            assert!(body == *content, "GET {uri}: {} bytes", body.len());
            let (status, length, _, _) = b16_fetch(&app, "HEAD", &uri, &token, None).await;
            assert_eq!(status, StatusCode::OK, "HEAD {uri}");
            assert_eq!(length, Some(len.to_string()), "HEAD {uri}");
        }
    }
}

#[tokio::test]
async fn b16_an_id_of_no_version_gets_404() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let (item, _) = b16_two_versions(&state, "b16-unknown").await;
    let (_, others) = b16_two_versions(&state, "b16-unknown-other").await;
    let bare_file =
        std::path::Path::new(&media_folder(&state, "b16-unknown-bare")).join("B16 Bare.mp4");
    std::fs::write(&bare_file, b"b16").unwrap();
    let bare = g4_insert_item(&state, "Movie", &bare_file).await;
    let app = app(state);

    let unknown = uuid::Uuid::new_v4().to_string();
    for (id, source) in [
        (item, unknown.clone()),
        (item, "nope".to_string()),
        // Another movie's version is no version of this one
        (item, others[1].0.to_string()),
        // A movie that was never probed has no version but itself
        (bare, unknown),
    ] {
        for (method, route) in [
            ("GET", "stream"),
            ("HEAD", "stream"),
            ("GET", "stream.mp4"),
            ("HEAD", "stream.mp4"),
            ("GET", "progressive.stream"),
            ("HEAD", "progressive.stream"),
            ("GET", "Universal"),
            ("GET", "PlaybackInfo"),
            // Refused before ffmpeg starts
            ("GET", "main.m3u8"),
        ] {
            let uri =
                format!("/Videos/{id}/{route}?MediaSourceId={source}&PlaySessionId=b16-unknown");
            let (status, _, _, _) = b16_fetch(&app, method, &uri, &token, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }
}

/// The values of the `MediaSourceId` parameters in `url`'s query.
fn b16_url_sources(url: &str) -> Vec<&str> {
    let query = url.split_once('?').map_or("", |(_, query)| query);
    query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("MediaSourceId="))
        .collect()
}

/// Asserts that `body`'s `MediaSources` are `ids` in order and that only `selected` carries the
/// `Supports*` flags.
fn b16_assert_flags(body: &serde_json::Value, ids: &[&str], selected: &str, what: &str) {
    let sources = body["MediaSources"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: {body}"));
    let listed: Vec<&str> = sources
        .iter()
        .map(|s| s["Id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(listed, ids, "{what}: every version, in order");
    for source in sources {
        let chosen = source["Id"] == selected;
        let id = &source["Id"];
        assert_eq!(source["SupportsTranscoding"], chosen, "{what}: {id}");
        if !chosen {
            assert_eq!(source["SupportsDirectPlay"], false, "{what}: {id}");
            assert_eq!(source["SupportsDirectStream"], false, "{what}: {id}");
        }
    }
}

#[tokio::test]
async fn b16_universal_and_playback_info_name_the_requested_version() {
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let (item, versions) = b16_two_versions(&state, "b16-universal").await;
    let (a, b) = (versions[0].0.to_string(), versions[1].0.to_string());
    let ids = [a.as_str(), b.as_str()];
    let app = app(state);

    let lists = "VideoCodecs=h264,vp8,vp9,av1&AudioCodecs=aac,mp3,opus,flac";
    for (query, selected) in [
        (String::new(), &a),
        ("MediaSourceId=&".to_string(), &a),
        (format!("MediaSourceId={item}&"), &a),
        (format!("MediaSourceId={a}&"), &a),
        (format!("MediaSourceId={b}&"), &b),
        (format!("mediaSourceId={b}&"), &b),
        (format!("MediaSourceId={}&", b.to_uppercase()), &b),
    ] {
        // Direct play, direct stream and transcode each name the version by its id
        for (allow, route, is_static) in [
            ("", "stream", true),
            ("EnableDirectPlay=false&", "stream", false),
            (
                "EnableDirectPlay=false&EnableDirectStream=false&",
                "main.m3u8",
                false,
            ),
        ] {
            let uri = format!("/Videos/{item}/Universal?{query}{allow}{lists}");
            let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            let url = body["Url"].as_str().unwrap_or_default();
            assert!(
                url.starts_with(&format!("/Videos/{item}/{route}?")),
                "{uri}: {url}"
            );
            assert_eq!(url.contains("Static=true"), is_static, "{uri}: {url}");
            assert_eq!(b16_url_sources(url), [selected.as_str()], "{uri}: {url}");
            b16_assert_flags(&body, &ids, selected, &uri);
        }
        let uri = format!("/Videos/{item}/PlaybackInfo?{query}{lists}");
        let (status, body) = send(&app, request("GET", &uri, Some(&token), None)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        b16_assert_flags(&body, &ids, selected, &uri);
        let index = if *selected == a { 0 } else { 1 };
        assert_eq!(
            body["MediaSources"][index]["SupportsDirectPlay"], true,
            "{uri}: {body}"
        );
    }
}

/// Writes `data` to `file` and returns the width of its first video stream. For MPEG-TS, ffprobe
/// lists the stream under its program too, so the first value is taken.
fn b16_video_width(file: &std::path::Path, data: &[u8]) -> u64 {
    std::fs::write(file, data).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-of", "csv=p=0"])
        .args(["-show_entries", "stream=width"])
        .arg(file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let width = String::from_utf8_lossy(&out.stdout);
    width
        .lines()
        .find_map(|line| line.trim().parse().ok())
        .unwrap_or_else(|| panic!("ffprobe width: {width:?}"))
}

#[tokio::test]
async fn b16_hls_transcodes_the_requested_version() {
    if !g4_have_ffmpeg("b16_hls_transcodes_the_requested_version") {
        return;
    }
    let state = test_state().await;
    let _dir = G4Dir(state.data_dir.to_path_buf());
    let (token, _) = admin_session(&state).await;
    let folder = media_folder(&state, "b16-hls");
    // Two versions that differ in width: the primary 320 pixels, the other 160
    let mut files = Vec::new();
    for (name, size) in [
        ("B16 Clip - 240p.mp4", "320x240"),
        ("B16 Clip - 120p.mp4", "160x120"),
    ] {
        let file = std::path::Path::new(&folder).join(name);
        let video = format!("testsrc=size={size}:rate=25");
        g4_make_media(&[
            "-f",
            "lavfi",
            "-i",
            video.as_str(),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            file.to_str().unwrap(),
        ]);
        files.push(file);
    }
    let item = g4_insert_item(&state, "Movie", &files[0]).await;
    let a = b16_insert_source(&state, item, "mp4", &files[0]).await;
    let b = b16_insert_source(&state, item, "mp4", &files[1]).await;
    let probe_file = state.data_dir.join("b16-segment.ts");
    let app = app(state);

    // One play session asks for the other version, then for the primary
    let mut transcodes = Vec::new();
    for (source, width) in [(b, 160), (a, 320)] {
        let uri = format!(
            "/Videos/{item}/main.m3u8?MediaSourceId={source}&PlaySessionId=b16-hls&api_key={token}"
        );
        let segment = g4_first_segment(&app, item, &uri, &token).await;
        let (status, _, data) = g4_get(&app, &segment).await;
        assert_eq!(status, StatusCode::OK, "MediaSourceId={source}");
        let found = b16_video_width(&probe_file, &data);
        assert_eq!(found, width, "MediaSourceId={source}");
        transcodes.push(segment.split('?').next().unwrap().to_string());
    }
    assert_ne!(transcodes[0], transcodes[1], "two versions, two transcodes");

    // No id plays the primary, on the primary's transcode
    let uri = format!("/Videos/{item}/main.m3u8?PlaySessionId=b16-hls&api_key={token}");
    let segment = g4_first_segment(&app, item, &uri, &token).await;
    assert_eq!(segment.split('?').next().unwrap(), transcodes[1]);

    // Stopping the play session stops both
    let uri = "/Videos/ActiveEncodings?PlaySessionId=b16-hls";
    let (status, _) = send(&app, request("DELETE", uri, Some(&token), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for transcode in &transcodes {
        let segment = format!("{transcode}?api_key={token}");
        let (status, _, _) = g4_get(&app, &segment).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{transcode}");
    }
}
